//! The Extended Clipboard extension, text only, from both ends.
//!
//! Baseline RFB cut text is latin-1: everything above U+00FF becomes `?` one
//! way and cannot be said at all the other. The extension carries UTF-8 instead,
//! inside the same two message types — a `ServerCutText` or `ClientCutText`
//! whose length is **negative**, `-length` being the size of a body that starts
//! with a `u32` of flags. The low 16 bits name formats, the top 8 an action:
//!
//! ```text
//! caps     what the sender is willing to receive, once per SetEncodings
//! notify   "my clipboard changed, and holds these formats now"
//! request  "send me these formats"
//! provide  the data itself, one zlib stream per message
//! peek     "tell me what you hold", answered with a notify
//! ```
//!
//! This is the only clipboard wlshare speaks. A latin-1 cut text is framed so
//! that it can be skipped, and is otherwise ignored at both ends.
//!
//! Text is the only format: the compositor's clipboard is shared as text, and
//! a peer that provides RTF or HTML beside it has them stepped over. On the wire
//! text is UTF-8 with CRLF line endings and a terminating NUL; here it is a
//! `String` with LF line endings, and [`provide`] and [`parse`] convert.
//!
//! Every body built here is framed by [`crate::msg::server_extended_cut_text`],
//! and every one received is the body [`crate::msg::ClientMsg::ExtendedCutText`]
//! carries.

use std::io::{Read as _, Write as _};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use thiserror::Error;

/// The extension's pseudo-encoding, listed by a client that speaks it.
pub use crate::ENCODING_EXTENDED_CLIPBOARD as ENCODING;

/// Plain text: UTF-8, CRLF line endings, NUL-terminated.
pub const FORMAT_TEXT: u32 = 1 << 0;

pub const ACTION_CAPS: u32 = 1 << 24;
pub const ACTION_REQUEST: u32 = 1 << 25;
pub const ACTION_PEEK: u32 = 1 << 26;
pub const ACTION_NOTIFY: u32 = 1 << 27;
pub const ACTION_PROVIDE: u32 = 1 << 28;

const FORMAT_MASK: u32 = 0x0000_ffff;
const ACTION_MASK: u32 = 0xff00_0000;

/// Every action this crate speaks, which is every action the extension has.
pub const ALL_ACTIONS: u32 = ACTION_CAPS | ACTION_REQUEST | ACTION_PEEK | ACTION_NOTIFY | ACTION_PROVIDE;

/// The longest text either end sends or accepts, in bytes of UTF-8. Half the
/// cut-text ceiling, so a provide's deflated body — which for text that does not
/// compress is a little larger than the text — always fits inside one message.
pub const MAX_TEXT: usize = crate::msg::MAX_CUT_TEXT / 2;

/// What a peer said it is willing to receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Action bits, [`ACTION_CAPS`] included.
    pub actions: u32,
    /// Format bits.
    pub formats: u32,
    /// The largest text the peer takes in a provide it did not ask for; zero
    /// asks to be notified instead. `None` when the peer takes no text at all.
    pub text_size: Option<u32>,
}

impl Caps {
    /// What a client that never sends caps is assumed to take, by the
    /// extension's own rule: text, RTF and HTML, request, notify and provide,
    /// and unsolicited text up to 20 MiB.
    pub const CLIENT_DEFAULT: Self = Self {
        actions: ACTION_REQUEST | ACTION_NOTIFY | ACTION_PROVIDE,
        formats: FORMAT_TEXT | (1 << 1) | (1 << 2),
        text_size: Some(20 * 1024 * 1024),
    };

    /// What wlshare's own ends take: text, every action, and no unsolicited
    /// text — a change is notified and the text asked for, as the extension
    /// recommends, so a large clipboard crosses only when it is wanted.
    pub const WLSHARE: Self = Self { actions: ALL_ACTIONS, formats: FORMAT_TEXT, text_size: Some(0) };

    pub fn takes(&self, action: u32) -> bool {
        self.actions & action != 0
    }

    pub fn takes_text(&self) -> bool {
        self.formats & FORMAT_TEXT != 0
    }
}

/// One extended cut text, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Caps(Caps),
    /// The peer wants these formats, as a provide.
    Request { formats: u32 },
    /// The peer wants a notify of what this end holds.
    Peek,
    /// The peer's clipboard changed and holds these formats; none is a
    /// clipboard that was cleared or holds nothing this extension carries.
    Notify { formats: u32 },
    /// The peer's clipboard. `None` when the provide carried no text.
    Provide { text: Option<String> },
}

