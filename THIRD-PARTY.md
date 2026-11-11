# Third-party software

Backseat for Windows ships the following third-party software next to `backseat.exe`.
Their license texts are in the `licenses` folder of the installation.

## GStreamer and its libraries (LGPL)

The GStreamer libraries and plugins are licensed under the GNU Lesser General Public
License, version 2.1 or later (the texts in `licenses/gstreamer-1.0`,
`licenses/gst-plugins-base-1.0`, `licenses/gst-plugins-bad-1.0`; the plugins from
gst-plugins-good — `rtp`, `rtpmanager`, `autodetect` — are under the same license). GLib, GObject, GIO, GModule and proxy-libintl are under the LGPL as
well, libnice under LGPL 2.1 or MPL 1.1.

- Version: GStreamer 1.28.8, the official MSVC x86_64 build from
  <https://gstreamer.freedesktop.org/download/>.
- Source code: <https://gstreamer.freedesktop.org/src/> (gstreamer, gst-plugins-base,
  gst-plugins-good, gst-plugins-bad, version 1.28.8) and the build recipes in cerbero,
  <https://gitlab.freedesktop.org/gstreamer/cerbero>, tag `1.28.8`.
- These libraries are used unmodified and linked dynamically: each is a separate DLL in
  the installation folder (plugins in `gstreamer-1.0`) and can be replaced with a
  compatible build of your own.

## GStreamer Rust plugins

The plugins `gstrsrtp.dll` (RTP for AV1) and `gstdav1d.dll` (AV1 decoding with dav1d) come
from gst-plugins-rs, whose plugins are under the MIT, Apache 2.0 or LGPL 2.1-or-later
license (texts in `licenses/gst-plugins-rs`). They are used unmodified, from the same
GStreamer 1.28.8 build; source: <https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs>.

## Other native libraries

| Library | DLL | License |
|---|---|---|
| OpenSSL | `libcrypto-3-x64.dll`, `libssl-3-x64.dll` | Apache 2.0 |
| libsrtp | `srtp2-1.dll` | BSD 3-clause |
| Opus | `opus-0.dll` | BSD 3-clause |
| ORC | `orc-0.4-0.dll` | BSD 2-clause |
| PCRE2 | `pcre2-8-0.dll` | BSD 3-clause with exception |
| libffi | `ffi-7.dll` | MIT |
| zlib | `z-1.dll` | zlib |
| usrsctp (inside `gstsctp-1.0-0.dll`) | | BSD 3-clause |
| dav1d (AV1 decoding on the CPU) | `dav1d.dll` | BSD 2-clause |
| Microsoft Visual C++ runtime | `vcruntime140.dll`, `vcruntime140_1.dll`, `msvcp140.dll` | Microsoft Visual Studio redistributable |

## Rust crates

`backseat.exe` is built from Rust crates published on crates.io, under MIT, Apache 2.0,
BSD, zlib, ISC, Unicode, Unlicense and MPL 2.0 licenses (the MPL 2.0 crates are used
unmodified; their source is on crates.io). The full list with each crate's license is in
`licenses/rust-crates.txt`.

## User interface

The window is drawn with GPUI (from the Zed editor, Apache 2.0) and gpui-component (Apache
2.0), with Lucide icons (ISC); they are among the crates in `licenses/rust-crates.txt`.
