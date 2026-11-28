# Changelog

Each `## X.Y.Z` section becomes the notes of the GitHub release for tag `vX.Y.Z`.

## 0.2.27-beta.2

- **Connecting through a punched hole is much faster.** At the start of each connection both sides find out what kind of NAT they are behind (about a second); when one keeps its ports and the other changes them (a provider's shared address), the hole is punched at once instead of after the direct try gives up. Voice with such a friend now connects in a few seconds instead of about 12; a stream starts in a few seconds instead of about 25. Behind other NATs nothing changes: the direct try goes first, as before, and if a NAT becomes easier later, the direct way is used again.
- If punching at once does not work, the punch after the direct try still comes, as in beta 1.
- While a hole is being punched, your viewers list shows it, not "failed".

## 0.2.27-beta.1

A beta: new things for those who want them sooner. It may have bugs.

- **Voice and streams through a hole punched in the NAT.** When two people cannot connect directly (one is behind a provider's shared address, CGNAT, the other behind an ordinary home router), Backseat now punches a hole through the NAT by itself and connects voice or the stream through it, with no relay server in between: the sound and video go straight from one computer to the other. It starts when the direct try gives up (about 20 seconds) and takes a few seconds more. The video window stays open meanwhile.
- **You see how each connection stands.** By a person in voice, on a stream's tile and in your viewers list: a quiet spinner while connecting, a yellow lightning bolt while punching through the NAT, and a small grey link that stays while the connection goes through a punched hole. Hover them for a word on what they mean.
- **Voice delay no longer grows in long sessions.** Sound that comes faster than it is played (two clocks that differ a little) is trimmed in pauses down to a small cushion, so a voice is not seconds late after an hour.
- **Beta versions, if you want them.** *Settings → Updates → Get beta versions*. Turned off (the default) you get stable versions only; turning it off later does not go back by itself, you update when a stable version newer than yours comes out.

## 0.2.26

- **Remote control through a local API.** Turn it on in *Settings → Remote control*: phones, a Stream Deck, scripts or AI agents on your computer or your home network can see your rooms, who is in them, voice and streams, and open a room, join or leave voice, mute or deafen, set a person's or a stream's volume, start, change or stop your stream, and watch someone. Every call needs a token; it shows as dots until you ask, so it does not leak on a stream, and a new token cuts off the old one. Off by default. The calls are described in [API.md](API.md).
- **Stop, then Watch the same stream at once** no longer ends the new watch a moment later.
- **The Change dialog of a running stream** starts from what actually runs, and keeps its resolution until you pick another source.
- **Leave while Backseat is joining a room again** after a lost connection now really leaves (it came back, or said "Lost the connection"), and that rejoin no longer pushes aside a room or a direct connection you opened meanwhile.

## 0.2.25

- **Fixed: in 0.2.24 viewers got only half of a stream's sound** (it sounded broken from the start) when the streamer shared all sound. The streamer's mixer is back to sending every piece; the fix of 0.2.24 (sound that no longer drifts and breaks up after a while) stays.

## 0.2.24

- **A stream's sound no longer breaks up after a while.** With "all sound" shared, the sound of a program could drift in time until the streamer's mixer threw most of it away: viewers heard it chopped, quieter, or almost gone, and only joining again helped. Each program's sound now follows the stream's own clock, so it cannot drift. If the mix still comes out much quieter than what it mixes, Backseat starts the captures again by itself.
- **Voices and other sounds have 30 ms of room** before the mixer gives up on a late piece, so a busy computer no longer makes a voice crackle. Voices play 30 ms later; you will not hear the difference.
- The log now shows, once a minute, how loud each voice and the stream came in and what went out to the speakers, and when a video window shows fewer frames than arrive.

## 0.2.23

- **Diagnostics for crackling sound.** Some users hear a stream's sound or a voice start to crackle after a pause, until they join again. This version changes nothing in the sound; it writes to the log where the sound's timing breaks (each program's capture, the mix and the encoder on the streamer's side; the received stream and voices, and the speakers, on the listener's side) and what the jitter buffers do. If you hear it, send your newest log from `%LOCALAPPDATA%\Backseat\logs` (and ask the streamer for theirs) soon: the app keeps only the last 5.

## 0.2.22

- **New noise suppression: DeepFilterNet3**, a neural network that keeps the voice natural and removes noise, hums and whines much better than RNNoise did. It replaces RNNoise and is the default; a saved RNNoise choice switches to it, and a chosen WebRTC level (Low…Very high) stays until you pick *Neural*. Your voice reaches others 20 ms later than with RNNoise (streams are not affected); it takes about 4% of one CPU core on a recent processor.
- **How hard it cuts** (*Settings → Voice*, under the noise suppression): *Medium* by default; *Strong* removes the most noise but may swallow quiet word ends; *Soft* keeps quiet speech and leaves a little noise. It changes at once, also in voice, without rejoining.
- *Remove the whine* and *Cut high frequencies* are gone: the new denoiser handles both better. Cuts they made before no longer apply.
- Changing voice settings twice quickly no longer loses the second change, and leaving voice or switching rooms while voice rejoins after a settings change no longer leaves the microphone open or a room online.

## 0.2.21

- **A stream made fullscreen from inside the app no longer shows as a separate window** in Alt-Tab and the taskbar; Alt-Tab to Backseat brings it back. Back from fullscreen, it returns to its place in the app. A video popped out into its own window shows the app's icon.
- **The go-live dialog shows your monitors as Windows lays them out**, to scale (side by side, above or below); click one to stream it. The primary monitor is picked first.
- **A stream's sound plays on the speakers chosen for voice** (Settings → Voice) and moves with them when you change them while watching. If those speakers are not there, it plays on the default ones.
- **Muting a stream or setting its volume is remembered**: every stream starts that way, also after a restart.
- Settings → Updates has a **What's new** button that opens the release notes.

## 0.2.20

- **The stream no longer freezes when a program whose sound you share closes.** Before, with "all sound" or one program's sound, closing that program could stop the picture and sound for every viewer.
- **Sound that goes silent comes back by itself.** With all sound shared, if a program plays but its captured sound is silent for 4 s, Backseat starts capturing it again; if every program is heard but the mix sent to viewers is silent, all of them start again. Before, the stream stayed silent until a viewer left and joined again.
- The bitrate and loss on the stream badge no longer drop to 0 after a network stall of a few seconds.
- The log now shows the level of each shared program and of the stream's sound once a minute, to find such cases.

## 0.2.19

- **Video in its own process:** a friend's stream is decoded and shown by a separate process (`backseat.exe --video-process`). If the video decoder or driver crashes, only the video stops, never the whole app; it starts again by itself within 1–3 s. If it keeps failing (4 crashes in a minute, or 3 starts in a row without a picture), the watch ends with "The video stopped working". Latency is the same as before; it takes about 30 MB more memory.
- **Zoom in fullscreen:** with the video itself in fullscreen, the mouse wheel over the picture zooms in (up to 4×) toward the cursor. Over the panel, or outside fullscreen, the wheel still sets the volume. Zoom resets when you leave fullscreen.
- The video window is no longer offered as a source to stream.
- A second click on Watch while the stream is opening no longer starts a second watch or shows an error.
- Fullscreen and pop-out show their new state at once.
- The log now says when the picture stops for a while or the stream's sound goes missing, to find those rare cases.

## 0.2.18

- Closing the window you stream now ends the stream, and you are told why. Before, viewers kept its last frame, and changing the source afterwards showed them nothing.
- Changing the source of a running stream can no longer freeze the app: a window that does not stop or start capturing in time is left behind, and a source that cannot start keeps the old one live and shows why.

## 0.2.17

- **See what you stream:** while you are live, a small preview of your stream sits next to the viewer count. It runs only while the Backseat window is in front (behind a game it costs nothing), also before anyone watches.
- Fixed: changing the window, monitor or frame rate of a running stream failed (*Apply* seemed to do nothing and the dialog stayed open). A failed *Apply* now shows why, above the buttons.
- The "… in its own window" line no longer flashes at the start of watching or after *Stop*, and sits in the middle of the box instead of being cut off at its edge.
- Your own *Stop* (or closing a popped-out video) no longer says "the stream ended: stopped".

## 0.2.16

- **Starts with Windows**, hidden in the tray, so friends can find you in voice and rooms. On by default; *Settings → General* turns it off. The uninstaller removes it.
- Voice comes back by itself after trouble: someone who joined or left voice while your connection to the server was down, a friend whose app crashed and came back, or a connection that broke (another network) no longer leaves two people "in voice" who cannot hear each other.
- Changing the microphone or its processing (or *Remove the whine*) keeps your voice in its own room; before, it moved to the room you had open and closed the other one.
- A double click on *Join voice* no longer opens it twice (and leaves some people unheard).
- The voice threshold listens after the noise suppression and the whine's cuts: a whine they remove no longer keeps your microphone open.
- Leaving voice in a room you had switched away from closes that room (your place is kept 20 s), instead of leaving you online there.
- *Hear yourself* stops when the settings close or the window goes to the tray, also during *Remove the whine*.
- Switching speakers no longer silences the app; speakers that do not open keep the current ones; unplugged speakers no longer stop the app's sound. A lost microphone is shown.
- A failed join (an ended or wrong invite, a server that does not answer) keeps you in the room you were in.
- An invite that leads to a server you have not used before opens the join form showing that server, instead of connecting by itself.
- Connecting to a server that does not answer gives up after 10 s instead of waiting forever.

## 0.2.15

- Fixed in this release (it was broken in 0.2.13 and 0.2.14): a stream of the whole system's sound sent silence. The programs' sound was taken but lost in the mixer.
- Windows' own sound engine (audiodg) is never taken into a stream's sound.
- **RNNoise is the noise suppression by default**: a small neural network, stronger than before on hums, whines and other noise that is not hiss. The previous levels stay a choice in *Settings → Voice*.
- **Remove the whine** (*Settings → Voice*, next to *Hear yourself*): stay quiet for 3 seconds and a steady whine or hum of your microphone (ground, USB) is found and cut out with narrow filters, also while you speak. A whine made of many tones (multiples of one) is cut whole. *Reset* takes it back.
- New option: *Cut high frequencies (above 8 kHz)*, for a high whine; speech needs little of that range.
- The installer is about 2 MB larger (the denoiser and the filters).

## 0.2.14

- Another copy of Backseat is never taken into a stream's sound nor offered to exclude (it showed up in that list when a second copy ran).
- *Settings → Streaming*: the list to add exclusions from is now called "Programs with sound": it shows every program that has sound open, also while it is quiet.

## 0.2.13

- **Your voice chat no longer gets into your stream through sound software.** With SteelSeries Sonar, GoXLR, Wave Link, Voicemeeter and the like, the voices you hear were played again by that software and went into a stream of the whole system's sound. Now the system's sound is taken one program at a time, leaving out Backseat and such sound routers.
- *Settings → Streaming* lists the programs whose sound never goes into such a stream. Discord is there at first (take it out with ✕); programs playing sound now can be added with one click.
- Fixed in this release: *Hear yourself* (0.2.12) did not start in the installed app, and with sound software it could play where you did not hear it. It now plays where voices and streams do, and its meter moves on its own, in push-to-talk too.
- The settings window is larger, and the voice settings are laid out in two columns so everything about the microphone fits on one screen.

## 0.2.12

- **Hear yourself.** *Settings → Voice → Hear yourself* plays your microphone back the way others hear it (processed, a moment later), with its level on the meter, also outside voice. It stops when the settings close.
- *Settings → Voice* now has the microphone's processing: noise suppression (off, low, normal, high, very high), echo cancelling, and evening out your microphone's loudness. Changes apply at once.
- A person's volume menu (click or right click in the voice list) and a room's menu show above the video; they were hidden behind it.

## 0.2.11

- Fixed in this release (it was broken in 0.2.9 and 0.2.10): in a quiet room the app reconnected to the server every few seconds and showed "Reconnecting…" again and again. The hosting's proxy keeps the server's pings from the app, so the new check saw silence; the app now pings the server itself every second.

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
