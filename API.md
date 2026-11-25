# Backseat local API

Backseat can be controlled by other programs over HTTP: a phone remote, a Stream Deck
button, a script, an AI agent. They see your rooms, who is in them, voice and streams, and
can open a room, join or leave voice, mute, set someone's volume, start, change or stop a
stream, and watch someone.

The API is served by the Backseat app itself, on your computer. There is no cloud part.

- Version: `v1` (all paths start with `/api/v1/`). Backseat 0.2.26 and newer.
- Format: JSON in, JSON out, UTF-8.
- Off by default.

## Turning it on

Settings → **Remote control** (a tab of its own, so the token is not on screen whenever
the settings are open, on a stream for example).

| Option | Meaning |
|---|---|
| Turn on remote control (local API) | Starts the API server. |
| Also from other devices on my network | Who can reach it. On (the default): this PC and other devices on your Wi-Fi or home network, a phone for example (listens on `0.0.0.0`). Off: only programs on this PC, such as scripts or the Stream Deck app (`127.0.0.1`); a phone cannot. |
| Address | What to type into the other device, e.g. `http://192.168.1.20:47820`. |
| Token | The secret every call needs, shown as dots until **Show**. **Copy** puts it on the clipboard either way, **New token** makes a new one and the old one stops working at once. |

The first time the API listens on the local network, Windows Firewall may ask whether
Backseat may accept connections on private networks. Allow it for private networks, or the
phone will not get through.

The settings live in `%LOCALAPPDATA%\Backseat\state.json` under `settings.api`:

```json
"api": { "enabled": true, "lan": true, "port": 47820, "token": "32 hex digits" }
```

The port (default **47820**) can only be changed there, then turn the API off and on again
(or restart Backseat).

## Authentication

Every call except `GET /api/v1/info` needs the token, in either form:

```
Authorization: Bearer 3f6c0e...
GET /api/v1/status?token=3f6c0e...
```

Prefer the header: a token in the address ends up in browser history and logs.

A wrong or missing token gets `401` (after a short pause). Anyone who has the token can do
everything below, including streaming your screen, so treat it like a password. The API
has no TLS: on the local network the token travels in the clear, so use it on networks you
trust (home Wi-Fi), not on public ones.

Browsers: every response allows any origin (`Access-Control-Allow-Origin: *`), and
`OPTIONS` preflights are answered, so a web page (for example one on your phone) can call
the API with `fetch` and the `Authorization` header.

## Conventions

- Reads are `GET`, actions are `POST`. A `POST` body is optional JSON; send
  `Content-Type: application/json`. No body is the same as `{}`.
- Every response has `"ok": true` or `"ok": false` with `"error": "text"`.
- Actions answer with the same object as `GET /status` (plus `"ok"`), taken after the
  action, so one call is enough to update a remote's screen.
- Actions that take time (opening a room, joining voice, starting a stream, watching) answer
  when done: up to ~30 s for a room, usually under 2 s.
- Path parts are URL-encoded: room codes may contain spaces (`Evening%20games`).
- People are identified by `id` (a number, from `room.members`). Ids belong to the
  connection: they change when someone (or you) joins the room again.
- The app's own window stays in step: what the API does shows there as if clicked.

Status codes:

| Code | When |
|---|---|
| 200 | Done. |
| 400 | Bad input (unknown mode, bad JSON, missing field). |
| 401 | Wrong or missing token. |
| 404 | Unknown path, room, streamer, monitor or window. |
| 405 | Method other than GET, POST, OPTIONS. |
| 409 | Not possible now (not in a room, already live, a join is running, the join failed, "still running; check /status" after a timeout). |
| 413, 431 | Body over 64 KB, headers over 16 KB. |
| 503 | Over 16 calls at once. |

## Polling

