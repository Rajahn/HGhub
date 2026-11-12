//! 当前这一集的离线保存。
//!
//! 只保存用户正在播放的那一个地址，不批量抓取。普通渐进文件直接落盘；
//! 普通 HLS（无加密，或播放器本来就会解的 AES-128 identity 密钥）会把分片
//! 收成一个可播放文件。SAMPLE-AES、Widevine、PlayReady、FairPlay 直接拒绝。

use crate::net::fetch_bytes;
use crate::ts_mp4;
use aes::cipher::{BlockDecrypt, KeyInit};
#[cfg(test)]
use aes::cipher::BlockEncrypt;
use aes::Aes128;
use serde::Serialize;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};

const MAX_BYTES: usize = 800 * 1024 * 1024;
const MAX_SEGMENTS: usize = 4000;

#[derive(Clone, Serialize)]
pub struct DownloadView {
    pub id: u64,
    pub title: String,
    pub url: String,
    pub episode: u32,
    pub phase: String,
    pub received: u64,
    pub total: u64,
    pub index: u32,
    pub count: u32,
    pub message: String,
    pub path: String,
    pub token: String,
    pub bytes: u64,
}

struct Job {
    view: DownloadView,
    cancel: Arc<AtomicBool>,
}

pub struct Downloads {
    jobs: Arc<Mutex<Vec<Job>>>,
}

impl Downloads {
    pub fn new() -> Self {
        Self { jobs: Arc::new(Mutex::new(Vec::new())) }
    }
}

pub struct MediaFile {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
    pub mime: &'static str,
    pub note: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    None,
    Aes128,
}

struct KeySpec {
    method: Method,
    uri: String,
    iv: Option<[u8; 16]>,
}

struct Segment {
    url: String,
    seq: u64,
    key: Option<usize>,
    map: Option<String>,
}

struct MediaPlaylist {
    segments: Vec<Segment>,
    keys: Vec<KeySpec>,
    ended: bool,
}

struct Variant {
    bandwidth: u64,
    codecs: String,
    url: String,
}

enum Playlist {
    Master(Vec<Variant>),
    Media(MediaPlaylist),
}

pub async fn build_media<F, Fut>(url: &str, fetch: F, cancel: &AtomicBool, mut progress: impl FnMut(u64, u32, u32)) -> Result<MediaFile, String>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, String>>,
{
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    let bytes = fetch(url.to_string()).await?;
    progress(bytes.len() as u64, 0, 0);
    if looks_like_playlist(url, &bytes) {
        build_hls(url, &bytes, fetch, cancel, progress).await
    } else {
        direct_file(url, bytes)
    }
}

fn looks_like_playlist(url: &str, bytes: &[u8]) -> bool {
    let text = std::str::from_utf8(bytes).unwrap_or("");
    text.trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U")
        || url.split('?').next().unwrap_or(url).to_ascii_lowercase().ends_with(".m3u8")
}

fn direct_file(url: &str, bytes: Vec<u8>) -> Result<MediaFile, String> {
    if bytes.len() < 16 {
        return Err("返回的内容不是视频".into());
    }
    if bytes.starts_with(b"ftyp") || bytes.get(4..8) == Some(b"ftyp") || bytes.starts_with(b"\0\0\0") && bytes.get(4..8) == Some(b"ftyp") {
        return Ok(MediaFile { bytes, extension: "mp4", mime: "video/mp4", note: String::new() });
    }
    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".webm") || bytes.starts_with(b"\x1a\x45\xdf\xa3") {
        return Ok(MediaFile { bytes, extension: "webm", mime: "video/webm", note: String::new() });
    }
    if path.ends_with(".mp4") || path.ends_with(".m4v") || path.ends_with(".mov") {
        return Ok(MediaFile { bytes, extension: "mp4", mime: "video/mp4", note: String::new() });
    }
    if bytes[0] == 0x47 {
        return package_ts(bytes);
    }
    Err("这个地址不是可保存的视频文件".into())
}

