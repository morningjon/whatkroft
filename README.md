# WhatKroft

A native WhatsApp client for the Linux desktop, written in Rust. No embedded
browser: the interface is GTK4 and libadwaita, and the WhatsApp protocol is
spoken directly through [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust).

![WhatKroft showing a group chat](docs/screenshot.png)

*Screenshot taken in demo mode with made-up data.*

> **Unofficial.** WhatKroft is not affiliated with, endorsed by, or supported
> by WhatsApp or Meta. Using a third-party client may violate WhatsApp's Terms
> of Service and can lead to account restrictions or a ban. Use it at your own
> risk, and consider trying it with a spare number first.

## Status

Early alpha. It links a device, shows your chats including archived ones,
and reads and sends text and media.

Working:

- Linking as a companion device by QR code, with a persistent session
- Chat list with names, previews, times and unread counts
- Archived chats: a separate Archived view, kept in step with your phone, and
  archive or unarchive from a chat's right-click menu or its header menu
- Reading conversations (direct and group), with day separators, loading
  earlier messages as you scroll up
- Photos and stickers shown in the conversation; videos, voice notes, audio
  and documents downloaded on click and opened or played
- Sending text, and sending files with the paperclip button or by dropping
  them on the conversation. Photos go as photos (PNG and WebP are converted to
  JPEG), MP4 as video, MP3/M4A/AAC/Ogg as audio, everything else as a document
- Sent / delivered / read ticks
- Read receipts when you open a chat, following your WhatsApp privacy setting
- History import from the phone on first link
- Local SQLite store, so the app opens instantly and reads offline
- Desktop notifications for incoming messages while the window is open
- Adaptive layout: list and conversation side by side, or one at a time when narrow
- Unlinking from the menu

Not yet:

- Replies, reactions, edits, deletions, mentions, link previews
- Recording voice notes; sent audio goes as an audio file, not a voice note
- Thumbnails for videos you send (the recipient sees a blank preview until
  they play it), and video playback inside the window (videos open in your
  video player)
- Animated stickers (the first frame is shown) and sending stickers
- Locations, contacts and polls show as a short description only
- Attachments that have expired on WhatsApp's servers cannot be re-requested
  from the phone yet
- Typing indicators and presence
- Voice and video calls
- Search, pin, mute, starred messages
- Status updates and channels (hidden for now)
- Notifications while the window is closed

Voice notes play inside the window when GTK's media backend and the GStreamer
plugins for the format are installed (on Arch: `gst-plugins-base` and
`gst-plugins-good`); otherwise they open in your audio player. The AppImage
does not bundle GStreamer, so there they open in your audio player.

## How it fits together

```
┌────────────┐   Unix socket, JSON lines   ┌──────────────┐   WhatsApp   ┌──────────┐
│ whatkroft  │ ◄─────────────────────────► │ whatkroftd   │ ◄──────────► │ WhatsApp │
│ GTK4 window│                             │ daemon       │   protocol   │ servers  │
└────────────┘                             │ ├ session.db │              └──────────┘
                                           │ ├ chats.db   │
                                           │ └ media/     │
                                           └──────────────┘
```

| Crate | What it is |
| --- | --- |
| `crates/whatkroftd` | Background daemon. Owns the WhatsApp connection (`wa.rs`), attachments (`media.rs`), the chat database (`store.rs`) and the socket server (`ipc.rs`). |
| `crates/whatkroft` | The GTK4 / libadwaita window. Talks only to the daemon. |
| `crates/whatkroft-ipc` | The wire protocol both sides share. |

The window starts the daemon if it is not already running. The daemon keeps
running after the window closes, so the session stays connected.

## Download

Each [release](https://github.com/morningjon/whatkroft/releases) has an
AppImage for x86-64 that bundles the window, the daemon, GTK4 and libadwaita:

```sh
chmod +x WhatKroft-*-x86_64.AppImage
./WhatKroft-*-x86_64.AppImage
```

It needs glibc 2.39 or newer (Ubuntu 24.04, Fedora 40, current Arch and
CachyOS). It runs through XWayland on Wayland sessions; set
`WHATKROFT_GDK_BACKEND=wayland` to try native Wayland instead.

## Building

You need Rust 1.94 or newer, GTK 4.12 or newer and libadwaita 1.4 or newer.

```sh
# Arch, CachyOS
sudo pacman -S --needed rust gtk4 libadwaita

# Fedora
sudo dnf install cargo gtk4-devel libadwaita-devel

# Debian, Ubuntu
sudo apt install cargo libgtk-4-dev libadwaita-1-dev
```

```sh
git clone https://github.com/morningjon/whatkroft
cd whatkroft
cargo build --release
```

## Running

```sh
./target/release/whatkroft
```

On first launch a QR code appears. On your phone open WhatsApp, go to
Settings, then Linked devices, tap Link a device and scan it. Your chats then
sync from the phone, which can take a minute.

To install both binaries into `~/.cargo/bin`:

```sh
cargo install --path crates/whatkroftd
cargo install --path crates/whatkroft
```

To build the AppImage yourself (also needs `patchelf`, `librsvg` and `curl`):

```sh
packaging/appimage/build.sh
```

`data/` has a desktop entry and an optional systemd user unit for running the
daemon in the background from login.

### Demo mode

To try the interface without linking an account, start the daemon with a
made-up account served from memory, then open the window:

```sh
./target/release/whatkroftd --demo &
./target/release/whatkroft
```

`--demo-pairing` stays on the pairing screen instead. Nothing is sent
anywhere in either mode, and the demo's attachments are generated on the spot.

## Where things are kept

| Path | Contents |
| --- | --- |
| `$XDG_DATA_HOME/whatkroft/session.db` | WhatsApp session keys. Anyone who copies this file can act as your linked device. |
| `$XDG_DATA_HOME/whatkroft/chats.db` | Your chats and messages, unencrypted. |
| `$XDG_DATA_HOME/whatkroft/media/` | Attachments you have viewed or sent, unencrypted, one folder per chat. |
| `$XDG_RUNTIME_DIR/whatkroft/daemon.sock` | Control socket. |

`XDG_DATA_HOME` defaults to `~/.local/share`. Both directories are created
readable by your user only. To reset completely, unlink from the menu (or
from your phone), stop `whatkroftd` and delete the data directory.

Set `RUST_LOG=debug` for verbose logs. `WHATKROFT_SOCKET` overrides the socket
path, and `whatkroftd --data-dir` the data directory.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

The socket protocol is plain JSON, one message per line, and is defined in
`crates/whatkroft-ipc/src/lib.rs`. You can poke the daemon by hand:

```sh
echo '{"id":1,"method":"list_chats"}' | nc -U "$XDG_RUNTIME_DIR/whatkroft/daemon.sock"
```

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).

WhatsApp is a trademark of its owner. This project uses the name only to
describe what it connects to.
