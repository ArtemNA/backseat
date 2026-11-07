# Changelog

Each `## X.Y.Z` section becomes the notes of the GitHub release for tag `vX.Y.Z`.

## 0.1.3

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
