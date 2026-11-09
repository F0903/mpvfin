# Architecture

mpvfin is one native mpv C plugin: `mpvfin.dll` (`mpvfin.so` on Linux) in the
`scripts/` folder of the user's own mpv. It exists only while mpv is open.
There is no agent, service, or second player backend, and nothing besides the
plugin talks to mpv.

## Components

```text
                  Jellyfin server
                 ^       ^       ^
    HTTP + WebSocket     |       | HTTP, with the Authorization header
                 |       |       |
        session/jobs  threads    local auth proxy (127.0.0.1)
                 |  ^                    ^
   jobs (submit) |  | events + waker     | media and subtitles, no token
                 v  |                    |
             plugin thread --- mpv client API ---> mpv
                   |
                   v
             mpvfin-core (reducer: the only owner of playback state)
```

- `mpvfin-core`: a synchronous reducer. It takes a typed event, updates its
  state and returns actions. It performs no I/O and owns no threads.
- `mpvfin-jellyfin`: config, the state file, login, WebSocket supervision,
  playback negotiation, reports and the local auth proxy. It runs entirely on
  its own threads and never touches mpv.
- `mpvfin-cplugin`: the plugin entry point, the mpv client-API wrapper, and
  the event loop that connects the other two.

## Threads and the waker

- **Plugin thread** (mpv's `mpv_open_cplugin` call). The only thread that
  calls `mpv_wait_event`, the reducer and the mpv API. It never does network
  I/O, so a dead server cannot stall mpv.
- **`session` thread.** Reads `script-opts/mpvfin.conf`, takes the state-file
  lock, logs in, registers capabilities and holds the WebSocket, reconnecting
  with capped backoff.
- **`jobs` thread.** Runs `Resolve` and report jobs in submission order.
- **`group` thread.** Runs SyncPlay requests and the group list, in order, so a
  slow report or resolve can never delay a `Ready`.
- **`clock` thread.** Measures the offset to the server's clock with
  `/GetUtcTime`: five samples at each connect and join, then a batch a minute
  while in a group. It turns a command's `When` into a local delay.
- **Proxy threads.** One accept thread, plus one per media request.

Session threads pass `SessionEvent`s over a channel and call `mpv_wakeup`
through a `Waker`, which interrupts the plugin thread's wait. The `Waker` is
disarmed before mpv's handle goes away. mpv sends its shutdown event only after
every hook has continued, so the plugin thread fails a resolve still unanswered
after four seconds; that bounds quitting while the server stalls. On shutdown
it queues the final stop report and gives the session a bounded two-second
flush, or none if a resolve is stuck past that deadline, since jobs run in
order and no report can be waiting behind it.

## Playback flow

Every Jellyfin item becomes an mpv playlist entry named `mpvfin://<item id>`,
so mpv's own playlist is the queue and the user's next/previous keys work.

1. A `Play` command reaches the reducer, which emits `QueueItems`. The plugin
   runs `loadfile` for each item, with `resume-playback=no` and the start
   position as per-file options.
2. When mpv opens an entry, the `on_load` hook fires. The plugin holds it. An
   entry that is not `mpvfin://` is continued straight away and left alone.
3. The reducer issues `ResolveMedia`. The `jobs` thread calls `PlaybackInfo`
   off mpv's event path and answers with `MediaResolved` or
   `MediaResolutionFailed`.
4. `ContinueLoad` sets `stream-open-filename` to the proxy URL, and the title,
   `save-position-on-quit=no` and an uncreatable `watch-later-dir` (a path
   under the state file) file-locally, then continues the hook, so mpv records
   no resume data for Jellyfin items, not even on `quit-watch-later`. Every
   hook is continued exactly once; `Mpv` rejects unknown or repeated ids.
5. On `file-loaded` the plugin reports start, then adds external subtitles
   with `mpv_command_async`, so a stalled subtitle cannot freeze the event
   loop, and applies the selected tracks once the last `sub-add` has replied.
   Pause, seek, volume, mute and track changes are reported once mpv confirms
   them, whoever made them.
6. `end-file` or a replacing cast reports stop exactly once, and only for an
   item whose start was reported.

Nothing mpvfin sets outlives the item, except volume and mute, which a remote
is meant to change, and a remote unpause, which clears the user's own pause.

## Skip button

After `MediaResolved`, the `jobs` thread fetches the item's media segments
(`GET /MediaSegments/{id}`), so playback never waits for them, and answers with
`SegmentsResolved`. The reducer checks them on every position change and emits
`ShowSkipButton` and `HideSkipButton`, starting at mpv's first `playback-restart`
for the item, so a cast with a start position cannot show a button from before
it. A skipped segment stays dismissed until playback leaves it.

The button is an `osd-overlay` plus two mpv input sections, defined at startup
and inert until enabled, so nothing changes mpv's input while no button is up:

- `mpvfin-skip-key` binds `ENTER` with `default`, so a user's own binding of the
  key wins. It is enabled only while the button is up; afterwards Enter is mpv's
  again.
- `mpvfin-skip-click` binds `MBTN_LEFT` with `force`. It is enabled only while
  the pointer is over the button, so a click elsewhere stays mpv's.

Either key, or a user's `script-binding mpvfin/skip`, reaches the plugin as a
client message and becomes `SkipRequested`. The reducer hides the button and
seeks to the segment's end. A segment that runs to the end of the file (within
5 s of the runtime) uses `seek 100 absolute-percent+exact` instead, so mpv
reaches EOF and advances the playlist itself. Skipping credits also submits a
`MarkPlayed` job, which runs on the `jobs` thread.

