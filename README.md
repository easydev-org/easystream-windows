# easystream-windows

The **Windows** easystream daemon: one native PE (`easystream.exe`) that owns
screen capture (Windows.Graphics.Capture), H.264 encode (Media Foundation),
input injection (SendInput) and UI Automation, and exposes:

* **est://** on `127.0.0.1:48079` — the multiplexed video/input/clipboard/a11y
  stream the `agent-worker` forwards (see `EST.md` in `easydev-org/easystream`).
* **HTTP** on `0.0.0.0:48078` — the web UI + the `/a11y/*` API (see `A11Y_HTTP.md`).

It is **pure Rust** (the `windows` crate: UIA / WGC / Media Foundation /
SendInput), cross-compiled from Linux with the `x86_64-pc-windows-gnu` target —
no Rust or Visual Studio is needed inside the guest.

## Build

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt-get install -y gcc-mingw-w64-x86-64
./rust/build-windows.sh        # -> rust/target/x86_64-pc-windows-gnu/release/easystream.exe
```

Crates come from the in-cluster artifact cargo registry (`rust/.cargo/config.toml`).

## Audio (system audio capture)

`Hello.hasAudio` is 1 when audio capture is available, and `0x05` Opus frames
carry 20 ms packets to viewers that send `0x06 AudioReq` (see `EST.md`).

Capture uses **WASAPI loopback** (`IMMDeviceEnumerator` →
`AUDCLNT_STREAMFLAGS_LOOPBACK`), which records whatever the default render
endpoint is playing — no virtual device and no extra driver are required, unlike
macOS. Encoding uses **libopus**, cross-compiled for `x86_64-w64-mingw32` and
linked statically via a small C shim (`src/opusshim.cpp`, built by `build.rs`).

`build-windows.sh` therefore needs the mingw libopus toolchain once:

```sh
./opus/build-opus-mingw.sh      # fetches + cross-builds libopus for mingw
```

If the guest has no audio endpoint at all (headless VMs often do not),
`GetDefaultAudioEndpoint` fails, the daemon advertises `hasAudio = 0` and no
`0x05` frames are sent; everything else keeps working.
