//! Blocking Jellyfin REST client and reconnect backoff.

use std::fmt;
use std::time::Duration;

use mpvfin_core::Secret;
use serde_json::Value;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

use crate::config::BaseUrl;

/// Why a request failed.
#[derive(Debug)]
pub enum HttpError {
    /// HTTP 401: the token is missing, invalid or revoked.
    Unauthorized,
    /// Any other non-success status.
    Status(u16),
    /// Connection, TLS, timeout or decoding failure; credential-free text.
    Transport(String),
}

impl HttpError {
    /// A transport error whose text has credentials masked.
    #[must_use]
    pub fn transport(text: &str) -> Self {
        Self::Transport(sanitize(text))
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::Status(code) => write!(f, "HTTP {code}"),
            Self::Transport(text) => f.write_str(text),
        }
    }
}

impl From<ureq::Error> for HttpError {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::StatusCode(code) => classify_status(code),
            e => Self::transport(&e.to_string()),
        }
    }
}

/// Maps a non-success HTTP status to an error.
#[must_use]
pub fn classify_status(code: u16) -> HttpError {
    if code == 401 {
        HttpError::Unauthorized
    } else {
        HttpError::Status(code)
    }
}

/// Masks the values of `ApiKey=`, `api_key=`, `Token=` and `secret=` (any case).
pub(crate) fn sanitize(text: &str) -> String {
    let lower = text.to_ascii_lowercase(); // same byte offsets as `text`
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    while let Some(value) = ["apikey=", "api_key=", "token=", "secret="]
        .iter()
        .filter_map(|k| lower[done..].find(k).map(|p| done + p + k.len()))
        .min()
    {
        let value = value + usize::from(text[value..].starts_with('"'));
        let end = text[value..]
            .find(|c: char| c.is_whitespace() || matches!(c, '&' | '"' | '\'' | ',' | ')'))
            .map_or(text.len(), |p| value + p);
        out.push_str(&text[done..value]);
        out.push_str("***");
        done = end;
    }
    out.push_str(&text[done..]);
    out
}

/// Backoff before reconnect `attempt` (0-based): `min(60, 2^attempt)` s
/// scaled by `0.75 + 0.5 * jitter`, with `jitter` in `[0, 1]`.
#[must_use]
pub fn reconnect_delay(attempt: u32, jitter: f64) -> Duration {
    Duration::from_secs(2u64.saturating_pow(attempt).min(60))
        .mul_f64(0.75 + 0.5 * jitter.clamp(0.0, 1.0))
}

/// REST client for one server and one `Authorization` header value.
#[derive(Clone)] // no Debug: the header carries the token
pub struct Client {
    agent: Agent,
    base: BaseUrl,
    auth: Secret,
}

impl Client {
    /// A client with a 5 s connect and 10 s overall timeout per request.
    #[must_use]
    pub fn new(base: BaseUrl, auth: Secret) -> Self {
        let agent = Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(10)))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into();
        Self { agent, base, auth }
    }

    /// The server base URL.
    #[must_use]
    pub fn base(&self) -> &BaseUrl {
        &self.base
    }

    /// `GET`s `path_and_query` and parses the JSON reply (`Null` when empty).
    ///
    /// # Errors
    /// Non-success status or transport failure.
    pub fn get(&self, path_and_query: &str) -> Result<Value, HttpError> {
        let request = self.agent.get(self.base.join(path_and_query));
        json_reply(request.header("Authorization", self.auth.expose()).call())
    }

    /// `POST`s `body` as JSON (no body when `Null`) and parses the JSON reply.
    ///
    /// # Errors
    /// Non-success status or transport failure.
    pub fn post(&self, path_and_query: &str, body: &Value) -> Result<Value, HttpError> {
        let request = self.agent.post(self.base.join(path_and_query));
        let request = request.header("Authorization", self.auth.expose());
        json_reply(if body.is_null() {
            request.send_empty()
        } else {
            request.send_json(body)
        })
    }
}

fn json_reply(
    reply: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Value, HttpError> {
    let text = reply?.body_mut().read_to_string()?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| HttpError::transport(&format!("bad JSON reply: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_delay_doubles_caps_and_jitters() {
        assert_eq!(reconnect_delay(0, 0.0), Duration::from_millis(750));
        assert_eq!(reconnect_delay(1, 1.0), Duration::from_millis(2500));
        assert_eq!(reconnect_delay(10, 0.5), Duration::from_mins(1));
        assert_eq!(reconnect_delay(40, 1.0), Duration::from_secs(75));
    }

    #[test]
    fn unauthorized_status_is_classified() {
        assert!(matches!(classify_status(401), HttpError::Unauthorized));
        assert!(matches!(classify_status(500), HttpError::Status(500)));
    }

    #[test]
    fn transport_errors_do_not_leak_credentials() {
        let msg = "io: failed wss://h/socket?ApiKey=SECRET&deviceId=d (Token=\"SECRET\")";
        let HttpError::Transport(text) = HttpError::transport(msg) else {
            panic!("not a transport error");
        };
        assert!(!text.contains("SECRET"), "{text}");
        assert!(text.contains("deviceId=d"), "{text}");
    }
}
