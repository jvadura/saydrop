//! The small bar shown while dictating: a level meter, the elapsed time, a
//! Stop button and a Cancel button; then a progress note while the text is
//! being produced.
//!
//! Like the toast it never takes focus. That matters more here: clicking
//! Stop must leave the keyboard focus in the window the text is pasted into.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, Ellipse, EndPaint, FillRect, FrameRect, GetDC, GetMonitorInfoW,
    GetStockObject, HDC, HFONT, InvalidateRect, LineTo, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, MoveToEx, NULL_PEN, PAINTSTRUCT, PS_SOLID, ReleaseDC, RoundRect, SRCCOPY,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IDC_ARROW, KillTimer, LoadCursorW,
    MA_NOACTIVATE, PostMessageW, RegisterClassW, SW_SHOWNOACTIVATE, SetTimer, ShowWindow, WM_APP,
    WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_PAINT, WM_TIMER, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::toast::{
    BACKGROUND, BORDER, ERROR_ACCENT, SUBTLE, TEXT, WARN_ACCENT, font, measure, rgb, round_corners,
};
use crate::util::{UiCell, tick, wide};

/// Posted to the owner window when the Stop button is clicked.
pub const WM_OVERLAY_STOP: u32 = WM_APP + 20;
/// Posted to the owner window when the Cancel button is clicked.
pub const WM_OVERLAY_CANCEL: u32 = WM_APP + 21;

const CLASS_NAME: &str = "saydrop.overlay";
/// Sent after `TrackMouseEvent` when the pointer leaves the window.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_ID: usize = 1;
const FRAME_MS: u32 = 50;
const BARS: usize = 24;
/// The countdown replaces the clock this many seconds before the limit.
const WARN_SECONDS: u64 = 30;

const BUTTON: COLORREF = rgb(0x33, 0x33, 0x33);
const BUTTON_HOVER: COLORREF = rgb(0x48, 0x48, 0x48);
const BAR: COLORREF = rgb(0xc8, 0xc8, 0xc8);
const BAR_QUIET: COLORREF = rgb(0x5a, 0x5a, 0x5a);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Recording,
    Transcribing,
    Cleaning,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hover {
    None,
    Stop,
    Cancel,
}

struct Overlay {
    hwnd: HWND,
    owner: HWND,
    phase: Phase,
    started: u64,
    max_seconds: u32,
    /// Recent input levels, oldest first, each 0.0 to 1.0.
    levels: [f32; BARS],
    font: HFONT,
    dpi: i32,
    width: i32,
    height: i32,
    stop: RECT,
    cancel: RECT,
    hover: Hover,
    tracking: bool,
}

impl Overlay {
    fn scale(&self, value: i32) -> i32 {
        value * self.dpi / 96
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: the font was created in `show` and is released once.
        unsafe { DeleteObject(self.font) };
    }
}

static OVERLAY: UiCell<Overlay> = UiCell::new();

pub fn register_class(instance: *mut c_void) -> bool {
    let name = wide(CLASS_NAME);
    // SAFETY: the class struct is fully initialised and `name` outlives the call.
    unsafe {
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(overlay_proc);
        class.hInstance = instance;
        class.lpszClassName = name.as_ptr();
        // Without a class cursor the pointer keeps whatever shape it had.
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        RegisterClassW(&class) != 0
    }
}

