//! Client identity and the `Authorization` header values.

use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use mpvfin_core::Secret;

/// Client name reported to Jellyfin.
pub const CLIENT: &str = "mpvfin";

/// Percent-encodes everything except ASCII alphanumerics and `-._~`.
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

/// The machine name: `COMPUTERNAME`, else `HOSTNAME`, else `mpvfin`.
#[must_use]
pub fn device_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| CLIENT.to_owned())
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
pub fn auth_header(device_name: &str, device_id: &str, token: Option<&Secret>) -> Secret {
    let mut v = format!(
        "MediaBrowser Client=\"{CLIENT}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
        pct(device_name),
        pct(device_id),
        env!("CARGO_PKG_VERSION"),
    );
    if let Some(t) = token {
        let _ = write!(v, ", Token=\"{}\"", pct(t.expose()));
    }
    Secret::new(v)
}

/// The full header line sent to media endpoints.
#[must_use]
pub fn media_auth_header(token: &Secret) -> Secret {
    Secret::new(format!(
        "Authorization: MediaBrowser Token=\"{}\"",
        pct(token.expose())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_header_percent_encodes_values() {
        let h = auth_header("Living \"Room\", PC", "id", None);
        assert!(h.expose().contains("Device=\"Living%20%22Room%22%2C%20PC\""));
        assert!(!h.expose().contains("Token"));
        let t = Secret::new("tok".into());
        let h = auth_header("n", "id", Some(&t));
        assert!(h.expose().ends_with(", Token=\"tok\""));
        assert!(h.expose().starts_with("MediaBrowser Client=\"mpvfin\", Device=\"n\""));
    }

    #[test]
    fn media_auth_header_has_token_only() {
        let h = media_auth_header(&Secret::new("tok".into()));
        assert_eq!(h.expose(), "Authorization: MediaBrowser Token=\"tok\"");
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
}
