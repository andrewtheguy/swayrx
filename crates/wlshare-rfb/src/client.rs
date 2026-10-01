//! The client's half of the wire, which the tests read the server's with: the
//! messages a client sends, built into bytes, and the server's, parsed from a
//! byte buffer.
//!
//! This is [`crate::msg`] seen from the other end — it frames exactly what
//! wlshare sends and nothing a server of another kind might. The builders are
//! checked against [`crate::msg::parse`], and the parser against the builders
//! the daemon sends with, so neither half is tested against itself.
//!
//! Parsing is incremental, as the server's is: [`parse`] looks at the front of
//! whatever has been read and returns a message and its length, asks for more,
//! or fails. A rectangle's pixels are framed here and not decoded.

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::audio::{
    AUDIO_FRAME_HEADER_LEN, AudioFormat, AudioParseError, CLIENT_AUDIO_BITRATE_LEN, CLIENT_AUDIO_DISABLE, CLIENT_AUDIO_ENABLE, CLIENT_AUDIO_FORMAT_LEN,
    CLIENT_AUDIO_SET_BITRATE, CLIENT_AUDIO_SET_FORMAT,
    CLIENT_AUDIO_SWITCH_LEN, MAX_AUDIO_FRAME, MSG_AUDIO_FRAME, MSG_QEMU, SERVER_AUDIO_BEGIN, SERVER_AUDIO_END, SUBMESSAGE_AUDIO,
};
use crate::density::{CLIENT_DENSITY_LEN, MSG_DENSITY, to_fixed};
use crate::msg::{
    CLIENT_ENABLE_CONTINUOUS_UPDATES, CLIENT_FENCE, CLIENT_FRAMEBUFFER_UPDATE_REQUEST, CLIENT_KEY_EVENT, CLIENT_POINTER_EVENT,
    CLIENT_SET_DESKTOP_SIZE, CLIENT_SET_ENCODINGS, CLIENT_SET_PIXEL_FORMAT, FENCE_MAX_PAYLOAD, MAX_CUT_TEXT, MAX_RECT_BODY, SERVER_CUT_TEXT,
    SERVER_END_OF_CONTINUOUS_UPDATES, SERVER_FENCE, SERVER_FRAMEBUFFER_UPDATE, Screen,
};
use crate::pixel::PixelFormat;
use crate::{
    ENCODING_AUDIO, ENCODING_CURSOR, ENCODING_CURSOR_WITH_ALPHA, ENCODING_DESKTOP_SIZE, ENCODING_EXTENDED_DESKTOP_SIZE, ENCODING_RAW,
    ENCODING_VP9, ENCODING_ZRLE,
};

// ── Handshake ────────────────────────────────────────────────────────────────

/// The longest reason or desktop name read from a server before the session is
/// trusted with anything.
const MAX_HANDSHAKE_TEXT: usize = 64 * 1024;

/// Why the handshake ended before the session began.
#[derive(Debug, Error)]
pub enum HandshakeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("the server speaks {0:?}, not RFB 3.8")]
    Version(String),
    #[error("the server refused the connection: {0}")]
    Refused(String),
    #[error("the login was refused: {0}")]
    Failed(String),
    #[error("the server sent {0} bytes of text where a sentence was expected")]
    TextTooLong(usize),
}

/// Read the server's version banner, which must be the one this crate speaks.
pub async fn read_version<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(), HandshakeError> {
    let mut version = [0u8; 12];
    reader.read_exact(&mut version).await?;
    if &version != crate::msg::PROTOCOL_VERSION {
        return Err(HandshakeError::Version(String::from_utf8_lossy(&version).trim_end().to_owned()));
    }
    Ok(())
}

/// Read the security types on offer, or the reason there are none.
pub async fn read_security_types<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, HandshakeError> {
    let count = usize::from(reader.read_u8().await?);
    if count == 0 {
        return Err(HandshakeError::Refused(read_text(reader).await?));
    }
    let mut types = vec![0u8; count];
    reader.read_exact(&mut types).await?;
    Ok(types)
}

/// Read the SecurityResult, and the reason behind a failed one.
pub async fn read_security_result<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(), HandshakeError> {
    match reader.read_u32().await? {
        0 => Ok(()),
        _ => Err(HandshakeError::Failed(read_text(reader).await?)),
    }
}

/// What a ServerInit says: the framebuffer as it is now, the format pixels
/// arrive in until another is set, and the desktop's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub format: PixelFormat,
    pub name: String,
}

