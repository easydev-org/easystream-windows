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

use crate::win::{char_keycode as win_char_keycode, clipboard as win_clip, key_by_name as win_key_by_name, Wgc, WinInput};
use crate::win_enc::MfEncoder;
/// Capture is the Windows.Graphics.Capture (WGC) backend.
pub enum Capture {
    Win(Wgc),
}

impl Capture {
    pub fn open() -> Result<Capture, String> {
        { return Ok(Capture::Win(Wgc::open()?)); }
    }
    /// grab returns the newest frame as top-down BGRA pixels.
    /// grab_ptr returns a pointer into the capture buffer (zero-copy), valid
    /// until the next grab. None when the backend cannot expose one.
    /// grab_into copies the newest frame into `out` (reused) and reports its
    /// size. Only Windows implements it (WGC); other backends use grab_ptr.
    pub fn grab_into(&self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>, String> {
        match self {
            Capture::Win(w) => w.grab_into(out),
        }
    }
    pub fn grab_ptr(&self) -> Option<(*const u8, u32, u32)> {
        match self {
            Capture::Win(_) => None,
        }
    }
    pub fn size(&self) -> (u32, u32) {
        match self {
            Capture::Win(w) => w.size(),
        }
    }
}

/// Input is the Windows input backend (SendInput).
pub enum Input {
    Win(WinInput),
}

impl Input {
    pub fn move_to(&self, x: i16, y: i16) {
        match self {
            Input::Win(i) => i.move_to(x, y),
        }
    }
    pub fn button(&self, num: u8, down: bool) {
        match self {
            Input::Win(i) => i.button(num, down),
        }
    }
    pub fn scroll(&self, dx: i32, dy: i32) {
        match self {
            Input::Win(i) => i.scroll(dx, dy),
        }
    }
    pub fn key(&self, keycode: u16, down: bool) {
        match self {
            Input::Win(i) => i.key(keycode, down),
        }
    }

}

/// Encoder is the H.264 encoder (Media Foundation). It takes BGRA and returns
/// an Annex-B access unit.
pub enum Encoder {
    Mf(MfEncoder),
}

impl Encoder {
    fn new(w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Encoder, String> {
        { return Ok(Encoder::Mf(MfEncoder::new(w, h, fps, bitrate)?)); }
    }
    fn force_idr(&mut self) {
        match self {
            Encoder::Mf(e) => e.force_idr(),
        }
    }
    fn encode(&mut self, bgra: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        match self {
            Encoder::Mf(e) => e.encode(bgra),
        }
    }
    fn encode_ptr(&mut self, p: *const u8, len: u32) -> Result<Vec<Vec<u8>>, String> {
        match self {
            Encoder::Mf(e) => {
                let slice = unsafe { std::slice::from_raw_parts(p, len as usize) };
                e.encode(slice)
            }
        }
    }

    /// Encode a frame, scaling from the capture geometry when the encoder emits
    /// something smaller (标清/高清).
    fn encode_ptr_sized(
        &mut self,
        p: *const u8,
        len: u32,
        sw: u32,
        sh: u32,
    ) -> Result<Vec<Vec<u8>>, String> {
        match self {
            Encoder::Mf(e) => {
                let slice = unsafe { std::slice::from_raw_parts(p, len as usize) };
                e.encode_sized(slice, sw, sh)
            }
        }
    }
}

const T_HELLO: u8 = 0x01;
/// Run-time reconfiguration (EST.md 0x08/0x09). A client asks; the server owns
/// the encoder and answers with ConfigAck, then a fresh Hello.
const T_CONFIG_SET: u8 = 0x08;
const T_CONFIG_ACK: u8 = 0x09;
/// Ask for an IDR without changing any configuration (EST.md 0x0A). The latch it
/// sets is the one a new subscriber sets, so N requests in one frame interval
/// produce exactly one IDR.
const T_KEYFRAME_REQ: u8 = 0x0A;
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
/// half is behind a Mutex (both threads write frames); the input handle is
/// behind a Mutex (capture and input both touch it).
pub struct Conn {
    stream: Mutex<TcpStream>,
    /// Access mode for THIS connection (View = read-only). Set by the Auth
    /// frame (0x03) or inherited from the HTTP pipe's token.
    mode: std::sync::atomic::AtomicU8,
    /// Whether this connection wants Audio frames (turned on by AudioReq 0x06).
    audio_on: std::sync::atomic::AtomicBool,
    /// The process-wide input backend (shared by every connection).
    input: &'static Option<Mutex<Input>>,
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

