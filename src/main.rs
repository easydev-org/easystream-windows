//! easystream (Rust) — the Linux/Windows screen daemon (see EST.md + A11Y_HTTP.md).
//!
//! ONE TCP connection multiplexes the H.264 video stream (0x02), input
//! (0x10-0x14), clipboard (0x30-0x32) and a11y queries (0x20-0x27). Capture,
//! encode and input injection all live in this one process, so there is no
//! cross-backend conflict. Rewritten in Rust so the ownership rules catch the
//! lifetime bugs that crashed the Zig version.
mod a11y;
#[cfg(not(windows))]
mod atspi;
mod est;
mod http;
mod png;
#[cfg(windows)]
mod uia;

// Linux backends (X11 / Wayland) use C shims + libx264.
#[cfg(not(windows))]
mod wayland;
#[cfg(not(windows))]
mod x11;
#[cfg(not(windows))]
mod x264;

// Windows backend (UIA / WGC / MF / SendInput) uses the `windows` crate.
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod win_enc;

use std::env;

fn main() {
    // Log panics (the guest runs us with stderr redirected to a file).
    std::panic::set_hook(Box::new(|info| {
        eprintln!("easystream panic: {info}");
    }));
    let port: u16 = env::var("EAZYSTREAM_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(48079);
    let http_port: u16 = env::var("EAZYSTREAM_HTTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(48078);
    let args: Vec<String> = env::args().collect();
    // Diagnostic one-shots (mirror the C# daemon's `wgctest`).
    #[cfg(windows)]
    if args.len() >= 2 && args[1] == "wgctest" {
        match win::Wgc::open() {
            Ok(w) => { let (a, b) = w.size(); println!("WGC ok {a}x{b}"); }
            Err(e) => println!("WGC FAILED: {e}"),
        }
        return;
    }
    #[cfg(windows)]
    if args.len() >= 2 && args[1] == "enc-test" {
        match win_enc::MfEncoder::new(1920, 1080, 30, 12_000_000) {
            Ok(_) => println!("MF encoder ok"),
            Err(e) => println!("MF encoder FAILED: {e}"),
        }
        return;
    }
    #[cfg(windows)]
    if args.len() >= 2 && args[1] == "uia-test" {
        match win::Uia::new() {
            Ok(u) => match u.apps() {
                Ok(a) => println!("UIA ok, {} apps", a.len()),
                Err(e) => println!("UIA apps FAILED: {e}"),
            },
            Err(e) => println!("UIA FAILED: {e}"),
        }
        return;
    }
    if args.len() >= 2 && args[1] == "serve" || args.len() < 2 {
        // The HTTP surface (webui + /est/ws pipe + /a11y) runs alongside the est
        // listener in the SAME process, so the daemon is the only thing a viewer
        // or the worker connects to.
        let a11y = std::sync::Arc::new(std::sync::Mutex::new(None));
        let a11y_http = std::sync::Arc::clone(&a11y);
        std::thread::spawn(move || {
            if let Err(e) = http::serve(http_port, port, a11y_http) {
                eprintln!("http: {e}");
            }
        });
        if let Err(e) = est::serve(port) {
            eprintln!("easystream: {e}");
            std::process::exit(1);
        }
        return;
    }
    // One-shot CLI (debugging) is a follow-up; the daemon is the contract.
    eprintln!("usage: easystream [serve]");
    std::process::exit(2);
}
