# mpvfin Plan 2: Skip Button

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show a clickable "Skip intro/recap/credits/preview" button with a hotkey during Jellyfin media segments, without taking any input away from the user the rest of the time.

**Architecture:** After playback negotiation the session fetches the item's media segments and sends them to the reducer. The reducer compares each confirmed position with those segments and emits show/hide/skip actions. The plugin draws the button with mpv's `osd-overlay` and enables temporary input sections only while the button is up.

**Tech Stack:** Rust 2024 (1.97), mpv client API, Jellyfin 10.11 `/MediaSegments` and `/UserPlayedItems`, the official Chapter Segments Provider plugin on the Docker test server.

**Spec:** `docs/superpowers/specs/2026-11-08-mpvfin-design.md`, goal 2 ("Skip button"). The group-seek behaviour in goal 3 belongs to plan 3.

## Global Constraints

- Edition 2024, `rust-version = "1.97"`, existing workspace lints. `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` pass at the end of every task.
- Work happens in the worktree `Z:\mpvfin\.worktrees\plan-2-skip`. It has no `.tools`: set `MPVFIN_E2E_MPV=Z:\mpvfin\.tools\mpv\mpv.com` for e2e runs and pass `-MpvPath Z:\mpvfin\.tools\mpv\mpv.com` to the smoke test. Do not create a junction to `.tools`.
- Clean room (`docs/clean-room.md`): never read, copy or adapt jellyfin-mpv-shim code.
- The plugin never writes `mpv.conf`/`input.conf` and adds no permanent key binding. `define-section` definitions are inert until enabled, and sections are enabled only while the button is on screen.
- The click only counts with the pointer over the button; every other click goes to uosc or mpv.
- The default hotkey is `ENTER` in a `default` section, so the user's `input.conf` binding wins and mpv's built-in binding does not. `script-binding <client>/skip` always works.
- mpv never receives the token. No network I/O or blocking wait on mpv's event thread. No panic crosses the C boundary. Every `on_load` hook continues exactly once.
- A missing, empty or failing segment response never affects playback. No segments means no button.
- Commercial segments get no button. No auto-skip option.
- OSD text appears only for things the user must act on; the button is the one addition.
- Never reset the Docker test server (`-Action reset`) in this plan: `up` must upgrade it in place.

## Review Focus

1. **Seeking back into a skipped intro:** the button returns. Test: `seeking_back_into_a_dismissed_segment_shows_it_again`.
2. **Resuming or casting into the middle of a segment:** the button shows and skip works. Test: `button_appears_in_intro_and_enter_skips_it` (casts from 8 s).
3. **Item changes while the button is up:** the hotkey and click are released, and Enter is mpv's again. Tests: `item_end_hides_the_button` and `enter_returns_to_mpv_after_the_segment`.
4. **Bad segment data** (end before start, unknown types, no runtime): ignored without panic. Test: `parse_segments_drops_bad_and_unwanted_entries`.
5. **Adjacent segments:** the old button is replaced, not stacked or stuck. Test: `adjacent_segments_swap_the_button`.

---

### Task 1: Test server produces real segments

**Files:**
- Modify: `scripts/test-server.ps1`
- Modify: `docs/jellyfin-protocol.md`

**Interfaces:**
- Produces segments on the Docker server. Plan tasks rely on these exact values.

| Episode | Chapter name | Start–end (s) | Expected segment |
|---|---|---|---|
| E01 | `Cold Open` | 0–5 | none |
| E01 | `Intro` | 5–20 | Intro |
| E01 | `Part 1` | 20–100 | none |
| E01 | `Credits` | 100–120 | Outro (to end of file) |
| E02 | `Recap` | 0–10 | Recap |
| E02 | `Part 1` | 10–50 | none |
| E02 | `Credits` | 50–70 | Outro |
| E02 | `Part 2` | 70–110 | none |
| E02 | `Preview` | 110–120 | Preview (to end of file) |
| E03 | none | | none |