## Watch together

A group is one more source of commands for the same reducer. The server is the
authority: mpv asks, and acts only on what comes back. The contract is
[jellyfin-syncplay.md](jellyfin-syncplay.md).

1. `script-binding mpvfin/watch-together` lists the groups on the `group`
   thread and opens the menu, in uosc if it is running and otherwise in mpv's
   console. Creating a group while playing publishes mpv's playlist and
   position as the group's queue.
2. A queue update replaces mpv's playlist. Each entry is named
   `mpvfin://<item id>/<playlist item id>`, because an item can be queued twice.
   The `on_load` hook adds per-file options: start paused, at the group's
   position, and `keep-open=always` without `keep-open-pause`.
3. When the position settles the reducer sends `Ready`. `Unpause` carries a
   time, so the plugin starts mpv at that moment, from that position. Anything
   mpv has to do first, such as a seek, happens while paused.
4. Pausing, seeking or changing entry in mpv, and casting to it, become requests
   to the group. A viewer's unpause is held back until the broadcast arrives,
   which then applies to everyone, mpv included.
5. At the end of an entry mpv holds, because of `keep-open`, and the plugin asks
   for the next one, or stops the group after the last. The server has no notion
   of an item ending.
6. Leaving or opening a local file takes mpv out of the group; playback carries
   on and the entry gets the viewer's `keep-open` back. An item that cannot load
   also takes mpv out, with an OSD message, and mpv moves on to its next entry or
   goes idle. After a dropped connection the session rejoins the remembered group.

While playing in a group the plugin compares mpv's position with the group's
clock, anchored at the last start. Under 120 ms is left alone. Up to 2 s, it
changes the file-local `speed` by 5% for as long as it takes (at most 30 s), then restores it.
Beyond 2 s it seeks. Each correction is followed by a cooldown, and a pause,
seek or new item drops the anchor, so a viewer's seek never reads as drift.

## Local auth proxy

mpv logs every property it sets at `-v` and every command argument at `debug`,
so anything handed to mpv ends up in its logs. **mpv therefore never receives
the token.** Media and subtitle URLs point at a proxy on
`http://127.0.0.1:<port>/<random 32 hex>/`, and the proxy adds the
`Authorization` header on the way upstream.

The path secret is random per run, only `GET`/`HEAD` on `/Videos/` are served,
upstream redirects are never followed, and only range and content headers are
passed back. The link is useless once mpv closes. Details are in
[jellyfin-protocol.md](jellyfin-protocol.md).

## One cast target per computer

The state file `script-opts/mpvfin-state.json` is held exclusively locked by
the window that connected. Other windows log that they are waiting and retry
every five seconds, so they take over when the first closes.

## Invariants

1. One reducer owns mutable playback state.
2. Every playback attempt has a monotonically increasing generation.
3. Results and callbacks carrying an old generation are ignored.
4. A resolved playback is stopped at most once, including replacement and
   shutdown paths.
5. Pause, seek, volume, and mute are reported only after player confirmation.
6. Callbacks enqueue events; they never block on HTTP, reconnect sleeps, or
   media loading.
7. A `file-loaded` event correlated to the current generation marks playback
   ready; media duration is not a readiness signal.
8. The plugin never sets a global mpv option, apart from the volume, mute and
   unpause a remote asks for. No token reaches a URL, property, argument or
   log.

## Workspace

- `mpvfin-core`: state, events, actions and lifecycle rules.
- `mpvfin-jellyfin`: Jellyfin transport, login, state file and media proxy.
- `mpvfin-cplugin`: native mpv plugin (`mpvfin.dll`) and client-API adapter.
- `mpvfin-e2e`: end-to-end tests against a Docker Jellyfin and a real mpv.
