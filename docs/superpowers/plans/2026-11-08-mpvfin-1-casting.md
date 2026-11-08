# mpvfin Plan 1: Casting and Remote Control

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the user's own mpv a Jellyfin cast target, with Quick Connect
login, remote control, queueing, track control, local-change reporting and
resume/watched reporting. Nothing in the user's mpv config may change.

**Architecture:**
- The existing reducer (`mpvfin-core`) stays the single owner of playback
  state. It is reworked around mpv's native playlist:
  - A cast puts `mpvfin://<itemId>` entries into mpv's playlist.
  - An `on_load` hook catches each entry as mpv starts it. The hook asks
    Jellyfin to negotiate playback, then redirects mpv to the real stream
    with per-file options, the way mpv's own ytdl hook works.
- A new `mpvfin-jellyfin` crate owns all network I/O on its own threads.
- The C plugin's thread runs the mpv event loop. It feeds the reducer and
  carries out its actions.
- Other threads only enqueue events and call `mpv_wakeup`.

**Tech Stack:**
- Rust 2024 (1.97).
- mpv client API through the existing `MPV_CPLUGIN_DYNAMIC_SYM` function
  pointer slots.
- `ureq` 3 for blocking HTTP and `tungstenite` for the WebSocket, both on
  rustls with the `ring` provider.
- `serde` and `serde_json`.
- Docker Compose for the Jellyfin 10.11 test server.
- PowerShell for scripts.

**Spec:** `docs/superpowers/specs/2026-11-08-mpvfin-design.md`. This plan
covers spec goal 1 ("Casting and remote control"), "Logging in" except the
username/password fallback, all anti-goals, and the Docker test server.
- The password fallback needs the prompt mechanism that watch-together
  introduces, so it moves to plan 3. Until then, a server with Quick Connect
  disabled gets a clear OSD message.
- Plan 2 (skip button) and plan 3 (watch together) are written after this
  plan ships.

## Global Constraints

- Edition 2024, `rust-version = "1.97"`, existing workspace lints.
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` pass at the end of every task.
- Clean room (`docs/clean-room.md`): never read, copy or adapt
  jellyfin-mpv-shim code, data structures, fixtures or layout. Derive wire
  formats from the official Jellyfin 10.11 API and SDKs, plus black-box
  checks against the Docker test server.
- The plugin is the only product. There is no tray app, agent or JSON-IPC
  backend. The `mpvfin-mpv` crate is deleted (Task 2).
- The plugin never writes `mpv.conf`/`input.conf` and never sets a global
  option. Everything it sets for a Jellyfin item goes through `loadfile`
  options or `file-local-options/…`. The only exceptions are `volume`,
  `mute` and `pause`.
- mpvfin owns exactly three files in mpv's config dir:
  - `scripts/mpvfin.dll` (`.so` on Linux)
  - `script-opts/mpvfin.conf` (user-written)
  - `script-opts/mpvfin-state.json`
- No access token in:
  - any URL handed to mpv;
  - any `loadfile` argument;
  - mpvfin's log lines;
  - the OSD;
  - any `Debug` output;
  - mpv's log at levels up to `debug`.

  Media auth is the per-file header `Authorization: MediaBrowser
  Token="<token>"`, set from the `on_load` hook.
- No network I/O, sleep or blocking wait on the mpv event thread. Worker
  threads never touch the mpv handle directly. They send events through a
  channel and wake mpv.
- No panic crosses the C boundary. Keep the existing `catch_unwind`.
- OSD text appears only for things the user must act on: the login code,
  errors, and a missing config. Diagnostics go to mpv's log with the prefix
  `[mpvfin] `.
- Direct play only. `PlayableMediaTypes` is `["Video"]`.
- Every `on_load` hook ID is continued exactly once.

## Review Focus

These are the five failure modes most likely to bite a real user that the
spec implies but no feature test would naturally hit. Each has a pinned
test in Task 9.

1. **Server unreachable** at startup or mid-playback: mpv keeps responding
   to input and quits within 5 s. Test: `mpv_stays_responsive_and_quits_when_server_unreachable`.
2. **Quitting mpv mid-episode:** Jellyfin keeps the resume position. Test:
   `quitting_mid_playback_saves_resume_position`.
3. **Opening a local file while a cast plays:** Jellyfin gets a stop report,
   and the local file plays with none of mpvfin's header or title. Test:
   `local_file_during_cast_reports_stop_and_stays_untouched`.
4. **Saved token rejected** (revoked, server reset): mpvfin falls back to
   Quick Connect instead of failing silently. Test:
   `rejected_token_falls_back_to_quick_connect`.
5. **User has `save-position-on-quit=yes`:** quitting during a cast writes
   no watch-later file for the Jellyfin item. Test (part of the isolation
   test): `config_dir_untouched_and_token_absent_from_log`.

---

### Task 1: Docker test server

**Files:**
- Create: `tests/server/compose.yml`
- Create: `scripts/test-server.ps1`
- Create: `tests/server/subs/en.srt`, `tests/server/subs/de.srt`,
  `tests/server/subs/fr.srt` (three cues each, 0–120 s)

**Interfaces:**
- Produces:
  - a server at `http://127.0.0.1:8096`;
  - admin user `admin` with password `mpvfin-test` (a throwaway local
    server; hardcoding is fine);
  - a TV library "Shows" containing `Test Show`, Season 1, episodes
    `S01E01`–`S01E03`, each 120 s;
  - Quick Connect enabled.

  Task 9 relies on these exact names.

- [ ] **Step 1: Write `tests/server/compose.yml`**
  - Project name `mpvfin-test`.
  - Service `jellyfin`, image `jellyfin/jellyfin:10.11.11` (if that tag is
    not on Docker Hub, use the newest `10.11.x` and record it in
    `docs/jellyfin-protocol.md`).
  - Port `127.0.0.1:8096:8096`.
  - Named volumes `config:/config`, `cache:/cache`, `media:/media`. Use named
    volumes, not Windows bind mounts.

- [ ] **Step 2: Write `scripts/test-server.ps1 -Action up|down|reset`**
  - `up` is idempotent:
    1. `docker compose up -d`.
    2. Wait for `GET /System/Info/Public`.
    3. If `/media/shows` is empty, generate the episodes inside the
       container with its bundled `/usr/lib/jellyfin-ffmpeg/ffmpeg`
       (`docker compose exec`). Each episode `Test Show - S01E0N.mkv` has:
       - `testsrc2` 320x180 at 24 fps, `-t 120`;
       - audio stream 1: `sine=440`, language `eng`;
       - audio stream 2: `sine=880`, language `ger`;
       - embedded srt subtitles from `en.srt` (`eng`) and `de.srt` (`ger`).

       Also copy `fr.srt` as sidecar `Test Show - S01E01.fr.srt`, which is
       the external subtitle.
    4. If `StartupWizardCompleted` is false, run the startup wizard over the
       API (`/Startup/Configuration`, `/Startup/User` with the admin
       credentials, `/Startup/RemoteAccess`, `/Startup/Complete`).
    5. Log in as admin and ensure `QuickConnectAvailable=true` in
       `/System/Configuration`.
    6. Create the "Shows" `tvshows` library at `/media/shows` if missing.
    7. Poll `GET /Items?Recursive=true&IncludeItemTypes=Episode` until it
       returns 3 items.
    8. Print `Test server ready: 3 episodes`.
  - `down` runs `docker compose stop`.
  - `reset` runs `docker compose down -v`.

- [ ] **Step 3: Verify from scratch**

  Run:
  ```powershell
  ./scripts/test-server.ps1 -Action reset
  ./scripts/test-server.ps1 -Action up
  ```
  Expected: the last line is `Test server ready: 3 episodes`.

  Running `up` a second time prints the same line without regenerating
  media.

- [ ] **Step 4: Record verified endpoints**

  Append a "Test server provisioning" section to
  `docs/jellyfin-protocol.md`. List the exact startup-wizard and library
  calls that worked, plus the image tag.

