//! RFB messages: the client's, parsed from a byte buffer, and the server's,
//! built into bytes. All integers are big-endian (RFC 6143).
//!
//! Parsing is incremental. [`parse`] looks at the front of whatever has been
//! read so far and either returns a message and how many bytes it took, asks for
//! more, or names a type it does not know — after which the connection is over,
//! because RFB has no framing to skip an unknown message by.

use thiserror::Error;

use crate::audio::{AudioFormat, AudioParseError, ClientAudio, MSG_QEMU};
use crate::camera::{CameraFormat, CameraParseError, ClientCamera, MSG_CAMERA};
use crate::density::{CLIENT_DENSITY_LEN, MSG_DENSITY};
use crate::microphone::{ClientMicrophone, MSG_MICROPHONE, MicrophoneParseError};
use crate::outputs::{MSG_OUTPUTS, SELECT_OUTPUT_LEN};
use crate::pixel::PixelFormat;

// ── Client message types ─────────────────────────────────────────────────────
pub const CLIENT_SET_PIXEL_FORMAT: u8 = 0;
pub const CLIENT_SET_ENCODINGS: u8 = 2;
pub const CLIENT_FRAMEBUFFER_UPDATE_REQUEST: u8 = 3;
pub const CLIENT_KEY_EVENT: u8 = 4;
pub const CLIENT_POINTER_EVENT: u8 = 5;
pub const CLIENT_CUT_TEXT: u8 = 6;
pub const CLIENT_ENABLE_CONTINUOUS_UPDATES: u8 = 150;
pub const CLIENT_FENCE: u8 = 248;
pub const CLIENT_SET_DESKTOP_SIZE: u8 = 251;

// ── Server message types ─────────────────────────────────────────────────────
pub const SERVER_FRAMEBUFFER_UPDATE: u8 = 0;
pub const SERVER_CUT_TEXT: u8 = 3;
pub const SERVER_END_OF_CONTINUOUS_UPDATES: u8 = 150;
pub const SERVER_FENCE: u8 = 248;

// ── Fence flags ──────────────────────────────────────────────────────────────
pub const FENCE_BLOCK_BEFORE: u32 = 1;
pub const FENCE_BLOCK_AFTER: u32 = 2;
pub const FENCE_SYNC_NEXT: u32 = 4;
pub const FENCE_REQUEST: u32 = 0x8000_0000;
/// The fence payload's ceiling, per the extension.
pub const FENCE_MAX_PAYLOAD: usize = 64;

// ── ExtendedDesktopSize ──────────────────────────────────────────────────────
/// The rectangle's x is the reason: the server changed the size on its own.
pub const EDS_REASON_SERVER: u16 = 0;
/// This client's SetDesktopSize; y then carries the status.
pub const EDS_REASON_THIS_CLIENT: u16 = 1;
/// Another client's SetDesktopSize.
pub const EDS_REASON_OTHER_CLIENT: u16 = 2;
pub const EDS_STATUS_OK: u16 = 0;
pub const EDS_STATUS_PROHIBITED: u16 = 1;
pub const EDS_STATUS_OUT_OF_RESOURCES: u16 = 2;
pub const EDS_STATUS_INVALID_LAYOUT: u16 = 3;

/// Cut text longer than this is refused rather than buffered.
pub const MAX_CUT_TEXT: usize = 16 * 1024 * 1024;

/// The most bytes one rectangle may carry: past any framebuffer a desktop has,
/// and short of what a length field gone wrong would have a client buffer.
pub const MAX_RECT_BODY: usize = 512 * 1024 * 1024;

/// One screen of an ExtendedDesktopSize layout, or of a SetDesktopSize request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    pub id: u32,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub flags: u32,
}

impl Screen {
    /// The one screen this server ever reports: the whole framebuffer.
    pub fn whole(width: u16, height: u16) -> Self {
        Self { id: 0, x: 0, y: 0, width, height, flags: 0 }
    }
}

