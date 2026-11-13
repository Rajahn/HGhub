//! 业务命令：登录 / 列表 / 详情 / 播放 / 互动。

use crate::net::{base, edge};
use crate::session::SessionStore;
use serde_json::{json, Value};
use tauri::State;
use tauri_plugin_opener::OpenerExt;

pub struct Api {
    pub session: SessionStore,
}

impl Api {
    pub fn new() -> Self {
        Self {
            session: SessionStore::new(),
        }
    }

    /// POST 信封：新播放接口 /api/videos/{id}/episodes/{ep}/playback 需要。
    async fn post(&self, path: &str, body: Option<Value>) -> Result<Value, String> {
        let origin = base();
        let token = self.session.token(&origin);
        edge(&origin, "POST", path, body, token.as_deref()).await
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let origin = base();
        let token = self.session.token(&origin);
        edge(&origin, "GET", path, None, token.as_deref()).await
    }

    async fn authed(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let origin = base();
        let token = self
            .session
            .token(&origin)
            .ok_or_else(|| "请先登录".to_string())?;
        edge(&origin, method, path, body, Some(&token)).await
    }
}

/// API/运行错误与自检统一写入 debug.log，每行附 RFC 3339 UTC 时间和进程编号。
#[tauri::command]
pub async fn debug_log(app: tauri::AppHandle, msg: String) -> Result<(), String> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    crate::logging::append_debug(&dir, &msg)
}

/// 网络自检：系统代理 / DNS / 连通性快照，用于远程定位「打不开」。
#[tauri::command]
pub async fn net_diag() -> Result<Value, String> {
    Ok(crate::net::diagnose().await)
}

/* ───────────────────────── 线路（数据源） ───────────────────────── */

/// 当前线路 + 固定线路清单（镜像线路另外按需拉取）。
#[tauri::command]
pub async fn line_state() -> Result<Value, String> {
    Ok(json!({
        "base": base(),
        "default": crate::net::DEFAULT_BASE,
        "fixed": crate::lines::fixed(),
    }))
}

/// 手动切换线路：立即生效并落盘（下次启动沿用）。
#[tauri::command]
pub async fn line_set(state: State<'_, Api>, url: String) -> Result<String, String> {
    let previous = base();
    let next = crate::net::set_base(&url)?;
    if previous != next {
        state.session.clear();
    }
    Ok(next)
}

/// 从地址发布页抓镜像线路（只在用户点开「线路」时调用）。
#[tauri::command]
pub async fn line_mirrors() -> Result<Value, String> {
    // 用户手动点「刷新 / 打开线路」时重新读一次系统代理：代理软件开关过之后旧客户端会卡死
    crate::net::refresh_client();
    crate::lines::mirrors().await
}

/// 可达性探测：只报结果，不做自动检查 / 自动切换。
#[tauri::command]
pub async fn line_probe(urls: Vec<String>) -> Result<Value, String> {
    crate::net::refresh_client();
    Ok(crate::lines::probe(urls).await)
}

/// 自动选线：`.com` 与 `.ai` 各探一次，选通的那条、快的那条。
///
/// 和 [`line_set`] 的区别是**不清会话**：两个域名是同一个站点，登录态本来就通用；
/// 自动切换如果清会话，用户每换一次网络就被踢下线，那是不能接受的。
#[tauri::command]
pub async fn line_auto() -> Result<Value, String> {
    crate::net::refresh_client();
    let pick = crate::lines::auto_pick().await;
    let chosen = pick
        .get("base")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let previous = base();
    let mut switched = false;
    if !chosen.is_empty() && chosen != previous {
        let next = crate::net::set_base(&chosen)?;
        switched = next != previous;
    }
    Ok(json!({
        "previous": previous,
        "base": crate::net::base(),
        "switched": switched,
        "pick": pick,
    }))
}