- [ ] **Step 5: Commit**

  `git add tests/server scripts/test-server.ps1 docs/jellyfin-protocol.md && git commit -m "test: add Docker Jellyfin test server"`

---

### Task 2: Reducer rework for playlist entries and the load hook

**Files:**
- Modify: `crates/mpvfin-core/src/lib.rs` (types, reducer, tests)
- Delete: `crates/mpvfin-mpv/` and its entry in the workspace `members`. Its
  only job was translating the old actions. The C plugin is the only
  backend, and keeping the crate would break the workspace build here.

**Interfaces:**
- Produces the following. Every type derives
  `Clone, Debug, Eq, PartialEq` unless noted.

```rust
pub struct Secret(String);            // Debug prints "Secret(***)"
impl Secret { pub fn new(s: String) -> Self; pub fn expose(&self) -> &str; }

pub enum QueueMode { Replace, Next, Last }
pub struct QueueRequest {
    pub item_ids: Vec<String>,
    pub mode: QueueMode,
    pub start_index: usize,            // Replace only; clamp to len-1
    pub start_position: Duration,      // Replace only; applies to the start item
    pub selection: StreamSelection,    // Replace only; applies to the start item
}
#[derive(Default)] pub struct StreamSelection { pub audio: Option<i32>, pub subtitle: Option<i32> } // Jellyfin indices; subtitle -1 = off

pub struct PlayerToken { pub entry_id: i64, pub hook_id: u64 }

pub struct TrackMap { pub audio: Vec<i32>, pub subtitles: Vec<i32> } // Jellyfin indices in mpv order; mpv id = position + 1
impl TrackMap {
    pub fn mpv_audio_id(&self, index: i32) -> Option<i64>;
    pub fn mpv_subtitle_id(&self, index: i32) -> Option<i64>;
    pub fn jellyfin_audio_index(&self, mpv_id: i64) -> Option<i32>;
    pub fn jellyfin_subtitle_index(&self, mpv_id: i64) -> Option<i32>;
}

pub struct ResolvedMedia {
    pub item_id: String,
    pub uri: String,                   // credential-free stream URL
    pub title: String,
    pub play_session_id: String,
    pub media_source_id: String,
    pub play_method: PlayMethod,
    pub auth_header: Secret,           // full header line: Authorization: MediaBrowser Token="…"
    pub external_subtitles: Vec<String>, // credential-free URLs, in TrackMap order after embedded subs
    pub tracks: TrackMap,
}

pub enum EndReason { Eof, Stopped, Failed(String) }
pub enum StopReason { Ended, Stopped, Failed, Shutdown }
```

- `PlaybackSnapshot` gains `audio_stream_index: Option<i32>` and
  `subtitle_stream_index: Option<i32>`. A subtitle value of `None` means
  off.
- `PlayRequest` is removed.
- `ReceiverEvent` becomes:
  - `RemotePlay(QueueRequest)`
  - `RemoteNext`, `RemotePrevious`
  - `RemotePause`, `RemoteResume`, `RemoteTogglePause`
  - `RemoteSeek(Duration)`, `RemoteStop`
  - `RemoteSetVolume(u8)`, `RemoteSetMuted(bool)`, `RemoteToggleMute`
  - `RemoteSetAudioStream(i32)`, `RemoteSetSubtitleStream(i32)`
  - `PlayerOpening { token: PlayerToken, item_id: String }`
  - `MediaResolved { generation, media }`
  - `MediaResolutionFailed { generation, message }`
  - `PlayerFileLoaded { generation }`
  - `PlayerPositionChanged { generation, position, seek_completed }`
  - `PlayerPauseChanged { generation, paused }`
  - `PlayerVolumeChanged { generation, volume_percent }`
  - `PlayerMuteChanged { generation, muted }`
  - `PlayerTracksChanged { generation, audio_stream_index: Option<i32>, subtitle_stream_index: Option<i32> }`
  - `PlayerEnded { generation, reason: EndReason }`
  - `ProgressTick`, `Shutdown`
- `ReceiverAction` becomes:
  - `QueueItems(QueueRequest)`
  - `PlaylistNext`, `PlaylistPrevious`
  - `ResolveMedia { generation, token: PlayerToken, item_id: String }`
  - `ContinueLoad { generation, hook_id: u64, media: ResolvedMedia, selection: StreamSelection }`
  - `AbortLoad { hook_id: u64 }`
  - `SetPaused { generation, paused }`, `Seek { generation, position }`
  - `SetVolume { generation, volume_percent }`, `SetMuted { generation, muted }`
  - `SetAudioStream { generation, index: i32 }`
  - `SetSubtitleStream { generation, index: i32 }`
  - `StopPlayer { generation }`
  - `ReportStarted(PlaybackSnapshot)`, `ReportProgress(PlaybackSnapshot)`
  - `ReportStopped { snapshot, reason: StopReason }`
  - `PlaybackFailed { generation, item_id, message }`
- `CancelMediaResolution`, `LoadPlayer` and `PlayerFailed` are removed.
- `PlaybackState` becomes:
  - `Idle`
  - `Resolving { generation, token, item_id, selection }`
  - `Loading { generation, media }`
  - `Playing(PlaybackSnapshot)`

**Transition rules the tests pin:**
- **Remote play:** `RemotePlay` changes no state and emits `QueueItems`. For
  `Replace` with a non-default selection, it remembers
  `(item_ids[start_index], selection)`. The first `PlayerOpening` for that
  item ID consumes the remembered selection. Any later `RemotePlay` replaces
  it.
- **Opening an entry:** `PlayerOpening` first does this:
  - If the state is `Resolving`, it emits `AbortLoad` for the old hook.
  - If the state is `Loading` or `Playing`, it terminates with
    `StopReason::Stopped` (report only; no `StopPlayer`).

  Then it allocates a generation, enters `Resolving`, and emits
  `ResolveMedia`.
- **Resolution result:** `MediaResolved` for the current generation moves to
  `Loading` and emits `ContinueLoad`. `MediaResolutionFailed` for the
  current generation moves to `Idle` and emits `AbortLoad` and then
  `PlaybackFailed`. Results for any other generation are ignored.
- **File loaded:** `PlayerFileLoaded` moves `Loading` to `Playing` and emits
  `ReportStarted`. The snapshot starts at position zero, unpaused, volume
  100, unmuted, with no track indices. The adapter sends the real values
  right after.
- **End of an entry:** `PlayerEnded` for the current generation moves to
  `Idle` and reports a stop exactly once:
  - `Eof` reports `StopReason::Ended`.
  - `Stopped` reports `StopReason::Stopped`.
  - `Failed` reports `StopReason::Failed` and also emits `PlaybackFailed`.

  It never emits `StopPlayer`, because mpv already ended the entry.
  `PlayerEnded` while `Resolving` for that generation emits `AbortLoad` and
  goes `Idle` without a report.
- **Remote stop:**
  - While `Playing` or `Loading`, it emits only `StopPlayer`. The report
    follows from the resulting `PlayerEnded`.
  - While `Resolving`, it emits `AbortLoad` and `StopPlayer` and goes `Idle`
    without a report.
- **Other remote commands:**
  - `RemoteTogglePause` and `RemoteToggleMute` invert the confirmed snapshot
    value.
  - `RemoteSetAudioStream` and `RemoteSetSubtitleStream` emit their action
    only while `Playing`.
  - `RemoteNext` and `RemotePrevious` always emit `PlaylistNext` and
    `PlaylistPrevious`.
- **Player track changes:** `PlayerTracksChanged` updates the snapshot and
  emits `ReportProgress` only if something changed.
- **Shutdown:** while `Resolving`, it emits `AbortLoad`. While `Loading` or
  `Playing`, it reports `StopReason::Shutdown`. It never emits `StopPlayer`.
- Pause, volume, mute, position and progress-tick behaviour stays as it is
  today.

