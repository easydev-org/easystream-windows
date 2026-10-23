//! Run-time video configuration: geometry, frame rate, bitrate and encoder.
//!
//! This is the Linux side of the EST.md reconfiguration contract. Until now the
//! encoder was opened with literals (Encoder::new(w, h, 30, 12_000_000)), so a
//! viewer could not trade quality for bandwidth at all.
//!
//! Two geometries are kept deliberately separate:
//!
//! * CAPTURE geometry - what the screen source produces. The daemon cannot
//!   change it on Linux (the compositor owns the output size), so it is
//!   recorded once and used to clamp requests.
//! * OUTPUT geometry - what the encoder emits. This is what ConfigSet changes,
//!   and what requests are validated against.
//!
//! Scaling happens after capture and before encode. Doing it any other way would
//! mean renegotiating the screen source, which neither wlr screencopy nor X11
//! XGetImage can do without tearing the pipeline down.

use std::sync::atomic::{AtomicU32, Ordering};

/// Encoder codes, matching what EST.md puts in Hello video block.
pub const ENC_UNKNOWN: u8 = 0;
pub const ENC_X264: u8 = 1;
pub const ENC_VAAPI: u8 = 2;   // unused here; kept for the shared numbering
pub const ENC_MF: u8 = 3;

/// Bitrate floor/ceiling. Below the floor H.264 at 30 fps stops producing
/// usable frames; above the ceiling one viewer can saturate the sandbox link.
/// Both are REFUSALS, not clamps: silently substituting a value would make the
/// UI believe it got what it asked for.
const MIN_BITRATE: u32 = 100_000;
const MAX_BITRATE: u32 = 50_000_000;
const MIN_DIM: u32 = 64;
const MAX_DIM: u32 = 7680;
const MIN_FPS: u8 = 1;
const MAX_FPS: u8 = 120;

/// The live configuration. Atomics rather than a Mutex: the pump reads these on
/// every frame and must never be blocked by a reader.
pub struct VideoConfig {
    cap_w: AtomicU32,
    cap_h: AtomicU32,
    out_w: AtomicU32,
    out_h: AtomicU32,
    fps: AtomicU32,
    bitrate: AtomicU32,
    /// 0 = let the server choose; otherwise a ENC_* constant.
    requested_enc: AtomicU32,
    /// The encoder actually in use, so Hello reports the truth even when a
    /// VAAPI request fell back to x264.
    active_enc: AtomicU32,
    /// Bumped on every accepted change so the pump notices cheaply.
    generation: AtomicU32,
}

/// A validated change request, as parsed from a ConfigSet frame.
#[derive(Default, Clone, Copy)]
pub struct Patch {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u8>,
    pub bitrate: Option<u32>,
    pub encoder: Option<u8>,
}

impl VideoConfig {
    pub const fn new() -> VideoConfig {
        VideoConfig {
            cap_w: AtomicU32::new(0),
            cap_h: AtomicU32::new(0),
            out_w: AtomicU32::new(0),
            out_h: AtomicU32::new(0),
            fps: AtomicU32::new(30),
            bitrate: AtomicU32::new(12_000_000),
            requested_enc: AtomicU32::new(0),
            active_enc: AtomicU32::new(ENC_UNKNOWN as u32),
            generation: AtomicU32::new(0),
        }
    }

    /// Record the capture source size. Called when capture starts; the output
    /// follows it until someone asks for something else.
    pub fn set_capture(&self, w: u32, h: u32) {
        self.cap_w.store(w, Ordering::Relaxed);
        self.cap_h.store(h, Ordering::Relaxed);
        if self.out_w.load(Ordering::Relaxed) == 0 {
            self.out_w.store(w, Ordering::Relaxed);
            self.out_h.store(h, Ordering::Relaxed);
        }
    }

    pub fn capture(&self) -> (u32, u32) {
        (self.cap_w.load(Ordering::Relaxed), self.cap_h.load(Ordering::Relaxed))
    }

    pub fn output(&self) -> (u32, u32) {
        (self.out_w.load(Ordering::Relaxed), self.out_h.load(Ordering::Relaxed))
    }

    pub fn fps(&self) -> u8 { self.fps.load(Ordering::Relaxed) as u8 }

