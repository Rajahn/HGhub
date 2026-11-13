//! 下载续传、整剧队列和网络错误文案的纯决策。
//!
//! 这里不发网络请求。调用方按 [`BodyPlan`] 写盘：`Append` 只允许接在已有长度之后，
//! `Replace` 必须整文件替换，`Restart` / `Reject` 禁止把本次响应拼进旧文件。

use std::time::Duration;

pub const NET_ATTEMPTS: u32 = 4;
pub const CONCURRENCY: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end_inclusive: u64,
    pub total: Option<u64>,
}

/// 这一次响应体允许怎样落盘。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyPlan {
    /// 响应是完整内容。写到临时文件，完成后替换，禁止追加。
    Replace,
    /// 响应从 `at` 续写。调用方必须先核对本地长度等于 `at`。
    Append { at: u64 },
    /// 本地文件已经是完整内容，丢弃响应体。
    Complete,
    /// 响应不能写入。丢掉本地半成品，下次从头请求。
    Restart,
    /// 响应不能写入，本地半成品保持原样。
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub plan: BodyPlan,
    pub message: &'static str,
}

pub fn parse_content_range(raw: &str) -> Option<ByteRange> {
    let rest = raw.trim().strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let start = start.parse().ok()?;
    let end_inclusive = end.parse().ok()?;
    if end_inclusive < start {
        return None;
    }
    let total = if total == "*" {
        None
    } else {
        Some(total.parse().ok()?)
    };
    Some(ByteRange { start, end_inclusive, total })
}

fn validator_changed(stored: &Validators, etag: Option<&str>, last_modified: Option<&str>) -> bool {
    match (&stored.etag, etag) {
        (Some(saved), Some(got)) if saved != got => return true,
        _ => {}
    }
    match (&stored.last_modified, last_modified) {
        (Some(saved), Some(got)) if saved != got => return true,
        _ => {}
    }
    false
}

/// 根据已有字节、校验信息和这次响应，决定能不能拼接。
pub fn plan_resume(
    have: u64,
    stored: &Validators,
    status: u16,
    content_range: Option<&ByteRange>,
    response_etag: Option<&str>,
    response_lm: Option<&str>,
    content_length: Option<u64>,
) -> Decision {
    if have == 0 {
        return Decision { plan: BodyPlan::Replace, message: "开始下载" };
    }
    if status == 416 {
        if stored.total == Some(have) {
            return Decision { plan: BodyPlan::Complete, message: "本地文件已经完整" };
        }
        return Decision {
            plan: BodyPlan::Restart,
            message: "服务器拒绝了断点范围，将重新下载，不会拼接旧数据",
        };
    }
    let changed = validator_changed(stored, response_etag, response_lm);
    if status == 206 {
        let Some(range) = content_range else {
            return Decision {
                plan: BodyPlan::Restart,
                message: "服务器没有说明字节范围，已停止拼接",
            };
        };
        if let Some(len) = content_length {
            let span = range.end_inclusive.saturating_sub(range.start).saturating_add(1);
            if len != span {
                return Decision {
                    plan: BodyPlan::Reject,
                    message: "响应长度和字节范围不一致，已停止拼接",
                };
            }
        }
        if changed || stored.total.zip(range.total).is_some_and(|(saved, got)| saved != got) || range.start != have {
            return Decision {
                plan: BodyPlan::Restart,
                message: "服务器内容或范围与本地断点不一致，已停止拼接并准备重新下载",
            };
        }
        return Decision {
            plan: BodyPlan::Append { at: have },
            message: "从断点继续",
        };
    }
    if status == 200 {
        if !changed && content_length == Some(have) && stored.total == Some(have) {
            return Decision { plan: BodyPlan::Complete, message: "本地文件已经完整" };
        }
        return Decision {
            plan: BodyPlan::Replace,
            message: "服务器没有按断点返回，将重新保存完整文件，不会拼到已有数据后面",
        };
    }
    Decision { plan: BodyPlan::Reject, message: "无法确认续传响应，已停止拼接" }
}

