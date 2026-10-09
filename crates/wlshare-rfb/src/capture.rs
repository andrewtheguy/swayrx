//! The capture file: what a session handed its VP9 encoder, frame by frame
//! and exact, so that an encoder can be run again on the pictures a desktop
//! was actually shown as and the rectangles they came with.
//!
//! Not the wire: no client is ever sent this. It is here because the daemon
//! writes it and the tools that play it back read it (`examples/vp9cap.rs`),
//! and this crate is the one both build against, on any machine. The format
//! is this module's alone and owes nothing to the codec: a capture is the
//! pixels in front of an encoder, whichever encoder that is.
//!
//! Everything is little-endian. The file is a magic and then frames until it
//! ends:
//!
//! ```text
//! u8[8]   "WLSHCAP1"
//! frame*
//! ```
//!
//! ```text
//! u64     microseconds since the capture's first frame
//! u16     width
//! u16     height
//! u8      quality     the 1..100 dial the encoder was at
//! u8      flags       bit 0: a keyframe was asked
//!                     bit 1: rectangles follow; without it the frame is the
//!                            whole picture
//! u16     count       the rectangles, 0 for a whole picture
//! rect[count]         u16 x, y, width, height: where the picture changed
//! rows                `B, G, R, X`, each row the picture's width
//! ```
//!
//! A whole picture's rows are all of them, top to bottom. A frame of
//! rectangles carries only the rows its rectangles span, each row once and
//! whole, top to bottom ([`rows`]): what an encoder told where the picture
//! changed reads of it, and so what the daemon holds of it. The rest of the
//! picture is the frames before, which is why a frame of rectangles is only
//! ever read after a whole picture of its size.
//!
//! Nothing is compressed here; a 4K whole picture is 33 MB. `zstd` over the
//! file afterwards is what makes a capture keepable.

use std::io::{self, Read, Write};
use std::ops::Range;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::vp9::Rect;

/// What every capture file begins with.
pub const MAGIC: &[u8; 8] = b"WLSHCAP1";

const FLAG_KEYFRAME: u8 = 1;
const FLAG_RECTS: u8 = 2;
/// A frame's fixed fields, before its rectangles.
const HEADER: usize = 16;

/// Why a capture could not be written or read.
#[derive(Debug, Error)]
pub enum CaptureError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("not a capture: the file does not begin with {}", String::from_utf8_lossy(MAGIC))]
    Magic,
    #[error("the capture ends inside frame {0}")]
    Truncated(u64),
    #[error("frame {0} has flags {1:#04x}")]
    Flags(u64, u8),
    #[error("frame {0} is {1}x{2} and has no pixels")]
    Empty(u64, u16, u16),
    #[error("frame {0} is rectangles of a {1}x{2} picture no frame before it holds")]
    NoPicture(u64, u16, u16),
    #[error("a {0}x{1} frame was handed {2} bytes of pixels at a stride of {3}")]
    Pixels(u16, u16, usize, usize),
    #[error("a frame of {0} rectangles is more than the file counts")]
    Rects(usize),
}

