//! System-tray UI.
//!
//! The tray (main thread) renders state and turns clicks / the global hotkey
//! into commands. A dedicated control thread owns the tokio runtime and the
//! active streaming session, and reports the *true* state back so the icon and
//! check marks stay correct (e.g. if a connection fails).

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG,
};

use crate::cast;

const WM_HOTKEY: u32 = 0x0312;
const PM_REMOVE: u32 = 0x0001;
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_NOREPEAT: u32 = 0x4000;
const HOTKEY_ID: i32 = 1;

/// Commands sent from the tray to the control thread.
enum Cmd {
    Start(usize),
    Stop,
    Quit,
}

/// True state reported by the control thread back to the tray.
enum Status {
    Streaming(usize),
    Stopped,
}

/// A selectable global-hotkey preset for the on/off toggle.
struct Preset {
    label: &'static str,
    mods: u32,
    vk: u32, // 0 = disabled
}

fn presets() -> Vec<Preset> {
    vec![
        Preset { label: "Ctrl+Alt+H", mods: MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, vk: 0x48 },
        Preset { label: "Ctrl+Shift+H", mods: MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, vk: 0x48 },
        Preset { label: "Ctrl+Alt+P", mods: MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, vk: 0x50 },
        Preset { label: "Disabled", mods: 0, vk: 0 },
    ]
}

/// Register the given preset as the global toggle hotkey (unregistering first).
fn set_hotkey(p: &Preset) {
    unsafe {
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
        if p.vk != 0 {
            RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, p.mods, p.vk);
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    let (dev_tx, dev_rx) = std::sync::mpsc::channel::<Vec<String>>();
    let (status_tx, status_rx) = std::sync::mpsc::channel::<Status>();

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

    // Settings submenu: pick the global toggle shortcut.
    let preset_list = presets();
    let settings = Submenu::new("Settings", true);
    settings.append(&MenuItem::new("Toggle on/off shortcut", false, None))?;
    let mut preset_checks: Vec<CheckMenuItem> = Vec::new();
    let mut preset_ids: Vec<(MenuId, usize)> = Vec::new();
    for (i, p) in preset_list.iter().enumerate() {
        let item = CheckMenuItem::new(p.label, true, i == 0, None);
        preset_ids.push((item.id().clone(), i));
        settings.append(&item)?;
        preset_checks.push(item);
    }
    menu.append(&settings)?;
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

    // Register the default toggle hotkey.
    let mut current_preset = 0usize;
    set_hotkey(&preset_list[current_preset]);

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
            } else if let Some(idx) = device_ids.iter().find(|(id, _)| *id == ev.id).map(|(_, i)| *i) {
                last_device = idx;
                if active != Some(idx) {
                    let _ = cmd_tx.send(Cmd::Start(idx));
                    active = Some(idx);
                }
                apply_state(&off_check, &device_checks, &tray, active);
            } else if let Some(p) = preset_ids.iter().find(|(id, _)| *id == ev.id).map(|(_, i)| *i) {
                current_preset = p;
                set_hotkey(&preset_list[p]);
                for (i, c) in preset_checks.iter().enumerate() {
                    c.set_checked(i == current_preset);
                }
            }
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
    for cmd in cmd_rx {
        match cmd {
            Cmd::Start(idx) => {
                if let Some(s) = session.take() {
                    rt.block_on(s.stop());
                }
                if let Some(dev) = devices.get(idx).cloned() {
                    let name = dev.name.clone();
                    match rt.block_on(cast::Session::start(dev)) {
                        Ok(s) => {
                            tracing::info!("streaming to {name}");
                            session = Some(s);
                            let _ = status_tx.send(Status::Streaming(idx));
                        }
                        Err(e) => {
                            tracing::error!("failed to start streaming to {name}: {e:#}");
                            let _ = status_tx.send(Status::Stopped);
                        }
                    }
                }
            }
            Cmd::Stop => {
                if let Some(s) = session.take() {
                    rt.block_on(s.stop());
                    tracing::info!("stopped");
                }
                let _ = status_tx.send(Status::Stopped);
            }
            Cmd::Quit => {
                if let Some(s) = session.take() {
                    rt.block_on(s.stop());
                }
                break;
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
