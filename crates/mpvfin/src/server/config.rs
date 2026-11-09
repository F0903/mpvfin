//! `mpvfin.conf` parsing.

use jellyfin::BaseUrl;

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
    Ok(Config {
        server: BaseUrl::parse(server).map_err(|e| format!("server: {e}"))?,
    })
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
}
