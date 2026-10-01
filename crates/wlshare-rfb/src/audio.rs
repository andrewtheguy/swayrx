//! wlshare's audio extension: how the desktop's sound reaches a client over the
//! RFB connection it already has, as FLAC.
//!
//! The extension is private, and its client is the remotex gateway:
//!
//! - The client lists the pseudo-encoding [`crate::ENCODING_AUDIO`] (`WLSF`) in
//!   `SetEncodings`. The server announces support with an empty
//!   pseudo-rectangle of that encoding ([`audio_rect`]) in a
//!   `FramebufferUpdate` — the only way support is ever announced, as with
//!   ExtendedDesktopSize.
//! - The client then sets the sample format it wants
//!   ([`ClientAudio::SetFormat`]) and enables audio ([`ClientAudio::Enable`]);
//!   it may disable it again ([`ClientAudio::Disable`]). These three are the
//!   client messages of the QEMU Audio extension `rfbproto` registers — message
//!   type 255, submessage 1 — taken as they are, though QEMU's pseudo-encoding,
//!   -259, is not spoken: what it promises is raw samples, and none are sent.
//! - The server sends [`audio_begin`] when a stream starts and [`audio_end`]
//!   when it stops, QEMU's messages again, and between them the sound as FLAC
//!   frames, one to a message of the private type [`MSG_AUDIO_FRAME`]
//!   ([`FlacEncoder`]).
//!
//! FLAC is lossless: the client decodes exactly the samples the capture
//! produced, while music and speech take about two-thirds of their PCM rate or
//! less, and silence a few bytes a frame. The FLAC stream header (`STREAMINFO`)
//! is never sent, because everything in it is already agreed — the format is
//! the one the client set, and every frame carries [`AudioFormat::block_frames`]
//! frames of it, twenty milliseconds, so a client builds the header itself.
//! Each frame decodes on its own, so one a session dropped costs nothing but
//! its own samples.
//!
//! The tests read every frame back as a client does: `streaminfo` is the
//! header it builds, and `FlacDecoder` turns each frame back into samples in
//! the format it set.
//!
//! FLAC stores only signed samples, of at most 24 bits, so the formats are the
//! four of 8 and 16 bits. An unsigned sample has its top bit flipped before it
//! is encoded, which maps its range exactly onto the signed one of the same
//! width, with silence landing on zero; the client flips it back, and since the
//! flip is its own inverse it gets the original values bit for bit. Samples are
//! little-endian on both sides of the codec.

use desktop_flac::{Encoder, Stream};
use thiserror::Error;

/// The message type the client's messages and the server's begin and end use,
/// shared with every other QEMU extension; [`SUBMESSAGE_AUDIO`] names this one.
pub const MSG_QEMU: u8 = 255;
/// The submessage type under [`MSG_QEMU`] that is audio.
pub const SUBMESSAGE_AUDIO: u8 = 1;

/// The FLAC frame message's type, server → client only; outside every
/// registered type.
pub const MSG_AUDIO_FRAME: u8 = 0xE4;
/// The bytes of a FLAC frame message before the frame, type included.
pub const AUDIO_FRAME_HEADER_LEN: usize = 8;
/// The most bytes one FLAC frame may be. The largest block there is — twenty
/// milliseconds of 16-bit stereo at [`MAX_FREQUENCY`] — is 7680 bytes of
/// samples, and FLAC adds a few header bytes at worst, so a length past this
/// is a server that has lost its framing rather than a frame to buffer.
pub const MAX_AUDIO_FRAME: usize = 64 * 1024;

/// Client operation: start sending audio.
pub const CLIENT_AUDIO_ENABLE: u16 = 0;
/// Client operation: stop sending audio.
pub const CLIENT_AUDIO_DISABLE: u16 = 1;
/// Client operation: the sample format the client wants, which follows.
pub const CLIENT_AUDIO_SET_FORMAT: u16 = 2;

/// Server operation: the stream stopped.
pub const SERVER_AUDIO_END: u16 = 0;
/// Server operation: a stream started.
pub const SERVER_AUDIO_BEGIN: u16 = 1;

