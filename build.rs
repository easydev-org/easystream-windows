// The Windows backend is pure Rust (the `windows` crate + Media Foundation)
// EXCEPT the Opus encoder, which is a tiny C++ shim over libopus (compiled for
// the mingw target; libopus.a is cross-built and installed to /usr/local).
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }
    println!("cargo:rerun-if-changed=src/opusshim.cpp");
    cc::Build::new()
        .file("src/opusshim.cpp")
        .cpp(true)
        .include("/usr/local/include")   // opus headers (cross-built)
        .compile("na_opus");
    println!("cargo:rustc-link-search=native=/usr/local/lib");
    println!("cargo:rustc-link-lib=static=opus");
    println!("cargo:rustc-link-lib=stdc++");
}
