# mpvfin Plan 3: Watch Together

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user's mpv join Jellyfin SyncPlay groups alongside official clients, with a join/create/leave menu, group-wide pause, seek and queue control, and drift correction.

**Architecture:** The reducer gains a group state. While grouped, the group's queue is mirrored into mpv's playlist as `mpvfin://<itemId>/<playlistItemId>` entries, items load paused and hold at their end, and every local or remote control becomes a SyncPlay request whose broadcast is what actually gets applied. A dedicated worker sends group requests so a slow report can't delay `Ready`. Drift correction lives in a small pure module driven by the plugin thread.

**Tech Stack:** Rust 2024 (1.97), mpv client API, Jellyfin 10.11 SyncPlay HTTP and WebSocket, uosc's menu API, mpv's console `get-input`.

**Spec:** `docs/superpowers/specs/2026-11-08-mpvfin-design.md`, goal 3. The wire contract is the research file `.superpowers/research/syncplay.md`, committed by Task 1 as `docs/jellyfin-syncplay.md`. UI facts are in `.superpowers/research/ui-prompts.md`.

**Not in this plan:** the username/password login fallback. Measured: anything typed into mpv's console is written to `--log-file` at every log level and shown on screen. That decision is waiting on the owner.

## Global Constraints

- Edition 2024, `rust-version = "1.97"`, existing workspace lints. `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` pass at the end of every task.
- Work happens in the worktree `Z:\mpvfin\.worktrees\plan-2-skip`, branch `plan-3-syncplay`. It has no `.tools`: set `MPVFIN_E2E_MPV=Z:\mpvfin\.tools\mpv\mpv.com`, pass `-MpvPath Z:\mpvfin\.tools\mpv\mpv.com` to the smoke test, and do not create a junction.
- Clean room (`docs/clean-room.md`): never read jellyfin-mpv-shim. Do not read jellyfin-web source either; behave from the server contract and black-box checks.
- Never reset the Docker test server (`-Action reset`). Every mpv run uses a temp `--config-dir` and `--vo=null --ao=null`. Leave alone mpv processes you didn't start.
- The plugin sets no global option except `volume`, `mute` and the plain unpause. Everything else per item goes through `file-local-options/…` or `loadfile` options. Speed corrections use `file-local-options/speed` and are put back.
- mpv never receives the token. No network I/O or blocking wait on mpv's event thread. No panic crosses the C boundary. Every `on_load` hook continues exactly once.
- Outside a group, behaviour is exactly what it is today.
- HTTP success means "accepted": group state changes come only from the socket.
- Pids must be current: a stale `NextItem`/`PreviousItem` parks the group in Waiting (contract section 8).
- Seeks issued for the group are exact: the server's Ready tolerance is 500 ms.

## Review Focus

1. **A member joins or leaves mid-playback:** mpv pauses at the frozen position and resumes with the group; nothing is reported twice. Test: `partner_joining_pauses_and_resumes_mpv`.
2. **The user's local pause, resume or seek while grouped:** reaches the group and is applied from the broadcast, with no echo loop. Test: `local_pause_and_seek_reach_the_group`.
3. **The socket drops while grouped:** membership is restored by rejoining. Test: `reconnect_rejoins_the_group`.
4. **End of the last queue item:** the group stops instead of hanging in Playing. Test: `end_of_queue_stops_the_group`.
5. **Opening a local file while grouped:** mpv leaves the group rather than dragging others along. Test: `local_file_leaves_the_group`.

---

### Task 1: Contract in the repo and a scripted partner

**Files:**
- Create: `docs/jellyfin-syncplay.md` (from `.superpowers/research/syncplay.md`, dropping section 14)
- Modify: `scripts/test-server.ps1` (ensure user `partner` / `mpvfin-test` exists, idempotently)
- Modify: `crates/mpvfin-e2e/Cargo.toml`, `crates/mpvfin-e2e/src/lib.rs`
- Create: `crates/mpvfin-e2e/tests/partner.rs`

