//! The daemon's a11y surface, exposed over HTTP (A11Y_HTTP.md).
//!
//! UI Automation. It produces the SAME text formats the one-shot CLI prints,
//! so the HTTP layer is identical to the other platforms'.

/// The platform a11y backend.
pub enum Backend {
    Windows(crate::win::Uia),
}

impl Backend {
    pub fn open() -> Result<Backend, String> {
        {
            Ok(Backend::Windows(crate::win::Uia::new()?))
        }
    }

    /// apps returns the tab-separated `pid\tname\tfocused` listing.
    pub fn apps(&self) -> Result<String, String> {
        match self {
            Backend::Windows(u) => {
                let focused = u.focused_pid();
                let mut out = String::new();
                for (pid, name) in u.apps()? {
                    let f = if pid == focused { "focused" } else { "" };
                    out.push_str(&format!("{pid}\t{name}\t{f}\n"));
                }
                Ok(out)
            }
        }
    }

    pub fn tree(&self, pid: u32, depth: u32) -> Result<String, String> {
        match self {
            Backend::Windows(u) => u.tree(pid, depth),
        }
    }

    pub fn find(&self, pid: u32, sel: &str, out: u8) -> Result<String, String> {
        match self {
            Backend::Windows(u) => u.find(pid, sel, out),
        }
    }

    pub fn action(&self, pid: u32, sel: &str, action: &str, value: Option<&str>) -> Result<String, String> {
        match self {
            Backend::Windows(u) => u.action(pid, sel, action, value),
        }
    }
}
