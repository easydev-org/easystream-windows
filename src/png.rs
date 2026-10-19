//! A minimal PNG encoder (no dependencies).
//!
//! The a11y `/a11y/screenshot` endpoint returns image/png. We only need to
//! encode a single RGBA/BGRA frame, so this writes an 8-bit RGBA PNG whose
//! IDAT is a zlib stream of *stored* (uncompressed) deflate blocks — legal
//! zlib that any decoder accepts, and it avoids pulling in a compression
//! crate. A screenshot is a rare, one-shot call, so the size cost is fine.

/// crc32 (IEEE) over `data`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc_in = Vec::with_capacity(4 + data.len());
    crc_in.extend_from_slice(kind);
    crc_in.extend_from_slice(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
}

/// adler32 (zlib) over `data`.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b): (u32, u32) = (1, 0);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// zlib-wrap `raw` as a sequence of stored deflate blocks.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x01]; // CMF/FLG: deflate, 32K window, no dict
    let mut i = 0usize;
    if raw.is_empty() {
        z.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while i < raw.len() {
        let n = std::cmp::min(65535, raw.len() - i);
        let last = i + n == raw.len();
        z.push(if last { 1 } else { 0 }); // BFINAL + BTYPE=00 (stored)
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes());
        z.extend_from_slice(&raw[i..i + n]);
        i += n;
    }
    z.extend_from_slice(&adler32(raw).to_be_bytes());
    z
}

/// encode_rgba builds a PNG from `w*h*4` bytes in RGBA order.
pub fn encode_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0); // filter type 0 (None)
        let off = y * w as usize * 4;
        raw.extend_from_slice(&rgba[off..off + w as usize * 4]);
    }
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no filter, no interlace
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    png
}

/// encode_bgra builds a PNG from `w*h*4` bytes in BGRA order (the capture
/// backends' native layout).
pub fn encode_bgra(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
    let mut rgba = bgra.to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2); // B<->R
        px[3] = 255;   // force opaque: the captured alpha is undefined
    }
    encode_rgba(w, h, &rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_header_and_size() {
        let png = encode_rgba(2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR width/height are at bytes 16..24.
        assert_eq!(&png[16..20], &2u32.to_be_bytes());
        assert_eq!(&png[20..24], &1u32.to_be_bytes());
    }
}