**Interfaces** (harness):

```rust
pub struct Partner { /* HTTP client as `partner`, WebSocket reader thread */ }
impl Partner {
    pub fn connect(server: &Server) -> Self;          // fresh device id; posts capabilities; opens socket
    pub fn set_auto_ready(&self, on: bool);           // default true: answers item-changing PlayQueue and Seek with Ready at the given position, pinging first, like a well-behaved client
    pub fn create_group(&self, name: &str) -> String; // waits for GroupJoined; returns group id
    pub fn join(&self, group_id: &str);
    pub fn leave(&self);
    pub fn set_new_queue(&self, item_ids: &[&str], index: usize, start: Duration);
    pub fn pause(&self); pub fn unpause(&self); pub fn stop(&self);
    pub fn seek(&self, position: Duration);
    pub fn wait_update(&self, kind: &str, timeout: Duration) -> Value;  // next SyncPlayGroupUpdate of that Type
    pub fn wait_command(&self, kind: &str, timeout: Duration) -> Value; // next SyncPlayCommand with that Command
    pub fn last_queue(&self) -> Value;                // latest PlayQueue Data
}
```

- [ ] **Step 1: Write `crates/mpvfin-e2e/tests/partner.rs`**, `#[ignore]` like the others: `two_partners_can_start_a_group`, where one creates, the other joins, the first sets a queue of E01, and both reach `StateUpdate` `Playing` and receive `Unpause`.
- [ ] **Step 2: Run it to see it fail** (compile error).
- [ ] **Step 3: Implement** the harness, the contract doc and the `partner` user.
- [ ] **Step 4: Run** `./scripts/test-server.ps1 -Action up`, the new test, then workspace tests, clippy and fmt. Expected: pass.
- [ ] **Step 5: Commit** `test: SyncPlay contract and a scripted group partner`

---

### Task 2: Wire format

**Files:**
- Create: `crates/mpvfin-jellyfin/src/syncplay.rs`
- Modify: `crates/mpvfin-jellyfin/src/wire.rs`, `src/lib.rs`
- Modify: `crates/mpvfin-core/src/lib.rs` (types only; add `mod group;` in Task 4)

**Interfaces** (core; `Clone, Debug, Eq, PartialEq`):

```rust
pub struct GroupEntry { pub item_id: String, pub playlist_item_id: String }
pub enum QueueReason { NewPlaylist, SetCurrentItem, NextItem, PreviousItem, RemoveItems, Other }
pub struct GroupQueue { pub reason: QueueReason, pub last_update_ms: i64, pub entries: Vec<GroupEntry>, pub playing_index: Option<usize>, pub start_position: Duration, pub is_playing: bool }
pub enum GroupCommandKind { Unpause, Pause, Seek, Stop }
pub struct GroupCommand { pub kind: GroupCommandKind, pub playlist_item_id: String, pub position: Duration, pub delay_ms: i64 } // delay_ms = When − server now at receipt; may be negative
pub struct GroupSummary { pub id: String, pub name: String, pub members: usize, pub state: String }
pub enum GroupRequest {
    New { name: String }, Join { group_id: String }, Leave,
    SetNewQueue { item_ids: Vec<String>, index: usize, start: Duration }, Queue { item_ids: Vec<String>, next: bool },
    SetPlaylistItem { pid: String }, NextItem { pid: String }, PreviousItem { pid: String },
    Unpause, Pause, Stop, Seek { position: Duration },
    Ready { position: Duration, is_playing: bool, pid: String }, Buffering { position: Duration, pid: String },
}
```

**Interfaces** (`syncplay.rs`):

