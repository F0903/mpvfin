//! Local HTTP proxy: mpv only ever sees loopback URLs; the `Authorization`
//! header is added here, on the way upstream, so it cannot reach mpv's logs.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use jellyfin::Secret;
use ureq::http::Response;
use ureq::{Agent, Body};

use jellyfin::BaseUrl;
use jellyfin::new_device_id;
use jellyfin::tls_config;

const HEAD_LIMIT: usize = 16 * 1024;
const HEAD_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);
const FORWARDED_HEADERS: [&str; 4] = [
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
];

/// The full `Authorization` header value; `None` when logged out.
pub type TokenSource = Arc<dyn Fn() -> Option<Secret> + Send + Sync>;

struct Shared {
    agent: Agent,
    upstream: BaseUrl,
    auth: TokenSource,
    secret: String,
    stop: AtomicBool,
}

/// A running proxy; dropping it stops it.
pub struct Proxy {
    base: BaseUrl,
    addr: SocketAddr,
    shared: Arc<Shared>,
}

impl Proxy {
    /// Binds `127.0.0.1:0` and starts accepting; performs no other I/O.
    ///
    /// # Errors
    /// The socket cannot be bound or its thread cannot start.
    pub fn start(upstream: BaseUrl, auth: TokenSource) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        let secret = new_device_id();
        let base = BaseUrl::parse(&format!("http://{addr}/{secret}")).map_err(io::Error::other)?;
        let agent = Agent::config_builder()
            .timeout_resolve(Some(CONNECT_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            // Per read, not total: a film outlasts any global limit.
            .timeout_per_read(Some(UPSTREAM_READ_TIMEOUT))
            .max_redirects(0) // the header must never reach another host
            .http_status_as_error(false)
            .tls_config(tls_config())
            .build()
            .into();
        let shared = Arc::new(Shared {
            agent,
            upstream,
            auth,
            secret,
            stop: AtomicBool::new(false),
        });
        let accepting = shared.clone();
        thread::Builder::new()
            .name("mpvfin-proxy".into())
            .spawn(move || accept(&listener, &accepting))?;
        Ok(Self { base, addr, shared })
    }

    /// `http://127.0.0.1:<port>/<secret>`.
    #[must_use]
    pub fn base(&self) -> &BaseUrl {
        &self.base
    }

    /// Stops accepting and ends running copies; returns within half a second.
    /// A thread stuck on a read ends at that read's timeout.
    pub fn stop(&self) {
        if !self.shared.stop.swap(true, Ordering::Relaxed) {
            // Wakes the blocking accept.
            let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500));
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What to fetch upstream for one client request.
#[derive(Debug, Eq, PartialEq)]
pub struct Forward {
    /// Upstream path and query, starting `/Videos/`.
    pub path_and_query: String,
    /// The client's `Range` value, when it starts `bytes=`.
    pub range: Option<String>,
    /// The client sent `HEAD`.
    pub head_only: bool,
}

/// Validates a request head (request line, then header lines).
///
/// # Errors
/// The HTTP status to answer with.
pub fn parse_request(head: &str, secret: &str) -> Result<Forward, u16> {
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or_default().split_ascii_whitespace();
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(400);
    };
    if !version.starts_with("HTTP/") {
        return Err(400);
    }
    if method != "GET" && method != "HEAD" {
        return Err(405);
    }
    let rest = target
        .strip_prefix('/')
        .and_then(|t| t.strip_prefix(secret))
        .and_then(|t| t.strip_prefix('/'))
        .ok_or(404u16)?;
    let path = rest.split('?').next().unwrap_or_default();
    let decoded_dots = path.to_ascii_lowercase().replace("%2e", ".");
    if !path.starts_with("Videos/") || decoded_dots.contains("..") {
        return Err(403);
    }
    let range = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
        .map(|(_, value)| value.trim())
        .filter(|value| value.starts_with("bytes="))
        .map(str::to_owned);
    Ok(Forward {
        path_and_query: format!("/{rest}"),
        range,
        head_only: method == "HEAD",
    })
}

/// The status line and allowed headers of a response to mpv.
#[must_use]
pub fn response_head(status: u16, headers: &[(String, String)]) -> String {
    let mut head = format!("HTTP/1.1 {status} \r\n");
    for (name, value) in headers {
        if FORWARDED_HEADERS
            .iter()
            .any(|allowed| name.eq_ignore_ascii_case(allowed))
        {
            let _ = write!(head, "{name}: {value}\r\n");
        }
    }
    head.push_str("Connection: close\r\n\r\n");
    head
}

fn accept(listener: &TcpListener, shared: &Arc<Shared>) {
    for connection in listener.incoming() {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(client) = connection else {
            thread::sleep(Duration::from_millis(100)); // e.g. out of handles; do not spin
            continue;
        };
        let shared = shared.clone();
        // A failed spawn drops the closure, which closes the connection.
        let _ = thread::Builder::new()
            .name("mpvfin-proxy-client".into())
            .spawn(move || serve(client, &shared));
    }
}

fn serve(mut client: TcpStream, shared: &Shared) {
    let _ = client.set_read_timeout(Some(HEAD_READ_TIMEOUT));
    let reply = read_head(&mut client)
        .and_then(|head| parse_request(&head, &shared.secret))
        .and_then(|forward| Ok((fetch_upstream(&forward, shared)?, forward.head_only)));
    let (response, head_only) = match reply {
        Ok(reply) => reply,
        Err(status) => return reject(client, status),
    };
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect();
    let head = response_head(response.status().as_u16(), &headers);
    if client.set_nonblocking(true).is_ok()
        && send(&mut client, head.as_bytes(), &shared.stop)
        && !head_only
    {
        copy(
            response.into_body().into_reader(),
            &mut client,
            &shared.stop,
        );
    }
}

/// Reads up to the blank line; returns the head without it.
fn read_head(client: &mut TcpStream) -> Result<String, u16> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0; 4096];
    loop {
        if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            if end + 4 > HEAD_LIMIT {
                return Err(431);
            }
            head.truncate(end);
            return String::from_utf8(head).map_err(|_| 400);
        }
        if head.len() >= HEAD_LIMIT {
            return Err(431);
        }
        match client.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(400),
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
}

