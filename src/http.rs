//! The daemon's HTTP surface (A11Y_HTTP.md) — the ONE thing a viewer or the
//! worker connects to.
//!
//!   GET  /            the easystream web UI (index.html)
//!   GET  /app.js      the web UI client
//!   GET  /est/ws      a TRANSPARENT byte pipe (WebSocket <-> est TCP)
//!   GET  /a11y/*      the a11y tree/action/clipboard API
//!
//! Written on raw TcpStream (no HTTP/WS crates): the surface is tiny, and a
//! hand-rolled server avoids the tiny_http/tungstenite socket-ownership clash
//! while keeping the daemon dependency-light (libdbus for AT-SPI is the only
//! non-trivial dep).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine;

const INDEX_HTML: &str = include_str!("../webui/index.html");
const APP_JS: &str = include_str!("../webui/app.js");

/// a11y is the shared platform backend, opened lazily on first /a11y request.
pub type A11y = Arc<Mutex<Option<crate::a11y::Backend>>>;

pub fn serve(port: u16, est_port: u16, a11y: A11y) -> Result<(), String> {
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| e.to_string())?;
    println!("easystream http on 0.0.0.0:{port} (est pipe -> 127.0.0.1:{est_port})");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let a11y = Arc::clone(&a11y);
                thread::spawn(move || {
                    // COM must be initialised on EVERY thread that touches UIA.
                    // The daemon initialises MTA on the main thread, but each
                    // HTTP connection is a fresh thread — without this, UIA
                    // calls fail with 0x80040201 ("an event was unable to
                    // invoke any of the subscribers"). MTA matches the main
                    // thread, so the already-created IUIAutomation can be used
                    // from here.
                    #[cfg(windows)]
                    unsafe {
                        let _ = windows::Win32::System::Com::CoInitializeEx(
                            None,
                            windows::Win32::System::Com::COINIT_MULTITHREADED,
                        );
                    }
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Err(e) = handle(stream, est_port, a11y) {
                            eprintln!("http: {e}");
                        }
                    }));
                });
            }
            Err(e) => eprintln!("http accept: {e}"),
        }
    }
    Ok(())
}

fn handle(stream: TcpStream, est_port: u16, a11y: A11y) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    // Request line.
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let _method = parts.next().unwrap_or("").to_string();
    let url = parts.next().unwrap_or("/").to_string();
    // Headers.
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let path = url.split('?').next().unwrap_or("/").to_string();
    let query = url.splitn(2, '?').nth(1).unwrap_or("").to_string();

    match path.as_str() {
        "/" | "/index.html" => respond_text(&mut reader, 200, "text/html; charset=utf-8", INDEX_HTML),
        "/app.js" => respond_text(&mut reader, 200, "application/javascript; charset=utf-8", APP_JS),
        "/health" => respond_text(&mut reader, 200, "text/plain", "ok"),
        "/est/ws" => ws_pipe(reader, headers, est_port),
        p if p.starts_with("/a11y/") => a11y_route(&mut reader, p, &query, a11y),
        _ => respond_text(&mut reader, 404, "text/plain", "not found"),
    }
}

fn respond_text(reader: &mut BufReader<TcpStream>, status: u16, ctype: &str, body: &str) -> Result<(), String> {
    let mut s = reader.get_ref();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())
}


fn respond_bytes(reader: &mut BufReader<TcpStream>, status: u16, ctype: &str, body: &[u8]) -> Result<(), String> {
    let mut s = reader.get_ref();
    let head = format!(
        "HTTP/1.1 {status} {}
Content-Type: {ctype}
Content-Length: {}
Connection: close

",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(body).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())
}

/// screenshot_png captures the screen and encodes it as a PNG. WGC owns the
/// capture session, so a fresh handle per call keeps this a one-shot.
fn screenshot_png(region: &str) -> Result<Vec<u8>, String> {
    let mut wgc = crate::win::Wgc::open()?;
    let (w, h) = wgc.size();
    let mut px: Vec<u8> = Vec::new();
    wgc.grab_into(&mut px)?;
    if region.is_empty() {
        return Ok(crate::png::encode_bgra(w, h, &px));
    }
    let nums: Vec<i64> = region.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    if nums.len() != 4 { return Err("region must be X,Y,W,H".into()); }
    let (rx, ry, rw, rh) = (nums[0].max(0) as u32, nums[1].max(0) as u32, nums[2].max(1) as u32, nums[3].max(1) as u32);
    let (rx, ry) = (rx.min(w.saturating_sub(1)), ry.min(h.saturating_sub(1)));
    let (rw, rh) = (rw.min(w - rx), rh.min(h - ry));
    let mut crop = Vec::with_capacity((rw * rh * 4) as usize);
    for y in ry..ry + rh {
        let off = ((y * w + rx) * 4) as usize;
        crop.extend_from_slice(&px[off..off + (rw * 4) as usize]);
    }
    Ok(crate::png::encode_bgra(rw, rh, &crop))
}

/// clipboard_get/set use the real Win32 clipboard (shared with est 0x30-0x32).
fn clipboard_get() -> Result<String, String> { Ok(crate::win::clipboard::get()) }
fn clipboard_set(text: &str) -> Result<String, String> { crate::win::clipboard::set(text); Ok("ok".into()) }

fn q(query: &str, key: &str) -> String {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return percent_encoding::percent_decode_str(it.next().unwrap_or(""))
                .decode_utf8_lossy()
                .to_string();
        }
    }
    String::new()
}

