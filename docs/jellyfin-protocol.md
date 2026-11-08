# Jellyfin protocol baseline

This is the clean-room interoperability contract for the first Jellyfin
transport. It was checked on 2026-08-16 against Jellyfin Server 10.11.11 and
the official SDKs. The transport must tolerate unknown fields and message
types so newer servers remain forward compatible.

## Identity and authentication

- Persist one opaque device ID across launches.
- Authenticate REST requests with the modern `MediaBrowser` authorization
  header carrying client name, device name, device ID, version, and token.
- For the initial prototype, obtain a token with
  `POST /Users/AuthenticateByName`. Add Quick Connect after the basic receiver
  lifecycle is reliable.
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

Retain the returned play-session ID, media-source ID, play method, stream
indices, ETag, and required HTTP headers. Do not pass a server-local
`MediaSource.Path` to mpv. Construct an authenticated server streaming URL for
normal server-hosted files. Redact that URL wherever it may contain an API key.

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
3. `POST /Sessions/Playing/Stopped` exactly once on EOF, replacement, remote
   stop, load failure, or shutdown.

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
4. `GET`/`POST /System/Configuration` with `QuickConnectAvailable=true`.
5. `POST /Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=true`
   with body `{"LibraryOptions":{"PathInfos":[{"Path":"/media/shows"}]}}`.
6. `GET /Items?Recursive=true&IncludeItemTypes=Episode` until
   `TotalRecordCount` is 3.