fn fetch_upstream(forward: &Forward, shared: &Shared) -> Result<Response<Body>, u16> {
    let auth = (shared.auth)().ok_or(503u16)?;
    let url = shared.upstream.join(&forward.path_and_query);
    let request = if forward.head_only {
        shared.agent.head(url)
    } else {
        shared.agent.get(url)
    };
    let request = request
        .header("Authorization", auth.expose())
        // ureq would otherwise ask for gzip and decode it behind `Content-Length`.
        .header("Accept-Encoding", "identity");
    let request = match &forward.range {
        Some(range) => request.header("Range", range),
        None => request,
    };
    let response = request.call().map_err(|_| 502u16)?;
    if matches!(response.status().as_u16(), 200 | 206 | 404 | 416) {
        Ok(response)
    } else {
        Err(502)
    }
}

/// Copies until the body ends, either side fails, or the proxy stops. Dropping
/// `body` part-way closes the upstream connection.
fn copy(mut body: impl Read, client: &mut TcpStream, stop: &AtomicBool) {
    let mut chunk = vec![0; 64 * 1024];
    while let Ok(n @ 1..) = body.read(&mut chunk) {
        if !send(client, &chunk[..n], stop) {
            return;
        }
    }
}

/// Writes `data` to a non-blocking socket, waiting while it is full (a paused
/// mpv can stay that way for hours). False once stopped or the client left.
///
/// Not a blocking write: on Windows nothing can wake a blocked `send`, and one
/// that timed out leaves the socket state undefined.
fn send(client: &mut TcpStream, mut data: &[u8], stop: &AtomicBool) -> bool {
    let mut wait = Duration::from_millis(1);
    while !data.is_empty() {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        match client.write(data) {
            Ok(0) => return false,
            Ok(n) => {
                data = &data[n..];
                wait = Duration::from_millis(1);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(100));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

fn reject(mut client: TcpStream, status: u16) {
    let head = response_head(status, &[("Content-Length".to_owned(), "0".to_owned())]);
    if client.write_all(head.as_bytes()).is_ok() {
        // Closing with unread request bytes resets the connection and loses the reply.
        let _ = client.shutdown(Shutdown::Write);
        let _ = io::copy(&mut client.take(64 * 1024), &mut io::sink());
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    const TOKEN: &str = "MediaBrowser Token=\"SECRET-TOKEN\"";
    const PARTIAL: &str = "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-4/10\r\n\
        Content-Length: 3\r\nSet-Cookie: a=b\r\n\r\nabc";

    fn proxy_to(upstream: &BaseUrl, auth: Option<&'static str>) -> Proxy {
        let token: TokenSource = Arc::new(move || auth.map(|a| Secret::new(a.to_owned())));
        Proxy::start(upstream.clone(), token).unwrap()
    }

    fn base_of(listener: &TcpListener) -> BaseUrl {
        BaseUrl::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap()
    }

    /// Answers every connection with `reply` and reports each request head.
    fn stub(reply: String) -> (BaseUrl, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = base_of(&listener);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = conn.unwrap();
                let Ok(head) = read_head(&mut conn) else {
                    continue;
                };
                let _ = tx.send(head);
                let _ = conn.write_all(reply.as_bytes());
            }
        });
        (base, rx)
    }

    /// Streams forever and reports when the proxy closes the connection.
    fn endless_upstream() -> (BaseUrl, Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = base_of(&listener);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            read_head(&mut conn).unwrap();
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\n\r\n")
                .unwrap();
            while conn.write_all(&[0; 8192]).is_ok() {}
            tx.send(()).unwrap();
        });
        (base, rx)
    }

    fn open(proxy: &Proxy, request: &str) -> TcpStream {
        let mut conn = TcpStream::connect(proxy.addr).unwrap();
        conn.write_all(request.as_bytes()).unwrap();
        conn
    }

    fn fetch(proxy: &Proxy, request: &str) -> String {
        let mut reply = Vec::new();
        open(proxy, request).read_to_end(&mut reply).unwrap();
        String::from_utf8_lossy(&reply).into_owned()
    }

    fn get(proxy: &Proxy, path: &str, headers: &str) -> String {
        format!(
            "GET /{}{path} HTTP/1.1\r\nHost: l\r\n{headers}\r\n",
            proxy.shared.secret
        )
    }

    #[test]
    fn parse_request_applies_path_and_method_rules() {
        let parse = |line: &str| parse_request(&format!("{line}\r\nHost: x"), "s");
        let forward = |path: &str, head_only| {
            Ok(Forward {
                path_and_query: path.to_owned(),
                range: None,
                head_only,
            })
        };
        assert_eq!(
            parse("GET /s/Videos/a?x=1 HTTP/1.1"),
            forward("/Videos/a?x=1", false)
        );
        assert_eq!(
            parse("HEAD /s/Videos/a HTTP/1.1"),
            forward("/Videos/a", true)
        );
        assert_eq!(
            parse("GET /s/Videos/a?x=.. HTTP/1.1"),
            forward("/Videos/a?x=..", false)
        );
        for (line, status) in [
            ("GET /s/Users/x HTTP/1.1", 403),
            ("GET /s/Videos HTTP/1.1", 403),
            ("GET /s//Videos/a HTTP/1.1", 403),
            ("GET /s/Videos/../Users HTTP/1.1", 403),
            ("GET /s/Videos/%2E%2e/Users HTTP/1.1", 403),
            ("GET /s/Videos/.%2E/Users HTTP/1.1", 403),
            ("GET /s/Videos/%2e./Users?x=1 HTTP/1.1", 403),
            ("POST /s/Videos/a HTTP/1.1", 405),
            ("get /s/Videos/a HTTP/1.1", 405),
            ("GET /wrong/Videos/a HTTP/1.1", 404),
            ("GET /s HTTP/1.1", 404),
            ("GET /ss/Videos/a HTTP/1.1", 404),
            ("", 400),
            ("GET", 400),
            ("GET /s/Videos/a", 400),
            ("GET /s/Videos/a HTTP/1.1 extra", 400),
            ("GET /s/Videos/a FTP/1.1", 400),
        ] {
            assert_eq!(parse(line), Err(status), "{line}");
        }
        let range = |header: &str| {
            parse_request(&format!("GET /s/Videos/a HTTP/1.1\r\n{header}"), "s")
                .unwrap()
                .range
        };
        assert_eq!(range("Range: bytes=5-"), Some("bytes=5-".to_owned()));
        assert_eq!(range("rAnGe:bytes=5-"), Some("bytes=5-".to_owned()));
        assert_eq!(range("Range: items=1"), None);
        assert_eq!(range("X-Range: bytes=5-"), None);
    }

    #[test]
    fn response_head_forwards_only_allowed_headers() {
        let headers: Vec<(String, String)> = [
            ("content-range", "bytes 0-1/2"),
            ("CONTENT-TYPE", "video/mp4"),
            ("Content-Length", "2"),
            ("accept-ranges", "bytes"),
            ("Set-Cookie", "a=b"),
            ("Location", "http://elsewhere"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let head = response_head(206, &headers);
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert!(head.contains("content-range: bytes 0-1/2\r\n"), "{head}");
        assert!(head.contains("CONTENT-TYPE: video/mp4\r\n"), "{head}");
        assert!(head.contains("Content-Length: 2\r\n"), "{head}");
        assert!(head.contains("accept-ranges: bytes\r\n"), "{head}");
        assert!(!head.contains("Set-Cookie"), "{head}");
        assert!(!head.contains("Location"), "{head}");
        assert!(head.ends_with("Connection: close\r\n\r\n"), "{head}");
    }

    #[test]
    fn proxy_forwards_range_and_adds_auth_upstream() {
        let (upstream, seen) = stub(PARTIAL.to_owned());
        let proxy = proxy_to(&upstream, Some(TOKEN));
        let reply = fetch(
            &proxy,
            &get(
                &proxy,
                "/Videos/x/stream?static=true",
                "Range: bytes=2-4\r\n",
            ),
        );
        assert!(reply.starts_with("HTTP/1.1 206"), "{reply}");
        assert!(reply.ends_with("\r\n\r\nabc"), "{reply}");
        let lower = reply.to_ascii_lowercase();
        assert!(lower.contains("content-range: bytes 2-4/10\r\n"), "{reply}");
        assert!(!lower.contains("set-cookie"), "{reply}");
        assert!(!reply.contains("SECRET-TOKEN"), "{reply}");
        let head = seen.try_recv().unwrap();
        assert!(
            head.starts_with("GET /Videos/x/stream?static=true HTTP/1.1"),
            "{head}"
        );
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("authorization: "), "{head}");
        assert!(head.contains(&format!(": {TOKEN}")), "{head}");
        assert!(lower.contains("range: bytes=2-4"), "{head}");
        assert!(lower.contains("accept-encoding: identity"), "{head}");
    }

    #[test]
    fn proxy_without_login_returns_503_and_skips_upstream() {
        let (upstream, seen) = stub(PARTIAL.to_owned());
        let proxy = proxy_to(&upstream, None);
        let reply = fetch(&proxy, &get(&proxy, "/Videos/x", ""));
        assert!(reply.starts_with("HTTP/1.1 503"), "{reply}");
        assert!(seen.try_recv().is_err());
    }

    #[test]
    fn proxy_rejects_wrong_secret_without_contacting_upstream() {
        let (upstream, seen) = stub(PARTIAL.to_owned());
        let proxy = proxy_to(&upstream, Some(TOKEN));
        let reply = fetch(&proxy, "GET /wrong/Videos/x HTTP/1.1\r\nHost: l\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
        assert!(seen.try_recv().is_err());
    }

    #[test]
    fn upstream_redirect_is_not_followed() {
        let (elsewhere, seen_elsewhere) = stub(PARTIAL.to_owned());
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}/Videos/x\r\nContent-Length: 0\r\n\r\n",
            elsewhere.as_str()
        );
        let (upstream, _) = stub(redirect);
        let proxy = proxy_to(&upstream, Some(TOKEN));
        let reply = fetch(&proxy, &get(&proxy, "/Videos/x", ""));
        assert!(reply.starts_with("HTTP/1.1 502"), "{reply}");
        assert!(seen_elsewhere.try_recv().is_err());
    }

    #[test]
    fn upstream_failures_become_502_without_upstream_text() {
        let body = "boom SECRET-DETAIL";
        let error = format!(
            "HTTP/1.1 500 Oops\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (upstream, _) = stub(error);
        let proxy = proxy_to(&upstream, Some(TOKEN));
        let reply = fetch(&proxy, &get(&proxy, "/Videos/x", ""));
        assert!(reply.starts_with("HTTP/1.1 502"), "{reply}");
        assert!(!reply.contains("SECRET-DETAIL"), "{reply}");
    }

    #[test]
    fn oversized_request_head_is_rejected() {
        let (upstream, seen) = stub(PARTIAL.to_owned());
        let proxy = proxy_to(&upstream, Some(TOKEN));
        let padding = format!("X-Pad: {}\r\n", "a".repeat(17 * 1024));
        let reply = fetch(&proxy, &get(&proxy, "/Videos/x", &padding));
        assert!(reply.starts_with("HTTP/1.1 431"), "{reply}");
        assert!(seen.try_recv().is_err());
    }

    #[test]
    fn client_disconnect_ends_the_upstream_read() {
        let (upstream, ended) = endless_upstream();
        let proxy = proxy_to(&upstream, Some(TOKEN));
        let mut client = open(&proxy, &get(&proxy, "/Videos/x", ""));
        client.read_exact(&mut [0; 1024]).unwrap();
        drop(client);
        ended.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn stop_is_prompt_and_ends_a_copy_blocked_on_a_paused_client() {
        let (upstream, ended) = endless_upstream();
        let proxy = proxy_to(&upstream, Some(TOKEN));
        let mut client = open(&proxy, &get(&proxy, "/Videos/x", ""));
        client.read_exact(&mut [0; 1024]).unwrap();
        thread::sleep(Duration::from_millis(300)); // socket buffers fill; the copy blocks
        let started = Instant::now();
        proxy.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
        ended.recv_timeout(Duration::from_secs(5)).unwrap();
        let refused = (0..40).any(|_| {
            thread::sleep(Duration::from_millis(50));
            TcpStream::connect_timeout(&proxy.addr, Duration::from_millis(200)).is_err()
        });
        assert!(refused, "still accepting after stop");
        drop(client); // held open so only `stop` could have ended the copy
    }
}
