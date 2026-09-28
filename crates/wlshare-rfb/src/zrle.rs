//! The ZRLE encoder (RFC 6143 §7.7.6) with Raw beside it.
//!
//! A rectangle is cut into 64×64 tiles, left to right and top to bottom, the
//! right and bottom tiles shrinking to what is left. Each tile chooses one of
//! five shapes by estimating what each would cost and taking the smallest:
//!
//! | subencoding | shape | when it wins |
//! |---|---|---|
//! | 0 | raw CPIXELs | photographs, gradients |
//! | 1 | one CPIXEL | a flat area |
//! | 2–16 | palette, then 1/2/4-bit indices packed per row | UI with few colours |
//! | 128 | runs of CPIXEL + length | few colours, long runs |
//! | 130–255 | palette, then index-or-run per entry | text, most desktop content |
//!
//! The tiles of one rectangle are deflated together into one zlib stream that
//! the client keeps for the whole connection, flushed at every rectangle so the
//! client can decode without waiting for the next. One [`ZrleEncoder`] therefore
//! belongs to one client and is used in the order its rectangles are written.
//!
//! A CPIXEL is three bytes in every format this server accepts (the fourth byte
//! of a 32-bit pixel with 8-bit channels at byte-aligned shifts carries nothing),
//! so ZRLE spends 25% less than Raw before compression even on incompressible
//! content.

use flate2::{Compress, Compression, FlushCompress, Status};

use crate::pixel::PixelFormat;

/// The tile edge.
pub const TILE: usize = 64;
/// The largest palette a tile may carry: subencoding 255 is a palette of 127.
const MAX_PALETTE: usize = 127;

/// One client's ZRLE state: its zlib stream and the scratch space.
pub struct ZrleEncoder {
    deflate: Compress,
    /// The current tile's pixels as values in the client's format.
    tile: Vec<u32>,
    /// The rectangle's tiles, before compression.
    plain: Vec<u8>,
    palette: Palette,
}

impl Default for ZrleEncoder {
    fn default() -> Self {
        Self::new(Compression::fast())
    }
}

impl ZrleEncoder {
    /// A fresh stream at the given deflate level.
    pub fn new(level: Compression) -> Self {
        Self {
            deflate: Compress::new(level, true),
            tile: Vec::with_capacity(TILE * TILE),
            plain: Vec::new(),
            palette: Palette::default(),
        }
    }

    /// Append a ZRLE rectangle payload — the length word, then the compressed
    /// tiles — for the `width`×`height` pixels at `pixels`, whose rows are
    /// `stride` bytes apart, in the framebuffer's `B, G, R, X` layout.
    pub fn encode_rect(
        &mut self,
        pixels: &[u8],
        stride: usize,
        width: usize,
        height: usize,
        format: &PixelFormat,
        out: &mut Vec<u8>,
    ) {
        self.plain.clear();
        let cpixel = format.cpixel();
        for ty in (0..height).step_by(TILE) {
            let th = TILE.min(height - ty);
            for tx in (0..width).step_by(TILE) {
                let tw = TILE.min(width - tx);
                self.tile.clear();
                for row in 0..th {
                    let base = (ty + row) * stride + tx * 4;
                    for px in 0..tw {
                        let i = base + px * 4;
                        self.tile.push(format.value([pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]));
                    }
                }
                encode_tile(&self.tile, tw, th, format, cpixel, &mut self.palette, &mut self.plain);
            }
        }
        deflate_flush(&mut self.deflate, &self.plain, out);
    }
}

/// Deflate `input` onto `out` behind a big-endian length word, flushing so the
/// client can decode this rectangle without the next.
fn deflate_flush(deflate: &mut Compress, input: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    let mut consumed = 0usize;
    loop {
        out.reserve((input.len() - consumed) / 2 + 256);
        let before_in = deflate.total_in();
        let status = deflate
            .compress_vec(&input[consumed..], out, FlushCompress::Sync)
            .expect("deflate cannot fail on in-memory buffers");
        consumed += (deflate.total_in() - before_in) as usize;
        // The flush is complete once the input is gone and the output had room
        // to spare: a full buffer means deflate stopped for lack of space.
        if consumed == input.len() && out.len() < out.capacity() {
            break;
        }
        debug_assert!(!matches!(status, Status::StreamEnd));
    }
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_be_bytes());
}

