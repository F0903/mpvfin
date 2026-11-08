# Jellyfin SyncPlay client contract (server 10.11.11)

Researched 2026-11-08 against the OpenAPI document of the running test server, the
server source at tag `v10.11.11`, and black-box probes with two or three sessions.
jellyfin-mpv-shim and jellyfin-web were not read.

Tags: **[source]** read in server source or OpenAPI. **[live]** observed against the
Docker test server (10.11.11). **[inferred]** my conclusion or recommendation; no server
text states it.

Abbreviations: `PQ` = `SyncPlayGroupUpdate` with `Type: "PlayQueue"`. Ticks are 100 ns.
`pid` = `PlaylistItemId` of a SyncPlay queue entry.

## 1. Wire basics

- All SyncPlay traffic from server to client arrives on the existing `/socket` WebSocket;
  HTTP responses carry almost nothing. There are no client-to-server SyncPlay WebSocket
  messages (`SessionWebSocketListener.ProcessMessageAsync` is a no-op). [source]
- Only two message types matter: `SyncPlayCommand` and `SyncPlayGroupUpdate`. [source][live]
- JSON is PascalCase; enums are strings, not numbers. [live]
- GUIDs on the wire are 32 hex with no dashes (`"3d0ace87ceea4c8ba295174761730fad"`),
  including `GroupId` in `SendCommand`/updates and every `PlaylistItemId`. The single
  exception is the `Data` string of `GroupLeft`, which is the dashed form. [live]
  Requests accept both forms. [live]
- Timestamps are UTC with `Z` and a variable number of fractional digits: trailing zeros are
  dropped, e.g. `...03.941608Z`, `...35.69981Z`, `...28.957Z`. Parse 0 to 7+ digits. [live]
  Send `When` in UTC with `Z`. A value without a zone was accepted, but that is only safe
  because the test server runs in UTC. [live][inferred]
- `MessageId` is present on every message and is informational; nothing acknowledges it.
  [source][live]

## 2. HTTP endpoints

All under `/SyncPlay/` unless noted, with the normal `Authorization` header. Every body
field is optional in OpenAPI and defaults to a zero value when absent; see section 3 for
what that means in practice. [source]

| Method and path | Body | Success | Policy |
| --- | --- | --- | --- |
| `GET /List` | - | 200 `GroupInfoDto[]` | JoinGroup |
| `GET /{id}` | - | 200 `GroupInfoDto`, 404 unknown | JoinGroup |
| `POST /New` | `{"GroupName": string}` | **200** `GroupInfoDto` (doc comment says 204) | CreateGroup |
| `POST /Join` | `{"GroupId": guid}` | 204 | JoinGroup |
| `POST /Leave` | none | 204 | IsInGroup |
| `POST /SetNewQueue` | `{"PlayingQueue": guid[], "PlayingItemPosition": int, "StartPositionTicks": long}` | 204 | IsInGroup |
| `POST /SetPlaylistItem` | `{"PlaylistItemId": guid}` | 204 | IsInGroup |
| `POST /RemoveFromPlaylist` | `{"PlaylistItemIds": guid[], "ClearPlaylist": bool, "ClearPlayingItem": bool}` | 204 | IsInGroup |
| `POST /MovePlaylistItem` | `{"PlaylistItemId": guid, "NewIndex": int}` | 204 | IsInGroup |
| `POST /Queue` | `{"ItemIds": guid[], "Mode": "Queue"|"QueueNext"}` | 204 | IsInGroup |
| `POST /Unpause` `/Pause` `/Stop` | none | 204 | IsInGroup |
| `POST /Seek` | `{"PositionTicks": long}` | 204 | IsInGroup |
| `POST /Buffering` | `{"When": date, "PositionTicks": long, "IsPlaying": bool, "PlaylistItemId": guid}` | 204 | IsInGroup |
| `POST /Ready` | same as Buffering | 204 | IsInGroup |
| `POST /SetIgnoreWait` | `{"IgnoreWait": bool}` | 204 | IsInGroup |
| `POST /NextItem` `/PreviousItem` | `{"PlaylistItemId": guid}` | 204 | IsInGroup |
| `POST /SetRepeatMode` | `{"Mode": "RepeatOne"|"RepeatAll"|"RepeatNone"}` | 204 | IsInGroup |
| `POST /SetShuffleMode` | `{"Mode": "Sorted"|"Shuffle"}` | 204 | IsInGroup |
| `POST /Ping` | `{"Ping": long}` (milliseconds) | 204 | HasAccess only |
| `GET /GetUtcTime` (root) | - | 200 `{"RequestReceptionTime","ResponseTransmissionTime"}`, no auth | none |

`GroupInfoDto`: `{"GroupId","GroupName","State","Participants":[username...],"LastUpdatedAt"}`.
`Participants` is distinct usernames, so two sessions of one user appear once. [source][live]
`/New` trims the name; empty is allowed; more than 200 characters is 400. [live]

