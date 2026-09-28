//! Screen pixels to encoder pictures: rows of BGRX (the X server's 32-bit
//! little-endian layout, which the display helper hands over unchanged) into
//! planar Y′CbCr with the BT.709 matrix at full range, the colour description
//! every stream signals. Fixed point with 14 fractional bits; each
//! coefficient row sums to exactly one (Y) or zero (Cb, Cr), so a grey maps
//! to itself with Cb = Cr = 128 and white stays 255. 4:2:0 averages each 2×2
//! block before converting its chroma.
//!
//! Only rows that changed are converted: a picture is kept between frames
//! and updated in place.

use super::{Chroma, Picture};

const BYTES_PER_PIXEL: usize = 4;
const SHIFT: u32 = 14;
const HALF: i32 = 1 << (SHIFT - 1);
const CHROMA_ZERO: i32 = 128 << SHIFT;
// BT.709: Kr = 0.2126, Kb = 0.0722; Cb = (B - Y) / 1.8556, Cr = (R - Y) / 1.5748.
const Y: [i32; 3] = [3483, 11718, 1183];
const CB: [i32; 3] = [-1877, -6315, 8192];
const CR: [i32; 3] = [8192, -7441, -751];

#[inline(always)]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    ((Y[0] * r + Y[1] * g + Y[2] * b + HALF) >> SHIFT) as u8
}

/// Chroma of `r`, `g`, `b` summed over `2^extra` pixels.
#[inline(always)]
fn chroma(coefficients: [i32; 3], r: i32, g: i32, b: i32, extra: u32) -> u8 {
    let value = (coefficients[0] * r + coefficients[1] * g + coefficients[2] * b
        + (CHROMA_ZERO << extra)
        + (HALF << extra))
        >> (SHIFT + extra);
    value.clamp(0, 255) as u8
}

/// A picture the producer owns and updates in place, in the layout
/// `Picture` names: Y, Cb, Cr planes with tightly packed rows. A fresh one is
/// black.
pub(crate) struct Planar {
    chroma: Chroma,
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl Planar {
    pub(crate) fn new(chroma: Chroma, width: u32, height: u32) -> Self {
        let (width, height) = (width as usize, height as usize);
        let luma = width * height;
        let mut data = vec![128u8; chroma.picture_bytes(width, height)];
        data[..luma].fill(0);
        Self {
            chroma,
            width,
            height,
            data,
        }
    }

    pub(crate) fn picture(&self) -> Picture<'_> {
        Picture {
            chroma: self.chroma,
            width: self.width as u32,
            height: self.height as u32,
            data: &self.data,
        }
    }

    fn chroma_width(&self) -> usize {
        match self.chroma {
            Chroma::Subsampled => self.width / 2,
            Chroma::Full => self.width,
        }
    }

    /// The rows `[top, bottom)` of a `source_width` × `source_height` BGRX
    /// source whose rows are `stride` bytes apart, converted in place. The
    /// picture may be larger than the source; a 4:2:0 picture widens the
    /// range to whole row pairs within the source. Returns false when the
    /// source cannot hold what it claims to.
    pub(crate) fn convert(
        &mut self,
        source: &[u8],
        stride: usize,
        (source_width, source_height): (usize, usize),
        (top, bottom): (usize, usize),
    ) -> bool {
        let width = source_width.min(self.width);
        let height = source_height.min(self.height);
        let (top, bottom) = match self.chroma {
            Chroma::Subsampled => (top & !1, (bottom + 1) & !1),
            Chroma::Full => (top, bottom),
        };
        let (top, bottom) = (top.min(height), bottom.min(height));
        if top >= bottom {
            return true;
        }
        if stride < width * BYTES_PER_PIXEL
            || source.len() < (bottom - 1) * stride + width * BYTES_PER_PIXEL
        {
            return false;
        }
        let luma_size = self.width * self.height;
        let chroma_width = self.chroma_width();
        let (luma_plane, chroma_planes) = self.data.split_at_mut(luma_size);
        let chroma_size = chroma_planes.len() / 2;
        let (cb_plane, cr_plane) = chroma_planes.split_at_mut(chroma_size);
        let row = |index: usize| &source[index * stride..index * stride + width * BYTES_PER_PIXEL];
        match self.chroma {
            Chroma::Full => {
                for index in top..bottom {
                    let at = index * self.width;
                    convert_full(
                        row(index),
                        &mut luma_plane[at..at + width],
                        &mut cb_plane[at..at + width],
                        &mut cr_plane[at..at + width],
                    );
                }
            }
            Chroma::Subsampled => {
                for index in (top..bottom).step_by(2) {
                    let next = (index + 1).min(height - 1);
                    let chroma_at = (index / 2) * chroma_width;
                    let (upper, lower) = luma_plane.split_at_mut(next.max(index + 1) * self.width);
                    let upper = &mut upper[index * self.width..index * self.width + width];
                    let lower = if next == index {
                        None
                    } else {
                        Some(&mut lower[..width])
                    };
                    convert_subsampled(
                        row(index),
                        row(next),
                        upper,
                        lower,
                        &mut cb_plane[chroma_at..chroma_at + width.div_ceil(2).min(chroma_width)],
                        &mut cr_plane[chroma_at..chroma_at + width.div_ceil(2).min(chroma_width)],
                    );
                }
            }
        }
        true
    }