- [ ] **Step 1: Write the failing tests**

  Rewrite the module's test helpers for the new types and replace the
  existing tests with these. Keep the intent of the existing ones.
  - `remote_play_queues_items_without_changing_state`: `RemotePlay(Replace, [a,b], start 1, 30 s)` yields `[QueueItems(that request)]` and `playback()` is `Idle`.
  - `opening_entry_starts_resolution_with_fresh_generation`: `PlayerOpening{token(7, 70), "a"}` yields `[ResolveMedia{gen 1, token, "a"}]`. A second open after the first ends yields gen 2.
  - `resolution_continues_the_pending_hook_with_requested_selection`: `RemotePlay(Replace [a,b], start 0, selection {audio 2, subtitle -1})`, then open `a`, then resolve. The last action is `ContinueLoad{hook_id 70, selection {Some(2), Some(-1)}}`. Opening `b` afterwards gets `StreamSelection::default()`.
  - `play_is_reported_only_after_correlated_file_loaded`: no `Report*` before `PlayerFileLoaded` for the right generation. `PlayerFileLoaded` for a stale generation does nothing.
  - `stale_resolution_is_ignored_after_next_entry_opens`: open a (hook 70), then open b (hook 80). This yields `[AbortLoad{70}, ResolveMedia{gen 2…}]`. A late `MediaResolved{gen 1}` yields `[]`.
  - `every_hook_is_continued_exactly_once`: drive four scripts and count the `ContinueLoad` plus `AbortLoad` actions for each hook ID. Each count must be exactly 1.
    - resolved;
    - resolution failed;
    - superseded by another open;
    - shutdown while resolving.
  - `end_of_file_reports_stopped_once_without_stop_command`: `PlayerEnded{Eof}` yields `[ReportStopped{reason Ended}]`. A repeat yields `[]`.
  - `remote_stop_stops_player_then_reports_on_end`: `RemoteStop` yields `[StopPlayer]`. Then `PlayerEnded{Stopped}` yields `[ReportStopped{reason Stopped}]`.
  - `player_failure_reports_failed_stop_and_surfaces_error`: `PlayerEnded{Failed("x")}` while `Playing` yields `[ReportStopped{Failed}, PlaybackFailed{message "x"}]`.
  - `toggles_invert_confirmed_state`: when the snapshot is paused=false and muted=true, `RemoteTogglePause` yields `SetPaused{true}` and `RemoteToggleMute` yields `SetMuted{false}`.
  - `track_changes_are_reported_only_after_player_confirmation`: `RemoteSetAudioStream(2)` yields `[SetAudioStream{index 2}]` and no report. `PlayerTracksChanged{Some(2), Some(5)}` yields one `ReportProgress` with both indices. A repeat yields `[]`.
  - `controller_changes_are_reported_only_after_player_confirmation`: keep the existing test, adapted.
  - `shutdown_reports_active_playback_without_stop_command`: `Shutdown` while `Playing` yields `[ReportStopped{Shutdown}]`.
  - `track_map_round_trips_embedded_and_external_indices`: `TrackMap{audio [1,2], subtitles [3,4,7]}`. `mpv_audio_id(2)` is `Some(2)`, `mpv_subtitle_id(7)` is `Some(3)`, `jellyfin_subtitle_index(1)` is `Some(3)`, `mpv_audio_id(9)` is `None`.
  - `secret_debug_is_redacted`: `format!("{:?}", Secret::new("tok".into()))` does not contain `tok`.

- [ ] **Step 2: Run the tests and confirm they fail to compile or fail**

  Run: `cargo test -p mpvfin-core`
  Expected: compile errors for the missing types and variants.

- [ ] **Step 3: Implement the types and reducer transitions above** in `crates/mpvfin-core/src/lib.rs`.

  Keep the single `handle` match and the existing helper style
  (`terminate_current`, `playing_mut`). `terminate_current` loses its
  `StopPlayer` push. Remote stop emits `StopPlayer` itself.

- [ ] **Step 4: Run the tests and lints**

  Run: `cargo test --workspace` and
  `cargo clippy --workspace --all-targets -- -D warnings`
  Expected: all pass, and nothing references `mpvfin_mpv`.

- [ ] **Step 5: Commit**

  `git add -A crates Cargo.toml Cargo.lock && git commit -m "core: drive playback from mpv playlist entries and load hook"`

---

### Task 3: `mpvfin-jellyfin` crate: config, state file, identity and URLs

**Files:**
- Modify: `Cargo.toml` (add the member `crates/mpvfin-jellyfin`)
- Create: `crates/mpvfin-jellyfin/Cargo.toml`. Dependencies: `mpvfin-core`,
  `serde` (derive), `serde_json`. Network crates arrive in Task 6.
- Create: `crates/mpvfin-jellyfin/src/lib.rs` (module declarations and
  re-exports)
- Create: `crates/mpvfin-jellyfin/src/config.rs`
- Create: `crates/mpvfin-jellyfin/src/state.rs`
- Create: `crates/mpvfin-jellyfin/src/identity.rs`

**Interfaces:**

```rust
// config.rs
pub struct Config { pub server: BaseUrl }
pub fn parse_config(text: &str) -> Result<Config, String>;   // mpv script-opts: `key=value` lines, `#` comments, whitespace trimmed; unknown keys ignored
pub struct BaseUrl(String);                                  // scheme+host+optional path prefix, no trailing slash
impl BaseUrl {
    pub fn parse(s: &str) -> Result<Self, String>;           // requires http:// or https://
    pub fn join(&self, path_and_query: &str) -> String;      // path_and_query starts with '/'
    pub fn websocket(&self, token: &Secret, device_id: &str) -> Secret; // ws/wss + prefix + /socket?ApiKey=…&deviceId=…
    pub fn as_str(&self) -> &str;
}

// state.rs
#[derive(Serialize, Deserialize)] pub struct State { pub device_id: String, pub server: Option<String>, pub user_id: Option<String>, pub access_token: Option<String> }
pub struct StateFile { /* holds the locked std::fs::File */ }
pub enum LockError { Busy, Io(std::io::Error) }
impl StateFile {
    pub fn lock(path: &Path) -> Result<Self, LockError>;     // create if missing; File::try_lock; Busy if held elsewhere
    pub fn load(&mut self) -> State;                         // missing/corrupt -> fresh State with new device_id
    pub fn save(&mut self, state: &State) -> std::io::Result<()>; // rewrite through the same locked handle
}
// State's Debug must not print access_token.

// identity.rs
pub const CLIENT: &str = "mpvfin";
pub fn device_name() -> String;                              // COMPUTERNAME, else HOSTNAME, else "mpvfin"
pub fn new_device_id() -> String;                            // 32 lowercase hex chars from std RandomState hashing (no extra crate)
pub fn auth_header(device_name: &str, device_id: &str, token: Option<&Secret>) -> Secret; // header VALUE
pub fn media_auth_header(token: &Secret) -> Secret;          // full LINE: Authorization: MediaBrowser Token="…"
```

`auth_header` value format, with each value percent-encoded (everything
except ASCII alphanumerics and `-._~`):

```
MediaBrowser Client="mpvfin", Device="<name>", DeviceId="<id>", Version="<CARGO_PKG_VERSION>"[, Token="<token>"]
```

- [ ] **Step 1: Write the failing tests** (in each module's `#[cfg(test)]`)
  - `parses_server_from_script_opts`: `"# c\n server = https://h/jf/ \nother=1"` yields server `https://h/jf`.
  - `missing_server_is_an_error`: `""` gives `Err` mentioning `server`.
  - `base_url_rejects_non_http_schemes`: `ftp://h` gives `Err`.
  - `join_preserves_path_prefix`: `BaseUrl::parse("https://h/jf/")?.join("/Sessions/Playing")` is `https://h/jf/Sessions/Playing`.
  - `websocket_url_maps_scheme_and_keeps_prefix`: `http://h:8096` gives `ws://h:8096/socket?ApiKey=t&deviceId=d`, and `https://h/jf` gives `wss://h/jf/socket?ApiKey=t&deviceId=d`.
  - `auth_header_percent_encodes_values`: device `Living "Room", PC` gives `Device="Living%20%22Room%22%2C%20PC"`. A token is appended only when given.
  - `media_auth_header_has_token_only`: `Authorization: MediaBrowser Token="tok"`.
  - `new_device_id_is_32_hex_and_unique`: two calls differ, and both match `^[0-9a-f]{32}$` (checked by hand, no regex crate).
  - `state_round_trips_through_locked_file`: save, drop, re-lock and load give the same `State`.
  - `corrupt_state_gets_fresh_device_id`: `not json` in the file gives a `State` with a 32-hex ID and no token.
  - `second_lock_is_busy_while_first_held`: in a temp dir, `lock` twice gives `Err(LockError::Busy)` the second time. After dropping the first, a third `lock` succeeds.
  - `state_debug_hides_token`: `{:?}` of a `State` holding token `tok` does not contain `tok`.