/// The bytes of an enable or a disable, type and submessage included.
pub const CLIENT_AUDIO_SWITCH_LEN: usize = 4;
/// The bytes of a set-format, type and submessage included.
pub const CLIENT_AUDIO_FORMAT_LEN: usize = 10;

/// The lowest sampling frequency a client may ask for.
///
/// A FLAC frame here is twenty milliseconds, and FLAC has no block shorter
/// than 16 frames, which puts the floor at 800 Hz; 8 kHz is the lowest rate
/// real audio uses, so nothing a client could legitimately want is refused.
pub const MIN_FREQUENCY: u32 = 8_000;

/// The highest sampling frequency a client may ask for.
///
/// The field is a `u32`, and a server that took it at its word would overflow
/// the buffer size in frames it asks PipeWire for. FLAC's own ceiling is the
/// twenty bits its stream header has for the rate; this one is twice what the
/// desktop's own graph runs at, and more than any client of this server asks
/// for.
pub const MAX_FREQUENCY: u32 = 96_000;

/// A sample's encoding, as QEMU's set-format numbers them. Its 32-bit codes,
/// 4 and 5, are not here: FLAC stores at most 24 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    U8 = 0,
    S8 = 1,
    U16 = 2,
    S16 = 3,
}

impl SampleFormat {
    /// The format a wire byte names, if it is one carried here.
    pub fn from_wire(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::U8,
            1 => Self::S8,
            2 => Self::U16,
            3 => Self::S16,
            _ => return None,
        })
    }

    /// One sample's width in bytes.
    pub fn bytes(self) -> usize {
        match self {
            Self::U8 | Self::S8 => 1,
            Self::U16 | Self::S16 => 2,
        }
    }

    /// Whether the top bit is flipped on the way into FLAC and back out.
    pub fn unsigned(self) -> bool {
        matches!(self, Self::U8 | Self::U16)
    }
}

/// The format a client asked for: what the samples every FLAC frame decodes to
/// are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample: SampleFormat,
    /// 1 or 2.
    pub channels: u8,
    /// Samples per second per channel.
    pub frequency: u32,
}

impl AudioFormat {
    /// What QEMU streams to a client that enables audio without setting a
    /// format: CD-quality signed 16-bit stereo.
    pub const DEFAULT: Self = Self { sample: SampleFormat::S16, channels: 2, frequency: 44_100 };

    /// The bytes of one frame: one sample per channel.
    pub fn frame_bytes(&self) -> usize {
        self.sample.bytes() * usize::from(self.channels)
    }

    /// The frames in every FLAC frame: twenty milliseconds, rounded down —
    /// 960 at 48 kHz, 882 at 44.1.
    pub fn block_frames(&self) -> usize {
        (self.frequency / 50) as usize
    }

    /// Whether this is a format the extension carries: 1 or 2 channels, at
    /// [`MIN_FREQUENCY`] to [`MAX_FREQUENCY`]. A set-format is parsed only
    /// into one that is, and nothing is encoded, decoded or sent in one that
    /// is not.
    pub fn check(&self) -> Result<(), AudioParseError> {
        if !(1..=2).contains(&self.channels) {
            return Err(AudioParseError::BadChannels(self.channels));
        }
        if self.frequency < MIN_FREQUENCY {
            return Err(AudioParseError::FrequencyTooLow(self.frequency));
        }
        if self.frequency > MAX_FREQUENCY {
            return Err(AudioParseError::FrequencyTooHigh(self.frequency));
        }
        Ok(())
    }
}

/// An audio submessage from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAudio {
    Enable,
    Disable,
    SetFormat(AudioFormat),
}

