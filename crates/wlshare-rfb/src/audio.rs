//! wlshare's audio extension: how the desktop's sound reaches a client over the
//! RFB connection it already has, as FLAC or as Opus, whichever the client
//! asked for.
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
//!   when it stops, QEMU's messages again, and between them the sound, one
//!   frame of it to a message of the private type [`MSG_AUDIO_FRAME`]: a FLAC
//!   frame ([`FlacEncoder`]), or an Opus packet ([`OpusEncoder`]) for a client
//!   that listed [`crate::ENCODING_AUDIO_OPUS`] beside the encoding
//!   ([`Codec`]). Which is the client's to choose and no configuration of the
//!   server's, as the format is.
//! - A client that asked for Opus may name the rate it is coded at
//!   ([`ClientAudio::SetBitrate`]), before the stream or while it runs: a
//!   private operation beside QEMU's three.
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
//! the format it set; an Opus stream's packets are decoded by FFmpeg behind the
//! `OpusHead` a client builds.
//!
//! FLAC stores only signed samples, of at most 24 bits, so the formats are the
//! four of 8 and 16 bits. An unsigned sample has its top bit flipped before it
//! is encoded, which maps its range exactly onto the signed one of the same
//! width, with silence landing on zero; the client flips it back, and since the
//! flip is its own inverse it gets the original values bit for bit. Samples are
//! little-endian on both sides of the codec.
//!
//! Opus is the lossy choice, for a client whose own listener takes Opus: the
//! remotex gateway hands each packet to the browser as it came, where it would
//! otherwise decode the FLAC and code Opus itself. A packet is twenty
//! milliseconds too, at the one frame size the whole stream keeps, and decodes
//! from the packets before it as any Opus stream does, and nothing on the
//! wire numbers them, so a server must send every packet it coded: one that
//! has to lose sound loses it before the encoder. The format's frequency
//! must be one Opus codes at ([`OPUS_FREQUENCIES`]); its sample format says
//! only what the capture hands the encoder, since what an Opus decoder gives
//! back is its own business. The coding is `desktop-opus`'s, the one place a
//! desktop's sound is made Opus for wlshare and the gateway alike, with libopus
//! linked statically under it.

use desktop_flac::{Encoder, Stream};
use thiserror::Error;

/// The message type the client's messages and the server's begin and end use,
/// shared with every other QEMU extension; [`SUBMESSAGE_AUDIO`] names this one.
pub const MSG_QEMU: u8 = 255;
/// The submessage type under [`MSG_QEMU`] that is audio.
pub const SUBMESSAGE_AUDIO: u8 = 1;

/// The frame message's type, server → client only; outside every registered
/// type. One FLAC frame or one Opus packet follows its header.
pub const MSG_AUDIO_FRAME: u8 = 0xE4;
/// The bytes of a frame message before the frame, type included.
pub const AUDIO_FRAME_HEADER_LEN: usize = 8;
/// The most bytes one frame may be. The largest block there is — twenty
/// milliseconds of 16-bit stereo at [`MAX_FREQUENCY`] — is 7680 bytes of
/// samples, and FLAC adds a few header bytes at worst, while an Opus packet is
/// under [`OPUS_MAX_PACKET`], so a length past this is a server that has lost
/// its framing rather than a frame to buffer.
pub const MAX_AUDIO_FRAME: usize = 64 * 1024;

/// Client operation: start sending audio.
pub const CLIENT_AUDIO_ENABLE: u16 = 0;
/// Client operation: stop sending audio.
pub const CLIENT_AUDIO_DISABLE: u16 = 1;
/// Client operation: the sample format the client wants, which follows.
pub const CLIENT_AUDIO_SET_FORMAT: u16 = 2;
/// Client operation: the rate Opus is coded at, which follows. wlshare's own,
/// beside QEMU's three.
pub const CLIENT_AUDIO_SET_BITRATE: u16 = 3;

/// Server operation: the stream stopped.
pub const SERVER_AUDIO_END: u16 = 0;
/// Server operation: a stream started.
pub const SERVER_AUDIO_BEGIN: u16 = 1;

