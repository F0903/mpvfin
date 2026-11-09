//! The server's WebSocket: connecting, keep-alives and inbound messages.

use std::fmt;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::client::sanitize;
use crate::identity::Secret;
use crate::remote::{Inbound, parse_inbound};
use crate::url::BaseUrl;

/// An open WebSocket to the server. A read waits at most 250 ms.
pub struct Socket(WebSocket<MaybeTlsStream<TcpStream>>);

/// Credential-free error text.
fn clean(e: impl fmt::Display) -> String {
    sanitize(&e.to_string())
}

impl Socket {
    /// Connects with a 5 s timeout per address and does the WebSocket handshake.
    ///
    /// # Errors
    /// Credential-free text saying what failed.
    pub fn connect(base: &BaseUrl, token: &Secret, device_id: &str) -> Result<Self, String> {
        let url = base.websocket(token, device_id);
        let request = url.expose().into_client_request().map_err(clean)?;
        let uri = request.uri();
        let host = uri
            .host()
            .unwrap_or_default()
            .trim_matches(['[', ']'])
            .to_owned();
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("wss") {
                443
            } else {
                80
            });
        let mut error = io::Error::new(io::ErrorKind::NotFound, "the server name has no address");
        let mut tcp = None;
        for addr in (host.as_str(), port).to_socket_addrs().map_err(clean)? {
            match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
                Ok(stream) => {
                    tcp = Some(stream);
                    break;
                }
                Err(e) => error = e,
            }
        }
        let tcp = tcp.ok_or_else(|| clean(error))?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(clean)?;
        tcp.set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(clean)?;
        let (socket, _) = tungstenite::client_tls(request, tcp).map_err(clean)?;
        // Set on the stream itself: on Windows a `try_clone` handle has its own timeouts.
        let tcp = match socket.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp,
            MaybeTlsStream::Rustls(tls) => tls.get_ref(),
            _ => return Err("unsupported WebSocket stream".into()),
        };
        // Short, because a quit with a stalled resolve only has the rest of its bound for this.
        tcp.set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(clean)?;
        Ok(Self(socket))
    }

    /// Sends a keep-alive.
    ///
    /// # Errors
    /// The socket failed.
    pub fn send_keep_alive(&mut self) -> Result<(), String> {
        self.0
            .send(Message::text(r#"{"MessageType":"KeepAlive"}"#))
            .map_err(clean)
    }

    /// The next frame, `None` when none came in time. Frames other than text
    /// messages read as `Inbound::Ignored`: they still show the socket is alive.
    ///
    /// # Errors
    /// The socket failed.
    pub fn read(&mut self) -> Result<Option<Inbound>, String> {
        match self.0.read() {
            Ok(Message::Text(text)) => Ok(Some(parse_inbound(&text))),
            Ok(_) => Ok(Some(Inbound::Ignored)),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(clean(e)),
        }
    }

    /// Writes and flushes the close frame. A failure is moot: the session is ending.
    pub fn close(&mut self) {
        let _ = self.0.close(None);
    }
}
