//! Tray icons: a snail, rabbit and leopard per mode (Fluent Emoji by
//! Microsoft, MIT licensed), and a grey dot when the mode is unknown.

use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, LR_DEFAULTCOLOR,
    SM_CXSMICON,
};

use crate::victus::Mode;

const QUIET: &[u8] = include_bytes!("../assets/quiet.ico");
const DEFAULT: &[u8] = include_bytes!("../assets/default.ico");
const PERFORMANCE: &[u8] = include_bytes!("../assets/performance.ico");
const UNKNOWN_COLOR: [u8; 3] = [0x8A, 0x8A, 0x8A];

pub struct Icons {
    quiet: HICON,
    default: HICON,
    performance: HICON,
    unknown: HICON,
}

impl Icons {
    pub fn load() -> windows::core::Result<Self> {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        Ok(Self {
            quiet: from_ico(QUIET, size)?,
            default: from_ico(DEFAULT, size)?,
            performance: from_ico(PERFORMANCE, size)?,
            unknown: dot(size, UNKNOWN_COLOR)?,
        })
    }

    pub fn for_mode(&self, mode: Option<Mode>) -> HICON {
        match mode {
            Some(Mode::Quiet) => self.quiet,
            Some(Mode::Default) => self.default,
            Some(Mode::Performance) => self.performance,
            None => self.unknown,
        }
    }
}

fn from_ico(ico: &[u8], size: i32) -> windows::core::Result<HICON> {
    let image = best_image(ico, size as u32).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
    unsafe { CreateIconFromResourceEx(image, true, 0x0003_0000, size, size, LR_DEFAULTCOLOR) }
}

/// The smallest image in an .ico file that is at least `size` wide, or the
/// largest one if all are smaller. Downscaling looks better than upscaling.
fn best_image(ico: &[u8], size: u32) -> Option<&[u8]> {
    let count = u16::from_le_bytes(ico.get(4..6)?.try_into().ok()?) as usize;
    (0..count)
        .filter_map(|i| {
            let entry = ico.get(6 + 16 * i..6 + 16 * (i + 1))?;
            let width = if entry[0] == 0 { 256 } else { u32::from(entry[0]) };
            let len = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
            Some((width, ico.get(offset..offset.checked_add(len)?)?))
        })
        .min_by_key(|&(width, _)| if width >= size { (0, width) } else { (1, u32::MAX - width) })
        .map(|(_, image)| image)
}

fn dot(size: i32, color: [u8; 3]) -> windows::core::Result<HICON> {
    let side = size as usize;
    let pixels = circle(side, color);
    // 1-bpp rows are padded to 16 bits; all zeros = draw the color bitmap everywhere.
    let mask_bits = vec![0u8; side.div_ceil(16) * 2 * side];
    unsafe {
        let color = CreateBitmap(size, size, 1, 32, Some(pixels.as_ptr().cast()));
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr().cast()));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// Anti-aliased filled circle, a third of the icon wide, as top-down BGRA pixels (straight alpha).
fn circle(size: usize, [r, g, b]: [u8; 3]) -> Vec<u8> {
    let center = size as f32 / 2.0;
    let radius = size as f32 / 3.0;
    let mut out = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let coverage = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            out.extend_from_slice(&[b, g, r, (coverage * 255.0).round() as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn width_of(png: &[u8]) -> u32 {
        u32::from_be_bytes(png[16..20].try_into().unwrap())
    }

    #[test]
    fn picks_the_smallest_image_that_is_big_enough() {
        for ico in [QUIET, DEFAULT, PERFORMANCE] {
            for (want, got) in [(16, 16), (20, 20), (22, 24), (30, 32), (48, 48), (100, 64)] {
                let png = best_image(ico, want).unwrap();
                assert!(png.starts_with(PNG_SIGNATURE));
                assert_eq!(width_of(png), got, "asked for {want}");
            }
        }
    }

    #[test]
    fn rejects_truncated_files() {
        assert!(best_image(&QUIET[..40], 16).is_none());
        assert!(best_image(&[], 16).is_none());
    }

    #[test]
    fn circle_is_opaque_in_the_middle_and_clear_in_the_corners() {
        let size = 16;
        let px = circle(size, [1, 2, 3]);
        let alpha = |x: usize, y: usize| px[(y * size + x) * 4 + 3];
        assert_eq!(px.len(), size * size * 4);
        assert_eq!(alpha(8, 8), 255);
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(15, 15), 0);
        assert_eq!(&px[(8 * size + 8) * 4..][..3], &[3, 2, 1]);
    }
}