## 3. Session identity, policy, errors

**Which session a request acts for.** `RequestHelpers.GetSession` keys on the auth claims:
session id = MD5 of `Client + DeviceId`, so one per client name and device. It is created on
demand if absent. [source] The WebSocket connects with `?ApiKey=` and resolves the same
session through the token's device record. [source][live] The id is stable across
reconnects and logins. [live] SyncPlay membership is **per session**, not per user: a second
device of the same user is a separate member or non-member. [live]

**Policy.** `UserPolicy.SyncPlayAccess`: `CreateAndJoinGroups` (default; `partner` got it
without being set [live]), `JoinGroups`, `None`. [source]

| Policy | Passes when |
| --- | --- |
| HasAccess | access is not `None`, **or** the user is active in a group |
| CreateGroup | `CreateAndJoinGroups` |
| JoinGroup | `CreateAndJoinGroups` or `JoinGroups` |
| IsInGroup | the **user** has an active-session count above 0 (not this session) |

Every endpoint also requires HasAccess at class level. [source]

**Errors: mostly a 204 plus a WebSocket update.** Treat HTTP success as "request accepted",
never as "it worked". State changes come only over the socket.

| Case | Result |
| --- | --- |
| Join an unknown group | 204, then `Type: "GroupDoesNotExist"` [live] |
| Join a group whose queue you can't access | 204, then `LibraryAccessDenied` [source] |
| Any playback request while this session is not in a group | 204, then `NotInGroup` [live] |
| Same, and the user is active in no group | 403 empty body from IsInGroup; `Ping` still 204 + `NotInGroup` [live] |
| No auth header, or invalid token | 400 `Error processing request.` on `/SyncPlay/List` (not 401) [live] |
| Missing JSON body where one is required | 415 [live] |
| `GroupName` too long or null | 400 validation problem [live] |
| `GET /SyncPlay/{id}` unknown or non-GUID | 404 [live] |
| `SetNewQueue` with an item that doesn't exist | 500, and the group is left in `Waiting` [live] |
| `SetNewQueue` empty, or `PlayingItemPosition` out of range | 204, silently nothing [live] |
| Access-denied item in `SetNewQueue` or `Queue` | 204, silently nothing [source] |

Do not use IsInGroup as a membership signal. The counter leaks: re-`Join` of a group you are
already in increments it with no matching decrement. [source] `partner` passed IsInGroup
after that test [live], and `admin` already passed it before any probe ran [live].
`NotInGroup` over the socket is the real signal.

Do not send `Ping` before `GroupJoined`; it just produces a `NotInGroup`. [live]

## 4. Time sync

- `GET /GetUtcTime` needs no auth. Handler reads the clock at entry and exit, so both fields
  are effectively one sample (identical in the response below). [source][live]
  ```json
  {"RequestReceptionTime":"2026-11-08T20:09:51.3987244Z","ResponseTransmissionTime":"2026-11-08T20:09:51.3987244Z"}
  ```
- NTP-style estimate. With local send `t0`, server `t1`, `t2`, local receive `t3`:
  `offset = ((t1 - t0) + (t2 - t3)) / 2`, `rtt = (t3 - t0) - (t2 - t1)`, and
  `serverNow = localNow + offset`. [inferred from the field names and the source comment
  "Implementing NTP on such a high level"] Take several samples, keep the lowest-RTT one, and
  re-sample periodically. [inferred] Local probes measured RTT 2-3 ms, offset within 1 ms. [live]
