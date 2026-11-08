//! `mpvfin.conf` parsing and the Jellyfin base URL.

use mpvfin_core::Secret;

use crate::identity::pct;

/// User configuration from `script-opts/mpvfin.conf`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// The Jellyfin server.
    pub server: BaseUrl,
}

/// Parses mpv script-opts text: `key=value` lines, `#` comments, unknown keys ignored.
///
/// # Errors
/// Returns a message when `server` is missing or not an http(s) URL.
pub fn parse_config(text: &str) -> Result<Config, String> {
    // Windows editors may save the file with a byte order mark.
    let server = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == "server")
        .map(|(_, v)| v.trim())
        .ok_or("mpvfin.conf: missing `server`")?;
    Ok(Config { server: BaseUrl::parse(server).map_err(|e| format!("server: {e}"))? })
}

/// Scheme, host and optional path prefix, without a trailing slash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaseUrl(String);

impl BaseUrl {
    /// Parses an `http://` or `https://` URL.
    ///
    /// # Errors
    /// Returns a message for other schemes or an empty host.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim().trim_end_matches('/');
        let host = s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("https://"))
            .ok_or_else(|| format!("`{s}` must start with http:// or https://"))?;
        if host.is_empty() || host.starts_with('/') {
            return Err(format!("`{s}` has no host"));
        }
        Ok(Self(s.to_owned()))
    }

    /// Appends `path_and_query` (starting with `/`) to the base.
    #[must_use]
    pub fn join(&self, path_and_query: &str) -> String {
        format!("{}{path_and_query}", self.0)
    }

    /// The websocket URL; it carries the token, hence the `Secret`.
    #[must_use]
    pub fn websocket(&self, token: &Secret, device_id: &str) -> Secret {
        let rest = self.0.strip_prefix("http").unwrap_or(&self.0); // "s://h" or "://h"
        Secret::new(format!(
            "ws{rest}/socket?ApiKey={}&deviceId={}",
            pct(token.expose()),
            pct(device_id)
        ))
    }

    /// The URL text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_from_script_opts() {
        let c = parse_config("# c\n server = https://h/jf/ \nother=1").unwrap();
        assert_eq!(c.server.as_str(), "https://h/jf");
    }

    #[test]
    fn leading_byte_order_mark_is_ignored() {
        let c = parse_config("\u{feff}server=https://h\n").unwrap();
        assert_eq!(c.server.as_str(), "https://h");
    }

    #[test]
    fn missing_server_is_an_error() {
        assert!(parse_config("").unwrap_err().contains("server"));
    }

    #[test]
    fn base_url_rejects_non_http_schemes() {
        assert!(BaseUrl::parse("ftp://h").is_err());
    }

    #[test]
    fn join_preserves_path_prefix() {
        let b = BaseUrl::parse("https://h/jf/").unwrap();
        assert_eq!(b.join("/Sessions/Playing"), "https://h/jf/Sessions/Playing");
    }

    #[test]
    fn websocket_url_maps_scheme_and_keeps_prefix() {
        let t = Secret::new("t".into());
        let w = |u: &str| BaseUrl::parse(u).unwrap().websocket(&t, "d");
        assert_eq!(w("http://h:8096").expose(), "ws://h:8096/socket?ApiKey=t&deviceId=d");
        assert_eq!(w("https://h/jf").expose(), "wss://h/jf/socket?ApiKey=t&deviceId=d");
    }
}