async fn build_hls<F, Fut>(url: &str, first: &[u8], fetch: F, cancel: &AtomicBool, mut progress: impl FnMut(u64, u32, u32)) -> Result<MediaFile, String>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, String>>,
{
    let text = std::str::from_utf8(first).map_err(|_| "播放列表不是文本")?;
    let playlist = parse_playlist(url, text)?;
    let media = match playlist {
        Playlist::Media(media) => media,
        Playlist::Master(variants) => {
            let variant = pick_variant(&variants).ok_or("播放列表里没有可下载的视频流")?;
            if cancel.load(Ordering::Relaxed) {
                return Err("已取消".into());
            }
            let nested = fetch(variant.url.clone()).await?;
            let nested_text = std::str::from_utf8(&nested).map_err(|_| "播放列表不是文本")?;
            match parse_playlist(&variant.url, nested_text)? {
                Playlist::Media(media) => media,
                Playlist::Master(_) => return Err("嵌套的播放列表无法展开".into()),
            }
        }
    };
    if !media.ended {
        return Err("这是未结束的播放列表，不能保存成一个完整文件".into());
    }
    if media.segments.is_empty() {
        return Err("播放列表里没有分片".into());
    }
    if media.segments.len() > MAX_SEGMENTS {
        return Err("分片太多，已停止下载".into());
    }
    let mut key_bytes: Vec<Option<Vec<u8>>> = Vec::new();
    for key in &media.keys {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        if key.method == Method::Aes128 {
            let raw = fetch(key.uri.clone()).await?;
            if raw.len() != 16 {
                return Err("加密密钥长度不对，无法保存".into());
            }
            key_bytes.push(Some(raw));
        } else {
            key_bytes.push(None);
        }
    }
    let total = media.segments.len() as u32;
    let mut received = 0u64;
    let mut plain = Vec::new();
    let mut init: Option<Vec<u8>> = None;
    let mut init_url: Option<String> = None;
    for (index, segment) in media.segments.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        if let Some(map) = &segment.map {
            if init_url.as_deref() != Some(map.as_str()) {
                let bytes = fetch(map.clone()).await?;
                init = Some(bytes);
                init_url = Some(map.clone());
            }
        }
        let mut body = fetch(segment.url.clone()).await?;
        if let Some(key_index) = segment.key {
            let key = key_bytes.get(key_index).and_then(|k| k.as_ref()).ok_or("缺少解密密钥")?;
            let iv = media.keys[key_index].iv.unwrap_or_else(|| sequence_iv(segment.seq));
            body = aes128_cbc_decrypt(&body, key, &iv)?;
        }
        received += body.len() as u64;
        if received as usize > MAX_BYTES {
            return Err("视频超过 800MB，已停止下载".into());
        }
        plain.extend(body);
        progress(received, index as u32 + 1, total);
    }
    if let Some(init) = init {
        let mut all = init;
        all.extend(plain);
        if all.len() > MAX_BYTES {
            return Err("视频超过 800MB，已停止下载".into());
        }
        return Ok(MediaFile {
            bytes: all,
            extension: "mp4",
            mime: "video/mp4",
            note: "已合并为 MP4".into(),
        });
    }
    if plain.first().copied() == Some(0x47) {
        package_ts(plain)
    } else if plain.get(4..8) == Some(b"ftyp") || plain.get(4..8) == Some(b"styp") || plain.get(4..8) == Some(b"moof") {
        Ok(MediaFile { bytes: plain, extension: "mp4", mime: "video/mp4", note: "已合并为 MP4".into() })
    } else {
        Err("分片不是可识别的视频数据".into())
    }
}

fn package_ts(ts: Vec<u8>) -> Result<MediaFile, String> {
    match ts_mp4::transmux_ts(&ts) {
        Ok(mp4) => Ok(MediaFile {
            bytes: mp4.bytes,
            extension: "mp4",
            mime: "video/mp4",
            note: "已保存为 MP4".into(),
        }),
        Err(_) => Ok(MediaFile {
            bytes: ts,
            extension: "ts",
            mime: "video/mp2t",
            note: "这一集不是可封装的 H.264，已保存为 MPEG-TS。系统相册可能打不开，可用支持 TS 的播放器打开".into(),
        }),
    }
}

fn pick_variant(variants: &[Variant]) -> Option<&Variant> {
    variants.iter().max_by_key(|v| {
        let avc = if v.codecs.to_ascii_lowercase().contains("avc1") { 1u8 } else { 0 };
        (avc, v.bandwidth)
    })
}

