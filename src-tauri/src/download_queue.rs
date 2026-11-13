//! 可恢复的下载队列。进程结束后任务还在，但不会假装自己仍在后台下载。

use crate::download_io::{self, DirectOutcome};
use crate::download_plan::{
    claim_slots, mark_interrupted, plan_series, resumable_failure, should_refresh_source, user_facing_net_error, QueueSlot, SeriesAction, CONCURRENCY,
};
use crate::hls_download::{self, file_name};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::http::{Response, StatusCode};
use tauri::{AppHandle, Manager, State};

static ROOT: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Serialize, Deserialize)]
struct PersistedJob {
    id: u64,
    #[serde(default)]
    video_id: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    episode: u32,
    #[serde(default)]
    phase: String,
    #[serde(default)]
    received: u64,
    #[serde(default)]
    total: u64,
    #[serde(default)]
    index: u32,
    #[serde(default)]
    count: u32,
    #[serde(default)]
    message: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    position: f64,
    #[serde(default)]
    duration: f64,
}

#[derive(Clone, Serialize)]
pub struct DownloadView {
    pub id: u64,
    pub video_id: i64,
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
    pub position: f64,
    pub duration: f64,
}

struct Job {
    rec: PersistedJob,
    cancel: Arc<AtomicBool>,
}

pub struct Downloads {
    jobs: Arc<Mutex<Vec<Job>>>,
    root: Arc<Mutex<PathBuf>>,
    busy: Arc<Mutex<HashSet<u64>>>,
    started: AtomicBool,
}

impl Downloads {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(Vec::new())),
            root: Arc::new(Mutex::new(PathBuf::new())),
            busy: Arc::new(Mutex::new(HashSet::new())),
            started: AtomicBool::new(false),
        }
    }

    pub fn attach(&self, app: AppHandle) {
        let Some(root) = app_root(&app) else { return };
        let _ = ROOT.set(root.clone());
        {
            let mut slot = self.root.lock().unwrap_or_else(|err| err.into_inner());
            if slot.as_os_str().is_empty() {
                let loaded = load_queue(&root);
                *self.jobs.lock().unwrap_or_else(|err| err.into_inner()) = loaded;
                *slot = root;
            }
        }
        self.kick(app);
    }

    fn kick(&self, app: AppHandle) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let jobs = Arc::clone(&self.jobs);
        let root = Arc::clone(&self.root);
        let busy = Arc::clone(&self.busy);
        tauri::async_runtime::spawn(async move {
            loop {
                let claimed = claim_work(&jobs, &root, &busy);
                for id in claimed {
                    let jobs = Arc::clone(&jobs);
                    let root = Arc::clone(&root);
                    let busy = Arc::clone(&busy);
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        run_one(app, jobs, root, id).await;
                        busy.lock().unwrap_or_else(|err| err.into_inner()).remove(&id);
                    });
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }
}

fn app_root(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("hghub"));
    let root = dir.join("offline");
    fs::create_dir_all(&root).ok()?;
    Some(root)
}

fn claim_work(jobs: &Mutex<Vec<Job>>, root: &Mutex<PathBuf>, busy: &Mutex<HashSet<u64>>) -> Vec<u64> {
    let mut list = jobs.lock().unwrap_or_else(|err| err.into_inner());
    let busy_now: Vec<u64> = busy.lock().unwrap_or_else(|err| err.into_inner()).iter().copied().collect();
    let order: Vec<(u64, String)> = list.iter().map(|job| (job.rec.id, job.rec.phase.clone())).collect();
    let view: Vec<(u64, &str)> = order.iter().map(|(id, phase)| (*id, phase.as_str())).collect();
    let ids = claim_slots(&view, &busy_now, CONCURRENCY);
    if ids.is_empty() {
        return Vec::new();
    }
    let mut busy_guard = busy.lock().unwrap_or_else(|err| err.into_inner());
    for id in &ids {
        busy_guard.insert(*id);
        if let Some(job) = list.iter_mut().find(|job| job.rec.id == *id) {
            job.cancel.store(false, Ordering::Relaxed);
            job.rec.phase = "running".into();
            job.rec.message = "正在下载。系统强制结束或限制后台后，需要重新打开应用并点继续。".into();
        }
    }
    let root = root.lock().unwrap_or_else(|err| err.into_inner()).clone();
    if !root.as_os_str().is_empty() {
        save_queue(&root, &list);
    }
    ids
}

