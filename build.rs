// The Windows backend is pure Rust (the `windows` crate + Media Foundation);
// there are no C/C++ shims to compile, so this build script is intentionally
// empty. (The Linux daemon in the `easystream` repo needs `cc` for its
// x264/xcb/wayland shims.)
fn main() {}