/// Lay the bar out for a display of `dpi`, measuring text with `dc`.
fn build(dc: HDC, dpi: i32, owner: HWND, phase: Phase, max_seconds: u32) -> Overlay {
    let scale = |value: i32| value * dpi / 96;

    let text_font = font(scale(14), 600);
    let clock_width = measure(dc, text_font, &wide("0:00 left")).cx;
    let stop_text_width = measure(dc, text_font, &wide("Stop")).cx;

    let padding = scale(14);
    let height = scale(46);
    let button_height = scale(28);
    let button_top = (height - button_height) / 2;
    let meter_x = padding + scale(10) + scale(10);
    let meter_width = BARS as i32 * scale(5) - scale(2);
    let clock_x = meter_x + meter_width + scale(12);
    let stop_x = clock_x + clock_width + scale(12);
    let stop_width = scale(12) + scale(9) + scale(7) + stop_text_width + scale(12);
    let cancel_x = stop_x + stop_width + scale(6);
    let width = cancel_x + button_height + scale(9);

    Overlay {
        hwnd: null_mut(),
        owner,
        phase,
        started: tick(),
        max_seconds,
        levels: [0.0; BARS],
        font: text_font,
        dpi,
        width,
        height,
        stop: RECT {
            left: stop_x,
            top: button_top,
            right: stop_x + stop_width,
            bottom: button_top + button_height,
        },
        cancel: RECT {
            left: cancel_x,
            top: button_top,
            right: cancel_x + button_height,
            bottom: button_top + button_height,
        },
        hover: Hover::None,
        tracking: false,
    }
}

/// Show the bar in `phase` on the monitor containing `anchor`. Clicks on its
/// buttons are posted to `owner`. `max_seconds` of 0 means no limit.
pub fn show(instance: *mut c_void, owner: HWND, anchor: POINT, phase: Phase, max_seconds: u32) {
    hide();

    // SAFETY: Win32/GDI calls with valid arguments; the font ends up owned
    // by the Overlay and is freed in its Drop.
    unsafe {
        let monitor = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
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
        let overlay = build(screen, dpi_x as i32, owner, phase, max_seconds);
        ReleaseDC(null_mut(), screen);
        let (width, height) = (overlay.width, overlay.height);

        let work = info.rcWork;
        let x = work.left + ((work.right - work.left) - width) / 2;
        let y = work.bottom - height - overlay.scale(56);
        OVERLAY.set(overlay);

        let class = wide(CLASS_NAME);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            null(),
            WS_POPUP,
            x,
            y,
            width,
            height,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            OVERLAY.take();
            return;
        }
        if let Some(overlay) = OVERLAY.get() {
            overlay.hwnd = hwnd;
        }
        round_corners(hwnd);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, TIMER_ID, FRAME_MS, None);
    }
}

pub fn is_visible() -> bool {
    OVERLAY.get().is_some()
}

pub fn set_phase(phase: Phase) {
    if let Some(overlay) = OVERLAY.get() {
        overlay.phase = phase;
        overlay.hover = Hover::None;
        // SAFETY: our own window.
        unsafe { InvalidateRect(overlay.hwnd, null(), 0) };
    }
}

/// Feed the meter with the peak level (0.0 to 1.0) of the newest audio.
pub fn push_level(peak: f32) {
    if let Some(overlay) = OVERLAY.get() {
        overlay.levels.copy_within(1.., 0);
        overlay.levels[BARS - 1] = meter_level(peak);
    }
}

/// Map a linear peak to bar height: -50 dB and below is empty, 0 dB is full.
/// A linear meter would hardly move for normal speech.
fn meter_level(peak: f32) -> f32 {
    if peak <= 0.0 {
        return 0.0;
    }
    ((20.0 * peak.log10() + 50.0) / 50.0).clamp(0.0, 1.0)
}

pub fn hide() {
    let Some(overlay) = OVERLAY.take() else {
        return;
    };
    // SAFETY: our own window; state is already taken so the messages
    // DestroyWindow sends find nothing to act on.
    unsafe {
        KillTimer(overlay.hwnd, TIMER_ID);
        DestroyWindow(overlay.hwnd);
    }
}

fn clock_text(elapsed_seconds: u64, max_seconds: u32) -> (String, bool) {
    let format = |seconds: u64| format!("{}:{:02}", seconds / 60, seconds % 60);
    let max = max_seconds as u64;
    if max > 0 && elapsed_seconds + WARN_SECONDS >= max {
        let left = max.saturating_sub(elapsed_seconds);
        (format!("{} left", format(left)), true)
    } else {
        (format(elapsed_seconds), false)
    }
}

