//! The VP9 encoding: the whole framebuffer as one VP9 stream, for a desktop
//! client that would rather have a picture that moves than one that is exact.
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
//! decoder does not guess. Its chroma is **4:4:4** (profile 1) — a colour sample
//! per pixel, since the loss 4:2:0 costs a desktop is its text's colour and not
//! its edges — unless the client listed [`ENCODING_VP9_SUBSAMPLED`] beside the
//! encoding ([`Vp9Stream`]), which the remotex gateway does for a browser whose
//! decoder takes only profile 0; wlshare's own desktop clients never do, and
//! [`Vp9Decoder`] takes 4:4:4 alone. The quantizer is pinned by a 1–100 quality dial, which only the
//! encoder's owner moves ([`Vp9Encoder::set_quality`]): no bitrate, no adaptive
//! quantization, no dropped frames, so every frame is sent at exactly the
//! dial's quality at the time. The client's pixel format does not apply to this
//! encoding; what it decodes to is its own business, and [`Vp9Decoder`] writes
//! `B, G, R, X`.
//!
//! The coding itself is [`desktop_vp9`]'s, the one place libvpx is spoken to
//! for wlshare and for the remotex gateway alike, behind `encode` on the
//! server's side and `decode` on the client's. What this module owns is the
//! framing — the length word and its ceiling — and the framebuffer's pixels in
//! and out.

use thiserror::Error;

pub use desktop_vp9::{Chroma, QUALITY_MAX, QUALITY_MIN};

/// Listed beside [`crate::ENCODING_VP9`], asks for the stream at 4:2:0 (VP9
/// profile 0) in place of 4:4:4: the ASCII bytes `WLS0`. The remotex gateway
/// lists it for a browser whose decoder takes only profile 0; wlshare's own
/// desktop clients never do, and [`Vp9Decoder`] takes 4:4:4 alone.
pub const ENCODING_VP9_SUBSAMPLED: i32 = 0x574c_5330;

/// Listed beside [`crate::ENCODING_VP9`] with a quality 1–100 added, names the
/// ceiling the stream's walk never goes above, in place of the server's
/// `vp9_quality`: the ASCII bytes `WLQ` and the value, the way Tight's quality
/// levels ride `SetEncodings`. A value outside the dial is not one of these.
pub const ENCODING_VP9_QUALITY_BASE: i32 = 0x574c_5100;

/// Listed beside [`crate::ENCODING_VP9`], holds the dial at its ceiling: the
/// ASCII bytes `WLSD`. The walk then hears nothing in a fence, and only a
/// blocked write moves it.
pub const ENCODING_VP9_HELD: i32 = 0x574c_5344;

/// What the client wants the VP9 stream to be, read from its `SetEncodings`:
/// the three pseudo-encodings above, beside the encoding itself. Pseudo-encodings
/// rather than a message because a server that is not wlshare ignores an
/// encoding it does not know, where a message it does not know ends the
/// connection; and they ride the very list that names the encoding, so the first
/// frame is already what was asked for. The remotex gateway lists them from the
/// target's own keys, so that those keys mean on a passed stream what they mean
/// on one the gateway codes itself; wlshare's own desktop clients list none, and
/// a list without them is 4:4:4 at the server's `vp9_quality`, with the walk.
///
/// A change of chroma starts the stream over at a keyframe; a change of quality
/// or of the walk moves the running encoder's dial without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Stream {
    pub chroma: Chroma,
    pub quality: u8,
    pub adaptive: bool,
}

impl Vp9Stream {
    /// The stream `encodings` asks for, with `quality` — the server's own — as
    /// the ceiling where the list names none.
    pub fn listed(encodings: &[i32], quality: u8) -> Self {
        let has = |e: i32| encodings.contains(&e);
        let asked = encodings
            .iter()
            .filter_map(|&e| u8::try_from(e.checked_sub(ENCODING_VP9_QUALITY_BASE)?).ok())
            .find(|q| (QUALITY_MIN..=QUALITY_MAX).contains(q));
        Self {
            chroma: if has(ENCODING_VP9_SUBSAMPLED) { Chroma::Subsampled } else { Chroma::Full },
            quality: asked.unwrap_or(quality),
            adaptive: !has(ENCODING_VP9_HELD),
        }
    }

