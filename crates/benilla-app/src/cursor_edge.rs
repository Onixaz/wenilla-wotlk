//! The cursor cropped to the window: Chromium drops a CSS custom cursor that crosses the
//! viewport's edge, where the reference clips its cursor at the screen's edge.

/// The `(width, height)` of a `w`×`h` cursor, its hotspot at the top-left, that fits in a window
/// of `window` logical px with the pointer at `pointer`; `None` when it fits whole or there is no
/// pointer. At least one pixel each way is kept.
pub(crate) fn edge_crop(
    pointer: Option<(f32, f32)>,
    window: (f32, f32),
    (w, h): (u32, u32),
) -> Option<(u32, u32)> {
    let (x, y) = pointer?;
    let fit = |room: f32, side: u32| (room.floor().max(1.0) as u32).min(side);
    let (cw, ch) = (fit(window.0 - x, w), fit(window.1 - y, h));
    (cw < w || ch < h).then_some((cw, ch))
}

#[cfg(test)]
mod tests {
    use super::edge_crop;

    const WINDOW: (f32, f32) = (1280.0, 720.0);

    fn crop(x: f32, y: f32) -> Option<(u32, u32)> {
        edge_crop(Some((x, y)), WINDOW, (32, 32))
    }

    #[test]
    fn a_cursor_with_room_is_whole() {
        assert_eq!(crop(100.0, 100.0), None);
        assert_eq!(crop(1248.0, 688.0), None, "exactly 32 px of room each way");
        assert_eq!(edge_crop(None, WINDOW, (32, 32)), None, "no pointer");
        assert_eq!(
            edge_crop(Some((1279.5, 719.5)), WINDOW, (1, 1)),
            None,
            "a 1×1 image always fits"
        );
    }

    #[test]
    fn near_the_bottom_edge_only_the_rows_that_fit_show() {
        assert_eq!(crop(100.0, 700.0), Some((32, 20)));
        assert_eq!(crop(100.0, 689.0), Some((32, 31)));
        // A fractional pointer (a HiDPI screen) rounds the room down, so the image stays inside.
        assert_eq!(crop(100.0, 700.5), Some((32, 19)));
    }

    #[test]
    fn near_the_right_edge_and_the_corner_it_crops_each_way() {
        assert_eq!(crop(1270.0, 100.0), Some((10, 32)));
        assert_eq!(crop(1270.0, 710.0), Some((10, 10)));
    }

    #[test]
    fn at_the_last_pixel_or_past_the_edge_one_pixel_is_kept() {
        assert_eq!(crop(1279.5, 719.5), Some((1, 1)));
        assert_eq!(crop(1300.0, 800.0), Some((1, 1)));
    }
}
