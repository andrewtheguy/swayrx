//! The VP9 encoding: the whole framebuffer as one VP9 stream, for a client
//! that would rather have a picture that moves than one that is exact.
//!
//! A private encoding, [`crate::ENCODING_VP9`], which only a client that lists
//! it is ever sent. Its rectangle always covers the whole framebuffer, and its
//! body is a length word and one VP9 frame:
//!
//! ```text
//! u32 length   the frame's bytes
//! u8[length]   one VP9 frame (a superframe counts as one)
//! ```
//!
//! The rectangles of successive updates are one stream, each frame coded
//! against the frames before it, so none of them means anything alone. The
//! stream begins with a keyframe, and so does every picture a decoder has to be
//! able to start from: the first after the encoding is listed, the first at a
//! new framebuffer size, and the one that answers a non-incremental request.
//!
//! **What a frame holds.** Every frame is 8-bit VP9 converted from the
//! framebuffer as BT.601 at studio swing, which the keyframe header says so a
//! decoder does not guess. Its chroma is **4:4:4** (profile 1) and nothing
//! else — a colour sample per pixel, since the loss 4:2:0 costs a desktop is
//! its text's colour and not its edges. The quantizer is pinned by a 1–100 quality
//! dial, which only the encoder's owner moves ([`Vp9Encoder::set_quality`]): no
//! bitrate, no adaptive quantization, no dropped frames, so every frame is sent
//! at exactly the dial's quality at the time. The client's pixel format does
//! not apply to this encoding; what it decodes to is its own business.
//!
//! The coding itself is [`screen_vp9_native`]'s, an encoder written in Rust
//! for a desktop, which links no libvpx. What this module owns is the
//! framing — the length word and its ceiling — and the framebuffer's pixels in.

use thiserror::Error;

pub use screen_vp9_native::{QUALITY_MAX, QUALITY_MIN, Rect};

/// Listed beside [`crate::ENCODING_VP9`] with a quality 1–100 added, names the
/// ceiling the stream's walk never goes above: the ASCII bytes `WLQ` and the
/// value, the way Tight's quality levels ride `SetEncodings`. A value outside
/// the dial is not one of these. A VP9 list carries one: the server has no
/// quality of its own.
pub const ENCODING_VP9_QUALITY_BASE: i32 = 0x574c_5100;

/// Listed beside [`crate::ENCODING_VP9`], holds the dial at its ceiling: the
/// ASCII bytes `WLSD`. The walk then hears nothing in a fence, and only a
/// blocked write moves it.
pub const ENCODING_VP9_HELD: i32 = 0x574c_5344;

/// What the client wants the VP9 stream to be, read from its `SetEncodings`:
/// the two pseudo-encodings above, beside the encoding itself. Pseudo-encodings
/// rather than a message because a server that is not wlshare ignores an
/// encoding it does not know, where a message it does not know ends the
/// connection; and they ride the very list that names the encoding, so the first
/// frame is already what was asked for. The remotex gateway lists them from the
/// target's own keys, so that those keys mean on a passed stream what they mean
/// on one the gateway codes itself. A list that names VP9 without a quality
/// is fatal ([`NoQuality`]): the server has no quality of its own, the gateway
/// names one beside the encoding on every list, and a client that is not the
/// gateway lists no VP9 at all. Without `WLSD` it walks.
///
/// A change of quality or of the walk moves the running encoder's dial
/// without a keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Stream {
    pub quality: u8,
    pub adaptive: bool,
}

impl Vp9Stream {
    /// The stream `encodings` asks for: `None` where the list does not name
    /// [`crate::ENCODING_VP9`], and [`NoQuality`] where it does and names no
    /// quality on the dial beside it.
    pub fn listed(encodings: &[i32]) -> Result<Option<Self>, NoQuality> {
        let has = |e: i32| encodings.contains(&e);
        if !has(crate::ENCODING_VP9) {
            return Ok(None);
        }
        let quality = encodings
            .iter()
            .filter_map(|&e| u8::try_from(e.checked_sub(ENCODING_VP9_QUALITY_BASE)?).ok())
            .find(|q| (QUALITY_MIN..=QUALITY_MAX).contains(q))
            .ok_or(NoQuality)?;
        Ok(Some(Self { quality, adaptive: !has(ENCODING_VP9_HELD) }))
    }