pub async fn read_server_init<R: AsyncRead + Unpin>(reader: &mut R) -> Result<ServerInit, HandshakeError> {
    let width = reader.read_u16().await?;
    let height = reader.read_u16().await?;
    let mut format = [0u8; 16];
    reader.read_exact(&mut format).await?;
    let name = read_text(reader).await?;
    Ok(ServerInit { width, height, format: PixelFormat::parse(&format), name })
}

async fn read_text<R: AsyncRead + Unpin>(reader: &mut R) -> Result<String, HandshakeError> {
    let len = reader.read_u32().await? as usize;
    if len > MAX_HANDSHAKE_TEXT {
        return Err(HandshakeError::TextTooLong(len));
    }
    let mut text = vec![0u8; len];
    reader.read_exact(&mut text).await?;
    Ok(String::from_utf8_lossy(&text).into_owned())
}

// ── Client messages ──────────────────────────────────────────────────────────

pub fn set_pixel_format(format: &PixelFormat) -> [u8; 20] {
    let mut msg = [0u8; 20];
    msg[0] = CLIENT_SET_PIXEL_FORMAT;
    msg[4..].copy_from_slice(&format.to_bytes());
    msg
}

/// SetEncodings, in the client's order of preference.
pub fn set_encodings(encodings: &[i32]) -> Vec<u8> {
    let mut msg = vec![CLIENT_SET_ENCODINGS, 0];
    msg.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for encoding in encodings {
        msg.extend_from_slice(&encoding.to_be_bytes());
    }
    msg
}

fn region(kind: u8, flag: bool, x: u16, y: u16, width: u16, height: u16) -> [u8; 10] {
    let mut msg = [0u8; 10];
    msg[0] = kind;
    msg[1] = u8::from(flag);
    msg[2..4].copy_from_slice(&x.to_be_bytes());
    msg[4..6].copy_from_slice(&y.to_be_bytes());
    msg[6..8].copy_from_slice(&width.to_be_bytes());
    msg[8..10].copy_from_slice(&height.to_be_bytes());
    msg
}

pub fn framebuffer_update_request(incremental: bool, x: u16, y: u16, width: u16, height: u16) -> [u8; 10] {
    region(CLIENT_FRAMEBUFFER_UPDATE_REQUEST, incremental, x, y, width, height)
}

pub fn enable_continuous_updates(enable: bool, x: u16, y: u16, width: u16, height: u16) -> [u8; 10] {
    region(CLIENT_ENABLE_CONTINUOUS_UPDATES, enable, x, y, width, height)
}

/// A KeyEvent carrying an X11 keysym.
pub fn key_event(down: bool, keysym: u32) -> [u8; 8] {
    let mut msg = [0u8; 8];
    msg[0] = CLIENT_KEY_EVENT;
    msg[1] = u8::from(down);
    msg[4..].copy_from_slice(&keysym.to_be_bytes());
    msg
}

/// A PointerEvent: the button mask and the position in framebuffer pixels.
pub fn pointer_event(buttons: u8, x: u16, y: u16) -> [u8; 6] {
    let mut msg = [0u8; 6];
    msg[0] = CLIENT_POINTER_EVENT;
    msg[1] = buttons;
    msg[2..4].copy_from_slice(&x.to_be_bytes());
    msg[4..6].copy_from_slice(&y.to_be_bytes());
    msg
}

/// A ClientFence. The echo of a server's fence is its flags without
/// [`crate::msg::FENCE_REQUEST`] and its payload as it came.
pub fn fence(flags: u32, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= FENCE_MAX_PAYLOAD);
    let mut msg = vec![CLIENT_FENCE, 0, 0, 0];
    msg.extend_from_slice(&flags.to_be_bytes());
    msg.push(payload.len() as u8);
    msg.extend_from_slice(payload);
    msg
}

/// SetDesktopSize: the framebuffer wanted, as one screen covering all of it —
/// the only layout this server accepts.
pub fn set_desktop_size(width: u16, height: u16) -> Vec<u8> {
    let screen = Screen::whole(width, height);
    let mut msg = vec![CLIENT_SET_DESKTOP_SIZE, 0];
    msg.extend_from_slice(&width.to_be_bytes());
    msg.extend_from_slice(&height.to_be_bytes());
    msg.extend_from_slice(&[1, 0]);
    msg.extend_from_slice(&screen.id.to_be_bytes());
    msg.extend_from_slice(&screen.x.to_be_bytes());
    msg.extend_from_slice(&screen.y.to_be_bytes());
    msg.extend_from_slice(&screen.width.to_be_bytes());
    msg.extend_from_slice(&screen.height.to_be_bytes());
    msg.extend_from_slice(&screen.flags.to_be_bytes());
    msg
}