/// 当前走的是镜像时，敲一次主站：通了就切回（不清会话）。
///
/// 和 [`line_auto`] 的分工：[`line_auto`] 是「整体比一次」，要抓发布页；
/// 这个只问一句「主站现在通不通」，代价一个请求，可以频繁调用。
#[tauri::command]
pub async fn line_try_main() -> Result<Value, String> {
    crate::net::refresh_client();
    let probe = crate::lines::try_main().await;
    let previous = base();
    let mut switched = false;
    if probe.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let chosen = probe.get("base").and_then(Value::as_str).unwrap_or_default();
        if !chosen.is_empty() && chosen != previous {
            let next = crate::net::set_base(chosen)?;
            switched = next != previous;
        }
    }
    Ok(json!({
        "previous": previous,
        "base": crate::net::base(),
        "switched": switched,
        "probe": probe,
    }))
}

/* ───────────────────────── 更新检查 ───────────────────────── */

const REPO: &str = "zxsos/HGhub";

fn ver_parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .map(|s| s.trim().parse::<u64>().unwrap_or(0))
        .collect()
}

/// 只比较数字段：`v1.2.0` > `1.1.9`；解析不出的段按 0 处理。
fn is_newer(latest: &str, current: &str) -> bool {
    let (a, b) = (ver_parts(latest), ver_parts(current));
    for i in 0..a.len().max(b.len()) {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 从 tag / name 里取出 `x.y.z`。
///
/// 我们的 tag 是 `hgfix-1.2.1-r5`，上游是 `v1.2.0`，release name 是 `HGhub 1.2.1+hg.1`。
/// 直接拿去比会从第一位就判负（`hgfix-1` 解析成 0），所以先抠出版本号。
fn version_in(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut dots = 0;
        let mut j = i;
        while j < chars.len() && (chars[j].is_ascii_digit() || (chars[j] == '.' && dots < 2)) {
            if chars[j] == '.' {
                dots += 1;
            }
            j += 1;
        }
        if dots == 2 {
            return chars[start..j].iter().collect();
        }
        i = j.max(i + 1);
    }
    String::new()
}

/// 线路体检：探一次 `/api/videos`，和自动选线用同一个判据。
#[tauri::command]
pub async fn line_health() -> Result<Value, String> {
    Ok(crate::lines::health().await)
}
/// 对比 GitHub Release 的 tag 与当前版本（未认证 60 次/小时/IP，够用）。
#[tauri::command]
pub async fn check_update() -> Result<Value, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = crate::net::get(
        &url,
        std::time::Duration::from_secs(12),
        Some("application/vnd.github+json"),
        None,
    )
    .await?;

    let status = resp.status();
    if status.as_u16() == 404 {
        // 还没发过 release
        return Ok(json!({
            "current": current, "latest": "", "has_update": false,
            "url": format!("https://github.com/{REPO}/releases"),
        }));
    }
    if !status.is_success() {
        return Err(format!("GitHub 返回 HTTP {status}"));
    }

    let v: Value = resp.json().await.map_err(|e| crate::net::err_text(&e))?;
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let html = v
        .get("html_url")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases"));
    let notes = v.get("body").and_then(|x| x.as_str()).unwrap_or("");
    // tag 是 `hgfix-1.2.1-r5`，不能直接拿整串比大小
    let latest = {
        let found = version_in(&tag);
        if found.is_empty() {
            tag.trim_start_matches(['v', 'V']).to_string()
        } else {
            found
        }
    };

    // 顺手把「这个平台该下哪个包」挑出来，前端好直接应用内更新
    let assets = v.get("assets").and_then(Value::as_array).cloned().unwrap_or_default();
    let asset = assets
        .iter()
        .filter_map(|a| {
            let name = a.get("name").and_then(Value::as_str)?;
            let url = a.get("browser_download_url").and_then(Value::as_str)?;
            Some((name.to_string(), url.to_string()))
        })
        .filter(|(n, _)| asset_rank(n) > 0)
        .max_by_key(|(n, _)| asset_rank(n));
    Ok(json!({
        "current": current,
        "latest": latest,
        "tag": tag,
        "has_update": !latest.is_empty() && is_newer(&latest, &current),
        "asset": asset.map(|(name, url)| json!({ "name": name, "url": url })),
        "url": html,
        "name": v.get("name").and_then(|x| x.as_str()).unwrap_or(""),
        "published_at": v.get("published_at").and_then(|x| x.as_str()).unwrap_or(""),
        "notes": notes.chars().take(800).collect::<String>(),
    }))
}