fn parse_playlist(base: &str, text: &str) -> Result<Playlist, String> {
    let text = text.trim_start_matches('\u{feff}');
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err("响应不是 HLS 播放列表".into());
    }
    let mut variants = Vec::new();
    let mut pending_inf: Option<(u64, String)> = None;
    let mut segments = Vec::new();
    let mut keys: Vec<KeySpec> = Vec::new();
    let mut current_key: Option<usize> = None;
    let mut current_map: Option<String> = None;
    let mut sequence = 0u64;
    let mut ended = false;
    let mut saw_media = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let bandwidth = attr(rest, "BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0);
            let codecs = attr(rest, "CODECS").unwrap_or_default();
            pending_inf = Some((bandwidth, codecs));
            continue;
        }
        if line.starts_with("#EXT-X-KEY:") {
            let spec = parse_key(base, line)?;
            match spec.method {
                Method::None => current_key = None,
                Method::Aes128 => {
                    keys.push(spec);
                    current_key = Some(keys.len() - 1);
                }
            }
            continue;
        }
        if line.starts_with("#EXT-X-MAP:") {
            let uri = attr(line.split_once(':').map(|(_, r)| r).unwrap_or(""), "URI")
                .ok_or("初始化分片缺少地址")?;
            current_map = Some(resolve(base, &uri));
            continue;
        }
        if line.starts_with("#EXT-X-BYTERANGE:") {
            return Err("这种播放列表使用了字节范围，当前不能离线保存".into());
        }
        if line.starts_with("#EXT-X-MEDIA-SEQUENCE:") {
            if let Some(n) = line.split_once(':').and_then(|(_, n)| n.trim().parse().ok()) {
                sequence = n;
            }
            continue;
        }
        if line.starts_with("#EXTINF:") {
            saw_media = true;
            continue;
        }
        if line == "#EXT-X-ENDLIST" {
            ended = true;
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let absolute = resolve(base, line);
        if let Some((bandwidth, codecs)) = pending_inf.take() {
            variants.push(Variant { bandwidth, codecs, url: absolute });
        } else {
            saw_media = true;
            segments.push(Segment { url: absolute, seq: sequence, key: current_key, map: current_map.clone() });
            sequence = sequence.saturating_add(1);
        }
    }
    if !variants.is_empty() && !saw_media {
        return Ok(Playlist::Master(variants));
    }
    Ok(Playlist::Media(MediaPlaylist { segments, keys, ended }))
}

fn parse_key(base: &str, line: &str) -> Result<KeySpec, String> {
    let rest = line.split_once(':').map(|(_, r)| r).unwrap_or("");
    let method = attr(rest, "METHOD").unwrap_or_default();
    let format = attr(rest, "KEYFORMAT").unwrap_or_default();
    let uri = attr(rest, "URI").unwrap_or_default();
    if !format.is_empty() && !format.eq_ignore_ascii_case("identity") {
        return Err(protected_message(&format));
    }
    let method = match method.to_ascii_uppercase().as_str() {
        "NONE" => Method::None,
        "AES-128" => Method::Aes128,
        "SAMPLE-AES" | "SAMPLE-AES-CTR" => return Err("这是样本级加密（DRM），不能下载".into()),
        other if other.is_empty() => Method::None,
        other => return Err(format!("不支持的加密方式 {other}，不能下载")),
    };
    if method == Method::Aes128 && uri.is_empty() {
        return Err("加密播放列表没有密钥地址".into());
    }
    if uri_is_drm(&uri) {
        return Err(protected_message(&uri));
    }
    let iv = attr(rest, "IV").and_then(|v| parse_iv(&v));
    if attr(rest, "IV").is_some() && iv.is_none() {
        return Err("加密向量无法识别".into());
    }
    Ok(KeySpec { method, uri: if uri.is_empty() { String::new() } else { resolve(base, &uri) }, iv })
}

fn protected_message(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let kind = if lower.contains("widevine") || lower.contains("edef8ba9") {
        "Widevine"
    } else if lower.contains("playready") || lower.contains("9a04f079") {
        "PlayReady"
    } else if lower.contains("fairplay") || lower.contains("skd") || lower.contains("streamingkeydelivery") {
        "FairPlay"
    } else {
        "DRM"
    };
    format!("这是受保护的 {kind} 流，不能下载")
}