/// The bytes of an enable or a disable, type and submessage included.
pub const CLIENT_AUDIO_SWITCH_LEN: usize = 4;
/// The bytes of a set-format, type and submessage included.
pub const CLIENT_AUDIO_FORMAT_LEN: usize = 10;
/// The bytes of a set-bitrate, type and submessage included.
pub const CLIENT_AUDIO_BITRATE_LEN: usize = 8;

/// The rates Opus codes at, in Hz: the only frequencies a format coded as Opus
/// may have.
pub use desktop_opus::RATES as OPUS_FREQUENCIES;
/// The Opus rate, in bits per second, of a stream whose client named none.
pub use desktop_opus::BITRATE_DEFAULT as OPUS_BITRATE_DEFAULT;
/// The lowest and highest Opus rates a client may ask for, in bits per second.
pub use desktop_opus::{BITRATE_MAX as OPUS_BITRATE_MAX, BITRATE_MIN as OPUS_BITRATE_MIN};
/// The most bytes one Opus packet is.
pub use desktop_opus::MAX_PACKET as OPUS_MAX_PACKET;
/// The samples, at 48 kHz, a decoder is to discard from the start of an Opus
/// stream, which the `OpusHead` a client builds states as its pre-skip: nothing
/// of that header is sent.
pub use desktop_opus::PRE_SKIP as OPUS_PRE_SKIP;

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
/// are in, and what the capture hands the Opus encoder.
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
    /// The rate Opus is coded at, in bits per second.
    SetBitrate(u32),
}

/// What the sound is coded as, which the client chooses in its `SetEncodings`:
/// Opus where [`crate::ENCODING_AUDIO_OPUS`] is listed beside
/// [`crate::ENCODING_AUDIO`], FLAC where it is not. A pseudo-encoding rather
/// than a message, as the VP9 stream's choices are, so it rides the list that
/// asks for the sound at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Flac,
    Opus,
}

impl Codec {
    /// The codec `encodings` asks for.
    pub fn listed(encodings: &[i32]) -> Self {
        if encodings.contains(&crate::ENCODING_AUDIO_OPUS) { Self::Opus } else { Self::Flac }
    }
}

/// Why a client's audio submessage could not be one.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AudioParseError {
    #[error("QEMU submessage {0} is not audio, the one QEMU extension spoken here")]
    UnknownSubmessage(u8),
    #[error("audio operation {0} is not enable, disable, set-format or set-bitrate")]
    UnknownOperation(u16),
    #[error("sample format {0} is not U8, S8, U16 or S16, the ones FLAC carries here")]
    UnknownSampleFormat(u8),
    #[error("{0} channels; the extension allows 1 or 2")]
    BadChannels(u8),
    #[error("a frequency of {0} Hz, under the {MIN_FREQUENCY} Hz this server accepts")]
    FrequencyTooLow(u32),
    #[error("a frequency of {0} Hz, over the {MAX_FREQUENCY} Hz this server accepts")]
    FrequencyTooHigh(u32),
    #[error("an Opus rate of {0} bit/s, outside the {OPUS_BITRATE_MIN} to {OPUS_BITRATE_MAX} Opus is coded at")]
    BadBitrate(u32),
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
        CLIENT_AUDIO_SET_BITRATE => {
            if buf.len() < CLIENT_AUDIO_BITRATE_LEN {
                return Ok(None);
            }
            let bitrate = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
            check_bitrate(bitrate)?;
            Ok(Some((ClientAudio::SetBitrate(bitrate), CLIENT_AUDIO_BITRATE_LEN)))
        }
        other => Err(AudioParseError::UnknownOperation(other)),
    }
}