    /// The list that asks for this stream: the encoding, the quality always,
    /// and the hold where it is asked.
    #[cfg(test)]
    fn encodings(self) -> Vec<i32> {
        let mut listed = vec![crate::ENCODING_VP9, ENCODING_VP9_QUALITY_BASE + i32::from(self.quality)];
        if !self.adaptive {
            listed.push(ENCODING_VP9_HELD);
        }
        listed
    }
}

/// A list that names VP9 and no quality on the dial beside it, which no client
/// sends: the gateway names one on every list, and a client that is not the
/// gateway lists no VP9. Fatal to the connection, as a malformed message is.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("the list names VP9 without a quality 1–100 beside it")]
pub struct NoQuality;

/// Why a picture could not be encoded, or in the tests a frame decoded.
#[derive(Debug, Error)]
pub enum Vp9Error {
    /// The codec, or the pixels in front of it, refused.
    #[error(transparent)]
    Codec(#[from] screen_vp9_native::Error),
    #[error("a frame of {0} bytes is over the {max}-byte ceiling", max = crate::msg::MAX_RECT_BODY)]
    FrameTooLong(usize),
    #[cfg(test)]
    #[error("the frame is {0}x{1} and its rectangle {2}x{3}")]
    Size(u32, u32, usize, usize),
    /// libvpx, the tests' decoder, refused the frame.
    #[cfg(test)]
    #[error("{0}")]
    Decode(String),
}

/// How many threads the encoder gets: the machine less two cores, at most
/// [`MAX_ENCODER_THREADS`]. An encode is a burst of milliseconds a frame that
/// the person at the other end is waiting on, and the encoder splits it
/// across threads by rows and tile columns; the two cores kept back are for
/// the compositor and the session, which are what make the next frame. The
/// count also bounds the stream's tile columns, and so what a decoder's
/// threads have to split.
fn encoder_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(2)).clamp(1, MAX_ENCODER_THREADS)
}

/// The most threads the encoder takes, whatever the machine: what a 4K
/// picture can use. The useful count is the picture's, not the machine's —
/// superblock rows are handed out within each tile column, so the work to
/// share grows with the picture — at about one thread a megapixel: eight for
/// 4K's 8.3. 4K is the largest desktop this is tuned for; a larger one is an
/// edge case that streams, not a target, and gets the 4K count. Measured on
/// the libvpx encoder this one replaced, not on this one.
const MAX_ENCODER_THREADS: usize = 8;

/// How many threads the tests' decoder gets: half the machine, at most four.
/// A decode gains nothing past the stream's tile columns.
#[cfg(test)]
fn decoder_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get() / 2).clamp(1, 4)
}

/// One connection's VP9 stream, server side: screen-vp9-native's stream at
/// one picture size, which converts the framebuffer and codes it. A framebuffer of
/// another size needs another encoder, whose first frame is a keyframe by
/// construction.
pub struct Vp9Encoder {
    stream: screen_vp9_native::Stream,
}

impl Vp9Encoder {
    /// An encoder for a `width`×`height` picture at `quality` (1–100).
    pub fn new(width: u16, height: u16, quality: u8) -> Result<Self, Vp9Error> {
        Ok(Self { stream: screen_vp9_native::Stream::new(width, height, quality, encoder_threads())? })
    }

    /// The picture size this encoder codes.
    pub fn size(&self) -> (u16, u16) {
        self.stream.size()
    }

    /// The quality the next frame is coded at, clamped to the dial.
    pub fn quality(&self) -> u8 {
        self.stream.quality()
    }

    /// The coarsest quality any block of the client's picture was last coded
    /// at: below the stream's ceiling while a frame the link coarsened has left
    /// blocks no finer frame has coded since, which a frame of damage does not
    /// do for the blocks it skips and a whole frame does for all of them.
    pub fn coarsest(&self) -> u8 {
        self.stream.coarsest()
    }

    /// Move the dial on the running encoder, clamped to it. The next frame is
    /// coded at the new quantizer against the frames before it: rebuilding the
    /// encoder would cost a keyframe, the most bytes a frame can be, at the
    /// moment a slow link can least afford them.
    pub fn set_quality(&mut self, quality: u8) {
        self.stream.set_quality(quality);
    }