- [ ] **Step 2: Run the tests to verify they fail**

  Run: `cargo test -p mpvfin-jellyfin`
  Expected: FAIL (unresolved items).

- [ ] **Step 3: Implement the interfaces above** in the listed files.

  For temp dirs in tests, use `std::env::temp_dir()` with a unique
  subfolder. Do not add `tempfile`.

- [ ] **Step 4: Run the tests to verify they pass**

  Run: `cargo test -p mpvfin-jellyfin`
  Expected: PASS.

- [ ] **Step 5: Commit**

  `git add Cargo.toml crates/mpvfin-jellyfin && git commit -m "jellyfin: config, locked state file, identity and URLs"`

---

### Task 4: Inbound messages and playback reports

**Files:**
- Create: `crates/mpvfin-jellyfin/src/wire.rs`
- Create: `crates/mpvfin-jellyfin/src/report.rs`
- Modify: `docs/jellyfin-protocol.md` (add the queue, track, toggle and
  display-message commands that were verified)

**Interfaces:**

```rust
// wire.rs
pub enum Inbound {
    Receiver(ReceiverEvent),
    DisplayMessage { text: String, duration: Duration },
    ForceKeepAlive(Duration),
    Ignored,
}
pub fn parse_inbound(text: &str) -> Inbound;     // never panics; unknown types/fields -> Ignored / tolerated
pub fn ticks_to_duration(ticks: i64) -> Duration; // negative -> ZERO; 1 tick = 100 ns
pub fn duration_to_ticks(d: Duration) -> i64;

// report.rs
pub fn started_body(s: &PlaybackSnapshot) -> serde_json::Value;
pub fn progress_body(s: &PlaybackSnapshot) -> serde_json::Value;
pub fn stopped_body(s: &PlaybackSnapshot, failed: bool) -> serde_json::Value;
```

**Mappings to pin:**
- `MessageType: "Play"` uses these `Data` fields:
  - `ItemIds` (empty means `Ignored`);
  - `PlayCommand`: `PlayNow` means `Replace`, `PlayNext` means `Next`, `PlayLast` means `Last`, anything else is `Ignored`;
  - `StartIndex` (null means 0);
  - `StartPositionTicks` (null means 0);
  - `AudioStreamIndex` and `SubtitleStreamIndex` become the `selection`.
- `MessageType: "Playstate"`, using `Data.Command` and `SeekPositionTicks`:
  - `Pause`, `Unpause` and `PlayPause` map to `RemotePause`, `RemoteResume` and `RemoteTogglePause`.
  - `Stop` maps to `RemoteStop`.
  - `NextTrack` and `PreviousTrack` map to `RemoteNext` and `RemotePrevious`.
  - `Seek` maps to `RemoteSeek`.
  - Anything else is `Ignored`.
- `MessageType: "GeneralCommand"`, with `Data.Name` and string `Data.Arguments`:
  - `SetVolume{Volume}` maps to `RemoteSetVolume`, clamped to 0..=100.
  - `Mute`, `Unmute` and `ToggleMute` map to `RemoteSetMuted(true)`, `RemoteSetMuted(false)` and `RemoteToggleMute`.
  - `SetAudioStreamIndex{Index}` and `SetSubtitleStreamIndex{Index}` map to the matching remote events.
  - `DisplayMessage{Header,Text,TimeoutMs}` maps to `DisplayMessage`. The text is `"{Header}\n{Text}"`, or just `Text` when the header is empty. The duration is `TimeoutMs`, defaulting to 5000 ms.
  - A malformed argument gives `Ignored`.
- `MessageType: "ForceKeepAlive"` with numeric `Data` seconds maps to `ForceKeepAlive`.
- Report bodies, using exact keys:
  - Start and progress send `ItemId`, `MediaSourceId`, `PlaySessionId`, `PlayMethod` (`"DirectPlay"` / `"DirectStream"` / `"Transcode"`), `PositionTicks`, `IsPaused`, `IsMuted`, `VolumeLevel`, `AudioStreamIndex` (omitted when `None`), `SubtitleStreamIndex` (`-1` when `None`) and `CanSeek: true`.
  - Stopped sends `ItemId`, `MediaSourceId`, `PlaySessionId`, `PositionTicks` and `Failed`.

- [ ] **Step 1: Write the failing tests**

  Use hand-written JSON strings. These are original fixtures; don't copy
  any from elsewhere.
  - `parses_play_now_with_start_index_ticks_and_selection`: `PlayNow`, `[a,b,c]`, `StartIndex 1`, `StartPositionTicks 300000000`, `AudioStreamIndex 2` give `Receiver(RemotePlay(QueueRequest{Replace, start_index 1, 30 s, selection{Some(2), None}}))`.
  - `play_next_and_play_last_map_to_queue_modes`.
  - `unsupported_play_commands_and_empty_items_are_ignored`: `PlayShuffle` gives `Ignored`, and `ItemIds: []` gives `Ignored`.
  - `parses_playstate_commands`: a table over all seven commands. `Seek` with `SeekPositionTicks 600000000` gives `RemoteSeek(60 s)`.
  - `parses_general_commands_with_string_arguments`: `SetVolume "55"` gives 55, `"500"` gives 100, and `"abc"` gives `Ignored`. Also cover `Mute`, `Unmute`, `ToggleMute`, `SetAudioStreamIndex "2"` and `SetSubtitleStreamIndex "-1"`.
  - `display_message_joins_header_and_text`.
  - `force_keep_alive_sets_interval`: `Data 60` gives `ForceKeepAlive(60 s)`.
  - `unknown_messages_and_fields_are_tolerated`: `{"MessageType":"Sessions","Data":[]}` gives `Ignored`. A `Play` message with an extra unknown field still parses. `"not json"` gives `Ignored`.
  - `ticks_convert_both_ways`: `ticks_to_duration(-5)` is `ZERO`, and 30 s is 300000000 ticks.
  - `start_report_body_matches_snapshot`: compare the exact `serde_json::json!` for a snapshot at 30 s, paused, volume 40, audio `Some(2)`, subtitle `None` (expect `SubtitleStreamIndex: -1` and `PositionTicks: 300000000`).
  - `stopped_report_marks_failure`.

- [ ] **Step 2: Run the tests to verify they fail**

  Run: `cargo test -p mpvfin-jellyfin wire report`
  Expected: FAIL.

- [ ] **Step 3: Implement `parse_inbound`, the tick helpers and the three body builders.**

  Parse into `serde_json::Value` or tolerant serde structs, whichever is
  shorter. Never `unwrap` on wire data.

- [ ] **Step 4: Run the tests to verify they pass**

  Run: `cargo test -p mpvfin-jellyfin`
  Expected: PASS.

- [ ] **Step 5: Commit**

  `git add crates/mpvfin-jellyfin docs/jellyfin-protocol.md && git commit -m "jellyfin: parse remote commands and build playback reports"`

---

### Task 5: Capabilities and playback negotiation

**Files:**
- Create: `crates/mpvfin-jellyfin/src/negotiate.rs`
- Modify: `docs/jellyfin-protocol.md` (capabilities body, PlaybackInfo
  body, stream URL, subtitle delivery)

**Interfaces:**

