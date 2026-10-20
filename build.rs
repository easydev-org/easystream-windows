// The Windows backend is pure Rust (the `windows` crate + Media Foundation)
// EXCEPT the Opus encoder, which is a tiny C++ shim over libopus.
//
// This used to assume the mingw cross-build layout, with the library
// hand-compiled into /usr/local:
//
//   .include("/usr/local/include")
//   cargo:rustc-link-search=native=/usr/local/lib
//   cargo:rustc-link-lib=static=opus
//   cargo:rustc-link-lib=stdc++
//
// On a real Windows runner none of that exists, and the build died at the first
// file it tried to compile:
//
//   src/opusshim.cpp(6): fatal error C1083: Cannot open include file:
//   'opus/opus.h': No such file or directory
//
// So the layout is now chosen from the target environment instead of assumed:
//
//   * MSVC (x86_64-pc-windows-msvc, native or arm64): vcpkg supplies opus.
//     AC_INCLUDE / AC_LIBRARY_PATH come from `vcpkg install` in the workflow
//     (VCPKG_INSTALLATION_ROOT + the triplet), and the C++ runtime is MSVC's, not
//     libstdc++.
//   * mingw (…-windows-gnu): the historical /usr/local layout still applies, so
//     existing container builds keep working.

use std::env;
use std::path::Path;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    println!("cargo:rerun-if-changed=src/opusshim.cpp");

    let mut build = cc::Build::new();
    build.file("src/opusshim.cpp").cpp(true);

    if target_env == "gnu" {
        // Cross-built libopus, installed by hand (see the Containerfile).
        build.include("/usr/local/include");
        build.compile("na_opus");
        println!("cargo:rustc-link-search=native=/usr/local/lib");
        println!("cargo:rustc-link-lib=static=opus");
        println!("cargo:rustc-link-lib=stdc++");
        return;
    }

    // MSVC. Prefer an explicit include dir when the caller supplies one, then
    // fall back to the vcpkg layout that the workflow sets up.
    let mut include = None;
    let mut libdir = None;

    if let Ok(p) = env::var("OPUS_INCLUDE_DIR") {
        include = Some(p);
    }
    if let Ok(p) = env::var("OPUS_LIB_DIR") {
        libdir = Some(p);
    }

    if include.is_none() || libdir.is_none() {
        if let (Ok(root), Ok(triplet)) =
            (env::var("VCPKG_INSTALLATION_ROOT"), env::var("VCPKGRS_TRIPLET"))
        {
            let base = Path::new(&root).join("installed").join(&triplet);
            if include.is_none() {
                let inc = base.join("include");
                if inc.is_dir() {
                    include = Some(inc.to_string_lossy().into_owned());
                }
            }
            if libdir.is_none() {
                let lib = base.join("lib");
                if lib.is_dir() {
                    libdir = Some(lib.to_string_lossy().into_owned());
                }
            }
        }
    }

    match include {
        Some(inc) => {
            build.include(&inc);
        }
        None => {
            // Fail with an actionable message rather than letting cl.exe emit
            // "cannot open opus/opus.h" with no hint about what to install.
            panic!(
                "opus headers not found. Set OPUS_INCLUDE_DIR, or provide \
                 VCPKG_INSTALLATION_ROOT + VCPKGRS_TRIPLET (e.g. \
                 `vcpkg install opus:x64-windows-static-md`)."
            );
        }
    }
    build.compile("na_opus");

    if let Some(lib) = libdir {
        println!("cargo:rustc-link-search=native={lib}");
    }
    println!("cargo:rustc-link-lib=static=opus");
    // MSVC links the C++ runtime itself; `stdc++` does not exist there.
}