fn contains(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// Blend two colours; `amount` 0.0 gives `from`, 1.0 gives `to`.
fn mix(from: COLORREF, to: COLORREF, amount: f32) -> COLORREF {
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xFF) as f32;
        let b = ((to >> shift) & 0xFF) as f32;
        ((a + (b - a) * amount.clamp(0.0, 1.0)) as u32) << shift
    };
    channel(0) | channel(8) | channel(16)
}

unsafe fn paint(dc: HDC, overlay: &Overlay) {
    let (width, height) = (overlay.width, overlay.height);
    // SAFETY: drawing into a DC we own for the duration; every GDI object
    // created is deleted and selections are restored before returning.
    unsafe {
        let fill = |rect: RECT, color: COLORREF| {
            let brush = CreateSolidBrush(color);
            FillRect(dc, &rect, brush);
            DeleteObject(brush);
        };
        let rounded = |rect: &RECT, color: COLORREF, radius: i32| {
            let brush = CreateSolidBrush(color);
            let old_brush = SelectObject(dc, brush);
            let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
            RoundRect(
                dc,
                rect.left,
                rect.top,
                rect.right + 1,
                rect.bottom + 1,
                radius,
                radius,
            );
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            DeleteObject(brush);
        };
        let disc = |cx: i32, cy: i32, radius: i32, color: COLORREF| {
            let brush = CreateSolidBrush(color);
            let old_brush = SelectObject(dc, brush);
            let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
            Ellipse(
                dc,
                cx - radius,
                cy - radius,
                cx + radius + 1,
                cy + radius + 1,
            );
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            DeleteObject(brush);
        };
        let text = |x: i32, y: i32, value: &str, color: COLORREF| {
            let value = wide(value);
            SetTextColor(dc, color);
            TextOutW(dc, x, y, value.as_ptr(), value.len() as i32 - 1);
        };

        let full = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        fill(full, BACKGROUND);
        let border = CreateSolidBrush(BORDER);
        FrameRect(dc, &full, border);
        DeleteObject(border);

        SetBkMode(dc, TRANSPARENT as _);
        let old_font = SelectObject(dc, overlay.font);
        let line_height = measure(dc, overlay.font, &wide("Ag")).cy;
        let text_y = (height - line_height) / 2;
        let padding = overlay.scale(14);
        let middle = height / 2;
        let now = tick();

        match overlay.phase {
            Phase::Recording => {
                // A red dot that breathes, so "recording" is unmistakable.
                let beat = ((now % 1400) as f32 / 1400.0 * std::f32::consts::TAU).cos();
                let dot = mix(BACKGROUND, ERROR_ACCENT, 0.65 + 0.35 * beat);
                disc(padding + overlay.scale(5), middle, overlay.scale(5), dot);

                let meter_x = padding + overlay.scale(20);
                let pitch = overlay.scale(5);
                let bar_width = overlay.scale(3);
                let tallest = overlay.scale(26);
                for (index, level) in overlay.levels.iter().enumerate() {
                    let bar_height = ((tallest as f32 * level) as i32).max(overlay.scale(2));
                    let left = meter_x + index as i32 * pitch;
                    fill(
                        RECT {
                            left,
                            top: middle - bar_height / 2,
                            right: left + bar_width,
                            bottom: middle - bar_height / 2 + bar_height,
                        },
                        if *level > 0.02 { BAR } else { BAR_QUIET },
                    );
                }

                let elapsed = now.saturating_sub(overlay.started) / 1000;
                let (clock, warn) = clock_text(elapsed, overlay.max_seconds);
                let clock_x = meter_x + BARS as i32 * pitch - overlay.scale(2) + overlay.scale(12);
                text(
                    clock_x,
                    text_y,
                    &clock,
                    if warn { WARN_ACCENT } else { TEXT },
                );

                let stop = overlay.stop;
                let hovered = overlay.hover == Hover::Stop;
                rounded(
                    &stop,
                    if hovered { BUTTON_HOVER } else { BUTTON },
                    overlay.scale(8),
                );
                let square = overlay.scale(9);
                let square_x = stop.left + overlay.scale(12);
                fill(
                    RECT {
                        left: square_x,
                        top: middle - square / 2,
                        right: square_x + square,
                        bottom: middle - square / 2 + square,
                    },
                    ERROR_ACCENT,
                );
                text(square_x + square + overlay.scale(7), text_y, "Stop", TEXT);
            }
            Phase::Transcribing | Phase::Cleaning => {
                // Three dots taking turns: work is in progress.
                let active = (now / 280) % 3;
                for index in 0..3 {
                    let color = if index == active { TEXT } else { BAR_QUIET };
                    disc(
                        padding + overlay.scale(4) + index as i32 * overlay.scale(11),
                        middle,
                        overlay.scale(3),
                        color,
                    );
                }
                let label = if overlay.phase == Phase::Transcribing {
                    "Transcribing\u{2026}"
                } else {
                    "Cleaning up\u{2026}"
                };
                text(padding + overlay.scale(42), text_y, label, TEXT);
            }
        }

        // The cancel button: a cross.
        let cancel = overlay.cancel;
        if overlay.hover == Hover::Cancel {
            rounded(&cancel, BUTTON_HOVER, overlay.scale(8));
        }
        let pen = CreatePen(PS_SOLID, overlay.scale(2).max(1), SUBTLE);
        let old_pen = SelectObject(dc, pen);
        let cx = (cancel.left + cancel.right) / 2;
        let arm = overlay.scale(5);
        MoveToEx(dc, cx - arm, middle - arm, null_mut());
        LineTo(dc, cx + arm + 1, middle + arm + 1);
        MoveToEx(dc, cx + arm, middle - arm, null_mut());
        LineTo(dc, cx - arm - 1, middle + arm + 1);
        SelectObject(dc, old_pen);
        DeleteObject(pen);

        SelectObject(dc, old_font);
    }
}