There are no push events in v1. Poll `GET /status`: once a second is fine (it is cheap and
reads the app's state, nothing goes over the internet). For "who is speaking" poll faster,
e.g. 4–5 times a second, while the screen that shows it is open.

## Calls

| Method | Path | What it does |
|---|---|---|
| GET | `/api/v1/info` | App name and versions. No token. |
| GET | `/api/v1/status` | Everything at once: open room, members, voice, your stream, what you watch. |
| GET | `/api/v1/rooms` | Saved rooms (the list on the left of the app). |
| POST | `/api/v1/rooms/{code}/open` | Open (join) a saved room. |
| POST | `/api/v1/room/open` | The same, with `{"room": "code"}` in the body. |
| POST | `/api/v1/room/leave` | Leave the open room. |
| POST | `/api/v1/voice/join` | Join the open room's voice chat. |
| POST | `/api/v1/voice/leave` | Leave voice. |
| POST | `/api/v1/voice/mute` | Microphone off/on. |
| POST | `/api/v1/voice/members/{id}/volume` | How loud member `id` is for you. |
| POST | `/api/v1/voice/deafen` | Sound off/on. While deafened the microphone sends nothing, as in Discord. |
| GET | `/api/v1/sources` | Monitors and windows to stream, codecs, modes, bitrates. |
| POST | `/api/v1/live/start` | Start streaming in the open room. |
| POST | `/api/v1/live/update` | Change the running stream: source, resolution, frame rate, bitrate. Viewers stay. |
| POST | `/api/v1/live/stop` | Stop streaming. |
| POST | `/api/v1/watch/{id}` | Watch the stream of member `id`. |
| POST | `/api/v1/watch/{id}/stop` | Stop watching. |
| POST | `/api/v1/watch/{id}/mute` | That stream's sound off/on. |
| POST | `/api/v1/watch/{id}/volume` | That stream's volume. |
| POST | `/api/v1/watch/{id}/quality` | Ask for a lower quality. |
| POST | `/api/v1/watch/{id}/window` | Fullscreen, pop out, stats overlay of the video window. |

### GET /api/v1/info

No token needed. Use it to check that the address is right.

```json
{ "ok": true, "app": "backseat", "version": "0.2.26", "api": 1 }
```

### GET /api/v1/status

```json
{
  "ok": true,
  "version": "0.2.26",
  "joining": false,
  "reconnecting": false,
  "room": {
    "code": "K7M2Q9XR4TBA",
    "title": "Evening games",
    "server": "wss://example.com/ws",
    "direct": null,
    "reconnecting": false,
    "me": 12,
    "members": [
      { "id": 12, "name": "Alex", "me": true, "streaming": true, "watching": false,
        "in_voice": true, "speaking": false, "mic_muted": false, "deafened": false,
        "volume": null },
      { "id": 15, "name": "Maria", "me": false, "streaming": false, "watching": false,
        "in_voice": true, "speaking": true, "mic_muted": false, "deafened": false,
        "volume": 100 }
    ]
  },
  "background_rooms": [],
  "voice": { "joined": true, "room": "K7M2Q9XR4TBA", "mic_muted": false, "deafened": false,
             "push_to_talk": false, "talking": true },
  "live": {
    "since_secs": 754, "quality": "1080p60 20000", "encoder": "AV1 · NVENC",
    "upload_mbit": 21.4,
    "settings": { "source": { "kind": "monitor", "id": 65537 }, "mode": "1080p60",
                  "kbit": 20000, "codec": "av1", "audio": "system" },
    "viewers": [ { "id": 15, "name": "Maria", "quality": "1080p60 20000",
                   "rtt_ms": 18.0, "loss_pct": 0.0, "problems": [] } ]
  },
  "watching": []
}
```

| Field | Meaning |
|---|---|
| `joining` | A room is being opened right now. |
| `reconnecting` | The connection to the server broke and is being made again (your place is kept). |
| `room` | The open room, `null` when none is open. |
| `room.code` | The room's code: what `/rooms/{code}/open` takes. |
| `room.title` | Its display name, `null` when it has none (show `code` then). |
| `room.server` | The signaling server, `null` if unknown. |
| `room.direct` | `"connected"` or `"waiting"` for a direct connection (beta, no server), else `null`. |
| `room.me` | Your own member id. |
| `room.members[]` | Everyone in the room, you included (`me: true`). |
| `members[].streaming` | They are live; you can watch them. |
| `members[].watching` | You are watching them. |
| `members[].in_voice`, `speaking`, `mic_muted`, `deafened` | Voice chat state. `speaking` is true while their sound is above the speaking threshold. |
| `members[].volume` | How loud they are for you, percent (100: as they speak); `null` for you and for those not in voice. |
| `background_rooms` | Codes of rooms kept connected in the background (you switched away while watching, streaming or in voice there). |
| `voice.joined`, `voice.room` | You are in voice, and in which room's (it may be a background room). |
| `voice.mic_muted`, `voice.deafened` | Your microphone and sound switches. They are kept between voice joins, so they are valid outside voice too. |
| `voice.push_to_talk`, `voice.talking` | Push-to-talk is on; your microphone is sending right now. |
| `live` | Your own stream, `null` when not live. `quality` is "`mode` `kbit`". `problems` are short codes such as `loss:4.7` or `queue:1748`. Right after the start the numbers are 0 and `viewers` empty for up to a second. |
| `live.settings` | What it runs with, in the form `/live/start` takes. |
| `watching[]` | Streams you watch: `streamer` (member id), `name`, `quality` you asked for (`null`: as streamed), `codec`, `fps` received, `latency_ms`, `mbit`, `loss_pct`, `muted`, `volume` (percent), `fullscreen`, `popped_out` (in its own window), `window_closed`. |

Member lists come from the open room only; for a background room you only get its code.

### GET /api/v1/rooms

```json
{
  "ok": true,
  "rooms": [
    { "code": "K7M2Q9XR4TBA", "title": "Evening games", "label": "Evening games",
      "server": "wss://example.com/ws", "my_name": "Alex",
      "open": true, "background": false, "temporary": false }
  ]
}
```

`label` is what the app shows (the title, else the code). `open`: it is the open room.
`background`: kept connected in the background. `temporary`: a room with an end time.
You are online only in the open room and in background rooms.

Rooms are added in the app itself (Join / invite link). The API opens saved rooms only.

### POST /api/v1/rooms/{code}/open

Opens a saved room, as a click on it in the app does. The room you were in stays connected in
the background if you watch, stream or talk there; otherwise it closes. A failed join
leaves you where you were and answers `409` with the reason.

```
POST /api/v1/rooms/K7M2Q9XR4TBA/open
```

Same with a body: `POST /api/v1/room/open` with `{"room": "K7M2Q9XR4TBA"}`.

### POST /api/v1/room/leave

Leaves the open room for good (the others see you leave). Voice, your stream and your
watches there end.

### POST /api/v1/voice/join, /voice/leave

Joins or leaves the open room's voice chat. Joining opens the microphone (the one chosen in
the settings). Voice, watching and streaming happen in one room at a time: joining here
ends them in background rooms. Leaving voice works wherever you are in voice.

