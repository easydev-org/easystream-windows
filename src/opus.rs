//! Opus (audio) encoder via the tiny C shim (src/opusshim.cpp). Accepts s16le
//! interleaved PCM and emits one Opus packet per 20 ms frame.
use std::ffi::c_void;

extern "C" {
    fn na_opus_open(rate: std::os::raw::c_int, channels: std::os::raw::c_int, bitrate: std::os::raw::c_int) -> *mut c_void;
    fn na_opus_frame_samples(h: *mut c_void) -> std::os::raw::c_int;
    fn na_opus_encode(h: *mut c_void, pcm: *const i16, out: *mut u8, out_cap: std::os::raw::c_int) -> std::os::raw::c_int;
    fn na_opus_close(h: *mut c_void);
}

pub struct Opus {
    enc: *mut c_void,
    pub channels: usize,
    pub frame_samples: usize,
}

unsafe impl Send for Opus {}

impl Opus {
    /// rate is 48000 by convention; channels 1 or 2.
    pub fn new(rate: u32, channels: u32, bitrate: u32) -> Result<Opus, String> {
        unsafe {
            let enc = na_opus_open(rate as i32, channels as i32, bitrate as i32);
            if enc.is_null() {
                return Err("opus encoder init failed".into());
            }
            let fs = na_opus_frame_samples(enc) as usize;
            Ok(Opus { enc, channels: channels as usize, frame_samples: fs })
        }
    }
    /// encode one 20 ms frame; returns the Opus packet (empty on error).
    pub fn encode(&mut self, pcm: &[i16]) -> Vec<u8> {
        let mut out = vec![0u8; 4000];
        let n = unsafe {
            na_opus_encode(self.enc, pcm.as_ptr(), out.as_mut_ptr(), out.len() as i32)
        };
        if n <= 0 {
            return Vec::new();
        }
        out.truncate(n as usize);
        out
    }
}

impl Drop for Opus {
    fn drop(&mut self) {
        unsafe { na_opus_close(self.enc) }
    }
}