    /// with_input runs `f` with the input connection, if any (there is no
    /// input backend wired yet, so input frames are ignored there).
    fn with_input(&self, f: impl FnOnce(&Input)) {
        if let Some(i) = self.input.as_ref() {
            if let Ok(x) = i.lock() {
                f(&x);
            }
        }
    }
}

/// Build a Hello for the CURRENT video configuration (EST.md).
///
/// 23 bytes: the v1 core (7), the audio block (8) and the video block at offset
/// 15 (8). The video block is APPENDED, so a client that stops at the audio
/// block parses this exactly as before.
fn hello_payload(cap_w: u32, cap_h: u32) -> Vec<u8> {
    let mut hello = vec![0u8; 7 + 8 + 8];
    hello[0] = 1;
    hello[1] = 1; // H.264
    // Report what the encoder EMITS, not the capture size: once someone picks
    // 标清 the two differ, and a decoder configured for the capture size rejects
    // every frame.
    let (out_w, out_h) = {
        let (ow, oh) = crate::vcfg::VIDEO.output();
        if ow != 0 && oh != 0 { (ow, oh) } else { (cap_w, cap_h) }
    };
    hello[2..4].copy_from_slice(&(out_w as u16).to_be_bytes());
    hello[4..6].copy_from_slice(&(out_h as u16).to_be_bytes());
    hello[6] = crate::vcfg::VIDEO.fps();
    let (has_audio, a_rate, a_ch, a_ms) = crate::audio::params();
    hello[7] = if has_audio { 1 } else { 0 };
    hello[8] = 1; // Opus
    hello[9..13].copy_from_slice(&a_rate.to_be_bytes());
    hello[13] = a_ch;
    hello[14] = a_ms;
    hello[15..23].copy_from_slice(&crate::vcfg::hello_block());
    hello
}

/// Every live connection, so a reconfiguration can be announced to all of them.
/// Weak references: a closed socket must drop out on its own.
static CONNS: OnceLock<Mutex<Vec<std::sync::Weak<Conn>>>> = OnceLock::new();

fn conns() -> &'static Mutex<Vec<std::sync::Weak<Conn>>> {
    CONNS.get_or_init(|| Mutex::new(Vec::new()))
}

fn register_conn(c: &Arc<Conn>) {
    if let Ok(mut v) = conns().lock() {
        v.retain(|w| w.strong_count() > 0);
        v.push(Arc::downgrade(c));
    }
}

/// Re-announce the video configuration to every live connection. Hello is the
/// only authoritative statement of geometry (EST.md), so viewers that stayed
/// connected after a ConfigSet would otherwise keep decoding at the old size.
fn send_hello_to_all(cap_w: u32, cap_h: u32) {
    let payload = hello_payload(cap_w, cap_h);
    if let Ok(v) = conns().lock() {
        for w in v.iter() {
            if let Some(c) = w.upgrade() {
                let _ = c.write_frame(T_HELLO, &payload);
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
    /// request_keyframe asks the pump for an IDR on its next frame. Idempotent:
    /// the latch is shared, so a client asking every frame still gets one IDR per
    /// frame interval.
    fn request_keyframe(&self) {
        self.new_sub.store(true, std::sync::atomic::Ordering::Relaxed);
    }

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
    // Which vcfg generation the current encoder was built from; a mismatch means
    // a ConfigSet landed and the encoder must be rebuilt.
    let mut enc_gen: u32 = u32::MAX;
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
    // when a zero-copy pointer is unavailable).
    // The reader fills the initial None (grab_into); the grab_ptr arm
    // assigns it before the only read, so it starts uninitialised there.
    let mut last: Option<Vec<u8>> = None;
    loop {
        if !bc.has_subs() {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        {
            // Periodic memory trace: a leak here OOM-killed the pod,
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
        let (cap_w, cap_h) = cap.as_ref().unwrap().size();
        // Publish the capture size once; vcfg keeps capture and OUTPUT geometry
        // apart because the screen source owns the former.
        crate::vcfg::VIDEO.set_capture(cap_w, cap_h);

        let gen = crate::vcfg::VIDEO.generation();
        if enc.is_none() || gen != enc_gen {
            let (ow, oh) = crate::vcfg::VIDEO.output();
            let fps = crate::vcfg::VIDEO.fps() as u32;
            let br = crate::vcfg::VIDEO.bitrate();
            // Drop the old encoder BEFORE opening the new one: Media Foundation
            // holds a hardware transform, and two at once fails to open on a
            // small VM.
            enc = None;
            match Encoder::new(ow, oh, fps, br) {
                Ok(e) => {
                    crate::vcfg::VIDEO.set_active_encoder(crate::vcfg::ENC_MF);
                    eprintln!(
                        "est pump: encoder opened {}x{} fps={} br={} gen={}",
                        ow, oh, fps, br, gen
                    );
                    enc = Some(e);
                    enc_gen = gen;
                    // The new encoder has no reference frames and every client
                    // reset gotKey on the new Hello, so the first frame out must
                    // be an IDR.
                    bc.new_sub.store(true, std::sync::atomic::Ordering::Relaxed);
                }
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
        // Zero-copy: when a pointer into the capture buffer is available, we can
        // compare and encode straight from it (no per-frame 8 MB Vec copy).
        // A copy into a Vec when no zero-copy pointer is available.
        // Grab (zero-copy pointer when available; Vec otherwise) and decide.
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
                    // was OOM-killed (capture runs at full rate, so every
                    // frame changed). Copy into the existing buffer instead.
                    match prev.as_mut() {
                        Some(buf) if buf.len() == n => buf.copy_from_slice(cur),
                        _ => prev = Some(cur.to_vec()),
                    }
                    // Pass the CAPTURE size: the encoder may be emitting a
                    // smaller frame and converts+scales in one pass.
                    (true, enc.as_mut().unwrap().encode_ptr_sized(p, n as u32, cap_w, cap_h).ok())
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
                if got {
                    skipped = 0;
                } else {
                    skipped += 1;
                }
                // Diagnostic: WGC returning no frame forever is the failure this
                // build is chasing, and it is silent otherwise.
                {
                    static G: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    let n = G.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 5 || n % 150 == 0 {
                        eprintln!(
                            "est pump: grab got={} skipped={} last={:?} cap={}x{}",
                            got, skipped,
                            last.as_ref().map(|v| v.len()),
                            cap_w, cap_h
                        );
                    }
                }
                if let Some(px) = last.as_ref() {
                    // The MS encoder needs a CONTINUOUS input timeline, so feed it
                    // on every tick (the last frame when the screen is static) —
                    // gating on "changed" starves it and it never emits. The
                    // encoder's own rate control drops redundant frames.
                    if warmup > 0 { warmup -= 1; }
                    // encode(), not encode_ptr_sized(): the encoder emits exactly
                    // the size it was opened with, and this buffer is that size.
                    //
                    // Passing (ptr, len, cap_w, cap_h) separately was the change
                    // that broke Windows capture. When cap_w*cap_h*4 and the
                    // buffer length disagree by even one byte, the slice handed
                    // to the converter is the wrong length and the NV12 pass reads
                    // past it - and Media Foundation then accepts the input and
                    // emits nothing at all, which is the silent failure this build
                    // was chasing. Letting the Vec carry its own length removes
                    // the chance of them disagreeing.
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
        { WinInput::open().ok().map(|i| Mutex::new(Input::Win(i))) }
    })
}

/// shared_size returns the captured screen size, opening the capture ONCE (and
/// reusing it for the pump). On Windows this is the only WGC session.
fn shared_size() -> (u32, u32) {
    static SZ: OnceLock<(u32, u32)> = OnceLock::new();
    *SZ.get_or_init(|| {
        { Wgc::open().map(|c| c.size()).unwrap_or((1920, 1080)) }
    })
}

fn handle(stream: TcpStream) -> Result<(), String> {
    // ONE shared input backend for the whole process. Creating a fresh one per
    // connection was heavy (a new capture session + input objects;
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
        // (input is Option<Mutex<Input>>)
    });

    let hello = hello_payload(w, h);
    conn.write_frame(T_HELLO, &hello).map_err(|e| e.to_string())?;

    register_conn(&conn);

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

/// Parse a 0x08 ConfigSet body into a Patch.
///
/// The cursor walks the frame strictly: every field knows its own length, and a
/// frame that ends early is an error, not a partial patch. Truncation is the
/// most likely corruption here (a client that grew a field and an older daemon
/// reading it), and applying half of it would move the stream somewhere nobody
/// asked for.
fn parse_config(b: &[u8]) -> (Option<crate::vcfg::Patch>, Option<String>) {
    let mut p = crate::vcfg::Patch::default();
    let mut i = 0usize;
    let need = |i: usize, n: usize| i + n <= b.len();
    if !need(i, 1) { return (None, Some("truncated: hasWidth".into())); }
    if b[i] != 0 {
        if !need(i + 1, 2) { return (None, Some("truncated: width".into())); }
        p.width = Some(u16::from_be_bytes([b[i + 1], b[i + 2]]) as u32);
    }
    i += 3;
    if !need(i, 1) { return (None, Some("truncated: hasHeight".into())); }
    if b[i] != 0 {
        if !need(i + 1, 2) { return (None, Some("truncated: height".into())); }
        p.height = Some(u16::from_be_bytes([b[i + 1], b[i + 2]]) as u32);
    }
    i += 3;
    if !need(i, 1) { return (None, Some("truncated: hasFps".into())); }
    if b[i] != 0 {
        if !need(i + 1, 1) { return (None, Some("truncated: fps".into())); }
        p.fps = Some(b[i + 1]);
    }
    i += 2;
    if !need(i, 1) { return (None, Some("truncated: hasBitrate".into())); }
    if b[i] != 0 {
        if !need(i + 1, 4) { return (None, Some("truncated: bitrate".into())); }
        p.bitrate = Some(u32::from_be_bytes([
            b[i + 1], b[i + 2], b[i + 3], b[i + 4],
        ]));
    }
    i += 5;
    if !need(i, 1) { return (None, Some("truncated: hasEncoder".into())); }
    if b[i] != 0 {
        if !need(i + 1, 1) { return (None, Some("truncated: encoder".into())); }
        let e = b[i + 1];
        // 0xFF is the documented "let the server pick" value; translate it to the
        // Patch's 0 so vcfg keeps one meaning for zero.
        p.encoder = Some(if e == 0xFF { 0 } else { e });
    }
    (Some(p), None)
}

/// Build a 0x09 ConfigAck body: ok byte, then a length-prefixed UTF-8 reason.
fn ack(ok: bool, msg: &str) -> Vec<u8> {
    let m = msg.as_bytes();
    let m = &m[..m.len().min(255)];
    let mut v = Vec::with_capacity(2 + m.len());
    v.push(if ok { 1 } else { 0 });
    v.push(m.len() as u8);
    v.extend_from_slice(m);
    v
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
    if typ == T_KEYFRAME_REQ {
        // Not a mutation: no guest state changes, so a read-only viewer may use
        // it. Refusing would strand that viewer on a corrupt picture.
        broadcaster().request_keyframe();
        return Ok(());
    }
    if typ == T_CONFIG_SET {
        let (patch, err) = parse_config(b);
        if let Some(e) = err {
            eprintln!("est: ConfigSet parse failed: {e}");
            let _ = conn.write_frame(T_CONFIG_ACK, &ack(false, &e));
            return Ok(());
        }
        match crate::vcfg::VIDEO.apply(&patch.unwrap()) {
            Ok(()) => {
                let (ow, oh) = crate::vcfg::VIDEO.output();
                eprintln!(
                    "est: ConfigSet ok -> {}x{} fps={} br={} gen={}",
                    ow, oh,
                    crate::vcfg::VIDEO.fps(),
                    crate::vcfg::VIDEO.bitrate(),
                    crate::vcfg::VIDEO.generation()
                );
                let _ = conn.write_frame(T_CONFIG_ACK, &ack(true, ""));
                // Tell EVERY viewer, not just the one that asked: Hello is the
                // only authoritative statement of geometry.
                let (cw, ch) = crate::vcfg::VIDEO.capture();
                send_hello_to_all(cw, ch);
            }
            Err(e) => {
                eprintln!("est: ConfigSet rejected: {e}");
                let _ = conn.write_frame(T_CONFIG_ACK, &ack(false, &e));
            }
        }
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
                    });
                }
            }
        }
        T_TEXT => {
            conn.with_input(|x| {
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
            });
        }
        T_CLIP_GET => {
            let text = win_clip::get();
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
                    win_clip::set(&text);
                }
            }
        }
        T_PING => conn.write_frame(T_PONG, b).map_err(|e| e.to_string())?,
        _ => {}
    }
    Ok(())
}
