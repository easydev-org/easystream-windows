//! easystream (`est://`) server — the SINGLE interface of easystream (EST.md).
//!
//! One TCP connection multiplexes the video stream (0x02), input (0x10-0x14),
//! clipboard (0x30-0x32) and a11y queries (0x20-0x27). Capture, encode and input
//! share this one process. Rust owns the lifetimes: the `Conn` is behind an
//! `Arc`, so the video thread and the control reader cannot outlive it.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

#[cfg(not(windows))]
use crate::x11::{char_keycode, key_by_name, X11};
#[cfg(not(windows))]
use crate::wayland::{Wayland, WlInput};
#[cfg(not(windows))]
use crate::x264::X264;
#[cfg(windows)]
use crate::win::{char_keycode as win_char_keycode, clipboard as win_clip, key_by_name as win_key_by_name, Wgc, WinInput};
#[cfg(windows)]
use crate::win_enc::MfEncoder;
/// Capture is either the X11 (XGetImage) or Wayland (wlr-screencopy) backend.
pub enum Capture {
    #[cfg(not(windows))]
    X11(X11),
    #[cfg(not(windows))]
    Wl(Wayland),
    #[cfg(windows)]
    Win(Wgc),
}

impl Capture {
    pub fn open() -> Result<Capture, String> {
        #[cfg(windows)]
        { return Ok(Capture::Win(Wgc::open()?)); }
        #[cfg(not(windows))]
        {
            if std::env::var("WAYLAND_DISPLAY").map(|v| !v.is_empty()).unwrap_or(false) {
                return Ok(Capture::Wl(Wayland::open()?));
            }
            Ok(Capture::X11(X11::open()?))
        }
    }
    /// grab returns the newest frame as top-down BGRA pixels (the X11/Wayland
    /// API; Windows uses grab_into so it can reuse a caller-owned buffer).
    #[cfg(not(windows))]
    pub fn grab(&self) -> Result<Vec<u8>, String> {
        match self {
            Capture::X11(x) => x.grab(),
            Capture::Wl(w) => w.grab(),
        }
    }
    /// grab_ptr returns a pointer into the capture buffer (zero-copy), valid
    /// until the next grab. None when the backend cannot expose one.
    /// grab_into copies the newest frame into `out` (reused) and reports its
    /// size. Only Windows implements it (WGC); other backends use grab_ptr.
    #[cfg(windows)]
    pub fn grab_into(&self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>, String> {
        match self {
            Capture::Win(w) => w.grab_into(out),
        }
    }
    pub fn grab_ptr(&self) -> Option<(*const u8, u32, u32)> {
        match self {
            #[cfg(not(windows))]
            Capture::X11(x) => x.grab_shm_ptr(),
            #[cfg(not(windows))]
            Capture::Wl(w) => w.grab_shm_ptr(),
            #[cfg(windows)]
            Capture::Win(_) => None,
        }
    }
    pub fn size(&self) -> (u32, u32) {
        match self {
            #[cfg(not(windows))]
            Capture::X11(x) => x.size(),
            #[cfg(not(windows))]
            Capture::Wl(_) => (1920, 1080),
            #[cfg(windows)]
            Capture::Win(w) => w.size(),
        }
    }
}

/// Input is the platform input backend (X11 XTest or Wayland wlr-virtual-*).
pub enum Input {
    #[cfg(not(windows))]
    X11(X11),
    #[cfg(not(windows))]
    Wl(WlInput),
    #[cfg(windows)]
    Win(WinInput),
}

impl Input {
    pub fn move_to(&self, x: i16, y: i16) {
        match self {
            #[cfg(not(windows))]
            Input::X11(i) => i.move_to(x, y),
            #[cfg(not(windows))]
            Input::Wl(i) => i.move_to(x, y),
            #[cfg(windows)]
            Input::Win(i) => i.move_to(x, y),
        }
    }
    pub fn button(&self, num: u8, down: bool) {
        match self {
            #[cfg(not(windows))]
            Input::X11(i) => i.button(num, down),
            #[cfg(not(windows))]
            Input::Wl(i) => i.button(num, down),
            #[cfg(windows)]
            Input::Win(i) => i.button(num, down),
        }
    }
    pub fn scroll(&self, dx: i32, dy: i32) {
        match self {
            #[cfg(not(windows))]
            Input::X11(i) => {
                let (vbtn, vn) = if dy > 0 { (5u8, dy) } else { (4u8, -dy) };
                for _ in 0..vn { i.button(vbtn, true); i.button(vbtn, false); }
                let (hbtn, hn) = if dx > 0 { (7u8, dx) } else { (6u8, -dx) };
                for _ in 0..hn { i.button(hbtn, true); i.button(hbtn, false); }
            }
            #[cfg(not(windows))]
            Input::Wl(i) => i.scroll(dx, dy),
            #[cfg(windows)]
            Input::Win(i) => i.scroll(dx, dy),
        }
    }
    pub fn key(&self, keycode: u16, down: bool) {
        match self {
            #[cfg(not(windows))]
            Input::X11(i) => i.key(keycode as u8, down),
            #[cfg(not(windows))]
            Input::Wl(i) => i.key(keycode as u8, down),
            #[cfg(windows)]
            Input::Win(i) => i.key(keycode, down),
        }
    }

}

/// Encoder is the platform H.264 encoder (libx264 on Linux, Media Foundation on
/// Windows). Both take BGRA and return an Annex-B access unit.
pub enum Encoder {
    #[cfg(not(windows))]
    X264(X264),
    #[cfg(windows)]
    Mf(MfEncoder),
}

impl Encoder {
    fn new(w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Encoder, String> {
        #[cfg(windows)]
        { return Ok(Encoder::Mf(MfEncoder::new(w, h, fps, bitrate)?)); }
        #[cfg(not(windows))]
        { Ok(Encoder::X264(X264::new(w, h, fps, bitrate)?)) }
    }
    fn force_idr(&mut self) {
        match self {
            #[cfg(not(windows))]
            Encoder::X264(e) => e.force_idr(),
            #[cfg(windows)]
            Encoder::Mf(e) => e.force_idr(),
        }
    }
    fn encode(&mut self, bgra: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        match self {
            #[cfg(not(windows))]
            Encoder::X264(e) => e.encode(bgra).map(|au| if au.is_empty() { Vec::new() } else { vec![au] }),
            #[cfg(windows)]
            Encoder::Mf(e) => e.encode(bgra),
        }
    }
    fn encode_ptr(&mut self, p: *const u8, len: u32) -> Result<Vec<Vec<u8>>, String> {
        match self {
            #[cfg(not(windows))]
            Encoder::X264(e) => e.encode_ptr(p, len).map(|au| if au.is_empty() { Vec::new() } else { vec![au] }),
            #[cfg(windows)]
            Encoder::Mf(e) => {
                let slice = unsafe { std::slice::from_raw_parts(p, len as usize) };
                e.encode(slice)
            }
        }
    }
}

const T_HELLO: u8 = 0x01;
const T_VIDEO: u8 = 0x02;
const T_MOVE: u8 = 0x10;
const T_BUTTON: u8 = 0x11;
const T_SCROLL: u8 = 0x12;
const T_KEY: u8 = 0x13;
const T_TEXT: u8 = 0x14;
const T_AUTH: u8 = 0x03;
const T_AUTH_OK: u8 = 0x04;
const T_AUDIO: u8 = 0x05;
const T_AUDIO_REQ: u8 = 0x06;
const T_AUDIO_OFF: u8 = 0x07;
const T_CLIP_GET: u8 = 0x30;
const T_CLIP_DATA: u8 = 0x31;
const T_CLIP_SET: u8 = 0x32;
const T_PING: u8 = 0x50;
const T_PONG: u8 = 0x51;

/// Conn is shared between the video thread and the control reader. The write
/// half is behind a Mutex (both threads write frames); the X11 handle is behind
/// a Mutex (capture and input both touch it).
pub struct Conn {
    stream: Mutex<TcpStream>,
    /// Access mode for THIS connection (View = read-only). Set by the Auth
    /// frame (0x03) or inherited from the HTTP pipe's token.
    mode: std::sync::atomic::AtomicU8,
    /// Whether this connection wants Audio frames (turned on by AudioReq 0x06).
    audio_on: std::sync::atomic::AtomicBool,
    /// The process-wide input backend (shared by every connection).
    input: &'static Option<Mutex<Input>>,
    /// In-process clipboard fallback (Linux/Wayland have no system clipboard
    /// here); Windows uses the real Win32 clipboard instead.
    #[cfg(not(windows))]
    clipboard: Mutex<String>,
}

impl Conn {
    fn write_frame(&self, typ: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut hdr = [0u8; 6];
        hdr[0] = typ;
        hdr[2..6].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        let mut s = self.stream.lock().unwrap();
        s.write_all(&hdr)?;
        s.write_all(payload)?;
        s.flush()
    }