/// Why an extended cut text could not be read. None of these is a broken
/// connection — the message was framed and has been consumed — so an end
/// drops the message and carries on.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClipboardError {
    #[error("an extended clipboard message of {0} bytes, shorter than its flags")]
    Short(usize),
    #[error("an extended clipboard message with actions {0:#010x}, not exactly one")]
    Actions(u32),
    #[error("a caps message that ends before the size of each of its formats")]
    ShortCaps,
    #[error("a provide whose zlib stream does not inflate: {0}")]
    Inflate(String),
    #[error("a provide that ends inside the data it names")]
    ShortProvide,
    #[error("a provide of {0} bytes of text, over the {MAX_TEXT}-byte ceiling")]
    TooLong(u64),
    #[error("text of {0} bytes, over the {MAX_TEXT}-byte ceiling")]
    TooLongToSend(usize),
}

fn flags(actions: u32, formats: u32) -> [u8; 4] {
    (actions | (formats & FORMAT_MASK)).to_be_bytes()
}

/// A caps body: the flags, then the largest unsolicited size of each format
/// named, in format order. Text is the only format this crate names.
pub fn caps(caps: &Caps) -> Vec<u8> {
    let formats = if caps.text_size.is_some() { FORMAT_TEXT } else { 0 };
    let mut body = flags(caps.actions | ACTION_CAPS, formats).to_vec();
    if let Some(size) = caps.text_size {
        body.extend_from_slice(&size.to_be_bytes());
    }
    body
}

/// A notify body: text is held when `has_text`, nothing otherwise.
pub fn notify(has_text: bool) -> Vec<u8> {
    flags(ACTION_NOTIFY, if has_text { FORMAT_TEXT } else { 0 }).to_vec()
}

/// A request body for the text.
pub fn request() -> Vec<u8> {
    flags(ACTION_REQUEST, FORMAT_TEXT).to_vec()
}

/// A peek body.
pub fn peek() -> Vec<u8> {
    flags(ACTION_PEEK, 0).to_vec()
}

/// A provide body carrying `text`. Refused over [`MAX_TEXT`] rather than cut:
/// a clipboard that arrived shorter would look whole.
pub fn provide(text: &str) -> Result<Vec<u8>, ClipboardError> {
    if text.len() > MAX_TEXT {
        return Err(ClipboardError::TooLongToSend(text.len()));
    }
    let wire = to_wire(text);
    let mut encoder = ZlibEncoder::new(flags(ACTION_PROVIDE, FORMAT_TEXT).to_vec(), Compression::default());
    // Writing into a Vec cannot fail.
    encoder.write_all(&(wire.len() as u32).to_be_bytes()).expect("deflating into memory");
    encoder.write_all(&wire).expect("deflating into memory");
    Ok(encoder.finish().expect("deflating into memory"))
}

/// Read one extended cut text's body.
pub fn parse(body: &[u8]) -> Result<Message, ClipboardError> {
    let Some((head, rest)) = body.split_first_chunk::<4>() else { return Err(ClipboardError::Short(body.len())) };
    let flags = u32::from_be_bytes(*head);
    let (actions, formats) = (flags & ACTION_MASK, flags & FORMAT_MASK);
    // Caps may come with any other action bit set: they are what it takes.
    if actions & ACTION_CAPS != 0 {
        return parse_caps(actions, formats, rest);
    }
    match actions {
        ACTION_REQUEST => Ok(Message::Request { formats }),
        ACTION_PEEK => Ok(Message::Peek),
        ACTION_NOTIFY => Ok(Message::Notify { formats }),
        ACTION_PROVIDE => parse_provide(formats, rest),
        other => Err(ClipboardError::Actions(other)),
    }
}

fn parse_caps(actions: u32, formats: u32, sizes: &[u8]) -> Result<Message, ClipboardError> {
    // One size per format named, in format order; text is bit 0, so its size is
    // the first when it is named at all.
    let named = formats.count_ones() as usize;
    if sizes.len() < named * 4 {
        return Err(ClipboardError::ShortCaps);
    }
    let text_size = (formats & FORMAT_TEXT != 0).then(|| u32::from_be_bytes([sizes[0], sizes[1], sizes[2], sizes[3]]));
    Ok(Message::Caps(Caps { actions, formats, text_size }))
}