/// The rows the rectangles span of a picture `height` high, each once, top to
/// bottom, as ranges that neither touch nor overlap: the rows a frame of
/// rectangles carries. What lies past the picture's edge is not part of it.
pub fn rows(rects: &[Rect], height: u16) -> Vec<Range<u16>> {
    let mut spans: Vec<Range<u16>> = rects
        .iter()
        .map(|r| r.y.min(height)..r.y.saturating_add(r.height).min(height))
        .filter(|span| !span.is_empty())
        .collect();
    spans.sort_by_key(|span| span.start);
    let mut merged: Vec<Range<u16>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// A capture being written: the magic at once, then a frame for every call.
pub struct Writer<W: Write> {
    out: W,
    /// When the first frame was written, which every frame's time counts from.
    began: Option<Instant>,
}

impl<W: Write> Writer<W> {
    /// Begin a capture on `out`.
    pub fn new(mut out: W) -> Result<Self, CaptureError> {
        out.write_all(MAGIC)?;
        Ok(Self { out, began: None })
    }

    /// Write the frame an encoder is being handed now: a `size` picture of
    /// `B, G, R, X` in `pixels`, rows `stride` bytes apart, at the dial
    /// `quality`, with a keyframe asked or not. `changed` is where the
    /// picture differs from the one before, and then only the rows the
    /// rectangles span are read from `pixels`; `None` is a whole picture.
    pub fn frame(&mut self, size: (u16, u16), quality: u8, keyframe: bool, changed: Option<&[Rect]>, pixels: &[u8], stride: usize) -> Result<(), CaptureError> {
        let at = self.began.get_or_insert_with(Instant::now).elapsed();
        self.frame_at(at, size, quality, keyframe, changed, pixels, stride)
    }

    /// [`Self::frame`] at a time given, counted from the capture's first.
    #[allow(clippy::too_many_arguments)]
    pub fn frame_at(&mut self, at: Duration, size: (u16, u16), quality: u8, keyframe: bool, changed: Option<&[Rect]>, pixels: &[u8], stride: usize) -> Result<(), CaptureError> {
        let (width, height) = size;
        let row = usize::from(width) * 4;
        if width == 0 || height == 0 || stride < row || pixels.len() < (usize::from(height) - 1) * stride + row {
            return Err(CaptureError::Pixels(width, height, pixels.len(), stride));
        }
        let count = changed.map_or(Ok(0), |rects| u16::try_from(rects.len()).map_err(|_| CaptureError::Rects(rects.len())))?;
        let mut flags = 0;
        if keyframe {
            flags |= FLAG_KEYFRAME;
        }
        if changed.is_some() {
            flags |= FLAG_RECTS;
        }
        let mut header = Vec::with_capacity(HEADER + usize::from(count) * 8);
        header.extend_from_slice(&(at.as_micros() as u64).to_le_bytes());
        header.extend_from_slice(&width.to_le_bytes());
        header.extend_from_slice(&height.to_le_bytes());
        header.extend_from_slice(&[quality, flags]);
        header.extend_from_slice(&count.to_le_bytes());
        for rect in changed.unwrap_or_default() {
            for field in [rect.x, rect.y, rect.width, rect.height] {
                header.extend_from_slice(&field.to_le_bytes());
            }
        }
        self.out.write_all(&header)?;
        let spans = match changed {
            Some(rects) => rows(rects, height),
            None => std::iter::once(0..height).collect(),
        };
        for y in spans.into_iter().flatten() {
            let from = usize::from(y) * stride;
            self.out.write_all(&pixels[from..from + row])?;
        }
        Ok(())
    }

    /// Write out what is buffered.
    pub fn flush(&mut self) -> Result<(), CaptureError> {
        Ok(self.out.flush()?)
    }
}

/// One frame of a capture, as [`Reader::next`] hands it on.
#[derive(Debug)]
pub struct Frame<'a> {
    /// The frame's place in the capture, from 0.
    pub index: u64,
    /// When the encoder was handed it, counted from the capture's first.
    pub at: Duration,
    pub size: (u16, u16),
    /// The dial the encoder was at.
    pub quality: u8,
    /// A keyframe was asked.
    pub keyframe: bool,
    /// Where the picture differs from the frame before, or `None` for a
    /// picture that may differ anywhere.
    pub changed: Option<&'a [Rect]>,
    /// The whole picture as it stands with this frame, `B, G, R, X`, rows
    /// tight at the width: the frames before with this one's rows over them.
    pub pixels: &'a [u8],
}

impl Frame<'_> {
    /// How many pixels the frame says changed: the picture for a whole one,
    /// and otherwise what its rectangles cover, counted once where they
    /// overlap.
    pub fn changed_pixels(&self) -> u64 {
        let (width, height) = self.size;
        let Some(rects) = self.changed else {
            return u64::from(width) * u64::from(height);
        };
        // Row by row over the spans, each row's columns merged as the rows are.
        let mut total = 0;
        let mut columns: Vec<Range<u16>> = Vec::new();
        for y in rows(rects, height).into_iter().flatten() {
            columns.clear();
            columns.extend(
                rects
                    .iter()
                    .filter(|r| r.y <= y && y < r.y.saturating_add(r.height))
                    .map(|r| r.x.min(width)..r.x.saturating_add(r.width).min(width))
                    .filter(|span| !span.is_empty()),
            );
            columns.sort_by_key(|span| span.start);
            let mut end = 0;
            for span in &columns {
                total += u64::from(span.end.max(end) - span.start.max(end));
                end = end.max(span.end);
            }
        }
        total
    }
}

