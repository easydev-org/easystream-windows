//! Windows backend for easystream (est:// daemon): UI Automation (a11y tree +
//! actions), Windows.Graphics.Capture (screen), Media Foundation (H.264 encode)
//! and SendInput (mouse/keyboard). Everything goes through the `windows` crate's
//! bindings — those are generated at COMPILE time and call the native COM/Win32
//! vtables directly (no reflection, no runtime, no managed layer).
//!
//! Compiled from Linux with the `x86_64-pc-windows-gnu` target (mingw), so no
//! Rust/VS toolchain is needed inside the Windows guest.

// This module is the Windows-only backend: it declares raw COM interfaces whose
// method names (CreateForWindow, …) are fixed by the ABI, hence the module-wide
// non_snake_case allowance.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::Interface;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::System::Com::{
    CoInitializeEx, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Children};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId,
};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;

/// COM apartment init, once per process.
pub fn init() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    });
}

// ── capture (WGC) ───────────────────────────────────────────────────────────

/// Wgc captures the primary monitor via Windows.Graphics.Capture (the DWM GPU
/// path) and reads the frame back as top-down BGRA. The system cursor is
/// included (IsCursorCaptureEnabled), matching the other platforms.
pub struct Wgc {
    _device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    // Held only to keep the capture session alive (dropping them stops it).
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    width: u32,
    height: u32,
    /// Reused staging texture. Creating a fresh D3D11 staging texture on every
    /// frame leaked GPU resources and RAM; under the guest's tight memory a
    /// large allocation failed outright ("memory allocation of 8294400 bytes
    /// failed"), aborting the daemon.
    staging: std::sync::Mutex<Option<ID3D11Texture2D>>,
    staging_desc: std::sync::Mutex<(u32, u32)>,
}

unsafe impl Send for Wgc {}

