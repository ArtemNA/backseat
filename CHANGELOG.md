# Changelog

Each `## X.Y.Z` section becomes the notes of the GitHub release for tag `vX.Y.Z`.

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