```rust
pub const ZERO_ID: &str = "00000000000000000000000000000000";
pub enum GroupUpdate { Joined { group_id: String }, Left, Gone { message: &'static str }, Queue(GroupQueue), Command { command: GroupCommand, when_ms: i64 }, Ignored }
pub fn parse_group_message(message_type: &str, data: &Value) -> GroupUpdate;
pub fn parse_utc_ms(text: &str) -> Option<i64>;                                  // ISO 8601 with Z, 0–7+ fractional digits
pub fn format_utc(ms: i64) -> String;                                            // ms precision, Z
pub fn request_path_and_body(request: &GroupRequest, server_now_ms: i64) -> (&'static str, Value);
pub fn parse_groups(body: &Value) -> Vec<GroupSummary>;
pub fn clock_sample(t0_ms: i64, t1_ms: i64, t2_ms: i64, t3_ms: i64) -> (i64, i64); // (offset, rtt)
```

**Rules:** `GroupDoesNotExist` gives `Gone { "That group no longer exists" }`; `NotInGroup` gives `Gone { "You are no longer in that group" }`; `LibraryAccessDenied` gives `Gone { "You don't have access to everything in that group" }`. `GroupLeft` gives `Left`. `UserJoined`, `UserLeft`, `StateUpdate` give `Ignored`. `PlayingItemIndex` of `-1` gives `None`. `QueueReason` maps the five named reasons; `Queue`, `QueueNext`, `MoveItem`, `RepeatMode`, `ShuffleMode` give `Other`. `Ready`/`Buffering` bodies carry `When` from `server_now_ms`, `PositionTicks`, `IsPlaying` (Buffering sends `true`) and `PlaylistItemId`. Clamp nothing here.

- [ ] **Step 1: Write the failing tests** with original JSON modelled on the contract's shapes: `parses_every_update_type`, `parses_play_queue_with_no_current_item`, `parses_commands`, `parse_utc_handles_variable_fractions` (0, 3, 6, 7 digits and `…Z`), `parse_utc_rejects_garbage`, `format_utc_round_trips`, `every_request_maps_to_its_path_and_body` (a table for all 15 variants), `clock_sample_matches_ntp_formula`, `parse_groups_tolerates_bad_entries`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement**; in `wire.rs`, route `SyncPlayCommand` and `SyncPlayGroupUpdate` to a new `Inbound::Group(GroupUpdate)`.
- [ ] **Step 4: Run** workspace tests, clippy, fmt. Expected: pass.
- [ ] **Step 5: Commit** `jellyfin: SyncPlay wire format`

---

### Task 3: Session: clock, group worker, rejoin

**Files:** Modify `crates/mpvfin-jellyfin/src/session.rs`, `src/lib.rs`.

**Interfaces:**

```rust
pub enum Job { /* existing */, Group(GroupRequest), ListGroups }
pub enum SessionEvent { /* existing */, GroupList(Vec<GroupSummary>) }
```

**Rules:**
- `GroupRequest` and `ListGroups` run on their own FIFO thread, never behind reports. `Ready` and `Buffering` stamp `When` from the clock at send time. A failed request logs one line (no URL).
- Clock: at `GroupJoined`, take 5 `/GetUtcTime` samples, keep the lowest RTT, and store the offset. Repeat every 60 s while in a group. Clock work runs off the socket thread.
- `Ping`: send RTT/2 in ms right after each clock batch, and only while in a group.
- When a `SyncPlayCommand` arrives: `delay_ms = when_ms − (local_now_ms + offset)`, delivered as `ReceiverEvent::GroupCommandReceived`.
- Membership tracking: remember the group id on `Joined`, clear on `Left`/`Gone`. After the socket reconnects and capabilities are reposted, if a group id is remembered, `POST /SyncPlay/Join` it.
- Delivery to the reducer: `GroupJoined{group_id}`, `GroupLeft`, `GroupGone{message}`, `GroupQueueChanged(GroupQueue)`, `GroupCommandReceived(GroupCommand)` (those events are added in Task 4; send them from here once it exists, so add the core variants in this task as ignored arms).
- `shutdown` stays bounded; the group thread follows the same stop flag.

