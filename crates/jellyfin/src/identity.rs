//! Client identity and the `Authorization` header value.

use std::fmt;
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// A string that must never appear in logs; its `Debug` output is redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a sensitive value.
    #[must_use]
    pub const fn new(s: String) -> Self {
        Self(s)
    }

    /// Returns the sensitive value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// The application talking to Jellyfin, as the server and other clients see it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct App {
    /// Client name.
    pub name: &'static str,
    /// Client version.
    pub version: &'static str,
}

/// Percent-encodes everything except ASCII alphanumerics and `-._~`.
#[must_use]
pub(crate) fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The machine name: `COMPUTERNAME`, else `HOSTNAME`, else `fallback`.
#[must_use]
pub fn device_name(fallback: &str) -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// A fresh 32-character lowercase hex device id.
#[must_use]
pub fn new_device_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let state = std::collections::hash_map::RandomState::new();
    let mut id = String::with_capacity(32);
    for _ in 0..2 {
        let mut h = state.build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        let _ = write!(id, "{:016x}", h.finish());
    }
    id
}

/// The `Authorization` header value, with the token when given.
#[must_use]
pub fn auth_header(app: App, device_name: &str, device_id: &str, token: Option<&Secret>) -> Secret {
    let mut v = format!(
        "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
        pct(app.name),
        pct(device_name),
        pct(device_id),
        pct(app.version),
    );
    if let Some(t) = token {
        let _ = write!(v, ", Token=\"{}\"", pct(t.expose()));
    }
    Secret::new(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: App = App {
        name: "mpvfin",
        version: "1.2.3",
    };

    #[test]
    fn auth_header_percent_encodes_values() {
        let h = auth_header(APP, "Living \"Room\", PC", "id", None);
        assert!(
            h.expose()
                .contains("Device=\"Living%20%22Room%22%2C%20PC\"")
        );
        assert!(!h.expose().contains("Token"));
        let t = Secret::new("tok".into());
        let h = auth_header(APP, "n", "id", Some(&t));
        assert!(h.expose().ends_with(", Token=\"tok\""));
        assert!(
            h.expose()
                .starts_with("MediaBrowser Client=\"mpvfin\", Device=\"n\"")
        );
        assert!(h.expose().contains("Version=\"1.2.3\""));
    }

    #[test]
    fn new_device_id_is_32_hex_and_unique() {
        let (a, b) = (new_device_id(), new_device_id());
        assert_ne!(a, b);
        for id in [a, b] {
            assert_eq!(id.len(), 32);
            assert!(id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        }
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = format!("{:?}", Secret::new("tok".into()));
        assert!(!s.contains("tok"));
    }
}
