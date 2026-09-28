//! The client's pixel format, and the server's native one.
//!
//! The compositor hands over XRGB8888 in memory order `B, G, R, X`, which as an
//! RFB pixel format is 32 bits per pixel, depth 24, little-endian, red shifted
//! 16, green 8, blue 0 — [`PixelFormat::NATIVE`]. A client that asks for exactly
//! that gets the framebuffer's bytes as they are. Any other 32-bit true-colour
//! format with 8-bit channels is produced by [`PixelFormat::value`], which builds
//! the pixel's integer from the three channels, and [`PixelFormat::pixel_bytes`],
//! which lays it out in the client's byte order. Nothing else is supported:
//! 8- and 16-bit clients and colour maps are refused at `SetPixelFormat`.

use thiserror::Error;

/// An RFB PIXEL_FORMAT (RFC 6143 §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

/// Why a client's format cannot be produced.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PixelFormatError {
    #[error("{0} bits per pixel; only 32 is supported")]
    BitsPerPixel(u8),
    #[error("a colour map was asked for; only true colour is supported")]
    ColourMap,
    #[error("channel maxima {0}/{1}/{2}; only 8-bit channels (255) are supported")]
    ChannelMax(u16, u16, u16),
    #[error("channel shift {0} puts a channel past 32 bits")]
    Shift(u8),
    #[error("channels at shifts {0}/{1}/{2} overlap")]
    Overlap(u8, u8, u8),
}

