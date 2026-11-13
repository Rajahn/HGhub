//! 直连视频的落盘。续传是否允许拼接由 [`crate::download_plan`] 决定。

use crate::download_plan::{apply_body, plan_resume, BodyPlan, ByteRange, Decision, Validators};
use crate::net::{err_text, open_download};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MAX_BYTES: u64 = 800 * 1024 * 1024;
const STALL: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectMeta {
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub total: Option<u64>,
}

impl DirectMeta {
    fn validators(&self) -> Validators {
        Validators {
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            total: self.total,
        }
    }
}

pub fn load_meta(path: &Path) -> DirectMeta {
    fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

pub fn save_meta(path: &Path, meta: &DirectMeta) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "无法创建下载目录")?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(meta).map_err(|_| "无法记录断点")?).map_err(|_| "无法记录断点")?;
    fs::rename(&tmp, path).map_err(|_| "无法记录断点".to_string())
}

enum Mode {
    Replace { tmp: PathBuf, file: File, dest: PathBuf },
    Append { file: File },
    Idle,
}

/// 未提交的替换文件会在丢弃时删掉，避免半截新文件盖住还能续传的旧文件。
struct Staged {
    mode: Mode,
    committed: bool,
    written: u64,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Mode::Replace { tmp, .. } = &self.mode {
            let _ = fs::remove_file(tmp);
        }
    }
}

impl Staged {
    fn begin(dest: &Path, plan: &BodyPlan) -> Result<Self, String> {
        match plan {
            BodyPlan::Replace => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|_| "无法创建下载目录")?;
                }
                let tmp = dest.with_extension("restart");
                let file = File::create(&tmp).map_err(|_| "无法写入临时文件")?;
                Ok(Self { mode: Mode::Replace { tmp, file, dest: dest.to_path_buf() }, committed: false, written: 0 })
            }
            BodyPlan::Append { at } => {
                let file = OpenOptions::new().create(true).append(true).read(true).open(dest).map_err(|_| "无法打开断点文件")?;
                let len = file.metadata().map_err(|_| "无法读取断点文件")?.len();
                if len != *at {
                    return Err("本地文件长度与断点不一致，已停止拼接".into());
                }
                Ok(Self { mode: Mode::Append { file }, committed: false, written: len })
            }
            BodyPlan::Complete => Ok(Self { mode: Mode::Idle, committed: true, written: 0 }),
            BodyPlan::Restart | BodyPlan::Reject => Err("这次响应禁止写入".into()),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), String> {
        if self.written.saturating_add(chunk.len() as u64) > MAX_BYTES {
            return Err("视频超过 800MB，已停止下载".into());
        }
        match &mut self.mode {
            Mode::Replace { file, .. } | Mode::Append { file } => {
                file.write_all(chunk).map_err(|_| "写入下载文件失败")?;
                self.written += chunk.len() as u64;
                Ok(())
            }
            Mode::Idle => Ok(()),
        }
    }

    fn commit(mut self, expected: Option<u64>) -> Result<u64, String> {
        let written = self.written;
        let appending = matches!(self.mode, Mode::Append { .. });
        match &mut self.mode {
            Mode::Replace { file, .. } | Mode::Append { file } => {
                file.flush().map_err(|_| "写入下载文件失败")?;
                file.sync_all().map_err(|_| "写入下载文件失败")?;
            }
            Mode::Idle => {}
        }
        if let Some(total) = expected {
            if written != total {
                return Err(if appending {
                    "连接提前结束，已保留断点".into()
                } else {
                    "下载长度与服务器声明不一致，已丢弃不完整的结果".into()
                });
            }
        }
        if let Mode::Replace { tmp, dest, .. } = &self.mode {
            fs::rename(tmp, dest).map_err(|_| "无法替换下载文件")?;
        }
        self.committed = true;
        Ok(written)
    }
}

pub enum DirectOutcome {
    File { bytes: u64 },
    Playlist,
}

pub async fn download_direct(
    url: &str,
    dest: &Path,
    meta_path: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<DirectOutcome, String> {
    let mut meta = load_meta(meta_path);
    let mut restarted = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("已暂停".into());
        }
        let have = file_len(dest);
        if have == 0 {
            meta = DirectMeta::default();
        }
        let range = (have > 0).then_some(have);
        let if_range = meta.etag.clone().or_else(|| meta.last_modified.clone());
        let opened = open_download(url, range, if_range.as_deref()).await?;
        if have == 0 && looks_like_playlist(url, opened.content_type.as_deref()) {
            return Ok(DirectOutcome::Playlist);
        }
        let parsed = opened.content_range.as_deref().and_then(crate::download_plan::parse_content_range);
        let decision = plan_resume(
            have,
            &meta.validators(),
            opened.status,
            parsed.as_ref(),
            opened.etag.as_deref(),
            opened.last_modified.as_deref(),
            opened.content_length,
        );
        match decision.plan {
            BodyPlan::Restart => {
                let _ = fs::remove_file(dest);
                let _ = fs::remove_file(meta_path);
                meta = DirectMeta::default();
                if restarted {
                    return Err(decision.message.into());
                }
                restarted = true;
                continue;
            }
            BodyPlan::Reject => return Err(decision.message.into()),
            BodyPlan::Complete => return Ok(DirectOutcome::File { bytes: have }),
            BodyPlan::Replace | BodyPlan::Append { .. } => {
                let expected = expected_len(have, &decision, &parsed, opened.content_length);
                let next_meta = next_validators(&meta, &decision.plan, &opened, &parsed);
                match stream_body(dest, &decision, opened.response, expected, cancel, &mut progress).await? {
                    Streamed::Playlist => return Ok(DirectOutcome::Playlist),
                    Streamed::Bytes(bytes) => {
                        meta = next_meta;
                        if meta.total.is_none() {
                            meta.total = Some(bytes);
                        }
                        save_meta(meta_path, &meta)?;
                        return Ok(DirectOutcome::File { bytes });
                    }
                }
            }
        }
    }
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn looks_like_playlist(url: &str, content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| value.to_ascii_lowercase().contains("mpegurl"))
        || url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase().ends_with(".m3u8")
}

