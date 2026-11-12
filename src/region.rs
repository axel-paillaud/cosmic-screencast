//! A rectangle on screen, and its conversion from logical to physical pixels.

/// A rectangle on screen. Logical (screen) coordinates when it comes from the
/// user, physical pixels once converted with `to_physical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// Converts a logical region to physical pixels of a `frame_size` frame,
    /// clamped so it fits inside the frame.
    ///
    /// `logical_size` is the screen size in logical pixels (from the portal).
    /// With HiDPI scaling (e.g. 200%), frames are bigger than the logical
    /// screen, so the region must be scaled up. Returns `None` for an empty frame.
    pub fn to_physical(
        self,
        logical_size: Option<(i32, i32)>,
        frame_size: (u32, u32),
    ) -> Option<Region> {
        let (frame_w, frame_h) = frame_size;
        if frame_w == 0 || frame_h == 0 {
            return None;
        }
        let scale = match logical_size {
            Some((logical_w, _)) if logical_w > 0 => frame_w as f64 / logical_w as f64,
            _ => 1.0,
        };
        let scaled = |v: u32| (v as f64 * scale) as u32;

        let x = scaled(self.x).min(frame_w - 1);
        let y = scaled(self.y).min(frame_h - 1);
        Some(Region {
            x,
            y,
            width: scaled(self.width).clamp(1, frame_w - x),
            height: scaled(self.height).clamp(1, frame_h - y),
        })
    }
}

// Unit tests: run with `cargo test`. `#[cfg(test)]` means this module is only
// compiled for tests, not in the real program.
#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: u32, y: u32, width: u32, height: u32) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn same_size_is_unchanged() {
        let r = region(10, 20, 300, 200);
        assert_eq!(r.to_physical(Some((2560, 1440)), (2560, 1440)), Some(r));
    }

    #[test]
    fn hidpi_scales_up() {
        // 200% scaling: logical 1280x720, frames are 2560x1440.
        let r = region(10, 20, 300, 200);
        assert_eq!(
            r.to_physical(Some((1280, 720)), (2560, 1440)),
            Some(region(20, 40, 600, 400))
        );
    }

    #[test]
    fn unknown_logical_size_means_no_scaling() {
        let r = region(10, 20, 300, 200);
        assert_eq!(r.to_physical(None, (2560, 1440)), Some(r));
    }

    #[test]
    fn clamped_to_the_frame() {
        let r = region(2500, 1400, 300, 200);
        assert_eq!(
            r.to_physical(None, (2560, 1440)),
            Some(region(2500, 1400, 60, 40))
        );
    }

    #[test]
    fn outside_the_frame_keeps_one_pixel() {
        let r = region(5000, 5000, 10, 10);
        assert_eq!(r.to_physical(None, (100, 100)), Some(region(99, 99, 1, 1)));
    }

    #[test]
    fn empty_frame() {
        assert_eq!(region(0, 0, 10, 10).to_physical(None, (0, 0)), None);
    }
}