impl PixelFormat {
    /// The framebuffer's own format: XRGB8888 in memory, `B, G, R, X`.
    pub const NATIVE: Self = Self {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    /// Read the 16 bytes of a PIXEL_FORMAT.
    pub fn parse(b: &[u8; 16]) -> Self {
        Self {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_colour: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        }
    }

    /// The 16 bytes of a PIXEL_FORMAT, padding included.
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = self.bits_per_pixel;
        b[1] = self.depth;
        b[2] = u8::from(self.big_endian);
        b[3] = u8::from(self.true_colour);
        b[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        b[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        b[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        b[10] = self.red_shift;
        b[11] = self.green_shift;
        b[12] = self.blue_shift;
        b
    }

    /// Whether this server can produce the format.
    pub fn check(&self) -> Result<(), PixelFormatError> {
        if self.bits_per_pixel != 32 {
            return Err(PixelFormatError::BitsPerPixel(self.bits_per_pixel));
        }
        if !self.true_colour {
            return Err(PixelFormatError::ColourMap);
        }
        if (self.red_max, self.green_max, self.blue_max) != (255, 255, 255) {
            return Err(PixelFormatError::ChannelMax(self.red_max, self.green_max, self.blue_max));
        }
        for shift in [self.red_shift, self.green_shift, self.blue_shift] {
            if shift > 24 {
                return Err(PixelFormatError::Shift(shift));
            }
        }
        let masks = [self.red_shift, self.green_shift, self.blue_shift].map(|s| 0xFFu32 << s);
        if masks[0] & masks[1] != 0 || masks[0] & masks[2] != 0 || masks[1] & masks[2] != 0 {
            return Err(PixelFormatError::Overlap(self.red_shift, self.green_shift, self.blue_shift));
        }
        Ok(())
    }

    /// Whether the framebuffer's bytes can go out untouched.
    pub fn is_native(&self) -> bool {
        // The pad byte and the depth do not change the pixels a client sees.
        self.bits_per_pixel == 32
            && self.true_colour
            && !self.big_endian
            && (self.red_max, self.green_max, self.blue_max) == (255, 255, 255)
            && (self.red_shift, self.green_shift, self.blue_shift) == (16, 8, 0)
    }

    /// The pixel's integer value in this format, from a framebuffer pixel.
    #[inline]
    pub fn value(&self, bgrx: [u8; 4]) -> u32 {
        let [b, g, r, _] = bgrx;
        (u32::from(r) << self.red_shift) | (u32::from(g) << self.green_shift) | (u32::from(b) << self.blue_shift)
    }

    /// The framebuffer pixel a value in this format stands for: [`Self::value`]
    /// the other way, which is a client's way.
    #[cfg(test)]
    pub(crate) fn bgrx(&self, value: u32) -> [u8; 4] {
        [(value >> self.blue_shift) as u8, (value >> self.green_shift) as u8, (value >> self.red_shift) as u8, 0]
    }

    /// The value four PIXEL bytes hold, in this format's byte order.
    #[cfg(test)]
    pub(crate) fn pixel_value(&self, bytes: [u8; 4]) -> u32 {
        if self.big_endian { u32::from_be_bytes(bytes) } else { u32::from_le_bytes(bytes) }
    }

    /// A PIXEL's four bytes for a value, in this format's byte order.
    #[inline]
    pub fn pixel_bytes(&self, value: u32) -> [u8; 4] {
        if self.big_endian { value.to_be_bytes() } else { value.to_le_bytes() }
    }

    /// Where ZRLE's CPIXEL sits inside [`Self::pixel_bytes`]: `(offset, length)`.
    ///
    /// RFC 6143 §7.7.6: with 32 bits per pixel and a depth of 24 or less, when all
    /// colour bits fit in the least or the most significant three bytes, CPIXEL is
    /// those three bytes in PIXEL order. Otherwise it is the whole PIXEL.
    pub fn cpixel(&self) -> (usize, usize) {
        if self.bits_per_pixel != 32 || self.depth > 24 {
            return (0, 4);
        }
        let mask = (0xFFu32 << self.red_shift) | (0xFFu32 << self.green_shift) | (0xFFu32 << self.blue_shift);
        if mask & 0xFF00_0000 == 0 {
            // Least significant three bytes.
            if self.big_endian { (1, 3) } else { (0, 3) }
        } else if mask & 0xFF == 0 {
            // Most significant three bytes.
            if self.big_endian { (0, 3) } else { (1, 3) }
        } else {
            (0, 4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_format_round_trips_through_its_bytes() {
        let f = PixelFormat::NATIVE;
        assert_eq!(PixelFormat::parse(&f.to_bytes()), f);
        assert!(f.is_native());
        assert_eq!(f.cpixel(), (0, 3));
        // A framebuffer pixel B=0x11 G=0x22 R=0x33 is the value 0x332211 and
        // goes out as the same bytes it came in with.
        let v = f.value([0x11, 0x22, 0x33, 0xFF]);
        assert_eq!(v, 0x0033_2211);
        assert_eq!(f.pixel_bytes(v), [0x11, 0x22, 0x33, 0x00]);
    }

    #[test]
    fn a_big_endian_rgbx_format_is_swizzled_and_its_cpixel_is_the_low_bytes() {
        let f = PixelFormat { big_endian: true, ..PixelFormat::NATIVE };
        assert!(!f.is_native());
        f.check().unwrap();
        assert_eq!(f.pixel_bytes(f.value([0x11, 0x22, 0x33, 0])), [0x00, 0x33, 0x22, 0x11]);
        assert_eq!(f.bgrx(f.pixel_value([0x00, 0x33, 0x22, 0x11])), [0x11, 0x22, 0x33, 0]);
        assert_eq!(f.cpixel(), (1, 3));
    }

    #[test]
    fn a_format_with_a_channel_in_the_top_byte_uses_the_high_cpixel() {
        let f = PixelFormat { red_shift: 24, green_shift: 16, blue_shift: 8, ..PixelFormat::NATIVE };
        f.check().unwrap();
        assert_eq!(f.cpixel(), (1, 3));
        assert_eq!(PixelFormat { big_endian: true, ..f }.cpixel(), (0, 3));
    }

    #[test]
    fn unsupported_formats_are_named() {
        assert_eq!(
            PixelFormat { bits_per_pixel: 16, ..PixelFormat::NATIVE }.check(),
            Err(PixelFormatError::BitsPerPixel(16))
        );
        assert_eq!(PixelFormat { true_colour: false, ..PixelFormat::NATIVE }.check(), Err(PixelFormatError::ColourMap));
        assert_eq!(
            PixelFormat { red_max: 31, ..PixelFormat::NATIVE }.check(),
            Err(PixelFormatError::ChannelMax(31, 255, 255))
        );
        assert_eq!(PixelFormat { red_shift: 25, ..PixelFormat::NATIVE }.check(), Err(PixelFormatError::Shift(25)));
        assert_eq!(
            PixelFormat { red_shift: 4, ..PixelFormat::NATIVE }.check(),
            Err(PixelFormatError::Overlap(4, 8, 0))
        );
    }
}