/// Why a client's audio submessage could not be one.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AudioParseError {
    #[error("QEMU submessage {0} is not audio, the one QEMU extension spoken here")]
    UnknownSubmessage(u8),
    #[error("audio operation {0} is not enable, disable or set-format")]
    UnknownOperation(u16),
    #[error("sample format {0} is not U8, S8, U16 or S16, the ones FLAC carries here")]
    UnknownSampleFormat(u8),
    #[error("{0} channels; the extension allows 1 or 2")]
    BadChannels(u8),
    #[error("a frequency of {0} Hz, under the {MIN_FREQUENCY} Hz this server accepts")]
    FrequencyTooLow(u32),
    #[error("a frequency of {0} Hz, over the {MAX_FREQUENCY} Hz this server accepts")]
    FrequencyTooHigh(u32),
}

/// Parse the audio submessage at the front of `buf`, whose first byte is
/// [`MSG_QEMU`]. `Ok(None)` means more bytes are needed.
pub fn parse_client(buf: &[u8]) -> Result<Option<(ClientAudio, usize)>, AudioParseError> {
    debug_assert_eq!(buf.first(), Some(&MSG_QEMU));
    if buf.len() < CLIENT_AUDIO_SWITCH_LEN {
        return Ok(None);
    }
    if buf[1] != SUBMESSAGE_AUDIO {
        return Err(AudioParseError::UnknownSubmessage(buf[1]));
    }
    let operation = u16::from_be_bytes([buf[2], buf[3]]);
    match operation {
        CLIENT_AUDIO_ENABLE => Ok(Some((ClientAudio::Enable, CLIENT_AUDIO_SWITCH_LEN))),
        CLIENT_AUDIO_DISABLE => Ok(Some((ClientAudio::Disable, CLIENT_AUDIO_SWITCH_LEN))),
        CLIENT_AUDIO_SET_FORMAT => {
            if buf.len() < CLIENT_AUDIO_FORMAT_LEN {
                return Ok(None);
            }
            let sample = SampleFormat::from_wire(buf[4]).ok_or(AudioParseError::UnknownSampleFormat(buf[4]))?;
            let format = AudioFormat { sample, channels: buf[5], frequency: u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]) };
            format.check()?;
            Ok(Some((ClientAudio::SetFormat(format), CLIENT_AUDIO_FORMAT_LEN)))
        }
        other => Err(AudioParseError::UnknownOperation(other)),
    }
}

/// The announcement: an empty pseudo-rectangle of [`crate::ENCODING_AUDIO`],
/// sent inside a `FramebufferUpdate` as its whole content.
pub fn audio_rect() -> [u8; 12] {
    crate::msg::rect_header(0, 0, 0, 0, crate::ENCODING_AUDIO)
}

fn server_op(operation: u16) -> [u8; 4] {
    let op = operation.to_be_bytes();
    [MSG_QEMU, SUBMESSAGE_AUDIO, op[0], op[1]]
}

/// Server → client: a stream is starting; FLAC frames follow.
pub fn audio_begin() -> [u8; 4] {
    server_op(SERVER_AUDIO_BEGIN)
}

/// Server → client: the stream stopped.
pub fn audio_end() -> [u8; 4] {
    server_op(SERVER_AUDIO_END)
}

