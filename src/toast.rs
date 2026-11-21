//! The confirmation shown after a dictation: a small card that appears at
//! the bottom of the monitor and goes away on its own.
//!
//! It never takes focus, so the window the text was pasted into stays
//! active the whole time.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, FillRect, FrameRect, GetDC,
    GetMonitorInfoW, GetTextExtentPoint32W, HDC, HFONT, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, PAINTSTRUCT, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    TextOutW,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, KillTimer, MA_NOACTIVATE, RegisterClassW,
    SW_SHOWNOACTIVATE, SetTimer, ShowWindow, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_MOUSEACTIVATE,
    WM_PAINT, WM_RBUTTONDOWN, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::util::{UiCell, wide};

const CLASS_NAME: &str = "saydrop.toast";
const TIMER_ID: usize = 1;

const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: u32 = 2;

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

pub const BACKGROUND: COLORREF = rgb(0x1f, 0x1f, 0x1f);
pub const BORDER: COLORREF = rgb(0x4a, 0x4a, 0x4a);
pub const TEXT: COLORREF = rgb(0xf2, 0xf2, 0xf2);
pub const SUBTLE: COLORREF = rgb(0xa8, 0xa8, 0xa8);
pub const OK_ACCENT: COLORREF = rgb(0x3f, 0xb9, 0x50);
pub const WARN_ACCENT: COLORREF = rgb(0xe3, 0xa0, 0x08);
pub const ERROR_ACCENT: COLORREF = rgb(0xf8, 0x51, 0x49);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ok,
    Warning,
    Error,
}

impl Kind {
    fn accent(self) -> COLORREF {
        match self {
            Kind::Ok => OK_ACCENT,
            Kind::Warning => WARN_ACCENT,
            Kind::Error => ERROR_ACCENT,
        }
    }

    /// How long the toast stays: problems need time to be read.
    fn duration_ms(self) -> u32 {
        match self {
            Kind::Ok => 1800,
            Kind::Warning => 5000,
            Kind::Error => 7000,
        }
    }
}

pub struct Message<'a> {
    pub kind: Kind,
    pub title: &'a str,
    pub detail: &'a str,
    /// Screen point that picks the monitor the toast appears on.
    pub anchor: POINT,
}

struct Toast {
    hwnd: HWND,
    kind: Kind,
    title: Vec<u16>,
    detail: Vec<u16>,
    title_font: HFONT,
    detail_font: HFONT,
    layout: Layout,
}

#[derive(Clone, Copy)]
struct Layout {
    width: i32,
    height: i32,
    accent_width: i32,
    text_x: i32,
    title_y: i32,
    detail_y: i32,
}

impl Drop for Toast {
    fn drop(&mut self) {
        // SAFETY: GDI objects created in `show`, released exactly once.
        unsafe {
            DeleteObject(self.title_font);
            DeleteObject(self.detail_font);
        }
    }
}

static TOAST: UiCell<Toast> = UiCell::new();

pub fn register_class(instance: *mut c_void) -> bool {
    let name = wide(CLASS_NAME);
    // SAFETY: the class struct is fully initialised and `name` outlives the call.
    unsafe {
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(toast_proc);
        class.hInstance = instance;
        class.lpszClassName = name.as_ptr();
        RegisterClassW(&class) != 0
    }
}

/// Close the toast if one is showing. Returns whether there was one.
pub fn dismiss() -> bool {
    let Some(toast) = TOAST.take() else {
        return false;
    };
    // SAFETY: our own window; state is already taken so the messages
    // DestroyWindow sends find nothing to act on.
    unsafe {
        KillTimer(toast.hwnd, TIMER_ID);
        DestroyWindow(toast.hwnd);
    }
    true
}

/// A Segoe UI font of the given pixel height and weight.
pub fn font(height: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    // SAFETY: `face` is NUL-terminated and outlives the call. The trailing
    // arguments ask for the default character set and ClearType quality.
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            1,
            0,
            0,
            5,
            0,
            face.as_ptr(),
        )
    }
}

/// Lay the toast out for a display of `dpi`, measuring text with `dc`.
fn build(dc: HDC, dpi: i32, message: &Message) -> Toast {
    let scale = |value: i32| value * dpi / 96;

    let title_font = font(scale(15), 600);
    let detail_font = font(scale(12), 400);
    let title = wide(message.title);
    let detail = wide(message.detail);

    let title_size = measure(dc, title_font, &title);
    let detail_size = if message.detail.is_empty() {
        SIZE { cx: 0, cy: 0 }
    } else {
        measure(dc, detail_font, &detail)
    };

    let padding = scale(14);
    let gap = if detail_size.cy > 0 { scale(6) } else { 0 };
    let accent_width = scale(4);
    let text_height = title_size.cy + gap + detail_size.cy;
    let text_x = accent_width + padding;
    Toast {
        hwnd: null_mut(),
        kind: message.kind,
        title,
        detail,
        title_font,
        detail_font,
        layout: Layout {
            width: text_x + title_size.cx.max(detail_size.cx) + padding,
            height: text_height + padding * 2,
            accent_width,
            text_x,
            title_y: padding,
            detail_y: padding + title_size.cy + gap,
        },
    }
}