fn load_queue(root: &Path) -> Vec<Job> {
    let text = fs::read_to_string(root.join("queue.json")).unwrap_or_default();
    let mut jobs: Vec<PersistedJob> = serde_json::from_str(&text).unwrap_or_default();
    let mut changed = false;
    for job in &mut jobs {
        if let Some((phase, message)) = mark_interrupted(&job.phase) {
            job.phase = phase.into();
            job.message = message.into();
            changed = true;
        }
    }
    let loaded: Vec<Job> = jobs
        .into_iter()
        .map(|rec| Job { rec, cancel: Arc::new(AtomicBool::new(false)) })
        .collect();
    if changed {
        save_queue(root, &loaded);
    }
    loaded
}

fn save_queue(root: &Path, jobs: &[Job]) {
    let records: Vec<&PersistedJob> = jobs.iter().map(|job| &job.rec).collect();
    let Ok(bytes) = serde_json::to_vec_pretty(&records) else { return };
    let tmp = root.join("queue.json.tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(&tmp, root.join("queue.json"));
    }
}

fn view_of(job: &PersistedJob) -> DownloadView {
    DownloadView {
        id: job.id,
        video_id: job.video_id,
        title: job.title.clone(),
        url: job.url.clone(),
        episode: job.episode,
        phase: job.phase.clone(),
        received: job.received,
        total: job.total,
        index: job.index,
        count: job.count,
        message: job.message.clone(),
        path: job.path.clone(),
        token: job.token.clone(),
        bytes: job.bytes,
        position: job.position,
        duration: job.duration,
    }
}

fn slots(jobs: &[Job]) -> Vec<QueueSlot> {
    jobs.iter().map(|job| QueueSlot { id: job.rec.id, video_id: job.rec.video_id, episode: job.rec.episode, phase: job.rec.phase.clone() }).collect()
}

fn same_item(job: &PersistedJob, video_id: i64, episode: u32, url: &str) -> bool {
    if video_id > 0 {
        job.video_id == video_id && job.episode == episode
    } else {
        !url.is_empty() && job.url == url
    }
}

async fn run_one(app: AppHandle, jobs: Arc<Mutex<Vec<Job>>>, root: Arc<Mutex<PathBuf>>, id: u64) {
    let snapshot = {
        let list = jobs.lock().unwrap_or_else(|err| err.into_inner());
        list.iter().find(|job| job.rec.id == id).map(|job| (job.rec.clone(), Arc::clone(&job.cancel)))
    };
    let Some((mut rec, cancel)) = snapshot else { return };
    let root = root.lock().unwrap_or_else(|err| err.into_inner()).clone();
    let dir = root.join("jobs").join(rec.id.to_string());
    let _ = fs::create_dir_all(&dir);
    if rec.url.is_empty() && rec.video_id > 0 {
        match fresh_url(&app, rec.video_id, rec.episode).await {
            Ok(url) => {
                rec.url = url;
                update_job(&jobs, &root, id, |job| job.url.clone_from(&rec.url));
            }
            Err(err) => {
                finish(&jobs, &root, id, &cancel, Err(err));
                return;
            }
        }
    }
    let first = attempt(&jobs, &dir, &rec, &cancel).await;
    let result = match first {
        Err(err) if should_refresh_source(&err, rec.video_id) && !cancel.load(Ordering::Relaxed) => {
            match fresh_url(&app, rec.video_id, rec.episode).await {
                Ok(url) if url.starts_with("https://") && url != rec.url => {
                    rec.url = url;
                    update_job(&jobs, &root, id, |job| job.url.clone_from(&rec.url));
                    attempt(&jobs, &dir, &rec, &cancel).await
                }
                Ok(_) => Err(err),
                Err(refresh) => Err(format!("{err}（重新获取播放地址失败：{refresh}）")),
            }
        }
        other => other,
    };
    finish(&jobs, &root, id, &cancel, result);
}

async fn fresh_url(app: &AppHandle, video_id: i64, episode: u32) -> Result<String, String> {
    let api: State<crate::api::Api> = app.state();
    api.fresh_play_url(video_id, episode).await
}

struct Finished {
    file: PathBuf,
    location: String,
    token: String,
    note: String,
}

