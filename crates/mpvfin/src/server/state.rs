//! The persisted state file, which doubles as the single-instance lock.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::{fmt, io, path::Path};

use serde::{Deserialize, Serialize};

use jellyfin::new_device_id;

/// Persisted login and identity.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Stable device id.
    pub device_id: String,
    /// Server the token belongs to.
    pub server: Option<String>,
    /// Logged-in user id.
    pub user_id: Option<String>,
    /// Access token; never printed.
    pub access_token: Option<String>,
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("device_id", &self.device_id)
            .field("server", &self.server)
            .field("user_id", &self.user_id)
            .field("access_token", &self.access_token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Why the state file could not be locked.
#[derive(Debug)]
pub enum LockError {
    /// Another instance holds the lock.
    Busy,
    /// Opening or locking failed.
    Io(io::Error),
}

/// The state file, held locked for the life of this value.
#[derive(Debug)]
pub struct StateFile(File);

impl StateFile {
    /// Opens (creating if missing) and exclusively locks the file.
    ///
    /// # Errors
    /// `Busy` when held elsewhere, `Io` on any other failure.
    pub fn lock(path: &Path) -> Result<Self, LockError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(LockError::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(TryLockError::WouldBlock) => Err(LockError::Busy),
            Err(TryLockError::Error(e)) => Err(LockError::Io(e)),
        }
    }

    /// Reads the state; a missing or corrupt file yields a fresh one.
    pub fn load(&mut self) -> State {
        let mut text = String::new();
        let read = self
            .0
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.0.read_to_string(&mut text));
        read.ok()
            .and_then(|_| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| State {
                device_id: new_device_id(),
                server: None,
                user_id: None,
                access_token: None,
            })
    }

    /// Rewrites the file through the locked handle.
    ///
    /// # Errors
    /// Any I/O failure.
    pub fn save(&mut self, state: &State) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(state).map_err(io::Error::from)?;
        self.0.set_len(0)?;
        self.0.seek(SeekFrom::Start(0))?;
        self.0.write_all(&json)?;
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("mpvfin-{name}-{}", new_device_id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn file(&self) -> std::path::PathBuf {
            self.0.join("state.json")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn is_id(s: &str) -> bool {
        s.len() == 32 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
    }

    #[test]
    fn state_round_trips_through_locked_file() {
        let d = Dir::new("rt");
        let s = State {
            device_id: new_device_id(),
            server: Some("https://h".into()),
            user_id: Some("u".into()),
            access_token: Some("tok".into()),
        };
        let mut f = StateFile::lock(&d.file()).unwrap();
        f.save(&s).unwrap();
        drop(f);
        assert_eq!(StateFile::lock(&d.file()).unwrap().load(), s);
    }

    #[test]
    fn corrupt_state_gets_fresh_device_id() {
        let d = Dir::new("corrupt");
        std::fs::write(d.file(), "not json").unwrap();
        let s = StateFile::lock(&d.file()).unwrap().load();
        assert!(is_id(&s.device_id));
        assert!(s.access_token.is_none());
    }

    #[test]
    fn second_lock_is_busy_while_first_held() {
        let d = Dir::new("busy");
        let first = StateFile::lock(&d.file()).unwrap();
        assert!(matches!(StateFile::lock(&d.file()), Err(LockError::Busy)));
        drop(first);
        assert!(StateFile::lock(&d.file()).is_ok());
    }

    #[test]
    fn state_debug_hides_token() {
        let s = State {
            device_id: "d".into(),
            server: None,
            user_id: None,
            access_token: Some("s3cret".into()),
        };
        assert!(!format!("{s:?}").contains("s3cret"));
    }
}