/// 用系统默认浏览器打开链接（只允许 http/https）。
/// 这个平台该更新哪个包，数字越大越优先。
///
/// 安卓的 release 里同时有 arm64 / arm(v7) / x86_64 三个 apk，光看「是不是 apk」
/// 会先撞上 armv7 那个，64 位机器就降到 32 位跑了。所以按架构给分，取最高。
fn asset_rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    if cfg!(target_os = "android") {
        if !n.ends_with(".apk") {
            0
        } else if cfg!(target_arch = "aarch64") {
            if n.contains("arm64") || n.contains("aarch64") {
                3
            } else {
                1
            }
        } else if cfg!(target_arch = "x86_64") {
            if n.contains("x86_64") {
                3
            } else {
                1
            }
        } else if n.contains("x86") || n.contains("arm64") {
            1
        } else {
            2
        }
    } else if cfg!(target_os = "windows") {
        if n.ends_with(".exe") { 3 } else { 0 }
    } else if cfg!(target_os = "macos") {
        if n.ends_with(".dmg") { 3 } else { 0 }
    } else if n.ends_with(".deb") {
        3
    } else if n.ends_with(".appimage") {
        2
    } else {
        0
    }
}

#[tauri::command]
pub async fn open_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let target = crate::network_policy::parse_target(&url)?;
    // 固定使用默认处理器；不向前端暴露可指定程序的 opener 命令。
    app.opener()
        .open_url(target.as_str(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// 切换移动端屏幕方向（iOS 原生旋转：横屏 Landscape / 竖屏 Portrait）。
#[tauri::command]
pub async fn set_orientation(app: tauri::AppHandle, landscape: bool) -> Result<(), String> {
    #[cfg(target_os = "ios")]
    {
        let _ = app.run_on_main_thread(move || {
            request_ios_orientation(landscape);
        });
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (&app, landscape);
    }
    Ok(())
}

/// iOS 专用：切换 WKWebView 文本交互。
/// 主界面默认关闭以消除长按快进时的系统放大镜；输入框聚焦时前端调用此命令重新打开，
/// 失焦后再关闭，避免影响搜索/评论/登录等文本输入。
#[tauri::command]
pub fn set_text_interaction(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(target_os = "ios")]
    {
        use tauri::Manager;
        if let Some(window) = app.get_webview_window("main") {
            crate::ios_layout::set_text_interaction_on_webview(window.as_ref(), enabled);
        }
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (&app, enabled);
    }
    Ok(())
}

#[cfg(target_os = "ios")]
fn request_ios_orientation(landscape: bool) {
    use std::ffi::{c_char, c_void};
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }
    unsafe {
        let msg0: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let msg0_usize: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        let msg1_usize: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let msg2_ptr: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        let msg1_void: unsafe extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());

        // iOS 16+ UIWindowScene.requestGeometryUpdateWithPreferences:errorHandler:
        // UIInterfaceOrientationMaskPortrait = 1 << 1 = 2
        // UIInterfaceOrientationMaskLandscapeRight = 1 << 3 = 8
        let mask: usize = if landscape { 8 } else { 2 };
        let prefs_cls = objc_getClass(b"UIWindowSceneGeometryPreferencesIOS\0".as_ptr() as *const c_char);
        let app_cls = objc_getClass(b"UIApplication\0".as_ptr() as *const c_char);
        if !prefs_cls.is_null() && !app_cls.is_null() {
            let sel_alloc = sel_registerName(b"alloc\0".as_ptr() as *const c_char);
            let sel_init = sel_registerName(b"initWithInterfaceOrientations:\0".as_ptr() as *const c_char);
            let raw_prefs = msg0(prefs_cls, sel_alloc);
            let prefs = if !raw_prefs.is_null() {
                msg1_usize(raw_prefs, sel_init, mask)
            } else {
                std::ptr::null_mut()
            };

            let sel_shared = sel_registerName(b"sharedApplication\0".as_ptr() as *const c_char);
            let app = msg0(app_cls, sel_shared);
            if !app.is_null() && !prefs.is_null() {
                let sel_scenes = sel_registerName(b"connectedScenes\0".as_ptr() as *const c_char);
                let scenes_set = msg0(app, sel_scenes);
                if !scenes_set.is_null() {
                    let sel_all = sel_registerName(b"allObjects\0".as_ptr() as *const c_char);
                    let arr = msg0(scenes_set, sel_all);
                    if !arr.is_null() {
                        let sel_count = sel_registerName(b"count\0".as_ptr() as *const c_char);
                        let sel_at = sel_registerName(b"objectAtIndex:\0".as_ptr() as *const c_char);
                        let sel_req = sel_registerName(
                            b"requestGeometryUpdateWithPreferences:errorHandler:\0".as_ptr() as *const c_char,
                        );
                        let sel_responds = sel_registerName(b"respondsToSelector:\0".as_ptr() as *const c_char);
                        let msg_responds: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> bool =
                            std::mem::transmute(objc_msgSend as *const ());
                        let count = msg0_usize(arr, sel_count);
                        for i in 0..count {
                            let scene = msg1_usize(arr, sel_at, i);
                            if !scene.is_null() && msg_responds(scene, sel_responds, sel_req) {
                                msg2_ptr(scene, sel_req, prefs, std::ptr::null_mut());
                            }
                        }
                    }
                }
            }
        }

        // Fallback: UIDevice setValue:forKey:@"orientation" + attemptRotationToDeviceOrientation
        let dev_cls = objc_getClass(b"UIDevice\0".as_ptr() as *const c_char);
        let num_cls = objc_getClass(b"NSNumber\0".as_ptr() as *const c_char);
        let str_cls = objc_getClass(b"NSString\0".as_ptr() as *const c_char);
        let vc_cls = objc_getClass(b"UIViewController\0".as_ptr() as *const c_char);
        if !dev_cls.is_null() && !num_cls.is_null() && !str_cls.is_null() {
            let sel_cur = sel_registerName(b"currentDevice\0".as_ptr() as *const c_char);
            let dev = msg0(dev_cls, sel_cur);
            let sel_num = sel_registerName(b"numberWithInteger:\0".as_ptr() as *const c_char);
            let orient_val: usize = if landscape { 3 } else { 1 };
            let num = msg1_usize(num_cls, sel_num, orient_val);
            let sel_str = sel_registerName(b"stringWithUTF8String:\0".as_ptr() as *const c_char);
            let msg_str: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let key = msg_str(str_cls, sel_str, b"orientation\0".as_ptr() as *const c_char);
            if !dev.is_null() && !num.is_null() && !key.is_null() {
                let sel_set_val = sel_registerName(b"setValue:forKey:\0".as_ptr() as *const c_char);
                msg2_ptr(dev, sel_set_val, num, key);
            }
            if !vc_cls.is_null() {
                let sel_attempt = sel_registerName(b"attemptRotationToDeviceOrientation\0".as_ptr() as *const c_char);
                msg1_void(vc_cls, sel_attempt);
            }
        }
    }
}

