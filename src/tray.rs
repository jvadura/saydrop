//! The notification-area icon, its balloon messages and the autostart switch.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, SM_CXSMICON,
};

use crate::util::{APP_NAME, wide};

const ICON_ID: u32 = 1;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// What the icon shows; the tile colour changes with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Recording,
    Working,
}

impl Status {
    /// Tile colour as (red, green, blue).
    fn colour(self) -> (u32, u32, u32) {
        match self {
            Status::Idle => (0x6D, 0x4A, 0xE8),
            Status::Recording => (0xDC, 0x26, 0x26),
            Status::Working => (0xD9, 0x77, 0x06),
        }
    }
}

fn copy_into(target: &mut [u16], text: &str) {
    let limit = target.len() - 1;
    let mut written = 0;
    for unit in text.encode_utf16().take(limit) {
        target[written] = unit;
        written += 1;
    }
    target[written] = 0;
}

fn base(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: NOTIFYICONDATAW is plain data; zero is a valid starting state.
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = ICON_ID;
    data
}

/// Add the icon. Also called again whenever Explorer restarts.
pub fn add(hwnd: HWND, icon: HICON, callback_message: u32, tip: &str) -> bool {
    let mut data = base(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = callback_message;
    data.hIcon = icon;
    copy_into(&mut data.szTip, tip);
    // SAFETY: `data` is fully initialised for the flags set.
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) != 0 }
}

pub fn set_tip(hwnd: HWND, tip: &str) {
    let mut data = base(hwnd);
    data.uFlags = NIF_TIP;
    copy_into(&mut data.szTip, tip);
    // SAFETY: as in `add`.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
}

pub fn set_icon(hwnd: HWND, icon: HICON) {
    let mut data = base(hwnd);
    data.uFlags = NIF_ICON;
    data.hIcon = icon;
    // SAFETY: as in `add`.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
}

pub fn remove(hwnd: HWND) {
    let data = base(hwnd);
    // SAFETY: as in `add`.
    unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

/// Show a notification from the tray icon.
pub fn balloon(hwnd: HWND, title: &str, text: &str, warning: bool) {
    let mut data = base(hwnd);
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
    copy_into(&mut data.szInfoTitle, title);
    copy_into(&mut data.szInfo, text);
    // SAFETY: as in `add`.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
}

/// Draw the tray icon at the system's small-icon size: a coloured tile with
/// a white microphone. Generated at runtime so the binary needs no resources.
pub fn create_icon(status: Status) -> HICON {
    // SAFETY: GDI objects are created, handed to CreateIconIndirect (which
    // copies them) and deleted here.
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON).clamp(16, 64);

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = size;
        info.bmiHeader.biHeight = -size;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        let color = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if color.is_null() || bits.is_null() {
            return null_mut();
        }
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u8, (size * size * 4) as usize);
        render_icon(pixels, size as usize, status);

        let mask = CreateBitmap(size, size, 1, 1, null());
        let icon_info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&icon_info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

/// Fill `pixels` (BGRA, top-down, `size` x `size`) with the icon artwork.
fn render_icon(pixels: &mut [u8], size: usize, status: Status) {
    const SAMPLES: usize = 4;
    let (red, green, blue) = status.colour();
    for y in 0..size {
        for x in 0..size {
            let (mut tile, mut mark) = (0u32, 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let u = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    if in_tile(u, v) {
                        tile += 1;
                        if in_microphone(u, v) {
                            mark += 1;
                        }
                    }
                }
            }
            let total = (SAMPLES * SAMPLES) as u32;
            // Mix the tile colour with white by how much of the pixel the
            // microphone covers; fully transparent pixels stay black.
            let blend = |tile_channel: u32| {
                (tile_channel * (tile - mark) + 255 * mark)
                    .checked_div(tile)
                    .unwrap_or(0) as u8
            };
            let offset = (y * size + x) * 4;
            pixels[offset] = blend(blue);
            pixels[offset + 1] = blend(green);
            pixels[offset + 2] = blend(red);
            pixels[offset + 3] = (255 * tile / total) as u8;
        }
    }
}

/// Rounded square covering most of the unit square.
fn in_tile(u: f32, v: f32) -> bool {
    const INSET: f32 = 0.03;
    const RADIUS: f32 = 0.22;
    let (lo, hi) = (INSET, 1.0 - INSET);
    if u < lo || u > hi || v < lo || v > hi {
        return false;
    }
    let cx = u.clamp(lo + RADIUS, hi - RADIUS);
    let cy = v.clamp(lo + RADIUS, hi - RADIUS);
    (u - cx).powi(2) + (v - cy).powi(2) <= RADIUS * RADIUS
}