- [ ] **Step 1: Write the failing tests:** `command_delay_uses_the_clock_offset` (pure helper) and a stub-server session test `reconnect_rejoins_the_remembered_group`, using the stub pattern from `crates/mpvfin-jellyfin/src/session.rs` tests.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.** Confirm one real `New`/`Join`/`Ready` round trip against the Docker server.
- [ ] **Step 4: Run** workspace tests, clippy, fmt. Expected: pass.
- [ ] **Step 5: Commit** `jellyfin: group requests, clock sync and rejoin`

---

### Task 4: Reducer: membership, queue, commands

**Files:** Create `crates/mpvfin-core/src/group.rs` (`impl Receiver` for group handling, `struct Group`); modify `crates/mpvfin-core/src/lib.rs`.

**Interfaces:**

```rust
// ReceiverEvent
GroupCreate { name: String }, GroupJoin { group_id: String }, GroupLeave,
GroupJoined { group_id: String }, GroupLeft, GroupGone { message: String },
GroupQueueChanged(GroupQueue), GroupCommandReceived(GroupCommand),
GroupResumeDue { generation: Generation },
// PlayerOpening gains `playlist_item_id: Option<String>`

// ReceiverAction
Group(GroupRequest),
GroupLoad { entries: Vec<GroupEntry>, index: usize },            // replace mpv's playlist, start at index
GroupSyncPlaylist { entries: Vec<GroupEntry>, index: usize },    // re-mirror around the current entry
GroupPublishQueue { position: Duration },                        // runtime reads mpv's playlist and sends SetNewQueue
ScheduleResume { generation: Generation, after: Duration }, CancelResume,
DriftAnchor { generation: Generation, position: Option<Duration> }, // Some = anchor now; None = stop correcting
ShowMessage { text: String },
// ContinueLoad gains `group: Option<GroupLoadOptions>` where GroupLoadOptions { start: Duration }
```

`pub fn group_id(&self) -> Option<&str>` on `Receiver`.

**Rules the tests pin.** `Group` holds `id`, `entries`, `current_pid`, `last_update_ms`, `creating`, `awaiting_ready`, `expected_paused` (true on join), `own_seek`, `start`.
- `GroupCreate`/`GroupJoin`/`GroupLeave` emit `Group(New/Join/Leave)`; `GroupCreate` marks `creating`.
- `GroupJoined`: creates the group. If `creating` and an item is `Playing`: emit `GroupPublishQueue { position }`.
- `GroupLeft`, `GroupGone`: drop the group, `CancelResume`, `DriftAnchor{None}`. `GroupGone` also `ShowMessage`. Local playback continues.
- `GroupQueueChanged`: ignore if `last_update_ms` is older than the last applied (equal is accepted). With `playing_index: None`: store, no load. Let `new_pid` be the playing entry's pid. Item-changing means `NewPlaylist`, `SetCurrentItem`, `NextItem`, `PreviousItem`, or `RemoveItems` with a different pid.
  - Item-changing and `new_pid` is not what's loaded: `GroupLoad`, set `awaiting_ready`, `expected_paused = !is_playing` and `start`.
  - Item-changing and `new_pid` already loaded (a join, or our own navigation): set `awaiting_ready`. If loaded and the position differs from `start_position` by over 100 ms: `SetPaused(true)` if needed, then `Seek` with `own_seek`. Else emit `Group(Ready)` at once with `is_playing: false` (no seek will settle).
  - `RemoveItems` with a new pid and `is_playing`: load, no Ready expected.
  - Other reasons: `GroupSyncPlaylist`.