- [ ] **Step 1: Extend `up`, idempotently and without a reset**
  1. Install `Chapter Segments Provider` (assembly GUID `698b6f3314ca49b59d79fc3c0ab941f5`, version `4.0.0.0`) through `POST /Packages/Installed/{name}` if `GET /Plugins` doesn't list it, then restart the container and wait for the server.
  2. If the marker `/media/.chapters-v2` is missing, regenerate E01 and E02 in place with chapters from ffmetadata, keeping every existing stream, then create the marker and run a library refresh. Keep the file names, so the item IDs and existing tests are unaffected as far as Jellyfin allows.
  3. Run the `TaskExtractMediaSegments` scheduled task (`/ScheduledTasks`, then `/ScheduledTasks/Running/{id}`) and poll until E01 and E02 return their segments.
  4. Finish by checking the table above against `GET /MediaSegments/{itemId}`, and throw if it differs. Keep the existing final line, `Test server ready: 3 episodes`.

  If the plugin's name patterns don't match a chapter name in the table, change the chapter name to one the plugin matches, and record it in the protocol doc and your report.

- [ ] **Step 2: Verify**

  Run `./scripts/test-server.ps1 -Action up` twice. Expected: the last line both times is `Test server ready: 3 episodes`, and the second run regenerates nothing.

- [ ] **Step 3: Record** the install call, the chapter patterns, the scan task and the `/MediaSegments` response shape in `docs/jellyfin-protocol.md`.

- [ ] **Step 4: Commit** `test: give the Docker server real media segments`

---

### Task 2: Reducer: segments and skipping

**Files:**
- Modify: `crates/mpvfin-core/src/lib.rs`

**Interfaces:** every new type derives `Clone, Debug, Eq, PartialEq` (`SegmentKind` also `Copy`).

```rust
pub enum SegmentKind { Intro, Recap, Outro, Preview }
pub struct Segment { pub kind: SegmentKind, pub start: Duration, pub end: Duration, pub to_end_of_file: bool }

// ReceiverEvent
SegmentsResolved { generation: Generation, segments: Vec<Segment> }, // sorted by start
SkipRequested,

// ReceiverAction
ShowSkipButton { generation: Generation, kind: SegmentKind },
HideSkipButton { generation: Generation },
SeekToEnd { generation: Generation },
MarkPlayed { generation: Generation, item_id: String },
```

**Rules the tests pin:** the receiver keeps `segments`, `active: Option<Segment>` and `dismissed: Option<Segment>`.
- `SegmentsResolved` for the current generation (`Loading` or `Playing`) stores the list; any other generation is ignored. While `Playing`, evaluate the current position at once.
- After every `PlayerPositionChanged` for the playing generation, find the first segment with `start <= position < end`:
  - If `dismissed` is set and differs from the found segment, clear it.
  - A found segment equal to `dismissed` counts as none.
  - If the result differs from `active`: emit `HideSkipButton` for the old one if any, then `ShowSkipButton` for the new one if any. These come after any `ReportProgress` the event produced.
- `SkipRequested` does nothing unless `Playing` with `active` set. Otherwise it emits in this order: `MarkPlayed` (only for `Outro`), `HideSkipButton`, then `SeekToEnd` if `to_end_of_file` or else `Seek { position: end }`. It moves `active` to `dismissed`.
- `terminate_current` (and so every path that ends an item) emits `HideSkipButton` first if `active` is set, and clears all three fields.