/// A microphone: a capsule, the cradle around its lower half, a stem and a
/// foot.
fn in_microphone(u: f32, v: f32) -> bool {
    const CENTRE: f32 = 0.5;
    // Capsule: a vertical bar with round ends.
    const CAPSULE_RADIUS: f32 = 0.115;
    const CAPSULE_TOP: f32 = 0.30;
    const CAPSULE_BOTTOM: f32 = 0.50;
    // Cradle: the lower half of a ring around the capsule.
    const CRADLE_Y: f32 = 0.49;
    const CRADLE_INNER: f32 = 0.185;
    const CRADLE_OUTER: f32 = 0.265;

    let dx = u - CENTRE;
    let nearest = v.clamp(CAPSULE_TOP, CAPSULE_BOTTOM);
    let capsule = dx * dx + (v - nearest).powi(2) <= CAPSULE_RADIUS * CAPSULE_RADIUS;

    let ring = (dx * dx + (v - CRADLE_Y).powi(2)).sqrt();
    let cradle = v >= CRADLE_Y && (CRADLE_INNER..=CRADLE_OUTER).contains(&ring);

    let stem = dx.abs() <= 0.04 && (0.74..=0.84).contains(&v);
    let foot = dx.abs() <= 0.15 && (0.80..=0.87).contains(&v);
    capsule || cradle || stem || foot
}

fn open_run_key(access: u32) -> Option<HKEY> {
    let key_name = wide(RUN_KEY);
    let mut key: HKEY = null_mut();
    // SAFETY: valid out-pointer; the key is closed by every caller.
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key_name.as_ptr(), 0, access, &mut key) };
    (status == 0).then_some(key)
}

/// Is the app registered to start when the user signs in?
pub fn autostart_enabled() -> bool {
    let Some(key) = open_run_key(KEY_QUERY_VALUE) else {
        return false;
    };
    let name = wide(APP_NAME);
    // SAFETY: querying existence only, so no buffers are passed.
    unsafe {
        let status = RegQueryValueExW(
            key,
            name.as_ptr(),
            null(),
            null_mut(),
            null_mut(),
            null_mut(),
        );
        RegCloseKey(key);
        status == 0
    }
}

