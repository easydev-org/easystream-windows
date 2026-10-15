//! The daemon's a11y surface, exposed over HTTP (A11Y_HTTP.md).
//!
//! Linux uses AT-SPI2 over D-Bus; Windows uses UI Automation. Both produce the
//! SAME text formats the one-shot CLI prints, so the HTTP layer is identical.

/// The platform a11y backend.
pub enum Backend {
    #[cfg(windows)]
    Windows(crate::win::Uia),
    #[cfg(not(windows))]
    Atspi(crate::atspi::Atspi),
}

impl Backend {
    pub fn open() -> Result<Backend, String> {
        #[cfg(windows)]
        {
            Ok(Backend::Windows(crate::win::Uia::new()?))
        }
        #[cfg(not(windows))]
        {
            Ok(Backend::Atspi(crate::atspi::Atspi::open()?))
        }
    }

    /// apps returns the tab-separated `pid\tname\tfocused` listing.
    pub fn apps(&self) -> Result<String, String> {
        match self {
            #[cfg(windows)]
            Backend::Windows(u) => {
                let focused = u.focused_pid();
                let mut out = String::new();
                for (pid, name) in u.apps()? {
                    let f = if pid == focused { "focused" } else { "" };
                    out.push_str(&format!("{pid}\t{name}\t{f}\n"));
                }
                Ok(out)
            }
            #[cfg(not(windows))]
            Backend::Atspi(a) => a.apps(),
        }
    }

    pub fn tree(&self, pid: u32, depth: u32) -> Result<String, String> {
        match self {
            #[cfg(windows)]
            Backend::Windows(u) => u.tree(pid, depth),
            #[cfg(not(windows))]
            Backend::Atspi(a) => a.tree(pid, depth),
        }
    }

    pub fn find(&self, pid: u32, sel: &str, out: u8) -> Result<String, String> {
        match self {
            #[cfg(windows)]
            Backend::Windows(u) => u.find(pid, sel, out),
            #[cfg(not(windows))]
            Backend::Atspi(a) => a.find(pid, sel, out),
        }
    }

    pub fn action(&self, pid: u32, sel: &str, action: &str, value: Option<&str>) -> Result<String, String> {
        match self {
            #[cfg(windows)]
            Backend::Windows(u) => u.action(pid, sel, action, value),
            #[cfg(not(windows))]
            Backend::Atspi(a) => a.action(pid, sel, action, value),
        }
    }
}