- [ ] **Step 1: Write the failing tests**
  - `segment_button_shows_on_entry_and_hides_on_exit`.
  - `segments_arriving_mid_segment_show_the_button_at_once`: `Playing` at 8 s, then `SegmentsResolved` with Intro 5–20 gives `ShowSkipButton{Intro}`.
  - `stale_segments_are_ignored`.
  - `skip_seeks_to_segment_end_and_hides`: exactly `[HideSkipButton, Seek{20 s}]`, with no `MarkPlayed` for Intro.
  - `skipping_credits_marks_played_first`: `[MarkPlayed, HideSkipButton, Seek]` for a mid-file Outro.
  - `skipping_a_segment_at_end_of_file_seeks_to_end`: `to_end_of_file` gives `SeekToEnd`.
  - `dismissed_segment_stays_hidden_while_the_seek_lands`: a position event still inside the segment emits nothing.
  - `seeking_back_into_a_dismissed_segment_shows_it_again`: leave the segment, then re-enter.
  - `adjacent_segments_swap_the_button`: Outro 90–110 and Preview 110–120; moving from 109 to 111 gives `[HideSkipButton, ShowSkipButton{Preview}]`.
  - `skip_outside_a_segment_does_nothing`.
  - `item_end_hides_the_button`: `PlayerEnded` with a button up gives `HideSkipButton` before `ReportStopped`; a later position event emits nothing.
  - `next_item_starts_without_old_segments`.

- [ ] **Step 2: Run them to verify they fail.** `cargo test -p mpvfin-core`: compile errors.

- [ ] **Step 3: Implement** in the existing single `handle` match, with a private helper for the position evaluation. The cplugin's action `match` is exhaustive, so add arms that ignore the four new actions for now, with `// wired in Task 4`.

- [ ] **Step 4: Run** `cargo test --workspace`, clippy and fmt. Expected: pass.

- [ ] **Step 5: Commit** `core: track media segments and handle skip requests`

---

### Task 3: Fetching segments and marking played

**Files:**
- Create: `crates/mpvfin-jellyfin/src/segments.rs`
- Modify: `crates/mpvfin-jellyfin/src/lib.rs`, `crates/mpvfin-jellyfin/src/session.rs`
- Modify: `docs/jellyfin-protocol.md`

**Interfaces:**

```rust
// segments.rs
pub const END_OF_FILE_TOLERANCE: Duration = Duration::from_secs(5);
pub fn segments_path(item_id: &str) -> String;
pub fn parse_segments(response: &serde_json::Value, runtime_ticks: Option<i64>) -> Vec<Segment>;

// lib.rs
pub enum Job { /* existing */, MarkPlayed { item_id: String } }
```

**Rules:**
- `segments_path` is `/MediaSegments/{percent-encoded id}?includeSegmentTypes=Intro&includeSegmentTypes=Recap&includeSegmentTypes=Outro&includeSegmentTypes=Preview`.
- `parse_segments` reads `Items[]` with `Type`, `StartTicks`, `EndTicks`. It keeps the four types, drops anything else and any entry with `end <= start` or missing ticks, and sorts by start. `to_end_of_file` is true when `runtime_ticks` is known and `end + END_OF_FILE_TOLERANCE >= runtime`.
- The job worker, after a successful `Resolve` has sent `MediaResolved`: `GET segments_path`, then send `SegmentsResolved { generation, segments }` using the item's `RunTimeTicks`. The start of playback never waits for it. On any error, log one line (no URL) and send nothing.
- `Job::MarkPlayed`: `POST /UserPlayedItems/{id}?userId={user_id}`. On failure, log one line. Dropped when logged out.

- [ ] **Step 1: Write the failing tests**
  - `segments_path_requests_only_the_four_types`, including percent-encoding of the id.
  - `parse_segments_maps_ticks_and_kinds`: exact `Segment` values, sorted by start.
  - `parse_segments_drops_bad_and_unwanted_entries`: `Commercial`, `Unknown`, `end <= start`, missing ticks, non-object items; `{}` gives empty.
  - `to_end_of_file_uses_the_tolerance`: runtime 120 s with end 115 s is true, end 114 s is false, and an unknown runtime is false.

- [ ] **Step 2: Run them to verify they fail.** `cargo test -p mpvfin-jellyfin segments`.

- [ ] **Step 3: Implement** `segments.rs` and the worker changes. Confirm both endpoints against the running Docker server and record them in the protocol doc.

- [ ] **Step 4: Run** `cargo test --workspace`, clippy and fmt. Expected: pass.

- [ ] **Step 5: Commit** `jellyfin: fetch media segments and mark items played`

---

### Task 4: The button in the plugin

