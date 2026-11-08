# mpvfin

`mpvfin` makes your own mpv a Jellyfin cast target. It is one plugin file that
sits in mpv's `scripts/` folder, so your `mpv.conf`, key bindings and scripts
stay exactly as they are. It exists only while mpv is open.

This is a clean-room implementation against mpv's client API and Jellyfin's
official API. `jellyfin-mpv-shim` is not a dependency and none of its code is
used. See [docs/clean-room.md](docs/clean-room.md).

## What works

Casting and remote control from any Jellyfin client: play now, queues
(multi-item casts become mpv's playlist), pause, seek, stop, next and previous,
volume, mute, audio and subtitle tracks including the server's external
subtitles, and OSD messages. Start, progress and stop are reported, so resume
position and watched status come out right. Changes you make in mpv are
reported back too.

Direct play only; items mpv cannot direct-play fail with an OSD message. Watch
together is not built yet.

It needs mpv 0.38 or newer (for `loadfile url flags index options`), built
with C-plugin support. Windows is the tested platform.

## Install

Build the plugin, or use a build you already have:

```powershell
cargo build --release -p mpvfin-cplugin
```

1. Copy `target/release/mpvfin.dll` into the `scripts/` folder of mpv's config
   directory (`%APPDATA%\mpv\scripts\`, or `portable_config\scripts\` for a
   portable mpv).
2. Create `script-opts/mpvfin.conf` in that same config directory:

   ```ini
   server=https://jellyfin.example.com
   ```

3. Start mpv so it waits for a cast, for example from a shortcut:

   ```powershell
   mpv --idle --force-window
   ```

## Log in

On first start mpvfin shows a **Quick Connect** code on mpv's OSD. Approve it
in another Jellyfin app under Settings > Quick Connect. The login is saved, and
the password is never seen or stored. If the login is later revoked, mpvfin
goes back to Quick Connect.

mpv then appears in Jellyfin's cast menu under your computer's name. Only the
first open mpv window connects; others take over when it closes.

## Skip button

During an intro, recap, credits or preview, a button appears at the bottom
right. Click it or press **Enter** to jump to the end of that segment. Skipping
credits also marks the item as played; credits that run to the end of the file
go straight to the next item.

Enter is only mpvfin's while the button is up. The rest of the time it is mpv's
own key (next in the playlist), and your own binding of Enter in `input.conf`
always wins. To skip with a key of your choice, add this to `input.conf`:

```text
F5 script-binding mpvfin/skip
```

Your Jellyfin server needs a media segment provider plugin; without one it has
no segments and the button never appears.

## Uninstall

Delete these three files from mpv's config directory; nothing else is left
behind:

- `scripts/mpvfin.dll`
- `script-opts/mpvfin.conf`
- `script-opts/mpvfin-state.json` (the saved login and device ID)

## Development

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p mpvfin-cplugin
./scripts/smoke-test-cplugin.ps1 -MpvPath .tools/mpv/mpv.com
```

### End-to-end tests

These drive a real mpv against a throwaway Jellyfin in Docker. Plain
`cargo test` skips them. They use the portable mpv at `.tools/mpv/mpv.com` and
`target/debug/mpvfin.dll`.

```powershell
./scripts/test-server.ps1 -Action up
cargo build -p mpvfin-cplugin
cargo test -p mpvfin-e2e -- --ignored --test-threads=1
```

Set `MPVFIN_E2E_MPV` or `MPVFIN_E2E_SERVER` to use another mpv or server.
`./scripts/test-server.ps1 -Action down` stops the server, and `-Action reset`
deletes its data.

See [docs/architecture.md](docs/architecture.md) for how it works and
[docs/jellyfin-protocol.md](docs/jellyfin-protocol.md) for the wire contract.