fn expected_len(have: u64, decision: &Decision, range: &Option<ByteRange>, content_length: Option<u64>) -> Option<u64> {
    match decision.plan {
        BodyPlan::Append { .. } => range.as_ref().and_then(|item| item.total).or(Some(have.saturating_add(content_length.unwrap_or(0))).filter(|_| content_length.is_some())),
        BodyPlan::Replace => content_length.or(range.as_ref().and_then(|item| item.total)),
        _ => None,
    }
}

fn next_validators(previous: &DirectMeta, plan: &BodyPlan, opened: &crate::net::OpenedDownload, range: &Option<ByteRange>) -> DirectMeta {
    let total = match plan {
        BodyPlan::Append { .. } => range.as_ref().and_then(|item| item.total).or(previous.total),
        BodyPlan::Replace => opened.content_length.or(range.as_ref().and_then(|item| item.total)),
        _ => previous.total,
    };
    match plan {
        BodyPlan::Replace => DirectMeta {
            etag: opened.etag.clone().or_else(|| previous.etag.clone()),
            last_modified: opened.last_modified.clone().or_else(|| previous.last_modified.clone()),
            total,
        },
        _ => DirectMeta {
            etag: previous.etag.clone().or_else(|| opened.etag.clone()),
            last_modified: previous.last_modified.clone().or_else(|| opened.last_modified.clone()),
            total,
        },
    }
}

enum Streamed {
    Bytes(u64),
    Playlist,
}

fn playlist_bytes(bytes: &[u8]) -> bool {
    let text = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    text.starts_with(b"#EXTM3U")
}

async fn stream_body(
    dest: &Path,
    decision: &Decision,
    response: Option<reqwest::Response>,
    expected: Option<u64>,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(u64, u64),
) -> Result<Streamed, String> {
    let mut response = response.ok_or("响应没有内容")?;
    let mut staged = Staged::begin(dest, &decision.plan)?;
    let total = expected.unwrap_or(0);
    let mut pending = Vec::new();
    let mut decided = !matches!(decision.plan, BodyPlan::Replace);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("已暂停".into());
        }
        let chunk = tokio::time::timeout(STALL, response.chunk())
            .await
            .map_err(|_| "读取超时，连接可能已中断".to_string())?
            .map_err(|err| err_text(&err))?;
        let Some(chunk) = chunk else { break };
        if !decided {
            pending.extend_from_slice(&chunk);
            if playlist_bytes(&pending) {
                return Ok(Streamed::Playlist);
            }
            if pending.len() < 16 {
                continue;
            }
            staged.push(&pending)?;
            progress(staged.written, total);
            pending.clear();
            decided = true;
            continue;
        }
        staged.push(&chunk)?;
        progress(staged.written, total);
    }
    if !decided {
        if playlist_bytes(&pending) {
            return Ok(Streamed::Playlist);
        }
        if pending.len() < 16 {
            return Err("返回的内容不是视频".into());
        }
        staged.push(&pending)?;
    }
    staged.commit(expected).map(Streamed::Bytes)
}

/// 给测试用的内存落盘，与 `apply_body` 同一规则。
pub fn commit_memory(existing: &[u8], decision: &Decision, body: &[u8]) -> Result<Vec<u8>, &'static str> {
    apply_body(existing, &decision.plan, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download_plan::Validators;

    #[test]
    fn staged_append_refuses_a_gap_and_replace_leaves_the_original_until_commit() {
        let dir = std::env::temp_dir().join(format!("hghub-dl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("media.bin");
        fs::write(&dest, b"AAAA").unwrap();
        let err = Staged::begin(&dest, &BodyPlan::Append { at: 2 }).err().unwrap();
        assert!(err.contains("停止拼接"), "{err}");
        assert_eq!(fs::read(&dest).unwrap(), b"AAAA");

        let mut staged = Staged::begin(&dest, &BodyPlan::Replace).unwrap();
        staged.push(b"HELLO").unwrap();
        drop(staged);
        assert_eq!(fs::read(&dest).unwrap(), b"AAAA", "未完成的完整重下不能覆盖断点");
        assert!(!dest.with_extension("restart").exists());

        let mut staged = Staged::begin(&dest, &BodyPlan::Append { at: 4 }).unwrap();
        staged.push(b"BB").unwrap();
        let len = staged.commit(Some(6)).unwrap();
        assert_eq!(len, 6);
        assert_eq!(fs::read(&dest).unwrap(), b"AAAABB");

        let stored = Validators { etag: Some("\"v1\"".into()), last_modified: None, total: Some(6) };
        let decision = plan_resume(4, &stored, 200, None, Some("\"v1\""), None, Some(6));
        assert_eq!(commit_memory(b"AAAA", &decision, b"HELLO!").unwrap(), b"HELLO!");
        let _ = fs::remove_dir_all(&dir);
    }
}