async fn attempt(
    jobs: &Arc<Mutex<Vec<Job>>>,
    dir: &Path,
    rec: &PersistedJob,
    cancel: &AtomicBool,
) -> Result<Finished, String> {
    let dest = dir.join("partial.bin");
    let meta = dir.join("direct.json");
    let id = rec.id;
    let outcome = download_io::download_direct(&rec.url, &dest, &meta, cancel, |received, total| {
        if let Ok(mut list) = jobs.lock() {
            if let Some(job) = list.iter_mut().find(|job| job.rec.id == id) {
                job.rec.received = received;
                job.rec.total = total;
                job.rec.message = if total > 0 { format!("正在下载 {}%", (received.saturating_mul(100) / total.max(1)).min(99)) } else { "正在下载".into() };
            }
        }
    }).await;
    match outcome {
        Ok(DirectOutcome::File { bytes: _ }) => {
            let ext = sniff_media(&dest).unwrap_or("mp4");
            let final_path = dir.join(format!("media.{ext}"));
            if dest != final_path {
                let _ = fs::remove_file(&final_path);
                fs::rename(&dest, &final_path).map_err(|_| "无法保存视频文件")?;
            }
            publish_copy(rec, &final_path, ext)
        }
        Ok(DirectOutcome::Playlist) => {
            let _ = fs::remove_file(&dest);
            let hls_dir = dir.join("hls");
            let fetch = |url: String| async move { crate::net::fetch_bytes(&url).await.map(|(_, bytes)| bytes) };
            let saved = hls_download::download_hls_resumable(&rec.url, &hls_dir, fetch, cancel, |_, index, count| {
                if let Ok(mut list) = jobs.lock() {
                    if let Some(job) = list.iter_mut().find(|job| job.rec.id == id) {
                        job.rec.index = index;
                        job.rec.count = count;
                        job.rec.message = format!("正在下载分片 {index}/{count}");
                    }
                }
            }).await?;
            let ext = saved.extension().and_then(|ext| ext.to_str()).unwrap_or("mp4");
            let final_path = dir.join(format!("media.{ext}"));
            if saved != final_path {
                let _ = fs::remove_file(&final_path);
                fs::rename(&saved, &final_path).map_err(|_| "无法保存视频文件")?;
            }
            publish_copy(rec, &final_path, ext)
        }
        Err(err) => Err(err),
    }
}

fn publish_copy(rec: &PersistedJob, file: &Path, ext: &str) -> Result<Finished, String> {
    let name = file_name(&rec.title, rec.episode, ext);
    let mime = if ext == "webm" { "video/webm" } else if ext == "ts" { "video/mp2t" } else { "video/mp4" };
    match crate::local_media::publish(file, &name, mime) {
        Ok(saved) => Ok(Finished { file: file.to_path_buf(), location: saved.location, token: saved.token, note: String::new() }),
        Err(err) => Ok(Finished {
            file: file.to_path_buf(),
            location: format!("应用内 {}", file.display()),
            token: String::new(),
            note: format!("应用内已保存，导出到下载目录失败：{err}"),
        }),
    }
}

fn sniff_media(path: &Path) -> Option<&'static str> {
    let mut file = File::open(path).ok()?;
    let mut head = [0u8; 16];
    let n = file.read(&mut head).ok()?;
    let head = &head[..n];
    if head.len() >= 8 && (head.starts_with(b"\x1a\x45\xdf\xa3")) {
        Some("webm")
    } else if head.get(4..8) == Some(b"ftyp") {
        Some("mp4")
    } else if head.first() == Some(&0x47) {
        Some("ts")
    } else {
        Some("mp4")
    }
}

fn finish(jobs: &Mutex<Vec<Job>>, root: &Path, id: u64, cancel: &AtomicBool, result: Result<Finished, String>) {
    let mut list = jobs.lock().unwrap_or_else(|err| err.into_inner());
    let Some(job) = list.iter_mut().find(|job| job.rec.id == id) else { return };
    if cancel.load(Ordering::Relaxed) {
        job.rec.phase = "paused".into();
        job.rec.message = "已暂停，可从断点继续。系统强制结束后需要手动点继续。".into();
    } else {
        match result {
            Ok(saved) => {
                job.rec.phase = "done".into();
                job.rec.bytes = fs::metadata(&saved.file).map(|item| item.len()).unwrap_or(job.rec.received);
                job.rec.received = job.rec.bytes;
                job.rec.path = saved.location;
                job.rec.token = saved.token;
                job.rec.message = if saved.note.is_empty() { format!("已保存到 {}", job.rec.path) } else { saved.note };
            }
            Err(err) => {
                let paused = err == "已暂停" || resumable_failure(&err);
                job.rec.phase = if paused { "paused" } else { "failed" }.into();
                let shown = explain(&err);
                job.rec.message = if paused && !shown.contains("继续") {
                    format!("{shown} 点「继续」可从断点续传。")
                } else {
                    shown
                };
            }
        }
    }
    save_queue(root, &list);
}

