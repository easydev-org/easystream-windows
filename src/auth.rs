//! Access control: two tokens, two modes.
//!
//! A easystream daemon has two modes:
//!
//!   * VIEW     — read-only (the DEFAULT): video, a11y reads, screenshots,
//!                clipboard reads. NO token required. Every frame that would
//!                CHANGE the guest (input, a11y action, clipboard set) is
//!                dropped.
//!   * CONTROL  — full access: everything VIEW can do PLUS input
//!                (pointer/keyboard), a11y actions and clipboard writes. This
//!                requires the control token.
//!
//! Tokens come from the environment:
//!
//!   EAZYSTREAM_CONTROL_TOKEN   the token that grants CONTROL
//!   EAZYSTREAM_VIEW_TOKEN      (optional) an explicit read-only token; view is
//!                              open anyway, so this is only for symmetry
//!
//! Behaviour:
//!   * no token at all            -> VIEW (read-only), always allowed
//!   * the control token          -> CONTROL
//!   * anything else              -> VIEW
//!   * NEITHER token configured   -> CONTROL for everyone (auth off; keeps
//!                                   existing deployments working)
//!
//! Tokens are compared in constant time to avoid a timing oracle.

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Read-only: no input, no a11y actions, no clipboard writes.
    View,
    /// Full control.
    Control,
}

/// The configured tokens (read once from the environment).
struct Config {
    control: Option<String>,
    view: Option<String>,
    /// True when no token is configured at all (auth disabled).
    open: bool,
}

static CFG: OnceLock<Config> = OnceLock::new();

fn cfg() -> &'static Config {
    CFG.get_or_init(|| {
        let control = std::env::var("EAZYSTREAM_CONTROL_TOKEN").ok().filter(|s| !s.is_empty());
        let view = std::env::var("EAZYSTREAM_VIEW_TOKEN").ok().filter(|s| !s.is_empty());
        let open = control.is_none() && view.is_none();
        if open {
            eprintln!("auth: no tokens configured — running UNAUTHENTICATED (all clients = control)");
        } else {
            eprintln!(
                "auth: control token {} , view token {}",
                if control.is_some() { "set" } else { "unset" },
                if view.is_some() { "set" } else { "unset" }
            );
        }
        Config { control, view, open }
    })
}

/// True when auth is disabled (no tokens configured).
pub fn is_open() -> bool {
    cfg().open
}

/// constant-time equality (avoids leaking the token length/prefix via timing).
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for i in 0..a.len() {
        d |= a[i] ^ b[i];
    }
    d == 0
}

/// authorize maps a presented token to a Mode. It NEVER fails: view is the
/// open default, so a missing or wrong token simply means read-only.
pub fn authorize(token: Option<&str>) -> Mode {
    let c = cfg();
    if c.open {
        return Mode::Control; // auth disabled entirely
    }
    let t = token.unwrap_or("");
    // Only the CONTROL token upgrades past the read-only default.
    if let Some(ct) = &c.control {
        if ct_eq(t, ct) {
            return Mode::Control;
        }
    }
    Mode::View
}

/// is_mutation reports whether an est frame type CHANGES the guest — i.e. it is
/// forbidden in View mode. Everything else (video, hello, a11y reads,
/// clipboard reads, screenshots, ping) is allowed.
pub fn is_mutation(typ: u8) -> bool {
    matches!(
        typ,
        0x10 | 0x11 | 0x12 | 0x13 | 0x14   // pointer + keyboard
        | 0x26                              // a11y ActionReq
        | 0x32                              // ClipSet
    )
}