/* ───────────────────────── 会话 ───────────────────────── */

#[tauri::command]
pub async fn app_boot(state: State<'_, Api>) -> Result<Value, String> {
    let origin = base();
    Ok(json!({
        "user": state.session.user(&origin),
        "logged_in": state.session.token(&origin).is_some(),
        "base": origin,
        "strict_security": crate::network_policy::strict_security(),
        "cli_relaxed": crate::network_policy::cli_strict_override() == Some(false),
    }))
}

/// 切换严格安全模式（关闭后回退到兼容清洗与网络守卫模式，立即生效并落盘）。
#[tauri::command]
pub async fn security_set(strict: bool) -> Result<bool, String> {
    crate::network_policy::set_strict_security(strict)?;
    crate::net::refresh_client();
    crate::playback_log::record(
        "security.mode_changed",
        json!({"strict": crate::network_policy::strict_security()}),
    );
    Ok(crate::network_policy::strict_security())
}

#[tauri::command]
pub async fn api_login(
    state: State<'_, Api>,
    username: String,
    password: String,
) -> Result<Value, String> {
    let origin = base();
    let data = edge(
        &origin,
        "POST",
        "/api/account/loginByPassword",
        Some(json!({ "username": username, "password": password })),
        None,
    )
    .await?;
    let token = data
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "登录返回缺少 token".to_string())?
        .to_string();
    let user = data.get("user").cloned().unwrap_or(Value::Null);
    if base() != origin {
        return Err("登录期间线路已变化，请重新登录".into());
    }
    state.session.set(&origin, token, user.clone());
    Ok(json!({ "user": user }))
}