/// A client-to-server message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<i32>),
    FramebufferUpdateRequest { incremental: bool, x: u16, y: u16, width: u16, height: u16 },
    KeyEvent { down: bool, keysym: u32 },
    PointerEvent { buttons: u8, x: u16, y: u16 },
    /// Latin-1 cut text, which wlshare does not speak: framed so that it can be
    /// skipped, and otherwise ignored.
    CutText(Vec<u8>),
    /// An Extended Clipboard body ([`crate::clipboard`]): a `ClientCutText`
    /// with a negative length.
    ExtendedCutText(Vec<u8>),
    EnableContinuousUpdates { enable: bool, x: u16, y: u16, width: u16, height: u16 },
    Fence { flags: u32, payload: Vec<u8> },
    SetDesktopSize { width: u16, height: u16, screens: Vec<Screen> },
    /// The density extension's declaration: the size in pixels the client wants
    /// and the scale it wants them drawn at, as 16.16 fixed point
    /// ([`crate::density::from_fixed`]).
    ClientDensity { width: u16, height: u16, fixed: u32 },
    /// The outputs extension's request: share the output with this id, as the
    /// last list named it ([`crate::outputs`]).
    SelectOutput { id: u32 },
    /// The QEMU Audio extension: start sending the desktop's sound.
    AudioEnable,
    /// The QEMU Audio extension: stop.
    AudioDisable,
    /// The QEMU Audio extension: the sample format the client wants.
    AudioFormat(AudioFormat),
    /// The camera extension: a camera producing H.264 in this format
    /// ([`crate::camera`]).
    CameraPlug(CameraFormat),
    /// The camera extension: the camera is gone.
    CameraUnplug,
    /// The camera extension: one Annex B access unit.
    CameraSample { keyframe: bool, data: Vec<u8> },
    /// The microphone extension: a microphone is lent ([`crate::microphone`]).
    MicrophonePlug,
    /// The microphone extension: the microphone is gone.
    MicrophoneUnplug,
    /// The microphone extension: interleaved signed 16-bit little-endian PCM,
    /// in the format the last start named.
    MicrophoneSample(Vec<u8>),
}