```rust
pub fn capabilities_body() -> serde_json::Value;
pub fn playback_info_body(user_id: &str) -> serde_json::Value;
pub fn display_title(item: &serde_json::Value) -> String;
pub fn resolve_media(
    base: &BaseUrl, item_id: &str, item: &serde_json::Value,
    playback_info: &serde_json::Value, auth_header: Secret,
) -> Result<ResolvedMedia, String>;   // Err text is user-facing, credential-free
```

**Values to pin:**
- Capabilities:
  - `PlayableMediaTypes: ["Video"]`
  - `SupportedCommands: ["SetVolume","Mute","Unmute","ToggleMute","SetAudioStreamIndex","SetSubtitleStreamIndex","DisplayMessage"]`
  - `SupportsMediaControl: true`
  - `SupportsPersistentIdentifier: true`
  - `DeviceProfile`, which has:
    - `Name: "mpvfin"`;
    - `MaxStreamingBitrate` and `MaxStaticBitrate` both `1000000000`;
    - `DirectPlayProfiles: [{"Type":"Video"}]`;
    - `TranscodingProfiles: []`;
    - `SubtitleProfiles` with both an `Embed` and an `External` entry for
      each of `srt, subrip, ass, ssa, vtt, webvtt, sub, pgs, pgssub, dvdsub, dvbsub, ttml`.
- PlaybackInfo body:
  - `UserId`
  - `DeviceProfile` (same as above)
  - `IsPlayback: true`
  - `AutoOpenLiveStream: false`
  - `EnableDirectPlay: true`
  - `EnableDirectStream: false`
  - `EnableTranscoding: false`
  - `MaxStreamingBitrate: 1000000000`
- Source choice:
  - Take the first entry in `MediaSources` with `SupportsDirectPlay == true`,
    `RequiresOpening != true` and `IsRemote != true`.
  - If there is none, return `Err("This item can't be played directly; the server would need to transcode it")`.
  - A non-null `ErrorCode` gives `Err("Jellyfin refused playback: {ErrorCode}")`.
- URIs:
  - The stream URI is `base.join("/Videos/{item_id}/stream?static=true&mediaSourceId={source.Id}")`.
  - Each external subtitle URI is `base.join(DeliveryUrl)` with every
    `ApiKey` or `api_key` query parameter removed (case-insensitive),
    keeping the other parameters.
- `TrackMap`:
  - `audio` holds the `Index` of each `Type=="Audio"` stream, in ascending
    `Index` order.
  - `subtitles` holds the `Type=="Subtitle"` streams: first the embedded
    ones (`IsExternal != true`) in ascending `Index` order, then the
    external ones with a `DeliveryUrl`, in ascending `Index` order.
  - `external_subtitles` follows the same external order.
- Title:
  - An episode becomes `"{SeriesName} S{season:02}E{episode:02} - {Name}"`.
  - An item with a `ProductionYear` becomes `"{Name} ({ProductionYear})"`.
  - Otherwise the title is `Name`.

- [ ] **Step 1: Write the failing tests**
  - `capabilities_advertise_only_implemented_commands`: exact `SupportedCommands` array and `PlayableMediaTypes`.
  - `playback_info_request_disables_transcoding`.
  - `resolves_direct_play_source_to_static_stream_url`.
  - `track_map_orders_embedded_then_external`: streams `[video 0, audio 1, audio 2, sub 3, sub 4, sub 5 external with DeliveryUrl]` give audio `[1,2]`, subtitles `[3,4,5]`, and one external URL.
  - `delivery_url_credentials_are_stripped`: `/Videos/x/y/Subtitles/5/0/Stream.srt?ApiKey=SECRET&foo=1` gives `{base}/Videos/x/y/Subtitles/5/0/Stream.srt?foo=1`. The lowercase `api_key` form is stripped too, and neither result contains `SECRET`.
  - `rejects_sources_needing_transcode_opening_or_remote`: one case each for `SupportsDirectPlay:false`, `RequiresOpening:true`, `IsRemote:true` and `ErrorCode:"NotAllowed"`.
  - `title_formats_episodes_movies_and_fallback`: `Test Show S01E02 - Pilot`, `X (2020)`, and `Name`.

- [ ] **Step 2: Run the tests to verify they fail**

  Run: `cargo test -p mpvfin-jellyfin negotiate`
  Expected: FAIL.

- [ ] **Step 3: Implement the four functions.**

- [ ] **Step 4: Run the tests to verify they pass**

  Run: `cargo test -p mpvfin-jellyfin`
  Expected: PASS.

- [ ] **Step 5: Verify against the real server**

  With the Task 1 server up, use `Invoke-RestMethod` to:
  1. authenticate as admin;
  2. POST `capabilities_body()` (copy the JSON) to `/Sessions/Capabilities/Full`;
  3. POST `playback_info_body` for `S01E01`.

  Expected: S01E01's PlaybackInfo has `SupportsDirectPlay: true`, two embedded
  subtitle streams, and one external (`fr`) subtitle with a `DeliveryUrl`.
  If any field name differs from the above, fix the code and tests to match
  the server.

  Record the response shape in `docs/jellyfin-protocol.md`, including
  whether `DeliveryUrl` carries an API key.

- [ ] **Step 6: Commit**

  `git add crates/mpvfin-jellyfin docs/jellyfin-protocol.md && git commit -m "jellyfin: capabilities and direct-play negotiation"`

---

### Task 6: HTTP client and session supervisor

**Files:**
- Modify: `crates/mpvfin-jellyfin/Cargo.toml`. Add
  `ureq = { version = "3", features = ["json", "platform-verifier"] }` and
  `tungstenite` with `rustls-tls-native-roots`. Configure features so the
  only rustls crypto provider compiled is `ring`.
- Create: `crates/mpvfin-jellyfin/src/client.rs`
- Create: `crates/mpvfin-jellyfin/src/session.rs`
- Modify: `crates/mpvfin-jellyfin/src/lib.rs` (public API below)
- Modify: `docs/jellyfin-protocol.md` (Quick Connect flow, as verified)

**Interfaces:**

```rust
// lib.rs (public)
pub struct Paths { pub config_file: PathBuf, pub state_file: PathBuf }
pub enum SessionEvent { Receiver(ReceiverEvent), Osd { text: String, duration: Duration }, Log(String) }
pub type Sink = Arc<dyn Fn(SessionEvent) + Send + Sync>;
pub enum Report { Started(PlaybackSnapshot), Progress(PlaybackSnapshot), Stopped { snapshot: PlaybackSnapshot, failed: bool } }
pub enum Job { Resolve { generation: Generation, item_id: String }, Report(Report) }
pub fn start(paths: Paths, sink: Sink) -> SessionHandle;
impl SessionHandle {
    pub fn submit(&self, job: Job);                 // never blocks
    pub fn shutdown(self, flush: Duration);         // drain jobs until `flush` elapses, close socket, return
}

// client.rs
pub enum HttpError { Unauthorized, Status(u16), Transport(String) } // Transport text must not contain URLs with credentials
pub struct Client { /* ureq::Agent, BaseUrl, header value */ }    // connect timeout 5 s, global timeout 10 s
pub fn reconnect_delay(attempt: u32, jitter: f64) -> Duration;   // min(60, 2^attempt) s × (0.75 + 0.5·jitter), jitter in [0,1]
```

**Supervisor behaviour** (one thread for the session, one FIFO worker
thread for jobs):

1. **Read config.**
   - On failure, log `[mpvfin] no server configured in <path>` and show the
     OSD `mpvfin: set server=… in script-opts/mpvfin.conf` for 5 s, once.
   - Then idle until shutdown.
2. **Lock the state file.** On `Busy`, log
   `[mpvfin] another mpv window is the Jellyfin cast target; waiting` once
   and retry every 5 s.