#[tauri::command]
pub async fn api_register(
    state: State<'_, Api>,
    username: String,
    password: String,
    email: String,
) -> Result<Value, String> {
    let origin = base();
    let mut body = json!({
        "username": username,
        "password": password,
        "confirm_password": password,
    });
    if !email.trim().is_empty() {
        body["email"] = json!(email);
    }
    let data = edge(
        &origin,
        "POST",
        "/api/account/registerByPassword",
        Some(body),
        None,
    )
    .await?;
    let token = data
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "注册返回缺少 token".to_string())?
        .to_string();
    let user = data.get("user").cloned().unwrap_or(Value::Null);
    if base() != origin {
        return Err("注册期间线路已变化，请重新登录".into());
    }
    state.session.set(&origin, token, user.clone());
    Ok(json!({ "user": user }))
}

#[tauri::command]
pub async fn api_logout(state: State<'_, Api>) -> Result<Value, String> {
    let origin = base();
    let token = state.session.token(&origin);
    let _ = edge(
        &origin,
        "POST",
        "/api/account/logout",
        None,
        token.as_deref(),
    )
    .await;
    state.session.clear();
    Ok(json!({ "ok": true }))
}

#[tauri::command]
pub async fn api_me(state: State<'_, Api>) -> Result<Value, String> {
    let origin = base();
    let token = state.session.token(&origin);
    let me = edge(&origin, "GET", "/api/account/me", None, token.as_deref()).await?;
    if base() != origin {
        return Err("读取账号期间线路已变化".into());
    }
    if let Some(token) = state.session.token(&origin) {
        state.session.set(&origin, token, me.clone());
    }
    Ok(me)
}

/* ───────────────────────── 列表 ───────────────────────── */

#[tauri::command]
pub async fn api_feed(state: State<'_, Api>, page: u32, size: u32) -> Result<Value, String> {
    state
        .get(&format!("/api/videos?page={page}&size={size}"))
        .await
}

/* ───────────────────────── 专题 / 标签 ───────────────────────── */

/// 专题列表：`/api/topics` 返回 data.items（标题/封面/简介/收录数）。
#[tauri::command]
pub async fn api_topics(state: State<'_, Api>) -> Result<Value, String> {
    state.get("/api/topics?size=100").await
}

/// 专题详情：`/api/topics/{slug}` 返回 data = { topic, videos, related }。
#[tauri::command]
pub async fn api_topic(state: State<'_, Api>, slug: String) -> Result<Value, String> {
    let encoded: String = url::form_urlencoded::byte_serialize(slug.as_bytes()).collect();
    state.get(&format!("/api/topics/{encoded}")).await
}

/// 标签分组：`/api/tags` 返回 data.categories[]，每个含 tags[]（name/slug/hot_score）。
#[tauri::command]
pub async fn api_tags(state: State<'_, Api>) -> Result<Value, String> {
    state.get("/api/tags").await
}

/// 按标签取视频：后端按中文标签名匹配（网页 /tag/<slug>/ 实际请求 /api/videos/tag/<name>）。
#[tauri::command]
pub async fn api_tag_videos(
    state: State<'_, Api>,
    tag: String,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    let encoded: String = url::form_urlencoded::byte_serialize(tag.as_bytes()).collect();
    state
        .get(&format!("/api/videos/tag/{encoded}?page={page}&size={size}"))
        .await
}