/// Why a client's bytes could not be a message.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("unknown client message type {0}")]
    UnknownType(u8),
    #[error("a cut text of {0} bytes is over the {MAX_CUT_TEXT}-byte ceiling")]
    CutTextTooLong(usize),
    #[error("a fence payload of {0} bytes is over the {FENCE_MAX_PAYLOAD}-byte ceiling")]
    FencePayloadTooLong(usize),
    #[error("QEMU message: {0}")]
    Audio(#[from] AudioParseError),
    #[error("camera message: {0}")]
    Camera(#[from] CameraParseError),
    #[error("microphone message: {0}")]
    Microphone(#[from] MicrophoneParseError),
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
pub fn parse(buf: &[u8]) -> Result<Option<(ClientMsg, usize)>, ParseError> {
    let Some(&kind) = buf.first() else { return Ok(None) };
    macro_rules! need {
        ($n:expr) => {
            if buf.len() < $n {
                return Ok(None);
            }
        };
    }
    let msg = match kind {
        CLIENT_SET_PIXEL_FORMAT => {
            need!(20);
            let mut f = [0u8; 16];
            f.copy_from_slice(&buf[4..20]);
            (ClientMsg::SetPixelFormat(PixelFormat::parse(&f)), 20)
        }
        CLIENT_SET_ENCODINGS => {
            need!(4);
            let n = usize::from(u16_at(buf, 2));
            need!(4 + 4 * n);
            let encodings = (0..n).map(|i| u32_at(buf, 4 + 4 * i) as i32).collect();
            (ClientMsg::SetEncodings(encodings), 4 + 4 * n)
        }
        CLIENT_FRAMEBUFFER_UPDATE_REQUEST => {
            need!(10);
            (
                ClientMsg::FramebufferUpdateRequest {
                    incremental: buf[1] != 0,
                    x: u16_at(buf, 2),
                    y: u16_at(buf, 4),
                    width: u16_at(buf, 6),
                    height: u16_at(buf, 8),
                },
                10,
            )
        }
        CLIENT_KEY_EVENT => {
            need!(8);
            (ClientMsg::KeyEvent { down: buf[1] != 0, keysym: u32_at(buf, 4) }, 8)
        }
        CLIENT_POINTER_EVENT => {
            need!(6);
            (ClientMsg::PointerEvent { buttons: buf[1], x: u16_at(buf, 2), y: u16_at(buf, 4) }, 6)
        }
        CLIENT_CUT_TEXT => {
            need!(8);
            let len = u32_at(buf, 4) as i32;
            let extended = len < 0;
            let len = len.unsigned_abs() as usize;
            if len > MAX_CUT_TEXT {
                return Err(ParseError::CutTextTooLong(len));
            }
            need!(8 + len);
            let body = buf[8..8 + len].to_vec();
            (if extended { ClientMsg::ExtendedCutText(body) } else { ClientMsg::CutText(body) }, 8 + len)
        }
        CLIENT_ENABLE_CONTINUOUS_UPDATES => {
            need!(10);
            (
                ClientMsg::EnableContinuousUpdates {
                    enable: buf[1] != 0,
                    x: u16_at(buf, 2),
                    y: u16_at(buf, 4),
                    width: u16_at(buf, 6),
                    height: u16_at(buf, 8),
                },
                10,
            )
        }
        CLIENT_FENCE => {
            need!(9);
            let flags = u32_at(buf, 4);
            let len = usize::from(buf[8]);
            if len > FENCE_MAX_PAYLOAD {
                return Err(ParseError::FencePayloadTooLong(len));
            }
            need!(9 + len);
            (ClientMsg::Fence { flags, payload: buf[9..9 + len].to_vec() }, 9 + len)
        }
        CLIENT_SET_DESKTOP_SIZE => {
            need!(8);
            let n = usize::from(buf[6]);
            need!(8 + 16 * n);
            let screens = (0..n)
                .map(|i| {
                    let o = 8 + 16 * i;
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
            (ClientMsg::SetDesktopSize { width: u16_at(buf, 2), height: u16_at(buf, 4), screens }, 8 + 16 * n)
        }
        MSG_DENSITY => {
            need!(CLIENT_DENSITY_LEN);
            (ClientMsg::ClientDensity { width: u16_at(buf, 2), height: u16_at(buf, 4), fixed: u32_at(buf, 6) }, CLIENT_DENSITY_LEN)
        }
        MSG_OUTPUTS => {
            need!(SELECT_OUTPUT_LEN);
            (ClientMsg::SelectOutput { id: u32_at(buf, 4) }, SELECT_OUTPUT_LEN)
        }
        MSG_QEMU => match crate::audio::parse_client(buf)? {
            None => return Ok(None),
            Some((ClientAudio::Enable, n)) => (ClientMsg::AudioEnable, n),
            Some((ClientAudio::Disable, n)) => (ClientMsg::AudioDisable, n),
            Some((ClientAudio::SetFormat(format), n)) => (ClientMsg::AudioFormat(format), n),
        },
        MSG_CAMERA => match crate::camera::parse_client(buf)? {
            None => return Ok(None),
            Some((ClientCamera::Plug(format), n)) => (ClientMsg::CameraPlug(format), n),
            Some((ClientCamera::Unplug, n)) => (ClientMsg::CameraUnplug, n),
            Some((ClientCamera::Sample { keyframe, data }, n)) => (ClientMsg::CameraSample { keyframe, data }, n),
        },
        MSG_MICROPHONE => match crate::microphone::parse_client(buf)? {
            None => return Ok(None),
            Some((ClientMicrophone::Plug, n)) => (ClientMsg::MicrophonePlug, n),
            Some((ClientMicrophone::Unplug, n)) => (ClientMsg::MicrophoneUnplug, n),
            Some((ClientMicrophone::Sample(pcm), n)) => (ClientMsg::MicrophoneSample(pcm), n),
        },
        other => return Err(ParseError::UnknownType(other)),
    };
    Ok(Some(msg))
}

// ── Handshake ────────────────────────────────────────────────────────────────

/// The version banner, ours and the one a client must answer with.
pub const PROTOCOL_VERSION: &[u8; 12] = b"RFB 003.008\n";

/// Security type 1: no authentication, offered alone when nothing is configured.
pub const SECURITY_NONE: u8 = 1;

/// The security types on offer: a count, then the types.
pub fn security_types(types: &[u8]) -> Vec<u8> {
    let mut msg = vec![types.len() as u8];
    msg.extend_from_slice(types);
    msg
}

/// A handshake refusal: zero security types, then the reason.
pub fn security_refusal(reason: &str) -> Vec<u8> {
    let mut msg = vec![0u8];
    msg.extend_from_slice(&(reason.len() as u32).to_be_bytes());
    msg.extend_from_slice(reason.as_bytes());
    msg
}

/// SecurityResult: OK.
pub fn security_ok() -> [u8; 4] {
    [0, 0, 0, 0]
}

/// SecurityResult: failed, with the reason RFB 3.8 lets the server give.
pub fn security_failed(reason: &str) -> Vec<u8> {
    let mut msg = vec![0, 0, 0, 1];
    msg.extend_from_slice(&(reason.len() as u32).to_be_bytes());
    msg.extend_from_slice(reason.as_bytes());
    msg
}

/// ServerInit: the framebuffer's size and format, and the desktop's name.
pub fn server_init(width: u16, height: u16, format: &PixelFormat, name: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(24 + name.len());
    msg.extend_from_slice(&width.to_be_bytes());
    msg.extend_from_slice(&height.to_be_bytes());
    msg.extend_from_slice(&format.to_bytes());
    msg.extend_from_slice(&(name.len() as u32).to_be_bytes());
    msg.extend_from_slice(name.as_bytes());
    msg
}

// ── Server messages ──────────────────────────────────────────────────────────

/// The head of a FramebufferUpdate carrying `rects` rectangles.
pub fn update_header(rects: u16) -> [u8; 4] {
    let n = rects.to_be_bytes();
    [SERVER_FRAMEBUFFER_UPDATE, 0, n[0], n[1]]
}

/// A rectangle header: position, size and encoding.
pub fn rect_header(x: u16, y: u16, width: u16, height: u16, encoding: i32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0..2].copy_from_slice(&x.to_be_bytes());
    h[2..4].copy_from_slice(&y.to_be_bytes());
    h[4..6].copy_from_slice(&width.to_be_bytes());
    h[6..8].copy_from_slice(&height.to_be_bytes());
    h[8..12].copy_from_slice(&encoding.to_be_bytes());
    h
}

/// A DesktopSize rectangle: the announcement is the header, there is no body.
pub fn desktop_size_rect(width: u16, height: u16) -> [u8; 12] {
    rect_header(0, 0, width, height, crate::ENCODING_DESKTOP_SIZE)
}

/// An ExtendedDesktopSize rectangle, header and layout. The header's x is the
/// reason and its y the status (RFC 6143 has neither; the extension repurposes
/// them), and the body is the screen list.
pub fn extended_desktop_size_rect(reason: u16, status: u16, width: u16, height: u16, screens: &[Screen]) -> Vec<u8> {
    let mut rect = Vec::with_capacity(16 + 16 * screens.len());
    rect.extend_from_slice(&rect_header(reason, status, width, height, crate::ENCODING_EXTENDED_DESKTOP_SIZE));
    rect.push(screens.len() as u8);
    rect.extend_from_slice(&[0, 0, 0]);
    for s in screens {
        rect.extend_from_slice(&s.id.to_be_bytes());
        rect.extend_from_slice(&s.x.to_be_bytes());
        rect.extend_from_slice(&s.y.to_be_bytes());
        rect.extend_from_slice(&s.width.to_be_bytes());
        rect.extend_from_slice(&s.height.to_be_bytes());
        rect.extend_from_slice(&s.flags.to_be_bytes());
    }
    rect
}

/// EndOfContinuousUpdates, which is also how support for them is announced.
pub fn end_of_continuous_updates() -> [u8; 1] {
    [SERVER_END_OF_CONTINUOUS_UPDATES]
}

/// A ServerFence.
pub fn fence(flags: u32, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= FENCE_MAX_PAYLOAD);
    let mut msg = vec![SERVER_FENCE, 0, 0, 0];
    msg.extend_from_slice(&flags.to_be_bytes());
    msg.push(payload.len() as u8);
    msg.extend_from_slice(payload);
    msg
}

/// ServerCutText carrying an Extended Clipboard body ([`crate::clipboard`]):
/// the length is the body's, negated.
pub fn server_extended_cut_text(body: &[u8]) -> Vec<u8> {
    extended_cut_text(SERVER_CUT_TEXT, body)
}

/// Either end's cut text with an extended body, which the two frame alike.
pub(crate) fn extended_cut_text(kind: u8, body: &[u8]) -> Vec<u8> {
    debug_assert!(body.len() <= MAX_CUT_TEXT);
    let mut msg = vec![kind, 0, 0, 0];
    msg.extend_from_slice(&(-(body.len() as i32)).to_be_bytes());
    msg.extend_from_slice(body);
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_message_asks_for_more_and_consumes_nothing() {
        assert_eq!(parse(&[]), Ok(None));
        assert_eq!(parse(&[CLIENT_SET_ENCODINGS, 0, 0, 2, 0, 0, 0]), Ok(None));
        assert_eq!(parse(&[CLIENT_CUT_TEXT, 0, 0, 0, 0, 0, 0, 3, b'a']), Ok(None));
    }

    #[test]
    fn set_encodings_carries_negative_and_private_numbers() {
        let mut buf = vec![CLIENT_SET_ENCODINGS, 0, 0, 3];
        for e in [16i32, -313, crate::ENCODING_DENSITY] {
            buf.extend_from_slice(&e.to_be_bytes());
        }
        buf.push(0xFF); // the next message's first byte, untouched
        let (msg, used) = parse(&buf).unwrap().unwrap();
        assert_eq!(msg, ClientMsg::SetEncodings(vec![16, -313, 0x574c_5348]));
        assert_eq!(used, 16);
    }

    #[test]
    fn the_fixed_size_messages_parse() {
        let (m, n) = parse(&[3, 1, 0, 2, 0, 3, 0x0D, 0x80, 0x07, 0x0A]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::FramebufferUpdateRequest { incremental: true, x: 2, y: 3, width: 3456, height: 1802 });
        assert_eq!(n, 10);
        let (m, _) = parse(&[4, 1, 0, 0, 0, 0, 0xFF, 0x0D]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::KeyEvent { down: true, keysym: 0xFF0D });
        let (m, _) = parse(&[5, 0b101, 0, 10, 0, 20]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::PointerEvent { buttons: 5, x: 10, y: 20 });
        let (m, _) = parse(&[150, 1, 0, 0, 0, 0, 0, 8, 0, 4]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::EnableContinuousUpdates { enable: true, x: 0, y: 0, width: 8, height: 4 });
        let (m, n) = parse(&[0xE0, 0, 0x0D, 0x80, 0x07, 0x0A, 0, 2, 0, 0]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::ClientDensity { width: 3456, height: 1802, fixed: 0x0002_0000 });
        assert_eq!(n, 10);
        assert_eq!(parse(&[0xE0, 0, 0x0D, 0x80, 0x07, 0x0A, 0, 2, 0]).unwrap(), None, "a short ClientDensity asks for more");
        let (m, n) = parse(&[0xE1, 0, 0, 0, 0, 0, 0, 7]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::SelectOutput { id: 7 });
        assert_eq!(n, 8);
        assert_eq!(parse(&[0xE1, 0, 0, 0, 0, 0, 0]).unwrap(), None, "a short SelectOutput asks for more");
    }

    #[test]
    fn the_audio_submessages_parse_and_a_bad_one_is_fatal() {
        use crate::audio::SampleFormat;
        let (m, n) = parse(&[255, 1, 0, 0, 9]).unwrap().unwrap();
        assert_eq!((m, n), (ClientMsg::AudioEnable, 4));
        let (m, _) = parse(&[255, 1, 0, 1]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::AudioDisable);
        assert_eq!(parse(&[255, 1, 0, 2, 3, 2, 0, 0]), Ok(None));
        let (m, n) = parse(&[255, 1, 0, 2, 3, 2, 0, 0, 0xBB, 0x80]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::AudioFormat(AudioFormat { sample: SampleFormat::S16, channels: 2, frequency: 48_000 }));
        assert_eq!(n, 10);
        assert_eq!(parse(&[255, 7, 0, 0]), Err(ParseError::Audio(AudioParseError::UnknownSubmessage(7))));
    }

    #[test]
    fn the_camera_submessages_parse_and_a_bad_one_is_fatal() {
        let (m, n) = parse(&[0xE2, 0, 0, 0, 0x02, 0x80, 0x01, 0xE0, 0, 0, 0, 15, 0, 0, 0, 1]).unwrap().unwrap();
        assert_eq!(m, ClientMsg::CameraPlug(CameraFormat { width: 640, height: 480, fps_numerator: 15, fps_denominator: 1 }));
        assert_eq!(n, 16);
        assert_eq!(parse(&[0xE2, 1, 0, 0]).unwrap().unwrap(), (ClientMsg::CameraUnplug, 4));
        let (m, n) = parse(&[0xE2, 2, 1, 0, 0, 0, 0, 2, 0xAB, 0xCD, 0xE2]).unwrap().unwrap();
        assert_eq!((m, n), (ClientMsg::CameraSample { keyframe: true, data: vec![0xAB, 0xCD] }, 10));
        assert_eq!(parse(&[0xE2, 2, 1, 0, 0, 0, 0, 2, 0xAB]), Ok(None));
        assert_eq!(parse(&[0xE2, 9, 0, 0]), Err(ParseError::Camera(CameraParseError::UnknownOperation(9))));
    }

    #[test]
    fn the_microphone_submessages_parse_and_a_bad_one_is_fatal() {
        assert_eq!(parse(&[0xE3, 0, 0, 0, 0xFF]).unwrap().unwrap(), (ClientMsg::MicrophonePlug, 4));
        assert_eq!(parse(&[0xE3, 1, 0, 0]).unwrap().unwrap(), (ClientMsg::MicrophoneUnplug, 4));
        let (m, n) = parse(&[0xE3, 2, 0, 0, 0, 0, 0, 2, 0x34, 0x12, 0xE3]).unwrap().unwrap();
        assert_eq!((m, n), (ClientMsg::MicrophoneSample(vec![0x34, 0x12]), 10));
        assert_eq!(parse(&[0xE3, 2, 0, 0, 0, 0, 0, 2, 0x34]), Ok(None));
        assert_eq!(parse(&[0xE3, 9, 0, 0]), Err(ParseError::Microphone(MicrophoneParseError::UnknownOperation(9))));
    }

    #[test]
    fn cut_text_is_latin1_unless_its_length_is_negative() {
        let (m, n) = parse(&[6, 0, 0, 0, 0, 0, 0, 2, 0xE9, b'a']).unwrap().unwrap();
        assert_eq!(m, ClientMsg::CutText(vec![0xE9, b'a']));
        assert_eq!(n, 10);
        let mut buf = vec![6, 0, 0, 0];
        buf.extend_from_slice(&(-4i32).to_be_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4]);
        let (m, n) = parse(&buf).unwrap().unwrap();
        assert_eq!(m, ClientMsg::ExtendedCutText(vec![1, 2, 3, 4]));
        assert_eq!(n, 12);
        let mut huge = vec![6, 0, 0, 0];
        huge.extend_from_slice(&((MAX_CUT_TEXT as u32) + 1).to_be_bytes());
        assert_eq!(parse(&huge), Err(ParseError::CutTextTooLong(MAX_CUT_TEXT + 1)));
    }

    #[test]
    fn fence_and_set_desktop_size_parse() {
        let mut buf = vec![248, 0, 0, 0];
        buf.extend_from_slice(&FENCE_REQUEST.to_be_bytes());
        buf.extend_from_slice(&[2, 9, 9]);
        let (m, n) = parse(&buf).unwrap().unwrap();
        assert_eq!(m, ClientMsg::Fence { flags: FENCE_REQUEST, payload: vec![9, 9] });
        assert_eq!(n, 11);

        let mut buf = vec![251, 0, 0x0D, 0x80, 0x07, 0x0A, 1, 0];
        buf.extend_from_slice(&7u32.to_be_bytes());
        buf.extend_from_slice(&[0, 0, 0, 0, 0x0D, 0x80, 0x07, 0x0A]);
        buf.extend_from_slice(&0x1234u32.to_be_bytes());
        let (m, n) = parse(&buf).unwrap().unwrap();
        assert_eq!(
            m,
            ClientMsg::SetDesktopSize {
                width: 3456,
                height: 1802,
                screens: vec![Screen { id: 7, x: 0, y: 0, width: 3456, height: 1802, flags: 0x1234 }],
            }
        );
        assert_eq!(n, 24);
    }

    #[test]
    fn an_unknown_type_is_fatal() {
        assert_eq!(parse(&[7, 0, 0]), Err(ParseError::UnknownType(7)));
    }

    #[test]
    fn server_messages_have_the_documented_layouts() {
        assert_eq!(update_header(2), [0, 0, 0, 2]);
        assert_eq!(rect_header(1, 2, 3, 4, -308), [0, 1, 0, 2, 0, 3, 0, 4, 0xFF, 0xFF, 0xFE, 0xCC]);
        let rect = extended_desktop_size_rect(EDS_REASON_THIS_CLIENT, EDS_STATUS_OK, 8, 4, &[Screen::whole(8, 4)]);
        assert_eq!(rect.len(), 12 + 4 + 16);
        assert_eq!(&rect[..4], &[0, 1, 0, 0]);
        assert_eq!(rect[12], 1);
        assert_eq!(&rect[16..20], &[0, 0, 0, 0]);
        assert_eq!(&rect[24..28], &[0, 8, 0, 4]);
        assert_eq!(fence(FENCE_REQUEST, &[1]), vec![248, 0, 0, 0, 0x80, 0, 0, 0, 1, 1]);
        assert_eq!(server_extended_cut_text(&[8, 0, 0, 1]), vec![3, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFC, 8, 0, 0, 1]);
        let init = server_init(8, 4, &PixelFormat::NATIVE, "desktop");
        assert_eq!(init.len(), 31);
        assert_eq!(&init[..4], &[0, 8, 0, 4]);
        assert_eq!(&init[20..], &[0, 0, 0, 7, b'd', b'e', b's', b'k', b't', b'o', b'p']);
        assert_eq!(security_types(&[SECURITY_NONE, 5]), vec![2, 1, 5]);
        assert_eq!(security_failed("no"), vec![0, 0, 0, 1, 0, 0, 0, 2, b'n', b'o']);
    }
}