fn parse_provide(formats: u32, deflated: &[u8]) -> Result<Message, ClipboardError> {
    if formats & FORMAT_TEXT == 0 {
        return Ok(Message::Provide { text: None });
    }
    // Text is bit 0 and so first in the stream: its length, then its bytes. The
    // formats after it are never inflated. The length is read before the text,
    // so a stream that inflates without end is refused on what it claims.
    let mut stream = ZlibDecoder::new(deflated);
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).map_err(|e| ClipboardError::Inflate(e.to_string()))?;
    let length = u32::from_be_bytes(length);
    // The wire form is the text, a CR per line and the NUL, so a text at the
    // ceiling claims more than the ceiling. Twice it bounds what is inflated,
    // and the text itself is measured after.
    if u64::from(length) > 2 * MAX_TEXT as u64 + 1 {
        return Err(ClipboardError::TooLong(u64::from(length)));
    }
    let mut wire = Vec::with_capacity(length as usize);
    stream.take(u64::from(length)).read_to_end(&mut wire).map_err(|e| ClipboardError::Inflate(e.to_string()))?;
    if wire.len() != length as usize {
        return Err(ClipboardError::ShortProvide);
    }
    let text = from_wire(&wire);
    if text.len() > MAX_TEXT {
        return Err(ClipboardError::TooLong(text.len() as u64));
    }
    Ok(Message::Provide { text: Some(text) })
}

/// LF text to the wire's: every line ending CRLF, then the NUL. A CRLF or a
/// lone CR already in the text is one line ending, not two.
fn to_wire(text: &str) -> Vec<u8> {
    let lf = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut wire = lf.replace('\n', "\r\n").into_bytes();
    wire.push(0);
    wire
}