#[tauri::command]
pub async fn api_category(
    state: State<'_, Api>,
    cate: String,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    state
        .get(&format!(
            "/api/videos/category/{cate}?page={page}&size={size}"
        ))
        .await
}

#[tauri::command]
pub async fn api_rank(state: State<'_, Api>, kind: String) -> Result<Value, String> {
    let kind = match kind.as_str() {
        "recommend" | "potential" => kind,
        _ => "hot".to_string(),
    };
    state.get(&format!("/api/ranks/{kind}")).await
}

/// 全站搜索：`/api/search?q=...&type=video|post`。
///
/// `type` 省略时后端默认只搜短剧；吃瓜帖必须显式传 `type=post`，
/// 否则搜「吃瓜频道关键词」永远是空结果（issue #34）。
#[tauri::command]
pub async fn api_search(
    state: State<'_, Api>,
    q: String,
    page: u32,
    size: u32,
    kind: Option<String>,
) -> Result<Value, String> {
    let encoded: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
    let scope = match kind.as_deref() {
        Some("post") => "&type=post",
        Some("video") => "&type=video",
        _ => "",
    };
    state
        .get(&format!("/api/search?q={encoded}&page={page}&size={size}{scope}"))
        .await
}

/// 热搜榜：`/api/search/hot` 返回 data.items（word/heat）。
#[tauri::command]
pub async fn api_search_hot(state: State<'_, Api>) -> Result<Value, String> {
    state.get("/api/search/hot").await
}

#[tauri::command]
pub async fn api_posts(state: State<'_, Api>, page: u32, size: u32) -> Result<Value, String> {
    state
        .get(&format!("/api/posts?page={page}&size={size}"))
        .await
}

#[tauri::command]
pub async fn api_post(state: State<'_, Api>, id: i64) -> Result<Value, String> {
    state.get(&format!("/api/posts/{id}")).await
}

/// 吃瓜帖视频：`POST /api/posts/{id}/playback` → `data.videos[].url`（mode=1 为明文 m3u8）。
///
/// 帖详情里的 `video_url` / `video_api` 恒为空，视频地址只由这个接口下发；
/// 不调它的话吃瓜页永远只有图文（issue #11）。无视频时接口返回 status=0
/// 「视频不存在或不可播放」，经 `edge()` 转换后就是 Err，前端按「无视频」处理。
#[tauri::command]
pub async fn api_post_playback(state: State<'_, Api>, id: i64) -> Result<Value, String> {
    state
        .post(&format!("/api/posts/{id}/playback"), Some(json!({})))
        .await
}

/* ───────────────────────── 详情 / 播放 ───────────────────────── */

#[tauri::command]
pub async fn api_video(state: State<'_, Api>, id: i64) -> Result<Value, String> {
    state.get(&format!("/api/videos/{id}")).await
}

fn non_empty(v: Option<&Value>) -> Option<&Value> {
    v.filter(|x| !x.is_null() && x.as_str().is_some_and(|s| !s.trim().is_empty()))
}

/// 把新接口返回的 streams.main / streams.hevc 回填到 video_url / video_url_hevc，
/// 前端只认 video_url，旧字段缺失时也兼容。
fn normalize_play_info(info: &mut Value) {
    let Some(obj) = info.as_object_mut() else { return };
    for (field, key) in [("video_url", "main"), ("video_url_hevc", "hevc")] {
        let url = obj
            .get("streams")
            .and_then(|st| st.get(key))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned);
        if let Some(url) = url {
            if non_empty(obj.get(field)).is_none() {
                obj.insert(field.into(), json!(url));
            }
        }
    }
}

fn has_play_url(info: &Value) -> bool {
    info.get("video_url")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty())
}

impl Api {
    /// 重新向播放接口要地址。只接受 HTTPS，供下载在 401/403/410 时换一次源。
    pub async fn fresh_play_url(&self, id: i64, ep: u32) -> Result<String, String> {
        let info = self.play_info(id, ep).await?;
        let url = info.get("video_url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if url.starts_with("https://") {
            Ok(url)
        } else {
            Err("播放接口没有返回可用的 HTTPS 地址".into())
        }
    }