impl Wgc {
    pub fn open() -> Result<Wgc, String> {
        init();
        unsafe {
            // 1) D3D11 device (hardware, else WARP for the virtio display).
            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            let drv = windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
            let flags = windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            let sdk = windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION;
            let r = windows::Win32::Graphics::Direct3D11::D3D11CreateDevice(
                None,
                drv,
                None,
                flags,
                None,
                sdk,
                Some(&mut device),
                None,
                Some(&mut ctx),
            );
            if r.is_err() || device.is_none() {
                // WARP fallback (the virtio GPU has no hardware D3D11 device).
                let drv = windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
                let _ = windows::Win32::Graphics::Direct3D11::D3D11CreateDevice(
                    None,
                    drv,
                    None,
                    flags,
                    None,
                    sdk,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                );
            }
            let device = device.ok_or("D3D11CreateDevice failed")?;
            let ctx = ctx.ok_or("D3D11CreateDevice: no context")?;

            // 2) Capture item for the primary monitor.
            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                    .map_err(|e| e.to_string())?;
            let mut item_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            interop
                .CreateForMonitor(
                    MonitorFromPoint(POINT { x: 0, y: 0 }, 1),
                    &GraphicsCaptureItem::IID,
                    &mut item_ptr,
                )
                .map_err(|e| e.to_string())?;
            let item: GraphicsCaptureItem = windows::core::Interface::from_raw(item_ptr);
            let size = item.Size().map_err(|e| e.to_string())?;

            // 3) Wrap the D3D11 device as the WinRT IDirect3DDevice WGC needs.
            let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device.cast().map_err(|e| e.to_string())?;
            let inspectable = windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice(&dxgi).map_err(|e| e.to_string())?;
            let rtdev: IDirect3DDevice = inspectable.cast().map_err(|e| e.to_string())?;

            // 4) Frame pool + session (cursor included).
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &rtdev,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            ).map_err(|e| e.to_string())?;
            let session = pool.CreateCaptureSession(&item).map_err(|e| e.to_string())?;
            let _ = session.SetIsCursorCaptureEnabled(true);
            session.StartCapture().map_err(|e| e.to_string())?;

            Ok(Wgc {
                _device: device,
                ctx,
                _item: item,
                pool,
                _session: session,
                width: size.Width as u32,
                height: size.Height as u32,
                staging: std::sync::Mutex::new(None),
                staging_desc: std::sync::Mutex::new((0, 0)),
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// grab pulls the newest frame as top-down BGRA. WGC only delivers frames
    /// when the screen changes, so None means "no change" (the pump then skips
    /// grab_into copies the newest frame into `out` (reused by the caller) and
    /// returns its size, or None when the screen did not change. This avoids a
    /// fresh ~8 MB allocation on every frame.
    pub fn grab_into(&self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>, String> {
        unsafe {
            // WGC only delivers a frame on StartCapture and then when the screen
            // changes, so the first grab may race the first frame. Poll briefly
            // so a static screen still yields its initial frame.
            let mut frame = match self.pool.TryGetNextFrame() {
                Ok(f) => Some(f),
                Err(_) => None,
            };
            let mut tries = 0;
            while frame.is_none() && tries < 100 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                frame = self.pool.TryGetNextFrame().ok();
                tries += 1;
            }
            let frame = match frame {
                Some(f) => f,
                None => return Ok(None),
            };
            let surface = frame.Surface().map_err(|e| e.to_string())?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| e.to_string())?;
            let mut tex_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            access
                .GetInterface(&ID3D11Texture2D::IID, &mut tex_ptr)
                .map_err(|e| e.to_string())?;
            let tex: ID3D11Texture2D = windows::core::Interface::from_raw(tex_ptr);
            let desc = {
                let mut d = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut d);
                d
            };
            let mut sd = desc;
            sd.Usage = windows::Win32::Graphics::Direct3D11::D3D11_USAGE_STAGING;
            sd.BindFlags = 0;
            sd.CPUAccessFlags = windows::Win32::Graphics::Direct3D11::D3D11_CPU_ACCESS_READ.0 as u32;
            sd.MiscFlags = 0;
            sd.MipLevels = 1;
            sd.ArraySize = 1;
            sd.SampleDesc.Count = 1;
            sd.SampleDesc.Quality = 0;
            // Reuse the staging texture while the size is unchanged; creating a
            // new one every frame leaked GPU resources and RAM.
            let mut desc_guard = self.staging_desc.lock().unwrap();
            let mut staging_guard = self.staging.lock().unwrap();
            if staging_guard.is_none() || *desc_guard != (sd.Width, sd.Height) {
                let mut staging: Option<ID3D11Texture2D> = None;
                self._device
                    .CreateTexture2D(&sd, None, Some(&mut staging))
                    .map_err(|e| e.to_string())?;
                *staging_guard = staging;
                *desc_guard = (sd.Width, sd.Height);
            }
            // Clone the COM pointer (cheap refcount bump) so we hold an owned
            // ID3D11Texture2D while the guard's borrow stays scoped.
            let staging: ID3D11Texture2D = staging_guard.as_ref().ok_or("CreateTexture2D: no texture")?.clone();
            self.ctx.CopyResource(&staging, &tex);
            let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&staging, 0, windows::Win32::Graphics::Direct3D11::D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| e.to_string())?;
            let row = (sd.Width * 4) as usize;
            // Copy straight into the caller's reused buffer.
            if out.len() != row * sd.Height as usize {
                out.resize(row * sd.Height as usize, 0);
            }
            for y in 0..sd.Height as usize {
                let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * row), row);
            }
            let _ = self.ctx.Unmap(&staging, 0);
            Ok(Some((sd.Width, sd.Height)))
        }
    }
}

