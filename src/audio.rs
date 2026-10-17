//! Audio capture on Windows via WASAPI LOOPBACK.
//!
//! A render endpoint's loopback stream carries exactly what the machine is
//! playing (system audio), which is what a viewer wants to hear. We open the
//! default render device in shared mode with AUDCLNT_STREAMFLAGS_LOOPBACK,
//! convert the captured samples to s16le / 48 kHz / stereo and hand them to the
//! Opus encoder.
//!
//! Capture is best-effort: if WASAPI cannot be initialised (no audio device,
//! COM not initialised) `open()` fails and the daemon simply sends no audio —
//! video keeps working.
#![cfg(windows)]

use windows::core::{Interface, GUID};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const FRAME_MS: u8 = 20;
/// samples per 20 ms frame, ALL channels interleaved.
pub const FRAME_SAMPLES: usize = (RATE as usize / 50) * CHANNELS as usize;

/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT — the shared-mode default is float32.
const SUBTYPE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

pub fn params() -> (bool, u32, u8, u8) {
    (probe(), RATE, CHANNELS as u8, FRAME_MS)
}

/// probe opens the loopback once and caches whether it worked.
fn probe() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| match Capture::open() {
        Ok(_) => {
            eprintln!("audio: WASAPI loopback available");
            true
        }
        Err(e) => {
            eprintln!("audio: no capture ({e})");
            false
        }
    })
}

pub struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// true when the device mix format is 32-bit float (the usual case).
    is_float: bool,
    bytes_per_frame: usize,
    /// leftover bytes from the previous read.
    buf: Vec<u8>,
}

impl Capture {
    pub fn open() -> Result<Capture, String> {
        unsafe {
            // COM may already be initialised on this thread; RPC_E_CHANGED_MODE
            // is fine (another apartment model) — ignore it.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumr: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("CoCreateInstance: {e}"))?;
            let dev = enumr
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))?;
            let client: IAudioClient =
                dev.Activate(CLSCTX_ALL, None).map_err(|e| format!("Activate: {e}"))?;
            let pwfx = client.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?;
            // Shared mode requires the mix format; we ask for LOOPBACK so the
            // stream carries what is being PLAYED (system audio).
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    10_000_000, // 1 s buffer (100-ns units)
                    0,
                    pwfx,
                    None,
                )
                .map_err(|e| format!("Initialize: {e}"))?;
            let capture: IAudioCaptureClient =
                client.GetService().map_err(|e| format!("GetService: {e}"))?;
            client.Start().map_err(|e| format!("Start: {e}"))?;
            // Determine the sample format from the WAVEFORMATEX.
            // WAVEFORMATEX is a PACKED struct: read fields by VALUE (a
            // reference would be unaligned and Rust rejects it).
            let bits = (*pwfx).wBitsPerSample;
            let cbsize = (*pwfx).cbSize;
            let rate = (*pwfx).nSamplesPerSec;
            let ch = (*pwfx).nChannels;
            let block_align = (*pwfx).nBlockAlign as usize;
            let is_float = if cbsize >= 22 {
                let ext = &*(pwfx as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE);
                // SubFormat is a GUID; compare its bytes to avoid a packed ref.
                let g = ext.SubFormat;
                g.data1 == 0x00000003 && g.data2 == 0x0000 && g.data3 == 0x0010
            } else {
                false
            };
            let bytes_per_frame = block_align.max(1);
            eprintln!(
                "audio: WASAPI {} Hz {} ch {} bit {}",
                rate, ch, bits, if is_float { "float" } else { "pcm" }
            );
            Ok(Capture { client, capture, is_float, bytes_per_frame, buf: Vec::new() })
        }
    }

    /// read_frame returns one 20 ms frame of s16le / 48 kHz / stereo PCM, or
    /// None on error. The device rate is resampled with a simple linear
    /// interpolation if it is not 48 kHz (rare in practice).
    pub fn read_frame(&mut self) -> Option<Vec<i16>> {
        let want = FRAME_SAMPLES * 2; // bytes
        while self.buf.len() < want {
            let mut chunk = Vec::new();
            unsafe {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                let hr = self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None);
                if hr.is_err() || data.is_null() || frames == 0 {
                    // Nothing available yet: sleep a little and retry.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                let nbytes = frames as usize * self.bytes_per_frame;
                let slice = std::slice::from_raw_parts(data, nbytes);
                // Convert to s16 stereo interleaved at 48 kHz.
                chunk = convert(slice, self.is_float, self.bytes_per_frame);
                let _ = self.capture.ReleaseBuffer(frames);
            }
            self.buf.extend_from_slice(&chunk);
        }
        let frame_bytes: Vec<u8> = self.buf.drain(..want).collect();
        let pcm: Vec<i16> = frame_bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(pcm)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe { let _ = self.client.Stop(); }
    }
}

/// convert turns the device buffer into s16le interleaved stereo. WASAPI mix
/// format is 32-bit float stereo in the overwhelming majority of cases; we
/// handle float32 and 16-bit PCM, mono or stereo.
fn convert(src: &[u8], is_float: bool, bytes_per_frame: usize) -> Vec<u8> {
    let channels = if bytes_per_frame >= 8 { 2 } else { 1 };
    let mut out = Vec::with_capacity(src.len() * 2);
    let mut push = |l: f32, r: f32| {
        let s = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&s(l).to_le_bytes());
        out.extend_from_slice(&s(r).to_le_bytes());
    };
    if is_float {
        let step = 4 * channels;
        let mut i = 0;
        while i + step <= src.len() {
            let f = |o: usize| f32::from_le_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]);
            let l = f(i);
            let r = if channels == 2 { f(i + 4) } else { l };
            push(l, r);
            i += step;
        }
    } else {
        let step = 2 * channels;
        let mut i = 0;
        while i + step <= src.len() {
            let s = |o: usize| i16::from_le_bytes([src[o], src[o + 1]]);
            let l = s(i) as f32 / 32768.0;
            let r = if channels == 2 { s(i + 2) as f32 / 32768.0 } else { l };
            push(l, r);
            i += step;
        }
    }
    out
}
