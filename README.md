# Backseat

Backseat is a Windows desktop app for streaming games to friends in rooms. The video goes
straight from the streamer's computer to each viewer over a peer-to-peer WebRTC
connection; there is no media server in between. The only server is a small signaling
server that tells the people in a room about each other.

The interface looks and works like a Discord server: rooms in a rail on the left, the
people and streams of the open room in the middle, video inside the main window or in its
own window.

## Features

- **Rooms by invite code.** Create a room and send the invite (a code or a link); only
  people with the code get in. Invite links open the app directly.
- **Several streams per room.** Anyone in the room can go live; anyone can watch any
  stream, several at once.
- **Hardware encoding** on NVIDIA (NVENC), AMD (AMF) and Intel (Quick Sync) graphics, in
  H.264, H.265 or AV1. Only encoders that work on the computer are offered.
- **Quality presets**: 720p, 1080p or 1440p at 30 or 60 fps, with bitrate presets from 2 to
  50 Mbit/s. The streamer sets the maximum; each viewer may pick a
  lower quality, never a higher one.
- **One viewer's problems stay with that viewer.** Each viewer gets their own stream; a
  viewer on a slow link catches up by skipping frames, and the others see nothing of it.
  The streamer sees a mark next to that viewer.
- **Upload planning**: the app shows how much upload the stream needs for the current
  viewers and warns when the next viewer would not fit into your limit.
- **Screen or window capture** with system sound, the sound of one program only, or no
  sound.
- **Low latency**: lost packets are repaired by retransmission, and the viewer shows the
  measured latency in its stats.
- **Change a stream while live**: switch the screen or window, resolution, frame rate or
  bitrate without viewers reconnecting.
- **Sound controls on the video**, also in fullscreen: mute, volume (mouse wheel too).
- **Tray and updates**: closing the window keeps Backseat in the tray; new versions from
  GitHub Releases are installed at start (can be turned off in Settings).
- **English, Ukrainian and Russian** interface; it follows the system language.

## Requirements

- Windows 10 or Windows 11, 64-bit.
- To stream: a graphics card with a hardware video encoder (NVIDIA NVENC, AMD AMF or
  Intel Quick Sync). Watching works on any reasonably recent computer; AV1 streams are
  decoded on the CPU when the graphics card cannot decode AV1.
- Microsoft Edge WebView2 Runtime. It is part of Windows 11; the installer installs it
  when it is missing (this needs internet access).
- A signaling server that everyone in the room can reach (see below).

## Install

Download `backseat-setup-X.Y.Z.exe` from the
[Releases](../../releases) page and run it. It installs for the current user only, so it
needs no administrator rights. `SHA256SUMS.txt` next to it has the checksum:

```powershell
Get-FileHash .\backseat-setup-X.Y.Z.exe -Algorithm SHA256
```

The installer is not code-signed, so Windows SmartScreen may warn about it.

On the first start the app asks for the signaling server address, for example
`wss://signaling.example.com/ws`, a room and your name.

## Run your own signaling server

The signaling server is the `server` crate in `crates/server`. It only passes small
messages between the people in a room (who is here, who is live, connection offers); no
video or sound goes through it, so it runs fine on a small free instance.

```sh
cargo run --release -p server                 # listens on 127.0.0.1:7800
cargo run --release -p server -- 0.0.0.0:7800 # listens on all interfaces
PORT=10000 cargo run --release -p server      # what hosting platforms set: 0.0.0.0:$PORT
```

- WebSocket endpoint: `/ws`. In the app the address is `ws://HOST:PORT/ws`, or
  `wss://HOST/ws` behind a TLS proxy (most hosting platforms provide one).
- Health check: `GET /live` answers `ok`.
- Invite links: `GET /j/CODE` is a small page that opens the room in the app.
- `STREAMER_KEEP_ALIVE_URL`: optional. The server requests this URL every few minutes,
  which keeps free hosts that sleep when idle awake. Set it to the server's own public
  `https://.../live`.

The server needs no GStreamer and builds on Linux, macOS and Windows.

A build of the app can also have one server built in, with no server field in the
interface: set `STREAMER_SERVER_URL` at build time, or pass `-Server wss://HOST/ws` to
`tools\installer.ps1`.

## Build from source

You need:

- Rust (stable, MSVC toolchain) and Visual Studio 2022 or its Build Tools with the C++
  workload.
- GStreamer 1.28.8, the official MSVC x86_64 installer from
  <https://gstreamer.freedesktop.org/download/> (runtime and development files).
- NSIS 3, to build the installer.

Then, in PowerShell:

```powershell
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = 'C:\path\to\gstreamer\1.0\msvc_x86_64\'
.\tools\bundle.ps1                     # dist\backseat: the app with the DLLs it needs
.\tools\installer.ps1 -WebView2 .\MicrosoftEdgeWebview2Setup.exe
                                       # dist\backseat-setup-X.Y.Z.exe
```

`bundle.ps1` builds `backseat.exe`, copies the GStreamer libraries and plugins the app
uses, the Visual C++ runtime and the license texts, and fails if any DLL import does not
resolve. The WebView2 bootstrapper is
<https://go.microsoft.com/fwlink/p/?LinkId=2124703>.

Releases are built by the `release` workflow in `.github/workflows`: pushing a tag
`vX.Y.Z` builds the installer and publishes it with the notes from `CHANGELOG.md`.

## License

Backseat is released under the [MIT License](LICENSE).

The Windows build ships third-party software next to `backseat.exe`, most notably
GStreamer and GLib under the GNU LGPL 2.1 or later (used unmodified, as separate DLLs that
can be replaced), plus OpenSSL, libsrtp, Opus and other libraries under their own
licenses. See [THIRD-PARTY.md](THIRD-PARTY.md); the license texts are installed in the
`licenses` folder.