#[windows::core::interface("3628E81B-3CAC-4C60-B7F4-23CE0E0C3356")]
unsafe trait IGraphicsCaptureItemInterop: windows::core::IUnknown {
    unsafe fn CreateForWindow(&self, window: HWND, riid: *const windows::core::GUID, out: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
    unsafe fn CreateForMonitor(&self, monitor: HMONITOR, riid: *const windows::core::GUID, out: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
}

#[windows::core::interface("A9B3D012-3DF2-4EE3-B8D1-8695F457D3C1")]
unsafe trait IDirect3DDxgiInterfaceAccess: windows::core::IUnknown {
    unsafe fn GetInterface(&self, riid: *const windows::core::GUID, out: *mut *mut core::ffi::c_void) -> windows::core::Result<()>;
}

#[link(name = "user32")]
extern "system" {
    fn MonitorFromPoint(pt: POINT, dwFlags: u32) -> HMONITOR;
}

#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct HMONITOR(pub *mut c_void);
unsafe impl windows::core::Interface for HMONITOR {
    type Vtable = ();
    const IID: windows::core::GUID = windows::core::GUID::zeroed();
}

// ── a11y (UIA) ──────────────────────────────────────────────────────────────

/// Uia drives UI Automation: list apps, walk the tree, find/act on elements.
pub struct Uia {
    auto: IUIAutomation,
}

unsafe impl Send for Uia {}
unsafe impl Sync for Uia {}

impl Uia {
    pub fn new() -> Result<Uia, String> {
        init();
        unsafe {
            let auto: IUIAutomation = windows::Win32::System::Com::CoCreateInstance(
                &CUIAutomation,
                None,
                windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
            )
            .map_err(|e| e.to_string())?;
            Ok(Uia { auto })
        }
    }

    pub fn apps(&self) -> Result<Vec<(u32, String)>, String> {
        unsafe {
            let root = self.auto.GetRootElement().map_err(|e| e.to_string())?;
            let cond = self.auto.CreateTrueCondition().map_err(|e| e.to_string())?;
            let children = root.FindAll(TreeScope_Children, &cond).map_err(|e| e.to_string())?;
            let n = children.Length().map_err(|e| e.to_string())?;
            let mut seen = std::collections::BTreeMap::new();
            for i in 0..n {
                if let Ok(el) = children.GetElement(i) {
                    if let Ok(pid) = el.CurrentProcessId() {
                        let pid = pid as u32;
                        if pid > 0 {
                            seen.entry(pid).or_insert_with(|| format!("pid{pid}"));
                        }
                    }
                }
            }
            Ok(seen.into_iter().collect())
        }
    }

    pub fn focused_pid(&self) -> u32 {
        unsafe {
            let hwnd = GetForegroundWindow();
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            pid
        }
    }
}

// ── input (SendInput) ───────────────────────────────────────────────────────

pub struct WinInput;

impl WinInput {
    pub fn open() -> Result<WinInput, String> {
        init();
        Ok(WinInput)
    }
    pub fn move_to(&self, x: i16, y: i16) {
        win_input::mouse_move(x as i32, y as i32);
    }
    pub fn button(&self, num: u8, down: bool) {
        win_input::mouse_button(num, down);
    }
    pub fn scroll(&self, dx: i32, dy: i32) {
        win_input::mouse_scroll(dx, dy);
    }
    pub fn key(&self, vk: u16, down: bool) {
        win_input::key(vk, down);
    }
}

mod win_input {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
    };

    fn send(inp: INPUT) {
        unsafe { SendInput(&[inp], std::mem::size_of::<INPUT>() as i32); }
    }

    pub fn mouse_move(x: i32, y: i32) {
        // SendInput coordinates for MOUSEEVENTF_ABSOLUTE are normalised to
        // 0..65535 across the virtual screen. Without ABSOLUTE the dx/dy are
        // RELATIVE deltas, so a click at (x,y) would barely move the pointer.
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            let nx = ((x - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32;
            let ny = ((y - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32;
            send(INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: nx,
                        dy: ny,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }

    #[allow(dead_code)]
    pub fn mouse_move_relative(x: i32, y: i32) {
        send(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx: x, dy: y, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 },
            },
        });
    }

    pub fn mouse_button(num: u8, down: bool) {
        let f = match (num, down) {
            (1, true) => MOUSEEVENTF_LEFTDOWN,
            (1, false) => MOUSEEVENTF_LEFTUP,
            (2, true) => MOUSEEVENTF_MIDDLEDOWN,
            (2, false) => MOUSEEVENTF_MIDDLEUP,
            (3, true) => MOUSEEVENTF_RIGHTDOWN,
            (3, false) => MOUSEEVENTF_RIGHTUP,
            _ => return,
        };
        send(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: f, time: 0, dwExtraInfo: 0 },
            },
        });
    }

