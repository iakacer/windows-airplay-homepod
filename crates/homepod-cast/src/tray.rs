//! System-tray UI.
//!
//! The tray (main thread) renders state and turns clicks / the global hotkey
//! into commands. A dedicated control thread owns the tokio runtime and the
//! active streaming session, and reports the *true* state back so the icon and
//! check marks stay correct (e.g. if a connection fails).
//!
//! Volume and the global toggle hotkey are configured in a small native
//! settings window (see [`crate::settings_window`]): the slider applies volume
//! live, and Save reports a new hotkey which the main thread re-registers.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG,
};

use crate::cast;
use crate::settings_window;

const WM_HOTKEY: u32 = 0x0312;
const PM_REMOVE: u32 = 0x0001;
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_NOREPEAT: u32 = 0x4000;
const HOTKEY_ID: i32 = 1;

/// Default global toggle hotkey: Ctrl+Alt+H (used until the user picks one).
const DEFAULT_HOTKEY_MODS: u32 = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
const DEFAULT_HOTKEY_VK: u32 = 0x48; // 'H'

/// Commands sent from the tray to the control thread.
enum Cmd {
    Start(usize),
    Stop,
    SetVolume(f32),
    Quit,
}

/// True state reported by the control thread back to the tray.
enum Status {
    Streaming(usize),
    Stopped,
}