/// Whether `bitrate`, in bits per second, is one Opus is coded at here. A
/// set-bitrate is parsed only into one that is.
pub fn check_bitrate(bitrate: u32) -> Result<(), AudioParseError> {
    if (OPUS_BITRATE_MIN..=OPUS_BITRATE_MAX).contains(&bitrate) { Ok(()) } else { Err(AudioParseError::BadBitrate(bitrate)) }
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

/// Server → client: a stream is starting; its frames follow.
pub fn audio_begin() -> [u8; 4] {
    server_op(SERVER_AUDIO_BEGIN)
}

/// Server → client: the stream stopped.
pub fn audio_end() -> [u8; 4] {
    server_op(SERVER_AUDIO_END)
}

/// Why a frame could not be made.
#[derive(Debug, Error)]
pub enum AudioEncodeError {
    /// The format or the rate is not one the extension carries.
    #[error(transparent)]
    Unsupported(#[from] AudioParseError),
    /// The format's frequency is not one Opus codes at, or libopus refused
    /// the stream, a rate or a block of it.
    #[error(transparent)]
    Opus(#[from] desktop_opus::Error),
    /// libFLAC is not on this system, or it refused the stream or a block of
    /// it.
    #[error(transparent)]
    Flac(#[from] desktop_flac::Error),
}

/// A frame message with nothing in it yet: the header, its length to be
/// written once the frame is behind it ([`finish`]).
fn begin(capacity: usize) -> Vec<u8> {
    let mut msg = Vec::with_capacity(AUDIO_FRAME_HEADER_LEN + capacity);
    msg.extend_from_slice(&[MSG_AUDIO_FRAME, 0, 0, 0, 0, 0, 0, 0]);
    msg
}

fn finish(mut msg: Vec<u8>) -> Vec<u8> {
    let length = (msg.len() - AUDIO_FRAME_HEADER_LEN) as u32;
    msg[4..AUDIO_FRAME_HEADER_LEN].copy_from_slice(&length.to_be_bytes());
    msg
}

/// Samples held until they fill a block: what the capture gave that did not,
/// kept for its next buffer.
#[derive(Default)]
struct Pending(Vec<u8>);

impl Pending {
    /// Add `samples` and hand every whole block of `block_bytes` to `encode`,
    /// in order, keeping what is left over.
    fn push<E>(&mut self, samples: &[u8], block_bytes: usize, mut encode: impl FnMut(&[u8]) -> Result<Vec<u8>, E>) -> Result<Vec<Vec<u8>>, E> {
        self.0.extend_from_slice(samples);
        let whole = self.0.len() / block_bytes;
        let messages = self.0.chunks_exact(block_bytes).map(&mut encode).collect::<Result<Vec<_>, E>>();
        self.0.drain(..whole * block_bytes);
        messages
    }
}

/// One stream's FLAC encoder: the capture's buffers in, [`MSG_AUDIO_FRAME`]
/// messages out, one for every [`AudioFormat::block_frames`] frames.
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
    pending: Pending,
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
        Ok(Self { format, codec, pending: Pending::default(), samples: Vec::new() })
    }

    /// Take `samples`, interleaved little-endian in the client's format and a
    /// whole number of frames, and return a message for every FLAC frame they
    /// completed.
    pub fn push(&mut self, samples: &[u8]) -> Result<Vec<Vec<u8>>, AudioEncodeError> {
        debug_assert!(samples.len().is_multiple_of(self.format.frame_bytes()));
        let block_bytes = self.format.block_frames() * self.format.frame_bytes();
        let mut pending = std::mem::take(&mut self.pending);
        let messages = pending.push(samples, block_bytes, |block| self.encode(block));
        self.pending = pending;
        messages
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
        let mut msg = begin(block.len());
        self.codec.encode(&self.samples, &mut msg)?;
        Ok(finish(msg))
    }
}

/// One stream's Opus encoder: the capture's buffers in, [`MSG_AUDIO_FRAME`]
/// messages out, one packet for every [`AudioFormat::block_frames`] frames,
/// twenty milliseconds, as [`FlacEncoder`]'s are and with the same message
/// around it.
///
/// The packets are `desktop-opus`'s, which the remotex gateway codes the sound
/// of a desktop it encodes itself with, so a passed stream and one made there
/// are the same. No header goes with them: a client builds `OpusHead` from the
/// format it set and [`OPUS_PRE_SKIP`].
pub struct OpusEncoder {
    format: AudioFormat,
    codec: desktop_opus::Encoder,
    /// Samples not yet a whole packet's worth, as the capture gave them.
    pending: Pending,
    /// One block as the encoder takes it, signed 16-bit, reused.
    samples: Vec<i16>,
}

impl OpusEncoder {
    /// An encoder for `format` at `bitrate` bits per second, if both are ones
    /// the extension carries as Opus.
    pub fn new(format: AudioFormat, bitrate: u32) -> Result<Self, AudioEncodeError> {
        format.check()?;
        check_bitrate(bitrate)?;
        let codec = desktop_opus::Encoder::new(desktop_opus::Stream { rate: format.frequency, channels: format.channels }, bitrate)?;
        Ok(Self { format, codec, pending: Pending::default(), samples: Vec::new() })
    }

    /// Move the rate to `bitrate` bits per second, from the next packet on.
    /// Nothing else changes, and a decoder needs no telling: a packet carries
    /// its own coding parameters.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), AudioEncodeError> {
        check_bitrate(bitrate)?;
        Ok(self.codec.set_bitrate(bitrate)?)
    }

    /// Take `samples`, interleaved little-endian in the client's format and a
    /// whole number of frames, and return a message for every Opus packet they
    /// completed.
    pub fn push(&mut self, samples: &[u8]) -> Result<Vec<Vec<u8>>, AudioEncodeError> {
        debug_assert!(samples.len().is_multiple_of(self.format.frame_bytes()));
        let block_bytes = self.format.block_frames() * self.format.frame_bytes();
        let mut pending = std::mem::take(&mut self.pending);
        let messages = pending.push(samples, block_bytes, |block| self.encode(block));
        self.pending = pending;
        messages
    }

    fn encode(&mut self, block: &[u8]) -> Result<Vec<u8>, AudioEncodeError> {
        // The encoder takes signed 16-bit: an unsigned sample has its top bit
        // flipped, as for FLAC, and an 8-bit one becomes the high byte.
        let flip = if self.format.sample.unsigned() { 0x80 } else { 0 };
        self.samples.clear();
        match self.format.sample.bytes() {
            1 => self.samples.extend(block.iter().map(|&sample| i16::from((sample ^ flip) as i8) << 8)),
            _ => self.samples.extend(block.as_chunks::<2>().0.iter().map(|&[low, high]| i16::from_le_bytes([low, high ^ flip]))),
        }
        let mut msg = begin(block.len());
        self.codec.encode(&self.samples, &mut msg)?;
        Ok(finish(msg))
    }
}

/// One stream's encoder, in the codec its client asked for.
pub enum AudioEncoder {
    Flac(FlacEncoder),
    Opus(OpusEncoder),
}

impl AudioEncoder {
    /// An encoder of `codec` for `format`; `bitrate`, in bits per second, is
    /// the rate Opus starts at, and nothing to FLAC.
    pub fn new(codec: Codec, format: AudioFormat, bitrate: u32) -> Result<Self, AudioEncodeError> {
        Ok(match codec {
            Codec::Flac => Self::Flac(FlacEncoder::new(format)?),
            Codec::Opus => Self::Opus(OpusEncoder::new(format, bitrate)?),
        })
    }

    /// A message for every frame `samples` completed: see [`FlacEncoder::push`]
    /// and [`OpusEncoder::push`].
    pub fn push(&mut self, samples: &[u8]) -> Result<Vec<Vec<u8>>, AudioEncodeError> {
        match self {
            Self::Flac(encoder) => encoder.push(samples),
            Self::Opus(encoder) => encoder.push(samples),
        }
    }

    /// Move the rate Opus is coded at; FLAC has none.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), AudioEncodeError> {
        match self {
            Self::Flac(_) => Ok(()),
            Self::Opus(encoder) => encoder.set_bitrate(bitrate),
        }
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
        assert_eq!(parse_client(&[255, 1, 0, 4]), Err(AudioParseError::UnknownOperation(4)));
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

    #[test]
    fn the_bitrate_parses_within_libopus_bounds() {
        let set_at = |bitrate: u32| {
            let b = bitrate.to_be_bytes();
            parse_client(&[255, 1, 0, 3, b[0], b[1], b[2], b[3]])
        };
        assert_eq!(set_at(96_000), Ok(Some((ClientAudio::SetBitrate(96_000), 8))));
        assert_eq!(parse_client(&[255, 1, 0, 3, 0, 1, 0x77]), Ok(None));
        assert!(set_at(OPUS_BITRATE_MIN).is_ok() && set_at(OPUS_BITRATE_MAX).is_ok());
        for bitrate in [0, OPUS_BITRATE_MIN - 1, OPUS_BITRATE_MAX + 1, u32::MAX] {
            assert_eq!(set_at(bitrate), Err(AudioParseError::BadBitrate(bitrate)));
        }
        assert_eq!((OPUS_BITRATE_MIN, OPUS_BITRATE_DEFAULT, OPUS_BITRATE_MAX), (6_000, 96_000, 510_000));
    }

    /// The codec is FLAC unless the list that asks for the sound says Opus.
    #[test]
    fn the_codec_is_the_one_the_encodings_list() {
        assert_eq!(crate::ENCODING_AUDIO_OPUS.to_be_bytes(), *b"WLOP");
        assert_eq!(Codec::listed(&[crate::ENCODING_ZRLE, crate::ENCODING_AUDIO]), Codec::Flac);
        assert_eq!(Codec::listed(&[crate::ENCODING_AUDIO, crate::ENCODING_AUDIO_OPUS]), Codec::Opus);
        assert_eq!(Codec::listed(&[]), Codec::Flac);
    }

    /// One Ogg page holding `packet` whole, as RFC 3533 lays a page out, its
    /// checksum the format's own CRC-32: unreflected, from zero.
    fn ogg_page(kind: u8, granule: u64, sequence: u32, packet: &[u8]) -> Vec<u8> {
        let mut page = Vec::from(*b"OggS");
        page.extend_from_slice(&[0, kind]);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&1u32.to_le_bytes());
        page.extend_from_slice(&sequence.to_le_bytes());
        page.extend_from_slice(&[0; 4]);
        // A packet is a run of 255-byte segments and one shorter, which ends it.
        let full = packet.len() / 255;
        page.push(full as u8 + 1);
        page.extend(std::iter::repeat_n(255, full));
        page.push((packet.len() % 255) as u8);
        page.extend_from_slice(packet);
        let mut crc = 0u32;
        for &byte in &page {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
            }
        }
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        page
    }

    /// Decode a run of Opus frame messages as a client would have them
    /// decoded: behind the `OpusHead` it builds from the format it set, as
    /// RFC 7845 lays one out, by FFmpeg's own Opus decoder — which shares
    /// nothing with libopus, and must be on the path as `ffmpeg` — checking
    /// each message's framing on the way. The samples come back signed 16-bit
    /// at 48 kHz in the format's channels; the head states no pre-skip, so
    /// that every packet's twenty milliseconds are among them.
    fn decode_opus(format: AudioFormat, messages: &[Vec<u8>]) -> Vec<i16> {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let mut head = Vec::from(*b"OpusHead");
        head.extend_from_slice(&[1, format.channels, 0, 0]);
        head.extend_from_slice(&format.frequency.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]);
        let mut file = ogg_page(2, 0, 0, &head);
        file.extend(ogg_page(0, 0, 1, b"OpusTags\x04\0\0\0test\0\0\0\0"));
        for (n, msg) in messages.iter().enumerate() {
            assert_eq!(msg[0], 0xE4);
            assert_eq!(&msg[1..4], &[0, 0, 0]);
            let len = u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
            assert_eq!(msg.len(), 8 + len);
            assert!(len > 0 && len <= OPUS_MAX_PACKET);
            let last = if n + 1 == messages.len() { 4 } else { 0 };
            file.extend(ogg_page(last, (n as u64 + 1) * 960, n as u32 + 2, &msg[8..]));
        }
        let channels = format.channels.to_string();
        let mut ffmpeg = Command::new("ffmpeg")
            // `-c:a opus` before the input names FFmpeg's decoder rather than
            // its wrapper around libopus.
            .args(["-v", "error", "-c:a", "opus", "-i", "pipe:0", "-f", "s16le", "-ar", "48000", "-ac", &channels, "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the Opus tests decode with ffmpeg, which must be installed");
        let mut stdin = ffmpeg.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(&file));
        let output = ffmpeg.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(output.status.success(), "ffmpeg refused the stream");
        let decoded: Vec<i16> = output.stdout.as_chunks::<2>().0.iter().map(|&pair| i16::from_le_bytes(pair)).collect();
        assert_eq!(decoded.len(), messages.len() * 960 * usize::from(format.channels), "every packet is twenty milliseconds");
        decoded
    }

    /// A tone on each channel, an octave apart, at half scale in whatever
    /// format: Opus is lossy, so what survives is the tone and where it is.
    fn tone(format: AudioFormat, frames: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * format.frame_bytes());
        for n in 0..frames {
            for c in 0..format.channels {
                let t = n as f64 / f64::from(format.frequency);
                let full = ((t * 440.0 * (1.0 + f64::from(c)) * std::f64::consts::TAU).sin() * 0.5 * f64::from(i32::MAX)) as i32;
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

    /// The mean square of every `step`th sample from `from`, past the first
    /// fifth, where the encoder is still settling.
    fn energy(samples: &[i16], from: usize, step: usize) -> f64 {
        let settled = &samples[samples.len() / 5..];
        let picked: Vec<f64> = settled.iter().skip(from).step_by(step).map(|&s| f64::from(s)).collect();
        picked.iter().map(|s| s * s).sum::<f64>() / picked.len() as f64
    }

    /// Every sample format and channel count, at every rate Opus codes at,
    /// comes back from an independent decoder as the half-scale tone that went
    /// in, fed in buffers that do not line up with the packets: a wrong flip
    /// or width would be noise or a different level.
    #[test]
    fn every_format_survives_opus() {
        // A sine at half of full scale.
        let expected = (0.5 * 32_768.0_f64).powi(2) / 2.0;
        for sample in [SampleFormat::U8, SampleFormat::S8, SampleFormat::U16, SampleFormat::S16] {
            for channels in [1, 2] {
                for frequency in OPUS_FREQUENCIES {
                    let format = AudioFormat { sample, channels, frequency };
                    let blocks = 25;
                    let pcm = tone(format, blocks * format.block_frames());
                    let mut encoder = OpusEncoder::new(format, OPUS_BITRATE_DEFAULT).unwrap();
                    let mut messages = Vec::new();
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
                    let decoded = decode_opus(format, &messages);
                    for channel in 0..usize::from(channels) {
                        let ratio = energy(&decoded, channel, usize::from(channels)) / expected;
                        assert!((0.6..1.6).contains(&ratio), "{format:?} channel {channel}: {ratio}");
                    }
                }
            }
        }
    }

    /// A sound on the left alone is on the left alone after the trip.
    #[test]
    fn opus_keeps_the_channels_apart() {
        let format = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        let mut pcm = tone(format, 25 * 960);
        pcm.as_chunks_mut::<4>().0.iter_mut().for_each(|frame| frame[2..].fill(0));
        let messages = OpusEncoder::new(format, OPUS_BITRATE_DEFAULT).unwrap().push(&pcm).unwrap();
        let decoded = decode_opus(format, &messages);
        let (left, right) = (energy(&decoded, 0, 2), energy(&decoded, 1, 2));
        assert!(left > 1_000_000.0 && right * 10.0 < left, "{left} against {right}");
    }

    /// A frequency Opus does not code at, and a rate outside libopus's, have
    /// no Opus encoder; FLAC carries the same format.
    #[test]
    fn opus_refuses_a_frequency_or_a_rate_it_does_not_code_at() {
        let good = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        for frequency in [44_100, 11_025, MAX_FREQUENCY] {
            let format = AudioFormat { frequency, ..good };
            assert!(matches!(OpusEncoder::new(format, OPUS_BITRATE_DEFAULT), Err(AudioEncodeError::Opus(desktop_opus::Error::Unsupported(_)))));
            assert!(matches!(AudioEncoder::new(Codec::Opus, format, OPUS_BITRATE_DEFAULT), Err(AudioEncodeError::Opus(_))));
            assert!(AudioEncoder::new(Codec::Flac, format, OPUS_BITRATE_DEFAULT).is_ok());
        }
        assert!(matches!(OpusEncoder::new(AudioFormat { channels: 3, ..good }, OPUS_BITRATE_DEFAULT), Err(AudioEncodeError::Unsupported(AudioParseError::BadChannels(3)))));
        assert!(matches!(OpusEncoder::new(good, 1), Err(AudioEncodeError::Unsupported(AudioParseError::BadBitrate(1)))));
        let mut encoder = OpusEncoder::new(good, OPUS_BITRATE_DEFAULT).unwrap();
        assert!(matches!(encoder.set_bitrate(u32::MAX), Err(AudioEncodeError::Unsupported(AudioParseError::BadBitrate(u32::MAX)))));
    }

    /// What does not fill a packet waits, as for FLAC, and silence is a few
    /// bytes a packet.
    #[test]
    fn an_opus_packet_waits_for_its_block_and_silence_is_small() {
        let format = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        let mut encoder = OpusEncoder::new(format, OPUS_BITRATE_DEFAULT).unwrap();
        assert!(encoder.push(&vec![0; 959 * 4]).unwrap().is_empty());
        assert_eq!(encoder.push(&[0; 4]).unwrap().len(), 1);
        let messages = encoder.push(&vec![0; 10 * 960 * 4]).unwrap();
        assert_eq!(messages.len(), 10);
        assert!(messages.iter().skip(2).all(|msg| msg.len() < 8 + 16), "{:?}", messages.iter().map(Vec::len).collect::<Vec<_>>());
    }

    /// The rate moves on a running stream, through the encoder a capture
    /// holds, and the decoder reads across the change; FLAC takes the message
    /// and has nothing to move.
    #[test]
    fn the_opus_rate_moves_on_a_running_stream() {
        let format = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        let pcm = tone(format, 20 * 960);
        let mut encoder = AudioEncoder::new(Codec::Opus, format, 96_000).unwrap();
        let before = encoder.push(&pcm).unwrap();
        encoder.set_bitrate(16_000).unwrap();
        let after = encoder.push(&pcm).unwrap();
        let average = |messages: &[Vec<u8>]| messages.iter().map(Vec::len).sum::<usize>() / messages.len();
        assert!(average(&after) * 2 < average(&before), "{} against {}", average(&after), average(&before));
        let all: Vec<Vec<u8>> = before.into_iter().chain(after).collect();
        assert!(energy(&decode_opus(format, &all), 0, 2) > 1_000_000.0);

        let mut flac = AudioEncoder::new(Codec::Flac, format, 96_000).unwrap();
        flac.set_bitrate(16_000).unwrap();
        assert_eq!(decode(format, &flac.push(&pcm[..960 * 4]).unwrap()), &pcm[..960 * 4]);
    }

    /// The pre-skip a client's `OpusHead` states is the encoder's own.
    #[test]
    fn the_pre_skip_is_the_encoders_lookahead() {
        assert_eq!(OPUS_PRE_SKIP, 312);
        for rate in OPUS_FREQUENCIES {
            let mut encoder = desktop_opus::Encoder::new(desktop_opus::Stream { rate, channels: 2 }, OPUS_BITRATE_DEFAULT).unwrap();
            assert_eq!(encoder.pre_skip().unwrap(), OPUS_PRE_SKIP);
        }
    }
}