fn explain(err: &str) -> String {
    if err.chars().any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch)) {
        err.to_string()
    } else {
        user_facing_net_error(err)
    }
}

fn update_job(jobs: &Mutex<Vec<Job>>, root: &Path, id: u64, change: impl FnOnce(&mut PersistedJob)) {
    let mut list = jobs.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(job) = list.iter_mut().find(|job| job.rec.id == id) {
        change(&mut job.rec);
    }
    save_queue(root, &list);
}

fn enqueue(list: &mut Vec<Job>, video_id: i64, title: &str, episode: u32, url: &str) -> u64 {
    if let Some(job) = list.iter_mut().find(|job| same_item(&job.rec, video_id, episode, url)) {
        if matches!(job.rec.phase.as_str(), "queued" | "running" | "paused" | "done") {
            return job.rec.id;
        }
        job.rec.phase = "queued".into();
        job.rec.message = "等待下载".into();
        job.cancel.store(false, Ordering::Relaxed);
        if !url.is_empty() {
            job.rec.url = url.to_string();
        }
        if !title.is_empty() {
            job.rec.title = title.to_string();
        }
        return job.rec.id;
    }
    let id = list.iter().map(|job| job.rec.id).max().unwrap_or(0) + 1;
    list.push(Job {
        rec: PersistedJob {
            id,
            video_id,
            title: title.to_string(),
            url: url.to_string(),
            episode,
            phase: "queued".into(),
            received: 0,
            total: 0,
            index: 0,
            count: 0,
            message: "等待下载".into(),
            path: String::new(),
            token: String::new(),
            bytes: 0,
            position: 0.0,
            duration: 0.0,
        },
        cancel: Arc::new(AtomicBool::new(false)),
    });
    id
}

#[tauri::command]
pub async fn download_start(app: AppHandle, state: State<'_, Downloads>, url: String, title: String, episode: u32, video_id: Option<i64>) -> Result<DownloadView, String> {
    state.attach(app);
    let url = url.trim().to_string();
    let video_id = video_id.unwrap_or(0);
    if url.is_empty() && video_id <= 0 {
        return Err("还没有可下载的视频地址".into());
    }
    if !url.is_empty() {
        crate::network_policy::parse_target(&url)?;
    }
    let mut list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let id = enqueue(&mut list, video_id, &title, episode, &url);
    let root = state.root.lock().map_err(|_| "下载状态不可用")?.clone();
    save_queue(&root, &list);
    list.iter().find(|job| job.rec.id == id).map(|job| view_of(&job.rec)).ok_or_else(|| "下载没有开始".into())
}

#[tauri::command]
pub async fn download_series(app: AppHandle, state: State<'_, Downloads>, video_id: i64, title: String, episodes: Vec<u32>) -> Result<Vec<DownloadView>, String> {
    state.attach(app);
    if video_id <= 0 {
        return Err("还不知道这是哪一部剧".into());
    }
    if episodes.is_empty() {
        return Err("这部剧没有可下载的集数".into());
    }
    let mut list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let actions = plan_series(&slots(&list), video_id, &episodes);
    let mut ids = Vec::new();
    for action in actions {
        match action {
            SeriesAction::Skip { id } => ids.push(id),
            SeriesAction::Revive { id } => {
                if let Some(job) = list.iter_mut().find(|job| job.rec.id == id) {
                    job.rec.phase = "queued".into();
                    job.rec.message = "等待下载".into();
                    job.cancel.store(false, Ordering::Relaxed);
                    ids.push(id);
                }
            }
            SeriesAction::Create { episode } => ids.push(enqueue(&mut list, video_id, &title, episode, "")),
        }
    }
    let root = state.root.lock().map_err(|_| "下载状态不可用")?.clone();
    save_queue(&root, &list);
    Ok(list.iter().filter(|job| ids.contains(&job.rec.id)).map(|job| view_of(&job.rec)).collect())
}

