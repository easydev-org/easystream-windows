#!/usr/bin/env bash
# Cross-compile the Windows est daemon (easystream.exe) from Linux.
#
# The Windows backend uses the `windows` crate (UIA / WGC / Media Foundation /
# SendInput via COMPILE-TIME-generated bindings), so no Rust or Visual Studio is
# needed inside the Windows guest — only mingw-w64 is needed on the build host.
#
#   sudo apt-get install -y gcc-mingw-w64-x86-64
#   rustup target add x86_64-pc-windows-gnu
#   ./build-windows.sh
#
# The produced binary is a single ~0.5 MB native PE that imports only system
# DLLs (kernel32/ole32/d3d11/mfplat/...), so it drops into the golden disk with
# no runtime to install.
#
# Crates come from the in-cluster artifact cargo registry (see .cargo/config.toml),
# which mirrors crates.io. No vendoring is needed.
set -Eeuo pipefail

cd "$(dirname "$0")"

if ! rustup target list --installed | grep -q x86_64-pc-windows-gnu; then
  rustup target add x86_64-pc-windows-gnu
fi

cargo build --release --target x86_64-pc-windows-gnu
echo "built: target/x86_64-pc-windows-gnu/release/easystream.exe"