/// Why a FLAC frame could not be made.
#[derive(Debug, Error)]
pub enum AudioEncodeError {
    /// The format is not one the extension carries.
    #[error(transparent)]
    Unsupported(#[from] AudioParseError),
    /// libFLAC is not on this system, or it refused the stream or a block of
    /// it.
    #[error(transparent)]
    Flac(#[from] desktop_flac::Error),
}

/// One stream's encoder: the capture's buffers in, [`MSG_AUDIO_FRAME`] messages
/// out, one for every [`AudioFormat::block_frames`] frames.
///
/// The capture's buffers are twenty milliseconds once PipeWire settles, but
/// its first ones are often shorter and a graph running a larger quantum hands
/// over more; a FLAC frame of fixed size is what makes the stream header the
/// client builds true, so the encoder keeps what does not fill a frame for the
/// next buffer. What is left when the stream stops is under twenty
/// milliseconds, and goes with it.
///
/// A frame is made the moment its last sample arrives, by `desktop-flac`'s
/// encoder, as a FLAC stream of its own, so every frame is numbered zero.
///
/// | Offset | Type | Field |
/// |---|---|---|
/// | 0 | U8 | 0xE4 |
/// | 1 | U8\[3\] | padding |
/// | 4 | U32 | length of the frame |
/// | 8 | U8[] | one FLAC frame |
pub struct FlacEncoder {
    format: AudioFormat,
    codec: Encoder,
    /// Samples not yet a whole frame's worth, as the capture gave them.
    pending: Vec<u8>,
    /// One block as libFLAC takes it, a signed integer to a sample, reused.
    samples: Vec<i32>,
}

impl FlacEncoder {
    /// An encoder for `format`, if it is one the extension carries and libFLAC
    /// is on this system to encode it.
    pub fn new(format: AudioFormat) -> Result<Self, AudioEncodeError> {
        format.check()?;
        let codec = Encoder::new(Stream {
            rate: format.frequency,
            channels: format.channels,
            bits: 8 * format.sample.bytes() as u8,
            block: format.block_frames() as u16,
        })?;
        Ok(Self { format, codec, pending: Vec::new(), samples: Vec::new() })
    }

    /// Take `samples`, interleaved little-endian in the client's format and a
    /// whole number of frames, and return a message for every FLAC frame they
    /// completed.
    pub fn push(&mut self, samples: &[u8]) -> Result<Vec<Vec<u8>>, AudioEncodeError> {
        debug_assert!(samples.len().is_multiple_of(self.format.frame_bytes()));
        self.pending.extend_from_slice(samples);
        let block_bytes = self.format.block_frames() * self.format.frame_bytes();
        let whole = self.pending.len() / block_bytes;
        let mut messages = Vec::with_capacity(whole);
        for i in 0..whole {
            let block = std::mem::take(&mut self.pending);
            let result = self.encode(&block[i * block_bytes..(i + 1) * block_bytes]);
            self.pending = block;
            messages.push(result?);
        }
        self.pending.drain(..whole * block_bytes);
        Ok(messages)
    }

    fn encode(&mut self, block: &[u8]) -> Result<Vec<u8>, AudioEncodeError> {
        // The top bit of an unsigned sample, flipped on the way in; it is in the
        // sample's last byte, the samples being little-endian.
        let flip = if self.format.sample.unsigned() { 0x80 } else { 0 };
        self.samples.clear();
        match self.format.sample.bytes() {
            1 => self.samples.extend(block.iter().map(|&sample| i32::from((sample ^ flip) as i8))),
            _ => self.samples.extend(block.as_chunks::<2>().0.iter().map(|&[low, high]| i32::from(i16::from_le_bytes([low, high ^ flip])))),
        }
        // The length is known once the frame is behind it.
        let mut msg = Vec::with_capacity(AUDIO_FRAME_HEADER_LEN + block.len());
        msg.extend_from_slice(&[MSG_AUDIO_FRAME, 0, 0, 0, 0, 0, 0, 0]);
        self.codec.encode(&self.samples, &mut msg)?;
        let length = (msg.len() - AUDIO_FRAME_HEADER_LEN) as u32;
        msg[4..AUDIO_FRAME_HEADER_LEN].copy_from_slice(&length.to_be_bytes());
        Ok(msg)
    }
}

/// The FLAC stream header a client builds from the format it set, as the FLAC
/// specification lays out `STREAMINFO`'s 34 bytes: nothing in it comes from the
/// server. The block size is [`AudioFormat::block_frames`] at both ends, and
/// the frame sizes, the total and the MD5 are unknown. A format
/// [`AudioFormat::check`] refuses has no header.
#[cfg(test)]
fn streaminfo(format: AudioFormat) -> Result<[u8; 34], AudioParseError> {
    format.check()?;
    let block = (format.block_frames() as u16).to_be_bytes();
    let mut info = [0u8; 34];
    info[0..2].copy_from_slice(&block);
    info[2..4].copy_from_slice(&block);
    // Rate (20 bits), channels - 1 (3), bits per sample - 1 (5), total samples
    // (36, unknown).
    let bits = 8 * format.sample.bytes() as u64;
    let packed = (u64::from(format.frequency) << 44) | (u64::from(format.channels - 1) << 41) | ((bits - 1) << 36);
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    Ok(info)
}

/// Why a FLAC frame could not be read back.
#[cfg(test)]
#[derive(Debug, Error, PartialEq, Eq)]
enum AudioDecodeError {
    /// The format is not one the extension carries.
    #[error(transparent)]
    Unsupported(#[from] AudioParseError),
    /// symphonia's own error is carried as its message.
    #[error("FLAC cannot carry this format: {0}")]
    Format(String),
    #[error("decoding a FLAC frame: {0}")]
    Decode(String),
    #[error("a FLAC frame of {got} frames of {channels} channels, where {want} of {expected} were agreed")]
    Shape { got: usize, channels: usize, want: usize, expected: usize },
}

/// One stream's decoder, the client's end of [`FlacEncoder`]: the frame a
/// [`MSG_AUDIO_FRAME`] message carries in, interleaved little-endian samples in
/// the client's format out. Made at a begin, from the format that was set.
///
/// Every frame decodes on its own, so a frame that fails costs its own twenty
/// milliseconds and the decoder goes on with the next.
#[cfg(test)]
struct FlacDecoder {
    format: AudioFormat,
    decoder: symphonia_bundle_flac::FlacDecoder,
    samples: Vec<i32>,
}

#[cfg(test)]
impl FlacDecoder {
    fn new(format: AudioFormat) -> Result<Self, AudioDecodeError> {
        use symphonia_core::codecs::audio::well_known::CODEC_ID_FLAC;
        use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};

        let info = streaminfo(format)?;
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_FLAC).with_extra_data(Box::new(info));
        let decoder = symphonia_bundle_flac::FlacDecoder::try_new(&params, &AudioDecoderOptions::default())
            .map_err(|e| AudioDecodeError::Format(e.to_string()))?;
        Ok(Self { format, decoder, samples: Vec::new() })
    }

    /// One frame — the bytes after a message's header — as
    /// [`AudioFormat::block_frames`] frames of interleaved little-endian
    /// samples in the format the stream was set to, bit for bit what the
    /// server captured.
    fn decode(&mut self, frame: &[u8]) -> Result<Vec<u8>, AudioDecodeError> {
        use symphonia_core::codecs::audio::AudioDecoder as _;
        use symphonia_core::packet::Packet;
        use symphonia_core::units::{Duration, Timestamp};

        let (block, channels) = (self.format.block_frames(), usize::from(self.format.channels));
        let packet = Packet::new(0, Timestamp::new(0), Duration::new(block as u64), frame.to_vec());
        let decoded = self.decoder.decode(&packet).map_err(|e| AudioDecodeError::Decode(e.to_string()))?;
        let got = (decoded.frames(), decoded.spec().channels().count());
        if got != (block, channels) {
            return Err(AudioDecodeError::Shape { got: got.0, channels: got.1, want: block, expected: channels });
        }
        self.samples.clear();
        decoded.copy_to_vec_interleaved(&mut self.samples);
        let width = self.format.sample.bytes();
        let mut out = Vec::with_capacity(self.samples.len() * width);
        for &sample in &self.samples {
            // The decoder scales to 32 bits; back down to the format's width.
            let mut bytes = (sample >> (32 - 8 * width)).to_le_bytes();
            if self.format.sample.unsigned() {
                bytes[width - 1] ^= 0x80;
            }
            out.extend_from_slice(&bytes[..width]);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_operations_parse() {
        assert_eq!(parse_client(&[255, 1, 0, 0]), Ok(Some((ClientAudio::Enable, 4))));
        assert_eq!(parse_client(&[255, 1, 0, 1, 9]), Ok(Some((ClientAudio::Disable, 4))));
        let set = [255, 1, 0, 2, 3, 2, 0, 0, 0xBB, 0x80];
        assert_eq!(
            parse_client(&set),
            Ok(Some((ClientAudio::SetFormat(AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 }), 10)))
        );
        assert_eq!(parse_client(&[255, 1, 0]), Ok(None));
        assert_eq!(parse_client(&set[..9]), Ok(None));
    }

    #[test]
    fn a_bad_submessage_operation_or_format_is_fatal() {
        assert_eq!(parse_client(&[255, 2, 0, 0]), Err(AudioParseError::UnknownSubmessage(2)));
        assert_eq!(parse_client(&[255, 1, 0, 3]), Err(AudioParseError::UnknownOperation(3)));
        assert_eq!(parse_client(&[255, 1, 0, 2, 6, 2, 0, 0, 0xBB, 0x80]), Err(AudioParseError::UnknownSampleFormat(6)));
        assert_eq!(parse_client(&[255, 1, 0, 2, 3, 3, 0, 0, 0xBB, 0x80]), Err(AudioParseError::BadChannels(3)));
        assert_eq!(parse_client(&[255, 1, 0, 2, 4, 2, 0, 0, 0xBB, 0x80]), Err(AudioParseError::UnknownSampleFormat(4)));
        assert_eq!(parse_client(&[255, 1, 0, 2, 3, 1, 0, 0, 0, 0]), Err(AudioParseError::FrequencyTooLow(0)));
    }

    /// The frequency is bounded at both ends. A `u32::MAX` accepted here would
    /// overflow the frame count the capture is asked for, and a rate under the
    /// floor makes a frame shorter than FLAC's encoder takes, so both bounds
    /// are parse rules rather than checks at the point of use.
    #[test]
    fn a_frequency_outside_the_bounds_is_fatal() {
        let set_at = |frequency: u32| {
            let f = frequency.to_be_bytes();
            parse_client(&[255, 1, 0, 2, 3, 2, f[0], f[1], f[2], f[3]])
        };
        assert!(matches!(set_at(MAX_FREQUENCY), Ok(Some((ClientAudio::SetFormat(_), _)))));
        assert_eq!(set_at(MAX_FREQUENCY + 1), Err(AudioParseError::FrequencyTooHigh(MAX_FREQUENCY + 1)));
        assert_eq!(set_at(u32::MAX), Err(AudioParseError::FrequencyTooHigh(u32::MAX)));
        assert!(matches!(set_at(MIN_FREQUENCY), Ok(Some((ClientAudio::SetFormat(_), _)))));
        assert_eq!(set_at(MIN_FREQUENCY - 1), Err(AudioParseError::FrequencyTooLow(MIN_FREQUENCY - 1)));
        // Every rate that survives leaves the daemon's frame count well inside
        // a u32 (`frequency * 20 / 1000` in `wlshare::audio`).
        assert!(MAX_FREQUENCY.checked_mul(20).is_some());
        // And the rates real clients ask for are all far below it.
        for rate in [8_000, 44_100, 48_000] {
            assert!(matches!(set_at(rate), Ok(Some((ClientAudio::SetFormat(_), _)))));
        }
    }

    #[test]
    fn sample_widths_frame_sizes_and_blocks() {
        assert_eq!(SampleFormat::from_wire(0), Some(SampleFormat::U8));
        assert_eq!(SampleFormat::from_wire(3), Some(SampleFormat::S16));
        assert_eq!(SampleFormat::from_wire(4), None, "32-bit is wider than FLAC stores");
        assert_eq!(SampleFormat::from_wire(5), None);
        assert_eq!(AudioFormat::DEFAULT.frame_bytes(), 4);
        assert_eq!(AudioFormat::DEFAULT.block_frames(), 882);
        assert_eq!(AudioFormat { sample: SampleFormat::U8, channels: 1, frequency: 8000 }.frame_bytes(), 1);
        assert_eq!(AudioFormat { sample: SampleFormat::U8, channels: 1, frequency: MIN_FREQUENCY }.block_frames(), 160);
        assert_eq!(AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 }.block_frames(), 960);
    }

    /// The server messages, read back by a decoder written from the
    /// documented layouts rather than from the builders.
    #[test]
    fn server_messages_have_the_documented_layouts() {
        assert_eq!(audio_rect(), [0, 0, 0, 0, 0, 0, 0, 0, b'W', b'L', b'S', b'F']);
        assert_eq!(audio_begin(), [255, 1, 0, 1]);
        assert_eq!(audio_end(), [255, 1, 0, 0]);
    }

    /// Decode a run of frame messages with [`FlacDecoder`] — symphonia's
    /// decoder, which shares nothing with libFLAC — checking each message's
    /// framing on the way.
    fn decode(format: AudioFormat, messages: &[Vec<u8>]) -> Vec<u8> {
        let mut decoder = FlacDecoder::new(format).unwrap();
        let mut out = Vec::new();
        for msg in messages {
            assert_eq!(msg[0], 0xE4);
            assert_eq!(&msg[1..4], &[0, 0, 0]);
            let len = u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
            assert_eq!(msg.len(), 8 + len);
            assert!(len <= MAX_AUDIO_FRAME);
            out.extend(decoder.decode(&msg[8..]).unwrap());
        }
        out
    }

    /// The header's fields where the FLAC specification puts them, read back
    /// bit by bit rather than with the packing that wrote them.
    #[test]
    fn the_streaminfo_has_the_specified_layout() {
        let info = streaminfo(AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 }).unwrap();
        assert_eq!(&info[0..4], &[0x03, 0xC0, 0x03, 0xC0], "960 at both ends");
        assert_eq!(&info[4..10], &[0; 6], "frame sizes unknown");
        // 48000 = 0x0BB80 in 20 bits, then 001 for two channels, then 01111 for
        // 16 bits, then a total of zero.
        assert_eq!(&info[10..14], &[0x0B, 0xB8, 0x02, 0xF0]);
        assert_eq!(&info[14..34], &[0; 20]);
    }