3. **Load the state.**
   - If `state.server` differs from the config, drop the token and user ID
     but keep `device_id`.
   - If there is no token, run Quick Connect:
     1. `GET /QuickConnect/Enabled`. If false, show the OSD
        `mpvfin: Quick Connect is disabled on this Jellyfin server` and
        stop here.
     2. `POST /QuickConnect/Initiate`, then log
        `[mpvfin] Quick Connect code: <code>`.
     3. Show the OSD
        `Jellyfin Quick Connect code: <code>\nApprove it in another Jellyfin app under Settings → Quick Connect`,
        refreshing it every 5 s with a 6 s duration.
     4. Poll `GET /QuickConnect/Connect?secret=…` every 3 s.
     5. Once authenticated, `POST /Users/AuthenticateWithQuickConnect` and
        save `user_id`, `access_token` and `server`.
     6. If the secret expires, start over.
4. **Register.**
   - `POST /Sessions/Capabilities/Full` with `capabilities_body()`.
   - On `Unauthorized`, clear the token, save, show the OSD
     `mpvfin: Jellyfin login expired, starting Quick Connect`, and go to 3.
5. **Connect the WebSocket.**
   - Connect to `base.websocket(token, device_id)`.
   - Log `[mpvfin] connected to Jellyfin as <device name>`.
   - Set a 1 s read timeout on the underlying TCP stream, so the loop can
     check the stop flag and send `{"MessageType":"KeepAlive"}` every
     ForceKeepAlive/2 (30 s until told otherwise).
   - Send each `parse_inbound` result to the sink as follows:
     - `Receiver(e)` becomes `SessionEvent::Receiver(e)`.
     - `DisplayMessage` becomes `SessionEvent::Osd`.
6. **Reconnect.** On socket or registration errors, log one line (no URL),
   sleep `reconnect_delay(attempt, jitter)` in stop-aware 1 s slices, and go
   to 4. Reset `attempt` after a connection that lasted 60 s or more.

**Job worker:**
- `Resolve` fetches the item with `GET /Items/{id}?userId={user_id}` and
  then the PlaybackInfo. It calls `resolve_media` with
  `media_auth_header(token)`, and sends either `MediaResolved` or
  `MediaResolutionFailed` with the job's generation.
- `Report` posts to `/Sessions/Playing`, `/Sessions/Playing/Progress` or
  `/Sessions/Playing/Stopped`.
- Jobs that arrive with no credentials are dropped.
- Jitter comes from `std::hash::RandomState`. Do not add a `rand` crate.

- [ ] **Step 1: Write the failing tests**
  - `reconnect_delay_doubles_caps_and_jitters`: `(0, 0.0)` gives 750 ms, `(1, 1.0)` gives 2.5 s, `(10, 0.5)` gives 60 s, and `(40, 1.0)` gives 75 s without overflow.
  - `unauthorized_status_is_classified`: a `classify_status(401)` helper gives `HttpError::Unauthorized`, and `500` gives `Status(500)`.
  - `transport_errors_do_not_leak_credentials`: build a `HttpError::Transport` from an error message containing `ApiKey=SECRET` through the same sanitizer the client uses. The resulting text does not contain `SECRET`.

- [ ] **Step 2: Run the tests to verify they fail**

  Run: `cargo test -p mpvfin-jellyfin client`
  Expected: FAIL.

- [ ] **Step 3: Implement `client.rs`, `session.rs` and the `lib.rs` API above.**

- [ ] **Step 4: Run the tests to verify they pass**

  Run: `cargo test -p mpvfin-jellyfin`
  Expected: PASS.

- [ ] **Step 5: Check the crypto provider**

  Run: `cargo tree -p mpvfin-jellyfin -i aws-lc-sys`
  Expected: an error saying the package was not found, which means
  aws-lc-sys is not in the dependency tree.

- [ ] **Step 6: Commit**

  `git add Cargo.lock crates/mpvfin-jellyfin docs/jellyfin-protocol.md && git commit -m "jellyfin: HTTP client, Quick Connect login and session supervisor"`

  The end-to-end behaviour is exercised in Task 9.

---

### Task 7: Plugin FFI, safe wrapper and pure adapter

**Files:**
- Create: `crates/mpvfin-cplugin/src/ffi.rs`. Move the existing raw types
  and `pfn_mpv_*` slots here.
- Create: `crates/mpvfin-cplugin/src/mpv.rs`
- Create: `crates/mpvfin-cplugin/src/adapter.rs`
- Modify: `crates/mpvfin-cplugin/Cargo.toml` (add `mpvfin-core`,
  `mpvfin-jellyfin`)
- Modify: `crates/mpvfin-cplugin/src/lib.rs` (modules only, for now)

**Interfaces:**
- `ffi.rs` exports `pfn_mpv_*` slots for these functions:
  - `wait_event`, `wakeup`, `command`, `command_string`
  - `command_ret`, `free_node_contents`
  - `set_property_string`, `get_property_string`, `free`
  - `observe_property`, `hook_add`, `hook_continue`
  - `client_name`, `error_string`
- It also defines repr(C) prefixes for `mpv_event`, `mpv_event_property`,
  `mpv_event_start_file`, `mpv_event_end_file` and `mpv_event_hook`, plus
  `mpv_node` (only the string case is read).
- Copy the event IDs, end-file reasons and format constants from mpv's
  public `client.h`. Do not guess them.
- Missing required slots make the plugin log once and return `-1`, as it
  does today.

```rust
// mpv.rs (safe wrapper used only on the plugin thread)
pub struct Mpv { /* handle + fn pointers */ }
impl Mpv {
    pub fn command(&self, args: &[&str]) -> Result<(), String>;
    pub fn command_string_result(&self, args: &[&str]) -> Option<String>; // mpv_command_ret; used for expand-path
    pub fn set_property(&self, name: &str, value: &str) -> Result<(), String>;
    pub fn get_property(&self, name: &str) -> Option<String>;
    pub fn observe(&self, name: &str, format: c_int);
    pub fn hook_add(&self, name: &str, priority: c_int);
    pub fn hook_continue(&self, id: u64);
    pub fn log(&self, line: &str);              // print-text "[mpvfin] " + escape_osd(line)
    pub fn show_text(&self, text: &str, duration: Duration); // show-text escape_osd(text)
    pub fn waker(&self) -> Waker;
}
#[derive(Clone)] pub struct Waker { /* Arc<Mutex<Option<(SendPtr, MpvWakeupFn)>>> */ }
impl Waker { pub fn wake(&self); pub fn disarm(&self); } // after disarm, wake() is a no-op; wakers never outlive the handle

// adapter.rs (pure)
pub const ENTRY_SCHEME: &str = "mpvfin://";
pub fn entry_uri(item_id: &str) -> String;
pub fn item_id_from_entry(uri: &str) -> Option<&str>;
pub fn queue_commands(req: &QueueRequest) -> Vec<Vec<String>>;
pub fn hook_properties(media: &ResolvedMedia) -> Vec<(&'static str, String)>;
pub fn after_load_commands(media: &ResolvedMedia, selection: &StreamSelection) -> Vec<Vec<String>>;
pub fn track_command(media: &ResolvedMedia, kind: TrackKind, jellyfin_index: i32) -> Option<Vec<String>>;
pub enum TrackKind { Audio, Subtitle }
pub fn end_reason(reason: c_int, error: Option<String>) -> EndReason;  // EOF→Eof; STOP|QUIT|REDIRECT→Stopped; ERROR→Failed
pub fn format_seconds(d: Duration) -> String;                          // "30.000"
pub fn escape_osd(text: &str) -> String;                               // "$" -> "$$": show-text/print-text expand ${…}, so a remote DisplayMessage could otherwise print ${file-local-options/http-header-fields}
```

**`queue_commands` algorithm** (mpv `loadfile <url> <flags> <index> <options>`):
- **Replace**, with start `s`:
  1. `["loadfile", uri(items[s]), "replace", "-1", opts]`, where `opts` is
     `"start=<secs>,resume-playback=no"`, or just `"resume-playback=no"`
     when the start position is zero.
  2. Each item after `s`, in order:
     `["loadfile", uri, "append", "-1", "resume-playback=no"]`.
  3. Each item before `s`, in order, with `i` = 0, 1, …:
     `["loadfile", uri, "insert-at", i, "resume-playback=no"]`.
- **Next:** iterate the items in reverse with `"insert-next"`.
- **Last:** iterate in order with `"append"`.