fn uri_is_drm(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.starts_with("skd:") || lower.contains("widevine") || lower.contains("playready") || lower.contains("fairplay")
}

fn parse_iv(raw: &str) -> Option<[u8; 16]> {
    let hex = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() > 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let padded = format!("{hex:0>32}");
    let mut iv = [0u8; 16];
    for i in 0..16 {
        iv[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

fn sequence_iv(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

fn attr(list: &str, name: &str) -> Option<String> {
    for part in split_attrs(list) {
        let Some((key, value)) = part.split_once('=') else { continue };
        if key.trim() == name {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn split_attrs(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, ch) in list.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        }
        if ch == ',' && !quoted {
            out.push(&list[start..i]);
            start = i + 1;
        }
    }
    if start <= list.len() {
        out.push(&list[start..]);
    }
    out
}

fn resolve(base: &str, target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }
    url::Url::parse(base)
        .ok()
        .and_then(|base| base.join(target).ok())
        .map(|url| url.to_string())
        .unwrap_or_else(|| target.to_string())
}

pub fn aes128_cbc_decrypt(data: &[u8], key: &[u8], iv: &[u8; 16]) -> Result<Vec<u8>, String> {
    if key.len() != 16 || data.is_empty() || data.len() % 16 != 0 {
        return Err("加密分片无法解密".into());
    }
    let cipher = Aes128::new_from_slice(key).map_err(|_| "密钥无效")?;
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let ct = block;
        cipher.decrypt_block((&mut block).into());
        for i in 0..16 {
            out.push(block[i] ^ prev[i]);
        }
        prev = ct;
    }
    Ok(strip_pkcs7(out))
}

fn strip_pkcs7(buf: Vec<u8>) -> Vec<u8> {
    let Some(pad) = buf.last().copied() else { return buf };
    let pad = pad as usize;
    if pad == 0 || pad > 16 || buf.len() < pad {
        return buf;
    }
    if buf[buf.len() - pad..].iter().all(|b| *b as usize == pad) {
        let mut buf = buf;
        buf.truncate(buf.len() - pad);
        buf
    } else {
        buf
    }
}

#[cfg(test)]
fn aes128_cbc_encrypt(data: &[u8], key: &[u8], iv: &[u8; 16]) -> Vec<u8> {
    let mut buf = data.to_vec();
    let pad = 16 - (buf.len() % 16);
    buf.extend(std::iter::repeat_n(pad as u8, pad));
    let cipher = Aes128::new_from_slice(key).unwrap();
    let mut prev = *iv;
    let mut out = Vec::with_capacity(buf.len());
    for chunk in buf.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        cipher.encrypt_block((&mut block).into());
        out.extend_from_slice(&block);
        prev = block;
    }
    out
}

fn file_name(title: &str, episode: u32, extension: &str) -> String {
    let mut safe: String = title.chars().map(|ch| match ch {
        '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
        ch if ch.is_control() => '_',
        ch => ch,
    }).take(40).collect();
    safe = safe.trim().trim_matches('.').to_string();
    if safe.is_empty() {
        safe = "视频".into();
    }
    if episode > 0 {
        format!("{safe}_第{episode}集.{extension}")
    } else {
        format!("{safe}.{extension}")
    }
}

#[tauri::command]
pub async fn download_start(app: AppHandle, state: State<'_, Downloads>, url: String, title: String, episode: u32) -> Result<DownloadView, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("还没有可下载的视频地址".into());
    }
    crate::network_policy::parse_target(&url)?;
    let jobs = Arc::clone(&state.jobs);
    {
        let list = jobs.lock().map_err(|_| "下载状态不可用")?;
        if let Some(job) = list.iter().find(|job| job.view.phase == "running" && job.view.url == url) {
            return Ok(job.view.clone());
        }
        if list.iter().any(|job| job.view.phase == "running") {
            return Err("已有下载在进行，请先取消或等待完成".into());
        }
    }
    let id = {
        let mut list = jobs.lock().map_err(|_| "下载状态不可用")?;
        let id = list.iter().map(|job| job.view.id).max().unwrap_or(0) + 1;
        list.push(Job {
            view: DownloadView {
                id, title: title.clone(), url: url.clone(), episode, phase: "running".into(),
                received: 0, total: 0, index: 0, count: 0, message: "正在准备".into(), path: String::new(), token: String::new(), bytes: 0,
            },
            cancel: Arc::new(AtomicBool::new(false)),
        });
        if list.len() > 8 {
            let running = list.iter().position(|job| job.view.phase == "running");
            if list.len() > 8 {
                if let Some(drop_at) = list.iter().position(|job| job.view.phase != "running") {
                    if running != Some(drop_at) {
                        list.remove(drop_at);
                    }
                }
            }
        }
        id
    };
    let cancel = jobs.lock().ok().and_then(|list| list.iter().find(|job| job.view.id == id).map(|job| Arc::clone(&job.cancel))).unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let jobs_task = Arc::clone(&jobs);
    let title_task = title.clone();
    tauri::async_runtime::spawn(async move {
        let cache = app.path().app_cache_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
        let _ = std::fs::create_dir_all(&cache);
        let progress_jobs = Arc::clone(&jobs_task);
        let result = build_media(&url, |target| async move { fetch_bytes(&target).await.map(|(_, bytes)| bytes) }, &cancel, move |received, index, count| {
            if let Ok(mut list) = progress_jobs.lock() {
                if let Some(job) = list.iter_mut().find(|job| job.view.id == id) {
                    job.view.received = received;
                    job.view.index = index;
                    job.view.count = count;
                    job.view.message = if count == 0 { "正在连接".into() } else { format!("正在下载 {index}/{count}") };
                }
            }
        }).await;
        let mut list = match jobs_task.lock() {
            Ok(list) => list,
            Err(_) => return,
        };
        let Some(job) = list.iter_mut().find(|job| job.view.id == id) else { return };
        if cancel.load(Ordering::Relaxed) {
            job.view.phase = "cancelled".into();
            job.view.message = "已取消".into();
            return;
        }
        match result {
            Ok(file) => {
                let name = file_name(&title_task, episode, file.extension);
                let temp = cache.join(format!("hghub-dl-{id}.part"));
                if std::fs::write(&temp, &file.bytes).is_err() {
                    job.view.phase = "failed".into();
                    job.view.message = "无法写入临时文件".into();
                    return;
                }
                match crate::local_media::publish(&temp, &name, file.mime) {
                    Ok(saved) => {
                        job.view.phase = "done".into();
                        job.view.message = if file.note.is_empty() {
                            format!("已保存到 {}", saved.location)
                        } else {
                            format!("{}。{}", file.note, saved.location)
                        };
                        job.view.path = saved.location;
                        job.view.token = saved.token;
                        job.view.bytes = file.bytes.len() as u64;
                        job.view.received = file.bytes.len() as u64;
                    }
                    Err(err) => {
                        job.view.phase = "failed".into();
                        job.view.message = err;
                    }
                }
                let _ = std::fs::remove_file(&temp);
            }
            Err(err) if err == "已取消" => {
                job.view.phase = "cancelled".into();
                job.view.message = "已取消".into();
            }
            Err(err) => {
                job.view.phase = "failed".into();
                job.view.message = err;
            }
        }
    });
    let list = jobs.lock().map_err(|_| "下载状态不可用")?;
    list.iter().find(|job| job.view.id == id).map(|job| job.view.clone()).ok_or_else(|| "下载没有开始".into())
}