/// Append a Raw rectangle: every pixel, in the client's format.
pub fn encode_raw_rect(pixels: &[u8], stride: usize, width: usize, height: usize, format: &PixelFormat, out: &mut Vec<u8>) {
    out.reserve(width * height * 4);
    for row in 0..height {
        let line = &pixels[row * stride..row * stride + width * 4];
        if format.is_native() {
            out.extend_from_slice(line);
        } else {
            for px in line.as_chunks::<4>().0 {
                out.extend_from_slice(&format.pixel_bytes(format.value(*px)));
            }
        }
    }
}

/// Which shape a tile takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Raw,
    Solid,
    Packed { bits: usize },
    PlainRle,
    PaletteRle,
}

/// How many bytes a run of `len` costs as a ZRLE run length: `len - 1` in
/// bytes of 255 with a final byte under 255.
fn run_length_bytes(len: usize) -> usize {
    (len - 1) / 255 + 1
}

fn write_run_length(mut remaining: usize, out: &mut Vec<u8>) {
    while remaining >= 255 {
        out.push(255);
        remaining -= 255;
    }
    out.push(remaining as u8);
}

/// Encode one tile onto `out`: the subencoding byte, then the shape's body.
fn encode_tile(
    tile: &[u32],
    width: usize,
    height: usize,
    format: &PixelFormat,
    (coff, clen): (usize, usize),
    palette: &mut Palette,
    out: &mut Vec<u8>,
) {
    let n = tile.len();
    debug_assert_eq!(n, width * height);
    let write_cpixel = |value: u32, out: &mut Vec<u8>| {
        let bytes = format.pixel_bytes(value);
        out.extend_from_slice(&bytes[coff..coff + clen]);
    };

    // One pass: the palette, and what each run-length shape would cost.
    palette.reset();
    let mut plain_rle = 0usize;
    let mut palette_rle = 0usize;
    let mut run = 0usize;
    let mut prev = tile[0];
    let account = |len: usize, plain_rle: &mut usize, palette_rle: &mut usize| {
        *plain_rle += clen + run_length_bytes(len);
        *palette_rle += if len == 1 { 1 } else { 1 + run_length_bytes(len) };
    };
    for &v in tile {
        palette.insert(v);
        if v == prev {
            run += 1;
        } else {
            account(run, &mut plain_rle, &mut palette_rle);
            prev = v;
            run = 1;
        }
    }
    account(run, &mut plain_rle, &mut palette_rle);

    let colours = palette.len();
    let mut best = (n * clen, Shape::Raw);
    if !palette.overflowed() {
        if colours == 1 {
            best = (clen, Shape::Solid);
        } else {
            if colours <= 16 {
                let bits = match colours {
                    2 => 1,
                    3..=4 => 2,
                    _ => 4,
                };
                let packed = colours * clen + height * (width * bits).div_ceil(8);
                if packed < best.0 {
                    best = (packed, Shape::Packed { bits });
                }
            }
            let cost = colours * clen + palette_rle;
            if cost < best.0 {
                best = (cost, Shape::PaletteRle);
            }
        }
    }
    if plain_rle < best.0 {
        best = (plain_rle, Shape::PlainRle);
    }

    match best.1 {
        Shape::Raw => {
            out.push(0);
            for &v in tile {
                write_cpixel(v, out);
            }
        }
        Shape::Solid => {
            out.push(1);
            write_cpixel(tile[0], out);
        }
        Shape::Packed { bits } => {
            out.push(colours as u8);
            for &c in palette.entries() {
                write_cpixel(c, out);
            }
            for row in tile.chunks_exact(width) {
                let mut byte = 0u8;
                let mut filled = 0;
                for &v in row {
                    byte = (byte << bits) | palette.index(v);
                    filled += bits;
                    if filled == 8 {
                        out.push(byte);
                        byte = 0;
                        filled = 0;
                    }
                }
                if filled > 0 {
                    out.push(byte << (8 - filled));
                }
            }
        }
        Shape::PlainRle => {
            out.push(128);
            for_each_run(tile, |v, len| {
                write_cpixel(v, out);
                write_run_length(len - 1, out);
            });
        }
        Shape::PaletteRle => {
            out.push(128 + colours as u8);
            for &c in palette.entries() {
                write_cpixel(c, out);
            }
            for_each_run(tile, |v, len| {
                let index = palette.index(v);
                if len == 1 {
                    out.push(index);
                } else {
                    out.push(index | 0x80);
                    write_run_length(len - 1, out);
                }
            });
        }
    }
}

