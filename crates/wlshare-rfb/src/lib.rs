//! The RFB wire wlshare speaks.
//!
//! Everything that decides bytes on the socket lives here and nowhere else: the
//! handshake pieces, the client messages and how they parse, the server messages
//! and how they are built, RSA-AES, the ZRLE encoder and the private extensions.
//! The daemon crate turns compositor events into calls on this crate and copies
//! the results to sockets; it writes no protocol byte of its own.
//!
//! The crate is platform-independent so that all of it is unit-tested on any
//! machine, the daemon being buildable only where libwayland and libxkbcommon
//! are. The audio extension's FLAC encoder is libFLAC and its Opus encoder
//! libopus, each from a prebuilt static archive, as libvpx is under
//! `screen-vp9`, so the crate builds and runs with neither installed.
//! Tests here use an independent decoder for
//! every encoder, so the two halves cannot share a misunderstanding.
//!
//! ## What is on the wire, and what is not
//!
//! RFB 3.8 as RFC 6143 has it, with these choices:
//!
//! - **Raw or ZRLE, and VP9 for a client that lists it.** ZRLE is the standard's
//!   best lossless encoding, and the remotex gateway lists it as its first
//!   standard pixel encoding. **The VP9 encoding** is a private one for the
//!   gateway: the whole framebuffer as one VP9 stream, 4:4:4 at a quality its
//!   owner sets unless the client asked for otherwise ([`vp9`]), for a desktop
//!   that moves. Raw is produced before the client's first `SetEncodings`, where
//!   the RFC requires it, and for a client whose list names neither. Tight,
//!   Hextile, RRE, CopyRect and every other lossy encoding are absent.
//! - **32-bit true colour only.** The server's native format is the compositor's
//!   XRGB8888, which is the `B, G, R, X` byte order the gateway forces. A client may
//!   ask for any 32-bit true-colour format with 8-bit channels and gets it by a
//!   swizzle; 8- and 16-bit formats and colour maps are refused.
//! - **ContinuousUpdates and Fence**, so a client that supports them gets frames
//!   as the screen changes with one pixel update in flight, instead of polling.
//! - **Cursor**, required of every client, so the compositor pointer stays out
//!   of captured pixels and its shape moves immediately at the client; and
//!   **Cursor With Alpha**, for a client that lists it, so the shape keeps its
//!   shadow and edges ([`cursor`]).
//! - **DesktopSize and ExtendedDesktopSize**, so a client may resize the desktop.
//! - **The density extension**, one pseudo-encoding and one message type, which
//!   is how a client learns the scale the framebuffer is drawn at and asks for
//!   the one it wants ([`density`]).
//! - **The outputs extension**, another pair of the same shape, which is how a
//!   client learns which outputs the compositor has and asks for the one it
//!   wants shared ([`outputs`]).
//! - **The audio extension**, a private pseudo-encoding and message type, which
//!   carries the desktop's sound over the same connection in the format the
//!   client chose: as FLAC, lossless, or as Opus for a client that lists a
//!   second pseudo-encoding beside the first. The client's messages and the
//!   stream's begin and end are the QEMU Audio extension's ([`audio`]).
//! - **The camera extension**, a third private pair, which is how a client lends
//!   the desktop a camera: the client plugs it and sends H.264, and the server
//!   says when the desktop's applications want frames ([`camera`]).
//! - **The microphone extension**, the camera's twin and a fourth private pair,
//!   which is how a client lends the desktop a microphone: the client plugs it,
//!   and sends 16-bit PCM in the format the server names while the desktop's
//!   applications record ([`microphone`]).
//! - **The scroll message**, one private client message, which is how a client
//!   scrolls by a distance in pixels where RFB's wheel buttons have only
//!   notches ([`scroll`]).
//! - **Extended Clipboard**, the registered one and the only clipboard spoken:
//!   text as UTF-8, notified and then asked for ([`clipboard`]). Latin-1 cut
//!   text is framed so that it can be skipped, and otherwise ignored.
//! - **None and RSA-AES** for security. The first is RFC 6143's; the second is
//!   RealVNC's, and the one way a client can name an account and have the
//!   session encrypted ([`rsa_aes`]). Classic VncAuth is deliberately absent: it
//!   names nobody and encrypts nothing after the login. Which is offered is the
//!   daemon's configuration; the crate speaks both.

