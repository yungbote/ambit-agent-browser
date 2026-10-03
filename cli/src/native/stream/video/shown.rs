//! What a viewer of exact units shows (contract browser-presentation-units):
//! the window's pixels as of the last unit sent. Small damage then leaves as
//! only the pixels that changed, and damage that changed nothing (a repaint
//! of text clipped out of view) as no unit at all. Measured on the latency
//! journey's typing page: a 1840 by 96 band of rows is 33 KB, the glyph a key
//! adds about 3 KB, and most keys there change nothing visible.

use super::policy::Band;
use crate::native::display::Rect;

const BYTES_PER_PIXEL: usize = 4;

/// The window's pixels (BGRX, its own rows) as the viewer shows them.
pub(super) struct Shown {
    visible: Rect,
    pixels: Vec<u8>,
}

impl Shown {
    /// The viewer was sent a picture of `source` (a BGRX framebuffer whose
    /// rows are `stride` bytes apart), whose rows `rows` changed since the
    /// unit before it: a window that moved, or nothing shown yet, records
    /// every row.
    pub(super) fn picture(
        shown: &mut Option<Self>,
        source: &[u8],
        stride: usize,
        visible: Rect,
        rows: Option<Band>,
    ) {
        let current = shown.as_ref().is_some_and(|shown| shown.visible == visible);
        if !current {
            let bytes = visible.width as usize * visible.height as usize * BYTES_PER_PIXEL;
            *shown = Some(Self {
                visible,
                pixels: vec![0; bytes],
            });
        }
        let Some(record) = shown.as_mut() else {
            return;
        };
        let whole = Band::whole(visible.y as u32 + visible.height);
        let rows = if current { rows.unwrap_or(whole) } else { whole };
        if record.copy(source, stride, rows).is_none() {
            *shown = None;
        }
    }

    /// The smallest rectangle of `rows` whose pixels differ from what is
    /// shown, now recorded as shown: `Some(None)` when nothing changed, and
    /// none when there is nothing to compare against (the window moved, or
    /// the rows are not all in `source`).
    pub(super) fn change(
        &mut self,
        source: &[u8],
        stride: usize,
        visible: Rect,
        rows: Band,
    ) -> Option<Option<Rect>> {
        if visible != self.visible {
            return None;
        }
        let width = visible.width as usize * BYTES_PER_PIXEL;
        let (top, bottom) = self.clip(rows);
        let mut bounds: Option<(usize, usize, usize, usize)> = None;
        for row in top..bottom {
            let from = self.source_row(row, stride);
            let now = source.get(from..from + width)?;
            let at = (row - visible.y as usize) * width;
            let then = &self.pixels[at..at + width];
            if now == then {
                continue;
            }
            let differs = |pixel: &(usize, (&[u8], &[u8]))| pixel.1 .0 != pixel.1 .1;
            let pairs = || {
                now.chunks_exact(BYTES_PER_PIXEL)
                    .zip(then.chunks_exact(BYTES_PER_PIXEL))
                    .enumerate()
            };
            let left = pairs().find(differs).map_or(0, |(x, _)| x);
            let right = pairs().rev().find(differs).map_or(0, |(x, _)| x + 1);
            bounds = Some(bounds.map_or((left, row, right, row + 1), |(l, t, r, _)| {
                (l.min(left), t, r.max(right), row + 1)
            }));
            self.pixels[at..at + width].copy_from_slice(now);
        }
        Some(bounds.map(|(left, top, right, bottom)| Rect {
            x: visible.x + left as i32,
            y: top as i32,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        }))
    }

    /// Framebuffer rows `rows` inside the window.
    fn clip(&self, rows: Band) -> (usize, usize) {
        let top = (rows.top as usize).max(self.visible.y as usize);
        let bottom = (rows.bottom as usize).min(self.visible.y as usize + self.visible.height as usize);
        (top, bottom.max(top))
    }