fn for_each_run(tile: &[u32], mut f: impl FnMut(u32, usize)) {
    let mut prev = tile[0];
    let mut len = 0;
    for &v in tile {
        if v == prev {
            len += 1;
        } else {
            f(prev, len);
            prev = v;
            len = 1;
        }
    }
    f(prev, len);
}

/// A tile's distinct colours, up to [`MAX_PALETTE`], with an open-addressed
/// table so the lookup is one probe rather than a scan of the palette.
struct Palette {
    entries: Vec<u32>,
    /// Slot → palette index + 1; 0 is empty.
    slots: [u16; SLOTS],
    keys: [u32; SLOTS],
    overflowed: bool,
}

const SLOTS: usize = 512;

impl Default for Palette {
    fn default() -> Self {
        Self { entries: Vec::with_capacity(MAX_PALETTE), slots: [0; SLOTS], keys: [0; SLOTS], overflowed: false }
    }
}

impl Palette {
    fn reset(&mut self) {
        self.entries.clear();
        self.slots.fill(0);
        self.overflowed = false;
    }

    #[inline]
    fn slot(value: u32) -> usize {
        (value.wrapping_mul(0x9E37_79B9) >> 23) as usize
    }

    #[inline]
    fn insert(&mut self, value: u32) {
        if self.overflowed {
            return;
        }
        let mut i = Self::slot(value);
        loop {
            let s = self.slots[i];
            if s == 0 {
                if self.entries.len() == MAX_PALETTE {
                    self.overflowed = true;
                    return;
                }
                self.entries.push(value);
                self.slots[i] = self.entries.len() as u16;
                self.keys[i] = value;
                return;
            }
            if self.keys[i] == value {
                return;
            }
            i = (i + 1) % SLOTS;
        }
    }

