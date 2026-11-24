use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, SM_CXSMICON,
};

const COLOR: [u8; 3] = [0x2F, 0x80, 0xED];

pub fn create() -> windows::core::Result<HICON> {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let side = size as usize;
    let pixels = circle(side, COLOR);
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

/// Anti-aliased filled circle as top-down BGRA pixels (straight alpha).
fn circle(size: usize, [r, g, b]: [u8; 3]) -> Vec<u8> {
    let center = size as f32 / 2.0;
    let radius = center - 1.0;
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