#[tauri::command]
pub fn download_pause(state: State<'_, Downloads>, id: u64) -> Result<(), String> {
    let mut list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let job = list.iter_mut().find(|job| job.rec.id == id).ok_or("找不到这个下载")?;
    job.cancel.store(true, Ordering::Relaxed);
    if job.rec.phase == "running" || job.rec.phase == "queued" {
        job.rec.phase = "paused".into();
        job.rec.message = "已暂停，可从断点继续。".into();
    }
    let root = state.root.lock().map_err(|_| "下载状态不可用")?.clone();
    save_queue(&root, &list);
    Ok(())
}

#[tauri::command]
pub fn download_cancel(state: State<'_, Downloads>, id: u64) -> Result<(), String> {
    download_pause(state, id)
}

#[tauri::command]
pub fn download_resume(state: State<'_, Downloads>, id: u64) -> Result<(), String> {
    let mut list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let job = list.iter_mut().find(|job| job.rec.id == id).ok_or("找不到这个下载")?;
    if job.rec.phase == "done" {
        return Ok(());
    }
    job.cancel.store(false, Ordering::Relaxed);
    job.rec.phase = "queued".into();
    job.rec.message = "等待继续".into();
    let root = state.root.lock().map_err(|_| "下载状态不可用")?.clone();
    save_queue(&root, &list);
    Ok(())
}

#[tauri::command]
pub fn download_delete(state: State<'_, Downloads>, id: u64) -> Result<(), String> {
    let mut list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let Some(pos) = list.iter().position(|job| job.rec.id == id) else {
        return Err("找不到这个下载".into());
    };
    list[pos].cancel.store(true, Ordering::Relaxed);
    let token = list[pos].rec.token.clone();
    list.remove(pos);
    let root = state.root.lock().map_err(|_| "下载状态不可用")?.clone();
    save_queue(&root, &list);
    drop(list);
    let _ = fs::remove_dir_all(root.join("jobs").join(id.to_string()));
    if !token.is_empty() {
        crate::local_media::remove(&token).map_err(|_| "应用内文件已删除，系统下载目录中的副本未能删除")?;
    }
    Ok(())
}

#[tauri::command]
pub fn download_progress(state: State<'_, Downloads>, id: u64, position: f64, duration: f64) -> Result<(), String> {
    let mut list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let job = list.iter_mut().find(|job| job.rec.id == id).ok_or("找不到这个下载")?;
    if position.is_finite() && position >= 0.0 {
        job.rec.position = position;
    }
    if duration.is_finite() && duration > 0.0 {
        job.rec.duration = duration;
    }
    let root = state.root.lock().map_err(|_| "下载状态不可用")?.clone();
    save_queue(&root, &list);
    Ok(())
}

#[tauri::command]
pub fn download_list(state: State<'_, Downloads>) -> Result<Vec<DownloadView>, String> {
    let list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    Ok(list.iter().map(|job| view_of(&job.rec)).collect())
}

#[tauri::command]
pub fn download_open(state: State<'_, Downloads>, id: u64) -> Result<(), String> {
    let list = state.jobs.lock().map_err(|_| "下载状态不可用")?;
    let job = list.iter().find(|job| job.rec.id == id).ok_or("找不到这个下载")?;
    if job.rec.phase != "done" {
        return Err("下载还没有完成".into());
    }
    let token = if job.rec.token.is_empty() { job.rec.path.clone() } else { job.rec.token.clone() };
    let mime = if token.ends_with(".ts") || job.rec.path.ends_with(".ts") { "video/mp2t" } else if token.ends_with(".webm") { "video/webm" } else { "video/mp4" };
    drop(list);
    crate::local_media::open(&token, mime)
}