/// Paint through an off-screen bitmap so the animation does not flicker.
unsafe fn paint_buffered(window_dc: HDC, overlay: &Overlay) {
    // SAFETY: the memory DC and bitmap are created, used and released here.
    unsafe {
        let memory = CreateCompatibleDC(window_dc);
        let bitmap = CreateCompatibleBitmap(window_dc, overlay.width, overlay.height);
        if memory.is_null() || bitmap.is_null() {
            paint(window_dc, overlay);
        } else {
            let previous = SelectObject(memory, bitmap);
            paint(memory, overlay);
            BitBlt(
                window_dc,
                0,
                0,
                overlay.width,
                overlay.height,
                memory,
                0,
                0,
                SRCCOPY,
            );
            SelectObject(memory, previous);
        }
        if !bitmap.is_null() {
            DeleteObject(bitmap);
        }
        if !memory.is_null() {
            DeleteDC(memory);
        }
    }
}

fn hover_at(overlay: &Overlay, x: i32, y: i32) -> Hover {
    if contains(&overlay.cancel, x, y) {
        Hover::Cancel
    } else if overlay.phase == Phase::Recording && contains(&overlay.stop, x, y) {
        Hover::Stop
    } else {
        Hover::None
    }
}

fn point(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam & 0xFFFF) as i16 as i32,
        ((lparam >> 16) & 0xFFFF) as i16 as i32,
    )
}