**Files:**
- Create: `crates/mpvfin-cplugin/src/skip_button.rs` (pure)
- Modify: `crates/mpvfin-cplugin/src/ffi.rs`, `mpv.rs`, `runtime.rs`, `lib.rs`

**Interfaces:**

```rust
// skip_button.rs
pub struct Rect { pub x: i64, pub y: i64, pub w: i64, pub h: i64 }
impl Rect { pub fn contains(&self, x: i64, y: i64) -> bool; } // left/top inclusive, right/bottom exclusive
pub fn button_rect(osd_w: i64, osd_h: i64) -> Rect;
pub fn label(kind: SegmentKind) -> &'static str;
pub fn draw_command(kind: SegmentKind, osd_w: i64, osd_h: i64, hovered: bool) -> Vec<String>;
pub fn clear_command() -> Vec<String>;
pub fn define_section_commands(client: &str) -> Vec<Vec<String>>;
pub fn set_section_command(section: &str, enabled: bool) -> Vec<String>;
pub fn is_skip_press(args: &[String]) -> bool;
pub const KEY_SECTION: &str = "mpvfin-skip-key";
pub const CLICK_SECTION: &str = "mpvfin-skip-click";
```

**Values to pin:**
- `button_rect`: `h = max(36, osd_h * 6 / 100)`, `w = h * 4`, `x = osd_w - w - h`, `y = osd_h - h - osd_h * 14 / 100` (integer maths).
- `label`: `Skip intro`, `Skip recap`, `Skip credits`, `Skip preview`.
- `draw_command`: positional `["osd-overlay", "1", "ass-events", <data>, <osd_w>, <osd_h>, "0", "no", "no"]`. `<data>` is one ASS line with a filled rounded rectangle (`\p1` drawing) and a second with the centred label, font size `h / 2`, on a dark translucent fill; `hovered` makes the fill lighter.
- `clear_command`: `["osd-overlay", "1", "none", "", "0", "720", "0", "no", "no"]`.
- `define_section_commands`: `["define-section", KEY_SECTION, "ENTER script-binding <client>/skip\n", "default"]` and `["define-section", CLICK_SECTION, "MBTN_LEFT script-binding <client>/skip\n", "force"]`.
- `set_section_command`: `["enable-section", s]` or `["disable-section", s]`.
- `is_skip_press`: `args[0] == "key-binding"`, `args[1] == "skip"`, and `args[2]` starts with `d` or `p`. Repeats (`r`) and releases (`u`) are false.

**Runtime behaviour:**
- Startup: read the client name with `mpv_client_name`, run `define_section_commands`, and observe `osd-width` and `osd-height` (string format) and `mouse-pos` (`MPV_FORMAT_NONE`, used only as a change signal).
- `MPV_EVENT_CLIENT_MESSAGE` with `is_skip_press` feeds `ReceiverEvent::SkipRequested`.
- `ShowSkipButton`: remember the kind, enable `KEY_SECTION`, draw. `HideSkipButton`: disable both sections, clear the overlay, forget the kind.
- A missing or zero OSD size counts as 1280x720, so the hit test still works under `--vo=null`.
- While a button is up, a `mouse-pos` change reads `mouse-pos/x`, `mouse-pos/y` and `mouse-pos/hover`. The pointer is over the button when `hover` is true and `Rect::contains` holds. When that changes, enable or disable `CLICK_SECTION` and redraw. Nothing is read while no button is up. An OSD size change while a button is up redraws and rechecks the pointer.
- `SeekToEnd` runs `["seek", "100", "absolute-percent+exact"]`.
- `MarkPlayed` submits `Job::MarkPlayed`.

- [ ] **Step 1: Write the failing tests** in `skip_button.rs`
  - `button_rect_scales_with_the_osd`: 1280x720 gives `{1065, 577, 172, 43}`; 1920x1080 gives `{1600, 865, 256, 64}`; 640x360 gives `{460, 274, 144, 36}`.
  - `rect_contains_is_half_open`.
  - `labels_match_the_spec`.
  - `draw_command_is_positional_and_names_the_label`: the fixed args are exact, and `<data>` contains the label.
  - `hover_changes_the_drawn_fill`.
  - `clear_command_removes_the_overlay`.
  - `sections_use_default_for_keys_and_force_for_clicks`: exact commands for client `mpvfin`.
  - `only_presses_of_skip_count`: table of `d-`, `p-`, `r-`, `u-`, another binding name, another message, too few args.