### POST /api/v1/voice/mute, /voice/deafen

```json
{ "muted": true }
{ "deafened": false }
```

Without a body each call toggles. Both work outside voice too: they are what the next join
starts with.

### POST /api/v1/voice/members/{id}/volume

```json
{ "volume": 150 }
```

Percent, 0 to 200 (100: as they speak). Only for you; it is kept by their name, so it
comes back next time, as the volume set from the app's voice list. The member must be in
the open room and not you.

### GET /api/v1/sources

What can be streamed and how.

```json
{
  "ok": true,
  "monitors": [ { "id": 65537, "name": "DISPLAY1", "width": 2560, "height": 1440, "primary": true } ],
  "windows": [ { "id": 395274, "title": "Some Game", "exe": "game.exe", "pid": 8120,
                 "minimized": false, "width": 2560, "height": 1440 } ],
  "codecs": ["av1", "h265", "h264"],
  "modes": ["720p30", "720p60", "1080p30", "1080p60", "1440p30", "1440p60"],
  "kbit_presets": [2000, 4000, 8000, 12000, 15000, 20000, 35000, 50000]
}
```

`codecs`: the hardware encoders this PC has, best first. Window and monitor ids are valid
until the window closes or the monitors change; read them again before starting.

### POST /api/v1/live/start

Starts streaming in the open room. Every field is optional; missing ones are chosen as the
app's "Go live" dialog chooses them (the settings' Stream tab first).

```json
{
  "source": { "kind": "window", "id": 395274 },
  "mode": "1080p60",
  "kbit": 20000,
  "codec": "h265",
  "audio": "app"
}
```

| Field | Values | When missing |
|---|---|---|
| `source.kind` | `"monitor"`, `"window"` | `"monitor"` |
| `source.id` | an id from `/sources` | the primary monitor (a window needs an id) |
| `mode` | one of `modes` | the settings', else by the source's height (1440 / 1080 / 720), 60 fps |
| `kbit` | one of `kbit_presets` | the settings', else 20000 (15000 for AV1) |
| `codec` | one of `codecs` | the settings', else the first of `codecs` |
| `audio` | `"system"` (the whole PC's sound), `"app"` (only the window's program), `"none"` | the settings', else `system` for a monitor and `app` for a window |

The whole system's sound leaves out the programs listed in the settings (Discord by
default). Already live: `409`; use `/live/update`.

The simplest call streams the primary monitor with the usual settings:

```
POST /api/v1/live/start
```

### POST /api/v1/live/update

Changes the running stream in place: viewers keep watching. The body is as for
`/live/start`; what is missing stays as it is (see `live.settings` in `/status`).