fn a11y_route(reader: &mut BufReader<TcpStream>, path: &str, query: &str, a11y: A11y) -> Result<(), String> {
    // The backend is built on THIS thread, every request.
    //
    // An IUIAutomation created on one thread and used from another crashes the
    // daemon with a native access violation (not a Rust panic) even under MTA,
    // so the shared-cache the Linux backend uses is unsafe on Windows.
    // CoCreateInstance is cheap next to a tree walk. On Linux the D-Bus
    // connection IS shareable, so it is cached as before.
    #[cfg(windows)]
    let owned = crate::a11y::Backend::open()?;
    #[cfg(windows)]
    let be = &owned;
    #[cfg(not(windows))]
    let mut guard = a11y.lock().unwrap();
    #[cfg(not(windows))]
    let be = {
        if guard.is_none() {
            *guard = Some(crate::a11y::Backend::open()?);
        }
        guard.as_ref().unwrap()
    };
    let pid: u32 = q(query, "pid").parse().unwrap_or(0);
    // Screenshot returns PNG bytes, not text.
    if path == "/a11y/screenshot" {
        return match screenshot_png(&q(query, "region")) {
            Ok(png) => respond_bytes(reader, 200, "image/png", &png),
            Err(e) => respond_text(reader, 500, "text/plain; charset=utf-8", &e),
        };
    }
    let result: Result<String, String> = match path {
        "/a11y/apps" => be.apps(),
        "/a11y/tree" => be.tree(pid, q(query, "depth").parse().unwrap_or(0)),
        "/a11y/find" => {
            let o = match q(query, "o").as_str() {
                "bounds" => 1,
                "center" => 2,
                _ => 0,
            };
            be.find(pid, &q(query, "sel"), o)
        }
        "/a11y/action" => {
            let v = q(query, "value");
            be.action(pid, &q(query, "sel"), &q(query, "action"), if v.is_empty() { None } else { Some(&v) })
        }
        "/a11y/clipboard" => {
            let set = q(query, "set");
            if set.is_empty() { clipboard_get() } else { clipboard_set(&set) }
        }
        "/a11y/click" => input_click(query),
        "/a11y/type" => input_text(query),
        "/a11y/key" => input_key(query),
        "/a11y/scroll" => input_scroll(query),
        "/a11y/drag" => input_drag(query),
        _ => Err(format!("unknown a11y path {path}")),
    };
    match result {
        Ok(body) => respond_text(reader, 200, "text/plain; charset=utf-8", &body),
        Err(e) => respond_text(reader, 500, "text/plain; charset=utf-8", &e),
    }
}

// ── input (HTTP) ────────────────────────────────────────────────────────────

fn num(query: &str, key: &str) -> i32 {
    q(query, key).parse().unwrap_or(0)
}

fn with_input(f: impl FnOnce(&crate::est::Input)) -> Result<String, String> {
    match crate::est::shared_input() {
        Some(m) => {
            let g = m.lock().unwrap();
            f(&g);
            Ok("ok".into())
        }
        None => Err("no input backend".into()),
    }
}

fn input_click(query: &str) -> Result<String, String> {
    let (x, y) = (num(query, "x") as i16, num(query, "y") as i16);
    let btn = match q(query, "button").as_str() {
        "right" => 3u8,
        "middle" => 2u8,
        _ => 1u8,
    };
    let count = num(query, "count").max(1);
    with_input(|i| {
        i.move_to(x, y);
        for _ in 0..count {
            i.button(btn, true);
            i.button(btn, false);
        }
    })
}

fn input_text(query: &str) -> Result<String, String> {
    let text = q(query, "text");
    with_input(|i| {
        for ch in text.chars() {
            if let Some((kc, need_shift)) = char_keycode(ch) {
                if need_shift {
                    i.key(50, true);
                }
                i.key(kc as u16, true);
                i.key(kc as u16, false);
                if need_shift {
                    i.key(50, false);
                }
            }
        }
    })
}

fn input_key(query: &str) -> Result<String, String> {
    let name = q(query, "name");
    with_input(|i| {
        if let Some(kc) = key_by_name(&name) {
            i.key(kc as u16, true);
            i.key(kc as u16, false);
        }
    })
}