    /// A format outside the extension's has no header, encoder or decoder:
    /// no channels would underflow the header's field, and a rate past 20 bits
    /// would spill into the ones after it.
    #[test]
    fn a_format_the_extension_does_not_carry_is_refused_everywhere() {
        let good = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        for (format, error) in [
            (AudioFormat { channels: 0, ..good }, AudioParseError::BadChannels(0)),
            (AudioFormat { channels: 3, ..good }, AudioParseError::BadChannels(3)),
            (AudioFormat { frequency: MIN_FREQUENCY - 1, ..good }, AudioParseError::FrequencyTooLow(MIN_FREQUENCY - 1)),
            (AudioFormat { frequency: 1 << 20, ..good }, AudioParseError::FrequencyTooHigh(1 << 20)),
        ] {
            assert_eq!(streaminfo(format), Err(error.clone()));
            assert!(matches!(FlacEncoder::new(format), Err(AudioEncodeError::Unsupported(e)) if e == error));
            assert_eq!(FlacDecoder::new(format).err(), Some(AudioDecodeError::Unsupported(error.clone())));
            assert_eq!(crate::client::audio_set_format(&format), Err(error));
        }
    }

    /// A frame that is not one, and a frame of the wrong length for the format
    /// that was set, are errors rather than samples: the client drops that
    /// frame and goes on with the next.
    #[test]
    fn a_frame_that_does_not_decode_to_the_agreed_block_is_refused() {
        let format = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        let mut decoder = FlacDecoder::new(format).unwrap();
        assert!(matches!(decoder.decode(&[0xFF, 0xF8, 1, 2, 3]), Err(AudioDecodeError::Decode(_))));

        let other = AudioFormat { frequency: 44_100, ..format };
        let messages = FlacEncoder::new(other).unwrap().push(&signal(other, other.block_frames())).unwrap();
        assert!(decoder.decode(&messages[0][8..]).is_err(), "882 frames where 960 were agreed");

        // And the decoder is still good for a frame that is right.
        let pcm = signal(format, format.block_frames());
        let messages = FlacEncoder::new(format).unwrap().push(&pcm).unwrap();
        assert_eq!(decoder.decode(&messages[0][8..]).unwrap(), pcm);
    }

