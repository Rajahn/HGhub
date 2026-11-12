mod api;
mod hls_download;
#[cfg(target_os = "ios")]
mod ios_layout;
mod lines;
mod local_media;
#[cfg(target_os = "linux")]
mod linux_http;
mod logging;
#[cfg(target_os = "macos")]
mod macos_titlebar;
mod media;
mod media_dns;
mod net;
mod network_policy;
mod playback_log;
mod session;
mod ts_mp4;

use api::Api;
use tauri::Manager;

fn allowed_dev_host(host: &str) -> bool {
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        return true;
    }
    if let Some(configured) = option_env!("TAURI_DEV_HOST") {
        if host.eq_ignore_ascii_case(configured.trim()) {
            return true;
        }
    }
    std::env::var("TAURI_DEV_HOST")
        .ok()
        .is_some_and(|v| host.eq_ignore_ascii_case(v.trim()))
}

fn allowed_frontend_origin(origin: &str) -> bool {
    if matches!(
        origin,
        "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
    ) {
        return true;
    }
    // 移动端开发可能使用 TAURI_DEV_HOST；严格模式下仅放行本机或显式配置的 TAURI_DEV_HOST。
    cfg!(debug_assertions)
        && url::Url::parse(origin).is_ok_and(|u| {
            u.scheme() == "http"
                && u.port() == Some(1420)
                && (!network_policy::strict_security()
                    || u.host_str().is_some_and(allowed_dev_host))
        })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .manage(Api::new())
        .manage(hls_download::Downloads::new())
        .on_page_load(|webview, payload| {
            #[cfg(target_os = "ios")]
            {
                let finished = payload.event() == tauri::webview::PageLoadEvent::Finished;
                ios_layout::apply(webview, finished);
                if finished {
                    // 默认关闭 WKWebView 文本交互，消除视频长按快进时的系统放大镜。
                    // 前端在输入框聚焦时再按需打开（见 api::set_text_interaction）。
                    ios_layout::set_text_interaction_on_webview(webview, false);
                    let wv = (*webview).clone();
                    tauri::async_runtime::spawn(async move {
                        for delay_ms in [80_u64, 350] {
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            ios_layout::apply(&wv, true);
                        }
                    });
                }
            }
            #[cfg(not(target_os = "ios"))]
            let _ = (webview, payload);
        })
        .setup(|app| {
            if let Ok(dir) = app.path().app_data_dir() {
                let _ = std::fs::create_dir_all(&dir);
                // 上次选择的线路（设置 → 线路）在启动时恢复
                net::bind_base(&dir);
                playback_log::bind(&dir);
                let state: tauri::State<Api> = app.state();
                state.session.bind(dir);
            }
            // Linux 的 <video> 由 GStreamer 播放，它只认常规 HTTP scheme；
            // 这里起一个 loopback 代理，前端与 m3u8 改写都指向它（见 linux_http.rs）
            #[cfg(target_os = "linux")]
            linux_http::start();
            // macOS 主窗口在此创建：先按系统红绿灯尺寸算出居中位置（见 macos_titlebar.rs）
            #[cfg(target_os = "macos")]
            macos_titlebar::create_main_window(app)?;
            Ok(())
        })
        .plugin(
            tauri::plugin::Builder::<_, ()>::new("navigation-guard")
                .on_navigation(|_webview, url| {
                    let host = url.host_str().unwrap_or("");
                    (url.scheme() == "tauri" && host == "localhost")
                        // Android 上自定义协议表现为 http://<scheme>.localhost，WebView 有时把它
                        // 当成一次导航进来。这是进程内的媒体代理地址，不出网，严格模式也不该拦。
                        || (matches!(url.scheme(), "http" | "https") && host.ends_with(".localhost"))
                        || allowed_frontend_origin(&url.origin().ascii_serialization())
                        || (!network_policy::strict_security()
                            && matches!(url.scheme(), "http" | "https"))
                })
                .build(),
        )
        .register_asynchronous_uri_scheme_protocol("hg", |_app, request, responder| {
            let request_origin = request
                .headers()
                .get("Origin")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            if playback_log::enabled() {
                let resource = url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
                    .find(|(key, _)| key == "u")
                    .map(|(_, value)| playback_log::resource_id(&value));
                let route = match request.uri().path() { "/hls" => "playlist", "/seg" => "segment_or_key", "/img" => "image", _ => "unknown" };
                let range = request.headers().get("Range").and_then(|v| v.to_str().ok())
                    .filter(|v| v.len() <= 100 && v.bytes().all(|b| b.is_ascii_digit() || b"bytes=,- ".contains(&b)));
                let origin_kind = match request_origin.as_deref() { None => "missing", Some("null") => "null", Some(origin) if allowed_frontend_origin(origin) => "app", _ => "other" };
                playback_log::record("protocol.request", serde_json::json!({"resource": resource, "route": route, "method": request.method().as_str(), "range": range, "origin_kind": origin_kind}));
            }
            // 部分 Android WebView 在拦截 http://<scheme>.localhost 时给不出 Origin（或给空串），
            // 严格模式下会被误判成外部来源直接 403 —— 这就是「接口通了、m3u8 却立刻 networkError」。
            // 这条通道只通向 media::handle 的 /img /hls /seg，后端仍按严格模式做 SSRF 校验，
            // 所以缺 Origin 放行不等于开了个开放代理。
            let origin_ok = match request_origin.as_deref() {
                // WebView 拦截自定义协议时可能是 None、空串，甚至字面量 "null"（sandbox 语义）
                None | Some("") | Some("null") => true,
                Some(origin) => allowed_frontend_origin(origin),
            };
            if network_policy::strict_security() && !origin_ok {
                playback_log::record("security.origin_denied", serde_json::json!({"origin_null": request_origin.as_deref() == Some("null")}));
                responder.respond(
                    tauri::http::Response::builder()
                        .status(403)
                        .body(b"origin denied".to_vec())
                        .unwrap(),
                );
                return;
            }
            let uri = request.uri().clone();
            let path = uri.path().to_string();
            let query = uri.query().unwrap_or("").to_string();
            // 自定义协议基址随平台不同，统一定义在 media::LOCAL_ORIGIN
            // （Windows/Android 是 http://<scheme>.localhost，macOS/iOS/Linux 是 <scheme>://localhost）。
            // 它同时是 m3u8 分片/key 改写用的前缀，必须与前端 src/api.ts 的 LOCAL 一致。
            tauri::async_runtime::spawn(async move {
                let mut resp = media::handle(&path, &query, media::LOCAL_ORIGIN).await;
                if let Some(origin) =
                    request_origin.and_then(|s| tauri::http::HeaderValue::from_str(&s).ok())
                {
                    resp.headers_mut()
                        .insert("Access-Control-Allow-Origin", origin);
                    resp.headers_mut()
                        .insert("Vary", tauri::http::HeaderValue::from_static("Origin"));
                } else {
                    // WKWebView 内部页面可能不给 Origin；这时回 * 才能拿到分片
                    resp.headers_mut()
                        .insert("Access-Control-Allow-Origin", tauri::http::HeaderValue::from_static("*"));
                }
                responder.respond(resp);
            });
        })
        .invoke_handler(tauri::generate_handler![
            api::app_boot,
            playback_log::playback_log_status,
            playback_log::playback_log_set,
            playback_log::playback_log_write,
            playback_log::playback_log_read,
            api::security_set,
            api::debug_log,
            api::net_diag,
            api::line_state,
            api::line_set,
            api::line_mirrors,
            api::line_probe,
            api::line_auto,
            api::line_try_main,
            api::line_health,
            api::check_update,
            api::open_url,
            api::set_orientation,
            api::set_text_interaction,
            api::api_login,
            api::api_register,
            api::api_logout,
            api::api_me,
            api::api_feed,
            api::api_topics,
            api::api_topic,
            api::api_tags,
            api::api_tag_videos,
            api::api_category,
            api::api_rank,
            api::api_search,
            api::api_search_hot,
            api::cache_clear,
            api::api_posts,
            api::api_post,
            api::api_post_playback,
            api::api_post_like,
            api::api_post_collect,
            api::api_post_comments,
            api::api_post_categories,
            api::api_post_category,
            api::api_related,
            api::api_following,
            api::api_user,
            api::api_user_stats,
            api::api_user_videos,
            api::api_video,
            api::api_play,
            api::api_comments,
            api::api_comment_send,
            api::api_like,
            api::api_collect,
            api::api_follow,
            hls_download::download_start,
            hls_download::download_cancel,
            hls_download::download_list,
            hls_download::download_open,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