    async fn play_info(&self, id: i64, ep: u32) -> Result<Value, String> {
        let primary = self
            .post(&format!("/api/videos/{id}/episodes/{ep}/playback"), Some(json!({})))
            .await
            .map(|mut info| {
                normalize_play_info(&mut info);
                info
            });
        if let Ok(info) = &primary {
            if has_play_url(info) {
                return primary;
            }
        }
        match self.get(&format!("/api/videos/{id}/play?ep={ep}&page_texts=0")).await {
            Ok(mut legacy) => {
                normalize_play_info(&mut legacy);
                if has_play_url(&legacy) || primary.is_err() {
                    Ok(legacy)
                } else {
                    primary
                }
            }
            Err(_) => primary,
        }
    }
}

#[tauri::command]
pub async fn api_play(state: State<'_, Api>, id: i64, ep: u32) -> Result<Value, String> {
    state.play_info(id, ep).await
}

/* ───────────────────────── 评论 / 互动 ───────────────────────── */

#[tauri::command]
pub async fn api_comments(
    state: State<'_, Api>,
    id: i64,
    ep: u32,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    state
        .get(&format!(
            "/api/videos/{id}/comments?ep={ep}&page={page}&size={size}"
        ))
        .await
}

#[tauri::command]
pub async fn api_comment_send(
    state: State<'_, Api>,
    id: i64,
    ep: u32,
    text: String,
    reply_id: Option<i64>,
) -> Result<Value, String> {
    let mut body = json!({ "text": text });
    let path = match reply_id {
        Some(rid) if rid > 0 => {
            body["reply_id"] = json!(rid);
            format!("/api/videos/{id}/comments")
        }
        _ => format!("/api/videos/{id}/comments?ep={ep}"),
    };
    state.authed("POST", &path, Some(body)).await
}

#[tauri::command]
pub async fn api_like(state: State<'_, Api>, id: i64, ep: u32, on: bool) -> Result<Value, String> {
    let method = if on { "POST" } else { "DELETE" };
    state
        .authed(method, &format!("/api/videos/{id}/like?ep={ep}"), None)
        .await
}

#[tauri::command]
pub async fn api_collect(state: State<'_, Api>, id: i64, on: bool) -> Result<Value, String> {
    let method = if on { "POST" } else { "DELETE" };
    state
        .authed(method, &format!("/api/videos/{id}/collect"), None)
        .await
}

#[tauri::command]
pub async fn api_follow(state: State<'_, Api>, uid: i64, on: bool) -> Result<Value, String> {
    let method = if on { "POST" } else { "DELETE" };
    state
        .authed(method, &format!("/api/users/{uid}/follow"), None)
        .await
}

/* ───────────────────────── 本地缓存 ───────────────────────── */

#[tauri::command]
pub async fn cache_clear() -> Result<(), String> {
    crate::media::clear_caches();
    Ok(())
}

/* ───────────────────────── 吃瓜帖子互动 ───────────────────────── */

#[tauri::command]
pub async fn api_post_like(state: State<'_, Api>, id: i64, on: bool) -> Result<Value, String> {
    let method = if on { "POST" } else { "DELETE" };
    state
        .authed(method, &format!("/api/posts/{id}/like"), None)
        .await
}

#[tauri::command]
pub async fn api_post_collect(state: State<'_, Api>, id: i64, on: bool) -> Result<Value, String> {
    let method = if on { "POST" } else { "DELETE" };
    state
        .authed(method, &format!("/api/posts/{id}/collect"), None)
        .await
}

#[tauri::command]
pub async fn api_post_comments(
    state: State<'_, Api>,
    id: i64,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    state
        .get(&format!("/api/posts/{id}/comments?page={page}&size={size}"))
        .await
}

/// 吃瓜分类：`/api/site/categories/post` 返回 data.items（name/route）。
#[tauri::command]
pub async fn api_post_categories(state: State<'_, Api>) -> Result<Value, String> {
    state.get("/api/site/categories/post").await
}

