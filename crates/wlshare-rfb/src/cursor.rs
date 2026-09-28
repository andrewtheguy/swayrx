//! The pointer, sent apart from framebuffer pixels.
//!
//! Captured frames never contain the pointer. The compositor's cursor image
//! arrives on its own instead — whatever shape the application under it chose,
//! at the output's own density — and goes to the client as a pseudo-rectangle
//! whose x and y are the hotspot. The client draws it at the coordinates it
//! already sends in pointer events, so the pointer moves without waiting for a
//! captured frame.
//!
//! Two pseudo-encodings carry it. *Cursor With Alpha* (`-314`) when the client
//! lists it: premultiplied RGBA, Raw-encoded, which keeps a shadow and
//! antialiased edges as they are. The standard *Cursor* (`-239`), which every
//! client must list, otherwise: pixels in the client's format beside a 1-bit
//! mask, the alpha cut at half. An empty rectangle of either says there is no
//! pointer to draw — an application hid it, or it is on another output.
//!
//! RFB cursor dimensions are framebuffer pixels, which is what the compositor
//! renders a cursor image in, so the image is sent at its own size. It is cropped
//! to what it paints first: a cursor plane's buffer is often far larger than the
//! shape in it.

use crate::msg::rect_header;
use crate::pixel::PixelFormat;
use crate::{ENCODING_CURSOR, ENCODING_CURSOR_WITH_ALPHA, ENCODING_RAW};

/// The alpha at and above which the standard Cursor's mask calls a pixel opaque.
const MASK_THRESHOLD: u8 = 0x80;

/// A cursor image: premultiplied RGBA, rows top down, cropped to the pixels it
/// paints and the hotspot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    width: u16,
    height: u16,
    hotspot: (u16, u16),
    rgba: Vec<u8>,
}