    /// Read the picture — [`Self::size`] of `B, G, R, X` pixels whose rows
    /// are `stride` bytes apart — for the next frame to carry
    /// ([`Self::encode`]). It is read into the encoder's own planes, so
    /// `pixels` is the framebuffer itself, for as long as the conversion
    /// takes and no longer: nothing of it is kept.
    ///
    /// `changed` is the damage since the picture read before, or `None` for
    /// a picture that may differ anywhere. With it only the rectangles are
    /// read from `pixels`. An encoder's first read is the whole picture
    /// whatever `changed` says.
    pub fn read(&mut self, pixels: &[u8], stride: usize, changed: Option<&[Rect]>) -> Result<(), Vp9Error> {
        Ok(self.stream.read_bgrx(pixels, stride, changed)?)
    }

    /// Encode the picture as it was last read and append the rectangle's body
    /// to `out`: the length word and the frame. `keyframe` makes it one a
    /// decoder can start from; an encoder's first frame is one either way.
    ///
    /// Only the blocks the damage read since the last frame touches are
    /// coded: the rest of the frame is the picture the client already holds,
    /// at the quality it holds it. A keyframe and a picture read whole are
    /// the whole picture.
    ///
    /// Every encode makes a frame; an error leaves `out` as it was.
    pub fn encode(&mut self, keyframe: bool, out: &mut Vec<u8>) -> Result<(), Vp9Error> {
        self.framed(out, |stream, out| stream.encode(keyframe, out))
    }

    /// Settle the picture at `quality`: encode it as it was last read, whole
    /// and at that quality, and leave the dial where it was. What a desktop
    /// that went quiet while its link had it coarse is sent once: an inter
    /// frame unless `keyframe` asks, which sharpens every block, with the
    /// frames after it at what the link bears again.
    ///
    /// An error leaves `out` as it was, as [`Self::encode`]'s does.
    pub fn settle(&mut self, quality: u8, keyframe: bool, out: &mut Vec<u8>) -> Result<(), Vp9Error> {
        self.framed(out, |stream, out| stream.settle(quality, keyframe, out))
    }

    /// Append a rectangle's body to `out`: the length word, and the frame
    /// `encode` appends. Nothing is appended for an encode that failed or
    /// made a frame over the ceiling.
    fn framed(&mut self, out: &mut Vec<u8>, encode: impl FnOnce(&mut screen_vp9_native::Stream, &mut Vec<u8>) -> Result<bool, screen_vp9_native::Error>) -> Result<(), Vp9Error> {
        let length_at = out.len();
        out.extend_from_slice(&[0; 4]);
        if let Err(e) = encode(&mut self.stream, out) {
            out.truncate(length_at);
            return Err(e.into());
        }
        let len = out.len() - length_at - 4;
        if len > crate::msg::MAX_RECT_BODY {
            out.truncate(length_at);
            return Err(Vp9Error::FrameTooLong(len));
        }
        out[length_at..length_at + 4].copy_from_slice(&(len as u32).to_be_bytes());
        Ok(())
    }
}

/// One connection's VP9 stream, client side, which the tests read the encoder
/// back with: every VP9 rectangle is decoded by the same decoder, libvpx's,
/// in the order they arrive, since each frame is coded against the ones
/// before it.
#[cfg(test)]
struct Vp9Decoder {
    decoder: crate::vp9_decoder::Decoder,
}

#[cfg(test)]
impl Vp9Decoder {
    fn new() -> Result<Self, Vp9Error> {
        Ok(Self { decoder: crate::vp9_decoder::Decoder::new(decoder_threads()).map_err(Vp9Error::Decode)? })
    }

    /// Decode a VP9 rectangle payload — the frame, without its length word —
    /// of `width`×`height` pixels into `out`, whose first byte is the
    /// rectangle's first pixel and whose rows are `stride` bytes apart, as
    /// `B, G, R, X`.
    fn decode_rect(&mut self, payload: &[u8], width: usize, height: usize, out: &mut [u8], stride: usize) -> Result<(), Vp9Error> {
        let decoded = self.decoder.decode(payload).map_err(Vp9Error::Decode)?;
        let (w, h) = decoded.size();
        if (w as usize, h as usize) != (width, height) {
            return Err(Vp9Error::Size(w, h, width, height));
        }
        decoded.write_bgrx(out, stride).map_err(Vp9Error::Decode)
    }
}

/// The encoder read back by the decoder, through the rectangle's framing. The
/// coding itself is proved in `screen-vp9-native`; what these hold is the half this
/// module owns — the length word, its ceiling, the pixels in and out — and
/// that a client's decoder refuses what the encoding does not carry.
#[cfg(test)]
mod tests {
    use super::*;