/// A capture being read, which keeps the picture: every frame is handed on
/// as the whole picture it left, whatever part of it the file carried.
pub struct Reader<R: Read> {
    input: R,
    index: u64,
    size: (u16, u16),
    pixels: Vec<u8>,
    rects: Vec<Rect>,
}

impl<R: Read> Reader<R> {
    /// Begin reading a capture, which must open with the magic.
    pub fn new(mut input: R) -> Result<Self, CaptureError> {
        let mut magic = [0; MAGIC.len()];
        match input.read_exact(&mut magic) {
            Ok(()) if &magic == MAGIC => {}
            Ok(()) => return Err(CaptureError::Magic),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(CaptureError::Magic),
            Err(e) => return Err(e.into()),
        }
        Ok(Self { input, index: 0, size: (0, 0), pixels: Vec::new(), rects: Vec::new() })
    }

    /// The next frame, or `None` at the end of the capture. A capture that
    /// ends inside a frame is an error, not an end.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Frame<'_>>, CaptureError> {
        let index = self.index;
        let mut header = [0; HEADER];
        // The one place a capture may end: before a frame's first byte.
        let mut filled = 0;
        while filled < HEADER {
            match self.input.read(&mut header[filled..]) {
                Ok(0) if filled == 0 => return Ok(None),
                Ok(0) => return Err(CaptureError::Truncated(index)),
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let u16_at = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
        let at = Duration::from_micros(u64::from_le_bytes(header[..8].try_into().expect("eight bytes")));
        let (width, height, quality, flags, count) = (u16_at(8), u16_at(10), header[12], header[13], u16_at(14));
        if flags & !(FLAG_KEYFRAME | FLAG_RECTS) != 0 || (flags & FLAG_RECTS == 0 && count != 0) {
            return Err(CaptureError::Flags(index, flags));
        }
        if width == 0 || height == 0 {
            return Err(CaptureError::Empty(index, width, height));
        }
        let truncated = |e: io::Error| if e.kind() == io::ErrorKind::UnexpectedEof { CaptureError::Truncated(index) } else { e.into() };
        self.rects.clear();
        for _ in 0..count {
            let mut rect = [0; 8];
            self.input.read_exact(&mut rect).map_err(truncated)?;
            let field = |at: usize| u16::from_le_bytes([rect[at], rect[at + 1]]);
            self.rects.push(Rect { x: field(0), y: field(2), width: field(4), height: field(6) });
        }
        let row = usize::from(width) * 4;
        let spans = if flags & FLAG_RECTS == 0 {
            if self.size != (width, height) {
                self.size = (width, height);
                self.pixels.clear();
                self.pixels.resize(row * usize::from(height), 0);
            }
            std::iter::once(0..height).collect()
        } else {
            if self.size != (width, height) {
                return Err(CaptureError::NoPicture(index, width, height));
            }
            rows(&self.rects, height)
        };
        for span in spans {
            self.input.read_exact(&mut self.pixels[usize::from(span.start) * row..usize::from(span.end) * row]).map_err(truncated)?;
        }
        self.index += 1;
        Ok(Some(Frame {
            index,
            at,
            size: (width, height),
            quality,
            keyframe: flags & FLAG_KEYFRAME != 0,
            changed: (flags & FLAG_RECTS != 0).then_some(&self.rects[..]),
            pixels: &self.pixels,
        }))
    }
}

/// The writer read back twice: by a parser written here from the layout
/// above, which shares nothing with [`Reader`], and by the reader, whose
/// picture must be the one the frames were cut from.
#[cfg(test)]
mod tests {
    use super::*;

    /// A picture whose every pixel says where it is and which frame drew it.
    fn picture(width: u16, height: u16, stride: usize, frame: u8) -> Vec<u8> {
        let mut pixels = vec![0xee; stride * usize::from(height)];
        for y in 0..usize::from(height) {
            for x in 0..usize::from(width) {
                pixels[y * stride + x * 4..][..4].copy_from_slice(&[x as u8, y as u8, frame, 0xff]);
            }
        }
        pixels
    }

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect { x, y, width, height }
    }