/// The wire's text back to LF text: up to the NUL, CRLF to LF. Lossy on a
/// stray byte rather than refusing the whole clipboard over it.
fn from_wire(wire: &[u8]) -> String {
    let end = wire.iter().position(|&b| b == 0).unwrap_or(wire.len());
    String::from_utf8_lossy(&wire[..end]).replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provide body built by hand, for any formats: flags, then one zlib
    /// stream of each format's length and bytes. The independent encoder the
    /// parser is checked against.
    fn provide_by_hand(entries: &[(u32, &[u8])]) -> Vec<u8> {
        let mut formats = 0;
        let mut stream = Vec::new();
        for (format, data) in entries {
            formats |= format;
            stream.extend_from_slice(&(data.len() as u32).to_be_bytes());
            stream.extend_from_slice(data);
        }
        let mut body = (ACTION_PROVIDE | formats).to_be_bytes().to_vec();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&stream).unwrap();
        body.extend_from_slice(&encoder.finish().unwrap());
        body
    }

    /// The independent decoder [`provide`] is checked against.
    fn inflate_by_hand(body: &[u8]) -> (u32, Vec<u8>) {
        let flags = u32::from_be_bytes(body[..4].try_into().unwrap());
        let mut stream = Vec::new();
        ZlibDecoder::new(&body[4..]).read_to_end(&mut stream).unwrap();
        (flags, stream)
    }

    #[test]
    fn the_small_bodies_have_the_documented_layouts() {
        assert_eq!(caps(&Caps::WLSHARE), vec![0x1F, 0, 0, 0x01, 0, 0, 0, 0]);
        assert_eq!(notify(true), vec![0x08, 0, 0, 0x01]);
        assert_eq!(notify(false), vec![0x08, 0, 0, 0]);
        assert_eq!(request(), vec![0x02, 0, 0, 0x01]);
        assert_eq!(peek(), vec![0x04, 0, 0, 0]);
    }

    #[test]
    fn provide_is_crlf_utf8_with_its_nul() {
        let (flags, stream) = inflate_by_hand(&provide("画面\nnaïve ☕").unwrap());
        assert_eq!(flags, ACTION_PROVIDE | FORMAT_TEXT);
        let wire = "画面\r\nnaïve ☕\0".as_bytes();
        assert_eq!(&stream[..4], &(wire.len() as u32).to_be_bytes());
        assert_eq!(&stream[4..], wire);
        // A CRLF or lone CR already there is one line ending, not two.
        assert_eq!(inflate_by_hand(&provide("a\r\nb\rc").unwrap()).1[4..], *b"a\r\nb\r\nc\0");
    }

    #[test]
    fn the_messages_a_peer_sends_parse() {
        let mut theirs = (ACTION_CAPS | ACTION_REQUEST | ACTION_NOTIFY | FORMAT_TEXT | (1 << 2)).to_be_bytes().to_vec();
        theirs.extend_from_slice(&[0, 0, 0x10, 0, 0, 0, 0, 0]);
        assert_eq!(
            parse(&theirs),
            Ok(Message::Caps(Caps { actions: ACTION_CAPS | ACTION_REQUEST | ACTION_NOTIFY, formats: FORMAT_TEXT | (1 << 2), text_size: Some(0x1000) }))
        );
        assert_eq!(parse(&[0x02, 0, 0, 0x01]), Ok(Message::Request { formats: FORMAT_TEXT }));
        assert_eq!(parse(&[0x04, 0, 0, 0]), Ok(Message::Peek));
        assert_eq!(parse(&[0x08, 0, 0, 0x01]), Ok(Message::Notify { formats: FORMAT_TEXT }));
        assert_eq!(parse(&[0x08, 0, 0, 0]), Ok(Message::Notify { formats: 0 }));
        assert_eq!(parse(&provide_by_hand(&[(FORMAT_TEXT, b"one\r\ntwo\0")])), Ok(Message::Provide { text: Some("one\ntwo".into()) }));
    }

    #[test]
    fn a_format_after_the_text_is_stepped_over_and_one_without_text_is_none() {
        let body = provide_by_hand(&[(FORMAT_TEXT, b"the text\0"), (1 << 2, b"<b>the text</b>")]);
        assert_eq!(parse(&body), Ok(Message::Provide { text: Some("the text".into()) }));
        assert_eq!(parse(&provide_by_hand(&[(1 << 3, b"a bitmap")])), Ok(Message::Provide { text: None }));
    }

    #[test]
    fn what_is_not_one_message_is_an_error_and_not_a_panic() {
        assert_eq!(parse(&[0x08, 0, 0]), Err(ClipboardError::Short(3)));
        assert_eq!(parse(&[0x0A, 0, 0, 0x01]), Err(ClipboardError::Actions(ACTION_REQUEST | ACTION_NOTIFY)));
        assert_eq!(parse(&[0x01, 0, 0, 0x01, 0, 0]), Err(ClipboardError::ShortCaps));
        assert!(matches!(parse(&[0x10, 0, 0, 0x01, 1, 2, 3]), Err(ClipboardError::Inflate(_))));
        // A length that promises more than the stream holds.
        let mut stream = 100u32.to_be_bytes().to_vec();
        stream.extend_from_slice(b"short\0");
        let mut encoder = ZlibEncoder::new((ACTION_PROVIDE | FORMAT_TEXT).to_be_bytes().to_vec(), Compression::fast());
        encoder.write_all(&stream).unwrap();
        assert_eq!(parse(&encoder.finish().unwrap()), Err(ClipboardError::ShortProvide));
    }

    #[test]
    fn a_text_at_the_ceiling_crosses_and_one_past_it_does_not() {
        // Line breaks, whose CRs make the wire form longer than the text.
        let text = format!("{}\n{}", "a".repeat(1000), "b".repeat(MAX_TEXT - 1001));
        assert_eq!(text.len(), MAX_TEXT);
        assert_eq!(parse(&provide(&text).unwrap()), Ok(Message::Provide { text: Some(text) }));
        assert_eq!(provide(&"é".repeat(MAX_TEXT / 2 + 1)), Err(ClipboardError::TooLongToSend(MAX_TEXT + 2)));
    }

    /// A few kilobytes that claim gigabytes are refused on the claim, before
    /// anything is inflated.
    #[test]
    fn a_provide_that_inflates_without_end_is_refused_on_what_it_claims() {
        let mut encoder = ZlibEncoder::new((ACTION_PROVIDE | FORMAT_TEXT).to_be_bytes().to_vec(), Compression::best());
        encoder.write_all(&u32::MAX.to_be_bytes()).unwrap();
        encoder.write_all(&vec![b'a'; 4 * 1024 * 1024]).unwrap();
        let bomb = encoder.finish().unwrap();
        assert!(bomb.len() < 64 * 1024);
        assert_eq!(parse(&bomb), Err(ClipboardError::TooLong(u64::from(u32::MAX))));
        // And one that claims its size honestly is measured once inflated.
        let text = "a".repeat(MAX_TEXT + 1);
        let body = provide_by_hand(&[(FORMAT_TEXT, format!("{text}\0").as_bytes())]);
        assert_eq!(parse(&body), Err(ClipboardError::TooLong(MAX_TEXT as u64 + 1)));
    }
}
