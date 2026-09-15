//! System-tray icon + control menu (Windows).
//!
//! One `Shell_NotifyIconW` icon on a dedicated thread that owns a hidden window
//! and its own message loop. The icon is the InPhase phase-mark, rendered from
//! a baked alpha mask (`super::tray_mask`): electric blue while a session is
//! streaming, dim (50 % black) while idle.
//!
//! Right-click opens a menu that shows the current status and toggles the
//! runtime knobs — remote access, start-at-sign-in — that previously needed a
//! config edit or a setup script, plus "Open dashboard" and "Quit".

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, GWLP_USERDATA, HICON, ICONINFO,
    MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON, TPM_BOTTOMALIGN,
    TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_ENDSESSION,
    WM_LBUTTONDBLCLK, WM_NULL, WM_QUERYENDSESSION, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

use super::tray_mask;

/// Shared, mutable view the tray thread reads to build the icon + menu. The
/// host runtime updates these; [`Tray::refresh`] tells the tray to re-render.
#[derive(Default)]
pub struct TrayModel {
    /// Electric-blue icon when true, dim when false.
    pub streaming: AtomicBool,
    pub remote_access: AtomicBool,
    pub start_at_login: AtomicBool,
    /// One-line status shown (greyed) at the top of the menu and in the tooltip.
    pub status: Mutex<String>,
    /// Current pairing PIN line (greyed), e.g. `Pairing PIN: 123456`.
    pub pin_line: Mutex<String>,
}

impl TrayModel {
    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
    }

    pub fn set_pin_line(&self, s: impl Into<String>) {
        *self.pin_line.lock().unwrap() = s.into();
    }
}

/// What the menu items do. Each closure runs on the tray thread and is expected
/// to update [`TrayModel`] to match before returning.
pub struct TrayActions {
    pub open_dashboard: Box<dyn Fn() + Send + Sync>,
    pub set_remote_access: Box<dyn Fn(bool) + Send + Sync>,
    pub set_start_at_login: Box<dyn Fn(bool) + Send + Sync>,
    pub quit: Box<dyn Fn() + Send + Sync>,
}

const WM_TRAY: u32 = WM_APP + 1;
const WM_REFRESH: u32 = WM_APP + 2;
const ID_STATUS: usize = 0x10;
const ID_PIN: usize = 0x11;
const ID_REMOTE: usize = 0x21;
const ID_LOGIN: usize = 0x22;
const ID_DASH: usize = 0x23;
const ID_QUIT: usize = 0x24;
const ICON_UID: u32 = 0x1B12;

/// `RegisterWindowMessage("TaskbarCreated")` — broadcast when Explorer restarts;
/// we re-add the icon.
static WM_TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

struct Ctx {
    model: Arc<TrayModel>,
    actions: TrayActions,
    icon: Option<HICON>,
    icon_streaming: bool,
}

pub struct Tray {
    hwnd: Arc<AtomicIsize>,
    _thread: std::thread::JoinHandle<()>,
}

impl Tray {
    pub fn spawn(model: Arc<TrayModel>, actions: TrayActions) -> Option<Self> {
        let hwnd = Arc::new(AtomicIsize::new(0));
        let hwnd_out = hwnd.clone();
        let thread = std::thread::Builder::new()
            .name("inphase-tray".into())
            .spawn(move || unsafe { run(model, actions, hwnd_out) })
            .ok()?;
        Some(Self {
            hwnd,
            _thread: thread,
        })
    }

    /// A cheap, cloneable handle to poke the tray from another thread/task.
    pub fn handle(&self) -> TrayHandle {
        TrayHandle(self.hwnd.clone())
    }

    /// Re-render the icon + tooltip from the current [`TrayModel`].
    pub fn refresh(&self) {
        self.handle().refresh();
    }
}

#[derive(Clone)]
pub struct TrayHandle(Arc<AtomicIsize>);

impl TrayHandle {
    pub fn refresh(&self) {
        let h = self.0.load(Ordering::Acquire);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(HWND(h as *mut c_void), WM_REFRESH, WPARAM(0), LPARAM(0));
            }
        }
    }
}

