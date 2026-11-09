# Jellyfin protocol baseline

This is the clean-room interoperability contract for the first Jellyfin
transport. It was checked on 2026-08-16 against Jellyfin Server 10.11.11 and
the official SDKs. The transport must tolerate unknown fields and message
types so newer servers remain forward compatible.

## Identity and authentication

- Persist one opaque device ID across launches.
- Authenticate REST requests with the modern `MediaBrowser` authorization
  header carrying client name, device name, device ID, version, and token.
- Obtain a token with Quick Connect (see "Quick Connect login" below).
- Never write access tokens, authorization headers, or authenticated media
  URLs to logs.

References:

- [official TypeScript SDK authentication helper](https://github.com/jellyfin/jellyfin-sdk-typescript/blob/master/src/utils/authentication.ts)
- [Jellyfin authentication guide](https://kotlin-sdk.jellyfin.org/guide/authentication.html)
- [10.11.11 user controller](https://github.com/jellyfin/jellyfin/blob/v10.11.11/Jellyfin.Api/Controllers/UserController.cs)

## Receiver registration and commands

After authentication, post this device's implemented media profile and command
set to `POST /Sessions/Capabilities/Full`. Advertise only behavior covered by
tests. The first command set is:

- `Play` with `PlayNow`, `PlayNext` and `PlayLast` over `ItemIds`, honoring
  `StartIndex`, `StartPositionTicks`, `AudioStreamIndex` and
  `SubtitleStreamIndex` for `PlayNow`; other play commands are ignored;
- `Playstate` with pause, unpause, play/pause, seek, stop, next and previous
  track;
- `GeneralCommand` with volume, mute, unmute, toggle mute,
  `SetAudioStreamIndex`, `SetSubtitleStreamIndex` and `DisplayMessage`.
  `Arguments` values are strings (`Volume`, `Index`, `Header`, `Text`,
  `TimeoutMs`); a malformed argument ignores the command. Volume is clamped to
  0-100, a subtitle index of -1 means off, and a display message defaults to
  five seconds.

Open `ws(s)://<server-base-path>/socket?ApiKey=<token>` after registration,
preserving any path prefix in the configured server URL. Incoming envelopes
have `MessageType`, optional `MessageId`, and nullable `Data`. A
`ForceKeepAlive` message supplies the interval; reply with `KeepAlive` at
roughly half that interval. Reconnect with capped exponential backoff and
jitter, then repost capabilities.

References:

- [10.11.11 session controller](https://github.com/jellyfin/jellyfin/blob/v10.11.11/Jellyfin.Api/Controllers/SessionController.cs)
- [official SDK WebSocket service](https://github.com/jellyfin/jellyfin-sdk-typescript/blob/master/src/websocket/websocket-service.ts)
- [PlayRequest](https://github.com/jellyfin/jellyfin/blob/v10.11.11/MediaBrowser.Model/Session/PlayRequest.cs)
- [PlaystateCommand](https://github.com/jellyfin/jellyfin/blob/v10.11.11/MediaBrowser.Model/Session/PlaystateCommand.cs)
- [GeneralCommandType](https://github.com/jellyfin/jellyfin/blob/v10.11.11/MediaBrowser.Model/Session/GeneralCommandType.cs)

## Playback negotiation

Resolve each item with `POST /Items/{itemId}/PlaybackInfo`, putting negotiation
parameters and the registered device profile in the JSON body. The first
implementation enables direct play only and rejects live sources, sources that
require opening, transcodes, and incompatible media with a clear diagnostic.

Retain the returned play-session ID, media-source ID, play method and stream
indices; the ETag and required HTTP headers are not retained. Do not pass a
server-local
`MediaSource.Path` to mpv. Hand mpv only local proxy URLs (see "Local auth
proxy" below), never a token in a URL, header or property.

Jellyfin positions use 100-nanosecond ticks. Convert at the transport boundary;
the reducer and mpv port use `Duration`.

References:

- [10.11.11 media-info controller](https://github.com/jellyfin/jellyfin/blob/v10.11.11/Jellyfin.Api/Controllers/MediaInfoController.cs)
- [PlaybackInfoDto](https://github.com/jellyfin/jellyfin/blob/v10.11.11/Jellyfin.Api/Models/MediaInfoDtos/PlaybackInfoDto.cs)

## Playback reporting

Serialize reports for each playback generation in this order:

1. `POST /Sessions/Playing` after mpv confirms `file-loaded`.
2. `POST /Sessions/Playing/Progress` about every ten seconds and immediately
   after player-confirmed pause, resume, seek, volume, or mute changes.
3. `POST /Sessions/Playing/Stopped` exactly once after a started report, on
   EOF, replacement, remote stop, a playback failure, or shutdown. An item
   that never reached `file-loaded` sends no report at all: on 10.11.11 a
   stop without a start resets the saved resume position to zero.

Every asynchronous result carries its playback generation. Results for retired
generations are ignored, including late HTTP responses and mpv events.

References:

- [10.11.11 play-state controller](https://github.com/jellyfin/jellyfin/blob/v10.11.11/Jellyfin.Api/Controllers/PlaystateController.cs)
- [PlaybackStartInfo](https://github.com/jellyfin/jellyfin/blob/v10.11.11/MediaBrowser.Model/Session/PlaybackStartInfo.cs)
- [PlaybackProgressInfo](https://github.com/jellyfin/jellyfin/blob/v10.11.11/MediaBrowser.Model/Session/PlaybackProgressInfo.cs)
- [PlaybackStopInfo](https://github.com/jellyfin/jellyfin/blob/v10.11.11/MediaBrowser.Model/Session/PlaybackStopInfo.cs)

## Test server provisioning

`scripts/test-server.ps1 -Action up|down|reset` drives `tests/server/compose.yml`
(image `jellyfin/jellyfin:10.11.11`, `127.0.0.1:8096`, named volumes). Calls
verified against that image, all unauthenticated until login:

1. `GET /System/Info/Public` (readiness; `StartupWizardCompleted`).
2. Wizard: `POST /Startup/Configuration`
   `{"UICulture","MetadataCountryCode","PreferredMetadataLanguage"}`;
   `GET /Startup/User` (first-user record must exist); `POST /Startup/User`
   `{"Name":"admin","Password":"mpvfin-test"}`; `POST /Startup/RemoteAccess`
   `{"EnableRemoteAccess":true,"EnableAutomaticPortMapping":false}`;
   `POST /Startup/Complete`.
3. `POST /Users/AuthenticateByName` `{"Username","Pw"}` with an
   `X-Emby-Authorization: MediaBrowser Client=..., Device=..., DeviceId=..., Version=...`
   header; later calls add `Token="<AccessToken>"` to that header.
4. `GET`/`POST /System/Configuration` with `QuickConnectAvailable=true` and
   `MinResumeDurationSeconds=0`. The default of 300 s means Jellyfin never saves
   a resume position for the 120 s test episodes.
5. `POST /Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=true`
   with body `{"LibraryOptions":{"PathInfos":[{"Path":"/media/shows"}]}}`.
6. `GET /Items?Recursive=true&IncludeItemTypes=Episode` until
   `TotalRecordCount` is 3.

## Behaviour observed by the end-to-end tests

- A `PlayNow` for a single episode reaches the client as that episode and the
  rest of the series. mpvfin follows the queue it was sent, so when a
  one-episode cast ends mpv moves to the next episode.
- With a demuxer limit too small for the file's audio and video interleave
  (64 KiB on the test episodes), mpv logs `Too many packets in the demuxer
  packet queues`, marks the starved stream at EOF and ends the file after a
  seek. A plain mpv reading the server directly does the same, with no plugin
  or proxy involved. From 512 KiB up, seeks through the proxy land every time.

## Capabilities and playback negotiation (verified against the test server)

- `POST /Sessions/Capabilities/Full` with `capabilities_body()` returns 204.
  Body: `PlayableMediaTypes ["Video"]`, `SupportedCommands` (SetVolume, Mute,
  Unmute, ToggleMute, SetAudioStreamIndex, SetSubtitleStreamIndex,
  DisplayMessage), `SupportsMediaControl`, `SupportsPersistentIdentifier`, and a
  `DeviceProfile` (direct play only, no transcoding profiles, `Embed` and
  `External` subtitle profiles for 12 formats).
- `POST /Items/{id}/PlaybackInfo` with `playback_info_body(userId)` (direct
  play on, direct stream and transcoding off) returns top-level
  `PlaySessionId` and `MediaSources[]`. S01E01 yields one source with
  `Id` (equal to the item id), `SupportsDirectPlay: true`,
  `RequiresOpening: false`, `IsRemote: false` and `MediaStreams[]`, each with
  `Index`, `Type` (Video/Audio/Subtitle), `IsExternal`, and for subtitles
  `DeliveryMethod` (`Embed` or `External`).
- Stream indices are not grouped: the external `fr` subtitle has `Index 0`
  (before video 1, audio 2-3, embedded subtitles 4-5). Mapping therefore sorts
  by `Index` and puts embedded subtitles before external ones.
- Only the external subtitle has `DeliveryUrl`, e.g.
  `/Videos/{guid}/{sourceId}/Subtitles/0/0/Stream.subrip?ApiKey=<access token>`.
  It does carry the API key, so mpvfin strips every `ApiKey`/`api_key`
  parameter before handing the URL to mpv.
- Stream URL: `{base}/Videos/{itemId}/stream?static=true&mediaSourceId={sourceId}`,
  authenticated by the `Authorization` header only. mpvfin requests it through
  the local proxy.

## Local auth proxy

mpv logs every property set at `-v` and command arguments at `debug`, so it
must never hold the token. The plugin runs a proxy on
`http://127.0.0.1:<port>/<random 32 hex>` and builds every stream and subtitle
URL on that base; the `Authorization` header is added upstream.

- Only `GET` and `HEAD` under `/<secret>/Videos/` are served, and `..` is
  refused raw or percent-encoded. Other paths answer 403, so a leaked link
  cannot reach the rest of the API. A wrong secret answers 404.
- Responses carry only `Content-Type`, `Content-Length`, `Content-Range` and
  `Accept-Ranges`, plus `Connection: close`. Upstream `200`, `206`, `404` and
  `416` pass through; anything else becomes `502` with an empty body. A
  logged-out session answers `503` without contacting the server.
- Upstream requests use a 5 s connect timeout and a 30 s per-read timeout, no
  overall limit, zero redirects, and `Accept-Encoding: identity`.

## Quick Connect login (verified against the test server)

Checked on 2026-11-08 against the 10.11.11 test image. `Initiate` and
`AuthenticateWithQuickConnect` need the token-less `Authorization: MediaBrowser
Client=..., Device=..., DeviceId=..., Version=...` header; `Enabled` and
`Connect` work without it (mpvfin sends it on every call).

1. `GET /QuickConnect/Enabled` returns the JSON boolean `true` or `false`.
2. `POST /QuickConnect/Initiate` (no body) returns 200 with `Secret` (64 hex
   characters), `Code` (six digits), `Authenticated: false`, `DeviceId`,
   `DeviceName`, `AppName`, `AppVersion` and `DateAdded`. Without the
   `Authorization` header it returns 400.
3. `GET /QuickConnect/Connect?secret=<Secret>` returns the same object;
   `Authenticated` becomes `true` once a user approves the code. An unknown or
   expired secret returns 404 (`"Unknown secret"`).
4. Approval happens in another Jellyfin app; the test harness does it as admin
   with `POST /QuickConnect/Authorize?code=<Code>` (returns 200 `true`).
5. `POST /Users/AuthenticateWithQuickConnect` with `{"Secret":"<Secret>"}`
   returns 200 with `User`, `SessionInfo`, `AccessToken` (32 hex characters)
   and `ServerId`; the session belongs to the header's `DeviceId`. Before
   approval it returns 404.

mpvfin shows the code, polls step 3 every 3 s, starts over at step 2 on 404,
and saves `User.Id` and `AccessToken` with the server URL.

Login expiry: with an invalid or revoked token,
`POST /Sessions/Capabilities/Full` returns 401, and mpvfin restarts Quick
Connect. Revoking the device (`DELETE /Devices?id=<deviceId>`) makes the
server close the WebSocket with a normal close first.

The WebSocket stayed connected for over 75 s while mpvfin sent
`{"MessageType":"KeepAlive"}` every 30 s. On shutdown the server logged the
`Stopped` report before the socket closed.

## Media segments (verified against the test server)

Checked on 2026-11-08 against the 10.11.11 test image. Jellyfin derives
segments only through a provider plugin; `scripts/test-server.ps1 -Action up`
sets this up without a reset.

- Install: `POST /Packages/Installed/Chapter%20Segments%20Provider?assemblyGuid=698b6f3314ca49b59d79fc3c0ab941f5&version=4.0.0.0`
  (admin token), then restart the container. `GET /Plugins` lists it as
  `Chapter Segments Provider`, `Active`. Note: PowerShell 7 does not unroll a
  JSON array piped straight from `Invoke-RestMethod`; assign it first.
- Chapters: the plugin maps chapter names to segment types by its default name
  patterns. These names matched unchanged: `Intro` (Intro), `Recap` (Recap),
  `Credits` (Outro), `Preview` (Preview); `Cold Open` and `Part 1` produce
  nothing. Test chapters (E01: Cold Open 0-5, Intro 5-20, Part 1 20-100,
  Credits 100-120; E02: Recap 0-10, Part 1 10-50, Credits 50-70, Part 2
  70-110, Preview 110-120, so that credits end before the 90% watched point) are remuxed in place with `ffmpeg -map 0 -map_chapters 1 -c copy`;
  marker file `/media/.chapters-v2`. A segment that runs to the last chapter
  ends at the file end (`EndTicks` about 120.047 s, not exactly 120).
- Scan: scheduled task key `TaskExtractMediaSegments` ("Media Segment Scan");
  `POST /ScheduledTasks/Running/{taskId}` starts it, `GET /ScheduledTasks/{taskId}`
  reports `State` `Running` or `Idle`.
- Read: `GET /MediaSegments/{itemId}` returns `{"Items":[{"Id","ItemId","Type",
  "StartTicks","EndTicks"}],"TotalRecordCount","StartIndex"}`; `Type` is
  `Intro`, `Outro`, `Recap` or `Preview`; ticks are 100 ns units.
  `includeSegmentTypes` filters.

mpvfin requests
`GET /MediaSegments/{id}?includeSegmentTypes=Intro&includeSegmentTypes=Recap&includeSegmentTypes=Outro&includeSegmentTypes=Preview`
(200 with `Items` empty when the item has none; confirmed live). An `Outro`
`EndTicks` may exceed the item's `RunTimeTicks` slightly; mpvfin treats a
segment ending within 5 s of the runtime as running to the end of the file.

## Which session a request acts for (verified against the test server)

The session is keyed on the `Client` and `DeviceId` of the `Authorization`
header, while the WebSocket finds its session through the token's device
record. A token obtained under one `Client` value and used on HTTP under
another therefore acts on a second session with no socket. `POST
/SyncPlay/Join` from that session returns 204 and the group pauses for it, but
its updates reach nobody, and it stays a member until its device is removed.
mpvfin always logs in and connects under `Client="mpvfin"`.

The SyncPlay contract is in [jellyfin-syncplay.md](jellyfin-syncplay.md).

## Marking played (verified against the test server)

`POST /UserPlayedItems/{itemId}?userId={userId}` with no body returns 200 and
`UserData.Played` becomes true (`DELETE` on the same path undoes it).

## Casting one episode (verified against the test server)

A `PlayNow` for a single episode arrives with the rest of the series queued
behind it: casting E02 alone gives the plugin `[E02, E03]` (seen as two
`loadfile` commands; mpvfin adds nothing to the queue). Skipping to the end of
E02 therefore plays E03 instead of leaving mpv idle, which
`skipping_the_preview_at_end_of_file_ends_the_item` asserts.
