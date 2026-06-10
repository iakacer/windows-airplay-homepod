//! A small native Win32 settings window with a volume slider and a hotkey
//! capture box.
//!
//! It uses the OS built-in trackbar (`msctls_trackbar32`) and hotkey
//! (`msctls_hotkey32`) controls rather than a GUI toolkit, so it adds no heavy
//! dependencies and coexists with the tray's own message loop. The window runs
//! on its own thread (Win32 windows are thread-affine) and reports changes back
//! through two callbacks: volume is applied live as the slider moves; the hotkey
//! is reported when the user clicks Save.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetSysColorBrush, UpdateWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, IDC_ARROW, MSG, WNDCLASSW,
};

// --- Win32 constants (kept local to avoid extra windows-sys features) ---
const GWLP_USERDATA: i32 = -21;
const WM_DESTROY: u32 = 0x0002;
const WM_COMMAND: u32 = 0x0111;
const WM_HSCROLL: u32 = 0x0114;
const WM_CLOSE: u32 = 0x0010;

const WS_CHILD: u32 = 0x4000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_TABSTOP: u32 = 0x0001_0000;
const WS_BORDER: u32 = 0x0080_0000;
const WS_OVERLAPPED: u32 = 0x0000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const SW_SHOW: i32 = 5;
const CW_USEDEFAULT: i32 = i32::MIN; // 0x80000000

// Trackbar messages / styles.
const WM_USER: u32 = 0x0400;
const TBM_GETPOS: u32 = WM_USER;
const TBM_SETPOS: u32 = WM_USER + 5;
const TBM_SETRANGE: u32 = WM_USER + 6;
const TBS_AUTOTICKS: u32 = 0x0001;
const TBS_HORZ: u32 = 0x0000;

// Hotkey control messages / modifier flags (HOTKEYF_*).
const HKM_SETHOTKEY: u32 = WM_USER + 1;
const HKM_GETHOTKEY: u32 = WM_USER + 2;
const HOTKEYF_SHIFT: u32 = 0x01;
const HOTKEYF_CONTROL: u32 = 0x02;
const HOTKEYF_ALT: u32 = 0x04;

// RegisterHotKey modifier flags (MOD_*).
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_NOREPEAT: u32 = 0x4000;

const ID_SAVE: usize = 100;
const ID_CLOSE: usize = 101;

// Common Controls init (avoids needing the Win32_UI_Controls feature).
#[repr(C)]
struct INITCOMMONCONTROLSEX {
    dw_size: u32,
    dw_icc: u32,
}
const ICC_BAR_CLASSES: u32 = 0x0004;
const ICC_HOTKEY_CLASS: u32 = 0x0040;
#[link(name = "comctl32")]
extern "system" {
    fn InitCommonControlsEx(picce: *const INITCOMMONCONTROLSEX) -> i32;
}

/// Only one settings window at a time.
static OPEN: AtomicBool = AtomicBool::new(false);

