# mpvfin

`mpvfin` makes mpv act as a remotely controllable Jellyfin playback target.

The project is being built as a clean-room implementation against the public
mpv client API and Jellyfin's official API contract. `jellyfin-mpv-shim` is not
a dependency and none of its implementation is used here. See
[docs/clean-room.md](docs/clean-room.md).

## Current status

The first development slice contains:

- a deterministic, side-effect-free receiver state machine;
- an abstract mpv player boundary;
- a minimal native mpv C-plugin that proves the extension mechanism; and
- tests for stale events, replacement playback, and exactly-once stop reports.

Jellyfin transport and playback negotiation are the next slice.

## Build

```powershell
cargo test --workspace
cargo build -p mpvfin-cplugin
```

The plugin artifact is `target/debug/mpvfin.dll` on Windows. To smoke-test it
against a local mpv build:

```powershell
./scripts/smoke-test-cplugin.ps1 -MpvPath C:\path\to\mpv.com
```

On Windows, pass `mpv.com` so the smoke test can verify the plugin's terminal
load sentinel as well as its exit status.

## Design

The receiver core owns all mutable playback state. Network and player adapters
translate external events into typed core events and execute the resulting
actions. This keeps retries, callbacks, and late responses from mutating state
directly.

See [docs/architecture.md](docs/architecture.md) for the component boundaries
and invariants. The clean-room wire contract for the next slice is recorded in
[docs/jellyfin-protocol.md](docs/jellyfin-protocol.md).