- While grouped, `PlayerOpening` with the current pid resolves normally, with `group: Some({start})` on its `ContinueLoad`.
- Settling: when `PlayerPositionChanged { seek_completed: true }` arrives and `awaiting_ready`: clear it and emit `Group(Ready { position, is_playing: false, pid })`.
- `GroupCommandReceived`, ignored if `playlist_item_id` is neither the current pid nor `ZERO_ID`:
  - `Stop`: ignored if `ZERO_ID` or nothing's loaded. Otherwise `StopPlayer`, `CancelResume`, `DriftAnchor{None}`.
  - `Pause`: `expected_paused = true`, `CancelResume`, `DriftAnchor{None}`, `SetPaused(true)` if not paused, then `Seek` with `own_seek` if over 100 ms away.
  - `Seek`: `expected_paused = true`, `CancelResume`, `DriftAnchor{None}`, `SetPaused(true)` if not paused, `Seek` with `own_seek`, `awaiting_ready`.
  - `Unpause`: `expected_paused = false`. `delay_ms > 0`: `Seek` with `own_seek` if over 100 ms from `position`, then `ScheduleResume { after: delay }`. `delay_ms <= 0`: target is `position − delay_ms`; `Seek` with `own_seek` if over 100 ms away, `SetPaused(false)`, `DriftAnchor{Some(target)}`.
- `GroupResumeDue` for the current generation with `expected_paused == false`: `SetPaused(false)`, `DriftAnchor{Some(position)}`.
- Ending an item (`terminate_current`) keeps the group.

Ask for `Group(Ready)` only with a loaded item and a valid pid.

- [ ] **Step 1: Write the failing tests**, one per rule, named for the behaviour: `joined_after_create_publishes_the_playing_queue`, `joined_without_create_publishes_nothing`, `older_queue_updates_are_ignored`, `equal_last_update_is_applied`, `new_item_loads_paused_and_readies_on_settle`, `already_loaded_item_seeks_then_readies`, `already_positioned_item_readies_at_once`, `empty_queue_loads_nothing`, `other_reasons_only_resync_the_playlist`, `idle_stop_is_ignored`, `stop_stops_the_loaded_item`, `pause_seeks_to_the_given_position`, `seek_readies_after_it_lands`, `future_unpause_is_scheduled`, `late_unpause_catches_up`, `resume_due_starts_playback`, `commands_for_another_item_are_ignored`, `leaving_keeps_playback`, `gone_shows_why`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.** Add ignoring arms to `runtime.rs` with `// wired in Task 7`.
- [ ] **Step 4: Run** workspace tests, clippy, fmt. Expected: pass.
- [ ] **Step 5: Commit** `core: follow a SyncPlay group`

---

### Task 5: Reducer: controls become requests

**Files:** Modify `crates/mpvfin-core/src/group.rs`, `src/lib.rs`.

**Interfaces:**

```rust
// ReceiverEvent
PlayerReachedEnd { generation: Generation },
PlayerBuffering { generation: Generation, buffering: bool },
PlayerOpenedForeign,
DriftSeek { generation: Generation, position: Duration },
```

**Rules, all only while grouped:**
- `PlayerPauseChanged` differing from `expected_paused` is the user's: paused → `Group(Pause)`, `expected_paused = true`. Unpaused → `Group(Unpause)` and `SetPaused(true)` until the broadcast arrives. The existing `ReportProgress` stays.
- `PlayerPositionChanged { seek_completed: true }` with `own_seek` set clears it. Without it and without `awaiting_ready`: the user sought, so `Group(Seek { position })`.
- `RemotePlay`: `Replace` → `Group(SetNewQueue)`, clamped to the queue length; `Next`/`Last` → `Group(Queue)`. `RemotePause`/`RemoteResume`/`RemoteTogglePause`/`RemoteSeek`/`RemoteStop` → the matching request. `RemoteNext`/`RemotePrevious` → `NextItem`/`PreviousItem` with the current pid. Volume, mute and tracks stay local.
- `PlayerOpening` for an entry with a different queue pid means the user navigated: `Group(SetPlaylistItem { pid })`, and set `current_pid` and `awaiting_ready`.
- `PlayerReachedEnd` for the current generation, at most once per item: `NextItem { pid }` if a later entry exists, else `Group(Stop)`.
- `PlayerBuffering { true }` while `expected_paused == false` and not `awaiting_ready`: `Group(Buffering)`, once. `{ false }` after that: `Group(Ready { position, is_playing: true, pid })`.
- `SkipRequested` in a group: `MarkPlayed` and `HideSkipButton` as today, then `Group(Seek { end })`; for `to_end_of_file`, the `PlayerReachedEnd` behaviour instead.
- `DriftSeek`: `Seek` with `own_seek`.
- `PlayerOpenedForeign`: `Group(Leave)`.
- `SetNewQueue`'s start is clamped by the caller, which has only the item IDs: if unknown, send it as given.

