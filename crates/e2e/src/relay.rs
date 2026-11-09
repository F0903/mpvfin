//! A relay between the plugin and the server that can cut its sockets and
//! watch its group requests.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use std::{mem, thread};

use crate::server::Server;

/// A TCP relay to the test server whose WebSocket connections can be cut or held back,
/// which acts on the plugin's real socket while its HTTP and media requests keep flowing.
/// It also records the plugin's `POST /SyncPlay/*`, which no other member can see.
pub struct SocketRelay {
    url: String,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    held: Arc<AtomicBool>,
    posts: Arc<Mutex<Vec<String>>>,
}

impl SocketRelay {
    pub fn start(server: &Server) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind relay");
        let url = format!("http://{}", listener.local_addr().expect("relay address"));
        let upstream = server
            .url()
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned();
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::new(AtomicBool::new(false));
        let posts = Arc::new(Mutex::new(Vec::new()));
        let (open, on_hold, seen) = (sockets.clone(), held.clone(), posts.clone());
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let (upstream, open, on_hold, seen) = (
                    upstream.clone(),
                    open.clone(),
                    on_hold.clone(),
                    seen.clone(),
                );
                thread::spawn(move || relay_connection(client, &upstream, &open, &on_hold, &seen));
            }
        });
        Self {
            url,
            sockets,
            held,
            posts,
        }
    }

    /// The name of each `POST /SyncPlay/<name>` since the last call, oldest first.
    pub fn take_group_posts(&self) -> Vec<String> {
        mem::take(&mut *self.posts.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// While on, nothing the server sends over a WebSocket reaches the plugin.
    pub fn hold_server_messages(&self, on: bool) {
        self.held.store(on, Ordering::SeqCst);
    }

    /// The address to give the plugin instead of the server's.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Cuts every WebSocket passing through; returns how many there were.
    pub fn cut_sockets(&self) -> usize {
        let cut = mem::take(&mut *self.sockets.lock().unwrap_or_else(PoisonError::into_inner));
        for socket in &cut {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        cut.len()
    }
}

pub(crate) fn relay_connection(
    client: TcpStream,
    upstream: &str,
    sockets: &Mutex<Vec<TcpStream>>,
    held: &AtomicBool,
    posts: &Arc<Mutex<Vec<String>>>,
) {
    let mut start = [0; 16];
    let Ok(seen) = client.peek(&mut start) else {
        return;
    };
    let Ok(server) = TcpStream::connect(upstream) else {
        return;
    };
    let is_socket = start[..seen].starts_with(b"GET /socket");
    if is_socket {
        let Ok(handle) = client.try_clone() else {
            return;
        };
        sockets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(handle);
    }
    let (Ok(mut client_read), Ok(mut server_read)) = (client.try_clone(), server.try_clone())
    else {
        return;
    };
    let (mut client_write, mut server_write) = (client, server);
    let posts = posts.clone();
    let up = thread::spawn(move || {
        let (mut buffer, mut pending) = ([0; 16 * 1024], Vec::new());
        while let Ok(read @ 1..) = client_read.read(&mut buffer) {
            pending.extend_from_slice(&buffer[..read]);
            // Before forwarding, so a POST is on record before anything can answer it.
            scan_group_posts(
                &mut pending,
                &mut posts.lock().unwrap_or_else(PoisonError::into_inner),
            );
            if server_write.write_all(&buffer[..read]).is_err() {
                break;
            }
        }
        let _ = server_write.shutdown(std::net::Shutdown::Both);
    });
    let mut buffer = [0; 16 * 1024];
    while let Ok(read @ 1..) = server_read.read(&mut buffer) {
        while is_socket && held.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(10));
        }
        if client_write.write_all(&buffer[..read]).is_err() {
            break;
        }
    }
    let _ = client_write.shutdown(std::net::Shutdown::Both);
    let _ = up.join();
}

pub(crate) const GROUP_POST: &[u8] = b"POST /SyncPlay/";

/// Moves each complete `POST /SyncPlay/<name>` from `pending` to `posts`, keeping any tail
/// that may begin one: a request line can span two reads.
pub(crate) fn scan_group_posts(pending: &mut Vec<u8>, posts: &mut Vec<String>) {
    while let Some(at) = pending
        .windows(GROUP_POST.len())
        .position(|window| window == GROUP_POST)
    {
        let name_at = at + GROUP_POST.len();
        let Some(length) = pending[name_at..].iter().position(|b| *b == b' ') else {
            pending.drain(..at);
            return;
        };
        posts.push(String::from_utf8_lossy(&pending[name_at..name_at + length]).into_owned());
        pending.drain(..name_at + length);
    }
    let keep = pending.len().min(GROUP_POST.len() - 1);
    pending.drain(..pending.len() - keep);
}

#[cfg(test)]
mod tests {
    use super::scan_group_posts;

    #[test]
    fn group_posts_are_found_across_reads() {
        let (mut pending, mut posts) = (Vec::new(), Vec::new());
        for read in [
            "POST /SyncPlay/Pa",
            "use HTTP/1.1\r\n\r\n{}POST /Sync",
            "Play/Seek HTTP/1.1\r\n\r\nGET /Videos/x HTTP/1.1\r\n\r\n",
        ] {
            pending.extend_from_slice(read.as_bytes());
            scan_group_posts(&mut pending, &mut posts);
        }
        assert_eq!(posts, ["Pause", "Seek"]);
        assert!(pending.len() < 15, "nothing else is kept");
    }
}