/// Register the given hotkey as the global toggle (unregistering any prior one).
/// `vk == 0` disables the hotkey.
fn set_hotkey(mods: u32, vk: u32) {
    unsafe {
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
        if vk != 0 {
            RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, mods, vk);
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    let (dev_tx, dev_rx) = std::sync::mpsc::channel::<Vec<String>>();
    let (status_tx, status_rx) = std::sync::mpsc::channel::<Status>();
    // New hotkey chosen in the settings window -> main thread re-registers it.
    let (hk_tx, hk_rx) = std::sync::mpsc::channel::<(u32, u32)>();

    // Control thread: scans for devices, then owns the streaming session.
    let control = std::thread::spawn(move || control_loop(cmd_rx, dev_tx, status_tx));

    // Wait for the initial device scan.
    let names = dev_rx.recv_timeout(Duration::from_secs(8)).unwrap_or_default();

    // --- Build the menu ---
    let menu = Menu::new();

    // Single-selection group: "Stopped" + one item per device. Starts stopped.
    let off_check = CheckMenuItem::new("\u{25A0}  Stopped", true, true, None);
    menu.append(&off_check)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let mut device_checks: Vec<CheckMenuItem> = Vec::new();
    let mut device_ids: Vec<(MenuId, usize)> = Vec::new();
    if names.is_empty() {
        let none = MenuItem::new("No AirPlay devices found", false, None);
        menu.append(&none)?;
    } else {
        for (i, name) in names.iter().enumerate() {
            let item = CheckMenuItem::new(format!("\u{25B6}  {name}"), true, false, None);
            device_ids.push((item.id().clone(), i));
            menu.append(&item)?;
            device_checks.push(item);
        }
    }
    menu.append(&PredefinedMenuItem::separator())?;

    // Settings: opens a window with a volume slider and a hotkey capture box.
    let settings_item = MenuItem::new("Settings\u{2026}", true, None);
    menu.append(&settings_item)?;
    let settings_id = settings_item.id().clone();
    menu.append(&PredefinedMenuItem::separator())?;

    let quit_item = MenuItem::new("Quit", true, None);
    menu.append(&quit_item)?;
    let quit_id = quit_item.id().clone();
    let off_id = off_check.id().clone();

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("HomePod Cast — stream PC audio to AirPlay")
        .with_icon(make_icon(false))
        .build()?;

    // Register the saved toggle hotkey (or the default if none saved yet).
    let (mut cur_mods, mut cur_vk) =
        cast::load_hotkey().unwrap_or((DEFAULT_HOTKEY_MODS, DEFAULT_HOTKEY_VK));
    set_hotkey(cur_mods, cur_vk);

    let menu_rx = tray_icon::menu::MenuEvent::receiver();

    // UI state (optimistic; corrected by Status from the control thread).
    let mut active: Option<usize> = None; // None = stopped
    let mut last_device: usize = 0; // hotkey target when toggling on

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    let mut quit = false;
    while !quit {
        // Pump Windows messages (drives the tray + delivers WM_HOTKEY).
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if msg.message == WM_HOTKEY && msg.wParam == HOTKEY_ID as usize {
                if active.is_some() {
                    let _ = cmd_tx.send(Cmd::Stop);
                    active = None;
                } else if !device_checks.is_empty() {
                    let _ = cmd_tx.send(Cmd::Start(last_device));
                    active = Some(last_device);
                }
                apply_state(&off_check, &device_checks, &tray, active);
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Menu clicks.
        while let Ok(ev) = menu_rx.try_recv() {
            if ev.id == quit_id {
                let _ = cmd_tx.send(Cmd::Quit);
                quit = true;
            } else if ev.id == off_id {
                if active.is_some() {
                    let _ = cmd_tx.send(Cmd::Stop);
                    active = None;
                }
                apply_state(&off_check, &device_checks, &tray, active);
            } else if ev.id == settings_id {
                // Open the settings window: volume applies live via Cmd::SetVolume,
                // a chosen hotkey comes back on hk_tx for the main thread to register.
                let vol_tx = cmd_tx.clone();
                let hotkey_tx = hk_tx.clone();
                settings_window::open(
                    cast::load_volume(),
                    cur_mods,
                    cur_vk,
                    move |v| {
                        let _ = vol_tx.send(Cmd::SetVolume(v));
                    },
                    move |m, vk| {
                        let _ = hotkey_tx.send((m, vk));
                    },
                );
            } else if let Some(idx) = device_ids.iter().find(|(id, _)| *id == ev.id).map(|(_, i)| *i) {
                last_device = idx;
                if active != Some(idx) {
                    let _ = cmd_tx.send(Cmd::Start(idx));
                    active = Some(idx);
                }
                apply_state(&off_check, &device_checks, &tray, active);
            }
        }

        // Hotkey chosen in the settings window: re-register and persist.
        while let Ok((m, vk)) = hk_rx.try_recv() {
            cur_mods = m;
            cur_vk = vk;
            set_hotkey(cur_mods, cur_vk);
            cast::save_hotkey(cur_mods, cur_vk);
        }

        // True state from the control thread (corrects optimistic guesses).
        while let Ok(st) = status_rx.try_recv() {
            active = match st {
                Status::Streaming(i) => {
                    last_device = i;
                    Some(i)
                }
                Status::Stopped => None,
            };
            apply_state(&off_check, &device_checks, &tray, active);
        }

        if quit {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    unsafe { UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID) };
    let _ = control.join();
    // Guarantee termination even if a leaked library thread lingers.
    std::process::exit(0);
}

/// Reflect the current state in the check marks and tray icon.
fn apply_state(off: &CheckMenuItem, devices: &[CheckMenuItem], tray: &TrayIcon, active: Option<usize>) {
    off.set_checked(active.is_none());
    for (i, c) in devices.iter().enumerate() {
        c.set_checked(active == Some(i));
    }
    let _ = tray.set_icon(Some(make_icon(active.is_some())));
}

/// Owns the tokio runtime and the active session; reports state via `status_tx`.
fn control_loop(cmd_rx: Receiver<Cmd>, dev_tx: Sender<Vec<String>>, status_tx: Sender<Status>) {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("failed to start runtime: {e}");
            let _ = dev_tx.send(Vec::new());
            return;
        }
    };

    let devices = rt
        .block_on(cast::discover(Duration::from_secs(3)))
        .unwrap_or_default();
    let _ = dev_tx.send(devices.iter().map(|d| d.name.clone()).collect());

    let mut session: Option<cast::Session> = None;
    let mut volume = cast::load_volume();
    let mut last_feedback = std::time::Instant::now();
    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(Cmd::Start(idx)) => {
                if let Some(s) = session.take() {
                    rt.block_on(s.stop());
                }
                if let Some(dev) = devices.get(idx).cloned() {
                    let name = dev.name.clone();
                    match rt.block_on(cast::Session::start(dev, volume)) {
                        Ok(s) => {
                            tracing::info!("streaming to {name}");
                            session = Some(s);
                            last_feedback = std::time::Instant::now();
                            let _ = status_tx.send(Status::Streaming(idx));
                        }
                        Err(e) => {
                            tracing::error!("failed to start streaming to {name}: {e:#}");
                            let _ = status_tx.send(Status::Stopped);
                        }
                    }
                }
            }
            Ok(Cmd::Stop) => {
                if let Some(s) = session.take() {
                    rt.block_on(s.stop());
                    tracing::info!("stopped");
                }
                let _ = status_tx.send(Status::Stopped);
            }
            Ok(Cmd::SetVolume(v)) => {
                volume = v;
                cast::save_volume(v);
                if let Some(s) = session.as_mut() {
                    rt.block_on(s.set_volume(v));
                }
            }
            Ok(Cmd::Quit) => {
                if let Some(s) = session.take() {
                    rt.block_on(s.stop());
                }
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Periodic AirPlay keepalive — without it the HomePod tears down the
        // session and audio stops after a short while.
        if let Some(s) = session.as_mut() {
            if last_feedback.elapsed() >= Duration::from_secs(2) {
                rt.block_on(s.feedback());
                last_feedback = std::time::Instant::now();
            }
        }
    }
    // The library leaks an infinite spawn_blocking task per session, so a normal
    // runtime drop would hang. Force shutdown instead.
    rt.shutdown_timeout(Duration::from_millis(300));
}

/// 32x32 teal icon: a filled disc when active, a hollow ring when stopped.
fn make_icon(filled: bool) -> Icon {
    let size: u32 = 32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    let r_outer = 14.0_f32;
    let r_inner = 9.0_f32;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d2 = dx * dx + dy * dy;
            let inside = if filled {
                d2 <= r_outer * r_outer
            } else {
                d2 <= r_outer * r_outer && d2 >= r_inner * r_inner
            };
            if inside {
                let idx = ((y * size + x) * 4) as usize;
                rgba[idx] = 0x1e;
                rgba[idx + 1] = 0xb0;
                rgba[idx + 2] = 0xa6;
                rgba[idx + 3] = 0xff;
            }
        }
    }
    Icon::from_rgba(rgba, size, size).expect("valid icon")
}