- [ ] **Step 1: Write the failing tests:** `local_pause_requests_a_group_pause`, `local_resume_requests_unpause_and_holds`, `commanded_pause_is_not_echoed`, `local_seek_requests_a_group_seek`, `own_seek_is_not_echoed`, `remote_play_replaces_the_group_queue`, `remote_queue_modes_map`, `remote_controls_map_to_requests`, `volume_stays_local`, `navigating_mpvs_playlist_sets_the_group_item`, `end_of_item_requests_next_once`, `end_of_queue_stops_the_group`, `buffering_is_reported_once_and_cleared`, `buffering_while_loading_is_not_reported`, `skip_in_group_is_a_group_seek`, `skip_to_end_in_group_requests_next`, `foreign_file_leaves_the_group`, `outside_a_group_controls_are_unchanged`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** workspace tests, clippy, fmt. Expected: pass.
- [ ] **Step 5: Commit** `core: route controls through the group while grouped`

---

### Task 6: Drift correction

**Files:** Create `crates/mpvfin-cplugin/src/drift.rs`; modify `crates/mpvfin-cplugin/src/lib.rs`.

**Interfaces:**

```rust
pub enum DriftAction { None, SetSpeed(f64), RestoreSpeed(f64), Seek(Duration) }
pub struct DriftCorrector { /* anchor (position, Instant), correction end, cooldown end, base speed */ }
impl DriftCorrector {
    pub fn new() -> Self;
    pub fn anchor(&mut self, position: Option<Duration>, now: Instant) -> DriftAction; // None stops and restores if correcting
    pub fn observe(&mut self, position: Duration, speed: f64, now: Instant) -> DriftAction;
}
```

**Values to pin:** Expected position is `anchor.position + (now − anchor.at)`. Drift is `expected − actual`.
- Below **120 ms** absolute: `None`.
- 120 ms up to **2 s**: `SetSpeed(1.05)` when behind, `SetSpeed(0.95)` when ahead, for `|drift| / 0.05` s, capped at **30 s**. Then `RestoreSpeed(base)` and a **3 s** cooldown.
- Over 2 s: `Seek(expected)`, and a **5 s** cooldown.
- No corrections during a cooldown or a correction. No corrections when `speed` was not 1.0 before one started (the user changed it): the base is recorded from the observed speed, and anything else leaves playback alone.
- An `anchor` call resets everything, restoring speed first if correcting.

- [ ] **Step 1: Write the failing tests** with injected `Instant`s: `small_drift_is_left_alone`, `behind_speeds_up_ahead_slows_down`, `correction_lasts_drift_over_rate_then_restores`, `correction_duration_is_capped`, `large_drift_seeks`, `cooldown_blocks_a_repeat`, `user_speed_is_left_alone`, `anchor_none_restores_speed`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** workspace tests, clippy, fmt. Expected: pass.
- [ ] **Step 5: Commit** `cplugin: drift corrector`

---

### Task 7: Plugin wiring

**Files:** Modify `crates/mpvfin-cplugin/src/adapter.rs`, `runtime.rs`, possibly `mpv.rs`.

**Interfaces (adapter):**

```rust
pub fn group_entry_uri(item_id: &str, pid: &str) -> String;            // mpvfin://<itemId>/<pid>
pub fn entry_from_uri(uri: &str) -> Option<(&str, Option<&str>)>;      // replaces item_id_from_entry
pub fn group_load_commands(entries: &[GroupEntry], index: usize) -> Vec<Vec<String>>;
pub fn group_sync_commands(entries: &[GroupEntry], index: usize) -> Vec<Vec<String>>;
pub fn group_hook_properties(start: Duration) -> Vec<(&'static str, String)>;
```

