//! Capability probe.
//!
//! Mirrors the Linux daemon's /capabilities endpoint (see easydev-org/easystream
//! A11Y_HTTP.md) so the Go worker and any viewer can ask "which path did each
//! capability resolve to?" without reading logs. On Windows the answers are
//! mostly fixed, but the AUDIO one is not: whether WASAPI loopback actually
//! produced PCM depends on there being a render endpoint, and that is probed
//! lazily by audio.rs (proving it works means opening the device).
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Windows,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePath {
    /// Windows.Graphics.Capture (WGC).
    Wgc,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPath {
    /// SendInput.
    SendInput,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A11yPath {
    /// UI Automation (UIA).
    Uia,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPath {
    /// The real Win32 clipboard.
    Win32,
    None,
}

/// AudioPath is which capture backend actually produced PCM. It is reported as
/// a function (not a value) because audio.rs proves a backend by opening it and
/// reading a frame, which happens on its own schedule; asking before it
/// finishes says "Probing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPath {
    /// WASAPI loopback on the default render endpoint.
    WasapiLoopback,
    /// Still probing (audio::probe_done_pub() has not finished).
    Probing,
    None,
}

/// Capabilities is the result of probing the running host once.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub session: Session,
    pub capture: CapturePath,
    pub input: InputPath,
    pub a11y: A11yPath,
    pub clipboard: ClipboardPath,
    /// Filled in lazily: audio's answer arrives after its own probe.
    pub audio: fn() -> AudioPath,
    /// Human-readable detail for logs / the /capabilities endpoint.
    pub notes: Vec<String>,
}

impl Capabilities {
    pub fn to_json(&self) -> String {
        let notes = self
            .notes
            .iter()
            .map(|n| format!("\"{}\"", n.replace('"', "'")))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"session\":\"{:?}\",\"capture\":\"{:?}\",\"input\":\"{:?}\",\
             \"a11y\":\"{:?}\",\"clipboard\":\"{:?}\",\"audio\":\"{:?}\",\"notes\":[{}]}}",
            self.session,
            self.capture,
            self.input,
            self.a11y,
            self.clipboard,
            (self.audio)(),
            notes
        )
    }
}

/// capabilities runs once and caches the result for the process lifetime.
pub fn capabilities() -> &'static Capabilities {
    static C: OnceLock<Capabilities> = OnceLock::new();
    C.get_or_init(detect)
}

/// audio_path reports which audio backend produced PCM.
fn audio_path() -> AudioPath {
    if !crate::audio::probe_done_pub() {
        return AudioPath::Probing;
    }
    let (ok, _rate, _ch, _frame) = crate::audio::params();
    if ok {
        AudioPath::WasapiLoopback
    } else {
        AudioPath::None
    }
}

fn detect() -> Capabilities {
    Capabilities {
        session: Session::Windows,
        capture: CapturePath::Wgc,
        input: InputPath::SendInput,
        a11y: A11yPath::Uia,
        clipboard: ClipboardPath::Win32,
        audio: audio_path,
        notes: vec![
            "windows: WGC + Media Foundation + SendInput + UIA".into(),
            "audio: WASAPI loopback on the default render endpoint".into(),
        ],
    }
}