    /// with_input runs `f` with the X11 input connection, if any (Wayland has no
    /// input backend wired yet, so input frames are ignored there).
    fn with_input(&self, f: impl FnOnce(&Input)) {
        if let Some(i) = self.input.as_ref() {
            if let Ok(x) = i.lock() {
                f(&x);
            }
        }
    }
}

pub fn serve(port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    println!("easystream est server on 127.0.0.1:{port}");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || {
                    // A panic in one connection must NEVER kill the listener.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Err(e) = handle(stream) {
                            eprintln!("est conn: {e}");
                        }
                    }));
                    if r.is_err() {
                        eprintln!("est conn: panicked");
                    }
                });
            }
            Err(e) => eprintln!("est accept: {e}"),
        }
    }
    Ok(())
}

/// Broadcaster runs ONE capture+encode pipeline and fans the access units out to
/// every subscribed connection. Without it, each /stream/ws connection started
/// its own pump (N tabs = N x CPU).
struct Broadcaster {
    subs: Mutex<Vec<mpsc::SyncSender<Arc<Vec<u8>>>>>,
    /// Audio (Opus) subscribers, separate from video.
    asubs: Mutex<Vec<mpsc::SyncSender<Arc<Vec<u8>>>>>,
    /// bumped whenever a new viewer subscribes, so the pump sends a keyframe.
    new_sub: std::sync::atomic::AtomicBool,
}