**Values to pin:**
- `group_load_commands`: same shape as Replace in `queue_commands`, without `start=`, using `group_entry_uri` and `resume-playback=no`.
- `group_sync_commands`: `["playlist-clear"]`, then later entries with `append`, then earlier entries with `insert-at i`.
- `group_hook_properties`: `("file-local-options/pause","yes")`, `("file-local-options/keep-open","always")`, `("file-local-options/start", <secs>)`, added to the normal hook properties.

**Runtime behaviour:**
- `HOOK on_load`: a `mpvfin://` entry feeds `PlayerOpening` with its pid. A foreign entry while `receiver.group_id()` is set also feeds `PlayerOpenedForeign`; its hook continues at once, as today.
- `Group(request)` submits `Job::Group`. `GroupLoad`/`GroupSyncPlaylist` run their commands. `GroupPublishQueue` reads `playlist/count` and `playlist/N/filename`, keeps the `mpvfin://` entries, and submits `SetNewQueue` with the current index and position.
- `ScheduleResume` sets a deadline in the `wait_event` timeout; when due it feeds `GroupResumeDue`. `CancelResume` clears it, as does a later schedule.
- Observe `eof-reached`, `paused-for-cache` and `speed`. `eof-reached` turning true feeds `PlayerReachedEnd`; `paused-for-cache` feeds `PlayerBuffering`.
- `DriftAnchor` calls `DriftCorrector::anchor` and applies the result. After every `time-pos` for the current generation, call `observe` and apply: `SetSpeed`/`RestoreSpeed` set `file-local-options/speed`, `Seek` feeds `DriftSeek`.
- `ShowMessage` shows the OSD for 5 s.
- Keep key-binding parsing general: `is_skip_press` becomes a `key_press_name(args) -> Option<&str>` returning the binding name on a press.

- [ ] **Step 1: Write the failing tests** in `adapter.rs`: `group_uris_round_trip`, `plain_entries_have_no_pid`, `group_load_commands_match_the_queue_shape`, `group_sync_keeps_the_current_entry`, `group_hook_holds_at_the_end_and_loads_paused`, `key_press_names_are_reported_only_on_press`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.** Verify live that `file-local-options/keep-open` set in `on_load` holds at the end with `eof-reached` true and doesn't advance, and that `playlist-clear` leaves the current entry playing.
- [ ] **Step 4: Run** workspace tests, clippy, fmt and the smoke test. Expected: pass.
- [ ] **Step 5: Commit** `cplugin: wire group playback into mpv`

---

### Task 8: The watch-together menu

**Files:** Create `crates/mpvfin-cplugin/src/group_menu.rs` (pure); modify `runtime.rs`, `lib.rs`, `README.md`.

**Interfaces:**

```rust
pub enum MenuChoice { Join(String), Create, Leave }
pub struct MenuItem { pub title: String, pub choice: MenuChoice }
pub fn menu_items(groups: &[GroupSummary], current: Option<&str>) -> Vec<MenuItem>;
pub fn uosc_open_command(items: &[MenuItem], client: &str) -> Vec<String>;
pub fn uosc_close_command() -> Vec<String>;
pub fn uosc_probe_command() -> Vec<String>;
pub fn console_open_command(items: &[MenuItem], client: &str, handler: &str, legacy: bool) -> Vec<String>;
pub fn console_is_legacy(mpv_version: &str) -> bool;
pub fn parse_uosc_event(json: &str) -> Option<UoscEvent>;      // Activate(value) | Close
pub fn parse_console_event(args: &[String]) -> Option<ConsoleEvent>; // Submit(index) | Closed
```