/// ClientCutText carrying an Extended Clipboard body ([`crate::clipboard`]):
/// the length is the body's, negated.
pub fn client_extended_cut_text(body: &[u8]) -> Vec<u8> {
    crate::msg::extended_cut_text(crate::msg::CLIENT_CUT_TEXT, body)
}

/// The density extension's `ClientDensity`: the scale the client's display is
/// drawn at, which it would like the output to be, and the size in pixels it
/// wants at that scale ([`crate::density`]). The layout is `OutputScale`'s.
pub fn client_density(width: u16, height: u16, scale: f64) -> [u8; CLIENT_DENSITY_LEN] {
    let mut msg = [0u8; CLIENT_DENSITY_LEN];
    msg[0] = MSG_DENSITY;
    msg[2..4].copy_from_slice(&width.to_be_bytes());
    msg[4..6].copy_from_slice(&height.to_be_bytes());
    msg[6..].copy_from_slice(&to_fixed(scale).to_be_bytes());
    msg
}

fn audio_op(operation: u16) -> [u8; CLIENT_AUDIO_SWITCH_LEN] {
    let op = operation.to_be_bytes();
    [MSG_QEMU, SUBMESSAGE_AUDIO, op[0], op[1]]
}

/// The audio extension: start sending the desktop's sound, in the format last
/// set ([`crate::audio`]).
pub fn audio_enable() -> [u8; CLIENT_AUDIO_SWITCH_LEN] {
    audio_op(CLIENT_AUDIO_ENABLE)
}

/// The audio extension: stop.
pub fn audio_disable() -> [u8; CLIENT_AUDIO_SWITCH_LEN] {
    audio_op(CLIENT_AUDIO_DISABLE)
}

/// The audio extension: the format every FLAC frame is to decode to, and the
/// one Opus is coded from, if it is one the extension carries — the server takes any other as the end of the
/// connection.
pub fn audio_set_format(format: &AudioFormat) -> Result<[u8; CLIENT_AUDIO_FORMAT_LEN], AudioParseError> {
    format.check()?;
    let mut msg = [0u8; CLIENT_AUDIO_FORMAT_LEN];
    msg[..4].copy_from_slice(&audio_op(CLIENT_AUDIO_SET_FORMAT));
    msg[4] = format.sample as u8;
    msg[5] = format.channels;
    msg[6..].copy_from_slice(&format.frequency.to_be_bytes());
    Ok(msg)
}

/// The audio extension: the rate, in bits per second, the sound is coded at as
/// Opus, if it is one Opus is coded at — the server takes any other as the end
/// of the connection.
pub fn audio_set_bitrate(bitrate: u32) -> Result<[u8; CLIENT_AUDIO_BITRATE_LEN], AudioParseError> {
    crate::audio::check_bitrate(bitrate)?;
    let mut msg = [0u8; CLIENT_AUDIO_BITRATE_LEN];
    msg[..4].copy_from_slice(&audio_op(CLIENT_AUDIO_SET_BITRATE));
    msg[4..].copy_from_slice(&bitrate.to_be_bytes());
    Ok(msg)
}

// ── Server messages ──────────────────────────────────────────────────────────

/// What a rectangle of a FramebufferUpdate carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RectBody {
    /// Pixels in the format last set, four bytes each, rows top down.
    Raw(Vec<u8>),
    /// A ZRLE payload without its length word.
    Zrle(Vec<u8>),
    /// One VP9 frame without its length word; the rectangle is the whole
    /// framebuffer.
    Vp9(Vec<u8>),
    /// The framebuffer is now the rectangle's width and height.
    DesktopSize,
    /// The same, with the rectangle's x as the reason and its y as the status;
    /// a status other than OK answers this client's request and changes nothing.
    ExtendedDesktopSize { screens: Vec<Screen> },
    /// The pointer as pixels in the format last set and a 1-bit mask, the
    /// rectangle's x and y being the hotspot; empty for no pointer.
    Cursor { pixels: Vec<u8>, mask: Vec<u8> },
    /// The pointer as premultiplied RGBA; empty for no pointer.
    AlphaCursor(Vec<u8>),
    /// The server speaks the audio extension, which this client listed: an
    /// empty rectangle, and the only way that is ever said.
    Audio,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub body: RectBody,
}