- [ ] **Step 2: Run them to verify they fail.** `cargo test -p mpvfin-cplugin skip_button`.

- [ ] **Step 3: Implement** the module, the FFI (copy the event constant and `mpv_event_client_message` from mpv's public `client.h`) and the runtime wiring, replacing the Task 2 placeholder arms.

- [ ] **Step 4: Run** `cargo test --workspace`, clippy, fmt, build the DLL, and run the smoke test. Expected: pass.

- [ ] **Step 5: Commit** `cplugin: draw the skip button and handle its input`

---

### Task 5: End-to-end tests and docs

**Files:**
- Modify: `crates/mpvfin-e2e/src/lib.rs`, `crates/mpvfin-e2e/tests/cast.rs` (or a new `tests/skip.rs`)
- Modify: `README.md`, `docs/architecture.md`

**Harness changes:** `PlayerOptions` gains `default_bindings: bool` (false keeps today's `--input-default-bindings=no`; true omits that flag so `default` sections and mpv's built-ins are live) and `input_conf: String`, appended to the generated `input.conf`. Existing tests keep their behaviour.

- [ ] **Step 1: Write the tests.** All use `#[ignore]` like the others, `default_bindings: true` unless stated, and poll rather than sleep where a state can be observed.
  - `button_appears_in_intro_and_enter_skips_it`: cast E01 from 8 s; IPC `keypress ENTER`; `time-pos` lands in 20–24 s, and E01 is still playing.
  - `enter_returns_to_mpv_after_the_segment`: cast `[E01, E02]` from 30 s (no segment); `keypress ENTER` runs mpv's built-in next-file, so E02 plays.
  - `click_on_button_skips_and_click_elsewhere_does_not`: cast E01 from 8 s. IPC `mouse 10 10`, `keypress MBTN_LEFT`: position stays below 20 s. IPC `mouse 1150 600` (inside the 1280x720 rect), wait 300 ms for the hover change, `keypress MBTN_LEFT`: position lands in 20–24 s.
  - `user_input_conf_binding_wins_over_the_default_hotkey`: `input_conf = "ENTER set pause yes\n"`; cast E01 from 8 s; `keypress ENTER` pauses and does not skip; IPC `script-binding mpvfin/skip` then skips.
  - `skipping_credits_at_end_of_file_marks_played_and_plays_next`: cast `[E01, E02]`, seek to 102 s, skip: E02 plays, and E01's `UserData.Played` is true.
  - `skipping_credits_mid_file_lands_on_the_preview`: cast E02 alone, seek to 92 s, skip: `time-pos` lands in 110–114 s, E02 is still playing, `Played` is true. Skip again: playback of E02 ends.
  - `no_segments_means_enter_stays_mpvs`: cast `[E03, E01]` from 10 s; `keypress ENTER` moves to E01.

  Each test clears played state and resume on the items it uses first. Add a `Server::is_played(item_id)` helper and whatever clearing helper is needed.

- [ ] **Step 2: Run** with `MPVFIN_E2E_MPV` set: `cargo test -p mpvfin-e2e -- --ignored --test-threads=1`, twice. Expected: every test passes. Fix product code, not test expectations, unless a test contradicts the spec or verified server behaviour, and record any such case.

- [ ] **Step 3: Update docs.** README: what the button does, the default Enter key, binding `script-binding mpvfin/skip` in `input.conf`, and that the server needs a segment provider plugin. Architecture: the segment flow and the temporary input sections.

- [ ] **Step 4: Run** the full verification: workspace tests, clippy, fmt, the smoke test, and the e2e suite.

- [ ] **Step 5: Commit** `test: end-to-end skip button tests; document the button`