    /// The planes of a decoded picture of the same geometry, for proofs.
    #[cfg(test)]
    pub(crate) fn replace(&mut self, planes: &[Vec<u8>; 3]) {
        let joined: Vec<u8> = planes.iter().flatten().copied().collect();
        assert_eq!(joined.len(), self.data.len());
        self.data = joined;
    }

    /// Everything outside a `width` × `height` source back to black, after
    /// the source shrank: no stale pixels are encoded beyond it.
    pub(crate) fn clear_outside(&mut self, width: usize, height: usize) {
        let (width, height) = (width.min(self.width), height.min(self.height));
        let luma_size = self.width * self.height;
        let chroma_width = self.chroma_width();
        let (chroma_width_kept, chroma_height_kept, chroma_height) = match self.chroma {
            Chroma::Subsampled => (width.div_ceil(2), height.div_ceil(2), self.height / 2),
            Chroma::Full => (width, height, self.height),
        };
        let (luma_plane, chroma_planes) = self.data.split_at_mut(luma_size);
        for (index, row) in luma_plane.chunks_exact_mut(self.width).enumerate() {
            let kept = if index < height { width } else { 0 };
            row[kept..].fill(0);
        }
        for (index, row) in chroma_planes.chunks_exact_mut(chroma_width).enumerate() {
            let kept = if index % chroma_height < chroma_height_kept {
                chroma_width_kept
            } else {
                0
            };
            row[kept..].fill(128);
        }
    }
}

fn convert_full(source: &[u8], luma_row: &mut [u8], cb_row: &mut [u8], cr_row: &mut [u8]) {
    for (((pixel, y), cb), cr) in source
        .chunks_exact(BYTES_PER_PIXEL)
        .zip(luma_row.iter_mut())
        .zip(cb_row.iter_mut())
        .zip(cr_row.iter_mut())
    {
        let (b, g, r) = (i32::from(pixel[0]), i32::from(pixel[1]), i32::from(pixel[2]));
        *y = luma(r, g, b);
        *cb = chroma(CB, r, g, b, 0);
        *cr = chroma(CR, r, g, b, 0);
    }
}

/// One pair of rows: both luma rows, and one chroma row from each 2×2 block's
/// summed colour (an odd last column or row repeats its neighbour).
fn convert_subsampled(
    upper_source: &[u8],
    lower_source: &[u8],
    upper: &mut [u8],
    lower: Option<&mut [u8]>,
    cb_row: &mut [u8],
    cr_row: &mut [u8],
) {
    let width = upper.len();
    let pixel = |row: &[u8], x: usize| {
        let at = x.min(width - 1) * BYTES_PER_PIXEL;
        (
            i32::from(row[at + 2]),
            i32::from(row[at + 1]),
            i32::from(row[at]),
        )
    };
    for (x, y) in upper.iter_mut().enumerate() {
        let (r, g, b) = pixel(upper_source, x);
        *y = luma(r, g, b);
    }
    if let Some(lower) = lower {
        for (x, y) in lower.iter_mut().enumerate() {
            let (r, g, b) = pixel(lower_source, x);
            *y = luma(r, g, b);
        }
    }
    for (column, (cb, cr)) in cb_row.iter_mut().zip(cr_row.iter_mut()).enumerate() {
        let x = column * 2;
        let (mut r, mut g, mut b) = (0, 0, 0);
        for (row, dx) in [
            (upper_source, 0),
            (upper_source, 1),
            (lower_source, 0),
            (lower_source, 1),
        ] {
            let (pr, pg, pb) = pixel(row, x + dx);
            r += pr;
            g += pg;
            b += pb;
        }
        *cb = chroma(CB, r, g, b, 2);
        *cr = chroma(CR, r, g, b, 2);
    }
}

