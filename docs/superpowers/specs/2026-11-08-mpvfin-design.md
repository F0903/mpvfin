# mpvfin design

Date: 2026-11-08. Status: draft for owner approval.

## Intent

mpvfin turns the user's own mpv into a Jellyfin cast target. It does the
same job as jellyfin-mpv-shim (casting, remote control, watch together,
intro/credits skipping, watched-status sync). The difference is that it does
**not** own an mpv of its own. The shim runs its own mpv with its own config,
key bindings and menus. mpvfin is one native plugin file that sits in the
user's normal mpv and leaves everything else alone.

The owner's main reason is isolation. Their mpv setup (uosc, thumbfast,
shaders, custom `mpv.conf`/`input.conf`) must look and behave exactly as it
does today. mpvfin only adds Jellyfin on top.

## How it hooks into mpv (decided)

- mpvfin is an **mpv C plugin**: a Rust `cdylib` (`mpvfin.dll` on Windows,
  `mpvfin.so` on Linux) that the user drops into mpv's `scripts/` folder. This
  is what the existing `crates/mpvfin-cplugin` already does. The smoke test
  loaded it into mpv `v0.41.0-923` (shinchiro build, `cplugins` enabled).
- mpvfin exists only while mpv is open. Opening mpv makes it castable, and
  closing mpv removes it from Jellyfin. The owner is happy to open mpv on
  demand, for example with a shortcut that runs `mpv --idle --force-window`.
  **There is no tray app, background service, or launcher.**
- The out-of-process "JSON-IPC agent" in the old `docs/architecture.md` is
  dropped. The backend-neutral `mpvfin-mpv` port can stay, but nothing is
  built for a second backend.

## Goals

In build order. Each step must be usable on its own before the next starts.

### 1. Casting and remote control

- Shows up in Jellyfin's cast menu under the computer's name.
- Remote play from any Jellyfin client: play now, play next, and add to queue.
  Multi-item requests (e.g. a season) become mpv's playlist.
- Remote pause/resume/play-pause, seek, stop, next/previous item, volume,
  mute/unmute/toggle, and audio and subtitle track changes, including the
  server's external subtitle files.
- Reports start, progress and stop to Jellyfin, so resume position and
  watched status come out right. Jellyfin decides watched status from the
  stop position, as usual.
