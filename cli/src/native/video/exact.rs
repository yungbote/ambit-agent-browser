//! Exact units (contract browser-presentation-units): a rectangle of the
//! capture's pixels, lossless, as one 8-bit RGB PNG that a viewer decodes
//! natively and draws over its surface. Measured on a typed key
//! (`a_typed_key_as_an_exact_rectangle`): 0.07 ms and 2.9 KB for its 40 by 68
//! glyph, 2 ms and 15 KB for the 96 rows across the window, against 15 to 17
//! ms for an AV1 picture of the window.

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};

const BYTES_PER_PIXEL: usize = 4;

/// The PNG of the rectangle `(x, y, width, height)` of a BGRX picture whose
/// rows are `stride` bytes apart; none when it is empty or past the rows'
/// bytes (the caller keeps it inside the picture's window).
pub(crate) fn png(
    bgrx: &[u8],
    stride: usize,
    (x, y, width, height): (usize, usize, usize, usize),
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || (x + width) * BYTES_PER_PIXEL > stride {
        return None;
    }
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in y..y + height {
        let start = row * stride + x * BYTES_PER_PIXEL;
        for pixel in bgrx
            .get(start..start + width * BYTES_PER_PIXEL)?
            .chunks_exact(BYTES_PER_PIXEL)
        {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(&rgb, width as u32, height as u32, ExtendedColorType::Rgb8)
        .ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PNG holds exactly the rectangle's pixels, in RGB order, whatever
    /// the picture around it; a rectangle past its rows' bytes has none.
    #[test]
    fn a_rectangle_round_trips_exactly_and_only_inside_its_picture() {
        let (width, height, stride) = (37usize, 23usize, 37 * 4 + 12);
        let mut seed = 0x1234_5678u32;
        let bgrx: Vec<u8> = (0..stride * height)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let area = (5, 3, 20, 11);
        let decoded = image::load_from_memory(&png(&bgrx, stride, area).unwrap())
            .unwrap()
            .to_rgb8();
        assert_eq!((decoded.width(), decoded.height()), (20, 11));
        for (column, row, pixel) in decoded.enumerate_pixels() {
            let at = (3 + row as usize) * stride + (5 + column as usize) * 4;
            assert_eq!(pixel.0, [bgrx[at + 2], bgrx[at + 1], bgrx[at]]);
        }
        assert!(png(&bgrx, stride, (0, 0, width, height)).is_some());
        assert!(png(&bgrx, stride, (36, 0, 8, 1)).is_none(), "past a row's bytes");
        assert!(png(&bgrx, stride, (0, 20, 4, 4)).is_none(), "past the last row");
        assert!(png(&bgrx, stride, (0, 0, 0, 4)).is_none(), "empty");
    }
}