**Values to pin:**
- `hook_properties` returns these pairs, in order:
  - `("stream-open-filename", uri)`
  - `("file-local-options/http-header-fields", auth_header.expose())`
  - `("file-local-options/force-media-title", title)`
  - `("file-local-options/save-position-on-quit", "no")`
- `after_load_commands`:
  - First, `["sub-add", url, "auto"]` for each external subtitle, in order.
  - Then `["set", "file-local-options/aid", id]` if the selection has an
    audio index that maps.
  - Then `["set", "file-local-options/sid", id_or_"no"]` if the selection
    has a subtitle index. `-1` gives `"no"`.
- `track_command` uses the same `set file-local-options/aid|sid` form.

- [ ] **Step 1: Write the failing tests** (in `adapter.rs` and `mpv.rs`)
  - `replace_queue_loads_start_item_then_rest_in_order`: for `[a,b,c,d]` with start 2 at 30 s, assert the exact four command vectors: `c` replace with `start=30.000,resume-playback=no`, `d` append, `a` insert-at 0, `b` insert-at 1.
  - `replace_from_zero_has_no_start_option`.
  - `play_next_inserts_in_reverse_and_play_last_appends`.
  - `hook_sets_stream_header_title_and_disables_watch_later`: exact pairs.
  - `after_load_adds_external_subs_then_applies_selection`: with tracks `{audio [1,2], subtitles [3,4,5]}`, one external URL, and selection `{2, 5}`, expect `[sub-add url auto]`, then aid `2`, then sid `3`. A selection of `{None, -1}` gives only the `sub-add` and sid `no`.
  - `entries_are_recognised_and_foreign_paths_are_not`: `mpvfin://abc` gives `Some("abc")`. `C:\v.mkv` and `https://x/y` give `None`.
  - `end_file_reasons_map_to_end_reasons`.
  - `osd_text_escapes_property_expansion`: `escape_osd("a ${x} $")` is `"a $${x} $$"`.
  - `waker_is_inert_after_disarm`: inject a counting `extern "C" fn` as the wakeup function. One `wake` counts 1. After `disarm`, `wake` still counts 1.

- [ ] **Step 2: Run the tests to verify they fail**

  Run: `cargo test -p mpvfin-cplugin`
  Expected: FAIL.

- [ ] **Step 3: Implement `ffi.rs`, `mpv.rs` and `adapter.rs`.**

- [ ] **Step 4: Run the tests and lints**

  Run: `cargo test --workspace` and
  `cargo clippy --workspace --all-targets -- -D warnings`
  Expected: PASS.

- [ ] **Step 5: Commit**

  `git add -A crates Cargo.toml Cargo.lock && git commit -m "cplugin: mpv FFI wrapper and pure playlist/hook adapter"`

---

### Task 8: Plugin runtime loop

**Files:**
- Create: `crates/mpvfin-cplugin/src/runtime.rs`
- Modify: `crates/mpvfin-cplugin/src/lib.rs` (entry point calls `runtime::run`; remove the "plugin loaded" OSD)
- Modify: `scripts/smoke-test-cplugin.ps1`

**Interfaces:**
- Consumes:
  - `Mpv`, `Waker` and the adapter functions (Task 7);
  - `mpvfin_jellyfin::{start, Paths, SessionEvent, Job, Report}` (Task 6);
  - `Receiver` (Task 2).
- Produces: `pub fn run(mpv: Mpv) -> c_int`.

**Behaviour:**

1. **Startup.**
   1. Log `[mpvfin] loaded`.
   2. Observe `pause`, `volume`, `mute`, `time-pos`, `aid` and `sid`.
   3. `hook_add("on_load", 50)`.
   4. Build `Paths` from
      `command_string_result(&["expand-path", "~~/script-opts/mpvfin.conf"])`
      and the same call for `~~/script-opts/mpvfin-state.json`.
   5. Start the session. Its sink pushes into an `mpsc::Sender` and then
      calls `waker.wake()`.
2. **Loop.** `wait_event(timeout)` until the next 10 s `ProgressTick`, then
   handle the event:
   - **`START_FILE`:** remember the entry ID as the current entry.
   - **`HOOK on_load`:** read `stream-open-filename`.
     - If `item_id_from_entry` matches, feed
       `PlayerOpening{token{current entry, hook id}, item_id}`.
     - Otherwise call `hook_continue` immediately.
   - **`FILE_LOADED`:** feed `PlayerFileLoaded` for the current entry's
     generation, run `after_load_commands`, then feed the latest
     pause, volume, mute and track values as `Player*Changed` events.
   - **`END_FILE`:** map the event's entry ID to its generation and feed
     `PlayerEnded{end_reason(...)}`.
   - **`PLAYBACK_RESTART`:** feed `PlayerPositionChanged{seek_completed: true}`.
   - **`PROPERTY_CHANGE`:** feed the matching `Player*Changed` event for the
     current generation. Volume is clamped to 0..=100. `aid` and `sid` are
     mapped through `TrackMap`, and `"no"` means a subtitle of `None`.
   - **`SHUTDOWN`:** break out of the loop.

   File-loaded, end-file and property events for an entry with no known
   generation (a foreign file, or a failed resolution) are ignored.
3. **After every mpv event or wakeup**, drain the channel:
   - `Receiver(e)` goes to `Receiver::handle`, and the resulting actions are
     executed.
   - `Osd` goes to `show_text`.
   - `Log` goes to `mpv.log`.
4. **Action execution:**
   - `QueueItems`, `PlaylistNext`/`PlaylistPrevious`, `SetPaused`, `Seek`
     (`seek <secs> absolute`), `SetVolume`, `SetMuted`, the track actions
     and `StopPlayer` (`stop`) become mpv commands or property sets.
   - `ResolveMedia` records `entry_id → generation` and submits
     `Job::Resolve`.
   - `ContinueLoad` sets `hook_properties`, stores the media and selection
     for this generation, then calls `hook_continue`.
   - `AbortLoad` calls `hook_continue`.
   - `Report*` becomes `Job::Report`, with `failed` set when the reason is
     `Failed`.
   - `PlaybackFailed` shows the OSD `mpvfin: <message>` for 5 s.
   - A failed mpv command is logged, never unwrapped.
5. **Shutdown.**
   1. Feed `Shutdown` and execute its actions.
   2. `waker.disarm()`.
   3. `session.shutdown(Duration::from_secs(2))`.
   4. Return 0.

- [ ] **Step 1: Update the smoke test to expect the new behaviour (failing first)**
  - `smoke-test-cplugin.ps1` keeps `--no-config --length=0.1` and requires
    output containing both `[mpvfin] loaded` and
    `[mpvfin] no server configured`, plus exit code 0.
  - Its runtime is measured, and it fails if mpv takes over 5 s.

- [ ] **Step 2: Run it against the current build to verify it fails**

  Run:
  ```powershell
  cargo build -p mpvfin-cplugin
  ./scripts/smoke-test-cplugin.ps1 -MpvPath .tools/mpv/mpv.com
  ```
  Expected: FAIL (sentinel missing).

- [ ] **Step 3: Implement `runtime.rs` as described** and wire the entry point to it.

- [ ] **Step 4: Run the smoke test and the workspace checks**

  Run: the two commands from Step 2, then `cargo test --workspace` and the
  clippy command.
  Expected: `mpvfin C-plugin smoke test passed.` All tests pass and clippy
  is clean.

- [ ] **Step 5: Commit**

  `git add -A crates scripts && git commit -m "cplugin: event loop wiring reducer, mpv and Jellyfin session"`

---

### Task 9: End-to-end tests and docs

**Files:**
- Create: `crates/mpvfin-e2e/Cargo.toml` (`publish = false`; dev-only use of
  `ureq`, `serde_json`, `mpvfin-jellyfin` for `State`)