    /// A tone with noise on it, in whatever format: every byte a sample of it,
    /// so the full range of each width is exercised.
    fn signal(format: AudioFormat, frames: usize) -> Vec<u8> {
        let mut seed = 0x2545_f491_u32;
        let mut out = Vec::with_capacity(frames * format.frame_bytes());
        for n in 0..frames {
            for c in 0..format.channels {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let t = n as f64 / f64::from(format.frequency);
                let tone = (t * 440.0 * (1.0 + f64::from(c)) * std::f64::consts::TAU).sin() * 0.7;
                let noise = (f64::from(seed) / f64::from(u32::MAX) - 0.5) * 0.2;
                let full = ((tone + noise).clamp(-1.0, 1.0) * f64::from(i32::MAX)) as i32;
                let width = format.sample.bytes();
                let mut bytes = (full >> (32 - 8 * width)).to_le_bytes();
                if format.sample.unsigned() {
                    bytes[width - 1] ^= 0x80;
                }
                out.extend_from_slice(&bytes[..width]);
            }
        }
        out
    }

    /// Every format, channel count and a spread of rates decodes to exactly
    /// the samples that went in, fed in buffers that do not line up with the
    /// frames. 70001 Hz is a rate no frame header can state, so its frames
    /// leave it to the header the client builds.
    #[test]
    fn every_format_round_trips_bit_for_bit() {
        for sample in [SampleFormat::U8, SampleFormat::S8, SampleFormat::U16, SampleFormat::S16] {
            for channels in [1, 2] {
                for frequency in [MIN_FREQUENCY, 11_025, 44_100, 48_000, 70_001, MAX_FREQUENCY] {
                    let format = AudioFormat { sample, channels, frequency };
                    let blocks = 5;
                    let pcm = signal(format, blocks * format.block_frames());
                    let mut encoder = FlacEncoder::new(format).unwrap();
                    let mut messages = Vec::new();
                    // Uneven buffers of whole frames: short ones first, as
                    // PipeWire's are, then longer than a block.
                    let mut rest = &pcm[..];
                    for frames in [7, 100, format.block_frames() * 2 + 3].iter().cycle() {
                        if rest.is_empty() {
                            break;
                        }
                        let take = (frames * format.frame_bytes()).min(rest.len());
                        messages.extend(encoder.push(&rest[..take]).unwrap());
                        rest = &rest[take..];
                    }
                    assert_eq!(messages.len(), blocks, "{format:?}");
                    assert!(decode(format, &messages) == pcm, "{format:?} did not survive");
                }
            }
        }
    }