- Changes made locally in mpv (pausing with the user's own keys, seeking on
  uosc's timeline, switching tracks in uosc) are reported back. The remote
  controller then shows the truth.
- A remote "display message" command shows text on mpv's OSD.

### 2. Skip button

- Segment data comes from Jellyfin's Media Segments API (server 10.10+). If
  the server has no segment provider plugin, there are no segments and no
  button appears. That is fine.
- When playback enters an **intro, recap, credits (outro) or preview**
  segment, a button appears in the bottom-right corner, above where uosc's
  timeline sits: "Skip intro", "Skip recap", "Skip credits", "Skip preview".
  The plugin draws it on mpv's OSD, so it does not depend on uosc. It stays
  visible for the whole segment.
- Clicking it, or pressing the hotkey, seeks to the end of the segment. The
  button disappears when the user skips or the segment ends.
- **Skipping credits marks the item watched in Jellyfin**, even if that
  happens before Jellyfin's normal watched threshold. If the credits run to
  the end of the file (nothing meaningful after them), skipping advances to
  the next queued item. If nothing is queued, playback ends normally.
- **Input isolation:**
  - The hotkey and the mouse click are only active while the button is on
    screen.
  - The click only counts when the pointer is over the button. Every other
    click goes to uosc or mpv as usual.
  - The default hotkey is **Enter**, which mpv's built-in bindings use for
    "next file". If the user binds that key in `input.conf`, their binding
    wins. mpv's built-in defaults do not win.
  - The action is also exposed as `script-binding mpvfin/skip`, so the user
    can bind any key of their own.
- Commercial segments get no button (not requested).
- No auto-skip option.

### 3. Watch together (SyncPlay)

- Expected use: the owner is on mpv and the other person uses the official
  Jellyfin TV app or the web client. Interop with official clients is what
  counts, not mpvfin-to-mpvfin.
- A **"watch together" action** (`script-binding mpvfin/watch-together`, so
  the user binds the key or adds it to a uosc menu) opens a list of the
  server's SyncPlay groups, with options to join one, create a new group, or
  leave the current group.
  - If uosc is running, the list uses uosc's menu API.
  - Otherwise it uses mpv's built-in selection prompt.
  - If the built-in prompt turns out not to be usable from a C plugin, a
    plain OSD list driven by the same temporary-input rules as the skip
    button is acceptable.
- In a group:
  - Pause, resume and seek from any member, including the owner's own mpv
    keys and uosc, apply to everyone.
  - The group's queue decides what plays, and changes from either side are
    followed.
  - The owner changes what the group watches the same way they pick
    anything: casting from their phone while mpv is in a group **replaces
    the group's queue**. It does not leave the group. Next/previous in mpv
    (keys or uosc) become the group's next/previous item.
  - If anyone is loading or buffering, the group waits, and everyone starts
    together.
  - A skip-button press is a group seek.
- Staying in sync:
  - Small drift is corrected by briefly changing playback speed, which the
    viewer should not notice.
  - Only larger drift causes a seek.
  - Corrections have a threshold and a cooldown, so they never stutter or
    pile up.
  - mpvfin reports buffering and ready promptly.
  - Speed is put back to whatever it was before the correction.

### Logging in

- The user puts the server address in `script-opts/mpvfin.conf` (mpv's normal
  per-script config file, `key=value`).
- On first run with no saved login, mpvfin starts **Quick Connect** and shows
  the code on the OSD until the user approves it from another Jellyfin
  client. If the server has Quick Connect disabled, mpvfin asks for username
  and password through the same prompt mechanism the watch-together list
  uses.
- The resulting token and a persistent device ID are saved in one
  mpvfin-owned file in mpv's config folder, next to `script-opts/mpvfin.conf`.
  **The password is never stored.**
- If the token is rejected later (revoked or expired), mpvfin shows that on
  the OSD and goes back to the login flow.

## Non-goals

- Running while mpv is closed (no tray app, service or launcher).
- Transcoding. Direct play only at first, which is what
  `docs/jellyfin-protocol.md` already specifies. Items mpv cannot direct-play
  fail with a clear OSD message.
- A library browser or any menu besides watch-together and the login prompt.
  Track and subtitle selection stay with mpv and uosc.
- Auto-skip, commercial-segment skipping, shuffle and repeat modes.
- Supporting mpv front-ends or builds without C-plugin support.

## Anti-goals (a failure even if everything "works")

- **Changing the user's mpv**:
  - writing to `mpv.conf` or `input.conf`;
  - setting global options;
  - adding permanent key bindings;
  - shipping extra Lua scripts;
  - changing how mpv looks when not playing Jellyfin content.

  Everything set for a Jellyfin item is applied per file through `loadfile`
  options or file-local options, so it reverts when the item ends. The
  exceptions are volume and mute, because a remote volume change is meant to
  change mpv's volume.
- **Credentials leaking**: no access token in media URLs, logs, the OSD, mpv's
  watch-later files or uosc's history. Media and subtitle requests carry auth
  in a per-file HTTP header (mpv's `http-header-fields`), not in an
  `api_key` URL parameter. Set the header from an `on_load` hook, not as
  `loadfile` arguments, because mpv logs command arguments at debug level.
  The token must not appear in mpv's log at any level up to `debug`. ffmpeg
  dumping raw HTTP requests at `trace` level is accepted (designer's call).
  There is no auth proxy.
- **mpv's own resume fighting Jellyfin's**: Jellyfin is the source of truth
  for resume position. mpv's watch-later must neither override the start
  position nor record Jellyfin items. Set this per file.
- **Freezing or crashing mpv**:
  - Network I/O never runs on mpv's event-handling path.
  - A dead server or flaky Wi-Fi never blocks playback or quitting.
  - A Rust panic never crosses the C boundary (the existing `catch_unwind`).
- **Being noisy**: OSD messages only for things the user must act on (login
  code, errors, the skip button). Diagnostics go to mpv's log under the
  `mpvfin` prefix.

## Constraints

- **Language**: Rust (existing workspace, edition 2024, Rust 1.97). No Go.
- **Platforms**: Windows first. Linux should work with the same code and is
  not tested at first. macOS is not targeted.
- **mpv**: builds with `cplugins` enabled. The tested build is the portable
  mpv in `.tools/mpv`. The plugin uses only mpv's public client API through
  the `MPV_CPLUGIN_DYNAMIC_SYM` function-pointer mechanism already in
  `mpvfin-cplugin`, and does not link against a specific mpv.
- **Jellyfin**: 10.10+ for media segments. The existing protocol notes were
  checked against 10.11.11. Unknown fields and message types must be
  tolerated.
- **Clean room**: `docs/clean-room.md` still applies. The shim may be used
  only as a behavioural reference, never as a source of code, data
  structures, fixtures or layout.
- **Existing invariants**: everything in `docs/architecture.md` "Invariants"
  still holds. One reducer owns state, generations reject stale results,
  stop is reported exactly once, and controller changes are reported only
  after mpv confirms them.
- **Uninstall**: deleting `mpvfin.dll`, `script-opts/mpvfin.conf` and
  mpvfin's login file removes every trace.

## Decided by the owner

- Use the plugin approach, not the shim's managed-mpv approach.
- Opening mpv on demand to cast to it is fine.
- Features: casting/remote control, watch together, skip intro/credits,
  watched-status sync.
- Skip uses a clickable on-screen button plus a hotkey.
- Watch-together partners use the TV app or the browser.
- Joining watch together is done from a menu inside mpv.
- Login via the server address in `script-opts`, with a saved token.
- **Testing uses a throwaway local Jellyfin in Docker**, never the owner's
  real server. Docker 29 with Compose v5 is available on the dev machine
  (Linux engine). The test server is scripted and reproducible:
  - a pinned Jellyfin 10.11 image;
  - generated sample media (ffmpeg test sources, including an episode
    series);
  - a second user to act as the watch-together partner;
  - a media-segment provider plugin installed so the skip button has
    segments.

  The partner side of watch-together tests is a browser driven by
  automation against the server's web client. The owner does the final
  check against their real TV.

## Builder's calls already made (owner may override)

These came from the design conversation and the owner accepted them, but they
are the designer's suggestions, not the owner's own requirements:

- Enter as the default skip hotkey.
- Button covers recap and preview too, and stays up for the whole segment.
- Skipping credits marks watched and advances to the next item.
- Quick Connect first, with username/password as fallback.
- Build order 1 → 2 → 3.
- Direct play only.
- **One cast target per computer.** Only the first mpv window that opens
  connects to Jellyfin (a lock in mpvfin's own state). Other windows stay
  dormant until it closes. Two windows sharing one device ID would make
  Jellyfin flip between them.
- **Casting replaces whatever the window is playing**, like a Chromecast.
- **Casting while in a watch-together group changes the group's queue**
  rather than leaving the group.
- **Token hidden in mpv's logs up to `debug` level.** It is set via an
  `on_load` hook. ffmpeg's `trace`-level request dumps are accepted, and
  there is no auth proxy.
- **Following the queue only.** If Jellyfin sent no next item, mpvfin does not
  go looking for the next episode itself.

## Left to the builder

- Exact API calls, payloads and message handling for Quick Connect, SyncPlay,
  media segments, the extra playstate and general commands, and marking
  played. Derive them from the official 10.11 API and SDKs, as
  `docs/jellyfin-protocol.md` does, and extend that document.
- Crate choices. Small blocking libraries on plugin-owned threads (no async
  runtime) are a reasonable default. Use rustls so Windows needs no OpenSSL.
- How mpv input sections, OSD overlays and mouse tracking implement the
  temporary skip-button input, within the input-isolation rules above.
- How Jellyfin stream indices map to mpv track IDs, including external
  subtitles added after load.
- SyncPlay drift thresholds, correction speeds and cooldowns.
- Where in mpv's config folder the login/state file lives, and its format.
- Test strategy, within these rules:
  - core logic gets deterministic unit tests (as the existing reducer does);
  - wire handling gets original fixtures;
  - plugin behaviour gets smoke tests against the `.tools` mpv;
  - end-to-end checks run against the Docker test server described above.
- Updating `README.md` and `docs/architecture.md` to match this design.

## Success looks like

- The owner opens mpv, casts an episode from their phone, and controls it
  from the phone. mpv looks exactly as it always does.
- An intro starts, the button appears, and one click or Enter skips it.
  Skipping the credits marks the episode watched and the next one starts.
- The owner and someone on a TV watch a whole episode in one SyncPlay group,
  pausing and seeking from both sides. They stay in sync without visible
  stutter.
  **Open risk:** this depends on the TV's Jellyfin app supporting SyncPlay.
  LG and Samsung apps wrap the web client and should work. The official
  Android TV app did not support SyncPlay as of 2025. mpvfin itself does not
  change with the partner's client. Automated tests use the web client.
- Their `mpv.conf`, `input.conf` and scripts are byte-for-byte unchanged, and
  nothing they do outside Jellyfin playback behaves differently.