    /// A dark terminal with one-pixel coloured glyph stems at odd columns: the
    /// picture 4:2:0 cannot carry, since each stem shares its colour sample with
    /// three pixels of background.
    fn stems(width: usize, height: usize) -> (Vec<u8>, Vec<(usize, usize)>) {
        let mut pixels: Vec<u8> = [30u8, 30, 30, 0].repeat(width * height);
        let mut at = Vec::new();
        for y in (0..height).step_by(2) {
            for x in (1..width).step_by(4) {
                pixels[(y * width + x) * 4..][..4].copy_from_slice(&[198, 121, 255, 0]);
                at.push((x, y));
            }
        }
        (pixels, at)
    }

    /// Encode `pixels` and read back the length word, returning the frame.
    fn encode(encoder: &mut Vp9Encoder, pixels: &[u8], keyframe: bool) -> Vec<u8> {
        let (width, _) = encoder.size();
        let mut out = vec![0xEE];
        encoder.read(pixels, usize::from(width) * 4, None).expect("a read");
        encoder.encode(keyframe, &mut out).expect("an encode");
        assert_eq!(out[0], 0xEE, "appended, not overwritten");
        let len = u32::from_be_bytes(out[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, out.len() - 5, "the length word is the frame's");
        out.split_off(5)
    }

    /// The settle's rectangle is framed as any other, sharpens the picture a
    /// coarse dial left without a keyframe, and leaves the dial where it was.
    #[test]
    fn a_settle_sharpens_the_picture_and_leaves_the_dial() {
        let (width, height) = (64, 48);
        let (pixels, _) = stems(width, height);
        let mut encoder = Vp9Encoder::new(width as u16, height as u16, QUALITY_MIN).unwrap();
        let mut decoder = Vp9Decoder::new().unwrap();
        let mut back = vec![0; width * height * 4];
        let mut error = |frame: &[u8]| {
            decoder.decode_rect(frame, width, height, &mut back, width * 4).unwrap();
            back.chunks(4).zip(pixels.chunks(4)).map(|(got, want)| (0..3).map(|c| u64::from(got[c].abs_diff(want[c]))).sum::<u64>()).sum::<u64>()
        };
        let coarse = error(&encode(&mut encoder, &pixels, false));

        let mut out = vec![0xEE];
        encoder.settle(QUALITY_MAX, false, &mut out).expect("a settle");
        assert_eq!((out[0], u32::from_be_bytes(out[1..5].try_into().unwrap()) as usize), (0xEE, out.len() - 5), "the length word is the frame's");
        let frame = out.split_off(5);
        assert!(screen_vp9_native::frame_header(&frame).is_some_and(|header| !header.keyframe), "the settle cost a keyframe");
        let settled = error(&frame);
        assert!(settled < coarse / 2, "the settle left the picture at error {settled} of {coarse}");
        assert_eq!((encoder.quality(), encoder.coarsest()), (QUALITY_MIN, QUALITY_MAX));

        // An encoder nothing was read into has nothing to settle.
        let mut out = vec![0xEE];
        let mut unread = Vp9Encoder::new(width as u16, height as u16, QUALITY_MIN).unwrap();
        assert!(matches!(unread.settle(QUALITY_MAX, false, &mut out), Err(Vp9Error::Codec(screen_vp9_native::Error::Unread))));
        assert_eq!((out, unread.quality()), (vec![0xEE], QUALITY_MIN), "a settle that failed left something behind");
    }

    #[test]
    fn a_444_stream_keeps_a_one_pixel_stem_its_colour() {
        let (width, height) = (64, 48);
        let (pixels, at) = stems(width, height);
        let mut encoder = Vp9Encoder::new(width as u16, height as u16, QUALITY_MAX).unwrap();
        let frame = encode(&mut encoder, &pixels, false);
        assert!(screen_vp9_native::frame_header(&frame).is_some_and(|header| header.keyframe && header.profile == 1), "an encoder's first frame is a 4:4:4 keyframe");

        let mut decoder = Vp9Decoder::new().unwrap();
        let stride = width * 4 + 8;
        let mut out = vec![0; stride * height];
        decoder.decode_rect(&frame, width, height, &mut out, stride).unwrap();
        let worst = at
            .iter()
            .map(|&(x, y)| {
                let got = &out[y * stride + x * 4..][..3];
                got.iter().zip([198u8, 121, 255]).map(|(a, b)| a.abs_diff(b)).max().unwrap()
            })
            .max()
            .unwrap();
        assert!(worst <= 24, "a stem pixel came back {worst} code values off");
    }

    /// Only the frames that are asked to be keyframes are, the ones between
    /// decode against them, and the dial moves without one.
    #[test]
    fn keyframes_come_when_asked_and_the_dial_moves_between_them() {
        let (width, height) = (64, 32);
        let mut encoder = Vp9Encoder::new(width as u16, height as u16, 60).unwrap();
        let mut decoder = Vp9Decoder::new().unwrap();
        let mut out = vec![0; width * height * 4];
        for step in 0..5usize {
            let mut pixels = [80u8, 40, 20, 0].repeat(width * height);
            for x in step * 4..step * 4 + 8 {
                for y in 0..height {
                    pixels[(y * width + x) * 4..][..3].copy_from_slice(&[240, 240, 240]);
                }
            }
            if step == 2 {
                encoder.set_quality(QUALITY_MAX);
                assert_eq!(encoder.quality(), QUALITY_MAX);
            }
            let frame = encode(&mut encoder, &pixels, step == 3);
            let header = screen_vp9_native::frame_header(&frame).expect("a VP9 frame");
            assert_eq!(header.keyframe, step == 0 || step == 3, "frame {step}");
            decoder.decode_rect(&frame, width, height, &mut out, width * 4).unwrap();
            let lit = &out[(step * 4 + 4) * 4..][..3];
            assert!(lit.iter().all(|&c| c > 200), "frame {step} did not decode to its own picture: {lit:?}");
        }
    }

    /// A frame of damage reads the rows its rectangles span and nothing else
    /// of the buffer, and leaves the rest of the client's picture as it was.
    /// An encoder's first frame and a keyframe have no picture to be damage
    /// to, and are read whole.
    #[test]
    fn a_frame_of_damage_reads_and_codes_only_what_it_names() {
        let (width, height) = (96usize, 80usize);
        let stride = width * 4;
        const BACK: [u8; 4] = [80, 40, 20, 0];
        const LIT: [u8; 4] = [240, 240, 240, 0];
        let damage = Rect { x: 32, y: 16, width: 32, height: 16 };
        let lit = |pixels: &mut [u8], rect: Rect| {
            for y in usize::from(rect.y)..usize::from(rect.y + rect.height) {
                for x in usize::from(rect.x)..usize::from(rect.x + rect.width) {
                    pixels[y * stride + x * 4..][..4].copy_from_slice(&LIT);
                }
            }
        };
        let at = |out: &[u8], x: usize, y: usize| -> [u8; 3] { out[y * stride + x * 4..][..3].try_into().unwrap() };
        let near = |got: [u8; 3], want: [u8; 4]| (0..3).all(|c| got[c].abs_diff(want[c]) <= 24);
        let frame = |encoder: &mut Vp9Encoder, pixels: &[u8], changed: Option<&[Rect]>, keyframe: bool| {
            let mut out = Vec::new();
            encoder.read(pixels, stride, changed).expect("a read");
            encoder.encode(keyframe, &mut out).expect("an encode");
            out.split_off(4)
        };
        let mut encoder = Vp9Encoder::new(width as u16, height as u16, QUALITY_MAX).unwrap();
        let mut decoder = Vp9Decoder::new().unwrap();
        let mut out = vec![0; stride * height];
        let whole = BACK.repeat(width * height);

        // The first frame is told of damage it cannot be damage to.
        let first = frame(&mut encoder, &whole, Some(&[damage]), false);
        assert!(screen_vp9_native::frame_header(&first).is_some_and(|header| header.keyframe));
        decoder.decode_rect(&first, width, height, &mut out, stride).unwrap();
        assert!(near(at(&out, 4, 70), BACK) && near(at(&out, 48, 24), BACK), "the first frame was not the whole picture");

        // Damage, in a buffer whose other rows are not the picture.
        let mut rows = [0xAAu8, 0x55, 0xAA, 0].repeat(width * height);
        let span = usize::from(damage.y) * stride..usize::from(damage.y + damage.height) * stride;
        rows[span.clone()].copy_from_slice(&whole[span]);
        lit(&mut rows, damage);
        let second = frame(&mut encoder, &rows, Some(&[damage]), false);
        assert!(screen_vp9_native::frame_header(&second).is_some_and(|header| !header.keyframe));
        decoder.decode_rect(&second, width, height, &mut out, stride).unwrap();
        assert!(near(at(&out, 48, 24), LIT), "the damage came back {:?}", at(&out, 48, 24));
        for (x, y) in [(4, 4), (4, 24), (90, 24), (48, 4), (48, 70)] {
            assert!(near(at(&out, x, y), BACK), "({x}, {y}), outside the damage, came back {:?}", at(&out, x, y));
        }

        // A picture read whole is the whole buffer, and a keyframe where
        // one is asked.
        let mut all = whole.clone();
        lit(&mut all, Rect { x: 0, y: 64, width: 96, height: 16 });
        let third = frame(&mut encoder, &all, None, true);
        assert!(screen_vp9_native::frame_header(&third).is_some_and(|header| header.keyframe));
        decoder.decode_rect(&third, width, height, &mut out, stride).unwrap();
        assert!(near(at(&out, 48, 24), BACK) && near(at(&out, 48, 70), LIT), "the keyframe was not the whole picture");
    }

    #[test]
    fn a_frame_of_another_size_or_no_frame_at_all_is_an_error() {
        let mut encoder = Vp9Encoder::new(32, 16, 60).unwrap();
        let frame = encode(&mut encoder, &[0u8; 32 * 16 * 4], false);
        let mut out = vec![0; 64 * 64 * 4];
        let mut decoder = Vp9Decoder::new().unwrap();
        assert!(matches!(decoder.decode_rect(&frame, 16, 16, &mut out, 64), Err(Vp9Error::Size(32, 16, 16, 16))));
        assert!(decoder.decode_rect(&[0xFF, 0x00, 0x12], 32, 16, &mut out, 128).is_err());
        assert!(matches!(decoder.decode_rect(&frame, 32, 16, &mut out[..10], 128), Err(Vp9Error::Decode(_))));
        assert!(matches!(Vp9Encoder::new(0, 16, 60), Err(Vp9Error::Codec(screen_vp9_native::Error::Empty(0, 16)))));
        assert!(matches!(encoder.read(&[0; 12], 128, None), Err(Vp9Error::Codec(screen_vp9_native::Error::Buffer { .. }))));
    }
}

/// What a client's list asks the stream to be, both ways.
#[cfg(test)]
mod stream_tests {
    use super::*;

