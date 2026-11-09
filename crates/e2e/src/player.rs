//! A real mpv running the built plugin, driven over its IPC pipe.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use jellyfin::new_device_id;
use serde_json::{Value, json};

use crate::{State, TempDir, poll_until};

pub(crate) const IPC_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) const QUIT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PlayerOptions {
    pub server: String,
    pub state: State,
    pub mpv_conf: String,
    /// `None`: a temporary directory that lives as long as the `Player`.
    /// Otherwise the caller's; config files already there are kept.
    pub config_dir: Option<PathBuf>,
    /// Leaves `default` input sections and mpv's built-in keys live.
    pub default_bindings: bool,
    /// Appended to the generated `input.conf`.
    pub input_conf: String,
}

/// Writes the config files `Player::launch` needs into `dir`.
pub fn write_config(dir: &Path, opts: &PlayerOptions) {
    let scripts = dir.join("script-opts");
    fs::create_dir_all(&scripts).expect("create script-opts");
    let write = |path: PathBuf, content: &[u8]| {
        fs::write(&path, content).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    };
    let conf = format!("{}# e2e-sentinel\n", opts.mpv_conf);
    write(dir.join("mpv.conf"), conf.as_bytes());
    let input = format!("# e2e\n{}", opts.input_conf);
    write(dir.join("input.conf"), input.as_bytes());
    write(
        scripts.join("mpvfin.conf"),
        format!("server={}\n", opts.server).as_bytes(),
    );
    write(
        scripts.join("mpvfin-state.json"),
        &serde_json::to_vec(&opts.state).expect("serialize state"),
    );
}

pub(crate) type IpcRequest = (u64, String, Sender<Value>);

/// A spawned mpv with the plugin loaded and a private config dir.
pub struct Player {
    child: Child,
    requests: Option<Sender<IpcRequest>>,
    next_request: AtomicU64,
    config_dir: PathBuf,
    log_path: PathBuf,
    _owned_dir: Option<TempDir>,
}