    /// The pseudo-encodings that ask for this stream, to list beside the
    /// encoding: the quality always, the other two where they differ from what
    /// a list without them means.
    pub fn encodings(self) -> Vec<i32> {
        let mut listed = vec![ENCODING_VP9_QUALITY_BASE + i32::from(self.quality)];
        if self.chroma == Chroma::Subsampled {
            listed.push(ENCODING_VP9_SUBSAMPLED);
        }
        if !self.adaptive {
            listed.push(ENCODING_VP9_HELD);
        }
        listed
    }
}

/// Why a picture could not be encoded or a frame decoded.
#[derive(Debug, Error)]
pub enum Vp9Error {
    /// The codec, or the pixels in front of it, refused.
    #[error(transparent)]
    Codec(#[from] desktop_vp9::Error),
    #[error("a frame of {0} bytes is over the {max}-byte ceiling", max = crate::client::MAX_RECT_BODY)]
    FrameTooLong(usize),
    #[error("the frame is {0}x{1} and its rectangle {2}x{3}")]
    Size(u32, u32, usize, usize),
    #[error("the frame is {0}, not the 4:4:4 this encoding carries")]
    Chroma(&'static str),
}

/// How many threads the encoder gets: the machine less two cores, at most
/// [`MAX_ENCODER_THREADS`]. An encode is a burst of tens of milliseconds a
/// frame that the person at the other end is waiting on, and libvpx splits it
/// across threads by rows and tile columns; the two cores kept back are for
/// the compositor and the session, which are what make the next frame. Six
/// threads on a six-core host coded a scrolling 4K frame in three quarters of
/// the time three did, and a keyframe in a little over half.
#[cfg(feature = "encode")]
fn encoder_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(2)).clamp(1, MAX_ENCODER_THREADS)
}

/// The most threads the encoder takes, whatever the machine: what a 4K
/// picture can use. The useful count is the picture's, not the machine's —
/// libvpx hands out superblock rows within each tile column, so the work to
/// share grows with the picture — and the measurements, 1080p saturating at
/// three threads and 4K still gaining at six, put it at about one thread a
/// megapixel: eight for 4K's 8.3. 4K is the largest desktop this is tuned
/// for; a larger one is an edge case that streams, not a target, and gets the
/// 4K count. Not measured past six threads, since the host had six cores.
#[cfg(feature = "encode")]
const MAX_ENCODER_THREADS: usize = 8;

/// How many threads the decoder gets: half the machine, at most four. A
/// decode gains nothing past the stream's tile columns, and the window still
/// needs somewhere to draw.
#[cfg(feature = "decode")]
fn decoder_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get() / 2).clamp(1, 4)
}

/// One connection's VP9 stream, server side: an encoder at one picture size,
/// and the planes the framebuffer is converted into in front of it. A
/// framebuffer of another size needs another encoder, whose first frame is a
/// keyframe by construction.
#[cfg(feature = "encode")]
pub struct Vp9Encoder {
    encoder: desktop_vp9::Encoder,
    picture: desktop_vp9::Picture,
}

#[cfg(feature = "encode")]
impl Vp9Encoder {
    /// An encoder for a `width`×`height` picture at `chroma` and `quality`
    /// (1–100).
    pub fn new(width: u16, height: u16, chroma: Chroma, quality: u8) -> Result<Self, Vp9Error> {
        let picture = desktop_vp9::Picture::new(width, height, chroma)?;
        let encoder = desktop_vp9::Encoder::new(width, height, chroma, quality, encoder_threads())?;
        Ok(Self { encoder, picture })
    }

    /// The chroma this encoder codes.
    pub fn chroma(&self) -> Chroma {
        self.encoder.chroma()
    }

    /// The picture size this encoder codes.
    pub fn size(&self) -> (u16, u16) {
        self.encoder.size()
    }

    /// The quality the next frame is coded at, clamped to the dial.
    pub fn quality(&self) -> u8 {
        self.encoder.quality()
    }

    /// Move the dial on the running encoder, clamped to it. The next frame is
    /// coded at the new quantizer against the frames before it: rebuilding the
    /// encoder would cost a keyframe, the most bytes a frame can be, at the
    /// moment a slow link can least afford them.
    pub fn set_quality(&mut self, quality: u8) -> Result<(), Vp9Error> {
        Ok(self.encoder.set_quality(quality)?)
    }