    #[test]
    fn the_stream_is_read_from_the_list_beside_the_encoding() {
        let asked = Vp9Stream { quality: 60, adaptive: true };
        assert_eq!(asked.encodings(), [crate::ENCODING_VP9, 0x574c_513c]);
        let listed = [crate::ENCODING_VP9, 0x574c_513c, crate::ENCODING_ZRLE];
        assert_eq!(Vp9Stream::listed(&listed), Ok(Some(asked)));
        let held = Vp9Stream { quality: 100, adaptive: false };
        assert_eq!(held.encodings(), [crate::ENCODING_VP9, 0x574c_5164, 0x574c_5344]);
        assert_eq!(Vp9Stream::listed(&held.encodings()), Ok(Some(held)));
    }

    /// A list without VP9 asks for no stream, whatever else it names; the
    /// server has no quality of its own, so one that names VP9 without a
    /// quality is no list a client sends.
    #[test]
    fn a_list_that_names_vp9_without_a_quality_is_fatal() {
        assert_eq!(Vp9Stream::listed(&[crate::ENCODING_ZRLE]), Ok(None));
        assert_eq!(Vp9Stream::listed(&[0x574c_513c, crate::ENCODING_ZRLE]), Ok(None), "a quality without the encoding");
        assert_eq!(Vp9Stream::listed(&[crate::ENCODING_VP9, crate::ENCODING_ZRLE]), Err(NoQuality));
        // A quality off the dial is some other encoding, not a quality.
        for off in [ENCODING_VP9_QUALITY_BASE, ENCODING_VP9_QUALITY_BASE + 101, ENCODING_VP9_QUALITY_BASE + 255, ENCODING_VP9_QUALITY_BASE + 256] {
            assert_eq!(Vp9Stream::listed(&[crate::ENCODING_VP9, off]), Err(NoQuality), "{off:#x}");
        }
    }
}