**Values to pin** (see `.superpowers/research/ui-prompts.md`):
- `menu_items`: one `Join` per group except the current one, titled `<name> (<n> members)`, then `Create new group`, then `Leave group` when `current` is set.
- uosc: `open-menu` JSON with `type: "mpvfin-wt"`, `title: "Watch together"`, `callback: [client, "wt-menu"]`, items with `title` and a `value` that indexes `items`. On activate, send `close-menu mpvfin-wt`. The probe is `script-message-to uosc close-menu mpvfin-probe`.
- Console (current form): `script-message-to console get-input {"client_name":client,"handler_id":"input-event/mpvfin-wt","prompt":"Watch together:","items":[…]}`. Replies arrive as `["input-event/mpvfin-wt","submit","[N]"]`, 1-based, then `closed`.
- `console_is_legacy`: true for 0.39 through plain 0.41.0 (version strings with no commit suffix past `0.41.0`), false for anything newer. Legacy form: `get-input <client> <json without client_name/handler_id>`, replies `["input-event", event, json]`.

**Runtime behaviour:** the `watch-together` press submits `Job::ListGroups`. On `GroupList`: probe uosc and open its menu; else open the console; else show the OSD text `mpvfin: watch together needs uosc or mpv's console`. Remember the items until the choice. `Join` feeds `GroupJoin`, `Leave` feeds `GroupLeave`, `Create` feeds `GroupCreate { name: "<device name>" }`.

- [ ] **Step 1: Write the failing tests** for each pure function with the exact strings above.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement**, and add a README section: binding `script-binding mpvfin/watch-together`, adding it to a uosc menu, and what each menu entry does.
- [ ] **Step 4: Run** workspace tests, clippy, fmt, smoke. Expected: pass.
- [ ] **Step 5: Commit** `cplugin: watch-together menu`

---

### Task 9: End-to-end, a real browser, and docs

**Files:** Create `crates/mpvfin-e2e/tests/group.rs`; modify `docs/architecture.md`, `docs/jellyfin-protocol.md`, `README.md`.

- [ ] **Step 1: Write the tests** (`#[ignore]`, scripted `Partner`, played state cleared first):
  - `joining_a_playing_group_loads_paused_then_starts_together`: partner plays E01; mpv joins through `Job`-equivalent UI (open the console menu over IPC, `keypress` to choose); mpv reaches the group's position within 1 s and is unpaused.
  - `creating_a_group_while_playing_publishes_the_queue`.
  - `partner_joining_pauses_and_resumes_mpv`.
  - `partner_pause_seek_and_resume_are_followed`: positions within 300 ms of the commanded ones.
  - `unpause_starts_at_the_scheduled_time`.
  - `local_pause_and_seek_reach_the_group`: the partner receives the matching commands, and no echo loop follows within 2 s.
  - `next_and_previous_in_mpv_move_the_group`.
  - `cast_while_grouped_replaces_the_group_queue`.
  - `end_of_item_holds_and_requests_next`.
  - `end_of_queue_stops_the_group`.
  - `skip_in_a_group_seeks_everyone`.
  - `leaving_keeps_local_playback`.
  - `local_file_leaves_the_group`.
  - `reconnect_rejoins_the_group`: this one may be the stub-based session test from Task 3 if a real socket drop can't be forced; say which.
  - A menu test for uosc is not possible headless; the console route covers the flow.
- [ ] **Step 2: Run** the whole e2e suite twice with `MPVFIN_E2E_MPV` set. Expected: every test passes. Fix product code, not expectations, unless a test contradicts the spec or verified server behaviour.
- [ ] **Step 3: Real-client check.** With a headless browser (system Chrome or Edge via Playwright, whichever can decode H.264; put any scripts under `.superpowers/research/`, uncommitted), log in to the Docker server's web client as `partner`, create a group, play E01, and have mpv join. Confirm in each direction: pause, resume, seek. Confirm mpv starts when the browser starts, and record the measured offset. Record what you saw in `docs/jellyfin-syncplay.md`, answering its open questions where you can. If the browser can't run video headless, report that; don't fake it.
- [ ] **Step 4: Update docs.** Architecture: the group flow, the two workers, drift. README: watch together.
- [ ] **Step 5: Run** the full verification: workspace tests, clippy, fmt, smoke, e2e.
- [ ] **Step 6: Commit** `test: end-to-end watch together; document it`