    pub fn bitrate(&self) -> u32 { self.bitrate.load(Ordering::Relaxed) }

    pub fn generation(&self) -> u32 { self.generation.load(Ordering::Relaxed) }

    pub fn requested_encoder(&self) -> u8 { self.requested_enc.load(Ordering::Relaxed) as u8 }

    /// Record which encoder actually opened. The pump calls this after opening,
    /// because a VAAPI request may fall back to x264 and Hello must say so.
    pub fn set_active_encoder(&self, e: u8) {
        self.active_enc.store(e as u32, Ordering::Relaxed);
    }

    pub fn active_encoder(&self) -> u8 { self.active_enc.load(Ordering::Relaxed) as u8 }

    /// Apply a patch, validating it against the capture source.
    ///
    /// Returns Err(reason) and leaves the configuration untouched when any field
    /// is unacceptable: a partial apply would leave the encoder and the Hello
    /// describing different streams.
    pub fn apply(&self, p: &Patch) -> Result<(), String> {
        let (cw, ch) = self.capture();
        let (ow, oh) = self.output();
        let mut w = ow;
        let mut h = oh;
        // Geometry is all-or-nothing: changing only width would silently
        // distort the aspect ratio, and a scaling client always sends both.
        if p.width.is_some() || p.height.is_some() {
            w = p.width.unwrap_or(ow);
            h = p.height.unwrap_or(oh);
            for v in [w, h] {
                if !(MIN_DIM..=MAX_DIM).contains(&v) {
                    return Err(format!("size {} outside {}..{}", v, MIN_DIM, MAX_DIM));
                }
                if v % 2 != 0 {
                    return Err(format!("size {} is odd (H.264 4:2:0 needs even)", v));
                }
            }
            // Upscaling is refused rather than clamped: asking for more pixels
            // than the source has is a client bug, and a silent cap would make
            // the UI believe it received the larger size.
            if cw != 0 && ch != 0 && (w > cw || h > ch) {
                return Err(format!("size {}x{} exceeds capture {}x{}", w, h, cw, ch));
            }
        }
        let fps = p.fps.unwrap_or_else(|| self.fps());
        if !(MIN_FPS..=MAX_FPS).contains(&fps) {
            return Err(format!("fps {} outside {}..{}", fps, MIN_FPS, MAX_FPS));
        }
        let bitrate = p.bitrate.unwrap_or_else(|| self.bitrate());
        if !(MIN_BITRATE..=MAX_BITRATE).contains(&bitrate) {
            return Err(format!("bitrate {} outside {}..{}", bitrate, MIN_BITRATE, MAX_BITRATE));
        }
        if let Some(e) = p.encoder {
            if e != 0 && e != ENC_X264 && e != ENC_MF {
                // The macOS/Windows/Android codes are valid on the wire but
                // cannot be honoured here, so say so instead of pretending.
                return Err(format!("encoder {} not available on windows", e));
            }
        }
        self.out_w.store(w, Ordering::Relaxed);
        self.out_h.store(h, Ordering::Relaxed);
        self.fps.store(fps as u32, Ordering::Relaxed);
        self.bitrate.store(bitrate, Ordering::Relaxed);
        if let Some(e) = p.encoder { self.requested_enc.store(e as u32, Ordering::Relaxed); }
        // Bumping the generation LAST is the signal the pump waits for: every
        // field above must be visible before the counter changes.
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

pub static VIDEO: VideoConfig = VideoConfig::new();

/// The Hello video block (EST.md offsets 15..23).
///
/// flags bit0 advertises that ConfigSet is honoured, bit1 that the encoder is
/// hardware accelerated. The UI reads both to decide whether the quality row is
/// usable at all.
pub fn hello_block() -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0] = 1; // hasVideoConfig
    let enc = VIDEO.active_encoder();
    b[1] = enc;
    let mut flags = 0u8;
    flags |= 1; // reconfiguration supported
    if enc == ENC_VAAPI { flags |= 2; }
    b[2] = flags;
    b[3..7].copy_from_slice(&VIDEO.bitrate().to_be_bytes());
    // keyframe interval in frames: the pump forces an IDR every keepalive, and
    // two seconds is what the encoder keyint is set to.
    b[7] = VIDEO.fps().saturating_mul(2);
    b
}