pub fn show(instance: *mut c_void, message: &Message) {
    dismiss();

    // SAFETY: Win32/GDI calls with valid arguments. Every GDI object created
    // here ends up owned by the Toast (freed in Drop) or is released locally.
    unsafe {
        let monitor = MonitorFromPoint(message.anchor, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return;
        }
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) != 0 {
            dpi_x = 96;
        }
        let screen = GetDC(null_mut());
        let toast = build(screen, dpi_x as i32, message);
        ReleaseDC(null_mut(), screen);
        let layout = toast.layout;
        let kind = toast.kind;

        let work = info.rcWork;
        let x = work.left + ((work.right - work.left) - layout.width) / 2;
        let y = work.bottom - layout.height - 56 * dpi_x as i32 / 96;
        TOAST.set(toast);

        let class = wide(CLASS_NAME);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            null(),
            WS_POPUP,
            x,
            y,
            layout.width,
            layout.height,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            TOAST.take();
            return;
        }
        if let Some(toast) = TOAST.get() {
            toast.hwnd = hwnd;
        }
        round_corners(hwnd);

        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, TIMER_ID, kind.duration_ms(), None);
    }
}

/// Rounded corners on Windows 11; silently ignored on older systems.
pub fn round_corners(hwnd: HWND) {
    let corner = DWMWCP_ROUND;
    // SAFETY: the attribute value is a u32 of the size passed.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast(),
            size_of::<u32>() as u32,
        );
    }
}

/// Size of NUL-terminated `text` in `font`.
pub fn measure(dc: HDC, font: HFONT, text: &[u16]) -> SIZE {
    // SAFETY: `text` is NUL-terminated; the length passed excludes the NUL.
    unsafe {
        let previous = SelectObject(dc, font);
        let mut size = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32 - 1, &mut size);
        SelectObject(dc, previous);
        size
    }
}

unsafe fn paint(dc: HDC, toast: &Toast) {
    let layout = toast.layout;
    // SAFETY: drawing into the DC handed to WM_PAINT; brushes are deleted
    // and selections restored before returning.
    unsafe {
        let fill = |rect: RECT, color: COLORREF| {
            let brush = CreateSolidBrush(color);
            FillRect(dc, &rect, brush);
            DeleteObject(brush);
        };
        let full = RECT {
            left: 0,
            top: 0,
            right: layout.width,
            bottom: layout.height,
        };
        fill(full, BACKGROUND);
        let border = CreateSolidBrush(BORDER);
        FrameRect(dc, &full, border);
        DeleteObject(border);
        fill(
            RECT {
                left: 0,
                top: 0,
                right: layout.accent_width,
                bottom: layout.height,
            },
            toast.kind.accent(),
        );

        SetBkMode(dc, TRANSPARENT as _);
        let previous = SelectObject(dc, toast.title_font);
        SetTextColor(dc, TEXT);
        TextOutW(
            dc,
            layout.text_x,
            layout.title_y,
            toast.title.as_ptr(),
            toast.title.len() as i32 - 1,
        );
        SelectObject(dc, toast.detail_font);
        SetTextColor(dc, SUBTLE);
        TextOutW(
            dc,
            layout.text_x,
            layout.detail_y,
            toast.detail.as_ptr(),
            toast.detail.len() as i32 - 1,
        );
        SelectObject(dc, previous);
    }
}

unsafe extern "system" fn toast_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            // SAFETY: standard BeginPaint/EndPaint pairing on our own window.
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                if let Some(toast) = TOAST.get() {
                    paint(dc, toast);
                }
                EndPaint(hwnd, &ps);
            }
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_TIMER | WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            dismiss();
            0
        }
        // SAFETY: forwarding unhandled messages unchanged.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview;

    /// Writes pictures of the toast when SAYDROP_PREVIEW_DIR is set.
    #[test]
    fn preview_pictures() {
        let Some(dir) = preview::dir() else { return };
        let samples = [
            (
                "ok",
                Kind::Ok,
                "Pasted",
                "He hoped there would be stew for dinner, turnips and carrots, and\u{2026}",
            ),
            (
                "warning",
                Kind::Warning,
                "Pasted",
                "Not cleaned up: Could not connect to the server (connect, llm.example)",
            ),
            (
                "error",
                Kind::Error,
                "Dictation failed",
                "Speech server: Could not connect to the server (connect, 127.0.0.1)",
            ),
            ("short", Kind::Warning, "Too short", "Nothing was sent."),
        ];
        for dpi in [96, 144] {
            for (name, kind, title, detail) in samples {
                let message = Message {
                    kind,
                    title,
                    detail,
                    anchor: POINT { x: 0, y: 0 },
                };
                let toast = preview::with_measure_dc(|dc| build(dc, dpi, &message));
                let (width, height) = (toast.layout.width, toast.layout.height);
                // SAFETY: painting into the memory DC the helper provides.
                let pixels = preview::render(width, height, |dc| unsafe { paint(dc, &toast) });
                preview::save(&dir, &format!("toast-{name}-{dpi}"), width, height, &pixels);
            }
        }
    }
}