    /// Encode the picture — [`Self::size`] of `B, G, R, X` pixels whose rows
    /// are `stride` bytes apart — and append the rectangle's body to `out`: the
    /// length word and the frame. `keyframe` makes it one a decoder can start
    /// from; an encoder's first frame is one either way. Returns whether a body
    /// was appended: `false`, with `out` as it was, when the encoder produced no
    /// frame, since an empty rectangle is not a frame a client can decode and
    /// the pixels are the next frame's to carry.
    pub fn encode_rect(&mut self, pixels: &[u8], stride: usize, keyframe: bool, out: &mut Vec<u8>) -> Result<bool, Vp9Error> {
        self.picture.read_bgrx(pixels, stride)?;
        let length_at = out.len();
        out.extend_from_slice(&[0; 4]);
        match self.encoder.encode(&self.picture, keyframe, out) {
            Ok(Some(_)) => {}
            Ok(None) => {
                out.truncate(length_at);
                return Ok(false);
            }
            Err(e) => {
                out.truncate(length_at);
                return Err(e.into());
            }
        }
        let len = out.len() - length_at - 4;
        if len > crate::client::MAX_RECT_BODY {
            out.truncate(length_at);
            return Err(Vp9Error::FrameTooLong(len));
        }
        out[length_at..length_at + 4].copy_from_slice(&(len as u32).to_be_bytes());
        Ok(true)
    }
}

/// One connection's VP9 stream, client side: every VP9 rectangle is decoded
/// by the same decoder, in the order they arrive, since each frame is coded
/// against the ones before it.
#[cfg(feature = "decode")]
pub struct Vp9Decoder {
    decoder: desktop_vp9::Decoder,
}

#[cfg(feature = "decode")]
impl Vp9Decoder {
    pub fn new() -> Result<Self, Vp9Error> {
        Ok(Self { decoder: desktop_vp9::Decoder::new(decoder_threads())? })
    }

    /// Decode a VP9 rectangle payload — the frame, without its length word —
    /// of `width`×`height` pixels into `out`, whose first byte is the
    /// rectangle's first pixel and whose rows are `stride` bytes apart, as
    /// `B, G, R, X`.
    pub fn decode_rect(&mut self, payload: &[u8], width: usize, height: usize, out: &mut [u8], stride: usize) -> Result<(), Vp9Error> {
        let decoded = self.decoder.decode(payload)?;
        let (w, h) = decoded.size();
        if (w as usize, h as usize) != (width, height) {
            return Err(Vp9Error::Size(w, h, width, height));
        }
        if decoded.chroma() != Chroma::Full {
            return Err(Vp9Error::Chroma(decoded.chroma().name()));
        }
        if width == 0 || height == 0 {
            return Ok(());
        }
        Ok(decoded.write_bgrx(out, stride)?)
    }
}

