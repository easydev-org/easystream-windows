//! Media Foundation H.264 encoder (Windows hardware encoder, the counterpart of
//! macOS VideoToolbox). It accepts BGRA frames and emits Annex-B
//! access units, so the est pump treats it exactly like the other platforms.
//!
//! Uses the raw Media Foundation COM interfaces via the `windows` crate — no
//! runtime, no managed layer; the daemon stays a single small native binary.

use std::sync::OnceLock;
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub struct MfEncoder {
    transform: IMFTransform,
    /// The MS H.264 encoder is an ASYNC MFT: it reports "need input" / "have
    /// output" through its event generator, and ProcessOutput only returns data
    /// after a HaveOutput event. Driving it purely synchronously leaves it stuck
    /// at MF_E_TRANSFORM_NEED_MORE_INPUT forever.
    events: Option<IMFMediaEventGenerator>,
    need_input: bool,
    /// A REUSED NV12 staging buffer (allocated once, never per frame — a fresh
    /// ~3 MB Vec every frame churned the guest's tight memory until an alloc
    /// failed and aborted the daemon).
    nv12: Vec<u8>,
    /// True while `nv12` holds a frame the MFT has not consumed yet.
    pending_in_nv12: bool,
    width: u32,
    height: u32,
    fps: u32,
    /// Monotonic frame index; the sample time is index * frame duration. The
    /// encoder needs a CONTINUOUS timeline — a wall-clock timestamp jumps
    /// around and makes it think frames were lost (so it never emits).
    frame: i64,
    out_info: MFT_OUTPUT_STREAM_INFO,
    /// SPS/PPS (Annex-B), captured once and prepended to every IDR so late
    /// joiners can decode from any keyframe.
    csd: Option<Vec<u8>>,
}

unsafe impl Send for MfEncoder {}

fn mf_init() -> Result<(), String> {
    static ONCE: OnceLock<Result<(), String>> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))
    })
    .clone()
}

impl MfEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<MfEncoder, String> {
        mf_init()?;
        unsafe {
            let transform = find_encoder()?;

            // The Microsoft H.264 encoder is an ASYNC MFT: without unlocking it,
            // ProcessOutput only ever returns access-unit delimiters (no slices).
            // The C# daemon did the same (MF_TRANSFORM_ASYNC_UNLOCK = TRUE).
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }

            // Start from the MFT's OWN media types (they carry the attributes the
            // encoder requires) and only override size/rate/bitrate/subtype.
            // Building a type from scratch leaves those attributes out and the
            // encoder silently rejects every input frame (ProcessOutput keeps
            // returning NEED_MORE_INPUT).
            let out_type = transform
                .GetOutputAvailableType(0, 0)
                .unwrap_or(make_video_type(MFVideoFormat_H264, width, height, fps, bitrate)?);
            set_size_rate_bitrate(&out_type, width, height, fps, bitrate)?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| format!("SetOutputType: {e}"))?;
            // The MS encoder wants NV12; BGRA is converted in encode(). Force the
            // subtype because the MFT may offer I420/YV12 first, and feeding NV12
            // as I420 interleaves the chroma wrongly.
            let in_type = transform
                .GetInputAvailableType(0, 0)
                .unwrap_or(make_video_type(MFVideoFormat_NV12, width, height, fps, 0)?);
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| e.to_string())?;
            set_size_rate_bitrate(&in_type, width, height, fps, 0)?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| format!("SetInputType: {e}"))?;

            let out_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("GetOutputStreamInfo: {e}"))?;

            let events: Option<IMFMediaEventGenerator> = transform.cast().ok();

            // Make the encoder emit every frame as an IDR and run low-latency:
            // otherwise it buffers a whole GOP (~fps frames) before its first
            // output, so a short-lived session sees no video at all.
            if let Ok(codec) = transform.cast::<ICodecAPI>() {
                let v: windows::core::VARIANT = true.into();
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &v);
                let g: windows::core::VARIANT = 1u32.into();
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &g);
            }

            // The encoder does not process any input until it is told the stream
            // has started. Without these messages ProcessOutput only ever returns
            // MF_E_TRANSFORM_NEED_MORE_INPUT.
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| format!("BeginStreaming: {e}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| format!("StartOfStream: {e}"))?;
            Ok(MfEncoder { transform, events, need_input: false, nv12: Vec::new(), pending_in_nv12: false, width, height, fps, frame: 0, out_info, csd: None })
        }
    }

    pub fn force_idr(&mut self) {
        // Re-setting the output type (same params) forces the next frame to IDR.
        unsafe {
            if let Ok(t) = make_video_type(MFVideoFormat_H264, self.width, self.height, self.fps, 12_000_000) {
                let _ = self.transform.SetOutputType(0, &t, 0);
            }
        }
    }

    /// encode one BGRA frame, returning an Annex-B access unit (may be empty
    /// while the encoder buffers).
    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let w = self.width;
        let h = self.height;
        self.encode_sized(bgra, w, h)
    }

    /// encode_sized encodes a frame whose CAPTURE geometry may differ from the
    /// encoder's. Scaling happens here because the screen source only offers one
    /// size (WGC hands back the desktop's), so the encoder is the first place
    /// that legitimately owns an output geometry.
    pub fn encode_sized(
        &mut self,
        bgra: &[u8],
        src_w: u32,
        src_h: u32,
    ) -> Result<Vec<Vec<u8>>, String> {
        unsafe {
            // Convert into a REUSED buffer: allocating a fresh ~3 MB NV12 Vec
            // every frame (30 fps) churned the guest's tight memory until an
            // allocation failed outright and aborted the whole daemon.
            let need = (self.width as usize) * (self.height as usize) * 3 / 2;
            if self.nv12.len() != need {
                self.nv12.resize(need, 0);
            }
            if src_w == self.width && src_h == self.height {
                bgra_to_nv12_into(bgra, self.width, self.height, &mut self.nv12);
            } else {
                bgra_to_nv12_scaled(bgra, src_w, src_h, self.width, self.height, &mut self.nv12);
            }
            // Queue the frame; the event pump feeds it when the MFT asks.
            self.pending_in_nv12 = true;
            let mut out = Vec::new();
            // Drain events (bounded so one call cannot spin forever).
            for _ in 0..64 {
                let ev = match self.events.as_ref() {
                    Some(g) => match g.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                        Ok(e) => e,
                        Err(_) => break,
                    },
                    None => break,
                };
                let t = ev.GetType().map_err(|e| e.to_string())?;
                if t == METransformNeedInput.0 as u32 {
                    self.need_input = true;
                    self.try_feed()?;
                } else if t == METransformHaveOutput.0 as u32 {
                    self.collect(&mut out)?;
                }
            }
            // If there is no event generator, fall back to a synchronous round.
            if self.events.is_none() {
                // The sync MFT refuses a frame (MF_E_NOTACCEPTING) while its
                // output queue is full, so drain first and retry — feeding the
                // same frame forever without draining wedges the encoder.
                self.need_input = true;
                for _ in 0..4 {
                    match self.try_feed() {
                        Ok(()) => break,
                        Err(_) => {
                            self.collect(&mut out)?;
                            self.need_input = true;
                        }
                    }
                }
                for _ in 0..8 {
                    self.collect(&mut out)?;
                }
            }
            Ok(out)
        }
    }

    /// try_feed submits the queued NV12 frame when the MFT is ready for one.
    fn try_feed(&mut self) -> Result<(), String> {
        if !self.need_input {
            return Ok(());
        }
        if !self.pending_in_nv12 {
            return Ok(());
        }
        let nv12 = std::mem::take(&mut self.nv12);
        unsafe {
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32).map_err(|e| e.to_string())?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None).map_err(|e| format!("Lock: {e}"))?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.Unlock().map_err(|e| format!("Unlock: {e}"))?;
            buffer.SetCurrentLength(nv12.len() as u32).map_err(|e| e.to_string())?;
            let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
            sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
            let dur = 10_000_000 / self.fps.max(1) as i64;
            sample.SetSampleTime(self.frame * dur).map_err(|e| e.to_string())?;
            sample.SetSampleDuration(dur).map_err(|e| e.to_string())?;
            self.frame += 1;
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {
                    self.need_input = false;
                    self.pending_in_nv12 = false;
                    // Hand the (now unused) staging buffer back so the next
                    // frame converts into it instead of allocating.
                    self.nv12 = nv12;
                }
                Err(e) => {
                    // MF_E_NOTACCEPTING: keep it queued for the next round.
                    self.nv12 = nv12;
                    return Err(format!("ProcessInput: {e}"));
                }
            }
        }
        Ok(())
    }

    /// collect pulls ONE output sample (called on each HaveOutput event).
    fn collect(&mut self, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        unsafe {
            {
                // If the MFT can provide its own sample, pass NULL (the C# daemon
                // treats PROVIDES|CAN_PROVIDE the same way); else we must supply
                // one of at least cbSize.
                let mft_provides = (self.out_info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                    != 0;
                let provided = if !mft_provides {
                    let size = if self.out_info.cbSize > 0 { self.out_info.cbSize } else { self.width * self.height * 4 };
                    let buf = MFCreateMemoryBuffer(size).map_err(|e| e.to_string())?;
                    let s = MFCreateSample().map_err(|e| e.to_string())?;
                    s.AddBuffer(&buf).map_err(|e| e.to_string())?;
                    Some(s)
                } else {
                    None
                };
                let mut data = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(provided),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                };
                let mut status = 0u32;
                let r = self.transform.ProcessOutput(0, std::slice::from_mut(&mut data), &mut status);
                // ALWAYS take the output objects out of the ManuallyDrop fields,
                // even when ProcessOutput failed. ManuallyDrop suppresses the
                // drop glue, so an early `return` that skipped this leaked one
                // IMFSample (an ~8 MB buffer at 1920x1080 BGRA) per call — at
                // 30 fps the MFT ran out of memory within seconds and the
                // daemon aborted with "Not enough memory resources". pEvents
                // leaks the same way, so take both before branching on the
                // result.
                let sample = std::mem::ManuallyDrop::take(&mut data.pSample);
                let _events = std::mem::ManuallyDrop::take(&mut data.pEvents);
                if let Err(e) = r {
                    if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                        return Ok(());
                    }
                    if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                        if let Ok(t) = self.transform.GetOutputAvailableType(0, 0) {
                            let _ = self.transform.SetOutputType(0, &t, 0);
                        }
                        return Ok(());
                    }
                    return Ok(());
                }
                if let Some(sample) = sample {
                    if let Ok(buf) = sample.ConvertToContiguousBuffer() {
                        let mut ptr: *mut u8 = std::ptr::null_mut();
                        let mut cur = 0u32;
                        if buf.Lock(&mut ptr, None, Some(&mut cur)).is_ok() {
                            let bytes = std::slice::from_raw_parts(ptr, cur as usize);
                            for au in split_aus(bytes, &mut self.csd) {
                                out.push(au);
                            }
                            let _ = buf.Unlock();
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

/// find_encoder instantiates the Microsoft H.264 Encoder MFT directly (the same
/// CLSID the C# daemon used). Enumeration is unreliable on headless Windows
/// guests (the hardware MFT may not be registered), but CoCreateInstance of
/// CLSID_CMSH264EncoderMFT always works where the encoder is present.
unsafe fn find_encoder() -> Result<IMFTransform, String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    const CLSID_CMSH264ENCODERMFT: GUID = GUID::from_u128(0x6CA50344_051A_4DED_9779_A43305165E35);
    let transform: IMFTransform = CoCreateInstance(&CLSID_CMSH264ENCODERMFT, None, CLSCTX_INPROC_SERVER)
        .map_err(|e| format!("CoCreateInstance(H264 MFT): {e}"))?;
    Ok(transform)
}

/// bgra_to_nv12 converts top-down BGRA to NV12 using the BT.709 matrix (the
/// encoder emits High with no VUI colour info, so the browser's default BT.709
/// must match — otherwise blue and purple swap).
/// bgra_to_nv12_into converts top-down BGRA to NV12 using the BT.709 matrix,
/// writing into a caller-owned buffer of exactly w*h*3/2 bytes so the encoder
/// reuses ONE allocation instead of a fresh ~3 MB Vec per frame (that per-frame
/// churn made an allocation fail and aborted the daemon on the tight guest).
fn bgra_to_nv12_into(bgra: &[u8], w: u32, h: u32, nv12: &mut [u8]) {
    let (w, h) = (w as usize, h as usize);
    debug_assert_eq!(nv12.len(), w * h * 3 / 2);
    let y_size = w * h;
    for j in 0..h {
        for i in 0..w {
            let o = (j * w + i) * 4;
            let (b, g, r) = (bgra[o] as i32, bgra[o + 1] as i32, bgra[o + 2] as i32);
            nv12[j * w + i] = clamp_u8(((54 * r + 183 * g + 18 * b + 128) >> 8) + 16);
        }
    }
    for j in (0..h).step_by(2) {
        for i in (0..w).step_by(2) {
            let o = (j * w + i) * 4;
            let (b, g, r) = (bgra[o] as i32, bgra[o + 1] as i32, bgra[o + 2] as i32);
            let u = ((-29 * r - 99 * g + 128 * b + 128) >> 8) + 128;
            let v = ((128 * r - 116 * g - 12 * b + 128) >> 8) + 128;
            let uv = y_size + (j / 2) * w + i;
            nv12[uv] = clamp_u8(u);
            nv12[uv + 1] = clamp_u8(v);
        }
    }
}

/// bgra_to_nv12_scaled converts a frame whose CAPTURE size differs from the
/// encoder's, scaling with bilinear sampling while converting to NV12.
///
/// Windows has no libyuv here, and the plain converter walks the source at 1:1.
/// Bilinear rather than nearest-neighbour because the presets halve the
/// resolution: at 2:1 a naive pick drops every other line and makes text shimmer.
#[allow(clippy::too_many_arguments)]
fn bgra_to_nv12_scaled(
    bgra: &[u8],
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    nv12: &mut [u8],
) {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    debug_assert_eq!(nv12.len(), dw * dh * 3 / 2);
    let y_size = dw * dh;
    // Precompute the source columns once per row instead of per pixel.
    let mut xmap = vec![(0usize, 0usize, 0i32); dw];
    for i in 0..dw {
        let fx = (i as f32 + 0.5) * sw as f32 / dw as f32 - 0.5;
        let x0 = fx.floor().max(0.0) as usize;
        let x1 = (x0 + 1).min(sw - 1);
        let w = ((fx - x0 as f32) * 256.0) as i32;
        xmap[i] = (x0, x1, w);
    }
    for j in 0..dh {
        let fy = (j as f32 + 0.5) * sh as f32 / dh as f32 - 0.5;
        let y0 = fy.floor().max(0.0) as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = ((fy - y0 as f32) * 256.0) as i32;
        let row0 = y0 * sw;
        let row1 = y1 * sw;
        for i in 0..dw {
            let (x0, x1, wx) = xmap[i];
            let p00 = (row0 + x0) * 4;
            let p01 = (row0 + x1) * 4;
            let p10 = (row1 + x0) * 4;
            let p11 = (row1 + x1) * 4;
            let mut c = [0i32; 3];
            for k in 0..3 {
                let a = bgra[p00 + k] as i32 * (256 - wx) + bgra[p01 + k] as i32 * wx;
                let b = bgra[p10 + k] as i32 * (256 - wx) + bgra[p11 + k] as i32 * wx;
                c[k] = ((a * (256 - wy) + b * wy) >> 16).clamp(0, 255);
            }
            let (b, g, r) = (c[0], c[1], c[2]);
            nv12[j * dw + i] = clamp_u8(((54 * r + 183 * g + 18 * b + 128) >> 8) + 16);
        }
    }
    for j in (0..dh).step_by(2) {
        for i in (0..dw).step_by(2) {
            // Sample the chroma at the source position corresponding to (i,j).
            let sx = ((i as f32 + 0.5) * sw as f32 / dw as f32) as usize;
            let sy = ((j as f32 + 0.5) * sh as f32 / dh as f32) as usize;
            let sx = sx.min(sw - 1);
            let sy = sy.min(sh - 1);
            let o = (sy * sw + sx) * 4;
            let (b, g, r) = (bgra[o] as i32, bgra[o + 1] as i32, bgra[o + 2] as i32);
            let u = ((-29 * r - 99 * g + 128 * b + 128) >> 8) + 128;
            let v = ((128 * r - 116 * g - 12 * b + 128) >> 8) + 128;
            let uv = y_size + (j / 2) * dw + i;
            nv12[uv] = clamp_u8(u);
            nv12[uv + 1] = clamp_u8(v);
        }
    }
}

fn clamp_u8(v: i32) -> u8 {
    if v < 0 { 0 } else if v > 255 { 255 } else { v as u8 }
}

fn make_video_type(subtype: GUID, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<IMFMediaType, String> {
    unsafe {
        let t = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(|e| e.to_string())?;
        set_size_rate_bitrate(&t, w, h, fps, bitrate)?;
        Ok(t)
    }
}

/// set_size_rate_bitrate applies frame size, frame rate, interlace and (when
/// non-zero) the average bitrate — the attributes the encoder needs on BOTH the
/// input and the output type.
fn set_size_rate_bitrate(t: &IMFMediaType, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<(), String> {
    unsafe {
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
        let frame = ((w as u64) << 32) | (h as u64);
        t.SetUINT64(&MF_MT_FRAME_SIZE, frame).map_err(|e| e.to_string())?;
        let rate = ((fps as u64) << 32) | 1u64;
        t.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(|e| e.to_string())?;
        if bitrate > 0 {
            t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// split_aus splits one encoder output buffer (Annex-B, possibly SEVERAL access
/// units back to back) into individual access units. The MS H.264 encoder emits
/// each AU starting with an access-unit delimiter (NAL type 9); the SPS/PPS ride
/// inside the keyframe's AU, so no re-prepending is needed. A decoder that gets
/// a multi-AU blob would only ever see the first delimiter and fail.
fn split_aus(buf: &[u8], _csd: &mut Option<Vec<u8>>) -> Vec<Vec<u8>> {
    // Locate every Annex-B start code and the NAL type that follows it.
    let mut starts: Vec<(usize, u8)> = Vec::new();
    let mut i = 0usize;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            let (sc, off) = if buf[i + 2] == 1 {
                (3, i + 3)
            } else if buf[i + 2] == 0 && i + 3 < buf.len() && buf[i + 3] == 1 {
                (4, i + 4)
            } else {
                i += 1;
                continue;
            };
            if off < buf.len() {
                starts.push((i, buf[off] & 0x1f));
            }
            i += sc;
        } else {
            i += 1;
        }
    }
    if starts.is_empty() {
        return Vec::new();
    }
    let mut aus: Vec<Vec<u8>> = Vec::new();
    let mut au_start = 0usize;
    for (idx, (pos, nal_type)) in starts.iter().enumerate() {
        if idx == 0 {
            continue;
        }
        // A new AU begins at an access-unit delimiter (9).
        if *nal_type == 9 {
            aus.push(buf[au_start..*pos].to_vec());
            au_start = *pos;
        }
    }
    aus.push(buf[au_start..].to_vec());
    aus
}
