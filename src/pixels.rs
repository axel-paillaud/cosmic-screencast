//! Crops raw frames and converts them to RGBA for the GIF encoder.

use imgref::ImgVec;
use pipewire::spa::param::video::VideoFormat;
use rgb::RGBA8;

use crate::region::Region;

/// Byte order of a 32-bit pixel in the raw frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelOrder {
    /// Blue, green, red, then alpha or padding.
    Bgr,
    /// Red, green, blue, then alpha or padding.
    Rgb,
}

impl PixelOrder {
    /// `None` for formats we don't support (we only negotiate these four).
    pub fn from_format(format: VideoFormat) -> Option<PixelOrder> {
        match format {
            VideoFormat::BGRx | VideoFormat::BGRA => Some(PixelOrder::Bgr),
            VideoFormat::RGBx | VideoFormat::RGBA => Some(PixelOrder::Rgb),
            _ => None,
        }
    }
}

/// Copies `rect` (physical pixels) out of a raw frame, as RGBA.
///
/// `stride` is the real length of a row in bytes, sometimes larger than
/// width × 4. Returns `None` if `bytes` is too short for `rect`.
pub fn crop_to_rgba(
    bytes: &[u8],
    stride: usize,
    order: PixelOrder,
    rect: Region,
) -> Option<ImgVec<RGBA8>> {
    let (x, y, w, h) = (
        rect.x as usize,
        rect.y as usize,
        rect.width as usize,
        rect.height as usize,
    );
    let mut pixels = Vec::with_capacity(w * h);
    // Each row is first copied in one go: reading GPU memory (DMA-BUF) byte
    // by byte is very slow, a single big copy is much faster.
    let mut line = vec![0u8; w * 4];
    for row in y..y + h {
        let start = row * stride + x * 4;
        line.copy_from_slice(bytes.get(start..start + w * 4)?);
        for px in line.chunks_exact(4) {
            let (r, g, b) = match order {
                PixelOrder::Bgr => (px[2], px[1], px[0]),
                PixelOrder::Rgb => (px[0], px[1], px[2]),
            };
            // "x" formats have no reliable alpha: force full opacity.
            pixels.push(RGBA8::new(r, g, b, 255));
        }
    }
    Some(ImgVec::new(pixels, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3x2 BGRx frame with 4 bytes of padding per row (stride = 16).
    /// Pixel (col, row) has blue = col, green = row, red = 100.
    fn frame() -> Vec<u8> {
        let mut bytes = Vec::new();
        for row in 0..2u8 {
            for col in 0..3u8 {
                bytes.extend([col, row, 100, 0]);
            }
            bytes.extend([0xEE; 4]); // padding
        }
        bytes
    }

    #[test]
    fn crops_and_converts_bgr() {
        let rect = Region {
            x: 1,
            y: 0,
            width: 2,
            height: 2,
        };
        let image = crop_to_rgba(&frame(), 16, PixelOrder::Bgr, rect).unwrap();
        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(
            image.buf(),
            &[
                RGBA8::new(100, 0, 1, 255),
                RGBA8::new(100, 0, 2, 255),
                RGBA8::new(100, 1, 1, 255),
                RGBA8::new(100, 1, 2, 255),
            ]
        );
    }

    #[test]
    fn rgb_order_keeps_bytes() {
        let rect = Region {
            x: 0,
            y: 1,
            width: 1,
            height: 1,
        };
        let image = crop_to_rgba(&frame(), 16, PixelOrder::Rgb, rect).unwrap();
        assert_eq!(image.buf(), &[RGBA8::new(0, 1, 100, 255)]);
    }

    #[test]
    fn too_short_buffer_is_rejected() {
        let rect = Region {
            x: 0,
            y: 0,
            width: 3,
            height: 3, // the frame only has 2 rows
        };
        assert!(crop_to_rgba(&frame(), 16, PixelOrder::Bgr, rect).is_none());
    }
}
