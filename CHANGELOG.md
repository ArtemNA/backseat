# Changelog

Each `## X.Y.Z` section becomes the notes of the GitHub release for tag `vX.Y.Z`.

## 0.2.10

- Invite links bring their server. A room you open from an invite link (clicked or pasted) is on the server that link came from, whatever server is in your settings. Before, a friend with another server in the settings (or none) could not get into the room. Each room keeps its own server.

## 0.2.9

- A connection to the server that silently stops passing data (it happened through the hosting's proxy for ~25 s) is noticed within 6 s and made again, so your place in the room is kept. Before, the app learned of it only after the server had given the place away, and showed "Lost the connection to the server".
- If the server did give your place away, Backseat joins the room again by itself ("Reconnecting…") instead of stopping with an error. A stream you watched, your own stream and voice do not come back by themselves in that case.
- Updates show across the top of the window: "Downloading Backseat X…" with a moving bar while it downloads (it restarts by itself), or "Version X is available" with *Update now*. The arrow in the rooms list is gone.

## 0.2.8

- **Voice chat.** Every room has a voice chat: press *Join voice* in the list on the left. Echo cancelling and noise suppression let you talk over speakers, no headset needed. The voice goes directly between you, as the video does.
- In voice: your microphone and sound off and on (others see it), who is speaking, a volume for each person (click or right click them), and sounds when someone joins, goes live or starts watching (can be turned off).
- *Settings → Voice*: the microphone and the speakers, voice activity or a push-to-talk key, and the threshold above which you are heard, with a live meter. *Keep streams' sound out of my microphone* is there too, for speakers without headphones (off by default: stream sound then comes about 20 ms later, and it applies to streams opened after turning it on).
- Voice also keeps a room going in the background, like watching and streaming: switch rooms and the call goes on; the bar at the bottom left says which room it is in. Voice, watching and streaming happen in one room at a time.
- **Temporary rooms.** A room can come from a link that carries its own end time (`…/t/XXXX-XXXX-XXXX-XXXX-XXXX`, a 20-symbol code). Opening or pasting it joins like any invite, and the room takes its name from the link. When the time is up, nobody new gets in and everyone still inside is taken out with "This temporary room has ended."; the room then leaves the list on the left (also when the app was closed at that moment). Such links are made by the server's owner; the server keeps no list of them.
- People in voice show the hand cursor and light up under the mouse.

## 0.2.7

- **Rooms keep going in the background.** Switching to another room while you watch or stream keeps the room you left connected: your stream goes on for your viewers, and the stream you watch is there again as it was when you come back, with no new join. Such a room has a green dot on the left. A video popped out into its own window stays open.
- A room where you did nothing closes when you switch away, but the server keeps your place for 20 seconds: coming back within that time takes it again.
- Watching and streaming happen in one room at a time: starting either in another room stops it in the room left in the background.

## 0.2.6

- Switching rooms no longer drops you out of the room you switch to: the room you left was taken for a lost connection, which closed the new room and showed "Lost the connection to the server".
- While the connection to the server is being restored, a line says so ("Reconnecting…"); your place in the room is kept meanwhile.
- The quality lists under the video show above it: opening upwards they were hidden behind the video. While a list is open the video steps aside.
- No Apply button: a picked quality is asked for by itself 1.5 s after the last change (changing both the size and the bitrate restarts the streamer's encoder once). Picking "As streamed" again now goes back to the streamer's own quality.

## 0.2.5

- **Stream badge.** A small badge in the top right corner of the video shows the quality, bitrate and codec (`1080p60 · 12.4 Mbit/s · AV1`). Its dot is green while frames come as they should and amber when fewer come (the frame rate is then shown as a number) or packets are lost. **F3** shows or hides it; **Shift+F3** opens the detailed stats as F3 did before.
- F3, Shift+F3, F11 and Esc work anywhere in the app window, not only after clicking the video. The badge and the stats are one setting for all streams you watch, and a stream you open next starts with it.
- The video window's volume bar and buttons respond at once: they were redrawn only with the next video frame (at 30 fps a click showed up to 35 ms later, and the bar jumped while dragging). Median from input to screen now about 6 ms.
- Fixed in this release (it was broken in 0.2.4): after using the video window's volume bar, the mouse stayed captured by the video, the hand cursor showed everywhere and the rest of the window could not be clicked.
- Switching fullscreen is about three times faster (median 47 ms instead of 155 ms), and the misplaced text that flashed in the video's place during the switch is gone.
- A loading animation and the connection status show in the video's place until the first frame, instead of a black rectangle.
- Resizing the window no longer stretches the video for a moment and then jumps: the picture keeps its proportions at every size.
- The line under the video (the frame rate warning, hints) keeps its height, so the layout no longer jumps when a warning appears or goes away.
- The Stats button under the video changes its colour at once, not up to a second later.

## 0.2.3

- Fullscreen opens on the monitor the app is on: from a monitor other than the primary one it went fullscreen on the primary monitor.
- The sound and fullscreen buttons inside the video window show the hand cursor.

## 0.2.2

- The window list for going live no longer shows hidden windows of suspended Windows apps (Settings, Windows Input Experience, ApplicationFrameHost).
- Buttons, checkboxes and lists show the hand cursor.

## 0.2.1

- The go-live dialog keeps the codec from *Settings → Streaming*: AV1 chosen there was replaced with H.265 when the dialog opened right after start.
- Your name and status at the bottom left stay on one line (a long name ends with "…"); the go-live button there is an icon now.

## 0.2.0

- **A native window instead of the webview.** Backseat now draws its own window; Microsoft Edge WebView2 is no longer needed, and the app runs as one process instead of seven. It uses about 80–140 MB less memory (in a room, watching or streaming) and less processor time while watching.
- Your rooms, settings and language carry over from 0.1.9. Coming straight from 0.1.8 or older, the room list starts empty: join your rooms again with their invites.
- The join form scrolls when it is taller than the window (a direct connection's texts in a small window), and a long list of rooms on the left scrolls instead of squeezing.
- Uninstalling keeps your rooms and settings, as before.

## 0.1.9

- Your rooms and settings are now also kept in `%LOCALAPPDATA%\Backseat\state.json`, so later versions can carry them over.

## 0.1.8

- **Direct connection (beta).** Two people can connect with no server at all: one makes an invite, the other answers, and you swap these two texts once through any messenger. Then either of you can stream to the other, both at once too. Only for two people and with no reconnection: see the README for what to know before using it.
- **Short codes for direct connections.** Instead of the long text you can send an 8-character code like `K7Q2-9XMB`, valid for 5 minutes, with a countdown next to it. The text stays on a small server for those 5 minutes; untick *Make a short code* to keep it off any server. If the server does not answer within a few seconds, the full text is shown as before, and pasting a full text always works.
- **Open the log folder** from *Settings → General*, to send logs with a bug report.
- The form of a direct connection starts over when the connection ends.
- A stray "null" no longer shows under your stream's numbers, and the info under a watched stream no longer scrolls when it fits.
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
