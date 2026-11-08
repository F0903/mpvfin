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
- **Proxy threads.** One accept thread, plus one per media request.

Session threads pass `SessionEvent`s over a channel and call `mpv_wakeup`
through a `Waker`, which interrupts the plugin thread's wait. The `Waker` is
disarmed before mpv's handle goes away. On shutdown the plugin thread continues
any held hook, queues the final stop report, and gives the session a bounded
two-second flush.

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
4. `ContinueLoad` sets `stream-open-filename` to the proxy URL, and the title
   and `save-position-on-quit=no` file-locally, then continues the hook. Every
   hook is continued exactly once; `Mpv` rejects unknown or repeated ids.
5. On `file-loaded` the plugin adds external subtitles and applies the
   selected tracks, then reports start. Pause, seek, volume, mute and track
   changes are reported once mpv confirms them, whoever made them.
6. `end-file` or a replacing cast reports stop exactly once, and only for an
   item whose start was reported.

Nothing mpvfin sets outlives the item, except volume and mute, which a remote
is meant to change.

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
8. The plugin never sets a global mpv option, apart from the volume and mute
   a remote asks for. No token reaches a URL, property, argument or log.

## Workspace

- `mpvfin-core`: state, events, actions and lifecycle rules.
- `mpvfin-jellyfin`: Jellyfin transport, login, state file and media proxy.
- `mpvfin-cplugin`: native mpv plugin (`mpvfin.dll`) and client-API adapter.
- `mpvfin-e2e`: end-to-end tests against a Docker Jellyfin and a real mpv.