impl CursorImage {
    /// Cut a `width`×`height` premultiplied RGBA image down to the smallest box
    /// holding every pixel with any alpha and the hotspot, or `None` for an
    /// image with nothing in it.
    ///
    /// The hotspot is clamped into the image first: a client places the image by
    /// it, and one outside the image is not a place any client agrees on.
    pub fn cropped(width: u16, height: u16, hotspot: (i32, i32), rgba: &[u8]) -> Option<Self> {
        let (w, h) = (usize::from(width), usize::from(height));
        assert_eq!(rgba.len(), w * h * 4, "a {width}x{height} RGBA image");
        let mut painted: Option<(usize, usize, usize, usize)> = None;
        for (i, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
            if px[3] == 0 {
                continue;
            }
            let (x, y) = (i % w, i / w);
            painted = Some(match painted {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
        let (x0, y0, x1, y1) = painted?;
        let hx = hotspot.0.clamp(0, i32::from(width) - 1) as usize;
        let hy = hotspot.1.clamp(0, i32::from(height) - 1) as usize;
        let (x0, y0, x1, y1) = (x0.min(hx), y0.min(hy), x1.max(hx), y1.max(hy));
        let mut cropped = Vec::with_capacity((x1 - x0 + 1) * (y1 - y0 + 1) * 4);
        for y in y0..=y1 {
            cropped.extend_from_slice(&rgba[(y * w + x0) * 4..(y * w + x1 + 1) * 4]);
        }
        Some(Self {
            width: (x1 - x0 + 1) as u16,
            height: (y1 - y0 + 1) as u16,
            hotspot: ((hx - x0) as u16, (hy - y0) as u16),
            rgba: cropped,
        })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn hotspot(&self) -> (u16, u16) {
        self.hotspot
    }

    /// The pixels: premultiplied RGBA, rows top down.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The image a Cursor With Alpha rectangle carried, as a client reads it:
    /// the rectangle's x and y as the hotspot and its body as the pixels. `None`
    /// is the empty rectangle, which says there is no pointer to draw.
    #[cfg(test)]
    fn from_alpha_rect(width: u16, height: u16, hotspot: (u16, u16), rgba: Vec<u8>) -> Option<Self> {
        assert_eq!(rgba.len(), usize::from(width) * usize::from(height) * 4, "a {width}x{height} RGBA image");
        if rgba.is_empty() {
            return None;
        }
        Some(Self { width, height, hotspot: (hotspot.0.min(width - 1), hotspot.1.min(height - 1)), rgba })
    }

    /// The image a standard Cursor rectangle carried: `pixels` in `format`, and
    /// the mask saying which of them are drawn, which become opaque.
    #[cfg(test)]
    fn from_masked_rect(format: &PixelFormat, width: u16, height: u16, hotspot: (u16, u16), pixels: &[u8], mask: &[u8]) -> Option<Self> {
        let w = usize::from(width);
        let mask_stride = w.div_ceil(8);
        assert_eq!(pixels.len(), w * usize::from(height) * 4, "a {width}x{height} image");
        assert_eq!(mask.len(), mask_stride * usize::from(height), "a {width}x{height} mask");
        let mut rgba = Vec::with_capacity(pixels.len());
        for (i, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
            let (x, y) = (i % w, i / w);
            if mask[y * mask_stride + x / 8] & (0x80 >> (x % 8)) != 0 {
                let [b, g, r, _] = format.bgrx(format.pixel_value(*pixel));
                rgba.extend_from_slice(&[r, g, b, 255]);
            } else {
                rgba.extend_from_slice(&[0; 4]);
            }
        }
        Self::from_alpha_rect(width, height, hotspot, rgba)
    }
}

/// A Cursor With Alpha pseudo-rectangle: the image as it is, Raw-encoded, or an
/// empty one for no pointer.
pub fn alpha_cursor_rect(image: Option<&CursorImage>) -> Vec<u8> {
    let Some(image) = image else {
        let mut rect = rect_header(0, 0, 0, 0, ENCODING_CURSOR_WITH_ALPHA).to_vec();
        rect.extend_from_slice(&ENCODING_RAW.to_be_bytes());
        return rect;
    };
    let (hx, hy) = image.hotspot;
    let mut rect = Vec::with_capacity(16 + image.rgba.len());
    rect.extend_from_slice(&rect_header(hx, hy, image.width, image.height, ENCODING_CURSOR_WITH_ALPHA));
    rect.extend_from_slice(&ENCODING_RAW.to_be_bytes());
    rect.extend_from_slice(&image.rgba);
    rect
}

/// A standard Cursor pseudo-rectangle in the client's pixel format: each pixel
/// its colour with the alpha divided back out, the mask set where the alpha is
/// at least half, or an empty one for no pointer.
pub fn cursor_rect(format: &PixelFormat, image: Option<&CursorImage>) -> Vec<u8> {
    let Some(image) = image else {
        return rect_header(0, 0, 0, 0, ENCODING_CURSOR).to_vec();
    };
    let w = usize::from(image.width);
    let mask_stride = w.div_ceil(8);
    let mut mask = vec![0u8; mask_stride * usize::from(image.height)];
    let (hx, hy) = image.hotspot;
    let mut rect = Vec::with_capacity(12 + image.rgba.len() + mask.len());
    rect.extend_from_slice(&rect_header(hx, hy, image.width, image.height, ENCODING_CURSOR));
    for (i, &[r, g, b, a]) in image.rgba.as_chunks::<4>().0.iter().enumerate() {
        // Pixels outside the mask go out black: nobody draws them, and a flat
        // run is what a compressing transport does best with.
        let bgrx = if a >= MASK_THRESHOLD {
            let (x, y) = (i % w, i / w);
            mask[y * mask_stride + x / 8] |= 0x80 >> (x % 8);
            [unpremultiply(b, a), unpremultiply(g, a), unpremultiply(r, a), 0]
        } else {
            [0; 4]
        };
        rect.extend_from_slice(&format.pixel_bytes(format.value(bgrx)));
    }
    rect.extend_from_slice(&mask);
    rect
}

/// A premultiplied channel's straight value, rounded.
fn unpremultiply(channel: u8, alpha: u8) -> u8 {
    let (c, a) = (u32::from(channel), u32::from(alpha));
    ((c * 255 + a / 2) / a).min(255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x3 image with one painted 2x2 block at (1, 1): an opaque red pixel, a
    /// half-transparent white one, a faint shadow, and an opaque blue pixel.
    fn image() -> Vec<u8> {
        let mut rgba = vec![0u8; 4 * 3 * 4];
        let mut put = |x: usize, y: usize, px: [u8; 4]| rgba[(y * 4 + x) * 4..(y * 4 + x + 1) * 4].copy_from_slice(&px);
        put(1, 1, [255, 0, 0, 255]);
        put(2, 1, [128, 128, 128, 128]);
        put(1, 2, [0, 0, 0, 40]);
        put(2, 2, [0, 0, 255, 255]);
        rgba
    }

    #[test]
    fn the_image_is_cropped_to_what_it_paints_and_the_hotspot() {
        let cropped = CursorImage::cropped(4, 3, (1, 1), &image()).unwrap();
        assert_eq!((cropped.width(), cropped.height(), cropped.hotspot()), (2, 2, (0, 0)));
        assert_eq!(cropped.rgba, [[255, 0, 0, 255], [128, 128, 128, 128], [0, 0, 0, 40], [0, 0, 255, 255]].concat());

        // A hotspot in the transparent margin keeps the margin up to it.
        let wide = CursorImage::cropped(4, 3, (0, 0), &image()).unwrap();
        assert_eq!((wide.width(), wide.height(), wide.hotspot()), (3, 3, (0, 0)));
        assert_eq!(&wide.rgba[..4], &[0, 0, 0, 0]);

        // One past the image is clamped onto its last pixel.
        let clamped = CursorImage::cropped(4, 3, (9, -2), &image()).unwrap();
        assert_eq!((clamped.width(), clamped.height(), clamped.hotspot()), (3, 3, (2, 0)));

        // Nothing painted is no image at all.
        assert_eq!(CursorImage::cropped(4, 3, (1, 1), &[0u8; 4 * 3 * 4]), None);
        assert_eq!(CursorImage::cropped(0, 0, (0, 0), &[]), None);
    }

    #[test]
    fn a_client_reads_back_the_image_either_rectangle_carried() {
        let image = CursorImage::cropped(4, 3, (2, 1), &image()).unwrap();
        let wire = alpha_cursor_rect(Some(&image));
        assert_eq!(CursorImage::from_alpha_rect(2, 2, (1, 0), wire[16..].to_vec()), Some(image.clone()));
        assert_eq!(CursorImage::from_alpha_rect(0, 0, (0, 0), Vec::new()), None);
        // A hotspot outside the image is brought onto it, as the server's own is.
        assert_eq!(CursorImage::from_alpha_rect(1, 1, (7, 7), vec![1, 2, 3, 255]).unwrap().hotspot(), (0, 0));

        let format = PixelFormat { big_endian: true, red_shift: 24, green_shift: 16, blue_shift: 8, ..PixelFormat::NATIVE };
        let wire = cursor_rect(&format, Some(&image));
        let masked = CursorImage::from_masked_rect(&format, 2, 2, (1, 0), &wire[12..28], &wire[28..]).unwrap();
        // The mask kept red, the grey divided back out to white, and blue; the
        // shadow under the threshold is gone.
        assert_eq!(masked.rgba(), [[255, 0, 0, 255], [255, 255, 255, 255], [0, 0, 0, 0], [0, 0, 255, 255]].concat());
        assert_eq!(masked.hotspot(), (1, 0));
    }

    #[test]
    fn an_independent_decoder_reads_the_alpha_cursor_as_premultiplied_rgba() {
        let image = CursorImage::cropped(4, 3, (2, 1), &image()).unwrap();
        let wire = alpha_cursor_rect(Some(&image));

        assert_eq!(u16::from_be_bytes([wire[0], wire[1]]), 1, "hotspot x");
        assert_eq!(u16::from_be_bytes([wire[2], wire[3]]), 0, "hotspot y");
        assert_eq!(u16::from_be_bytes([wire[4], wire[5]]), 2);
        assert_eq!(u16::from_be_bytes([wire[6], wire[7]]), 2);
        assert_eq!(i32::from_be_bytes([wire[8], wire[9], wire[10], wire[11]]), -314);
        assert_eq!(i32::from_be_bytes([wire[12], wire[13], wire[14], wire[15]]), 0, "Raw");
        assert_eq!(&wire[16..], &[255, 0, 0, 255, 128, 128, 128, 128, 0, 0, 0, 40, 0, 0, 255, 255]);

        // No pointer: an empty rectangle, still with its encoding.
        let empty = alpha_cursor_rect(None);
        assert_eq!(empty.len(), 16);
        assert_eq!(&empty[..8], &[0; 8]);
        assert_eq!(i32::from_be_bytes([empty[8], empty[9], empty[10], empty[11]]), -314);
        assert_eq!(&empty[12..], &[0; 4]);
    }

    #[test]
    fn an_independent_decoder_reads_the_standard_cursor_as_straight_pixels_and_a_mask() {
        // This format puts colour in the high three bytes, big-endian. The
        // assertions decode the wire directly rather than reusing PixelFormat.
        let format = PixelFormat {
            big_endian: true,
            red_shift: 24,
            green_shift: 16,
            blue_shift: 8,
            ..PixelFormat::NATIVE
        };
        let image = CursorImage::cropped(4, 3, (1, 2), &image()).unwrap();
        let wire = cursor_rect(&format, Some(&image));

        assert_eq!(u16::from_be_bytes([wire[0], wire[1]]), 0, "hotspot x");
        assert_eq!(u16::from_be_bytes([wire[2], wire[3]]), 1, "hotspot y");
        assert_eq!(u16::from_be_bytes([wire[4], wire[5]]), 2);
        assert_eq!(u16::from_be_bytes([wire[6], wire[7]]), 2);
        assert_eq!(i32::from_be_bytes([wire[8], wire[9], wire[10], wire[11]]), -239);
        assert_eq!(wire.len(), 12 + 2 * 2 * 4 + 2);

        let pixels = &wire[12..28];
        // Red; the half-transparent grey divided back out to white; the faint
        // shadow under the threshold, sent black; blue.
        assert_eq!(&pixels[0..4], &[255, 0, 0, 0]);
        assert_eq!(&pixels[4..8], &[255, 255, 255, 0]);
        assert_eq!(&pixels[8..12], &[0, 0, 0, 0]);
        assert_eq!(&pixels[12..16], &[0, 0, 255, 0]);
        // Rows padded to whole bytes, MSB first: the shadow is left out.
        assert_eq!(&wire[28..], &[0b1100_0000, 0b0100_0000]);

        // No pointer: an empty rectangle and nothing after it.
        let empty = cursor_rect(&format, None);
        assert_eq!(empty.len(), 12);
        assert_eq!(&empty[..8], &[0; 8]);
        assert_eq!(i32::from_be_bytes([empty[8], empty[9], empty[10], empty[11]]), -239);
    }
}