// The Linux and Windows key maps have the same shape but different keycodes.
#[cfg(not(windows))]
fn char_keycode(ch: char) -> Option<(u8, bool)> { crate::x11::char_keycode(ch) }
#[cfg(windows)]
fn char_keycode(ch: char) -> Option<(u16, bool)> { crate::win::char_keycode(ch) }
#[cfg(not(windows))]
fn key_by_name(name: &str) -> Option<u8> { crate::x11::key_by_name(name) }
#[cfg(windows)]
fn key_by_name(name: &str) -> Option<u16> { crate::win::key_by_name(name) }

fn input_scroll(query: &str) -> Result<String, String> {
    let (x, y) = (num(query, "x") as i16, num(query, "y") as i16);
    let (dx, dy) = (num(query, "dx"), num(query, "dy"));
    with_input(|i| {
        i.move_to(x, y);
        i.scroll(dx, dy);
    })
}

fn input_drag(query: &str) -> Result<String, String> {
    let (x1, y1) = (num(query, "x1") as i16, num(query, "y1") as i16);
    let (x2, y2) = (num(query, "x2") as i16, num(query, "y2") as i16);
    let ms = num(query, "ms").max(1) as u64;
    with_input(|i| {
        i.move_to(x1, y1);
        i.button(1, true);
        let steps = 8u64;
        for s in 1..=steps {
            let nx = x1 as i32 + (x2 as i32 - x1 as i32) * s as i32 / steps as i32;
            let ny = y1 as i32 + (y2 as i32 - y1 as i32) * s as i32 / steps as i32;
            i.move_to(nx as i16, ny as i16);
            std::thread::sleep(std::time::Duration::from_millis(ms / steps));
        }
        i.button(1, false);
    })
}

// ── WebSocket (RFC 6455, server side, binary frames only) ───────────────────

/// ws_pipe upgrades the request and relays binary frames to/from a fresh est TCP
/// connection — a transparent byte pipe (the daemon does not parse the frames).
fn ws_pipe(mut reader: BufReader<TcpStream>, headers: Vec<(String, String)>, est_port: u16) -> Result<(), String> {
    let key = headers
        .iter()
        .find(|(k, _)| k == "sec-websocket-key")
        .map(|(_, v)| v.clone())
        .ok_or("no Sec-WebSocket-Key")?;
    // Accept = base64(sha1(key + GUID)).
    let accept = {
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(key.as_bytes());
        h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        base64::engine::general_purpose::STANDARD.encode(h.finalize())
    };
    let mut sock = reader.get_ref().try_clone().map_err(|e| e.to_string())?;
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    sock.write_all(resp.as_bytes()).map_err(|e| e.to_string())?;

    let tcp = TcpStream::connect(("127.0.0.1", est_port)).map_err(|e| e.to_string())?;
    let tcp = Arc::new(tcp);
    let tcp_r = Arc::clone(&tcp);
    let mut ws_w = sock.try_clone().map_err(|e| e.to_string())?;

    // est -> browser
    let reader_thread = thread::spawn(move || {
        let mut buf = [0u8; 1 << 16];
        let mut conn = &*tcp_r;
        loop {
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_write_binary(&mut ws_w, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tcp_r.shutdown(std::net::Shutdown::Both);
    });

    // browser -> est
    let mut tcp_w = &*tcp;
    loop {
        match ws_read(&mut reader) {
            Ok(Some(frame)) => {
                if tcp_w.write_all(&frame).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    let _ = tcp.shutdown(std::net::Shutdown::Both);
    let _ = reader_thread.join();
    Ok(())
}

/// ws_write_binary writes one unmasked, unfragmented binary frame.
fn ws_write_binary(w: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    let mut hdr = vec![0x82u8];
    let n = payload.len();
    if n < 126 {
        hdr.push(n as u8);
    } else if n <= u16::MAX as usize {
        hdr.push(126);
        hdr.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        hdr.push(127);
        hdr.extend_from_slice(&(n as u64).to_be_bytes());
    }
    w.write_all(&hdr)?;
    w.write_all(payload)?;
    w.flush()
}

/// ws_read reads one client frame (masked). Returns None for control frames.
fn ws_read(r: &mut BufReader<TcpStream>) -> Result<Option<Vec<u8>>, String> {
    let mut b2 = [0u8; 2];
    r.read_exact(&mut b2).map_err(|e| e.to_string())?;
    let opcode = b2[0] & 0x0f;
    let masked = b2[1] & 0x80 != 0;
    let mut len = (b2[1] & 0x7f) as usize;
    if len == 126 {
        let mut e = [0u8; 2];
        r.read_exact(&mut e).map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(e) as usize;
    } else if len == 127 {
        let mut e = [0u8; 8];
        r.read_exact(&mut e).map_err(|e| e.to_string())?;
        len = u64::from_be_bytes(e) as usize;
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask).map_err(|e| e.to_string())?;
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).map_err(|e| e.to_string())?;
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    match opcode {
        0x2 => Ok(Some(payload)),       // binary
        0x8 => Err("close".into()),     // close
        0x9 | 0xa => Ok(None),          // ping/pong: ignore
        _ => Ok(None),                  // text/continuation: ignore (est is binary)
    }
}
