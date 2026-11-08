# Architecture

## Components

```text
Jellyfin HTTP/WebSocket
          |
          v
  jellyfin transport
          |
          v
     mpvfin-core  <--- the only owner of playback/session state
          |
          v
      player port
       /       \
 C-plugin     JSON IPC
 adapter      adapter (later)
```

`mpvfin-core` is a synchronous reducer. It accepts a typed event, updates its
state, and returns actions for adapters to execute. It performs no I/O and owns
no threads.

The Jellyfin transport will own authentication, WebSocket supervision, HTTP
timeouts, capability advertisement, playback negotiation, and lifecycle
reporting. It cannot mutate receiver state directly.

The player adapter converts core actions to mpv commands and converts mpv
events back to core events. The initial native adapter is an mpv C-plugin. A
JSON-IPC adapter can later provide an out-of-process receiver mode without
changing the core.

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

## Planned workspace

- `mpvfin-core`: state, events, actions, and lifecycle rules.
- `mpvfin-mpv`: backend-neutral mpv command/event port.
- `mpvfin-cplugin`: native mpv plugin entry point and client-API adapter.
- `mpvfin-jellyfin`: official Jellyfin API transport (next slice).
- `mpvfin-agent`: optional always-on/JSON-IPC executable (later).