/// The encoder read back by the decoder, through the rectangle's framing. The
/// coding itself is proved in `desktop-vp9`; what these hold is the half this
/// module owns — the length word, its ceiling, the pixels in and out — and
/// that a client's decoder refuses what the encoding does not carry.
#[cfg(all(test, feature = "encode", feature = "decode"))]
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
        assert!(encoder.encode_rect(pixels, usize::from(width) * 4, keyframe, &mut out).expect("an encode"), "a frame");
        assert_eq!(out[0], 0xEE, "appended, not overwritten");
        let len = u32::from_be_bytes(out[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, out.len() - 5, "the length word is the frame's");
        out.split_off(5)
    }

    #[test]
    fn a_444_stream_keeps_a_one_pixel_stem_its_colour() {
        let (width, height) = (64, 48);
        let (pixels, at) = stems(width, height);
        let mut encoder = Vp9Encoder::new(width as u16, height as u16, Chroma::Full, QUALITY_MAX).unwrap();
        let frame = encode(&mut encoder, &pixels, false);
        assert!(desktop_vp9::frame_header(&frame).is_some_and(|header| header.keyframe && header.profile == 1), "an encoder's first frame is a 4:4:4 keyframe");

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
        let mut encoder = Vp9Encoder::new(width as u16, height as u16, Chroma::Full, 60).unwrap();
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
                encoder.set_quality(QUALITY_MAX).unwrap();
                assert_eq!(encoder.quality(), QUALITY_MAX);
            }
            let frame = encode(&mut encoder, &pixels, step == 3);
            let header = desktop_vp9::frame_header(&frame).expect("a VP9 frame");
            assert_eq!(header.keyframe, step == 0 || step == 3, "frame {step}");
            decoder.decode_rect(&frame, width, height, &mut out, width * 4).unwrap();
            let lit = &out[(step * 4 + 4) * 4..][..3];
            assert!(lit.iter().all(|&c| c > 200), "frame {step} did not decode to its own picture: {lit:?}");
        }
    }

    #[test]
    fn a_frame_of_another_size_or_chroma_or_no_frame_at_all_is_an_error() {
        let mut encoder = Vp9Encoder::new(32, 16, Chroma::Full, 60).unwrap();
        let frame = encode(&mut encoder, &[0u8; 32 * 16 * 4], false);
        let mut out = vec![0; 64 * 64 * 4];
        let mut decoder = Vp9Decoder::new().unwrap();
        assert!(matches!(decoder.decode_rect(&frame, 16, 16, &mut out, 64), Err(Vp9Error::Size(32, 16, 16, 16))));
        assert!(decoder.decode_rect(&[0xFF, 0x00, 0x12], 32, 16, &mut out, 128).is_err());
        assert!(matches!(decoder.decode_rect(&frame, 32, 16, &mut out[..10], 128), Err(Vp9Error::Codec(desktop_vp9::Error::Buffer { .. }))));
        assert!(matches!(Vp9Encoder::new(0, 16, Chroma::Full, 60), Err(Vp9Error::Codec(desktop_vp9::Error::Empty(0, 16)))));
        assert!(matches!(encoder.encode_rect(&[0; 12], 128, false, &mut Vec::new()), Err(Vp9Error::Codec(desktop_vp9::Error::Buffer { .. }))));

        // A 4:2:0 frame, which the gateway asks for, is a VP9 frame of the
        // same framing — and not one the desktop client's decoder carries.
        let mut subsampled = Vp9Encoder::new(32, 16, Chroma::Subsampled, 60).unwrap();
        assert_eq!(subsampled.chroma(), Chroma::Subsampled);
        let frame = encode(&mut subsampled, &[0u8; 32 * 16 * 4], false);
        assert_eq!(desktop_vp9::frame_header(&frame).expect("a VP9 frame").profile, 0);
        assert!(matches!(Vp9Decoder::new().unwrap().decode_rect(&frame, 32, 16, &mut out, 128), Err(Vp9Error::Chroma("4:2:0"))));
    }
}

/// What a client's list asks the stream to be, both ways.
#[cfg(test)]
mod stream_tests {
    use super::*;

    #[test]
    fn the_stream_is_read_from_the_list_beside_the_encoding() {
        let asked = Vp9Stream { chroma: Chroma::Subsampled, quality: 60, adaptive: true };
        assert_eq!(asked.encodings(), [0x574c_513c, 0x574c_5330]);
        let listed = [crate::ENCODING_VP9, 0x574c_513c, 0x574c_5330, crate::ENCODING_ZRLE];
        assert_eq!(Vp9Stream::listed(&listed, 90), asked);
        let held = Vp9Stream { chroma: Chroma::Full, quality: 100, adaptive: false };
        assert_eq!(held.encodings(), [0x574c_5164, 0x574c_5344]);
        assert_eq!(Vp9Stream::listed(&held.encodings(), 90), held);
    }

    #[test]
    fn a_list_that_names_nothing_is_the_servers_stream() {
        let none = [crate::ENCODING_VP9, crate::ENCODING_ZRLE];
        assert_eq!(Vp9Stream::listed(&none, 90), Vp9Stream { chroma: Chroma::Full, quality: 90, adaptive: true });
        // A quality off the dial is some other encoding, not a request.
        for off in [ENCODING_VP9_QUALITY_BASE, ENCODING_VP9_QUALITY_BASE + 101, ENCODING_VP9_QUALITY_BASE + 255, ENCODING_VP9_QUALITY_BASE + 256] {
            assert_eq!(Vp9Stream::listed(&[crate::ENCODING_VP9, off], 90).quality, 90, "{off:#x}");
        }
    }
}