- **`When` on a command** is the server time the command takes effect; `NewSyncPlayCommand`
  sets it from the group's `LastActivity`. **`EmittedAt`** is the server time the message was
  built. Together with `offset` they give the transit time. [source]
  - `Unpause`: `When` is in the future: start at `When` from `PositionTicks` (the source says
    "Clients will start playback at LastActivity from PositionTicks"). `PositionTicks` is the
    position *at* `When`. If `serverNow > When`, the correct position is
    `PositionTicks + (serverNow - When)`. [source]
  - `Pause`, `Seek`, `Stop`: `When` is usually about `EmittedAt` but **can be old**. Resends
    and corrections keep the original `When` (Seek correction `when=42.239 emit=45.763`,
    Stop in Idle `when=39.588 emit=43.645`). Do not schedule on it. [live] The exception is
    `Pause` sent to a member that is Ready while others buffer: `When = now + delay` ("pause
    when you reach the group's position"), which only matters if you reported `IsPlaying: true`.
    [source]
- **`Ready.When`** is the moment `PositionTicks` was sampled, in **server** time. The server
  computes `elapsed = serverNow - When`. If `|elapsed| > 2000 ms` it logs a warning and
  uses 0. If `IsPlaying` is false it also uses 0. [source] `Buffering.When`, `.PositionTicks`
  and `.IsPlaying` are never read for timing. [source]
- **`/SyncPlay/Ping`** stores `Ping` (ms) on the member (default 500). `GetHighestPing` feeds
  the start delay; the value is never validated. [source]
  - Unpause path: `delay = max(highest * 2, 500)` ms. [source] Measured `When-EmittedAt`: 1000 ms
    with default pings, 500 ms with 80/30 and 10/10. [live]
  - Ready path when everyone becomes ready: `delay = highest * 2` ms (the floor is a unit bug,
    `DefaultPing` added as ticks). Measured 1000 ms with default pings, about 20 ms with 10/10. [live]
  - A member that never pinged sits at 500 and pushes every start to 1 s. [live]
  - Send `Ping` once right after `GroupJoined`, then periodically. The value is most likely a
    one-way estimate (RTT / 2), because the server doubles it. [inferred]

## 5. WebSocket messages

Envelope: `{"MessageId":"...","Data":{...},"MessageType":"..."}`. Examples are verbatim from
the probes (no tokens) except that `MessageId` is dropped.

### `SyncPlayCommand`, `Data` = `SendCommand`

`GroupId`, `PlaylistItemId`, `When`, `PositionTicks` (`long?`, always a number in practice
[source]), `Command` (`Unpause` | `Pause` | `Stop` | `Seek`), `EmittedAt`.

```json
{"Data":{"GroupId":"08fbe1891d374bdcbd2be755283ca713","PlaylistItemId":"3d0ace87ceea4c8ba295174761730fad","When":"2026-11-08T20:10:03.941608Z","PositionTicks":0,"Command":"Unpause","EmittedAt":"2026-11-08T20:10:02.9420287Z"},"MessageType":"SyncPlayCommand"}
```

`PlaylistItemId` is `"000...0"` when there is no current item (for example the `Stop` sent on
joining an idle group, and after any operation that loses the playing item). [live]

### `SyncPlayGroupUpdate`, `Data` = `{"GroupId","Data","Type"}`

| `Type` | Inner `Data` | Who | Notes |
| --- | --- | --- | --- |
| `GroupJoined` | `GroupInfoDto` | new member | `State` is the state before the join [live] |
| `UserJoined` | username string | others | [live] |
| `UserLeft` | username string | others | [live] |
| `GroupLeft` | dashed group id string | the leaver | [live] |
| `StateUpdate` | `{"State","Reason"}` | all | see below |
| `PlayQueue` | `PlayQueueUpdate` | varies | see below |
| `NotInGroup` | `""` | caller | `GroupId` is all zero [live] |
| `GroupDoesNotExist` | `""` | caller | `GroupId` is all zero [live] |
| `LibraryAccessDenied` | `""` | caller | carries the real `GroupId` [source] |

```json
{"Data":{"GroupId":"08fbe1891d374bdcbd2be755283ca713","Data":{"GroupId":"08fbe1891d374bdcbd2be755283ca713","GroupName":"probe","State":"Idle","Participants":["admin","partner"],"LastUpdatedAt":"2026-11-08T20:09:55.4445569Z"},"Type":"GroupJoined"},"MessageType":"SyncPlayGroupUpdate"}
{"Data":{"GroupId":"08fbe1891d374bdcbd2be755283ca713","Data":"partner","Type":"UserJoined"},"MessageType":"SyncPlayGroupUpdate"}
{"Data":{"GroupId":"08fbe1891d374bdcbd2be755283ca713","Data":"admin","Type":"UserLeft"},"MessageType":"SyncPlayGroupUpdate"}
{"Data":{"GroupId":"08fbe1891d374bdcbd2be755283ca713","Data":"08fbe189-1d37-4bdc-bd2b-e755283ca713","Type":"GroupLeft"},"MessageType":"SyncPlayGroupUpdate"}
{"Data":{"GroupId":"00000000000000000000000000000000","Data":"","Type":"NotInGroup"},"MessageType":"SyncPlayGroupUpdate"}
{"Data":{"GroupId":"08fbe1891d374bdcbd2be755283ca713","Data":{"State":"Playing","Reason":"Ready"},"Type":"StateUpdate"},"MessageType":"SyncPlayGroupUpdate"}
```

**`StateUpdate`** `State`: `Idle` | `Waiting` | `Paused` | `Playing`. Although `Reason` is
typed as the 17-value `PlaybackRequestType`, only five are ever sent: `Unpause`, `Pause`,
`Seek`, `Buffer`, `Ready`. [source; all five live] Going to Idle emits **no** StateUpdate,
only the `Stop` command. [live] It is informational; commands and `PQ` are what the client acts on.

**`PlayQueue`** fields: `Reason`, `LastUpdate`, `Playlist` (`[{"ItemId","PlaylistItemId"}]`),
`PlayingItemIndex` (`-1` when none), `StartPositionTicks`, `IsPlaying`, `ShuffleMode`,
`RepeatMode`.

```json
{"Data":{"GroupId":"08fbe1891d374bdcbd2be755283ca713","Data":{"Reason":"NewPlaylist","LastUpdate":"2026-11-08T20:09:59.4080747Z","Playlist":[{"ItemId":"b2ed888f622e294c315858260487f63c","PlaylistItemId":"3d0ace87ceea4c8ba295174761730fad"},{"ItemId":"dfe80955875a66f40e5efb7c4a1758d7","PlaylistItemId":"d7f6bf48ce4041f5969b12ae55a79dd1"},{"ItemId":"0f3ab935188814fb242d519af6def96e","PlaylistItemId":"6c738b56730740468e5fee246869e457"}],"PlayingItemIndex":0,"StartPositionTicks":0,"IsPlaying":false,"ShuffleMode":"Sorted","RepeatMode":"RepeatNone"},"Type":"PlayQueue"},"MessageType":"SyncPlayGroupUpdate"}
```

| `Reason` | Sent on | Audience | Playing item changes? | Group state |
| --- | --- | --- | --- | --- |
| `NewPlaylist` | `SetNewQueue`, `Unpause` in Idle | all | yes | Waiting |
| `NewPlaylist` | on `Join` (except Idle) | joiner only | it's new to the joiner | Waiting |
| `NewPlaylist` | `New` while the creator reports playback | creator | see section 8 | Waiting |
| `SetCurrentItem` | `SetPlaylistItem` | all | yes | Waiting |
| `SetCurrentItem` | **fix-up** after your Ready/Buffering carried a wrong pid | caller only | no; `LastUpdate` unchanged [live] | Waiting |
| `NextItem`, `PreviousItem` | those requests | all | yes | Waiting |
| `RemoveItems` | `RemoveFromPlaylist` | all | only if the playing item was removed | unchanged, or Idle if the queue empties |
| `MoveItem`, `Queue`, `QueueNext`, `RepeatMode`, `ShuffleMode` | those requests | all | no | unchanged |

All ten live. Semantics: [source][live]

- `IsPlaying` is `state == Playing` at send time, so it is **false on every item-changing
  update**. In the item-changing cases `StartPositionTicks` is where to start.
- Non-item-changing updates while Playing carry an extrapolated position and `IsPlaying: true`.
  They need no Ready.
- `PlayingItemIndex` indexes `Playlist` as sent (in shuffle order when shuffled). The current
  pid is `Playlist[PlayingItemIndex].PlaylistItemId`. The pid of an entry is stable for its
  lifetime and identical on all members; a new `SetNewQueue` issues new ones. [live] The same
  `ItemId` can appear twice with different pids. [source]
- Stale filter: drop an update whose `LastUpdate` is **older** than the one applied, and accept
  equal, since the fix-up reuses it. [live][inferred]

## 6. State machine

The group is one of `Idle`, `Waiting`, `Paused`, `Playing`. No server timer ever moves it, and
nothing tracks real playback; only requests and membership changes do. [source]

Each `Waiting` holds `ResumePlaying` (go to Playing when all ready, else Paused).
Each member has `IsBuffering` and `IgnoreGroupWait`.

| Request | Idle | Waiting | Paused | Playing |
| --- | --- | --- | --- | --- |
| `SetNewQueue` | to Waiting, resume; `PQ NewPlaylist` all; all buffering | same; invalid queue → back to prior state silently, or Idle if already Waiting [source] | same as Idle | same as Idle |
| `SetPlaylistItem` | to Waiting | as above, `PQ SetCurrentItem` | to Waiting | to Waiting |
| `NextItem`, `PreviousItem` | to Waiting | right pid and item exists: `PQ` all, all buffering, resume=true. Wrong pid: **silent, stays Waiting**. No next: back to prior state, **Idle if already Waiting** [live] | to Waiting | to Waiting |
| `Unpause` | to Waiting, restart at 0, `PQ NewPlaylist` all [live] | resume already set: **force start**, all flags cleared, Playing with buffering ignored until next change, `Unpause` all [live]. Else resume=true, `StateUpdate Waiting/Unpause` [live] | to Playing, `Unpause` all | `Unpause` to caller only (resend) [live] |
| `Pause` | `Stop` to caller | resume=false; `StateUpdate Waiting/Pause`, no command [live] | `Pause` to caller only [live] | to Paused, `Pause` all, `StateUpdate Paused/Pause` |
| `Stop` | `Stop` to caller | to Idle, `Stop` all | to Idle, `Stop` all | to Idle, `Stop` all [live] |
| `Seek` | `Stop` to caller [source] | `Seek` all, all buffering, `StateUpdate Waiting/Seek` | to Waiting (resume=false), same | to Waiting (resume=true), same [live] |
| `Buffering` | `Stop` to caller | caller buffering; `Pause` to caller if resume is false; `StateUpdate Waiting/Buffer` [live] | to Waiting (resume=false), `Pause` to caller | to Waiting (resume=true), position frozen, `Pause` to all non-buffering, `StateUpdate Waiting/Buffer` [live]; ignored after a forced start [live] |
| `Ready` | `Stop` to caller [live] | see section 7 | `Pause` to caller, no pid check [live] | `Unpause` to caller, no pid check [live] |
| `SetIgnoreWait` | sets flag | sets flag; if nobody else blocks, Playing or Paused [live] | sets flag | sets flag |
| Queue edits, modes | `PQ` all | `PQ` all | `PQ` all | `PQ` all |
| `Ping` | stored | stored | stored | stored |

Sending the request that matches the current state is the "resync me" path: `Unpause` in
Playing or `Pause` in Paused resends the state to the caller only. [source][live]

Commands are sent to the **whole group including the requester**. [live] A client must apply
the broadcast, not the local action.

Buffering while Playing with the **wrong** pid still moves the group to Waiting, sends a
fix-up `PQ` to the caller only and tells nobody else: position is not frozen, the other
member keeps playing, and the **next Ready pauses the whole group** (`StateUpdate Paused/Pause`).
[live] Always send the current pid.

## 7. What a client must do

### What each message obliges

| Received | Client does | Then send |
| --- | --- | --- |
| `PQ` with the playing item changed (or first load) | load `Playlist[PlayingItemIndex]` at `StartPositionTicks`, **paused** | `Ready` when loaded and seeked |
| `Seek` | seek to `PositionTicks` exactly, stay paused | `Ready` when seek done |
| `Pause` | pause, then seek to `PositionTicks` | nothing |
| `Unpause` | at `When`, play from `PositionTicks` (late: add `serverNow - When`) | nothing |
| `Stop` | stop local playback | nothing |
| `StateUpdate`, `UserJoined`, `UserLeft` | show on the UI | nothing |
| `PQ` other reasons | update the local queue | nothing |

`Pause` to non-joining members during a join needs no Ready: they are not marked buffering.
[source] A Ready sent anyway is harmless (it just gets another `Pause`). [live]

### Ready

```
POST /SyncPlay/Ready
{"When": serverNow, "PositionTicks": <current player position>, "IsPlaying": <player unpaused?>, "PlaylistItemId": <current pid from latest PQ>}
```

Send it after each of: an item-changing `PQ` once loaded and positioned; a `Seek` once
completed; your own `Buffering` once you can play again. [source]

Server handling in `Waiting` [source; live where noted]:

1. Wrong pid: `PQ SetCurrentItem` to you, you stay buffering, nothing else. Reload or re-Ready
   with the id in that update. [live]
2. Position is clamped to the item runtime, then
   `clientPos = position + elapsed` (elapsed is 0 if `IsPlaying` is false or clocks are over 2 s apart),
   `delay = groupPosition - clientPos`.
3. If the group will resume:
   - `IsPlaying` false and `|delay| > 500 ms`: **`Seek` correction** to you, plus `StateUpdate
     Waiting/Ready` to all, and you stay buffering. [live]
   - Otherwise you are ready. Others still buffering: you get `Pause`, `When = now + delay`.
     [live]
   - All ready: `delay > highestPing * 2`: `Unpause` with `When = now + delay` to everyone
     except you, or to everyone if you are paused (you catch up, others wait for you). [live]
     Otherwise `Unpause` all at `now + highestPing * 2`. Then to Playing, `StateUpdate
     Playing/Ready`. [live]
4. If the group will stay paused:
   - `|group - reported| > 500 ms`: **`Seek` correction**. [live] This branch ignores `elapsed`.
   - All ready: to Paused. If the wait began in Playing or Paused, `Pause` to all
     and `StateUpdate Paused/...` follow; if it began in Waiting (a just-created group with a
     paused creator) the transition is **silent**. [source]

So seeks must be exact. A keyframe seek that lands more than 500 ms off produces a Seek /
Ready loop. [live for one round; loop inferred]

### Buffering

`POST /SyncPlay/Buffering` with the same body. Only `PlaylistItemId` matters. [source] Send it
when playback stalls while the group runs. It pauses every non-buffering member at the
computed position. [live] Then send `Ready` once resumed. If you kept playing, send
`IsPlaying: true` and your position: others resume at `now + delay` when you catch up, and
you get no `Unpause` command, only `StateUpdate Playing/Ready`. [live]

**Do not send Buffering for a seek or load the server initiated.** Everyone is already
marked buffering there and it would only add `StateUpdate` noise. [source]

### A member that never reports Ready

There is no timeout. Six seconds without a Ready: still Waiting, forever by source. [live]
Exits: [source; live except new queue]

- Any member sends `Unpause`: forced start, everyone gets `Unpause`, buffering is ignored
  until the next state change. Late Ready gets `Unpause` for itself. [live]
- The laggard sends `SetIgnoreWait: true`, which **only ever applies to the requesting session**.
  [source] It releases the group at once if nobody else blocks. [live]
- The laggard leaves or its session ends. Group proceeds, `Unpause` all, `StateUpdate
  Playing/Unpause`, arriving at the leaver just before its `GroupLeft`. [live]
- A new `SetNewQueue` / `Seek` / item change restarts the wait, so these do not help.

`SetIgnoreWait` persists until set false. [live] An ignored member still receives every command
[live]. But if it sends `Buffering` while Playing, the group still goes to Waiting and only a
Ready or Unpause gets it out, so an ignored member must not send Buffering. [live]

## 8. Flows

### Create and join

`POST /New` → 200 and `GroupJoined`, often **before** the HTTP response. [live] Then:

- Creator not playing: Idle, plus a `Stop` command to the creator. [live] Do not treat it as
  "stop what I'm doing": it is the normal notification for an empty group. [inferred]
- Creator reporting playback (`Sessions/Playing` seen): the group seeds itself from the
  session's `NowPlayingQueue` and `FullNowPlayingItem`, `RunTimeTicks`, `PositionTicks` and
  starts `Waiting` with `ResumePlaying = !IsPaused`. Creator gets `GroupJoined` (`State:
  Waiting`), `PQ NewPlaylist`, and must Ready. [live]
- **mpvfin sends no `NowPlayingQueue` today, so this seeds an empty queue**: `PQ` with
  `PlayingItemIndex: -1`, `Playlist: []`, and all later commands carry the zero pid; a Ready with
  zero pid goes to Playing with no item. [live] Fix by sending `SetNewQueue` right after `New`
  with mpv's playlist: it is valid from Waiting and gives real pids. [live] Sending
  `NowPlayingQueue` in reports would also work. [live] `NowPlayingQueue` only ever changes on
  a report with a non-empty one, and on `Stopped` if present, so it survives until replaced. [source]

### Join a group

`POST /Join` → 204. To the joiner, in order: [live]

1. `GroupJoined`, `Data.State` is the pre-join state.
2. Idle group: `Stop`, and that's all; wait for a queue.
3. Otherwise: `PQ NewPlaylist`, `IsPlaying: false`, `StartPositionTicks` = frozen group position.

Group side, same instant: others get `UserJoined`, then non-buffering members get `Pause` at the
frozen position; the group is Waiting with resume restored. [live]

Joiner must, in order: (a) `Ping`; (b) load the item paused at `StartPositionTicks`;
(c) `Ready` with `IsPlaying: false`, position within 500 ms, current pid; (d) on `Unpause`, start at `When`.
A wrong position yields `Seek`. [live] `Unpause` fans out to all once the joiner is ready, 1 s out
with default pings. [live] Other members do nothing but honour `Pause` then `Unpause`.

Joining a Paused group is the same but ends Paused: `Pause` all and `StateUpdate Paused/Ready`. [live]

Joining when already a member (`Restore session`) re-sends `GroupJoined` and `PQ`, tells others
`UserJoined`, re-enters Waiting. [live] Joining another group leaves the first: `GroupLeft` for
the old, `GroupJoined` for the new. [live]

Groups are listable by anyone with access to the group's whole queue. [source]

### New queue

Any member: `SetNewQueue`. `PlayingQueue` is taken as is, not expanded. [live] To all
including sender: `PQ NewPlaylist`; group Waiting, resume=true, everybody buffering. Each loads
and Readies. [live] An error returns 204 and nothing, or 500 for a bad id (section 3).

`StartPositionTicks` is **not** validated: 300 s on a 120 s item is sent to all, then every
Ready is clamped to the runtime and triggers `Seek` to 300 s; the group cannot start. [live]
Clamp before sending.

Next and previous, and `SetPlaylistItem`, have the same shape: `PQ` with their reason, all
buffering, resume=true (they always end up playing, even from Paused). [source][live]
`SetPlaylistItem` with an unknown id silently sets the playing index to -1 and position to 0;
the group stays in its old state with no item; later commands carry the zero pid. [live]

### End of an item

**The server has no end-of-file notion.** No timer, no report, no position check. [source]

- Whichever member hits the end sends `NextItem` with the pid of the item that just ended. The
  first one wins; a duplicate carrying the old pid is silently ignored. [live] Do it for every
  item end.
- A client must **not** advance on its own, since the group's queue is authoritative: after
  `PQ NextItem`, load `Playlist[PlayingItemIndex]`, then `Ready`.
- **End of queue:** `NextItem` for the last item does nothing, sends nothing, and the group stays
  `Playing` indefinitely with position still advancing. [live] Recommended: after the last item
  ends locally, a member sends `/SyncPlay/Stop`, which gives everyone `Stop`. [inferred] The web
  client, alone in a group, sent no SyncPlay request at all at the end of the queue (section 14).
  [live]
- **Stale `NextItem`/`PreviousItem` from Playing or Paused moves the group to Waiting with no
  update and no one buffering.** It stays there until someone sends a Ready. [live] Only send them
  with the current pid.

### Leaving, destruction, disconnect

- `POST /Leave`: `GroupLeft` to you, `UserLeft` to others. [live] Group continues; there is no
  owner. In Waiting, leaving can resume the group, and an `Unpause` may arrive just before your
  `GroupLeft`. [live] Ignore commands once you've asked to leave.
- A group is destroyed when the last member leaves: the last member gets `GroupLeft`, nothing else
  says so; `GET /SyncPlay/{id}` is 404. [live]
- **There is no kick.** The only way out is the session ending. [source] Logout gives others
  `UserLeft`. [live]
- **WebSocket closed: the session ends immediately** if it was the last socket
  (`CloseIfNeededAsync` → `SessionEnded` → `LeaveGroup`). Others get `UserLeft`; the session
  disappears from `/Sessions`. [live] A drop during Waiting releases the group by the same path.
  [source] A reconnect gets the **same session id** but is **not in the group**; `Ready` gives
  `NotInGroup`. [live]
  - To recover: repost Capabilities (session is new), re-`Join`, then the normal join flow. Joining
    pauses the others and re-enters Waiting. [live] `GroupDoesNotExist` means the group died.
  - Opening a new socket before closing the old keeps the session and membership (no `UserLeft`).
    [live] Not achievable after a dropped connection; `mpvfin` could use it on a planned restart.
- A socket that dies without a FIN (no close frame) stays "open" to the server; `KeepAliveSockets`
  removes it from the watch list but does not end the session, with a `TODO` to that effect.
  So a dead peer can hold a group in Waiting until TCP fails. [source] Only then does the group
  release.

## 9. Playback reports and group membership

- `PlaystateController` has no SyncPlay reference. [source] `Group` reads a session's report data in
  exactly one place: `CreateGroup`. Reports do not move group state [live: a `Stopped` report left
  the group `Playing`].
- SyncPlay works with no reports at all: the probes sent none in most scenarios. [live]
- Members should keep reporting exactly as now: `Sessions/Playing` on load, `/Progress` about every
  10 s and on changes, `/Stopped` once. Other server features (now-playing, resume, played state)
  and the 5-minute idle check still rely on them. [source]
- `PlaySessionId`: from the member's own `PlaybackInfo`, per item as today; it is unrelated to the
  group. [source]
- Report `PlaylistItemId` is a free-form string stored on the session (a probe sent
  `"playlistItem7"`), unrelated to SyncPlay's pid. [live] Reports may omit it [live], and SyncPlay
  pids do not need to appear in reports. [inferred]

## 10. Remote control of a member

`POST /Sessions/{id}/Playing` and `/Playing/{Command}` are **not** SyncPlay-aware: `SendPlayCommand`
and `SendPlaystateCommand` never look at groups. [source] A member gets the plain `Play` or
`Playstate` message and the group state is untouched. [live]

```json
{"Data":{"ItemIds":["dfe80955875a66f40e5efb7c4a1758d7","0f3ab935188814fb242d519af6def96e"],"StartPositionTicks":50000000,"PlayCommand":"PlayNow","ControllingUserId":"21982626c36a41e5a2a8d40b0b66feeb"},"MessageType":"Play"}
```

`Play` was expanded to the rest of the series even for a group member. [live] If the member acted on
it locally the group would never know, and members would diverge. Recommended while in a group:
[inferred]

- `Play` PlayNow → `SetNewQueue` (`ItemIds`, `StartIndex`, `StartPositionTicks`).
- `Play` PlayNext / PlayLast → `/SyncPlay/Queue` `QueueNext` / `Queue`.
- `Playstate` Pause/Unpause/Seek/Stop/NextTrack/PreviousTrack → the matching `/SyncPlay/*` request.

The same mapping applies to local mpv key presses: request, then apply the broadcast. Whether the
official clients do this is open.

## 11. Surprises and required ordering

1. WS `GroupJoined` usually beats the HTTP `200` from `New`. Drive state only from the socket. [live]
2. Requests take effect in arrival order and answers are WS-only. Keep `Ready` off any queue that
   can stall behind a slow job (mpvfin's `jobs` thread runs in submission order): `When` is stamped
   client-side and a Ready over 2 s late loses its elapsed-time correction. POSTs usually took
   15-40 ms and `GET /SyncPlay/{id}` 20-230 ms, but both spiked occasionally on this server: one
   `Ready` POST took about 3.6 s, another about 1.2 s, and a GET 3.3 s. [live][inferred]
3. Polling `GET /SyncPlay/{id}` or `/List` re-evaluates access on every queue item; use the socket. [source]
4. Pause requested during a Waiting that began in Playing (e.g. Seek then Pause): when ready, the
   server adds the elapsed time since the seek, so `Pause` jumped from 25 s to 31.4 s after 6.4 s
   of waiting. [live] Always seek to the `PositionTicks` given with a `Pause`.
5. `Seek` is clamped to the runtime [live]; `SetNewQueue`'s start position is not [live].
6. `Ready` in Paused or Playing skips the pid check [live]; in Waiting and for `Buffering` it is enforced.
7. Empty-queue states are reachable and legal: `RemoveFromPlaylist` of everything (`PQ` index -1,
   group goes Idle) [live]; `New` while playing without a queue [live].
8. Removing the playing item while Playing replaces it and resets position with **no** Waiting:
   `PQ RemoveItems`, `StartPositionTicks` about 0, `IsPlaying: true`, group stays Playing. Load the
   new item with no Ready expected. [live]
9. Whoever joins sets `Participants` by username; the same user on two devices appears once, and
   `UserJoined` shows the shared name. [live] Don't key anything on these strings.
10. Group creation and all requests are open to any member; there is no leader. [source]
11. `/Sessions/Playing` etc. keep `FullNowPlayingItem` and `NowPlayingQueue` on the session, and the
    server's idle sweep stops sessions with no report for over 5 minutes. [source]

## 12. Notes for mapping onto mpvfin [inferred]

- Keep one `SyncPlay` state in the reducer keyed by the latest `PQ`: pid per mpv playlist entry.
  `mpvfin://<itemId>` is not unique if an item is queued twice, so correlate by playlist position
  or pid, never by item id.
- mpv must not auto-advance into the next entry while grouped. Per-file `keep-open` or holding at
  `end-file` are the options; a global option is not allowed by the architecture invariants.
- Use `seek absolute+exact` for server seeks, and report `Ready` from the `playback-restart` that
  follows, with `pause=yes`, using `time-pos`.
- `paused-for-cache` is the natural `Buffering` trigger, once and only while the group is Playing.
- Local pause/seek/next must become requests. The current "confirm with mpv, then report" flow is
  the wrong direction while grouped.

## 13. Open questions

1. What the official clients actually send. Partly answered in section 14: `Ready` is sent with
   `IsPlaying: true` as well as `false`. Still open: the `Ping` value (no `POST /SyncPlay/Ping` was
   seen in a 40 s run), when `Buffering` is sent (none was), and drift-correction thresholds.
2. What official clients do at end of queue. Answered for the web client alone in a group, in
   section 14: it sends nothing. With other members, and for other clients, still open.
3. Whether official clients translate a remote `Play` into `SetNewQueue` while grouped.
4. `SyncPlayAccess: None` and `JoinGroups` behaviour and `LibraryAccessDenied` are source-only, because
   testing them means changing `partner`'s policy, which was not part of the brief.
5. Half-open socket behaviour was read from source, not provoked.
6. Why SyncPlay HTTP calls (GET and POST) sometimes took seconds on the test server.
7. Whether a server restart ends groups cleanly (groups are in-memory, so they vanish; reconnect
   behaviour not probed).

## 14. Checked against the real web client

2026-11-09, server 10.11.11: the server's own web client in headless Chrome (Playwright,
`channel: 'chrome'`, `--disable-gpu`, `--autoplay-policy=no-user-gesture-required`, direct play of
H.264) logged in as `partner`, and a real mpv running the plugin as `admin`. The web client created
the group and played E01; mpv joined from its menu. Only the web UI was driven, and its source was
not read. Four runs, all alike, the last after the fix to a viewer's resume. [live]

| Check | Result |
| --- | --- |
| Browser pauses when mpv joins | within 40 ms of mpv's choice |
| Both start after the join | 1.15 to 1.18 s after that pause |
| Start offset, mpv minus browser | 0, -30, 0 and 0 ms (sampled about every 30 ms) |
| Steady offset while playing, mpv minus browser | +30 to +130 ms, typically +80 ms |
| Offset while both are paused | +9 to +56 ms |

| Direction | Pause | Resume | Seek forward | Seek backward |
| --- | --- | --- | --- | --- |
| Browser to mpv | followed in about 50 ms | followed in about 1 s | followed in about 40 ms | followed in about 40 ms |
| mpv to browser | followed in about 40 ms | followed in about 1 s | followed in 40 to 80 ms | followed in 40 to 80 ms |

Times are from the action to the other side's change being seen. Resuming takes about 1 s because
the server schedules it. After every action the offset returned to the steady range above.

What the web client POSTs to `/SyncPlay/`, seen from its requests:

- `New` with `{"GroupName":"partner's group"}`, then `SetNewQueue`. Playing E03 from its page sent
  E03 alone; playing E01 from its page gave a queue of the three episodes.
- `Ready` after a load with `IsPlaying: false`, then again about 1 s later with `IsPlaying: true`.
- After a `Seek`, two `Ready` about a millisecond apart (`IsPlaying: true`, then `false`), and a
  third with `IsPlaying: true` about 1 s later.
- No `Ping` and no `Buffering` in a 40 s run.
- At the end of the last item of the queue, alone in the group: no request at all, neither `Stop`
  nor `NextItem`. The player closed and the item's page came back.

Not checked: any other official client, `Buffering` under a real stall, and a remote `Play` while
grouped (open question 3).