    /// Where framebuffer row `row` of the window starts in the source.
    fn source_row(&self, row: usize, stride: usize) -> usize {
        row * stride + self.visible.x as usize * BYTES_PER_PIXEL
    }

    /// Records rows `rows` of the window from `source`; none when they are
    /// not all in it.
    fn copy(&mut self, source: &[u8], stride: usize, rows: Band) -> Option<()> {
        let width = self.visible.width as usize * BYTES_PER_PIXEL;
        let (top, bottom) = self.clip(rows);
        for row in top..bottom {
            let from = self.source_row(row, stride);
            let at = (row - self.visible.y as usize) * width;
            self.pixels[at..at + width].copy_from_slice(source.get(from..from + width)?);
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A framebuffer 32 by 24 with a 20 by 16 window at (6, 4): every pixel
    /// a distinct value.
    fn framebuffer() -> (Vec<u8>, usize, Rect) {
        let stride = 32 * BYTES_PER_PIXEL;
        let pixels = (0..stride * 24).map(|index| (index * 7 % 251) as u8).collect();
        let visible = Rect {
            x: 6,
            y: 4,
            width: 20,
            height: 16,
        };
        (pixels, stride, visible)
    }

    fn set(pixels: &mut [u8], stride: usize, (x, y): (usize, usize)) {
        let at = y * stride + x * BYTES_PER_PIXEL;
        pixels[at] = pixels[at].wrapping_add(1);
    }

    /// After a picture, damage that changed nothing is no rectangle; a
    /// change is the smallest rectangle around its pixels, whatever rows the
    /// damage named, and is then shown, so the same damage again is none.
    #[test]
    fn a_change_is_the_smallest_rectangle_around_the_pixels_that_differ() {
        let (mut pixels, stride, visible) = framebuffer();
        let mut shown = None;
        Shown::picture(&mut shown, &pixels, stride, visible, None);
        let shown = shown.as_mut().unwrap();
        let rows = Band { top: 0, bottom: 24 };
        assert_eq!(shown.change(&pixels, stride, visible, rows), Some(None));
        set(&mut pixels, stride, (9, 7));
        set(&mut pixels, stride, (14, 10));
        set(&mut pixels, stride, (2, 8)); // beside the window: never shown
        assert_eq!(
            shown.change(&pixels, stride, visible, rows),
            Some(Some(Rect {
                x: 9,
                y: 7,
                width: 6,
                height: 4
            }))
        );
        assert_eq!(shown.change(&pixels, stride, visible, rows), Some(None));
        set(&mut pixels, stride, (20, 12));
        assert_eq!(
            shown.change(&pixels, stride, visible, Band { top: 0, bottom: 12 }),
            Some(None),
            "only the rows the damage named are compared"
        );
    }

    /// A picture records only the rows it changed; a window that moved has
    /// nothing to compare against until a picture records it whole.
    #[test]
    fn a_picture_records_its_rows_and_a_moved_window_starts_again() {
        let (mut pixels, stride, visible) = framebuffer();
        let mut shown = None;
        Shown::picture(&mut shown, &pixels, stride, visible, None);
        set(&mut pixels, stride, (10, 6));
        set(&mut pixels, stride, (10, 15));
        Shown::picture(&mut shown, &pixels, stride, visible, Some(Band { top: 6, bottom: 7 }));
        let all = Band { top: 0, bottom: 24 };
        let record = shown.as_mut().unwrap();
        assert_eq!(
            record.change(&pixels, stride, visible, all),
            Some(Some(Rect {
                x: 10,
                y: 15,
                width: 1,
                height: 1
            })),
            "row 15 was not in the picture's rows"
        );
        let moved = Rect { x: 5, ..visible };
        assert_eq!(record.change(&pixels, stride, moved, all), None);
        Shown::picture(&mut shown, &pixels, stride, moved, Some(Band { top: 0, bottom: 1 }));
        assert_eq!(shown.as_mut().unwrap().change(&pixels, stride, moved, all), Some(None));
    }
}