unsafe fn run(model: Arc<TrayModel>, actions: TrayActions, hwnd_out: Arc<AtomicIsize>) {
    WM_TASKBAR_CREATED.store(
        RegisterWindowMessageW(w!("TaskbarCreated")),
        Ordering::Relaxed,
    );

    let hmod = GetModuleHandleW(None).unwrap_or_default();
    let hinst = HINSTANCE(hmod.0);
    let class = w!("InPhaseTrayWindow");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc);

    let ctx = Box::into_raw(Box::new(Ctx {
        model,
        actions,
        icon: None,
        icon_streaming: false,
    }));

    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!("InPhase"),
        WS_OVERLAPPED,
        0,
        0,
        0,
        0,
        None,
        None,
        hinst,
        None,
    ) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("tray: window creation failed: {e}");
            drop(Box::from_raw(ctx));
            return;
        }
    };
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx as isize);
    hwnd_out.store(hwnd.0 as isize, Ordering::Release);

    add_icon(hwnd);
    apply_icon(hwnd, &mut *ctx, true);
    tracing::info!("tray icon ready");

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    let nid = base_nid(hwnd);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    if let Some(ic) = (*ctx).icon.take() {
        let _ = DestroyIcon(ic);
    }
    drop(Box::from_raw(ctx));
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
        if ctx.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ctx = &mut *ctx;

        if msg == WM_TASKBAR_CREATED.load(Ordering::Relaxed) && msg != 0 {
            add_icon(hwnd);
            apply_icon(hwnd, ctx, true);
            return LRESULT(0);
        }

        match msg {
            WM_TRAY => {
                match (lp.0 as u32) & 0xFFFF {
                    WM_RBUTTONUP => show_menu(hwnd, ctx),
                    WM_LBUTTONDBLCLK => (ctx.actions.open_dashboard)(),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_REFRESH => {
                apply_icon(hwnd, ctx, false);
                LRESULT(0)
            }
            WM_COMMAND => {
                dispatch(ctx, wp.0 & 0xFFFF);
                LRESULT(0)
            }
            WM_CLOSE | WM_ENDSESSION => {
                // Restart Manager / logoff send WM_CLOSE to this hidden window.
                // DefWindowProc would destroy it and only the tray thread would
                // exit; the host process kept Program Files locked and the
                // installer showed "unable to automatically close all applications".
                (ctx.actions.quit)();
                LRESULT(0)
            }
            WM_QUERYENDSESSION => LRESULT(1),
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn dispatch(ctx: &Ctx, id: usize) {
    match id {
        ID_REMOTE => {
            let next = !ctx.model.remote_access.load(Ordering::Relaxed);
            (ctx.actions.set_remote_access)(next);
        }
        ID_LOGIN => {
            let next = !ctx.model.start_at_login.load(Ordering::Relaxed);
            (ctx.actions.set_start_at_login)(next);
        }
        ID_DASH => (ctx.actions.open_dashboard)(),
        ID_QUIT => (ctx.actions.quit)(),
        _ => {}
    }
}

unsafe fn show_menu(hwnd: HWND, ctx: &Ctx) {
    let Ok(menu) = CreatePopupMenu() else { return };

    let status = wide(&ctx.model.status.lock().unwrap());
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_GRAYED,
        ID_STATUS,
        PCWSTR(status.as_ptr()),
    );
    let pin = wide(&ctx.model.pin_line.lock().unwrap());
    if pin.len() > 1 {
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, ID_PIN, PCWSTR(pin.as_ptr()));
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    let ra = ctx.model.remote_access.load(Ordering::Relaxed);
    let _ = AppendMenuW(
        menu,
        MF_STRING | if ra { MF_CHECKED } else { MF_STRING },
        ID_REMOTE,
        w!("Remote access"),
    );
    let sl = ctx.model.start_at_login.load(Ordering::Relaxed);
    let _ = AppendMenuW(
        menu,
        MF_STRING | if sl { MF_CHECKED } else { MF_STRING },
        ID_LOGIN,
        w!("Start at sign-in"),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_DASH, w!("Open dashboard"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit InPhase"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    // Documented workaround: force the menu to dismiss cleanly.
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}

fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_UID,
        ..Default::default()
    }
}

unsafe fn add_icon(hwnd: HWND) {
    let mut nid = base_nid(hwnd);
    nid.uFlags = NIF_MESSAGE;
    nid.uCallbackMessage = WM_TRAY;
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn apply_icon(hwnd: HWND, ctx: &mut Ctx, force: bool) {
    let streaming = ctx.model.streaming.load(Ordering::Relaxed);
    let status = ctx.model.status.lock().unwrap().clone();
    let pin = ctx.model.pin_line.lock().unwrap().clone();

    let mut nid = base_nid(hwnd);
    nid.uFlags = NIF_TIP;

    if force || ctx.icon.is_none() || streaming != ctx.icon_streaming {
        if let Some(icon) = make_icon(streaming) {
            nid.uFlags |= NIF_ICON;
            nid.hIcon = icon;
            if let Some(old) = ctx.icon.replace(icon) {
                let _ = DestroyIcon(old);
            }
            ctx.icon_streaming = streaming;
        }
    }

    let tip = if pin.is_empty() {
        format!("InPhase — {status}")
    } else {
        format!("InPhase — {status} — {pin}")
    };
    for (dst, ch) in nid.szTip.iter_mut().zip(tip.encode_utf16().chain([0])) {
        *dst = ch;
    }
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// Build an `HICON` of the phase-mark tinted for the current state.
unsafe fn make_icon(streaming: bool) -> Option<HICON> {
    let want = (GetSystemMetrics(SM_CXSMICON).max(16) as usize).min(64);
    let (mask, msz): (&[u8], usize) = if want <= 20 {
        (&tray_mask::MASK_16, 16)
    } else {
        (&tray_mask::MASK_32, 32)
    };
    // #16B8FF electric blue while streaming; 50 %-alpha black while idle.
    let (r, g, b, alpha_mul): (u32, u32, u32, u32) = if streaming {
        (0x16, 0xB8, 0xFF, 255)
    } else {
        (0x00, 0x00, 0x00, 128)
    };

    let n = want;
    let mut px = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let sx = (x * msz / n).min(msz - 1);
            let sy = (y * msz / n).min(msz - 1);
            let cov = mask[sy * msz + sx] as u32;
            let a = cov * alpha_mul / 255;
            let o = (y * n + x) * 4;
            // BGRA, premultiplied.
            px[o] = (b * a / 255) as u8;
            px[o + 1] = (g * a / 255) as u8;
            px[o + 2] = (r * a / 255) as u8;
            px[o + 3] = a as u8;
        }
    }

    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: n as i32,
            biHeight: -(n as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbm_color = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        return None;
    }
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());

    // Monochrome AND mask — all zero; the color bitmap's alpha does the masking.
    let and = vec![0u8; n * n];
    let hbm_mask = CreateBitmap(
        n as i32,
        n as i32,
        1,
        1,
        Some(and.as_ptr() as *const c_void),
    );

    let ii = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let icon = CreateIconIndirect(&ii).ok();
    let _ = DeleteObject(HGDIOBJ(hbm_color.0));
    let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
    icon
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