    /// The rows of `pixels` in `span`, tight.
    fn tight(pixels: &[u8], stride: usize, width: u16, span: Range<u16>) -> Vec<u8> {
        span.flat_map(|y| pixels[usize::from(y) * stride..][..usize::from(width) * 4].to_vec()).collect()
    }

    #[test]
    fn rows_are_merged_sorted_and_clipped() {
        assert_eq!(rows(&[rect(0, 6, 1, 2), rect(0, 1, 1, 2), rect(5, 2, 1, 3), rect(0, 9, 1, 50)], 10), vec![1..5, 6..8, 9..10]);
        assert_eq!(rows(&[rect(0, 3, 1, 2), rect(0, 5, 1, 1)], 10), vec![3..6]);
        assert_eq!(rows(&[rect(0, 10, 1, 1), rect(0, 4, 1, 0)], 10), Vec::<Range<u16>>::new());
        assert_eq!(rows(&[rect(0, 65535, 1, 65535)], 65535), Vec::<Range<u16>>::new());
    }

    #[test]
    fn the_bytes_are_the_layout() {
        let (width, height, stride) = (5u16, 7u16, 24usize);
        let first = picture(width, height, stride, 1);
        let second = picture(width, height, stride, 2);
        let rects = [rect(1, 4, 2, 2), rect(3, 1, 9, 1), rect(0, 5, 1, 1)];
        let mut file = Vec::new();
        let mut writer = Writer::new(&mut file).unwrap();
        writer.frame_at(Duration::ZERO, (width, height), 90, true, None, &first, stride).unwrap();
        writer.frame_at(Duration::from_micros(33_367), (width, height), 71, false, Some(&rects), &second, stride).unwrap();
        writer.frame_at(Duration::from_secs(5000), (width, height), 100, false, Some(&[]), &second, stride).unwrap();

        // The parser of the test: the layout, read with nothing of the module's.
        let mut at = 0;
        let mut take = |n: usize| {
            at += n;
            &file[at - n..at]
        };
        let u16_of = |bytes: &[u8]| u16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(take(8), b"WLSHCAP1");

        assert_eq!(take(8), 0u64.to_le_bytes());
        assert_eq!((u16_of(take(2)), u16_of(take(2))), (5, 7));
        assert_eq!(take(4), [90, 1, 0, 0]);
        assert_eq!(take(5 * 4 * 7), tight(&first, stride, width, 0..7));

        assert_eq!(take(8), 33_367u64.to_le_bytes());
        assert_eq!((u16_of(take(2)), u16_of(take(2))), (5, 7));
        assert_eq!(take(4), [71, 2, 3, 0]);
        for r in rects {
            let fields: Vec<u16> = take(8).chunks(2).map(u16_of).collect();
            assert_eq!(fields, [r.x, r.y, r.width, r.height]);
        }
        // Rows 1 and 4..6, in that order, each once and whole.
        assert_eq!(take(5 * 4), tight(&second, stride, width, 1..2));
        assert_eq!(take(5 * 4 * 2), tight(&second, stride, width, 4..6));

        assert_eq!(take(8), 5_000_000_000u64.to_le_bytes());
        assert_eq!(take(4), [5, 0, 7, 0]);
        assert_eq!(take(4), [100, 2, 0, 0]);
        assert_eq!(at, file.len());
    }

