# HomePod Cast

Stream **all your Windows PC audio to a HomePod** (or any AirPlay 2 speaker) from a tiny
system-tray app.

Windows can't send AirPlay on its own, and a HomePod requires the encrypted AirPlay 2
handshake (HomeKit transient pairing) — which is why most "free" tools don't work with it.
HomePod Cast captures your system audio via WASAPI loopback and streams it over AirPlay 2.

```
  ◯  (tray icon: ring = stopped, ● dot = streaming)
  ┌──────────────────────┐
  │ ■ Stopped         ✓  │   ← single-tick: exactly one is true
  │ ▶ Living Room        │
  │ ▶ Kitchen            │
  │ Settings…            │   ← volume slider + custom toggle hotkey
  │ Quit                 │
  └──────────────────────┘
```

## Features

- One-click streaming to any AirPlay 2 speaker from the system tray
- Tray icon shows state at a glance (hollow ring = stopped, filled dot = streaming)
- Single-selection menu — pick a speaker or **Stopped**, the tick follows reality
- **Volume slider** in Settings — applies live, connects at a low 25% (no jump-scare), and remembers your last setting
- **Custom global hotkey** to toggle on/off from anywhere — press your own combo in Settings (defaults to **Ctrl+Alt+H**), remembered across restarts
- Single ~11 MB executable, no installer, starts stopped

## Requirements

- Windows 10/11
- The PC and the speaker on the **same Wi-Fi/LAN**
- A one-time **Windows Firewall** inbound rule (AirPlay 2 has the speaker connect *back*
  to the PC, which Windows blocks by default)

## Usage

1. Run `homepod-cast.exe` — a teal icon appears in the tray (it starts stopped).
2. Right-click → pick your speaker. Play anything; it comes out of the speaker.
3. Right-click → **Settings…** to drag the volume slider (applies live) or set your own
   toggle hotkey (click the box, press the keys, click **Save shortcut**).
4. Click **■ Stopped** to stop, or press your hotkey to toggle on/off from anywhere.

If the speaker never appears or audio won't start, allow the app through the firewall
(run as admin, adjust the path):

```powershell
netsh advfirewall firewall add rule name="HomePod Cast" dir=in action=allow program="C:\path\to\homepod-cast.exe" enable=yes profile=any
```

## Build from source

Requires the Rust **GNU** toolchain and **MinGW-w64** (`gcc` + `dlltool`) on `PATH`:

```powershell
rustup default stable-x86_64-pc-windows-gnu
# install MinGW-w64 (e.g. from https://winlibs.com) and add its bin\ to PATH
cargo build -p homepod-cast --release
```

Output: `target/release/homepod-cast.exe` (no console window). `./build.ps1` does this and
copies it to `dist/HomePod Cast.exe`.

## Notes & limitations

- Best for **music and podcasts** — AirPlay buffers ~1–2 s, so it's not suited to video
  lip-sync or games.
- Audio is sent as ALAC over AirPlay 2 with NTP timing; a ~2 s keepalive keeps the session
  alive so playback doesn't drop.

## How it relates to airplay2-rs

This repo is a fork of [**airplay2-rs**](https://github.com/lmcgartland/airplay2-rs), which
implements the AirPlay 2 sender stack (discovery, HomeKit pairing, encryption, ALAC/RTP,
timing). HomePod Cast adds:

- `crates/homepod-cast` — the Windows capture + tray application
- Minor Windows-portability patches to `crates/airplay-audio` (cross-platform socket QoS in
  `rtp.rs`; `fdk-aac` made an optional `aac` feature, since it won't build on GCC ≥ 14 and
  HomePod uses ALAC)

## License

GPL-2.0, inherited from airplay2-rs. See [`LICENSE`](LICENSE).
