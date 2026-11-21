//! Test support: draw a piece of the UI into a PNG file so a person can look
//! at it without running the app.
//!
//!     set SAYDROP_PREVIEW_DIR=target\preview
//!     cargo test preview
//!
//! Without the variable the preview tests do nothing.

use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GdiFlush, HDC, SelectObject,
};

/// Where to write previews, if they were asked for.
pub fn dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("SAYDROP_PREVIEW_DIR").filter(|d| !d.is_empty())?);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Run `measure` with a device context that can measure text.
pub fn with_measure_dc<T>(measure: impl FnOnce(HDC) -> T) -> T {
    // SAFETY: a memory DC, created and deleted here.
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let value = measure(dc);
        DeleteDC(dc);
        value
    }
}

/// Let `draw` paint into a `width` x `height` bitmap; returns RGBA pixels.
pub fn render(width: i32, height: i32, draw: impl FnOnce(HDC)) -> Vec<u8> {
    // SAFETY: the DC and the bitmap are created, used and deleted here; the
    // pixel slice is read while the bitmap is still alive.
    unsafe {
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        let dc = CreateCompatibleDC(null_mut());
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        assert!(!dc.is_null() && !bitmap.is_null() && !bits.is_null());
        let previous = SelectObject(dc, bitmap);
        draw(dc);
        GdiFlush();
        let bgra = std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize);
        let rgba = bgra
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect();
        SelectObject(dc, previous);
        DeleteObject(bitmap);
        DeleteDC(dc);
        rgba
    }
}

pub fn save(dir: &Path, name: &str, width: i32, height: i32, rgba: &[u8]) {
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).unwrap();
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(rgba).unwrap();
    writer.finish().unwrap();
}