pub mod audio;
pub mod camera;
// Not the wire: the file a daemon run by hand writes of what its VP9 encoder
// was handed, and the tools that play it back read.
pub mod capture;
// Public for `examples/vp9-sink.rs`, the client the capture container runs
// against the daemon; the tests read the server's bytes with it.
pub mod client;
pub mod clipboard;
pub mod cursor;
pub mod density;
pub mod microphone;
pub mod msg;
pub mod outputs;
pub mod pixel;
pub mod rsa_aes;
pub mod scroll;
pub mod vp9;
pub mod zrle;

/// Raw: pixels as they are, in the client's format. RFC 6143 §7.7.1 requires
/// every server to produce it until asked for something else.
pub const ENCODING_RAW: i32 = 0;
/// ZRLE, the encoding this server chooses when the client lists it and not
/// [`ENCODING_VP9`].
pub const ENCODING_ZRLE: i32 = 16;
/// The VP9 encoding, the ASCII bytes `WLSV`: a client that lists it, with a
/// quality beside it ([`vp9::Vp9Stream`]), is sent every picture as one
/// rectangle over the whole framebuffer, a frame of one VP9 stream ([`vp9`]).
/// It wins over ZRLE wherever it is listed; listed without a quality it is
/// fatal, since the server has none of its own.
pub const ENCODING_VP9: i32 = 0x574c_5356;
/// DesktopSize pseudo-encoding: a rectangle announcing the framebuffer's new size.
pub const ENCODING_DESKTOP_SIZE: i32 = -223;
/// LastRect pseudo-encoding: a client that lists it accepts an update whose
/// rectangle count is a ceiling rather than a promise.
pub const ENCODING_LAST_RECT: i32 = -224;
/// Cursor pseudo-encoding. Every client must list it, because captured frames
/// contain no pointer: the compositor's cursor image goes out as [`cursor`]
/// has it.
pub const ENCODING_CURSOR: i32 = -239;
/// Cursor With Alpha pseudo-encoding: a client that lists it is sent the cursor
/// image with its alpha, instead of cut to a mask.
pub const ENCODING_CURSOR_WITH_ALPHA: i32 = -314;
/// ExtendedDesktopSize pseudo-encoding, and with it the client's SetDesktopSize.
pub const ENCODING_EXTENDED_DESKTOP_SIZE: i32 = -308;
/// Fence pseudo-encoding: the client will echo markers the server sends.
pub const ENCODING_FENCE: i32 = -312;
/// ContinuousUpdates pseudo-encoding: the client understands
/// EndOfContinuousUpdates and may enable continuous updates.
pub const ENCODING_CONTINUOUS_UPDATES: i32 = -313;
/// Extended Clipboard pseudo-encoding: the clipboard as UTF-8 ([`clipboard`]).
pub const ENCODING_EXTENDED_CLIPBOARD: i32 = 0xc0a1_e5ce_u32 as i32;
/// The density extension's pseudo-encoding, the ASCII bytes `WLSH`.
pub const ENCODING_DENSITY: i32 = 0x574c_5348;
/// The outputs extension's pseudo-encoding, the ASCII bytes `WLSO`.
pub const ENCODING_OUTPUTS: i32 = 0x574c_534f;
/// The camera extension's pseudo-encoding, the ASCII bytes `WLSC`.
pub const ENCODING_CAMERA: i32 = 0x574c_5343;
/// The microphone extension's pseudo-encoding, the ASCII bytes `WLSM`.
pub const ENCODING_MICROPHONE: i32 = 0x574c_534d;
/// The audio extension's pseudo-encoding, the ASCII bytes `WLSF`: a client that
/// lists it can take the desktop's sound, as FLAC unless it asks otherwise, and
/// is told so by an empty rectangle of this encoding ([`audio`]).
pub const ENCODING_AUDIO: i32 = 0x574c_5346;
/// Listed beside [`ENCODING_AUDIO`], asks for the sound as Opus in place of
/// FLAC: the ASCII bytes `WLOP` ([`audio::Codec`]).
pub const ENCODING_AUDIO_OPUS: i32 = 0x574c_4f50;