impl Broadcaster {
    fn subscribe(&self) -> mpsc::Receiver<Arc<Vec<u8>>> {
        // Bounded so a slow/stalled viewer cannot grow an unbounded backlog in
        // the daemon (it just drops frames for THAT viewer).
        let (tx, rx) = mpsc::sync_channel(4);
        if let Ok(mut s) = self.subs.lock() {
            s.push(tx);
        }
        self.new_sub.store(true, std::sync::atomic::Ordering::Relaxed);
        rx
    }
    fn broadcast(&self, au: Arc<Vec<u8>>) {
        if let Ok(mut s) = self.subs.lock() {
            // try_send: never block the pump on a slow consumer. Full -> drop
            // this frame for that subscriber (the next one keeps it current).
            s.retain(|tx| match tx.try_send(au.clone()) {
                Ok(()) => true,
                Err(mpsc::TrySendError::Full(_)) => true,
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
        }
    }
    fn has_subs(&self) -> bool {
        self.subs.lock().map(|s| !s.is_empty()).unwrap_or(false)
    }
    fn subscribe_audio(&self) -> mpsc::Receiver<Arc<Vec<u8>>> {
        let (tx, rx) = mpsc::sync_channel(8);
        if let Ok(mut s) = self.asubs.lock() { s.push(tx); }
        rx
    }
    fn broadcast_audio(&self, pkt: Arc<Vec<u8>>) {
        if let Ok(mut s) = self.asubs.lock() {
            s.retain(|tx| !matches!(tx.try_send(pkt.clone()), Err(mpsc::TrySendError::Disconnected(_))));
        }
    }
    fn has_audio_subs(&self) -> bool {
        self.asubs.lock().map(|s| !s.is_empty()).unwrap_or(false)
    }
}

static BC: OnceLock<Arc<Broadcaster>> = OnceLock::new();

fn broadcaster() -> Arc<Broadcaster> {
    BC.get_or_init(|| {
        let b = Arc::new(Broadcaster {
            subs: Mutex::new(Vec::new()),
            asubs: Mutex::new(Vec::new()),
            new_sub: std::sync::atomic::AtomicBool::new(false),
        });
        let bb = Arc::clone(&b);
        thread::spawn(move || pump(bb));
        let ba = Arc::clone(&b);
        thread::spawn(move || audio_pump(ba));
        b
    })
    .clone()
}

/// audio_pump is the single audio capture+encode loop (20 ms Opus frames),
/// shared by every audio subscriber.
fn audio_pump(bc: Arc<Broadcaster>) {
    let mut cap: Option<crate::audio::Capture> = None;
    let mut enc: Option<crate::opus::Opus> = None;
    loop {
        if !bc.has_audio_subs() {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        if cap.is_none() {
            match crate::audio::Capture::open() {
                Ok(c) => cap = Some(c),
                Err(e) => { eprintln!("audio pump: {e}"); thread::sleep(Duration::from_millis(1000)); continue; }
            }
        }
        if enc.is_none() {
            match crate::opus::Opus::new(crate::audio::RATE, crate::audio::CHANNELS, 96_000) {
                Ok(e) => enc = Some(e),
                Err(e) => { eprintln!("audio pump: opus {e}"); thread::sleep(Duration::from_millis(1000)); continue; }
            }
        }
        match cap.as_mut().unwrap().read_frame() {
            Some(pcm) => {
                let pkt = enc.as_mut().unwrap().encode(&pcm);
                if !pkt.is_empty() { bc.broadcast_audio(Arc::new(pkt)); }
            }
            None => { cap = None; thread::sleep(Duration::from_millis(500)); }
        }
    }
}

/// pump is the single capture+encode loop, shared by all connections. It idles
/// (no capture, no encode) while nobody is subscribed.
fn pump(bc: Arc<Broadcaster>) {
    let frame_dur = Duration::from_millis(33);
    let mut cap: Option<Capture> = None;
    let mut enc: Option<Encoder> = None;
    // Dirty-frame detection: skip encoding when the screen did not change (a
    // static desktop then costs almost no CPU). We still emit one frame every
    // `keepalive_frames` so the stream stays warm and late joiners get data.
    let mut prev: Option<Vec<u8>> = None;
    let mut skipped: u32 = 0;
    // Emit at least one frame every ~0.5s even when nothing changes, so a
    // missed damage event can never freeze the stream for long.
    const KEEPALIVE_FRAMES: u32 = 15;
    // The encoder needs a few frames of warm-up before it emits its first access
    // unit, and WGC only delivers frames when the screen changes: force-feed the
    // first few so the stream always starts on a static screen.
    let mut warmup: u32 = 5;
    // Reused "last frame" buffer for the no-zero-copy path (Windows WGC always,
    // and X11/Wayland when XSHM is unavailable).
    // Windows reads the initial None (grab_into fills it); the X11/Wayland arm
    // assigns it before the only read, so it starts uninitialised there.
    #[cfg(windows)]
    let mut last: Option<Vec<u8>> = None;
    #[cfg(not(windows))]
    let mut last: Option<Vec<u8>>;
    loop {
        if !bc.has_subs() {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        {
            // Periodic memory trace: the wayland path was OOM-killing the pod,
            // and we need to know whether the pump is the one growing.
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            if N.fetch_add(1, Ordering::Relaxed) % 100 == 0 {
                if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
                    let rss_pages: u64 = s.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
                    eprintln!("pump: rss={}MB subs={}", rss_pages * 4096 / 1048576, bc.has_subs());
                }
            }
        }
        let start = std::time::Instant::now();
        if cap.is_none() {
            match Capture::open() {
                Ok(c) => cap = Some(c),
                Err(e) => {
                    eprintln!("est pump: capture {e}");
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }
        let (w, h) = cap.as_ref().unwrap().size();
        if enc.is_none() {
            match Encoder::new(w, h, 30, 12_000_000) {
                Ok(e) => enc = Some(e),
                Err(e) => {
                    eprintln!("est pump: encoder {e}");
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }
        // A new viewer (or the periodic keepalive) needs a keyframe.
        let want_key = bc.new_sub.swap(false, std::sync::atomic::Ordering::Relaxed)
            || skipped >= KEEPALIVE_FRAMES;
        if want_key {
            enc.as_mut().unwrap().force_idr();
        }
        // Zero-copy: X11 exposes a pointer into the XSHM segment, so we can
        // compare and encode straight from it (no per-frame 8 MB Vec copy).
        // Wayland (no pointer API here) still copies into a Vec.
        // Grab (zero-copy pointer on X11; Vec otherwise) and decide.
        let ptr = cap.as_ref().unwrap().grab_ptr();
        let (changed, au) = match ptr {
            Some((p, w, h)) => {
                let n = (w as usize) * (h as usize) * 4;
                let cur = unsafe { std::slice::from_raw_parts(p, n) };
                let changed = match &prev {
                    None => true,
                    Some(prev) => {
                        if prev.len() != n {
                            true
                        } else {
                            // Compare only B,G,R — the X/A byte of an XRGB shm
                            // buffer is undefined and changes every frame.
                            prev.iter()
                                .enumerate()
                                .step_by(397)
                                .any(|(i, a)| i % 4 != 3 && *a != cur[i])
                        }
                    }
                };
                if changed || want_key {
                    // Reuse the previous frame's buffer instead of allocating a
                    // fresh ~8 MB Vec every frame: a large allocation per frame
                    // goes through mmap/munmap and ballooned RSS until the pod
                    // was OOM-killed (wayland captures at full rate, so every
                    // frame changed). Copy into the existing buffer instead.
                    match prev.as_mut() {
                        Some(buf) if buf.len() == n => buf.copy_from_slice(cur),
                        _ => prev = Some(cur.to_vec()),
                    }
                    (true, enc.as_mut().unwrap().encode_ptr(p, n as u32).ok())
                } else {
                    (false, None)
                }
            }
            None => {
                // No zero-copy pointer (Windows/other): grab a Vec. WGC only
                // delivers frames when the screen changes, so a miss reuses the
                // last frame; warmup/keepalive force-feed the encoder anyway so
                // the stream starts and stays alive on a static screen.
                // Windows: copy straight into the reused `last` buffer, so a
                // frame never allocates a fresh ~8 MB Vec (that alloc failed
                // outright under the guest's tight RAM and aborted the daemon).
                #[cfg(windows)]
                let got = {
                    if last.is_none() { last = Some(Vec::new()); }
                    match cap.as_ref().unwrap().grab_into(last.as_mut().unwrap()) {
                        Ok(Some(_)) => true,
                        Ok(None) => false,
                        Err(e) => {
                            eprintln!("est pump: grab {e}");
                            thread::sleep(frame_dur);
                            continue;
                        }
                    }
                };
                #[cfg(not(windows))]
                let got = match cap.as_ref().unwrap().grab() {
                    Ok(px) => { last = Some(px); true }
                    Err(e) => {
                        eprintln!("est pump: grab {e}");
                        thread::sleep(frame_dur);
                        continue;
                    }
                };
                if got {
                    skipped = 0;
                } else {
                    skipped += 1;
                }
                if let Some(px) = last.as_ref() {
                    // The MS encoder needs a CONTINUOUS input timeline, so feed it
                    // on every tick (the last frame when the screen is static) —
                    // gating on "changed" starves it and it never emits. The
                    // encoder's own rate control drops redundant frames.
                    if warmup > 0 { warmup -= 1; }
                    let r = enc.as_mut().unwrap().encode(px);
                    if let Err(e) = &r { eprintln!("est pump: encode {e}"); }
                    (true, r.ok())
                } else {
                    (false, None)
                }
            }
        };
        if !changed {
            // Idle: skip the encode and back off the poll rate (a grab is ~8 MB
            // of memcpy), capped so a change is noticed within ~200 ms.
            skipped += 1;
            let backoff = frame_dur * (1 + skipped.min(5));
            if let Some(rest) = backoff.checked_sub(start.elapsed()) {
                thread::sleep(rest);
            }
            continue;
        }
        skipped = 0;
        if let Some(aus) = au {
            for au in aus {
                if !au.is_empty() {
                    bc.broadcast(Arc::new(au));
                }
            }
        }
        if let Some(rest) = frame_dur.checked_sub(start.elapsed()) {
            thread::sleep(rest);
        }
    }
}

/// shared_input returns the process-wide input backend, opening it on first use.
/// Input is a single resource (one XTest connection / one wl_seat + virtual
/// keyboard / one SendInput sender), so every connection shares it.
pub fn shared_input() -> &'static Option<Mutex<Input>> {
    static IN: OnceLock<Option<Mutex<Input>>> = OnceLock::new();
    IN.get_or_init(|| {
        #[cfg(windows)]
        { WinInput::open().ok().map(|i| Mutex::new(Input::Win(i))) }
        #[cfg(not(windows))]
        {
            let wayland = std::env::var("WAYLAND_DISPLAY").map(|v| !v.is_empty()).unwrap_or(false);
            if wayland {
                match WlInput::open() {
                    Ok(i) => Some(Mutex::new(Input::Wl(i))),
                    Err(e) => { eprintln!("wayland input: {e}"); None }
                }
            } else {
                X11::open().ok().map(|i| Mutex::new(Input::X11(i)))
            }
        }
    })
}

/// shared_size returns the captured screen size, opening the capture ONCE (and
/// reusing it for the pump). On Windows this is the only WGC session.
fn shared_size() -> (u32, u32) {
    static SZ: OnceLock<(u32, u32)> = OnceLock::new();
    *SZ.get_or_init(|| {
        #[cfg(windows)]
        { Wgc::open().map(|c| c.size()).unwrap_or((1920, 1080)) }
        #[cfg(not(windows))]
        { (1920u32, 1080u32) }
    })
}

fn handle(stream: TcpStream) -> Result<(), String> {
    // ONE shared input backend for the whole process. Creating a fresh one per
    // connection was heavy (Wayland: a new wl_display + virtual keyboard;
    // Windows: a new D3D device) and made the daemon cost scale with viewers.
    let input = shared_input();
    // Screen size comes from the shared capture, never a per-connection capture:
    // on Windows Wgc::open() creates a frame pool AND calls StartCapture, so a
    // per-connection open meant N capture sessions.
    let (w, h) = shared_size();
    let conn = Arc::new(Conn {
        stream: Mutex::new(stream.try_clone().map_err(|e| e.to_string())?),
        // When auth is ON, an unauthenticated connection is READ-ONLY until it
        // sends a valid Auth frame — never Control by default.
        mode: std::sync::atomic::AtomicU8::new(if crate::auth::is_open() {
            crate::auth::Mode::Control as u8
        } else {
            crate::auth::Mode::View as u8
        }),
        audio_on: std::sync::atomic::AtomicBool::new(false),
        input,
        // (input is Option<Mutex<X11>>)
        #[cfg(not(windows))]
        clipboard: Mutex::new(String::new()),
    });

    // Hello (EST.md): version, codec, w, h, fps.
    let mut hello = vec![0u8; 7 + 8];
    hello[0] = 1;
    hello[1] = 1;
    hello[2..4].copy_from_slice(&(w as u16).to_be_bytes());
    hello[4..6].copy_from_slice(&(h as u16).to_be_bytes());
    hello[6] = 30;
    // Audio block @7: u8 present, u8 codec(1=Opus), u32 rate, u8 ch, u8 ms.
    // Set when the audio pump actually has a capture source (see audio.rs).
    let (has_audio, a_rate, a_ch, a_ms) = crate::audio::params();
    hello[7] = if has_audio { 1 } else { 0 };
    hello[8] = 1; // Opus
    hello[9..13].copy_from_slice(&a_rate.to_be_bytes());
    hello[13] = a_ch;
    hello[14] = a_ms;
    conn.write_frame(T_HELLO, &hello).map_err(|e| e.to_string())?;

    // Subscribe to the SHARED pump: this connection only forwards the access
    // units, so many viewers share one capture+encode pipeline.
    let rx = broadcaster().subscribe();
    let arx = if hello[7] == 1 { Some(broadcaster().subscribe_audio()) } else { None };
    let vc = Arc::clone(&conn);
    let video = thread::spawn(move || {
        while let Ok(au) = rx.recv() {
            if vc.write_frame(T_VIDEO, &au).is_err() {
                return;
            }
        }
    });

    // Audio forwarder (Opus packets -> T_AUDIO frames), gated by audio_on.
    let audio_thread = arx.map(|arx| {
        let ac = Arc::clone(&conn);
        thread::spawn(move || {
            while let Ok(pkt) = arx.recv() {
                if ac.audio_on.load(std::sync::atomic::Ordering::Relaxed)
                    && ac.write_frame(T_AUDIO, &pkt).is_err()
                {
                    return;
                }
            }
        })
    });

    // Control reader on this thread.
    control_loop(&conn, stream)?;
    // The client is gone; the pump will exit on its next failed write.
    let _ = video.join();
    if let Some(t) = audio_thread { let _ = t.join(); }
    Ok(())
}

fn control_loop(conn: &Arc<Conn>, mut s: TcpStream) -> Result<(), String> {
    let mut hdr = [0u8; 6];
    loop {
        if s.read_exact(&mut hdr).is_err() {
            return Ok(());
        }
        let typ = hdr[0];
        let len = u32::from_be_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len];
        if len > 0 && s.read_exact(&mut body).is_err() {
            return Ok(());
        }
        if let Err(e) = dispatch(conn, typ, &body) {
            eprintln!("est dispatch: {e}");
        }
    }
}

fn u16at(b: &[u8], o: usize) -> i16 {
    i16::from_be_bytes([b[o], b[o + 1]])
}

fn dispatch(conn: &Arc<Conn>, typ: u8, b: &[u8]) -> Result<(), String> {
    if typ == T_AUTH {
        let tok = std::str::from_utf8(b).unwrap_or("");
        let mode = crate::auth::authorize(Some(tok));
        conn.mode.store(mode as u8, std::sync::atomic::Ordering::SeqCst);
        let _ = conn.write_frame(T_AUTH_OK, &[mode as u8]);
        eprintln!("est: auth -> {:?}", mode);
        return Ok(());
    }
    if typ == T_AUDIO_REQ {
        conn.audio_on.store(true, std::sync::atomic::Ordering::SeqCst);
        return Ok(());
    }
    if typ == T_AUDIO_OFF {
        conn.audio_on.store(false, std::sync::atomic::Ordering::SeqCst);
        return Ok(());
    }
    // View mode: drop every frame that would CHANGE the guest.
    if conn.mode.load(std::sync::atomic::Ordering::SeqCst) == crate::auth::Mode::View as u8
        && crate::auth::is_mutation(typ)
    {
        return Ok(());
    }
    match typ {
        T_MOVE => {
            conn.with_input(|x| x.move_to(u16at(b, 0), u16at(b, 2)));
        }
        T_BUTTON => {
            conn.with_input(|x| x.button(b[0], b[1] != 0));
        }
        T_SCROLL => {
            let dx = u16at(b, 4);
            let dy = u16at(b, 6);
            conn.with_input(|x| x.scroll(dx as i32, dy as i32));
        }
        T_KEY => {
            // nameLen:u8 name nameLen char:u32 down:u8
            if b.len() >= 6 {
                let n = b[0] as usize;
                if b.len() >= 1 + n + 4 + 1 {
                    let name = std::str::from_utf8(&b[1..1 + n]).unwrap_or("");
                    let ch = u32::from_le_bytes([
                        b[1 + n],
                        b[2 + n],
                        b[3 + n],
                        b[4 + n],
                    ]);
                    let down = b[1 + n + 4] != 0;
                    conn.with_input(|x| {
                        #[cfg(windows)]
                        {
                            let shift: u16 = 0x10; // VK_SHIFT
                            // A single-character name is a literal character (a
                            // Ctrl+C combo arrives as name="c"); multi-char names
                            // are the named keys ("Return", "F5", …).
                            let mut handled = false;
                            if !name.is_empty() {
                                if let Some(k) = win_key_by_name(name) {
                                    x.key(k, down);
                                    handled = true;
                                }
                            }
                            let c = if !handled && !name.is_empty() && name.chars().count() == 1 {
                                name.chars().next().unwrap()
                            } else {
                                char::from_u32(ch).unwrap_or('\0')
                            };
                            if !handled && c != '\0' {
                                if let Some((k, need_shift)) = win_char_keycode(c) {
                                    if need_shift { x.key(shift, down); }
                                    x.key(k, down);
                                    if need_shift { x.key(shift, !down); }
                                }
                            }
                        }
                        #[cfg(not(windows))]
                        {
                            // Single-char name = a literal character (Ctrl+C);
                            // multi-char = a named key ("Return", "F5", …).
                            let mut handled = false;
                            if !name.is_empty() {
                                if let Some(k) = key_by_name(name) {
                                    x.key(k as u16, down);
                                    handled = true;
                                }
                            }
                            let c = if !handled && !name.is_empty() && name.chars().count() == 1 {
                                name.chars().next().unwrap()
                            } else {
                                char::from_u32(ch).unwrap_or('\0')
                            };
                            if !handled && c != '\0' {
                                if let Some((k, need_shift)) = char_keycode(c) {
                                if need_shift {
                                    x.key(50, down);
                                }
                                x.key(k as u16, down);
                                if need_shift {
                                    x.key(50, !down);
                                }
                            }
                        }
                        }
                    });
                }
            }
        }
        T_TEXT => {
            conn.with_input(|x| {
                #[cfg(windows)]
                {
                    let shift: u16 = 0x10;
                    for ch in b {
                        if let Some((kc, need_shift)) = win_char_keycode(*ch as char) {
                            if need_shift { x.key(shift, true); }
                            x.key(kc, true);
                            x.key(kc, false);
                            if need_shift { x.key(shift, false); }
                        }
                    }
                }
                #[cfg(not(windows))]
                for ch in b {
                    if let Some((kc, need_shift)) = char_keycode(*ch as char) {
                        if need_shift {
                            x.key(50, true);
                        }
                        x.key(kc as u16, true);
                        x.key(kc as u16, false);
                        if need_shift {
                            x.key(50, false);
                        }
                    }
                }
            });
        }
        T_CLIP_GET => {
            #[cfg(windows)]
            let text = win_clip::get();
            #[cfg(not(windows))]
            let text = conn.clipboard.lock().unwrap().clone();
            let mut p = Vec::with_capacity(8 + text.len());
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(&(text.len() as u32).to_be_bytes());
            p.extend_from_slice(text.as_bytes());
            conn.write_frame(T_CLIP_DATA, &p).map_err(|e| e.to_string())?;
        }
        T_CLIP_SET => {
            if b.len() >= 4 {
                let n = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
                if b.len() >= 4 + n {
                    let text = String::from_utf8_lossy(&b[4..4 + n]).to_string();
                    #[cfg(windows)]
                    win_clip::set(&text);
                    #[cfg(not(windows))]
                    { *conn.clipboard.lock().unwrap() = text; }
                }
            }
        }
        T_PING => conn.write_frame(T_PONG, b).map_err(|e| e.to_string())?,
        _ => {}
    }
    Ok(())
}