    pub fn mouse_scroll(dx: i32, dy: i32) {
        if dy != 0 {
            send(INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: (dy * 120) as u32, dwFlags: MOUSEEVENTF_WHEEL, time: 0, dwExtraInfo: 0 },
                },
            });
        }
        if dx != 0 {
            send(INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: (dx * 120) as u32, dwFlags: MOUSEEVENTF_HWHEEL, time: 0, dwExtraInfo: 0 },
                },
            });
        }
    }

    pub fn key(vk: u16, down: bool) {
        let flags = if down { Default::default() } else { KEYEVENTF_KEYUP };
        send(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        });
    }
}

// ── key map (name/char -> VK) ───────────────────────────────────────────────

/// key_by_name maps an EST.md key name to a Windows virtual-key code.
pub fn key_by_name(name: &str) -> Option<u16> {
    // Normalise: the frontend sends canonical names ("Backspace", "PageUp",
    // "Shift", "Meta", "Control"), but older callers used X11-style variants
    // ("BackSpace", "Page_Up", "Shift_L"). Lowercase and drop '_'/spaces so
    // every spelling resolves — a mismatched name silently dropped the key
    // (that is why Backspace did nothing).
    let n = name.to_lowercase().replace(['_', ' '], "");
    Some(match n.as_str() {
        "return" | "enter" => 0x0D,
        "escape" | "esc" => 0x1B,
        "backspace" | "back" => 0x08,
        "tab" => 0x09,
        "space" => 0x20,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "f1" => 0x70, "f2" => 0x71, "f3" => 0x72, "f4" => 0x73, "f5" => 0x74,
        "f6" => 0x75, "f7" => 0x76, "f8" => 0x77, "f9" => 0x78, "f10" => 0x79,
        "f11" => 0x7A, "f12" => 0x7B,
        "shift" => 0x10,
        "control" | "ctrl" => 0x11,
        "alt" => 0x12,
        "meta" | "super" | "win" => 0x5B,
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap();
            return char_keycode(c).map(|(k, _)| k);
        }
        _ => return None,
    })
}

/// char_keycode maps a character to (VK, needs_shift). Covers ASCII; the
/// daemon mostly types into text fields.
pub fn char_keycode(ch: char) -> Option<(u16, bool)> {
    let vk = match ch {
        'a'..='z' => (ch as u16) - ('a' as u16) + 0x41,
        'A'..='Z' => (ch as u16) - ('A' as u16) + 0x41,
        '0'..='9' => (ch as u16) - ('0' as u16) + 0x30,
        ' ' => 0x20,
        '\n' => 0x0D,
        '\t' => 0x09,
        '-' => 0xBD, '=' => 0xBB, '[' => 0xDB, ']' => 0xDD, '\\' => 0xDC,
        ';' => 0xBA, '\'' => 0xDE, ',' => 0xBC, '.' => 0xBE, '/' => 0xBF, '`' => 0xC0,
        _ => return None,
    };
    let shift = ch.is_ascii_uppercase() || matches!(ch, ':' | '<' | '>' | '?' | '{' | '}' | '|' | '~' | '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')');
    Some((vk, shift))
}

// ── clipboard (Win32) ───────────────────────────────────────────────────────

pub mod clipboard {
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    pub fn get() -> String {
        unsafe {
            if OpenClipboard(HWND(std::ptr::null_mut())).is_err() {
                return String::new();
            }
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32);
            let s = match h {
                Ok(h) if !h.is_invalid() => {
                    let p = GlobalLock(HGLOBAL(h.0));
                    if p.is_null() {
                        String::new()
                    } else {
                        let mut len = 0usize;
                        let w = p as *const u16;
                        while *w.add(len) != 0 {
                            len += 1;
                        }
                        let s = String::from_utf16_lossy(std::slice::from_raw_parts(w, len));
                        let _ = GlobalUnlock(HGLOBAL(h.0));
                        s
                    }
                }
                _ => String::new(),
            };
            let _ = CloseClipboard();
            s
        }
    }

    pub fn set(text: &str) {
        unsafe {
            if OpenClipboard(HWND(std::ptr::null_mut())).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = wide.len() * 2;
            if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
                let p = GlobalLock(hmem);
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, p as *mut u8, bytes);
                    let _ = GlobalUnlock(hmem);
                    let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0));
                }
            }
            let _ = CloseClipboard();
        }
    }
}
