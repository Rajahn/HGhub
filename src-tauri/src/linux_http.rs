//! Linux 桌面端的媒体通道：把 [`crate::media::handle`] 挂到一个只监听
//! 127.0.0.1 的 HTTP 服务上。
//!
//! 为什么需要它：Tauri 的自定义协议只对 WebView 自己的资源加载生效，而
//! WebKitGTK 的 `<video>` 是交给 GStreamer 播放的，GStreamer 只认 http/https/file
//! 这类常规 URI，直接给它 `hg://localhost` 打不开；同时 WebKitGTK 4.1 默认不提供
//! MediaSource，hls.js 也用不上。Linux 端的播放只能让 GStreamer 的 hlsdemux
//! 原生完成，而它需要一个真正可取的 HTTP 地址——就是这个服务。
//!
//! 端口固定，前端 `src/api.ts` 的 `LOCAL` 必须与 [`ORIGIN`] 一致。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

/// 监听端口。前端硬编码同一个值，两边必须同时改。
pub const PORT: u16 = 17650;

/// 前端与 m3u8 分片改写共用的 origin。
pub const ORIGIN: &str = "http://127.0.0.1:17650";

/// 在后台线程里起服务。绑定失败只记日志（播放会退回自定义协议，大概率失败）。
pub fn start() {
    std::thread::spawn(|| {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("[hg] 媒体端口 {PORT} 绑定失败：{err}");
                return;
            }
        };
        eprintln!("[hg] 媒体代理就绪：{ORIGIN}");
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                if let Err(err) = serve(stream) {
                    eprintln!("[hg] 媒体请求失败：{err}");
                }
            });
        }
    });
}

fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    let timeout = Some(std::time::Duration::from_secs(30));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some(value) = line.trim().strip_prefix("Range:") {
            range = Some(value.trim().to_string());
        }
    }

    let mut request = request_line.split_whitespace();
    let method = request.next().unwrap_or_default();
    let target = request.next().unwrap_or("/");
    if method != "GET" && method != "HEAD" {
        return write_plain(&mut stream, 405, "method not allowed");
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };

    let response = if path == "/local" {
        crate::download_queue::local_file_response(query, range.as_deref())
    } else {
        tauri::async_runtime::block_on(crate::media::handle(path, query, ORIGIN))
    };
    let (parts, body) = response.into_parts();

    let mut head = String::with_capacity(320);
    head.push_str(&format!(
        "HTTP/1.1 {} {}\r\n",
        parts.status.as_u16(),
        parts.status.canonical_reason().unwrap_or("OK")
    ));
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    for (name, value) in parts.headers.iter() {
        if let Ok(value) = value.to_str() {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), value));
        }
    }
    head.push_str("Access-Control-Allow-Origin: *\r\n");
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    if method != "HEAD" {
        stream.write_all(&body)?;
    }
    stream.flush()
}

fn write_plain(stream: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let reason = tauri::http::StatusCode::from_u16(status)
        .ok()
        .and_then(|code| code.canonical_reason())
        .unwrap_or("Error");
    let body = message.as_bytes();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
