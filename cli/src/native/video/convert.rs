//! Screen pixels to encoder pictures: rows of BGRX (the X server's 32-bit
//! little-endian layout, which the display helper hands over unchanged) into
//! planar Y′CbCr in `COLOUR`: the BT.709 matrix at full range. Each range's
//! matrix is kept in fixed point with 14 fractional bits (`Matrix`); each
//! chroma row sums to exactly zero, so a grey keeps Cb = Cr = 128, and every
//! grey's luma is its nearest code. 4:2:0 averages each 2×2 block before
//! converting its chroma.
//!
//! Only rows that changed are converted: a picture is kept between frames
//! and updated in place.

use super::{Chroma, Picture};

/// How samples map to colour, as the ITU-T H.273 code points a bitstream
/// signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Colour {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

/// The colour of every picture this module converts, and so what every
/// encoder signals (contract section 2, rev 4): BT.709 primaries, transfer
/// and matrix at full range. Chrome paints it within one code of the exact
/// inverse, where at limited range its painter puts saturated blue up to 12
/// codes off (media-producer/colour). A decoder that lost the signal would
/// paint limited range, so the signal is never left out.
pub(crate) const COLOUR: Colour = Colour {
    primaries: 1,
    transfer: 1,
    matrix: 1,
    full_range: true,
};

/// BT.709 (Kr = 0.2126, Kb = 0.0722; Cb = (B - Y) / 1.8556, Cr = (R - Y) /
/// 1.5748) at one range, in fixed point: the coefficient rows and black's
/// luma.
#[derive(Clone, Copy, Debug)]
struct Matrix {
    y: [i32; 3],
    cb: [i32; 3],
    cr: [i32; 3],
    black: u8,
}

/// Luma and chroma over all of 0-255.
const FULL: Matrix = Matrix {
    y: [3483, 11718, 1183],
    cb: [-1877, -6315, 8192],
    cr: [8192, -7441, -751],
    black: 0,
};

/// Luma scaled by 219/255 above 16 and chroma by 224/255: luma 16-235,
/// chroma 16-240.
const LIMITED: Matrix = Matrix {
    y: [2991, 10064, 1016],
    cb: [-1649, -5547, 7196],
    cr: [7196, -6536, -660],
    black: 16,
};

/// The matrix at the range `COLOUR` signals.
const MATRIX: Matrix = if COLOUR.full_range { FULL } else { LIMITED };

const BYTES_PER_PIXEL: usize = 4;
const SHIFT: u32 = 14;
const HALF: i32 = 1 << (SHIFT - 1);
/// Every grey's chroma.
const NEUTRAL: u8 = 128;
const CHROMA_ZERO: i32 = (NEUTRAL as i32) << SHIFT;

#[inline(always)]
fn luma(matrix: &Matrix, r: i32, g: i32, b: i32) -> u8 {
    let [kr, kg, kb] = matrix.y;
    ((kr * r + kg * g + kb * b + (i32::from(matrix.black) << SHIFT) + HALF) >> SHIFT) as u8
}