    #[test]
    fn the_reader_keeps_the_picture() {
        let stride = 40;
        let first = picture(6, 8, stride, 1);
        let second = picture(6, 8, stride, 2);
        let wide = picture(9, 3, stride, 3);
        let rects = [rect(2, 2, 3, 2), rect(0, 6, 6, 1)];
        let mut file = Vec::new();
        let mut writer = Writer::new(&mut file).unwrap();
        writer.frame_at(Duration::ZERO, (6, 8), 90, true, None, &first, stride).unwrap();
        writer.frame_at(Duration::from_millis(40), (6, 8), 85, false, Some(&rects), &second, stride).unwrap();
        writer.frame_at(Duration::from_millis(80), (9, 3), 90, true, None, &wide, stride).unwrap();

        let mut reader = Reader::new(&file[..]).unwrap();
        let frame = reader.next().unwrap().unwrap();
        assert_eq!((frame.index, frame.at, frame.size, frame.quality, frame.keyframe), (0, Duration::ZERO, (6, 8), 90, true));
        assert_eq!(frame.changed, None);
        assert_eq!(frame.changed_pixels(), 48);
        assert_eq!(frame.pixels, tight(&first, stride, 6, 0..8));

        let frame = reader.next().unwrap().unwrap();
        assert_eq!((frame.index, frame.at, frame.size, frame.quality, frame.keyframe), (1, Duration::from_millis(40), (6, 8), 85, false));
        assert_eq!(frame.changed, Some(&rects[..]));
        assert_eq!(frame.changed_pixels(), 12);
        // The second picture in the rows the rectangles span, the first elsewhere.
        let mut expected = tight(&first, stride, 6, 0..8);
        for span in [2..4, 6..7] {
            expected[usize::from(span.start) * 24..usize::from(span.end) * 24].copy_from_slice(&tight(&second, stride, 6, span));
        }
        assert_eq!(frame.pixels, expected);

        let frame = reader.next().unwrap().unwrap();
        assert_eq!((frame.index, frame.size, frame.keyframe), (2, (9, 3), true));
        assert_eq!(frame.pixels, tight(&wide, stride, 9, 0..3));
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn overlapping_rectangles_count_their_pixels_once() {
        let pixels = picture(10, 10, 40, 0);
        let rects = [rect(0, 0, 4, 4), rect(2, 2, 4, 4), rect(8, 3, 50, 1)];
        let mut file = Vec::new();
        let mut writer = Writer::new(&mut file).unwrap();
        writer.frame_at(Duration::ZERO, (10, 10), 90, true, None, &pixels, 40).unwrap();
        writer.frame_at(Duration::ZERO, (10, 10), 90, false, Some(&rects), &pixels, 40).unwrap();
        let mut reader = Reader::new(&file[..]).unwrap();
        reader.next().unwrap().unwrap();
        assert_eq!(reader.next().unwrap().unwrap().changed_pixels(), 16 + 16 - 4 + 2);
    }

    #[test]
    fn what_is_not_a_capture_is_refused() {
        assert!(matches!(Reader::new(&b"DKIF\0\0 \0"[..]), Err(CaptureError::Magic)));
        assert!(matches!(Reader::new(&b"WLSH"[..]), Err(CaptureError::Magic)));

        let pixels = picture(4, 4, 16, 0);
        let mut file = Vec::new();
        let mut writer = Writer::new(&mut file).unwrap();
        writer.frame_at(Duration::ZERO, (4, 4), 90, true, None, &pixels, 16).unwrap();
        writer.frame_at(Duration::ZERO, (4, 4), 90, false, Some(&[rect(0, 1, 4, 2)]), &pixels, 16).unwrap();
        let whole = file.clone();

        // Cut anywhere inside the second frame: its header, its rectangle, its rows.
        let second = MAGIC.len() + HEADER + 64;
        for cut in [second + 1, second + HEADER + 3, whole.len() - 1] {
            let mut reader = Reader::new(&whole[..cut]).unwrap();
            reader.next().unwrap().unwrap();
            assert!(matches!(reader.next(), Err(CaptureError::Truncated(1))), "cut at {cut}");
        }

        // Rectangles of a picture nothing before them holds.
        let headless = [&MAGIC[..], &whole[second..]].concat();
        let mut reader = Reader::new(&headless[..]).unwrap();
        assert!(matches!(reader.next(), Err(CaptureError::NoPicture(0, 4, 4))));

        let mut flags = whole.clone();
        flags[MAGIC.len() + 13] = 4;
        assert!(matches!(Reader::new(&flags[..]).unwrap().next(), Err(CaptureError::Flags(0, 4))));

        assert!(matches!(Writer::new(Vec::new()).unwrap().frame_at(Duration::ZERO, (4, 4), 90, true, None, &pixels[..60], 16), Err(CaptureError::Pixels(4, 4, 60, 16))));
        assert!(matches!(Writer::new(Vec::new()).unwrap().frame_at(Duration::ZERO, (0, 4), 90, true, None, &pixels, 16), Err(CaptureError::Pixels(0, 4, _, 16))));
    }
}