    #[inline]
    fn index(&self, value: u32) -> u8 {
        let mut i = Self::slot(value);
        loop {
            debug_assert!(self.slots[i] != 0, "a colour outside the palette");
            if self.keys[i] == value {
                return (self.slots[i] - 1) as u8;
            }
            i = (i + 1) % SLOTS;
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn entries(&self) -> &[u32] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Decompress, FlushDecompress};

    /// An independent ZRLE decoder, written from RFC 6143 §7.7.6 rather than
    /// from the encoder, that keeps one inflate stream across rectangles as a
    /// client does.
    struct Decoder {
        inflate: Decompress,
    }

    impl Decoder {
        fn new() -> Self {
            Self { inflate: Decompress::new(true) }
        }

        /// Decode one rectangle payload (length word included) to pixel values.
        fn rect(&mut self, payload: &[u8], width: usize, height: usize, format: &PixelFormat) -> Vec<u32> {
            let len = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
            assert_eq!(payload.len(), 4 + len, "the length word covers the payload");
            let mut plain = Vec::new();
            let mut consumed = 0;
            while consumed < len {
                plain.reserve(4096);
                let before = self.inflate.total_in();
                self.inflate.decompress_vec(&payload[4 + consumed..], &mut plain, FlushDecompress::Sync).unwrap();
                consumed += (self.inflate.total_in() - before) as usize;
            }
            let (coff, clen) = format.cpixel();
            let read_cpixel = |bytes: &mut &[u8]| -> u32 {
                let mut pixel = [0u8; 4];
                pixel[coff..coff + clen].copy_from_slice(&bytes[..clen]);
                *bytes = &bytes[clen..];
                if format.big_endian { u32::from_be_bytes(pixel) } else { u32::from_le_bytes(pixel) }
            };
            let read_run = |bytes: &mut &[u8]| -> usize {
                let mut len = 1usize;
                loop {
                    let b = bytes[0];
                    *bytes = &bytes[1..];
                    len += usize::from(b);
                    if b < 255 {
                        return len;
                    }
                }
            };
            let mut out = vec![0u32; width * height];
            let mut bytes = &plain[..];
            for ty in (0..height).step_by(TILE) {
                let th = TILE.min(height - ty);
                for tx in (0..width).step_by(TILE) {
                    let tw = TILE.min(width - tx);
                    let mut tile = Vec::with_capacity(tw * th);
                    let sub = bytes[0];
                    bytes = &bytes[1..];
                    match sub {
                        0 => {
                            for _ in 0..tw * th {
                                tile.push(read_cpixel(&mut bytes));
                            }
                        }
                        1 => {
                            let c = read_cpixel(&mut bytes);
                            tile.resize(tw * th, c);
                        }
                        2..=16 => {
                            let palette: Vec<u32> = (0..sub).map(|_| read_cpixel(&mut bytes)).collect();
                            let bits = match sub {
                                2 => 1,
                                3..=4 => 2,
                                _ => 4,
                            };
                            for _ in 0..th {
                                let row_bytes = (tw * bits).div_ceil(8);
                                let row = &bytes[..row_bytes];
                                bytes = &bytes[row_bytes..];
                                for x in 0..tw {
                                    let bit = x * bits;
                                    let byte = row[bit / 8];
                                    let shift = 8 - bits - (bit % 8);
                                    let idx = (usize::from(byte) >> shift) & ((1 << bits) - 1);
                                    tile.push(palette[idx]);
                                }
                            }
                        }
                        128 => {
                            while tile.len() < tw * th {
                                let c = read_cpixel(&mut bytes);
                                let len = read_run(&mut bytes);
                                tile.extend(std::iter::repeat_n(c, len));
                            }
                        }
                        130..=255 => {
                            let palette: Vec<u32> = (0..sub - 128).map(|_| read_cpixel(&mut bytes)).collect();
                            while tile.len() < tw * th {
                                let b = bytes[0];
                                bytes = &bytes[1..];
                                let idx = usize::from(b & 0x7F);
                                let len = if b & 0x80 != 0 { read_run(&mut bytes) } else { 1 };
                                tile.extend(std::iter::repeat_n(palette[idx], len));
                            }
                        }
                        other => panic!("subencoding {other} is not defined"),
                    }
                    assert_eq!(tile.len(), tw * th, "a tile of exactly its pixels");
                    for (row, chunk) in tile.chunks_exact(tw).enumerate() {
                        out[(ty + row) * width + tx..(ty + row) * width + tx + tw].copy_from_slice(chunk);
                    }
                }
            }
            assert!(bytes.is_empty(), "no bytes after the last tile");
            out
        }
    }

    /// A framebuffer of `w`×`h` from a colour function.
    fn framebuffer(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut fb = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = f(x, y);
                fb.extend_from_slice(&[b, g, r, 0]);
            }
        }
        fb
    }

    fn expected(fb: &[u8], format: &PixelFormat) -> Vec<u32> {
        fb.as_chunks::<4>().0.iter().map(|p| format.value(*p)).collect()
    }

    fn round_trip(w: usize, h: usize, format: PixelFormat, f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let fb = framebuffer(w, h, f);
        let mut enc = ZrleEncoder::default();
        let mut out = Vec::new();
        enc.encode_rect(&fb, w * 4, w, h, &format, &mut out);
        let got = Decoder::new().rect(&out, w, h, &format);
        assert_eq!(got, expected(&fb, &format));
        out
    }

    /// The subencoding bytes the encoder chose, tile by tile.
    fn subencodings(fb: &[u8], w: usize, h: usize, format: &PixelFormat) -> Vec<u8> {
        let mut enc = ZrleEncoder::default();
        enc.plain.clear();
        let cpixel = format.cpixel();
        let mut subs = Vec::new();
        for ty in (0..h).step_by(TILE) {
            let th = TILE.min(h - ty);
            for tx in (0..w).step_by(TILE) {
                let tw = TILE.min(w - tx);
                enc.tile.clear();
                for row in 0..th {
                    for px in 0..tw {
                        let i = (ty + row) * w * 4 + (tx + px) * 4;
                        enc.tile.push(format.value([fb[i], fb[i + 1], fb[i + 2], fb[i + 3]]));
                    }
                }
                let mut tile_bytes = Vec::new();
                encode_tile(&enc.tile, tw, th, format, cpixel, &mut enc.palette, &mut tile_bytes);
                subs.push(tile_bytes[0]);
            }
        }
        subs
    }

    #[test]
    fn a_flat_rectangle_is_one_solid_tile_each() {
        let fb = framebuffer(130, 70, |_, _| [0x11, 0x22, 0x33]);
        assert_eq!(subencodings(&fb, 130, 70, &PixelFormat::NATIVE), vec![1, 1, 1, 1, 1, 1]);
        round_trip(130, 70, PixelFormat::NATIVE, |_, _| [0x11, 0x22, 0x33]);
    }

    #[test]
    fn two_colours_in_columns_pack_to_one_bit() {
        let f = |x: usize, _| if x.is_multiple_of(2) { [255, 0, 0] } else { [0, 0, 255] };
        let fb = framebuffer(64, 64, f);
        // Alternating columns have no runs, so RLE loses and packing wins.
        assert_eq!(subencodings(&fb, 64, 64, &PixelFormat::NATIVE), vec![2]);
        round_trip(64, 64, PixelFormat::NATIVE, f);
    }

    #[test]
    fn a_few_colours_in_rows_become_palette_rle() {
        let f = |_, y: usize| [(y % 5) as u8 * 40, 7, 9];
        let fb = framebuffer(64, 64, f);
        assert_eq!(subencodings(&fb, 64, 64, &PixelFormat::NATIVE), vec![128 + 5]);
        round_trip(64, 64, PixelFormat::NATIVE, f);
    }

    #[test]
    fn long_runs_of_many_colours_become_plain_rle() {
        // Each row its own colour: 64 runs of 64, 64 colours — the palette would
        // cost 64 CPIXELs on top of the run bytes, so plain RLE is cheaper.
        let f = |_, y: usize| [y as u8, (y * 3) as u8, (y * 7) as u8];
        let fb = framebuffer(64, 64, f);
        assert_eq!(subencodings(&fb, 64, 64, &PixelFormat::NATIVE), vec![128]);
        round_trip(64, 64, PixelFormat::NATIVE, f);
    }

    #[test]
    fn a_gradient_is_raw() {
        let f = |x: usize, y: usize| [x as u8, y as u8, (x ^ y) as u8];
        let fb = framebuffer(64, 64, f);
        assert_eq!(subencodings(&fb, 64, 64, &PixelFormat::NATIVE), vec![0]);
        round_trip(64, 64, PixelFormat::NATIVE, f);
    }

    #[test]
    fn a_run_longer_than_255_needs_more_than_one_length_byte() {
        // 4096 pixels in one tile as two runs of 2048. Plain RLE spends two
        // CPIXELs and two nine-byte lengths; a palette would add its two entries
        // to the same lengths, so plain wins.
        let f = |_, y: usize| if y < 32 { [1, 2, 3] } else { [4, 5, 6] };
        let fb = framebuffer(64, 64, f);
        assert_eq!(subencodings(&fb, 64, 64, &PixelFormat::NATIVE), vec![128]);
        round_trip(64, 64, PixelFormat::NATIVE, f);
        assert_eq!(run_length_bytes(1), 1);
        assert_eq!(run_length_bytes(255), 1);
        assert_eq!(run_length_bytes(256), 2);
        assert_eq!(run_length_bytes(2048), 9);
    }

    #[test]
    fn odd_sizes_cut_partial_tiles_at_the_right_and_bottom() {
        round_trip(133, 67, PixelFormat::NATIVE, |x, y| [(x / 7) as u8, (y / 3) as u8, ((x + y) % 3) as u8 * 100]);
        round_trip(1, 1, PixelFormat::NATIVE, |_, _| [9, 8, 7]);
        round_trip(65, 1, PixelFormat::NATIVE, |x, _| if x == 64 { [1, 1, 1] } else { [2, 2, 2] });
    }

    #[test]
    fn packed_rows_pad_to_a_byte_each() {
        // 3 pixels wide, 2 colours: each row is one byte with 5 padding bits.
        round_trip(3, 5, PixelFormat::NATIVE, |x, y| if (x + y).is_multiple_of(2) { [1, 0, 0] } else { [0, 1, 0] });
        // 5 wide, 4 colours: 10 bits a row, two bytes.
        round_trip(5, 5, PixelFormat::NATIVE, |x, y| [((x * y) % 4) as u8, 0, 0]);
        // 3 wide, 9 colours: 12 bits a row, two bytes.
        round_trip(3, 3, PixelFormat::NATIVE, |x, y| [(x * 3 + y) as u8, 0, 0]);
    }

    #[test]
    fn the_stream_continues_across_rectangles() {
        let fb = framebuffer(200, 100, |x, y| [(x / 20) as u8, (y / 10) as u8, 0]);
        let format = PixelFormat::NATIVE;
        let mut enc = ZrleEncoder::default();
        let mut dec = Decoder::new();
        // Two rectangles out of one framebuffer, the second overlapping the first,
        // each a separate payload on the same stream.
        for (x, y, w, h) in [(0usize, 0usize, 200usize, 50usize), (100, 25, 100, 75)] {
            let mut out = Vec::new();
            enc.encode_rect(&fb[y * 800 + x * 4..], 800, w, h, &format, &mut out);
            let got = dec.rect(&out, w, h, &format);
            let want: Vec<u32> = (0..h)
                .flat_map(|r| (0..w).map(move |c| ((y + r) * 200 + x + c) * 4))
                .map(|i| format.value([fb[i], fb[i + 1], fb[i + 2], fb[i + 3]]))
                .collect();
            assert_eq!(got, want);
        }
    }

    #[test]
    fn other_client_formats_are_honoured_in_every_shape() {
        let formats = [
            PixelFormat { big_endian: true, ..PixelFormat::NATIVE },
            PixelFormat { red_shift: 0, green_shift: 8, blue_shift: 16, ..PixelFormat::NATIVE },
            PixelFormat { red_shift: 24, green_shift: 16, blue_shift: 8, ..PixelFormat::NATIVE },
            PixelFormat { red_shift: 24, green_shift: 16, blue_shift: 8, big_endian: true, ..PixelFormat::NATIVE },
        ];
        for format in formats {
            format.check().unwrap();
            round_trip(70, 70, format, |_, _| [1, 2, 3]);
            round_trip(70, 70, format, |x, _| if x.is_multiple_of(2) { [255, 0, 0] } else { [0, 0, 255] });
            round_trip(70, 70, format, |_, y| [(y % 5) as u8 * 40, 7, 9]);
            round_trip(70, 70, format, |_, y| [y as u8, (y * 3) as u8, (y * 7) as u8]);
            round_trip(70, 70, format, |x, y| [x as u8, y as u8, (x ^ y) as u8]);
        }
    }

    #[test]
    fn raw_is_the_framebuffer_in_the_clients_order() {
        let fb = framebuffer(3, 2, |x, y| [x as u8, y as u8, 200]);
        let mut out = Vec::new();
        encode_raw_rect(&fb, 12, 3, 2, &PixelFormat::NATIVE, &mut out);
        assert_eq!(out, fb);
        let mut out = Vec::new();
        let rgbx = PixelFormat { red_shift: 0, green_shift: 8, blue_shift: 16, ..PixelFormat::NATIVE };
        encode_raw_rect(&fb, 12, 3, 2, &rgbx, &mut out);
        assert_eq!(&out[..4], &[0, 0, 200, 0]);
        assert_eq!(&out[4..8], &[1, 0, 200, 0]);
        // A stride wider than the row skips the pixels outside the rectangle.
        let mut out = Vec::new();
        encode_raw_rect(&fb[4..], 12, 1, 2, &PixelFormat::NATIVE, &mut out);
        assert_eq!(out, [&fb[4..8], &fb[16..20]].concat());
    }

    #[test]
    fn a_palette_overflow_falls_back_to_raw_or_plain_rle() {
        // 128 colours in one tile, one per two rows: too many for a palette, so
        // the choice is between raw and plain RLE, and the runs make RLE win.
        let f = |_, y: usize| [(y * 2) as u8, 0, 0];
        let fb = framebuffer(64, 64, f);
        assert_eq!(subencodings(&fb, 64, 64, &PixelFormat::NATIVE), vec![128]);
        round_trip(64, 64, PixelFormat::NATIVE, f);
        // Hundreds of colours with no runs: raw.
        let g = |x: usize, y: usize| [(x + y) as u8, x as u8, 0];
        let fb = framebuffer(64, 64, g);
        assert_eq!(subencodings(&fb, 64, 64, &PixelFormat::NATIVE), vec![0]);
        round_trip(64, 64, PixelFormat::NATIVE, g);
    }
}
