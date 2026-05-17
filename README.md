<div align="center">
  <img src="sone.png" alt="SONE" width="150">
  <h1>SONE (Windows Port)</h1>
  <p>The native desktop client for <a href="https://tidal.com">TIDAL</a> on Windows. Lossless streaming with bit-perfect WASAPI exclusive output up to 24-bit/192kHz (MAX) — your DAC, not the Windows resampler. </p>
  <p>⚠️ Ported with help from AI agents</p>

  [![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
  [![Platform: Windows](https://img.shields.io/badge/Platform-Windows-blue.svg)]()
  [![Built with Tauri 2](https://img.shields.io/badge/Built_with-Tauri_2-orange.svg)](https://v2.tauri.app/)
</div>

> [!IMPORTANT]
> Requires an active [TIDAL](https://tidal.com) subscription. Not affiliated with TIDAL.

## Special Port Mention & Credits

This project is a Windows port of **SONE**, a brilliant native Linux desktop client for TIDAL. 

* **Original Repository & Credits**: Huge thanks to the original creator **[lullabyX](https://github.com/lullabyX)** for their incredible work on **[lullabyX/sone](https://github.com/lullabyX/sone)**.
* **Disclaimer**: This Windows port was created for personal use and **may not be actively or correctly maintained** in the future.

---

<p align="center">
  <img src="data/sone_homepage_readme.png" width="32%" alt="SONE TIDAL client — home page with lossless streaming library" />
  <img src="data/sone_drawer_readme.png" width="32%" alt="SONE now playing drawer — Hi-Res FLAC playback with synced lyrics" />
  <img src="data/sone_theme_readme.png" width="32%" alt="SONE custom theme — native music player with full color customization" />
</p>

## The Vision

The Windows desktop app TIDAL should have built. 

SONE delivers the complete, fully-featured experience you expect with seamless library management and a sleek, familiar workflow—and then supercharges it. 

We went beyond the basics with direct-to-DAC bit-perfect **WASAPI exclusive** output, a resizable-adaptive floating miniplayer, custom themes, Discord Rich Presence, and multi-service scrobbling (Last.fm, Libre.fm, ListenBrainz)—all wrapped in a fast, native Windows application.

<details>
<summary>Table of Contents</summary>

- [Features](#features)
- [Why SONE?](#why-sone)
- [Building from Source](#building-from-source)
- [Usage](#usage)
- [FAQ](#faq)
- [Tech Stack](#tech-stack)
- [License](#license)

</details>

## Features

### Audio

- **Lossless FLAC and High Quality streaming** up to Hi-Res (24-bit/192kHz) with automatic quality fallback
- **Bit-perfect output** — no resampling, no dithering. Your DAC receives the unaltered decoded signal
- **Exclusive WASAPI** — bypasses the Windows Audio Engine entirely for direct hardware access
- **Smart DAC matching** — automatically detects your hardware's supported formats and sample rates, picking the best fit
- **Volume normalization** (ReplayGain) with automatic context switching between album and track gain
- **Autoplay** — discovers and plays similar tracks when your queue ends

### Interface

- **Custom themes** — 12 presets and a full color picker for accent and background with both light/dark mode
- **Lyrics** — synced lyrics display for supported tracks
- **Miniplayer** — compact floating window with album art, playback controls, and resizable-adaptive layout
- **Full-screen player** — maximized view with album art, lyrics option and auto-hiding controls
- **Queue persistence** — picks up where you left off across restarts
- **Windows SMTC Integration** — full system media keys, play/pause state, track metadata, and taskbar audio integration
- **Scrobbling** — track your listening history on Last.fm, Libre.fm, and ListenBrainz with full ISRC and MusicBrainz metadata
- **Proxy support** — route traffic through HTTP, HTTPS, or SOCKS5 proxies
- **Discord Rich Presence** — show what you're listening to with album art, track info, and a direct TIDAL link
- **System tray** with playback controls and minimize-to-tray
- **Keyboard shortcuts** for all common actions with a built-in shortcut overlay

### Library

- **Library management** — browse and sort your playlists, albums, artists, and mixes with playlist folder support
- **Share** — share tracks, albums, playlists, artists, and mixes with your friends via a direct TIDAL link
- **Deep links** — open `tidal://` URLs directly in SONE

## Why SONE?

SONE is a lightweight, native alternative to the official TIDAL desktop player.

- **Full audio quality** — browsers and standard Electron apps downsample audio to a fixed Windows sample rate (often 48kHz) before it leaves the application. SONE is native — it outputs at the source's original sample rate, up to 192kHz (TIDAL's max). Exclusive WASAPI mode bypasses the system mixer entirely for bit-perfect output to your DAC.
- **Familiar interface** — a modern UI inspired by the streaming apps you already use
- **Direct hardware access** — GStreamer talks directly to your audio hardware. Lock your DAC to the exact source format, bypassing the Windows mixer
- **Lightweight** — built with Tauri and Rust. Small binary, low memory footprint
- **Encrypted at rest** — credentials, cache, and settings are encrypted with AES-256-GCM
- **No telemetry, no tracking** — fully open source under GPL-3.0. Your listening data stays on your machine

## Building from Source

To compile Sone on Windows and package it with a minimal, portable GStreamer runtime (~20MB):

### Prerequisites

1. **Rust**: Install via [rustup.rs](https://rustup.rs/).
2. **Node.js**: Install Node.js 18+ (LTS).

### 1. Install GStreamer MSVC

Sone uses **GStreamer (MSVC 64-bit)** to handle audio decoding and streaming.

1. Go to the [GStreamer download page](https://gstreamer.freedesktop.org/download/).
2. Under **Windows**, download and run the latest **MSVC 64-bit** installers:
   * **Runtime installer** (e.g. `gstreamer-1.0-msvc-x86_64-*.msi`)
   * **Development installer** (e.g. `gstreamer-1.0-devel-msvc-x86_64-*.msi`)
3. Choose a **Complete** installation for both.

### 2. Create the Bundled Runtime Folder

To bundle only the essential DLLs inside Sone's installer, create a folder under Sone at `src-tauri/gstreamer-runtime/` and structure it with files sourced from your GStreamer installation (`C:\Users\<username>\AppData\Local\Programs\gstreamer\1.0\msvc_x86_64\` by default):

```text
src-tauri/gstreamer-runtime/
├── *.dll (Core DLLs)
└── lib/
    ├── gio/
    │   └── modules/
    │       └── gioopenssl.dll (GIO Module)
    └── gstreamer-1.0/
        └── *.dll (Plugin DLLs)
```

##### Required DLL List:

* **Core DLLs** (from GStreamer's `bin/`):
  `ffi-7.dll`, `FLAC-8.dll`, `gio-2.0-0.dll`, `glib-2.0-0.dll`, `gmodule-2.0-0.dll`, `gobject-2.0-0.dll`, `gstadaptivedemux-1.0-0.dll`, `gstaudio-1.0-0.dll`, `gstbase-1.0-0.dll`, `gstisoff-1.0-0.dll`, `gstnet-1.0-0.dll`, `gstpbutils-1.0-0.dll`, `gstreamer-1.0-0.dll`, `gstriff-1.0-0.dll`, `gstrtp-1.0-0.dll`, `gsttag-1.0-0.dll`, `gsturidownloader-1.0-0.dll`, `gstvideo-1.0-0.dll`, `intl-8.dll`, `libcrypto-3-x64.dll`, `libssl-3-x64.dll`, `nghttp2.dll`, `ogg-0.dll`, `orc-0.4-0.dll`, `pcre2-8-0.dll`, `psl-5.dll`, `soup-3.0-0.dll`, `sqlite3-0.dll`, `xml2-16.dll`, `z-1.dll`.

* **Plugins** (from GStreamer's `lib/gstreamer-1.0/`):
  `gstadaptivedemux2.dll`, `gstasio.dll`, `gstaudioconvert.dll`, `gstaudioparsers.dll`, `gstaudioresample.dll`, `gstcoreelements.dll`, `gstdash.dll`, `gstdecklink.dll`, `gstflac.dll`, `gstisomp4.dll`, `gstplayback.dll`, `gstsoup.dll`, `gsttypefindfunctions.dll`, `gstvolume.dll`, `gstwasapi2.dll`, `gstwinks.dll`.

* **GIO Modules** (from GStreamer's `lib/gio/modules/`):
  `gioopenssl.dll` (essential for secure HTTPS connection to TIDAL).

### 3. Run the Preparation Script

Open your terminal and run the preparation script to automatically configure Tauri's installers:

```bash
node scripts/prepare-gstreamer.js
```

This generates `gstreamer-hooks.nsi` and `gstreamer-fragment.wxs` dynamically based on your files.

### 4. Build and Run

```bash
npm install
npm run tauri dev          # Development mode
npm run tauri build        # Release build (produces standalone .exe and .msi)
```

---

## Usage

1. Launch the app
2. Click **Get Login Code**. You'll be automatically redirected to the official [link.tidal.com](https://link.tidal.com) to login and approve Sone. Optionally, scan the **QR Code** to login via your mobile device.
3. Your library loads automatically — browse and play!

---

## FAQ

<details>
<summary>I'm getting a "Device busy" error in exclusive or bit-perfect mode</summary>

Another application is already holding the exclusive lock on your audio device. Exclusive and bit-perfect modes need direct hardware access — only one application can hold the device at a time. Close the locking application or change the output device inside SONE's settings.

</details>

<details>
<summary>What is the difference between exclusive mode and bit-perfect mode?</summary>

Both bypass Windows audio engine/WASAPI shared resampler and write directly to the hardware device.

**Exclusive mode** locks the device so no other app can play sounds. Audio is converted to a fixed format (32-bit integer, stereo) while preserving the source's native sample rate — no resampling occurs. You still have software volume control and volume normalization (ReplayGain).

**Bit-perfect mode** goes a step further. There is zero processing — no format conversion, no resampling, no volume control. The decoded audio reaches your DAC exactly as it was encoded. Sone's volume slider is disabled. This is the mode to use if you want the purest signal path to your DAC.

</details>

---

## Tech Stack

- **Backend:** Rust ([Tauri 2](https://v2.tauri.app/))
- **Frontend:** React 19, Tailwind 4, Jotai
- **Audio:** [GStreamer](https://gstreamer.freedesktop.org/) (WASAPI backend)
- **Config:** `%APPDATA%/sone/`

---

## License

[GPL-3.0-only](LICENSE)

---

**TL;DR** — SONE is an open-source, native Windows desktop client for TIDAL built with Tauri 2 and Rust. It streams lossless FLAC and Hi-Res audio up to 24-bit/192kHz, with exclusive WASAPI output that bypasses the Windows Audio Engine entirely for bit-perfect playback directly to your DAC. Lightweight, encrypted at rest — no telemetry, no tracking.