#[tauri::command]
pub fn download_cancel(state: State<'_, Downloads>, id: u64) -> Result<(), String> {
    let list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let job = list.iter().find(|job| job.view.id == id).ok_or("找不到这个下载")?;
    job.cancel.store(true, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
pub fn download_list(state: State<'_, Downloads>) -> Result<Vec<DownloadView>, String> {
    let list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    Ok(list.iter().map(|job| job.view.clone()).collect())
}

#[tauri::command]
pub fn download_open(state: State<'_, Downloads>, id: u64) -> Result<(), String> {
    let list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let job = list.iter().find(|job| job.view.id == id).ok_or("找不到这个下载")?;
    if job.view.phase != "done" {
        return Err("下载还没有完成".into());
    }
    let token = if job.view.token.is_empty() { job.view.path.clone() } else { job.view.token.clone() };
    let mime = if job.view.path.ends_with(".ts") { "video/mp2t" } else { "video/mp4" };
    drop(list);
    crate::local_media::open(&token, mime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn rejects_drm_and_accepts_plain_aes128_playlist() {
        let drm = parse_playlist("https://cdn.example/a.m3u8", "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n");
        let drm = match drm { Err(err) => err, Ok(_) => panic!("DRM playlist was accepted") };
        assert!(drm.contains("不能下载"), "{drm}");
        let widevine = parse_playlist("https://cdn.example/a.m3u8", "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"https://license.example/widevine\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\"\n#EXTINF:4,\na.ts\n");
        let widevine = match widevine { Err(err) => err, Ok(_) => panic!("Widevine playlist was accepted") };
        assert!(widevine.contains("Widevine") || widevine.contains("DRM"), "{widevine}");
        let ok = parse_playlist("https://cdn.example/v/index.m3u8", "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x0000000000000000000000000000002a\n#EXTINF:4,\nseg-1.ts\n#EXTINF:4,\nseg-2.ts\n#EXT-X-ENDLIST\n").unwrap();
        match ok {
            Playlist::Media(media) => {
                assert!(media.ended);
                assert_eq!(media.segments.len(), 2);
                assert!(media.segments[0].url.contains("seg-1.ts"));
                assert_eq!(media.keys[0].iv.unwrap()[15], 0x2a);
            }
            Playlist::Master(_) => panic!("expected media"),
        }
    }

    #[test]
    fn aes128_roundtrip_aligns_transport_stream() {
        let mut plain = vec![0x47, 0x11, 0x22, 0x33];
        plain.extend(std::iter::repeat(0x47).take(188 - 4));
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let enc = aes128_cbc_encrypt(&plain, &key, &iv);
        let dec = aes128_cbc_decrypt(&enc, &key, &iv).unwrap();
        assert_eq!(dec, plain);
    }

    #[tokio::test]
    async fn encrypted_hls_fixture_becomes_mp4_when_ffmpeg_exists() {
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("hghub-hls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ts_path = dir.join("seg.ts");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=30", "-f", "lavfi", "-i", "sine=frequency=880:sample_rate=44100", "-t", "1", "-shortest", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "30", "-bf", "0", "-c:a", "aac", "-f", "mpegts"])
            .arg(&ts_path)
            .status()
            .unwrap();
        assert!(status.success());
        let plain = std::fs::read(&ts_path).unwrap();
        let key = [3u8; 16];
        let iv = [4u8; 16];
        let enc = aes128_cbc_encrypt(&plain, &key, &iv);
        let mut files = HashMap::new();
        let playlist = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://cdn.example/key.bin\",IV=0x04040404040404040404040404040404\n#EXTINF:1,\nhttps://cdn.example/seg.ts\n#EXT-X-ENDLIST\n";
        files.insert("https://cdn.example/list.m3u8".to_string(), playlist.as_bytes().to_vec());
        files.insert("https://cdn.example/key.bin".to_string(), key.to_vec());
        files.insert("https://cdn.example/seg.ts".to_string(), enc);
        let files = Arc::new(files);
        let cancel = AtomicBool::new(false);
        let media = build_media("https://cdn.example/list.m3u8", {
            let files = Arc::clone(&files);
            move |url| {
                let files = Arc::clone(&files);
                async move { files.get(&url).cloned().ok_or_else(|| format!("missing {url}")) }
            }
        }, &cancel, |_, _, _| {}).await.expect("build");
        assert_eq!(media.extension, "mp4");
        let out = dir.join("out.mp4");
        std::fs::write(&out, &media.bytes).unwrap();
        let probe = std::process::Command::new("ffprobe").args(["-hide_banner", "-show_streams"]).arg(&out).output().unwrap();
        let text = String::from_utf8_lossy(&probe.stdout);
        assert!(probe.status.success() && text.contains("codec_name=h264") && text.contains("codec_name=aac"), "{text} {}", String::from_utf8_lossy(&probe.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