/// 按分类取吃瓜帖：`/api/posts/category/{route}`。
#[tauri::command]
pub async fn api_post_category(
    state: State<'_, Api>,
    route: String,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    let encoded: String = url::form_urlencoded::byte_serialize(route.as_bytes()).collect();
    state
        .get(&format!("/api/posts/category/{encoded}?page={page}&size={size}"))
        .await
}

/// 相关推荐：`/api/videos/{id}/related` 返回 data.items。
#[tauri::command]
pub async fn api_related(state: State<'_, Api>, id: i64) -> Result<Value, String> {
    state.get(&format!("/api/videos/{id}/related")).await
}

/// 关注流：`/api/videos/following`（需登录）。
#[tauri::command]
pub async fn api_following(
    state: State<'_, Api>,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    state
        .authed("GET", &format!("/api/videos/following?page={page}&size={size}"), None)
        .await
}

/// 用户主页：`/api/users/{id}`。
#[tauri::command]
pub async fn api_user(state: State<'_, Api>, id: i64) -> Result<Value, String> {
    state.get(&format!("/api/users/{id}")).await
}

/// 用户统计：`/api/users/{id}/stats`。
#[tauri::command]
pub async fn api_user_stats(state: State<'_, Api>, id: i64) -> Result<Value, String> {
    state.get(&format!("/api/users/{id}/stats")).await
}

/// 用户作品：`/api/videos/user/{id}`。
#[tauri::command]
pub async fn api_user_videos(
    state: State<'_, Api>,
    id: i64,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    state
        .get(&format!("/api/videos/user/{id}?page={page}&size={size}"))
        .await
}
#[cfg(test)]
mod tests {
    use super::{is_newer, version_in};
    use serde_json::Value;

    #[test]
    fn frontend_cannot_bypass_url_policy_with_opener_commands() {
        let capability: Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        for permission in capability["permissions"].as_array().unwrap() {
            let name = permission
                .as_str()
                .or_else(|| permission["identifier"].as_str())
                .unwrap();
            assert!(
                !name.starts_with("opener:"),
                "frontend opener command exposed: {name}"
            );
        }
    }

    #[test]
    fn external_links_are_urls_not_shell_commands() {
        for raw in [
            "https://example.com/|calc.exe",
            "https://example.com/?a=1&b=2",
            "https://example.com/%25PATH%25",
            "https://example.com/%VAR%",
        ] {
            assert!(crate::network_policy::parse_target(raw).is_ok());
        }
        for raw in [
            "file:///etc/passwd",
            "mailto:user@example.com",
            "tel:123",
            "data:text/html,test",
            "javascript:alert(1)",
            "https://example.com/\0cmd",
            "https://user:password@example.com/",
        ] {
            assert!(crate::network_policy::parse_target(raw).is_err());
        }
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("v1.0.2", "1.0.1"));
        assert!(is_newer("v1.1", "1.0.9"));
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(!is_newer("v1.0.1", "1.0.1"));
        assert!(!is_newer("v0.9.9", "1.0.1"));
        assert!(!is_newer("", "1.0.1"));
    }
    /// tag 不是纯版本号：`hgfix-1.2.1-r5` 整串拿去比会从第一位判负（`hgfix-1` 解析成 0），
    /// 结果就是「永远没有新版本」。所以先抠出 `x.y.z` 再比。
    #[test]
    fn reads_version_out_of_tag() {
        assert_eq!(version_in("hgfix-1.2.1-r5"), "1.2.1");
        assert_eq!(version_in("v1.2.0"), "1.2.0");
        assert_eq!(version_in("HGhub 1.2.1+hg.1"), "1.2.1");
        assert_eq!(version_in("release-notes"), "");
        assert!(is_newer(&version_in("hgfix-1.2.2-r6"), "1.2.1+hg.1"));
        assert!(is_newer(&version_in("hgfix-1.10.0-r9"), "1.9.9+hg.1"));
        assert!(!is_newer(&version_in("hgfix-1.2.1-r5"), "1.2.1+hg.1"));
    }
}