/// State shared with the window procedure (stored behind `GWLP_USERDATA`).
struct WindowState {
    trackbar: HWND,
    hotkey_ctrl: HWND,
    pct_label: HWND,
    on_volume: Box<dyn Fn(f32)>,
    on_hotkey: Box<dyn Fn(u32, u32)>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Translate `RegisterHotKey` MOD_* flags into the hotkey control's HOTKEYF_*.
fn mod_to_hotkeyf(mods: u32) -> u32 {
    let mut f = 0;
    if mods & MOD_ALT != 0 {
        f |= HOTKEYF_ALT;
    }
    if mods & MOD_CONTROL != 0 {
        f |= HOTKEYF_CONTROL;
    }
    if mods & MOD_SHIFT != 0 {
        f |= HOTKEYF_SHIFT;
    }
    f
}

/// Translate the hotkey control's HOTKEYF_* flags back into MOD_* (always with
/// MOD_NOREPEAT so holding the keys doesn't retrigger).
fn hotkeyf_to_mod(f: u32) -> u32 {
    let mut m = MOD_NOREPEAT;
    if f & HOTKEYF_ALT != 0 {
        m |= MOD_ALT;
    }
    if f & HOTKEYF_CONTROL != 0 {
        m |= MOD_CONTROL;
    }
    if f & HOTKEYF_SHIFT != 0 {
        m |= MOD_SHIFT;
    }
    m
}

/// Open the settings window (no-op if one is already open).
///
/// `on_volume(0.0..=1.0)` fires live as the slider moves; `on_hotkey(mods, vk)`
/// fires when Save is clicked with a valid key combination.
pub fn open(
    initial_volume: f32,
    initial_mods: u32,
    initial_vk: u32,
    on_volume: impl Fn(f32) + Send + 'static,
    on_hotkey: impl Fn(u32, u32) + Send + 'static,
) {
    if OPEN.swap(true, Ordering::SeqCst) {
        return; // already open
    }
    std::thread::spawn(move || {
        unsafe {
            run_window(
                initial_volume,
                initial_mods,
                initial_vk,
                Box::new(on_volume),
                Box::new(on_hotkey),
            );
        }
        OPEN.store(false, Ordering::SeqCst);
    });
}

unsafe fn run_window(
    initial_volume: f32,
    initial_mods: u32,
    initial_vk: u32,
    on_volume: Box<dyn Fn(f32)>,
    on_hotkey: Box<dyn Fn(u32, u32)>,
) {
    let hinstance = GetModuleHandleW(std::ptr::null());

    let icc = INITCOMMONCONTROLSEX {
        dw_size: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dw_icc: ICC_BAR_CLASSES | ICC_HOTKEY_CLASS,
    };
    InitCommonControlsEx(&icc);

    let class_name = wide("HomePodCastSettings");
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: std::ptr::null_mut(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
        hbrBackground: GetSysColorBrush(15), // COLOR_BTNFACE
        lpszMenuName: std::ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    // Re-registering an existing class is harmless; ignore the result.
    RegisterClassW(&wc);

    let title = wide("HomePod Cast — Settings");
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        title.as_ptr(),
        style,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        372,
        250,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinstance,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return;
    }

    let child = |class: &[u16], text: &[u16], style: u32, x, y, w, h, id: usize| -> HWND {
        CreateWindowExW(
            0,
            class.as_ptr(),
            text.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            w,
            h,
            hwnd,
            id as *mut c_void,
            hinstance,
            std::ptr::null(),
        )
    };

    let static_cls = wide("STATIC");
    let button_cls = wide("BUTTON");
    let trackbar_cls = wide("msctls_trackbar32");
    let hotkey_cls = wide("msctls_hotkey32");

    child(&static_cls, &wide("Volume"), 0, 16, 16, 120, 20, 0);
    let trackbar = child(
        &trackbar_cls,
        &wide(""),
        WS_TABSTOP | TBS_AUTOTICKS | TBS_HORZ,
        14,
        40,
        250,
        32,
        0,
    );
    let pct_label = child(&static_cls, &wide(""), 0, 280, 44, 60, 20, 0);

    child(&static_cls, &wide("Toggle on/off shortcut"), 0, 16, 92, 220, 20, 0);
    let hotkey_ctrl = child(
        &hotkey_cls,
        &wide(""),
        WS_TABSTOP | WS_BORDER,
        16,
        114,
        200,
        24,
        0,
    );

    child(&button_cls, &wide("Save shortcut"), WS_TABSTOP, 16, 158, 110, 30, ID_SAVE);
    child(&button_cls, &wide("Close"), WS_TABSTOP, 250, 158, 90, 30, ID_CLOSE);

    // Initialise the slider (0–100) and percent label.
    SendMessageW(trackbar, TBM_SETRANGE, 1, make_lparam(0, 100));
    let pos = (initial_volume.clamp(0.0, 1.0) * 100.0).round() as i32;
    SendMessageW(trackbar, TBM_SETPOS, 1, pos as LPARAM);
    set_pct(pct_label, pos);

    // Initialise the hotkey control: LOWORD = vk, next byte = HOTKEYF flags.
    let hk_word = (initial_vk & 0xff) | (mod_to_hotkeyf(initial_mods) << 8);
    SendMessageW(hotkey_ctrl, HKM_SETHOTKEY, hk_word as WPARAM, 0);

    let state = Box::new(WindowState {
        trackbar,
        hotkey_ctrl,
        pct_label,
        on_volume,
        on_hotkey,
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);

    ShowWindow(hwnd, SW_SHOW);
    UpdateWindow(hwnd);

    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// Pack two 16-bit values into an LPARAM (low, high).
fn make_lparam(low: i32, high: i32) -> LPARAM {
    (((high as u32) << 16) | (low as u32 & 0xffff)) as LPARAM
}

unsafe fn set_pct(label: HWND, pos: i32) {
    let text = wide(&format!("{pos}%"));
    SetWindowTextW(label, text.as_ptr());
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;

    match msg {
        WM_HSCROLL if !state.is_null() => {
            let s = &*state;
            let pos = SendMessageW(s.trackbar, TBM_GETPOS, 0, 0) as i32;
            (s.on_volume)((pos as f32 / 100.0).clamp(0.0, 1.0));
            set_pct(s.pct_label, pos);
            0
        }
        WM_COMMAND if !state.is_null() => {
            let id = wparam & 0xffff;
            if id == ID_SAVE {
                let s = &*state;
                let hk = SendMessageW(s.hotkey_ctrl, HKM_GETHOTKEY, 0, 0) as u32;
                let vk = hk & 0xff;
                let flags = (hk >> 8) & 0xff;
                if vk != 0 {
                    (s.on_hotkey)(hotkeyf_to_mod(flags), vk);
                }
                DestroyWindow(hwnd);
            } else if id == ID_CLOSE {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            if !state.is_null() {
                drop(Box::from_raw(state));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