impl Player {
    pub fn launch(opts: PlayerOptions) -> Self {
        let owned_dir = opts.config_dir.is_none().then(TempDir::new);
        let config_dir = match (&opts.config_dir, &owned_dir) {
            (Some(dir), _) => dir.clone(),
            (None, owned) => owned.as_ref().expect("just created").path().to_owned(),
        };
        // A second player on a shared dir must not touch the first's locked state.
        if !config_dir.join("mpv.conf").exists() {
            write_config(&config_dir, &opts);
        }
        let unique = new_device_id();
        let log_path = PathBuf::from(format!("{}.{unique}.log", config_dir.display()));
        let pipe = format!(r"\\.\pipe\mpvfin-e2e-{unique}");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mpv = std::env::var_os("MPVFIN_E2E_MPV")
            .map_or_else(|| root.join(".tools/mpv/mpv.com"), PathBuf::from);
        let mut child = Command::new(&mpv)
            .arg(format!("--config-dir={}", config_dir.display()))
            .args(["--idle=yes", "--vo=null", "--ao=null"])
            .arg(format!(
                "--script={}",
                root.join("target/debug/mpvfin.dll").display()
            ))
            .arg(format!("--input-ipc-server={pipe}"))
            .arg(format!("--log-file={}", log_path.display()))
            .arg("--msg-level=all=debug")
            // A stray Enter or media key on this machine would skip to the next item.
            .args((!opts.default_bindings).then_some("--input-default-bindings=no"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", mpv.display()));
        let Some(file) = connect_pipe(&pipe) else {
            kill_tree(&mut child);
            panic!("mpv did not create {pipe}");
        };
        let (requests, incoming) = mpsc::channel();
        thread::spawn(move || ipc_worker(&file, &incoming));
        Self {
            child,
            requests: Some(requests),
            next_request: AtomicU64::new(1),
            config_dir,
            log_path,
            _owned_dir: owned_dir,
        }
    }

    pub(crate) fn send(&self, command: &[Value]) -> mpsc::Receiver<Value> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let line = format!("{}\n", json!({"command": command, "request_id": id}));
        let (reply, response) = mpsc::channel();
        let requests = self.requests.as_ref().expect("live until drop");
        requests.send((id, line, reply)).expect("mpv IPC gone");
        response
    }

    pub(crate) fn request(&self, command: &[Value]) -> Value {
        self.send(command)
            .recv_timeout(IPC_TIMEOUT)
            .unwrap_or_else(|_| panic!("no mpv IPC reply to {command:?} in {IPC_TIMEOUT:?}"))
    }

    /// Reads a property; `Value::Null` while it is unavailable.
    pub fn get(&self, property: &str) -> Value {
        let mut reply = self.request(&[json!("get_property"), json!(property)]);
        match reply["error"].as_str() {
            Some("success") => reply["data"].take(),
            Some("property unavailable") => Value::Null,
            // An unset key of user-data is "not found", not a typo.
            Some("property not found") if property.starts_with("user-data/") => Value::Null,
            other => panic!("get {property}: {other:?}"),
        }
    }

    /// Runs an mpv command and requires it to succeed.
    pub fn command(&self, args: &[&str]) {
        let command: Vec<Value> = args.iter().map(|arg| json!(arg)).collect();
        let reply = self.request(&command);
        assert_eq!(reply["error"], "success", "{args:?}");
    }

    /// Sets a property to JSON null and requires it to succeed.
    pub fn set_null(&self, property: &str) {
        let reply = self.request(&[json!("set_property"), json!(property), Value::Null]);
        assert_eq!(reply["error"], "success", "set_null {property}");
    }

    /// Waits for a log line containing `needle` and returns it.
    #[track_caller]
    pub fn wait_log(&self, needle: &str, timeout: Duration) -> String {
        poll_until(timeout, || {
            self.log_text()
                .lines()
                .find(|line| line.contains(needle))
                .map(str::to_owned)
        })
    }

    /// Asks mpv to quit; returns the time until the process exited.
    pub fn quit(self) -> Duration {
        self.quit_with_log().0
    }

    /// `quit`, plus the whole log, shutdown lines included.
    pub fn quit_with_log(self) -> (Duration, String) {
        self.quit_using("quit")
    }

    /// `quit_with_log`, but through `quit-watch-later`.
    pub fn quit_watch_later(self) -> (Duration, String) {
        self.quit_using("quit-watch-later")
    }

    pub(crate) fn quit_using(mut self, command: &str) -> (Duration, String) {
        let started = Instant::now();
        // mpv may close the pipe before replying, so the reply is not awaited.
        drop(self.send(&[json!(command)]));
        while Instant::now() < started + QUIT_TIMEOUT {
            if self.child.try_wait().ok().flatten().is_some() {
                return (started.elapsed(), self.log_text());
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("mpv did not exit within {QUIT_TIMEOUT:?}");
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn log_text(&self) -> String {
        fs::read(&self.log_path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.requests = None;
        kill_tree(&mut self.child);
        if thread::panicking() {
            eprintln!("kept for diagnosis: {}", self.log_path.display());
        } else {
            let _ = fs::remove_file(&self.log_path);
        }
    }
}

/// Kills `child` if it is still running, with its whole process tree:
/// `mpv.com` is a wrapper, and killing it alone would orphan `mpv.exe`.
pub(crate) fn kill_tree(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub(crate) fn connect_pipe(pipe: &str) -> Option<File> {
    let deadline = Instant::now() + IPC_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(pipe) {
            return Some(file);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Owns the pipe. Requests run one at a time, because a synchronous pipe
/// handle cannot read and write concurrently. Ends when mpv closes the pipe.
pub(crate) fn ipc_worker(pipe: &File, requests: &mpsc::Receiver<IpcRequest>) {
    let mut writer = pipe;
    let mut reader = BufReader::new(pipe);
    for (id, line, reply) in requests {
        if writer.write_all(line.as_bytes()).is_err() {
            return;
        }
        loop {
            let mut text = String::new();
            match reader.read_line(&mut text) {
                Ok(1..) => {}
                _ => return,
            }
            // Other lines are mpv events.
            if let Ok(value) = serde_json::from_str::<Value>(&text)
                && value["request_id"] == id
            {
                let _ = reply.send(value);
                break;
            }
        }
    }
}