- Create: `crates/mpvfin-e2e/src/lib.rs` (harness)
- Create: `crates/mpvfin-e2e/tests/cast.rs`
- Modify: `Cargo.toml` (add the member)
- Modify: `README.md`. Cover installing (copy `mpvfin.dll` to `scripts/`),
  `script-opts/mpvfin.conf` with `server=`, starting
  `mpv --idle --force-window`, Quick Connect, uninstalling (the three
  files), and running the e2e tests.
- Modify: `docs/architecture.md`. Describe the plugin-only design, the
  playlist-entry and `on_load` flow, the threads and the waker. Remove the
  agent/IPC backend.

**Harness interfaces** (`src/lib.rs`):

```rust
pub struct Server;   // MPVFIN_E2E_SERVER, default http://127.0.0.1:8096; logs in as admin/mpvfin-test
impl Server {
    pub fn connect() -> Self;
    pub fn episodes(&self) -> Vec<(String, String)>;                 // (id, name) sorted S01E01..E03
    pub fn device_login(&self, device_id: &str) -> State;           // AuthenticateByName as admin with that DeviceId; State{device_id, server: Some(BaseUrl::as_str), user_id, access_token}
    pub fn mpvfin_session(&self, device_id: &str) -> Option<serde_json::Value>; // GET /Sessions?deviceId=…
    pub fn play(&self, session: &str, items: &[&str], start_index: usize, start: Duration);
    pub fn playstate(&self, session: &str, command: &str, seek: Option<Duration>);
    pub fn command(&self, session: &str, name: &str, args: serde_json::Value);
    pub fn authorize_quick_connect(&self, code: &str);
    pub fn resume_position(&self, item_id: &str) -> Duration;       // admin UserData.PlaybackPositionTicks
}
pub struct Player;   // temp config dir + spawned mpv
impl Player {
    pub fn launch(opts: PlayerOptions) -> Self;  // MPVFIN_E2E_MPV default .tools/mpv/mpv.com; target/debug/mpvfin.dll
    pub fn get(&self, property: &str) -> serde_json::Value;          // JSON IPC over \\.\pipe\mpvfin-e2e-<unique>
    pub fn command(&self, args: &[&str]);
    pub fn wait_log(&self, needle: &str, timeout: Duration) -> String; // returns the matching line
    pub fn quit(self) -> Duration;                                    // sends quit; returns time until process exit
    pub fn config_dir(&self) -> &Path;
    pub fn log_text(&self) -> String;
}
pub struct PlayerOptions { pub server: String, pub state: State, pub mpv_conf: String, pub config_dir: Option<PathBuf> } // State = mpvfin_jellyfin::State, always pre-seeded so every test knows its (fresh, random) device ID
pub fn poll_until<T>(timeout: Duration, f: impl FnMut() -> Option<T>) -> T; // panics with a clear message on timeout
```

**`Player::launch` behaviour:**
- It writes `mpv.conf` (the test's content plus the line
  `# e2e-sentinel`), an `input.conf` with one comment line,
  `script-opts/mpvfin.conf`, and `script-opts/mpvfin-state.json` from
  `opts.state`.
- Each test uses a fresh random device ID. "Preseeded" below means the
  state comes from `device_login`. "No token" means
  `State{device_id, server: None, user_id: None, access_token: None}`.
- It runs mpv with:
  ```
  --config-dir=<dir> --idle=yes --vo=null --ao=null --script=<dll>
  --input-ipc-server=<pipe> --log-file=<dir>.log --msg-level=all=debug
  ```
  The log file sits outside the config dir.
- Every test is `#[ignore = "needs ./scripts/test-server.ps1 -Action up"]`.

- [ ] **Step 1: Write the e2e tests** (`tests/cast.rs`)
  - `quick_connect_login_registers_session`: no token.
    - `wait_log("Quick Connect code:")`, parse the six digits, then `authorize_quick_connect`.
    - `poll_until` `mpvfin_session(device_id)` exists with `SupportsMediaControl == true`.
    - After `quit`, the state file holds a token.
  - `remote_play_pause_seek_stop_report_back`: preseeded.
    - Play `[E01]` from 30 s. Within 20 s, `NowPlayingItem.Id == E01` and `PlayState.PositionTicks >= 30 s`.
    - `Pause` gives `IsPaused == true` and mpv `pause == true`.
    - `Seek` to 60 s gives a position within 58–65 s.
    - `SetVolume "40"` gives mpv `volume == 40` and a reported `VolumeLevel == 40`.
    - `Stop` clears `NowPlayingItem`.
  - `queue_start_index_and_next_previous`:
    - Play `[E01,E02,E03]` with start index 1. mpv `playlist-count == 3` and E02 is now playing.
    - `NextTrack` plays E03, then `PreviousTrack` plays E02.
  - `local_pause_and_seek_are_reported`: during playback, IPC `set pause yes` gives a reported `IsPaused`. IPC `seek 90 absolute` gives a reported position within 88–95 s.
  - `remote_track_changes_select_mpv_tracks`: on E01:
    - Use the audio and subtitle indices from E01's PlaybackInfo. Don't hardcode them.
    - Selecting the second audio gives mpv `aid == 2`.
    - Selecting the external `fr` subtitle gives mpv `sid == 3`, and that entry in `track-list` has `external == true`.
    - `SetSubtitleStreamIndex -1` gives `sid == false`.
  - `config_dir_untouched_and_token_absent_from_log`:
    - Run with `mpv_conf = "save-position-on-quit=yes\nwatch-later-dir=~~/watch_later\n"`.
    - Snapshot every file under the config dir except `script-opts/mpvfin-state.json`.
    - Cast E01 (which loads the external subtitle) and let it play for 5 s, then `quit`.
    - The snapshot is byte-identical. There is no `watch_later` directory, or it is empty.
    - `log_text()` contains neither the token nor `ApiKey=` nor `api_key=`, case-insensitive.
  - `second_window_stays_dormant_until_first_closes`:
    - Launch two players sharing one `config_dir`.
    - The second logs `another mpv window is the Jellyfin cast target`. Its log has no `connected to Jellyfin` while the first is alive.
    - Quit the first. Within 20 s the second logs `connected to Jellyfin`, and `mpvfin_session(device_id)` exists again.
  - `mpv_stays_responsive_and_quits_when_server_unreachable` (Review Focus 1):
    - Run with `server = http://10.255.255.1:8096` and a preseeded token.
    - After 2 s, IPC `get pause` answers within 1 s, and `quit()` returns in under 5 s.
  - `quitting_mid_playback_saves_resume_position` (Review Focus 2): play E02, seek to 45 s, wait until it is reported, then `quit()`. `resume_position(E02)` is within 40–50 s.
  - `local_file_during_cast_reports_stop_and_stays_untouched` (Review Focus 3):
    - While E01 plays, IPC `loadfile av://lavfi:testsrc=size=64x64:rate=1 replace`.
    - Within 10 s, `NowPlayingItem` is gone.
    - mpv `force-media-title` is empty and `http-header-fields` is an empty list.
  - `rejected_token_falls_back_to_quick_connect` (Review Focus 4): preseed with `access_token` set to `"invalid"` (server and user ID from `device_login`). Within 20 s, `wait_log("Quick Connect code:")`.

- [ ] **Step 2: Run them against the server**

  Run:
  ```powershell
  ./scripts/test-server.ps1 -Action up
  cargo build -p mpvfin-cplugin
  cargo test -p mpvfin-e2e -- --ignored --test-threads=1
  ```
  Expected: all 11 pass. Fix product code, not test expectations, unless a
  test contradicts the spec or verified server behaviour. Record any such
  case in `docs/jellyfin-protocol.md`.

- [ ] **Step 3: Update `README.md` and `docs/architecture.md`** as listed under Files.

- [ ] **Step 4: Run the full verification**

  Run:
  ```
  cargo test --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  ./scripts/smoke-test-cplugin.ps1 -MpvPath .tools/mpv/mpv.com
  ```
  Then run the e2e command again. Expected: everything passes.

- [ ] **Step 5: Commit**

  `git add -A crates Cargo.toml Cargo.lock README.md docs && git commit -m "test: end-to-end casting tests against Docker Jellyfin; update docs"`
