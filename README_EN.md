# macrdp

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![macOS](https://img.shields.io/badge/macOS-14%2B-black.svg)](https://www.apple.com/macos/)
[![Apple Silicon](https://img.shields.io/badge/Apple%20Silicon-Supported-green.svg)](#)

English | **[中文](README.md)**

> **Fork notice**: This is a personal fork of [x6nux/macrdp](https://github.com/x6nux/macrdp), heavily modified for my own long-running remote-access use (a "modded" fork). Upstream provides the macOS RDP stack, ScreenCaptureKit capture, VideoToolbox encoding and the Tauri desktop app; this fork focuses on stability, remote input/display behavior, and local build/signing. See [Changes in this fork](#changes-in-this-fork).

**macOS Remote Desktop Server**

A native RDP server for macOS. Remote into your Mac from Windows, Linux, iOS, or Android — using any standard RDP client like Windows Remote Desktop (mstsc), Microsoft Remote Desktop, or FreeRDP.

> **Why macrdp?** macOS has no built-in RDP server. VNC is slow and blurry. macrdp gives your Mac a first-class remote desktop experience — fast, sharp, and compatible with every RDP client out of the box.

---

## Features

- **Standard RDP protocol** — works with any RDP client, no special software needed on the client side
- **Hardware-accelerated encoding** — GPU-powered H.264 via Apple VideoToolbox, low latency on Apple Silicon
- **High fidelity color** — AVC444 mode for pixel-perfect color reproduction (RDP 10)
- **Full keyboard & mouse** — complete input injection with 104-key mapping, numpad, modifiers, scroll
- **HiDPI / Retina support** — capture at 2x/3x resolution for sharp 4K remote display
- **Configurable** — resolution, frame rate, bitrate, encoder, quality presets, all via simple TOML config
- **Secure** — NLA/CredSSP authentication with auto-generated TLS certificates
- **Lock screen capture** — automatic CoreGraphics fallback when the screen is locked

---

## Changes in this fork

This fork targets unattended, long-running remote use (mostly from Windows clients). Main changes relative to upstream as of v0.1.11:

### Stability

- Fixed a SIGSEGV when starting ScreenCaptureKit: color properties on `SCStreamConfiguration` are now copied into memory instead of being held by `assign`, avoiding a dangling CFString.
- Display sleep no longer drops the remote session: the display is woken on connect; if it sleeps again mid-session, capture continues and resumes when it wakes.
- Without Screen Recording permission the server no longer hangs on "starting"; it reports a clear error and exits. Startup is also no longer blocked by TCC prompts.
- TLS certificates are written to a writable directory instead of the process working directory.

### Display

- **Resolution policy**: `auto` follows the client window, matching Windows Remote Desktop (the capture size is driven by the client's requested Bitmap-capset desktop size, capped at the physical display); an explicit `WxH` fixes the size and the client cannot override it. On a 150%-scaled Windows client, mstsc only requests the logical size (e.g. 2560×1440) and shows it 1:1 inside a 3840×2160 full-screen window — hence black borders on all sides. Fixing the resolution at 3840×2160 fills the screen sharply.

### Input

- Remote Caps Lock switches the macOS input method (Text Input Sources), for CJK input.
- Fixed the crash when pressing Caps Lock remotely: HIToolbox's TIS APIs must be called on the main thread; all calls are now dispatched to the main queue and input-method availability is cached.

### Desktop app (Tauri)

- Starts in the menu bar and opens the main window on demand; no permanent Dock icon, and the tray icon renders correctly.
- The main window lists every local IPv4 address (LAN / VPN / other) with one-click copy, refreshing automatically when a VPN reconnect changes the address.
- Corrected start/stop button state and feedback; the permissions page can open System Settings directly.
- Added a "keep display awake" toggle (off by default).

### Build & release

- Local builds work with Command Line Tools only, no full Xcode required.
- Stable self-signed signing identity `macrdp-dev`: `make sign-setup` creates it once, and every build is signed with the same identity, so Screen Recording / Accessibility grants survive reinstall and upgrade (an ad-hoc signature changes on every build and macOS treats each one as a new app).
- Added `make app` and `make dmg`; the DMG is built with `hdiutil` instead of Finder/AppleScript, so headless/SSH builds work.
- CI runs on macOS 26 runners, publishes Apple Silicon builds, and releases automatically on `v*` tags.

### Compatibility

- Config format stays compatible with upstream; new options have defaults, so existing configs need no changes.
- RDP protocol behavior is unchanged and remains compatible with mstsc, Microsoft Remote Desktop, FreeRDP and other standard clients.

---

## Requirements

- **macOS 14+** (Sonoma or later, required by ScreenCaptureKit)
- **Rust 1.75+**
- **Xcode Command Line Tools** (full Xcode is not required)
  - The project does not compile Swift, it only links the system Swift runtime; Makefile targets automatically add the CLT Swift runtime directory to `RUSTFLAGS`
  - If `cargo build` run by hand reports `libswiftCore.dylib` / `swiftCompatibility` link errors, set:
    ```bash
    export RUSTFLAGS="-L $(xcode-select -p)/usr/lib/swift/macosx"
    ```
  - If full Xcode is installed and `xcode-select -p` points to it, none of this is needed
- Screen Recording permission (System Settings > Privacy & Security)
- Accessibility permission (for keyboard/mouse injection)

---

## Quick Start

```bash
# Build
cargo build --release

# Run
cargo run --release --bin macrdp-server

# Connect from any RDP client → your-mac-ip:3389
```

### Build the desktop app

```bash
make install      # first time: install frontend dependencies
make sign-setup   # first time: create the stable signing certificate (recommended)
make app          # build .app signed with that certificate
make dmg          # package a DMG with hdiutil
```

To skip the stable signature, use `make app-adhoc && make dmg`; ad-hoc signatures change on every build, so permissions must be granted again after each reinstall.

---

## Configuration

Copy `config.example.toml` to `config.toml` and edit as needed:

```toml
# Network
port = 13389

# Authentication
username = "admin"
password = "123456"

# Display
width = 0          # 0 = auto-detect
height = 0
frame_rate = 60
hidpi_scale = 2    # 2x for 4K on Retina

# Encoding
quality = "high_quality"    # low_latency / balanced / high_quality
encoder = "hardware"        # hardware (GPU) / software (CPU)
chroma_mode = "avc420"      # avc420 (compatible) / avc444 (best quality)
bitrate_mbps = 50           # target bitrate (Mbps)

# Logging
log_level = "info"          # trace / debug / info / warn / error
```

Config search order:
1. `./config.toml`
2. `~/.config/macrdp/config.toml`
3. `~/Library/Application Support/macrdp/config.toml`

---

## Project Structure

```
crates/
├── macrdp-server/       Main server binary
├── macrdp-capture/      Screen capture
├── macrdp-input/        Keyboard & mouse injection
├── macrdp-encode/       Video encoding
├── ironrdp-server-gfx/  RDP protocol (IronRDP fork)
└── ironrdp-acceptor-patched/
                         RDP connection acceptor
```

---

## Acknowledgments

This project stands on the shoulders of giants. Special thanks to:

- **[x6nux/macrdp](https://github.com/x6nux/macrdp)** — the upstream project this fork is based on; it provides the macOS RDP server, ScreenCaptureKit capture, VideoToolbox encoding and Tauri desktop app.
- **[IronRDP](https://github.com/Devolutions/IronRDP)** — Pure Rust RDP protocol implementation. macrdp's protocol stack is built on a fork of ironrdp-server with GFX/AVC444 extensions.
- **[FreeRDP](https://github.com/FreeRDP/FreeRDP)** — The reference open-source RDP implementation. Its AVC444 dual-stream encoding approach and YUV444 B-area split algorithm were essential references.
- **[RustDesk](https://github.com/rustdesk/rustdesk)** — Open-source remote desktop software written in Rust. Its architecture for cross-platform screen capture and input injection was a great source of inspiration.

---

## License

This project is licensed under the **GNU General Public License v3.0** — see [LICENSE](LICENSE) for details. Any derivative work must also be distributed under GPLv3.

This fork uses the same license as upstream and keeps upstream's copyright and license notices; please preserve them when redistributing.

---

<details>
<summary><b>Keywords</b></summary>

macOS RDP server, Mac remote desktop server, RDP server for Mac, remote desktop protocol macOS, connect to Mac from Windows, connect to Mac from Linux, connect to Mac from Android, Windows Remote Desktop to Mac, mstsc Mac, Mac remote access, Mac screen sharing, remote control Mac, Apple Silicon remote desktop, Rust RDP server, VNC alternative Mac, FreeRDP Mac server, macOS remote desktop, Mac remote desktop solution, RDP for macOS

</details>