pub fn local_file_response(query: &str, range: Option<&str>) -> Response<Vec<u8>> {
    let Some(root) = ROOT.get() else {
        return plain(404, "离线库还没有准备好");
    };
    let Some(id) = query.split('&').find_map(|item| item.strip_prefix("id=")).and_then(|item| item.parse::<u64>().ok()) else {
        return plain(400, "缺少视频编号");
    };
    let dir = root.join("jobs").join(id.to_string());
    let Some(path) = ["media.mp4", "media.webm", "media.ts"].into_iter().map(|name| dir.join(name)).find(|path| path.is_file()) else {
        return plain(404, "这一集还没有下载完成");
    };
    let Ok(file_len) = fs::metadata(&path).map(|item| item.len()) else {
        return plain(404, "无法读取本地视频");
    };
    if file_len == 0 {
        return plain(404, "本地视频是空的");
    }
    let ctype = if path.extension().and_then(|ext| ext.to_str()) == Some("webm") {
        "video/webm"
    } else if path.extension().and_then(|ext| ext.to_str()) == Some("ts") {
        "video/mp2t"
    } else {
        "video/mp4"
    };
    let bounds = match range {
        Some(header) => match parse_range(header, file_len) {
            Ok(bounds) => bounds,
            Err(()) => return unsatisfiable(file_len),
        },
        None => (0, file_len - 1),
    };
    let partial = range.is_some();
    let Ok(body) = read_span(&path, bounds.0, bounds.1) else {
        return plain(500, "无法读取本地视频");
    };
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(if partial { 206 } else { 200 }).unwrap_or(StatusCode::OK))
        .header("Content-Type", ctype)
        .header("Accept-Ranges", "bytes")
        .header("Content-Length", body.len().to_string())
        .header("Cache-Control", "no-store");
    if partial {
        builder = builder.header("Content-Range", format!("bytes {}-{}/{}", bounds.0, bounds.1, file_len));
    }
    builder.body(body).unwrap_or_else(|_| Response::new(Vec::new()))
}

fn parse_range(header: &str, len: u64) -> Result<(u64, u64), ()> {
    let spec = header.trim().strip_prefix("bytes=").ok_or(())?.split(',').next().unwrap_or("").trim();
    if let Some(suffix) = spec.strip_prefix('-') {
        let n: u64 = suffix.parse().map_err(|_| ())?;
        if n == 0 || n > len {
            return Err(());
        }
        return Ok((len - n, len - 1));
    }
    let (start, end) = spec.split_once('-').ok_or(())?;
    let start: u64 = start.parse().map_err(|_| ())?;
    if start >= len {
        return Err(());
    }
    let end = if end.is_empty() { len - 1 } else { end.parse::<u64>().map_err(|_| ())?.min(len - 1) };
    if end < start {
        return Err(());
    }
    Ok((start, end))
}

fn read_span(path: &Path, start: u64, end_inclusive: u64) -> Result<Vec<u8>, ()> {
    let mut file = File::open(path).map_err(|_| ())?;
    file.seek(SeekFrom::Start(start)).map_err(|_| ())?;
    let len = end_inclusive.saturating_sub(start).saturating_add(1) as usize;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).map_err(|_| ())?;
    Ok(buf)
}

fn plain(status: u16, message: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(message.as_bytes().to_vec())
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

fn unsatisfiable(len: u64) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header("Content-Range", format!("bytes */{len}"))
        .body(Vec::new())
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloading_the_queue_pauses_a_running_job_without_touching_partial_bytes() {
        let root = std::env::temp_dir().join(format!("hghub-queue-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("jobs/7")).unwrap();
        fs::write(root.join("jobs/7/partial.bin"), b"abcdef").unwrap();
        let jobs = vec![Job {
            rec: PersistedJob {
                id: 7,
                video_id: 3,
                title: "测试剧".into(),
                url: "https://cdn.example/a.mp4".into(),
                episode: 2,
                phase: "running".into(),
                received: 6,
                total: 10,
                index: 0,
                count: 0,
                message: "正在下载".into(),
                path: String::new(),
                token: String::new(),
                bytes: 0,
                position: 12.5,
                duration: 40.0,
            },
            cancel: Arc::new(AtomicBool::new(false)),
        }];
        save_queue(&root, &jobs);
        let loaded = load_queue(&root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].rec.phase, "paused");
        assert!(loaded[0].rec.message.contains("手动继续"));
        assert_eq!(loaded[0].rec.position, 12.5);
        assert_eq!(fs::read(root.join("jobs/7/partial.bin")).unwrap(), b"abcdef");
        let again = load_queue(&root);
        assert_eq!(again[0].rec.phase, "paused");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn local_range_does_not_return_bytes_outside_the_request() {
        let root = std::env::temp_dir().join(format!("hghub-local-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("jobs/4")).unwrap();
        fs::write(root.join("jobs/4/media.mp4"), b"0123456789").unwrap();
        ROOT.set(root.clone()).ok();
        let response = local_file_response("id=4", Some("bytes=2-5"));
        assert_eq!(response.status(), 206);
        assert_eq!(response.body(), b"2345");
        assert_eq!(response.headers()["Content-Range"], "bytes 2-5/10");
        let bad = local_file_response("id=4", Some("bytes=20-30"));
        assert_eq!(bad.status(), 416);
        let _ = fs::remove_dir_all(&root);
    }
}