unsafe extern "system" fn overlay_proc(
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
                if let Some(overlay) = OVERLAY.get() {
                    paint_buffered(dc, overlay);
                }
                EndPaint(hwnd, &ps);
            }
            0
        }
        WM_TIMER => {
            // SAFETY: our own window.
            unsafe { InvalidateRect(hwnd, null(), 0) };
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_MOUSEMOVE => {
            if let Some(overlay) = OVERLAY.get() {
                let (x, y) = point(lparam);
                overlay.hover = hover_at(overlay, x, y);
                if !overlay.tracking {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: a fully initialised structure for our window.
                    overlay.tracking = unsafe { TrackMouseEvent(&mut track) } != 0;
                }
            }
            0
        }
        WM_MOUSELEAVE => {
            if let Some(overlay) = OVERLAY.get() {
                overlay.hover = Hover::None;
                overlay.tracking = false;
            }
            0
        }
        WM_LBUTTONUP => {
            if let Some(overlay) = OVERLAY.get() {
                let (x, y) = point(lparam);
                let owner = overlay.owner;
                let command = match hover_at(overlay, x, y) {
                    Hover::Stop => Some(WM_OVERLAY_STOP),
                    Hover::Cancel => Some(WM_OVERLAY_CANCEL),
                    Hover::None => None,
                };
                if let Some(command) = command {
                    // Posted, not sent: the owner may destroy this window.
                    // SAFETY: the owner is the main window of this program.
                    unsafe { PostMessageW(owner, command, 0, 0) };
                }
            }
            0
        }
        // SAFETY: forwarding unhandled messages unchanged.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_counts_up_then_down() {
        assert_eq!(clock_text(0, 240), ("0:00".to_string(), false));
        assert_eq!(clock_text(75, 240), ("1:15".to_string(), false));
        assert_eq!(clock_text(209, 240), ("3:29".to_string(), false));
        assert_eq!(clock_text(210, 240), ("0:30 left".to_string(), true));
        assert_eq!(clock_text(239, 240), ("0:01 left".to_string(), true));
        assert_eq!(clock_text(999, 240), ("0:00 left".to_string(), true));
        // No limit: never a countdown.
        assert_eq!(clock_text(3600, 0), ("60:00".to_string(), false));
    }

    #[test]
    fn meter_is_logarithmic() {
        assert_eq!(meter_level(0.0), 0.0);
        assert_eq!(meter_level(1.0), 1.0);
        assert_eq!(meter_level(0.001), 0.0);
        let speech = meter_level(0.1);
        assert!((0.55..0.65).contains(&speech), "{speech}");
    }

    /// Writes pictures of the bar when SAYDROP_PREVIEW_DIR is set.
    #[test]
    fn preview_pictures() {
        use crate::preview;
        let Some(dir) = preview::dir() else { return };
        let samples = [
            ("recording", Phase::Recording, 7, Hover::None),
            ("recording-near-limit", Phase::Recording, 215, Hover::Stop),
            ("transcribing", Phase::Transcribing, 0, Hover::None),
            ("cleaning", Phase::Cleaning, 0, Hover::Cancel),
        ];
        for dpi in [96, 144] {
            for (name, phase, elapsed, hover) in samples {
                let mut overlay =
                    preview::with_measure_dc(|dc| build(dc, dpi, null_mut(), phase, 240));
                overlay.started = tick().saturating_sub(elapsed * 1000);
                overlay.hover = hover;
                for (index, level) in overlay.levels.iter_mut().enumerate() {
                    // Something speech-like: bursts with pauses.
                    let wave = ((index as f32 * 0.9).sin() * (index as f32 * 0.31).cos()).abs();
                    *level = if index % 9 == 4 {
                        0.0
                    } else {
                        0.25 + 0.7 * wave
                    };
                }
                let (width, height) = (overlay.width, overlay.height);
                // SAFETY: painting into the memory DC the helper provides.
                let pixels = preview::render(width, height, |dc| unsafe { paint(dc, &overlay) });
                preview::save(&dir, &format!("bar-{name}-{dpi}"), width, height, &pixels);
            }
        }
    }

    #[test]
    fn colours_mix_per_channel() {
        assert_eq!(mix(rgb(0, 0, 0), rgb(200, 100, 50), 0.5), rgb(100, 50, 25));
        assert_eq!(
            mix(rgb(10, 20, 30), rgb(200, 100, 50), 0.0),
            rgb(10, 20, 30)
        );
        assert_eq!(
            mix(rgb(10, 20, 30), rgb(200, 100, 50), 2.0),
            rgb(200, 100, 50)
        );
    }
}