/// A server-to-client message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    Update(Vec<Rect>),
    /// Latin-1 cut text, which wlshare never sends: framed so that it can be
    /// skipped, and otherwise ignored.
    CutText(Vec<u8>),
    /// An Extended Clipboard body ([`crate::clipboard`]): a `ServerCutText`
    /// with a negative length.
    ExtendedCutText(Vec<u8>),
    EndOfContinuousUpdates,
    Fence { flags: u32, payload: Vec<u8> },
    /// The density extension's `OutputScale`: the framebuffer's size and the
    /// scale it is drawn at, as 16.16 fixed point.
    OutputScale { width: u16, height: u16, fixed: u32 },
    /// The audio extension: a stream is starting, and its frames follow.
    AudioBegin,
    /// The audio extension: the stream stopped.
    AudioEnd,
    /// The audio extension: one FLAC frame or one Opus packet, without its
    /// message header.
    AudioFrame(Vec<u8>),
}

/// Why a server's bytes could not be a message.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("unknown server message type {0}")]
    UnknownType(u8),
    #[error("a rectangle of encoding {0}, which was never asked for")]
    UnknownEncoding(i32),
    #[error("a cursor with alpha whose pixels are in encoding {0}, not Raw")]
    CursorEncoding(i32),
    #[error("a rectangle of {0} bytes is over the {MAX_RECT_BODY}-byte ceiling")]
    RectTooLong(usize),
    #[error("a cut text of {0} bytes is over the {MAX_CUT_TEXT}-byte ceiling")]
    CutTextTooLong(usize),
    #[error("a fence payload of {0} bytes is over the {FENCE_MAX_PAYLOAD}-byte ceiling")]
    FencePayloadTooLong(usize),
    /// The QEMU submessages share no length field, so one that is not audio
    /// cannot be measured, and the stream is at an offset nothing recovers from.
    #[error("QEMU submessage {0} is not audio, the one QEMU extension spoken here")]
    UnknownQemuSubmessage(u8),
    #[error("audio operation {0} is not begin or end")]
    UnknownAudioOperation(u16),
    #[error("an audio frame of {0} bytes is over the {MAX_AUDIO_FRAME}-byte ceiling")]
    AudioFrameTooLong(usize),
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Parse the message at the front of `buf`.
///
/// `Ok(None)` means more bytes are needed and nothing was consumed.
pub fn parse(buf: &[u8]) -> Result<Option<(ServerMsg, usize)>, ParseError> {
    let Some(&kind) = buf.first() else { return Ok(None) };
    macro_rules! need {
        ($n:expr) => {
            if buf.len() < $n {
                return Ok(None);
            }
        };
    }
    let msg = match kind {
        SERVER_FRAMEBUFFER_UPDATE => {
            need!(4);
            let count = usize::from(u16_at(buf, 2));
            let mut at = 4;
            let mut rects = Vec::with_capacity(count);
            for _ in 0..count {
                let Some((rect, used)) = parse_rect(&buf[at..])? else { return Ok(None) };
                rects.push(rect);
                at += used;
            }
            (ServerMsg::Update(rects), at)
        }
        SERVER_CUT_TEXT => {
            need!(8);
            let len = u32_at(buf, 4) as i32;
            let extended = len < 0;
            let len = len.unsigned_abs() as usize;
            if len > MAX_CUT_TEXT {
                return Err(ParseError::CutTextTooLong(len));
            }
            need!(8 + len);
            let body = buf[8..8 + len].to_vec();
            (if extended { ServerMsg::ExtendedCutText(body) } else { ServerMsg::CutText(body) }, 8 + len)
        }
        SERVER_END_OF_CONTINUOUS_UPDATES => (ServerMsg::EndOfContinuousUpdates, 1),
        SERVER_FENCE => {
            need!(9);
            let len = usize::from(buf[8]);
            if len > FENCE_MAX_PAYLOAD {
                return Err(ParseError::FencePayloadTooLong(len));
            }
            need!(9 + len);
            (ServerMsg::Fence { flags: u32_at(buf, 4), payload: buf[9..9 + len].to_vec() }, 9 + len)
        }
        MSG_DENSITY => {
            need!(10);
            (ServerMsg::OutputScale { width: u16_at(buf, 2), height: u16_at(buf, 4), fixed: u32_at(buf, 6) }, 10)
        }
        MSG_QEMU => {
            need!(4);
            if buf[1] != SUBMESSAGE_AUDIO {
                return Err(ParseError::UnknownQemuSubmessage(buf[1]));
            }
            match u16_at(buf, 2) {
                SERVER_AUDIO_BEGIN => (ServerMsg::AudioBegin, 4),
                SERVER_AUDIO_END => (ServerMsg::AudioEnd, 4),
                other => return Err(ParseError::UnknownAudioOperation(other)),
            }
        }
        MSG_AUDIO_FRAME => {
            need!(AUDIO_FRAME_HEADER_LEN);
            let len = u32_at(buf, 4) as usize;
            if len > MAX_AUDIO_FRAME {
                return Err(ParseError::AudioFrameTooLong(len));
            }
            need!(AUDIO_FRAME_HEADER_LEN + len);
            (ServerMsg::AudioFrame(buf[AUDIO_FRAME_HEADER_LEN..AUDIO_FRAME_HEADER_LEN + len].to_vec()), AUDIO_FRAME_HEADER_LEN + len)
        }
        other => return Err(ParseError::UnknownType(other)),
    };
    Ok(Some(msg))
}