/// Register or unregister the running executable under HKCU\...\Run.
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let key = open_run_key(KEY_SET_VALUE).ok_or("Cannot open the Run registry key")?;
    let name = wide(APP_NAME);
    // SAFETY: `command` is a NUL-terminated UTF-16 buffer whose byte length
    // is passed alongside it; the key is closed before returning.
    unsafe {
        let status = if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string());
            match exe {
                Ok(exe) => {
                    let command = wide(&format!("\"{}\"", exe.display()));
                    RegSetValueExW(
                        key,
                        name.as_ptr(),
                        0,
                        REG_SZ,
                        command.as_ptr().cast(),
                        (command.len() * 2) as u32,
                    )
                }
                Err(e) => {
                    RegCloseKey(key);
                    return Err(e);
                }
            }
        } else {
            RegDeleteValueW(key, name.as_ptr())
        };
        RegCloseKey(key);
        if status == 0 {
            Ok(())
        } else {
            Err(format!("Registry error {status}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sizes stored in `assets/saydrop.ico`, the icon embedded in the exe.
    const ICO_SIZES: [usize; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

    /// The .ico file for the artwork in `render_icon`, so the executable's
    /// icon and the tray icon come from one definition.
    fn ico_bytes() -> Vec<u8> {
        let images: Vec<(usize, Vec<u8>)> = ICO_SIZES
            .iter()
            .map(|&size| {
                let mut bgra = vec![0u8; size * size * 4];
                render_icon(&mut bgra, size, Status::Idle);
                let data = if size >= 256 {
                    ico_png_image(&bgra, size)
                } else {
                    ico_bmp_image(&bgra, size)
                };
                (size, data)
            })
            .collect();

        let mut out = Vec::new();
        out.extend(0u16.to_le_bytes()); // reserved
        out.extend(1u16.to_le_bytes()); // type: icon
        out.extend((images.len() as u16).to_le_bytes());
        let mut offset = 6 + 16 * images.len();
        for (size, data) in &images {
            // A dimension byte of 0 means 256.
            let dimension = if *size >= 256 { 0 } else { *size as u8 };
            out.extend([dimension, dimension, 0, 0]);
            out.extend(1u16.to_le_bytes()); // colour planes
            out.extend(32u16.to_le_bytes()); // bits per pixel
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((offset as u32).to_le_bytes());
            offset += data.len();
        }
        for (_, data) in &images {
            out.extend(data);
        }
        out
    }

    /// Classic icon image: a BITMAPINFOHEADER of double height, bottom-up
    /// BGRA rows, then an (all zero) 1-bit mask; alpha does the masking.
    fn ico_bmp_image(bgra: &[u8], size: usize) -> Vec<u8> {
        let mask_stride = size.div_ceil(32) * 4;
        let mut out = Vec::new();
        out.extend(40u32.to_le_bytes());
        out.extend((size as i32).to_le_bytes());
        out.extend((size as i32 * 2).to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(((size * size * 4 + mask_stride * size) as u32).to_le_bytes());
        out.extend([0u8; 16]);
        for row in bgra.chunks(size * 4).rev() {
            out.extend(row);
        }
        out.resize(out.len() + mask_stride * size, 0);
        out
    }

    /// Large icon images are stored as PNG to keep the file small.
    fn ico_png_image(bgra: &[u8], size: usize) -> Vec<u8> {
        let rgba: Vec<u8> = bgra
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, size as u32, size as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();
        writer.finish().unwrap();
        out
    }

    /// Regenerate with: set SAYDROP_WRITE_ICON=1, then `cargo test`.
    #[test]
    fn committed_exe_icon_matches_the_artwork() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/saydrop.ico");
        let expected = ico_bytes();
        if std::env::var_os("SAYDROP_WRITE_ICON").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &expected).unwrap();
        }
        let actual = std::fs::read(&path)
            .expect("assets/saydrop.ico is missing: run the tests once with SAYDROP_WRITE_ICON=1");
        assert!(
            actual == expected,
            "assets/saydrop.ico is out of date: run the tests once with SAYDROP_WRITE_ICON=1"
        );
    }

    #[test]
    fn icon_has_transparent_corners_and_a_white_microphone() {
        for size in [16usize, 20, 24, 32] {
            let mut pixels = vec![0u8; size * size * 4];
            render_icon(&mut pixels, size, Status::Idle);
            let at = |x: usize, y: usize| {
                let o = (y * size + x) * 4;
                (pixels[o], pixels[o + 1], pixels[o + 2], pixels[o + 3])
            };
            assert_eq!(at(0, 0).3, 0, "corner is transparent at {size}");
            // Just inside the left edge, halfway down: plain tile colour.
            assert_eq!(
                at(size / 8, size / 2),
                (0xE8, 0x4A, 0x6D, 255),
                "tile colour at {size}"
            );
            let white = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| **p == [255, 255, 255, 255])
                .count();
            assert!(
                white >= 8,
                "the microphone is visible at {size}: {white} white pixels"
            );
        }
    }

    #[test]
    fn each_status_has_its_own_colour() {
        let render = |status| {
            let mut pixels = vec![0u8; 16 * 16 * 4];
            render_icon(&mut pixels, 16, status);
            pixels
        };
        assert_ne!(render(Status::Idle), render(Status::Recording));
        assert_ne!(render(Status::Recording), render(Status::Working));
    }

    /// Writes pictures of the icon when SAYDROP_PREVIEW_DIR is set: every
    /// status at tray sizes (enlarged) and at 256 pixels, on a taskbar grey.
    #[test]
    fn preview_pictures() {
        use crate::preview;
        let Some(dir) = preview::dir() else { return };
        for (name, status) in [
            ("idle", Status::Idle),
            ("recording", Status::Recording),
            ("working", Status::Working),
        ] {
            for (size, zoom) in [(16usize, 8usize), (24, 6), (256, 1)] {
                let mut bgra = vec![0u8; size * size * 4];
                render_icon(&mut bgra, size, status);
                let side = size * zoom;
                let mut rgba = vec![0u8; side * side * 4];
                for y in 0..side {
                    for x in 0..side {
                        let source = ((y / zoom) * size + x / zoom) * 4;
                        let alpha = bgra[source + 3] as u32;
                        let over = |channel: u8| {
                            ((channel as u32 * alpha + 0x2b * (255 - alpha)) / 255) as u8
                        };
                        let target = (y * side + x) * 4;
                        rgba[target] = over(bgra[source + 2]);
                        rgba[target + 1] = over(bgra[source + 1]);
                        rgba[target + 2] = over(bgra[source]);
                        rgba[target + 3] = 255;
                    }
                }
                preview::save(
                    &dir,
                    &format!("icon-{name}-{size}"),
                    side as i32,
                    side as i32,
                    &rgba,
                );
            }
        }
    }

    #[test]
    fn tip_text_is_truncated_and_terminated() {
        let mut buffer = [0xFFFFu16; 8];
        copy_into(&mut buffer, "a very long tooltip");
        assert_eq!(buffer[7], 0);
        assert_eq!(String::from_utf16_lossy(&buffer[..7]), "a very ");
    }
}