```json
{ "mode": "1440p60", "kbit": 35000 }
{ "source": { "kind": "window", "id": 395274 } }
```

- A new source keeps the sound choice. With `"app"` (a program's sound) a new window brings
  its own program's sound; a monitor takes the whole system's sound instead.
- The codec, and sound on versus off, cannot change while live: `409`. Stop and start again.
- Viewers who asked for less keep that, unless the new quality is lower still.
- Not live: `409`.

### POST /api/v1/live/stop

Stops your stream; viewers see it end.

### POST /api/v1/watch/{id}

Watches member `id` (they must be `streaming`). The video opens in the app's window as a
click on Watch does.

```json
{ "quality": "720p30 4000" }
```

`quality` is optional: "`mode`" or "`mode` `kbit`". It can only lower what the streamer
sends (never above their own), and snaps to the presets. Missing: the settings' Watch tab,
else as streamed.

### POST /api/v1/watch/{id}/stop

### POST /api/v1/watch/{id}/mute

`{"muted": true}`, or no body to toggle. Only this stream's sound.

### POST /api/v1/watch/{id}/volume

`{"volume": 60}`: percent, 0 to 100, as the video's own volume slider.

### POST /api/v1/watch/{id}/quality

`{"quality": "1080p60"}`, same format as above.

### POST /api/v1/watch/{id}/window

`{"action": "fullscreen"}`, one of:

| Action | Effect |
|---|---|
| `show` | Bring the video window up. |
| `fullscreen`, `windowed`, `toggle_fullscreen` | Fullscreen on the monitor the video is on. |
| `pop_out`, `pop_in` | Into a window of its own, and back into the app. |
| `topmost`, `not_topmost` | Keep a popped-out window above others. |
| `overlay`, `no_overlay` | Detailed stats over the video (Shift+F3). |
| `badge`, `no_badge` | The small stats badge in the corner (F3). |

## Examples

PowerShell:

```powershell
$api = 'http://192.168.1.20:47820/api/v1'
$h = @{ Authorization = 'Bearer 3f6c0e...' }
Invoke-RestMethod "$api/status" -Headers $h
Invoke-RestMethod "$api/voice/mute" -Method Post -Headers $h
Invoke-RestMethod "$api/live/start" -Method Post -Headers $h -ContentType 'application/json' -Body '{"mode":"1080p60","kbit":20000}'
```

curl:

```sh
API=http://192.168.1.20:47820/api/v1
AUTH="Authorization: Bearer 3f6c0e..."
curl -s -H "$AUTH" $API/rooms
curl -s -H "$AUTH" -X POST $API/rooms/K7M2Q9XR4TBA/open
curl -s -H "$AUTH" -X POST $API/voice/join
curl -s -H "$AUTH" -X POST -H 'Content-Type: application/json' -d '{"deafened":true}' $API/voice/deafen
```

JavaScript (a web page on the phone):

```js
const api = 'http://192.168.1.20:47820/api/v1';
const token = '3f6c0e...';
async function call(path, body) {
  const r = await fetch(api + path, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const j = await r.json();
  if (!j.ok) throw new Error(j.error);
  return j;
}
const s = await call('/status');
await call('/voice/mute', { muted: !s.voice.mic_muted });
```

A page served over `https://` cannot call an `http://` address on the local network
(browsers block mixed content); serve the remote's page over `http://` or from a native app.

## Building a remote: a recipe

A Discord-like voice panel (current room, members, mute / deafen / leave, list of rooms):

1. `GET /info` once to check the address.
2. `GET /rooms` for the room list; `GET /status` every second (faster while the voice view
   is open, for `speaking`).
3. Room list item → `POST /rooms/{code}/open`, then `POST /voice/join`.
4. Mic button → `POST /voice/mute` (no body: toggles), headphones → `POST /voice/deafen`,
   leave → `POST /voice/leave`.
5. Members of `status.room.members` with `in_voice` are the voice list; `speaking`,
   `mic_muted`, `deafened` draw their state.
6. Go-live button → `POST /live/start` (or pick from `/sources` first); stop → `/live/stop`.
7. A member with `streaming: true` → Watch → `POST /watch/{id}`.
8. A volume slider per voice member → `POST /voice/members/{id}/volume`; per stream →
   `POST /watch/{id}/volume`.

## Not in v1

- Push events (WebSocket / SSE): poll `/status`.
- Adding new rooms, invites, the direct connection (beta), the app's settings.
- HTTPS.