/// One rectangle at the front of `buf`: its header, and the body its encoding
/// says follows.
fn parse_rect(buf: &[u8]) -> Result<Option<(Rect, usize)>, ParseError> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let (x, y, width, height) = (u16_at(buf, 0), u16_at(buf, 2), u16_at(buf, 4), u16_at(buf, 6));
    let encoding = u32_at(buf, 8) as i32;
    let pixels = usize::from(width) * usize::from(height) * 4;
    // Take `len` bytes at `from`, or ask for more.
    let take = |from: usize, len: usize| -> Result<Option<Vec<u8>>, ParseError> {
        if len > MAX_RECT_BODY {
            return Err(ParseError::RectTooLong(len));
        }
        Ok(buf.get(from..from + len).map(<[u8]>::to_vec))
    };
    let (body, used) = match encoding {
        ENCODING_RAW => {
            let Some(raw) = take(12, pixels)? else { return Ok(None) };
            (RectBody::Raw(raw), 12 + pixels)
        }
        ENCODING_ZRLE => {
            if buf.len() < 16 {
                return Ok(None);
            }
            let len = u32_at(buf, 12) as usize;
            let Some(payload) = take(16, len)? else { return Ok(None) };
            (RectBody::Zrle(payload), 16 + len)
        }
        ENCODING_VP9 => {
            if buf.len() < 16 {
                return Ok(None);
            }
            let len = u32_at(buf, 12) as usize;
            let Some(frame) = take(16, len)? else { return Ok(None) };
            (RectBody::Vp9(frame), 16 + len)
        }
        ENCODING_DESKTOP_SIZE => (RectBody::DesktopSize, 12),
        ENCODING_EXTENDED_DESKTOP_SIZE => {
            if buf.len() < 16 {
                return Ok(None);
            }
            let count = usize::from(buf[12]);
            if buf.len() < 16 + 16 * count {
                return Ok(None);
            }
            let screens = (0..count)
                .map(|i| {
                    let o = 16 + 16 * i;
                    Screen {
                        id: u32_at(buf, o),
                        x: u16_at(buf, o + 4),
                        y: u16_at(buf, o + 6),
                        width: u16_at(buf, o + 8),
                        height: u16_at(buf, o + 10),
                        flags: u32_at(buf, o + 12),
                    }
                })
                .collect();
            (RectBody::ExtendedDesktopSize { screens }, 16 + 16 * count)
        }
        ENCODING_CURSOR => {
            let mask_len = usize::from(width).div_ceil(8) * usize::from(height);
            let Some(body) = take(12, pixels + mask_len)? else { return Ok(None) };
            let mask = body[pixels..].to_vec();
            let mut body = body;
            body.truncate(pixels);
            (RectBody::Cursor { pixels: body, mask }, 12 + pixels + mask_len)
        }
        ENCODING_CURSOR_WITH_ALPHA => {
            if buf.len() < 16 {
                return Ok(None);
            }
            let inner = u32_at(buf, 12) as i32;
            if inner != ENCODING_RAW {
                return Err(ParseError::CursorEncoding(inner));
            }
            let Some(rgba) = take(16, pixels)? else { return Ok(None) };
            (RectBody::AlphaCursor(rgba), 16 + pixels)
        }
        ENCODING_AUDIO => (RectBody::Audio, 12),
        other => return Err(ParseError::UnknownEncoding(other)),
    };
    Ok(Some((Rect { x, y, width, height, body }, used)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{FlacEncoder, SampleFormat, audio_begin, audio_end, audio_rect};
    use crate::cursor::{CursorImage, alpha_cursor_rect, cursor_rect};
    use crate::msg::{self, ClientMsg};

    /// Every builder, read back by the parser the server reads clients with.
    #[test]
    fn the_server_parses_what_the_client_builds() {
        let parsed = |bytes: &[u8]| {
            let (m, n) = msg::parse(bytes).unwrap().unwrap();
            assert_eq!(n, bytes.len(), "{m:?} is the whole message");
            m
        };
        let rgbx = PixelFormat { red_shift: 0, green_shift: 8, blue_shift: 16, ..PixelFormat::NATIVE };
        assert_eq!(parsed(&set_pixel_format(&rgbx)), ClientMsg::SetPixelFormat(rgbx));
        assert_eq!(
            parsed(&set_encodings(&[ENCODING_ZRLE, ENCODING_CURSOR, crate::ENCODING_DENSITY])),
            ClientMsg::SetEncodings(vec![16, -239, 0x574c_5348])
        );
        assert_eq!(
            parsed(&framebuffer_update_request(true, 1, 2, 3456, 1802)),
            ClientMsg::FramebufferUpdateRequest { incremental: true, x: 1, y: 2, width: 3456, height: 1802 }
        );
        assert_eq!(
            parsed(&enable_continuous_updates(true, 0, 0, 8, 4)),
            ClientMsg::EnableContinuousUpdates { enable: true, x: 0, y: 0, width: 8, height: 4 }
        );
        assert_eq!(parsed(&key_event(true, 0xFFEB)), ClientMsg::KeyEvent { down: true, keysym: 0xFFEB });
        assert_eq!(parsed(&key_event(false, 0x0100_20AC)), ClientMsg::KeyEvent { down: false, keysym: 0x0100_20AC });
        assert_eq!(parsed(&pointer_event(0b101, 300, 20)), ClientMsg::PointerEvent { buttons: 5, x: 300, y: 20 });
        assert_eq!(parsed(&fence(msg::FENCE_BLOCK_BEFORE, &[0, 0, 0, 9])), ClientMsg::Fence { flags: 1, payload: vec![0, 0, 0, 9] });
        assert_eq!(
            parsed(&set_desktop_size(3456, 1802)),
            ClientMsg::SetDesktopSize { width: 3456, height: 1802, screens: vec![Screen::whole(3456, 1802)] }
        );
        assert_eq!(
            parsed(&client_density(2592, 1350, 1.5)),
            ClientMsg::ClientDensity { width: 2592, height: 1350, fixed: 0x0001_8000 }
        );
        assert_eq!(parsed(&client_extended_cut_text(&[2, 0, 0, 1])), ClientMsg::ExtendedCutText(vec![2, 0, 0, 1]));
        assert_eq!(parsed(&audio_enable()), ClientMsg::AudioEnable);
        assert_eq!(parsed(&audio_disable()), ClientMsg::AudioDisable);
        for sample in [SampleFormat::U8, SampleFormat::S8, SampleFormat::U16, SampleFormat::S16] {
            let format = AudioFormat { sample, channels: 1, frequency: 44_100 };
            assert_eq!(parsed(&audio_set_format(&format).unwrap()), ClientMsg::AudioFormat(format));
        }
        let format = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        assert_eq!(parsed(&audio_set_format(&format).unwrap()), ClientMsg::AudioFormat(format));
        assert_eq!(parsed(&audio_set_bitrate(64_000).unwrap()), ClientMsg::AudioBitrate(64_000));
        assert_eq!(audio_set_bitrate(64_000).unwrap(), [255, 1, 0, 3, 0, 0, 0xFA, 0]);
        assert_eq!(audio_set_bitrate(5_999), Err(AudioParseError::BadBitrate(5_999)));
    }

    #[test]
    fn the_client_parses_what_the_server_builds() {
        let (m, n) = parse(&msg::end_of_continuous_updates()).unwrap().unwrap();
        assert_eq!((m, n), (ServerMsg::EndOfContinuousUpdates, 1));
        let wire = msg::fence(msg::FENCE_REQUEST, &[0, 0, 0, 7]);
        assert_eq!(parse(&wire).unwrap().unwrap(), (ServerMsg::Fence { flags: msg::FENCE_REQUEST, payload: vec![0, 0, 0, 7] }, wire.len()));
        let wire = msg::server_extended_cut_text(&[8, 0, 0, 1]);
        assert_eq!(parse(&wire).unwrap().unwrap(), (ServerMsg::ExtendedCutText(vec![8, 0, 0, 1]), wire.len()));
        let latin1 = [3, 0, 0, 0, 0, 0, 0, 2, 0xE9, b'a'];
        assert_eq!(parse(&latin1).unwrap().unwrap(), (ServerMsg::CutText(vec![0xE9, b'a']), latin1.len()));
        let wire = crate::density::output_scale(3456, 1802, 2.0);
        assert_eq!(parse(&wire).unwrap().unwrap(), (ServerMsg::OutputScale { width: 3456, height: 1802, fixed: 0x0002_0000 }, 10));
        assert_eq!(parse(&[7]), Err(ParseError::UnknownType(7)));
    }

    /// The audio extension as the daemon sends it: the announcement, begin and
    /// end, and a frame the encoder made, each whole or not at all.
    #[test]
    fn the_client_parses_the_audio_the_server_builds() {
        let mut wire = msg::update_header(1).to_vec();
        wire.extend_from_slice(&audio_rect());
        let (m, n) = parse(&wire).unwrap().unwrap();
        assert_eq!((m, n), (ServerMsg::Update(vec![Rect { x: 0, y: 0, width: 0, height: 0, body: RectBody::Audio }]), wire.len()));

        assert_eq!(parse(&audio_begin()).unwrap().unwrap(), (ServerMsg::AudioBegin, 4));
        assert_eq!(parse(&audio_end()).unwrap().unwrap(), (ServerMsg::AudioEnd, 4));
        assert_eq!(parse(&audio_begin()[..3]), Ok(None));

        let format = AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 };
        let frames = FlacEncoder::new(format).unwrap().push(&vec![0; format.block_frames() * format.frame_bytes()]).unwrap();
        let mut wire = frames[0].clone();
        let whole = wire.len();
        wire.push(0xFF);
        for short in 0..whole {
            assert_eq!(parse(&wire[..short]), Ok(None), "{short} of {whole} bytes");
        }
        assert_eq!(parse(&wire).unwrap().unwrap(), (ServerMsg::AudioFrame(frames[0][8..].to_vec()), whole));
    }

    /// A QEMU submessage or operation that cannot be measured leaves the stream
    /// where nothing recovers it, and a frame length past any frame is a server
    /// that has lost its framing: all three end the connection.
    #[test]
    fn audio_that_cannot_be_framed_is_fatal() {
        assert_eq!(parse(&[255, 2, 0, 0]), Err(ParseError::UnknownQemuSubmessage(2)));
        // QEMU's own raw data, which this extension never sends.
        assert_eq!(parse(&[255, 1, 0, 2]), Err(ParseError::UnknownAudioOperation(2)));
        let mut wire = vec![0xE4, 0, 0, 0];
        wire.extend_from_slice(&(MAX_AUDIO_FRAME as u32 + 1).to_be_bytes());
        assert_eq!(parse(&wire), Err(ParseError::AudioFrameTooLong(MAX_AUDIO_FRAME + 1)));
    }

    /// One update with a rectangle of every kind, fed a byte short at every
    /// length: nothing is consumed until all of it is there, and the byte after
    /// it is left alone.
    #[test]
    fn an_update_of_every_rectangle_parses_whole_or_not_at_all() {
        let image = CursorImage::cropped(2, 1, (1, 0), &[255, 0, 0, 255, 0, 0, 128, 128]).unwrap();
        let mut wire = msg::update_header(7).to_vec();
        wire.extend_from_slice(&msg::rect_header(3, 4, 2, 1, ENCODING_RAW));
        wire.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        wire.extend_from_slice(&msg::rect_header(0, 0, 64, 64, ENCODING_ZRLE));
        wire.extend_from_slice(&[0, 0, 0, 3, 0xAA, 0xBB, 0xCC]);
        wire.extend_from_slice(&msg::desktop_size_rect(800, 600));
        wire.extend_from_slice(&msg::extended_desktop_size_rect(msg::EDS_REASON_THIS_CLIENT, msg::EDS_STATUS_PROHIBITED, 800, 600, &[Screen::whole(800, 600)]));
        wire.extend_from_slice(&alpha_cursor_rect(Some(&image)));
        wire.extend_from_slice(&cursor_rect(&PixelFormat::NATIVE, Some(&image)));
        wire.extend_from_slice(&msg::rect_header(0, 0, 800, 600, ENCODING_VP9));
        wire.extend_from_slice(&[0, 0, 0, 2, 0x82, 0x49]);
        let whole = wire.len();
        wire.push(0xFF);

        for short in 0..whole {
            assert_eq!(parse(&wire[..short]), Ok(None), "{short} of {whole} bytes");
        }
        let (m, n) = parse(&wire).unwrap().unwrap();
        assert_eq!(n, whole);
        let ServerMsg::Update(rects) = m else { panic!("an update") };
        assert_eq!(rects[0], Rect { x: 3, y: 4, width: 2, height: 1, body: RectBody::Raw(vec![1, 2, 3, 0, 4, 5, 6, 0]) });
        assert_eq!(rects[1].body, RectBody::Zrle(vec![0xAA, 0xBB, 0xCC]));
        assert_eq!((rects[2].width, rects[2].height, &rects[2].body), (800, 600, &RectBody::DesktopSize));
        assert_eq!((rects[3].x, rects[3].y), (msg::EDS_REASON_THIS_CLIENT, msg::EDS_STATUS_PROHIBITED));
        assert_eq!(rects[3].body, RectBody::ExtendedDesktopSize { screens: vec![Screen::whole(800, 600)] });
        assert_eq!((rects[4].x, rects[4].y), (1, 0), "the hotspot");
        assert_eq!(rects[4].body, RectBody::AlphaCursor(vec![255, 0, 0, 255, 0, 0, 128, 128]));
        // Opaque red, and half-transparent blue divided back out; both in the mask.
        assert_eq!(rects[5].body, RectBody::Cursor { pixels: vec![0, 0, 255, 0, 255, 0, 0, 0], mask: vec![0b1100_0000] });
        assert_eq!(rects[6], Rect { x: 0, y: 0, width: 800, height: 600, body: RectBody::Vp9(vec![0x82, 0x49]) });
    }

    #[test]
    fn a_rectangle_nobody_asked_for_is_fatal() {
        let mut wire = msg::update_header(1).to_vec();
        wire.extend_from_slice(&msg::rect_header(0, 0, 1, 1, 5));
        assert_eq!(parse(&wire), Err(ParseError::UnknownEncoding(5)));
        let mut wire = msg::update_header(1).to_vec();
        wire.extend_from_slice(&msg::rect_header(0, 0, 1, 1, ENCODING_ZRLE));
        wire.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(parse(&wire), Err(ParseError::RectTooLong(u32::MAX as usize)));
    }

    #[tokio::test]
    async fn the_handshake_reads_what_the_server_writes() {
        let mut wire = msg::PROTOCOL_VERSION.to_vec();
        wire.extend(msg::security_types(&[129, 5]));
        wire.extend(msg::security_ok());
        wire.extend(msg::server_init(1024, 768, &PixelFormat::NATIVE, "wlshare"));
        let mut reader = wire.as_slice();
        read_version(&mut reader).await.unwrap();
        assert_eq!(read_security_types(&mut reader).await.unwrap(), vec![129, 5]);
        read_security_result(&mut reader).await.unwrap();
        let init = read_server_init(&mut reader).await.unwrap();
        assert_eq!(init, ServerInit { width: 1024, height: 768, format: PixelFormat::NATIVE, name: "wlshare".to_owned() });
        assert!(reader.is_empty());

        let refusal = msg::security_refusal("this server speaks RFB 3.8 only");
        let err = read_security_types(&mut refusal.as_slice()).await.unwrap_err();
        assert!(matches!(&err, HandshakeError::Refused(r) if r == "this server speaks RFB 3.8 only"), "{err}");
        let failed = msg::security_failed("authentication failed");
        let err = read_security_result(&mut failed.as_slice()).await.unwrap_err();
        assert!(matches!(&err, HandshakeError::Failed(r) if r == "authentication failed"), "{err}");
        let err = read_version(&mut b"RFB 003.003\n".as_slice()).await.unwrap_err();
        assert!(matches!(&err, HandshakeError::Version(v) if v == "RFB 003.003"), "{err}");
    }
}