    /// What does not fill a frame waits for the samples that do, and nothing is
    /// sent for it until then.
    #[test]
    fn a_partial_frame_waits_for_the_rest() {
        let format = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        let pcm = signal(format, 960);
        let mut encoder = FlacEncoder::new(format).unwrap();
        assert!(encoder.push(&pcm[..959 * 4]).unwrap().is_empty());
        let messages = encoder.push(&pcm[959 * 4..]).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(decode(format, &messages), pcm);
    }

    /// Silence, the state a desktop is in most of the time, is a few bytes a
    /// frame rather than the 3840 of its samples.
    #[test]
    fn silence_is_a_few_bytes() {
        for sample in [SampleFormat::U8, SampleFormat::S16] {
            let format = AudioFormat { sample, channels: 2, frequency: 48_000 };
            let silence: Vec<u8> = match sample {
                SampleFormat::U8 => vec![0x80; 960 * 2],
                _ => vec![0; 960 * 4],
            };
            let mut encoder = FlacEncoder::new(format).unwrap();
            let messages = encoder.push(&silence).unwrap();
            assert_eq!(messages.len(), 1);
            assert!(messages[0].len() < 32, "{sample:?}: {} bytes", messages[0].len());
            assert_eq!(decode(format, &messages), silence);
        }
    }
}
