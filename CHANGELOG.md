# Changelog

Each `## X.Y.Z` section becomes the notes of the GitHub release for tag `vX.Y.Z`.

## 0.1.8

- **Direct connection (beta).** Two people can connect with no server at all: one makes an invite, the other answers, and you swap these two texts once through any messenger. Then either of you can stream to the other, both at once too. Only for two people and with no reconnection: see the README for what to know before using it.
- **Open the log folder** from *Settings → General*, to send logs with a bug report.
- The join form no longer jumps to the invite-code tab when reopening the last room fails at start while you are on another tab.
- The installer now includes what direct connections need, and the license notes list dav1d and the GStreamer Rust plugins.

## 0.1.7

- **Default settings for streaming and watching.** Settings → Streaming picks resolution, frame rate, bitrate, codec and sound in advance every time you go live. Settings → Watching sets the most you want to receive: a stream asks for that from the start, so a 50 Mbit/s stream comes to you at 12 if that is your limit.
- **A window's own sound by default.** Streaming a window now picks that program's sound only; a screen picks the system sound. Change it in Settings → Streaming.
- **No double name after a crash or an update.** Backseat remembers its place in the room and takes it back when it starts again, and it tells the server when it quits.

## 0.1.6

- **A broken connection to the server no longer ends the stream.** If the connection to the signaling server drops (it happens with hosted servers now and then), Backseat reconnects in the background and takes its place back; streams go on, and nobody in the room sees you leave. Before, the streamer's app could close at that moment.
- **Streams start faster.** A viewer who joins a running stream gets the picture within a second instead of after up to two.
- Cleaner log files: harmless start-up messages are counted instead of written out.

## 0.1.5

- **New icon** for the app, its window, the tray and the installer.
- **Better picture on AMD (H.265).** The encoder uses its quality preset: a slightly sharper picture at the same bitrate, with the same latency. AMD H.264 stays as it was: its low-latency mode would fix the blur after key frames but went about 50% over the chosen bitrate.

## 0.1.4

- **Better picture at low bitrates (NVIDIA).** The encoder's rate buffer now holds 8 frames instead of one. Key frames (every 2 s) no longer turn the picture blurry for half a second, H.265 keeps to the chosen bitrate instead of going 13–34% over, and the latency stays the same.
- Sound changes made from the video's own controls are written to the log.

## 0.1.3

- **Updates.** Backseat checks GitHub for a new version at start and installs it (the installer is checked against its SHA-256 first). Turn this off, or check by hand, in Settings → Updates.
- **Tray.** Closing the window keeps Backseat running in the tray; use the tray icon's menu to quit.
- **Settings.** A settings window (the gear next to Go live): default signaling server, language, close to tray, updates.
- **Change a stream while live.** The Change button next to End stream switches the screen or window, the resolution, frame rate, bitrate or which program's sound, and viewers stay connected. Viewers watching at the streamer's quality follow it up or down; viewers who picked a lower quality keep it unless it is now above the new maximum.
- **Fullscreen fix.** Going in and out of fullscreen several times could freeze the app and the stream; it no longer does.
- **Sound controls on the video.** Moving the mouse over a stream shows mute, volume and fullscreen buttons, also in fullscreen; the mouse wheel changes the volume.
- The room list scrolls when it is long instead of shrinking the icons.

## 0.1.2

- Wide streams keep the source aspect ratio instead of being stretched.
- The default stream resolution follows the picked screen or window.
- Only encoders that really work on this computer are offered; AMD (AMF) H.265 runs in
  low-latency mode.
- Opening an invite link twice no longer starts a second copy of the app: one app runs
  per user.
- The installer and the uninstaller close a running Backseat first, so its files can be
  replaced.

## 0.1.1

- AV1 streaming. Viewers without hardware AV1 decoding decode it on the CPU.
- Rooms by invite code: create a room with a random code, join by code or by a link;
  invite links open the app (`backseat://`).
- Saved rooms in a rail on the left; video plays inside the main window, and each stream
  can be popped out into its own window.
- Mute button for each watched stream.
- The app is now called Backseat; an install of the old "streamer" build is replaced.