/// Chroma of `r`, `g`, `b` summed over `2^extra` pixels.
#[inline(always)]
fn chroma(coefficients: [i32; 3], r: i32, g: i32, b: i32, extra: u32) -> u8 {
    let value = (coefficients[0] * r
        + coefficients[1] * g
        + coefficients[2] * b
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
    /// At most two source rows, reused when an aperture needs masking before
    /// subsampled chroma averaging. No full framebuffer copy is needed.
    masked_rows: Vec<u8>,
}

impl Planar {
    pub(crate) fn new(chroma: Chroma, width: u32, height: u32) -> Self {
        let (width, height) = (width as usize, height as usize);
        let luma = width * height;
        let mut data = vec![NEUTRAL; chroma.picture_bytes(width, height)];
        data[..luma].fill(MATRIX.black);
        Self {
            chroma,
            width,
            height,
            data,
            masked_rows: Vec::new(),
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

    /// Convert only owned visible pixels in their framebuffer coordinates.
    /// Outside luma is black. Chroma crossing the aperture samples its nearest
    /// owned edge, preserving inside colour without reading a neighbouring window.
    pub(crate) fn convert_visible(
        &mut self,
        source: &[u8],
        stride: usize,
        source_size: (usize, usize),
        visible: super::EncoderRegion,
        rows: (usize, usize),
    ) -> bool {
        let (left, top) = (visible.x as usize, visible.y as usize);
        let (Some(right), Some(bottom)) = (
            left.checked_add(visible.width as usize),
            top.checked_add(visible.height as usize),
        ) else {
            return false;
        };
        if visible.width == 0
            || visible.height == 0
            || right > source_size.0
            || bottom > source_size.1
            || right > self.width
            || bottom > self.height
        {
            return false;
        }
        if left == 0 && top == 0 && (right, bottom) == source_size {
            return self.convert(source, stride, source_size, rows);
        }
        let (width, height) = (
            source_size.0.min(self.width),
            source_size.1.min(self.height),
        );
        let (row_top, row_bottom) = match self.chroma {
            Chroma::Full => rows,
            Chroma::Subsampled => (rows.0 & !1, rows.1.saturating_add(1) & !1),
        };
        let (row_top, row_bottom) = (row_top.min(height), row_bottom.min(height));
        if row_top >= row_bottom {
            return true;
        }
        if stride < width * 4 || source.len() < (row_bottom - 1) * stride + width * 4 {
            return false;
        }
        let chroma_width = self.chroma_width();
        let luma_size = self.width * self.height;
        let (luma, chroma) = self.data.split_at_mut(luma_size);
        let (cb, cr) = chroma.split_at_mut(chroma.len() / 2);
        match self.chroma {
            Chroma::Full => {
                for y in row_top..row_bottom {
                    let at = y * self.width;
                    luma[at..at + self.width].fill(MATRIX.black);
                    cb[at..at + self.width].fill(NEUTRAL);
                    cr[at..at + self.width].fill(NEUTRAL);
                    if (top..bottom).contains(&y) {
                        convert_full(
                            &source[y * stride + left * 4..y * stride + right * 4],
                            &mut luma[at + left..at + right],
                            &mut cb[at + left..at + right],
                            &mut cr[at + left..at + right],
                        );
                    }
                }
            }
            Chroma::Subsampled => {
                self.masked_rows.resize(width * 8, 0);
                for y in (row_top..row_bottom).step_by(2) {
                    let next = (y + 1).min(height - 1);
                    self.masked_rows.fill(0);
                    for (slot, source_y) in [(0, y), (1, next)] {
                        if y < bottom && next >= top {
                            let source_y = source_y.clamp(top, bottom - 1);
                            let at = slot * width * 4;
                            self.masked_rows[at + left * 4..at + right * 4].copy_from_slice(
                                &source
                                    [source_y * stride + left * 4..source_y * stride + right * 4],
                            );
                            if left % 2 == 1 {
                                self.masked_rows[at + (left - 1) * 4..at + left * 4]
                                    .copy_from_slice(
                                        &source[source_y * stride + left * 4
                                            ..source_y * stride + (left + 1) * 4],
                                    );
                            }
                            if right % 2 == 1 && right < width {
                                self.masked_rows[at + right * 4..at + (right + 1) * 4]
                                    .copy_from_slice(
                                        &source[source_y * stride + (right - 1) * 4
                                            ..source_y * stride + right * 4],
                                    );
                            }
                        }
                    }
                    let (upper, lower) = luma.split_at_mut((y + 1) * self.width);
                    upper[y * self.width..(y + 1) * self.width].fill(MATRIX.black);
                    if next != y {
                        lower[..self.width].fill(MATRIX.black);
                    }
                    let chroma_at = y / 2 * chroma_width;
                    cb[chroma_at..chroma_at + chroma_width].fill(NEUTRAL);
                    cr[chroma_at..chroma_at + chroma_width].fill(NEUTRAL);
                    convert_subsampled(
                        &self.masked_rows[..width * 4],
                        &self.masked_rows[width * 4..],
                        &mut upper[y * self.width..y * self.width + width],
                        (next != y).then(|| &mut lower[..width]),
                        &mut cb[chroma_at..chroma_at + width.div_ceil(2).min(chroma_width)],
                        &mut cr[chroma_at..chroma_at + width.div_ceil(2).min(chroma_width)],
                    );
                    // Edge extension belongs to chroma's sampling footprint;
                    // source padding stays black in luma.
                    for row_y in [y, next] {
                        let at = row_y * self.width;
                        if !(top..bottom).contains(&row_y) {
                            luma[at..at + self.width].fill(MATRIX.black);
                        } else {
                            luma[at..at + left].fill(MATRIX.black);
                            luma[at + right..at + self.width].fill(MATRIX.black);
                        }
                    }
                }
            }
        }
        true
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
            row[kept..].fill(MATRIX.black);
        }
        for (index, row) in chroma_planes.chunks_exact_mut(chroma_width).enumerate() {
            let kept = if index % chroma_height < chroma_height_kept {
                chroma_width_kept
            } else {
                0
            };
            row[kept..].fill(NEUTRAL);
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
        let (b, g, r) = (
            i32::from(pixel[0]),
            i32::from(pixel[1]),
            i32::from(pixel[2]),
        );
        *y = luma(&MATRIX, r, g, b);
        *cb = chroma(MATRIX.cb, r, g, b, 0);
        *cr = chroma(MATRIX.cr, r, g, b, 0);
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
        *y = luma(&MATRIX, r, g, b);
    }
    if let Some(lower) = lower {
        for (x, y) in lower.iter_mut().enumerate() {
            let (r, g, b) = pixel(lower_source, x);
            *y = luma(&MATRIX, r, g, b);
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
        *cb = chroma(MATRIX.cb, r, g, b, 2);
        *cr = chroma(MATRIX.cr, r, g, b, 2);
    }
}

/// Y′CbCr back to RGB through the exact inverse of the BT.709 matrix at the
/// range `colour` signals, as a decoder that honours the signal paints it:
/// for proofs of the round trip.
#[cfg(test)]
pub(crate) fn to_rgb(colour: Colour, y: u8, cb: u8, cr: u8) -> [u8; 3] {
    assert_eq!(
        colour.matrix, COLOUR.matrix,
        "the proofs invert BT.709 only"
    );
    let (black, luma_scale, chroma_scale) = if colour.full_range {
        (FULL.black, 1.0, 1.0)
    } else {
        (LIMITED.black, 255.0 / 219.0, 255.0 / 224.0)
    };
    let y = (f64::from(y) - f64::from(black)) * luma_scale;
    let cb = (f64::from(cb) - f64::from(NEUTRAL)) * chroma_scale;
    let cr = (f64::from(cr) - f64::from(NEUTRAL)) * chroma_scale;
    let r = y + 1.5748 * cr;
    let g = y - 0.187_324 * cb - 0.468_124 * cr;
    let b = y + 1.8556 * cb;
    [r, g, b].map(|value| value.round().clamp(0.0, 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aperture_boundary_sampling_keeps_the_owned_colour_at_odd_chroma_edges() {
        let source = bgrx(&vec![[0, 255, 0]; 8 * 6]);
        for chroma in [Chroma::Full, Chroma::Subsampled] {
            let mut picture = Planar::new(chroma, 8, 6);
            assert!(picture.convert_visible(
                &source,
                32,
                (8, 6),
                super::super::EncoderRegion {
                    x: 1,
                    y: 1,
                    width: 5,
                    height: 3
                },
                (0, 6)
            ));
            let planes = picture.picture().planes().unwrap();
            for (x, y) in [(1, 1), (5, 1), (1, 3), (5, 3)] {
                let (cx, cy) = if chroma == Chroma::Full {
                    (x, y)
                } else {
                    (x / 2, y / 2)
                };
                let rgb = to_rgb(
                    COLOUR,
                    planes[0].0[y * 8 + x],
                    planes[1].0[cy * planes[1].1 + cx],
                    planes[2].0[cy * planes[2].1 + cx],
                );
                assert!(
                    rgb[0] <= 2 && rgb[1] >= 253 && rgb[2] <= 2,
                    "{chroma:?} at{x},{y}: {rgb:?}"
                );
            }
        }
    }

    fn bgrx(pixels: &[[u8; 3]]) -> Vec<u8> {
        pixels
            .iter()
            .flat_map(|[r, g, b]| [*b, *g, *r, 0])
            .collect()
    }

    /// Each range's matrix with the colour that signals it.
    const RANGES: [(Matrix, Colour); 2] = [
        (
            FULL,
            Colour {
                full_range: true,
                ..COLOUR
            },
        ),
        (
            LIMITED,
            Colour {
                full_range: false,
                ..COLOUR
            },
        ),
    ];

    /// In both ranges every coefficient is the nearest fixed-point value of
    /// BT.709, and each of the 256 greys lands on its nearest code with
    /// neutral chroma: white 255 and black 0 at full range, 235 and 16 at
    /// limited range.
    #[test]
    fn each_range_is_bt709_to_the_nearest_code_and_every_grey_stays_neutral() {
        let (kr, kb) = (0.2126, 0.0722);
        let kg = 1.0 - kr - kb;
        let unit = f64::from(1 << SHIFT);
        for (matrix, colour) in RANGES {
            let (luma_codes, chroma_codes) = if colour.full_range {
                (255, 255)
            } else {
                (219, 224)
            };
            let luma_scale = f64::from(luma_codes) / 255.0 * unit;
            let chroma_scale = f64::from(chroma_codes) / 255.0 * unit;
            let exact = [
                [kr, kg, kb].map(|k| k * luma_scale),
                [-kr, -kg, 1.0 - kb].map(|k| k / (2.0 * (1.0 - kb)) * chroma_scale),
                [1.0 - kr, -kg, -kb].map(|k| k / (2.0 * (1.0 - kr)) * chroma_scale),
            ];
            for (row, exact) in [matrix.y, matrix.cb, matrix.cr].iter().zip(exact) {
                for (coefficient, exact) in row.iter().zip(exact) {
                    assert!(
                        (f64::from(*coefficient) - exact).abs() <= 0.5,
                        "{colour:?}: {coefficient} for {exact}"
                    );
                }
            }
            assert_eq!(matrix.cb.iter().sum::<i32>(), 0);
            assert_eq!(matrix.cr.iter().sum::<i32>(), 0);
            for grey in 0..=255u8 {
                let value = i32::from(grey);
                let nearest = u32::from(matrix.black) + (luma_codes * u32::from(grey) + 127) / 255;
                assert_eq!(
                    u32::from(luma(&matrix, value, value, value)),
                    nearest,
                    "{colour:?}: {grey}"
                );
                for (coefficients, sum, extra) in [
                    (matrix.cb, value, 0),
                    (matrix.cr, value, 0),
                    (matrix.cb, 4 * value, 2),
                ] {
                    assert_eq!(chroma(coefficients, sum, sum, sum, extra), NEUTRAL);
                }
            }
        }
    }

    /// The inverse reads the samples at the range the signal names. Read at
    /// the other range the same samples look wrong: a limited-range white is
    /// a grey at full range, and a full-range 231 is stretched to 250 at
    /// limited range (the viewer lane's washed fixture).
    #[test]
    fn the_inverse_honours_the_signalled_range() {
        let [(_, full), (_, limited)] = RANGES;
        assert_eq!(to_rgb(full, 255, 128, 128), [255; 3]);
        assert_eq!(to_rgb(full, 0, 128, 128), [0; 3]);
        assert_eq!(to_rgb(limited, 235, 128, 128), [255; 3]);
        assert_eq!(to_rgb(limited, 16, 128, 128), [0; 3]);
        assert_eq!(to_rgb(full, 235, 128, 128), [235; 3]);
        assert_eq!(to_rgb(limited, 231, 128, 128), [250; 3]);
    }

    /// Colour bars through each range's matrix and the exact inverse at that
    /// range: white and black exactly, every other colour within one code
    /// value, never washed or tinted.
    #[test]
    fn colour_bars_round_trip_within_one_code_value_in_either_range() {
        let bars: [[u8; 3]; 10] = [
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
        let samples = |matrix: &Matrix, [r, g, b]: [u8; 3]| {
            let [r, g, b] = [r, g, b].map(i32::from);
            (
                luma(matrix, r, g, b),
                chroma(matrix.cb, r, g, b, 0),
                chroma(matrix.cr, r, g, b, 0),
            )
        };
        for (matrix, colour) in RANGES {
            for bar in bars {
                let (y, cb, cr) = samples(&matrix, bar);
                let back = to_rgb(colour, y, cb, cr);
                let allowed = u8::from(bar != [255; 3] && bar != [0; 3]);
                assert!(
                    (0..3).all(|channel| back[channel].abs_diff(bar[channel]) <= allowed),
                    "{colour:?}: {bar:?} came back {back:?}"
                );
            }
        }
        // Reference samples of pure red: BT.709 at full, then limited range.
        let [(full, _), (limited, _)] = RANGES;
        assert_eq!(samples(&full, [255, 0, 0]), (54, 99, 255));
        assert_eq!(samples(&limited, [255, 0, 0]), (63, 102, 240));
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
        let white = luma(&MATRIX, 255, 255, 255);
        assert_eq!(
            &planes[0].0[..4],
            &[
                luma(&MATRIX, 255, 0, 0),
                luma(&MATRIX, 0, 0, 255),
                white,
                white
            ]
        );
        // Two red and two blue pixels: their summed colour, averaged.
        assert_eq!(planes[1].0[0], chroma(MATRIX.cb, 510, 0, 510, 2));
        assert_eq!(planes[2].0[0], chroma(MATRIX.cr, 510, 0, 510, 2));
        // BT.709 of (127.5, 0, 127.5): Cb 177.1 and Cr 185.9 at full range,
        // 171.2 and 178.9 at limited range.
        let averaged = if COLOUR.full_range {
            (177, 186)
        } else {
            (171, 179)
        };
        assert_eq!((planes[1].0[0], planes[2].0[0]), averaged);
        assert_eq!((planes[1].0[1], planes[2].0[1]), (NEUTRAL, NEUTRAL));
    }

    /// Only the named rows change; rows outside them keep their pixels, and
    /// a picture larger than its source keeps black beyond it.
    #[test]
    fn conversion_touches_only_the_changed_rows_inside_the_source() {
        let (black, white) = (MATRIX.black, luma(&MATRIX, 255, 255, 255));
        let mut picture = Planar::new(Chroma::Full, 4, 4);
        let source: Vec<u8> = (0..3).flat_map(|_| bgrx(&[[255, 255, 255]; 2])).collect();
        assert!(picture.convert(&source, 8, (2, 3), (1, 2)));
        let planes = picture.picture().planes().unwrap();
        let luma: Vec<&[u8]> = planes[0].0.chunks(4).collect();
        assert_eq!(
            luma,
            [
                &[black; 4],
                &[white, white, black, black],
                &[black; 4],
                &[black; 4]
            ]
        );
        // A source row beyond what the buffer holds is refused, not read.
        assert!(!picture.convert(&source[..20], 8, (2, 3), (0, 3)));
        assert!(picture.convert(&source, 8, (2, 3), (0, 3)));
        picture.clear_outside(1, 1);
        let planes = picture.picture().planes().unwrap();
        assert_eq!(&planes[0].0[..4], &[white, black, black, black]);
        assert!(planes[0].0[4..].iter().all(|value| *value == black));
        assert!(planes[1].0[1..].iter().all(|value| *value == NEUTRAL));
    }
}