/// BT.709 full-range Y′CbCr back to RGB, for proofs of the round trip.
#[cfg(test)]
pub(crate) fn to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let (y, cb, cr) = (f64::from(y), f64::from(cb) - 128.0, f64::from(cr) - 128.0);
    let r = y + 1.5748 * cr;
    let g = y - 0.187_324 * cb - 0.468_124 * cr;
    let b = y + 1.8556 * cb;
    [r, g, b].map(|value| value.round().clamp(0.0, 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgrx(pixels: &[[u8; 3]]) -> Vec<u8> {
        pixels
            .iter()
            .flat_map(|[r, g, b]| [*b, *g, *r, 0])
            .collect()
    }

    #[test]
    fn coefficients_keep_grey_neutral_and_the_range_full() {
        assert_eq!(Y.iter().sum::<i32>(), 1 << SHIFT);
        assert_eq!(CB.iter().sum::<i32>(), 0);
        assert_eq!(CR.iter().sum::<i32>(), 0);
        for grey in [0u8, 1, 17, 128, 200, 254, 255] {
            let value = i32::from(grey);
            assert_eq!(luma(value, value, value), grey);
            assert_eq!(chroma(CB, value, value, value, 0), 128);
            assert_eq!(chroma(CR, value, value, value, 0), 128);
        }
    }

    /// Colour bars through the conversion and the exact inverse land within
    /// one code value of where they started: the matrix and range are the
    /// ones the stream signals, not washed or tinted.
    #[test]
    fn colour_bars_round_trip_within_one_code_value() {
        let bars = [
            [255, 255, 255],
            [255, 255, 0],
            [0, 255, 255],
            [0, 255, 0],
            [255, 0, 255],
            [255, 0, 0],
            [0, 0, 255],
            [0, 0, 0],
            [10, 102, 194],
            [29, 29, 31],
        ];
        let mut picture = Planar::new(Chroma::Full, bars.len() as u32, 1);
        let source = bgrx(&bars);
        assert!(picture.convert(&source, source.len(), (bars.len(), 1), (0, 1)));
        let planes = picture.picture().planes().unwrap();
        for (index, expected) in bars.iter().enumerate() {
            let back = to_rgb(planes[0].0[index], planes[1].0[index], planes[2].0[index]);
            for channel in 0..3 {
                assert!(
                    back[channel].abs_diff(expected[channel]) <= 1,
                    "bar {index}: {expected:?} came back {back:?}"
                );
            }
        }
        // Reference values of BT.709 full range for pure red.
        assert_eq!(
            (planes[0].0[5], planes[1].0[5], planes[2].0[5]),
            (54, 99, 255)
        );
    }

    #[test]
    fn subsampled_chroma_averages_each_block_and_luma_stays_per_pixel() {
        // One 2x2 block of red and blue, one of white.
        let rows = [
            [[255, 0, 0], [0, 0, 255], [255, 255, 255], [255, 255, 255]],
            [[0, 0, 255], [255, 0, 0], [255, 255, 255], [255, 255, 255]],
        ];
        let source: Vec<u8> = rows.iter().flat_map(|row| bgrx(row)).collect();
        let mut picture = Planar::new(Chroma::Subsampled, 4, 2);
        assert!(picture.convert(&source, 16, (4, 2), (0, 2)));
        let planes = picture.picture().planes().unwrap();
        assert_eq!(&planes[0].0[..4], &[54, 18, 255, 255]);
        // Two red and two blue pixels: their summed colour, averaged.
        assert_eq!(planes[1].0[0], chroma(CB, 510, 0, 510, 2));
        assert_eq!(planes[2].0[0], chroma(CR, 510, 0, 510, 2));
        // BT.709 full range of (127.5, 0, 127.5): Cb 177.14, Cr 185.9.
        assert_eq!((planes[1].0[0], planes[2].0[0]), (177, 186));
        assert_eq!((planes[1].0[1], planes[2].0[1]), (128, 128));
    }

    /// Only the named rows change; rows outside them keep their pixels, and
    /// a picture larger than its source keeps black beyond it.
    #[test]
    fn conversion_touches_only_the_changed_rows_inside_the_source() {
        let mut picture = Planar::new(Chroma::Full, 4, 4);
        let white = bgrx(&[[255, 255, 255]; 2]);
        let source: Vec<u8> = (0..3).flat_map(|_| white.clone()).collect();
        assert!(picture.convert(&source, 8, (2, 3), (1, 2)));
        let planes = picture.picture().planes().unwrap();
        let luma: Vec<&[u8]> = planes[0].0.chunks(4).collect();
        assert_eq!(luma, [&[0, 0, 0, 0], &[255, 255, 0, 0], &[0, 0, 0, 0], &[0, 0, 0, 0]]);
        // A source row beyond what the buffer holds is refused, not read.
        assert!(!picture.convert(&source[..20], 8, (2, 3), (0, 3)));
        assert!(picture.convert(&source, 8, (2, 3), (0, 3)));
        picture.clear_outside(1, 1);
        let planes = picture.picture().planes().unwrap();
        assert_eq!(&planes[0].0[..4], &[255, 0, 0, 0]);
        assert!(planes[0].0[4..].iter().all(|value| *value == 0));
        assert!(planes[1].0[1..].iter().all(|value| *value == 128));
    }
}