/// 按计划生成新文件内容。`Append` 起点不对、`Restart`/`Reject` 都不会改旧字节。
pub fn apply_body(existing: &[u8], plan: &BodyPlan, body: &[u8]) -> Result<Vec<u8>, &'static str> {
    match plan {
        BodyPlan::Append { at } => {
            if existing.len() as u64 != *at {
                return Err("本地长度与断点不一致，拒绝拼接");
            }
            let mut out = existing.to_vec();
            out.extend_from_slice(body);
            Ok(out)
        }
        BodyPlan::Replace => Ok(body.to_vec()),
        BodyPlan::Complete => Ok(existing.to_vec()),
        BodyPlan::Restart | BodyPlan::Reject => Err("这次响应禁止写入"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HlsIdentity {
    pub sequence: u64,
    pub count: u32,
    pub ended: bool,
}

/// 指纹忽略查询串，这样只换签名的同一分片可以续；路径、序号或集数变了就必须重下。
pub fn hls_fingerprint(identity: HlsIdentity, paths: &[impl AsRef<str>]) -> String {
    let mut raw = format!("seq={};n={};end={};", identity.sequence, identity.count, identity.ended as u8);
    for path in paths {
        raw.push_str(path_without_query(path.as_ref()));
        raw.push('\n');
    }
    format!("{:016x}", fnv64(raw.as_bytes()))
}

pub fn path_without_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 指纹一致才允许按序号复用已完成分片。不一致时一个旧分片都不能进入拼接。
pub fn reusable_segments(saved_fingerprint: Option<&str>, fingerprint: &str, complete: &[u32], count: u32) -> Vec<u32> {
    if saved_fingerprint != Some(fingerprint) {
        return Vec::new();
    }
    let mut kept: Vec<u32> = complete.iter().copied().filter(|index| *index < count).collect();
    kept.sort_unstable();
    kept.dedup();
    kept
}

pub fn assembly_indexes(reusable: &[u32], count: u32) -> Option<Vec<u32>> {
    if count == 0 || reusable.len() != count as usize {
        return None;
    }
    if (0..count).all(|index| reusable.contains(&index)) {
        Some((0..count).collect())
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSlot {
    pub id: u64,
    pub video_id: i64,
    pub episode: u32,
    pub phase: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeriesAction {
    Skip { id: u64 },
    Revive { id: u64 },
    Create { episode: u32 },
}

pub fn plan_series(existing: &[QueueSlot], video_id: i64, episodes: &[u32]) -> Vec<SeriesAction> {
    let mut episodes: Vec<u32> = episodes.iter().copied().filter(|ep| *ep > 0).collect();
    episodes.sort_unstable();
    episodes.dedup();
    episodes
        .into_iter()
        .map(|episode| {
            let hit = existing.iter().find(|job| job.video_id == video_id && job.episode == episode);
            match hit {
                Some(job) if matches!(job.phase.as_str(), "queued" | "running" | "paused" | "done") => {
                    SeriesAction::Skip { id: job.id }
                }
                Some(job) => SeriesAction::Revive { id: job.id },
                None => SeriesAction::Create { episode },
            }
        })
        .collect()
}

pub fn claim_slots(order: &[(u64, &str)], busy: &[u64], limit: usize) -> Vec<u64> {
    let mut out = Vec::new();
    if busy.len() >= limit {
        return out;
    }
    for (id, phase) in order {
        if out.len() + busy.len() >= limit {
            break;
        }
        if *phase == "queued" && !busy.contains(id) && !out.contains(id) {
            out.push(*id);
        }
    }
    out
}

pub fn mark_interrupted(phase: &str) -> Option<(&'static str, &'static str)> {
    if phase == "running" {
        Some((
            "paused",
            "上次下载因进程结束已暂停。点「继续」可从断点续传。系统强制结束或限制后台后，需要手动继续。",
        ))
    } else {
        None
    }
}

pub fn attempts_for(retry: bool) -> u32 {
    if retry { NET_ATTEMPTS } else { 1 }
}

pub fn retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(400u64 << attempt.min(4))
}

pub fn should_refresh_client(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("proxy")
        || lower.contains("tunnel")
        || lower.contains("connect")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("refused")
        || lower.contains("dns")
        || lower.contains("reset")
        || lower.contains("broken pipe")
        || lower.contains("sending request")
        || lower.contains("connection closed")
        || lower.contains("incomplete")
        || lower.contains("http2")
        || lower.contains("goaway")
}

/// 只有带剧集编号、且播放接口会重新签发地址时才换源。超时仍用原地址续传。
pub fn should_refresh_source(raw: &str, video_id: i64) -> bool {
    if video_id <= 0 {
        return false;
    }
    let lower = raw.to_ascii_lowercase();
    if raw.contains("禁止") || raw.contains("内网") || raw.contains("证书") || lower.contains("certificate") {
        return false;
    }
    http_status(raw).is_some_and(|code| matches!(code, 401 | 403 | 410))
}

pub fn http_status(raw: &str) -> Option<u16> {
    raw.trim()
        .strip_prefix("HTTP ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|code| code.parse().ok())
}

pub fn user_facing_net_error(raw: &str) -> String {
    if let Some(code) = http_status(raw) {
        return match code {
            401 | 403 => format!("HTTP {code} 没有访问权限或播放地址已失效"),
            404 => format!("HTTP {code} 找不到内容"),
            408 | 429 => format!("HTTP {code} 请求过于频繁或超时，请稍后重试"),
            410 => format!("HTTP {code} 播放地址已失效"),
            500..=599 => format!("HTTP {code} 服务暂时不可用，请稍后重试"),
            _ => format!("HTTP {code} 请求失败"),
        };
    }
    let lower = raw.to_ascii_lowercase();
    if lower.contains("certificate") || lower.contains("cert") || raw.contains("证书") {
        return "证书校验失败。请检查系统时间，或关闭代理的 HTTPS 解密。应用不会跳过证书校验。".into();
    }
    if lower.contains("timed out") || lower.contains("timeout") || raw.contains("超时") {
        return "连接超时。网络不稳定时可稍后继续，已下载的部分会保留。".into();
    }
    if lower.contains("dns") || raw.contains("解析") {
        return "域名解析失败。可切换网络，或在设置里更换线路。".into();
    }
    if lower.contains("reset") || lower.contains("broken pipe") || lower.contains("connection closed") || lower.contains("incomplete") {
        return "连接被中断。已保留断点，可点继续续传。".into();
    }
    if lower.contains("refused") {
        return "连接被拒绝。请检查网络或代理是否仍在运行。".into();
    }
    if lower.contains("error sending request") || lower.contains("sending request") {
        return "发送请求失败。多为连接被重置或线路抖动，可稍后重试；已下载的部分会保留。".into();
    }
    if raw.chars().any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch)) {
        return raw.chars().take(90).collect();
    }
    "网络请求失败，请稍后重试".into()
}

pub fn resumable_failure(raw: &str) -> bool {
    if raw.contains("不能下载") || raw.contains("DRM") || raw.contains("不是") || raw.contains("分片太多") || raw.contains("800MB") {
        return false;
    }
    if raw.contains("禁止") || raw.contains("内网") {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(etag: &str, total: u64) -> Validators {
        Validators { etag: Some(etag.into()), last_modified: Some("Mon".into()), total: Some(total) }
    }

    #[test]
    fn range_append_requires_matching_start_and_validators() {
        let stored = vals("\"v1\"", 10);
        let range = ByteRange { start: 4, end_inclusive: 9, total: Some(10) };
        let decision = plan_resume(4, &stored, 206, Some(&range), Some("\"v1\""), Some("Mon"), Some(6));
        assert_eq!(decision.plan, BodyPlan::Append { at: 4 });
        assert_eq!(apply_body(b"AAAA", &decision.plan, b"BBBBBB").unwrap(), b"AAAABBBBBB");
    }

    #[test]
    fn ignored_range_replaces_instead_of_appending() {
        let stored = vals("\"v1\"", 10);
        let decision = plan_resume(4, &stored, 200, None, Some("\"v1\""), Some("Mon"), Some(10));
        assert_eq!(decision.plan, BodyPlan::Replace);
        assert_eq!(apply_body(b"AAAA", &decision.plan, b"HELLOHELLO").unwrap(), b"HELLOHELLO");
    }

    #[test]
    fn changed_etag_or_mismatched_range_never_appends() {
        let stored = vals("\"v1\"", 10);
        let shifted = ByteRange { start: 0, end_inclusive: 9, total: Some(10) };
        let changed = plan_resume(4, &stored, 206, Some(&shifted), Some("\"v2\""), Some("Mon"), Some(10));
        assert_eq!(changed.plan, BodyPlan::Restart);
        assert!(apply_body(b"AAAA", &changed.plan, b"NEWDATA!!!").is_err());
        let start_mismatch = plan_resume(4, &stored, 206, Some(&shifted), Some("\"v1\""), Some("Mon"), Some(10));
        assert_eq!(start_mismatch.plan, BodyPlan::Restart);
        let missing = plan_resume(4, &stored, 206, None, Some("\"v1\""), Some("Mon"), Some(6));
        assert_eq!(missing.plan, BodyPlan::Restart);
        let disagree = plan_resume(4, &stored, 206, Some(&ByteRange { start: 4, end_inclusive: 9, total: Some(10) }), Some("\"v1\""), Some("Mon"), Some(4));
        assert_eq!(disagree.plan, BodyPlan::Reject);
        assert_eq!(apply_body(b"AAAA", &BodyPlan::Reject, b"XXXX").unwrap_err(), "这次响应禁止写入");
    }

    #[test]
    fn complete_file_is_not_rewritten_or_extended() {
        let stored = vals("\"v1\"", 4);
        let done = plan_resume(4, &stored, 416, None, None, None, None);
        assert_eq!(done.plan, BodyPlan::Complete);
        assert_eq!(apply_body(b"AAAA", &done.plan, b"MORE").unwrap(), b"AAAA");
        let ignored = plan_resume(4, &stored, 200, None, Some("\"v1\""), Some("Mon"), Some(4));
        assert_eq!(ignored.plan, BodyPlan::Complete);
    }

    #[test]
    fn hls_resume_ignores_signed_query_but_rejects_changed_media() {
        let id = HlsIdentity { sequence: 7, count: 2, ended: true };
        let first = hls_fingerprint(id, &["https://cdn.example/a.ts?auth=1", "https://cdn.example/b.ts?auth=1"]);
        let resigned = hls_fingerprint(id, &["https://cdn.example/a.ts?auth=2", "https://cdn.example/b.ts?auth=9"]);
        assert_eq!(first, resigned);
        assert_eq!(reusable_segments(Some(&first), &resigned, &[0], 2), vec![0]);
        assert!(assembly_indexes(&[0], 2).is_none());
        assert_eq!(assembly_indexes(&[0, 1], 2), Some(vec![0, 1]));
        let changed = hls_fingerprint(id, &["https://cdn.example/a.ts", "https://cdn.example/c.ts"]);
        assert!(reusable_segments(Some(&first), &changed, &[0, 1], 2).is_empty());
        assert!(assembly_indexes(&[], 2).is_none());
    }

    #[test]
    fn series_queue_sorts_dedups_and_keeps_a_failed_slot() {
        let existing = vec![
            QueueSlot { id: 8, video_id: 3, episode: 2, phase: "done".into() },
            QueueSlot { id: 9, video_id: 3, episode: 4, phase: "failed".into() },
        ];
        let plan = plan_series(&existing, 3, &[4, 2, 1, 1, 3]);
        assert_eq!(plan, vec![
            SeriesAction::Create { episode: 1 },
            SeriesAction::Skip { id: 8 },
            SeriesAction::Create { episode: 3 },
            SeriesAction::Revive { id: 9 },
        ]);
        let order = [(1u64, "queued"), (9, "failed"), (3, "queued"), (5, "paused")];
        assert_eq!(claim_slots(&order, &[], 2), vec![1, 3]);
        assert!(claim_slots(&order, &[1, 3], 2).is_empty());
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn restart_only_pauses_a_running_job() {
        assert!(mark_interrupted("running").unwrap().0 == "paused");
        assert!(mark_interrupted("paused").is_none());
        assert!(mark_interrupted("done").is_none());
    }

    #[test]
    fn retries_are_capped_and_only_expired_play_urls_refresh() {
        assert_eq!(attempts_for(false), 1);
        assert_eq!(attempts_for(true), NET_ATTEMPTS);
        assert!(retry_delay(0) < retry_delay(1) && retry_delay(1) < retry_delay(2));
        assert_eq!(retry_delay(4), retry_delay(9));
        assert!(should_refresh_client("error sending request for url (https://cdn.example/a) ← connection reset"));
        assert!(should_refresh_client("error sending request for url (https://cdn.example/a)"));
        assert!(!should_refresh_client("HTTP 404 Not Found"));
        assert!(should_refresh_source("HTTP 403 Forbidden", 12));
        assert!(should_refresh_source("HTTP 410 Gone", 12));
        assert!(!should_refresh_source("HTTP 403 Forbidden", 0));
        assert!(!should_refresh_source("error sending request ← operation timed out", 12));
        assert!(!should_refresh_source("证书校验失败", 12));
        let shown = user_facing_net_error("error sending request for url (https://cdn.example/a.ts) ← connection reset");
        assert!(shown.contains("连接被中断"), "{shown}");
        assert!(!shown.contains("http"));
        assert!(user_facing_net_error("error sending request for url (https://cdn.example/a)").contains("发送请求失败"));
        assert!(user_facing_net_error("HTTP 503 Service Unavailable").starts_with("HTTP 503"));
    }
}
