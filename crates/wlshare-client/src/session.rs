//! One connection, from the TCP handshake to the socket closing.
//!
//! This is the daemon's `session.rs` read backwards. It runs on its own thread,
//! on a current-thread tokio runtime, and it owns everything about the wire; the
//! window owns nothing but pixels it reads under a lock and events it posts to a
//! channel. The two never block each other for longer than a memcpy.
//!
//! What it speaks, and why:
//!
//! - **VP9 or ZRLE**, as the window chose, and nothing else. VP9 is wlshare's
//!   own encoding for a desktop client: the whole framebuffer as one 4:4:4
//!   stream at the quality the server sets, decoded by one decoder for
//!   the connection off the framebuffer's lock and copied in under it. It is
//!   listed alone, and pixels in any other encoding end the session: a server
//!   without VP9 is an error to be told about, not a picture to fall back to.
//!   ZRLE is exact, on one inflate stream for the whole connection, decoded
//!   straight into the framebuffer, with Raw listed behind it because the RFC
//!   has every server able to send it.
//! - **The server's own pixel format.** Asking for `XRGB8888` is asking for the
//!   framebuffer's bytes as they are, which is also what the window's texture
//!   reads; no pixel is swizzled anywhere between the compositor and the screen.
//! - **Cursor and Cursor With Alpha**, because captured frames have no pointer
//!   painted in them and a client that does not draw one has no pointer at all.
//! - **Fence and ContinuousUpdates**, so frames arrive as the desktop changes
//!   with one update in flight, rather than one round trip per frame. The
//!   fence that ends an update is echoed once the *window* has taken the
//!   pixels, not the moment they are decoded: the server times that round trip
//!   to decide what quality the link and this client will bear, so an echo
//!   sent from the decoder would tell it about the link and nothing about the
//!   screen. A window that cannot upload as fast as the desktop moves is
//!   exactly the client a coarser picture is for, and until the echo goes out
//!   the server encodes nothing further — a frame this window would only have
//!   overwritten unseen is never made. [`PAINT_GRACE`] is the cutoff for a
//!   window that is not drawing at all.
//! - **The density extension**, which is what the window's 1× or 2× is: the
//!   window says what scale the desktop should be drawn at, the server draws it at that
//!   scale, and `SetDesktopSize` asks for a framebuffer the size of the window in
//!   device pixels. The picture is then one device pixel per framebuffer pixel
//!   and never resampled, at either scale.
//!
//! - **Extended Clipboard**, text as UTF-8, both ways, and the only clipboard
//!   there is: a latin-1 cut text is dropped. A change on the desktop is
//!   notified, asked for at once and handed to the window; the window's text
//!   is notified when the window gives it, and sent when the desktop asks.
//!
//! - **The audio extension**, when the window asked for sound: listed, and
//!   answered with a format and an enable once the server announces it. Each
//!   FLAC frame is decoded as it arrives, on this thread, into the
//!   [`Playback`] the platform's audio device takes from. Asked for and not
//!   announced is a server with no sound to give, and the session runs silent.
//!
//! The camera, the microphone and output selection are not spoken. Their
//! pseudo-encodings are not listed, so the server never offers them.

use std::future::{Future, pending};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;
use wlshare_rfb::audio::FlacDecoder;
use wlshare_rfb::client::{self, RectBody, ServerMsg};
use wlshare_rfb::clipboard::{self, Caps, Message as ClipboardMessage};
use wlshare_rfb::cursor::CursorImage;
use wlshare_rfb::density::from_fixed;
use wlshare_rfb::msg::{FENCE_REQUEST, PROTOCOL_VERSION, SECURITY_NONE};
use wlshare_rfb::pixel::PixelFormat;
use wlshare_rfb::rsa_aes::{self, ClientKey, Credentials, FrameReader, Sealer, Strength};
use wlshare_rfb::vp9::Vp9Decoder;
use wlshare_rfb::zrle::ZrleDecoder;
use wlshare_rfb::{
    ENCODING_AUDIO, ENCODING_CONTINUOUS_UPDATES, ENCODING_CURSOR, ENCODING_CURSOR_WITH_ALPHA, ENCODING_DENSITY,
    ENCODING_DESKTOP_SIZE, ENCODING_EXTENDED_DESKTOP_SIZE, ENCODING_FENCE, ENCODING_RAW, ENCODING_VP9, ENCODING_ZRLE,
};

use crate::audio::{self, Playback};
use crate::framebuffer::Framebuffer;

/// The pseudo-encodings every session lists, and deliberately few: each is a
/// promise to understand what it turns on, and [`Live::apply`] ends the
/// connection over a rectangle nobody asked for. The pixel encodings go in
/// front of them — [`ENCODING_VP9`] alone, or ZRLE and Raw — and
/// [`ENCODING_AUDIO`] follows them when the window asked for sound
/// ([`encodings`]).
const ENCODINGS: &[i32] = &[
    ENCODING_CURSOR_WITH_ALPHA,
    ENCODING_CURSOR,
    ENCODING_EXTENDED_DESKTOP_SIZE,
    ENCODING_DESKTOP_SIZE,
    ENCODING_FENCE,
    ENCODING_CONTINUOUS_UPDATES,
    ENCODING_DENSITY,
    clipboard::ENCODING,
];

fn encodings(encoding: Encoding, audio: bool) -> Vec<i32> {
    let mut encodings = Vec::with_capacity(ENCODINGS.len() + 3);
    match encoding {
        Encoding::Vp9 => encodings.push(ENCODING_VP9),
        Encoding::Zrle => encodings.extend_from_slice(&[ENCODING_ZRLE, ENCODING_RAW]),
    }
    encodings.extend_from_slice(ENCODINGS);
    if audio {
        encodings.push(ENCODING_AUDIO);
    }
    encodings
}

/// How long to wait for the far end to answer at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the handshake may take once connected, through ServerInit: a
/// server that accepts a socket and never answers would otherwise leave the
/// window connecting for good. Long enough for a PAM login that is slow to say
/// no.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a live resize settles before the desktop is asked to follow it.
/// A drag posts a size every frame, and each one the server honoured would be a
/// compositor mode change and a full repaint.
const RESIZE_SETTLE: Duration = Duration::from_millis(120);
/// Read this much of the socket at a time.
const CHUNK: usize = 256 * 1024;

/// How long a fence waits for the window to take the frame before it is echoed
/// anyway.
///
/// The wait is pacing, not a protocol promise: a window that has stopped
/// drawing — hidden, minimised, an app that only redraws on demand — must not
/// be able to stop the session. Past this the echo goes out unpainted and the
/// window is counted as one that is not drawing, so the fences after it are
/// echoed at once and the desktop keeps streaming at whatever the link bears;
/// the first paint after that puts the window back in the loop.
const PAINT_GRACE: Duration = Duration::from_millis(500);

/// Where to connect and as whom.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Empty asks for the `None` security type; anything else asks for RSA-AES,
    /// which is the only type this client authenticates with.
    pub password: String,
    /// Whether to ask for the desktop's sound. A server without the audio
    /// extension ignores the asking.
    pub audio: bool,
    /// How the desktop's pixels are to arrive.
    pub encoding: Encoding,
}

/// How the window wants the desktop's pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// wlshare's VP9 encoding: the whole desktop as one 4:4:4 stream at the
    /// quality the server sets. Small and smooth when it moves, not exact.
    /// Nothing else is listed, so a server without it ends the session.
    Vp9,
    /// ZRLE: every pixel exactly as the desktop drew it.
    Zrle,
}

/// What the window asks the desktop to be: its size in device pixels, and the
/// scale the desktop is drawn at — the window's 1× or 2×.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Surface {
    pub width: u16,
    pub height: u16,
    pub scale: f64,
}

impl Surface {
    fn is_usable(&self) -> bool {
        self.width > 0 && self.height > 0 && (0.5..=8.0).contains(&self.scale)
    }
}

/// What the window asks the session to do.
#[derive(Debug, Clone)]
pub enum Command {
    /// The button mask and the position, in the window's device pixels.
    Pointer { buttons: u8, x: u16, y: u16 },
    Key { down: bool, keysym: u32 },
    Surface(Surface),
    /// The window's clipboard, which the desktop may now have. Kept, notified,
    /// and sent only when the desktop asks for it.
    Clipboard(String),
    Shutdown,
}

/// How far the session has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connecting,
    /// The handshake is done and the framebuffer is the desktop's size.
    Ready,
    /// The session is over, for the reason in [`Status::error`] if it failed.
    Closed,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub state: State,
    pub error: Option<String>,
    /// The desktop's name, from ServerInit.
    pub name: String,
    /// The scale the server says the framebuffer is drawn at, which is what
    /// the density extension answers a `ClientDensity` with.
    pub scale: f64,
    /// How many updates have been decoded into the framebuffer. A session that
    /// is Ready with none of these is one whose desktop has not moved.
    pub frames: u64,
    /// Whether the desktop's sound has been turned on: asked for, announced by
    /// the server, and enabled. Until then there is nothing to play.
    pub audio: bool,
    /// How many FLAC frames have been decoded, twenty milliseconds each. A
    /// server's capture runs whether the desktop plays anything or not, so
    /// this counts up for as long as the sound is on.
    pub sound: u64,
}

impl Default for Status {
    fn default() -> Self {
        Self { state: State::Connecting, error: None, name: String::new(), scale: 1.0, frames: 0, audio: false, sound: 0 }
    }
}

/// Everything the window reads and the session writes. One lock each, held for
/// a decode or a copy and never across an await.
pub struct Shared {
    pub framebuffer: Mutex<Framebuffer>,
    pub status: Mutex<Status>,
    /// The pointer shape as it last changed, `None` for a pointer that is
    /// hidden or has not arrived. The generation is what tells the window it
    /// is looking at a shape it has not seen.
    pub cursor: Mutex<(u64, Option<CursorImage>)>,
    /// The desktop's clipboard as it last arrived, and which arrival that was:
    /// a generation the window has not seen is text it has not put on the
    /// window's clipboard yet. `None` until the desktop has provided any.
    pub clipboard: Mutex<(u64, Option<String>)>,
    /// The desktop's sound, decoded and waiting for the platform's audio device.
    pub audio: Mutex<Playback>,
    /// Called from the session's thread whenever there is something new to
    /// draw. The window uses it to post itself a redraw; it must not block.
    wake: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// How many times the window has taken pixels off the framebuffer
    /// ([`Shared::took_frame`]). Counted rather than flagged so the session can
    /// tell a window that has drawn since it gave up waiting for one from a
    /// window that still has not.
    paints: AtomicU64,
    /// Woken by the same, for the fence the session is holding.
    painted: Notify,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            framebuffer: Mutex::new(Framebuffer::new(0, 0)),
            status: Mutex::new(Status::default()),
            cursor: Mutex::new((0, None)),
            clipboard: Mutex::new((0, None)),
            audio: Mutex::new(Playback::default()),
            wake: Mutex::new(None),
            paints: AtomicU64::new(0),
            painted: Notify::new(),
        }
    }
}

impl Shared {
    pub fn set_wake(&self, wake: Option<Box<dyn Fn() + Send + Sync>>) {
        *self.wake.lock().unwrap() = wake;
    }

    /// The window took what was decoded: called from the window's thread, by
    /// `Client::with_frame` and only when there was damage to take. It is what
    /// releases the fence the session is holding, so it must be called after
    /// the pixels are on their way to the screen and not before.
    pub fn took_frame(&self) {
        self.paints.fetch_add(1, Ordering::Relaxed);
        // A permit rather than only waking whoever is registered: a paint
        // between the session reading the damage and waiting on this would
        // otherwise be a wake-up lost, and the fence would sit out its grace.
        self.painted.notify_one();
    }

    /// How many frames the window has taken.
    fn paints(&self) -> u64 {
        self.paints.load(Ordering::Relaxed)
    }

    pub fn wake(&self) {
        // Cloned out from under the lock would need the box to be Clone; called
        // under it, a callback that re-entered would deadlock. It does not:
        // each app's asks its window for a redraw and returns.
        if let Some(wake) = self.wake.lock().unwrap().as_ref() {
            wake();
        }
    }

    fn set_state(&self, state: State) {
        self.status.lock().unwrap().state = state;
        self.wake();
    }

    /// Record why the session ended. The first reason wins: a failure usually
    /// causes a second one on the way out.
    pub fn fail(&self, error: String) {
        let mut status = self.status.lock().unwrap();
        status.state = State::Closed;
        if status.error.is_none() {
            status.error = Some(error);
        }
        drop(status);
        self.wake();
    }
}

/// The socket's read side, opened frame by frame after RSA-AES.
enum Reader {
    Plain(OwnedReadHalf),
    Sealed(Box<FrameReader<OwnedReadHalf>>),
}

impl Reader {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(r) => r.read(buf).await,
            Self::Sealed(r) => r.read(buf).await,
        }
    }
}

/// The socket's write side: whole messages, each sealed into frames after
/// RSA-AES. Generic over what it writes to so that the tests can read back what
/// the session decided to send.
struct Writer<W> {
    inner: W,
    sealer: Option<Sealer>,
}

impl<W: AsyncWrite + Unpin> Writer<W> {
    async fn send(&mut self, message: &[u8]) -> std::io::Result<()> {
        match &mut self.sealer {
            Some(sealer) => self.inner.write_all(&sealer.frame(message)).await,
            None => self.inner.write_all(message).await,
        }
    }
}

/// Run one session to its end. The error it returns is the one the window
/// shows; it is recorded in [`Shared::status`] either way.
pub async fn run(config: Config, surface: Surface, shared: Arc<Shared>, commands: UnboundedReceiver<Command>) -> anyhow::Result<()> {
    let result = connect_and_run(config, surface, &shared, commands).await;
    match &result {
        Ok(()) => shared.set_state(State::Closed),
        Err(error) => shared.fail(format!("{error:#}")),
    }
    result
}

async fn connect_and_run(config: Config, surface: Surface, shared: &Arc<Shared>, mut commands: UnboundedReceiver<Command>) -> anyhow::Result<()> {
    // Before the socket, not after: generating one takes real CPU time, and the
    // server's handshake is on a clock.
    let key = match config.password.is_empty() {
        true => None,
        false => Some(ClientKey::generate().context("generating this session's RSA key")?),
    };

    // Beside the commands rather than in front of them: closing a client sends
    // Shutdown and then waits for this thread, and a socket that is still
    // connecting — or a server that accepts one and never sends its version —
    // would otherwise hold the window for as long as it pleased.
    let mut surface = surface;
    let mut local_clipboard = None;
    let connecting = async {
        let socket = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((config.host.as_str(), config.port)))
            .await
            .with_context(|| format!("connecting to {}:{} took over {CONNECT_TIMEOUT:?}", config.host, config.port))?
            .with_context(|| format!("connecting to {}:{}", config.host, config.port))?;
        socket.set_nodelay(true)?;
        let (reader, writer) = socket.into_split();
        tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(reader, writer, &config, key))
            .await
            .with_context(|| format!("{}:{} did not finish its handshake within {HANDSHAKE_TIMEOUT:?}", config.host, config.port))?
    };
    let Some((mut reader, mut writer, init)) = while_connecting(&mut commands, &mut surface, &mut local_clipboard, connecting).await.transpose()? else {
        return Ok(());
    };

    {
        let mut status = shared.status.lock().unwrap();
        status.name = init.name.clone();
        status.state = State::Ready;
    }
    shared.framebuffer.lock().unwrap().resize(init.width, init.height)?;
    shared.wake();

    // The server's own format, so that nothing on this path swizzles a pixel.
    writer.send(&client::set_pixel_format(&Framebuffer::FORMAT)).await?;
    writer.send(&client::set_encodings(&encodings(config.encoding, config.audio))).await?;

    let mut session = Live {
        shared: Arc::clone(shared),
        zrle: ZrleDecoder::default(),
        encoding: config.encoding,
        vp9: None,
        scratch: Vec::new(),
        format: Framebuffer::FORMAT,
        surface,
        asked_scale: None,
        asked_size: None,
        awaiting_scale: false,
        held_surface: None,
        cursor_generation: 0,
        owed_fence: None,
        gave_up_at: None,
        server_clipboard: None,
        local_clipboard,
        clipboard_generation: 0,
        audio: if config.audio { Audio::Asked } else { Audio::Off },
    };
    session.ask_for(&mut writer, surface).await?;
    session.stream_whole_desktop(&mut writer).await?;

    let mut buf = Vec::with_capacity(CHUNK);
    let mut chunk = vec![0u8; CHUNK];
    let mut settle: Option<Instant> = None;

    loop {
        // Copied out before the select: the wait below is a future for as long
        // as the select runs, and the branches that answer it take the session
        // by value.
        let owed = session.owed_fence_deadline();
        tokio::select! {
            read = reader.read(&mut chunk) => {
                let read = read.context("reading from the server")?;
                if read == 0 {
                    return Ok(());
                }
                buf.extend_from_slice(&chunk[..read]);
                let mut at = 0;
                while let Some((msg, used)) = client::parse(&buf[at..]).context("parsing what the server sent")? {
                    at += used;
                    session.handle(msg, &mut writer).await?;
                }
                buf.drain(..at);
            }
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()) };
                match command {
                    Command::Shutdown => return Ok(()),
                    Command::Surface(wanted) => {
                        // Coalesced, not queued: a live resize posts one of
                        // these a frame and only the last is worth honouring.
                        // Against the surface the window last said, not the one
                        // last asked for — a window dragged out and back before
                        // the settle would otherwise leave the timer armed for a
                        // size it only passed through.
                        if wanted.is_usable() && wanted != session.surface {
                            session.surface = wanted;
                            settle = Some(Instant::now() + RESIZE_SETTLE);
                        }
                    }
                    Command::Pointer { buttons, x, y } => {
                        let (x, y) = session.to_framebuffer(x, y);
                        writer.send(&client::pointer_event(buttons, x, y)).await?;
                    }
                    Command::Key { down, keysym } => writer.send(&client::key_event(down, keysym)).await?,
                    Command::Clipboard(text) => {
                        session.local_clipboard = Some(text);
                        session.announce_clipboard(&mut writer).await?;
                    }
                }
            }
            painted = async { match owed { Some(at) => wait_for_paint(shared, at).await, None => pending().await } } => {
                session.fence_painted(painted, &mut writer).await?;
            }
            () = async { match settle { Some(at) => tokio::time::sleep_until(at).await, None => pending().await } } => {
                settle = None;
                let wanted = session.surface;
                session.ask_for(&mut writer, wanted).await?;
            }
        }
    }
}

/// Run the handshake while the window is still being listened to, and answer
/// `None` if the window gave up before it finished — the session is then over
/// before it began.
///
/// Input to a desktop that does not exist yet is dropped. The window's surface
/// is not: a window resized while connecting, or switched between 1× and 2×,
/// is the surface the session starts at. Nor is its clipboard, which the
/// desktop is told of once it can be.
async fn while_connecting<T>(
    commands: &mut UnboundedReceiver<Command>,
    surface: &mut Surface,
    clipboard: &mut Option<String>,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            done = &mut work => return Some(done),
            command = commands.recv() => match command {
                None | Some(Command::Shutdown) => return None,
                Some(Command::Surface(wanted)) if wanted.is_usable() => *surface = wanted,
                Some(Command::Clipboard(text)) => *clipboard = Some(text),
                Some(_) => {}
            },
        }
    }
}

/// RFB 3.8's handshake from the client's end, through the security type to
/// ServerInit.
async fn handshake(
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    config: &Config,
    key: Option<ClientKey>,
) -> anyhow::Result<(Reader, Writer<OwnedWriteHalf>, client::ServerInit)> {
    client::read_version(&mut reader).await.context("reading the server's version")?;
    writer.write_all(PROTOCOL_VERSION).await?;

    let offered = client::read_security_types(&mut reader).await.context("reading the security types")?;
    let (mut reader, mut writer) = match key {
        Some(key) => {
            // RSA-AES at the widest both ends have, which is how the password
            // is protected and the rest of the session encrypted.
            let chosen = [rsa_aes::SECURITY_RSA_AES_256, rsa_aes::SECURITY_RSA_AES_128]
                .into_iter()
                .find(|t| offered.contains(t))
                .with_context(|| format!("a password was given and the server offers no RSA-AES, only {offered:?}"))?;
            writer.write_all(&[chosen]).await?;
            let strength = Strength::of(chosen).expect("one of the two just chosen");
            let exchange = rsa_aes::begin(&mut reader, &mut writer, strength, key).await.context("the RSA-AES key exchange")?;
            log::info!("the server's key is {}, asking for {:?}", exchange.fingerprint(), exchange.subtype());
            let credentials = Credentials { username: config.username.clone(), password: config.password.clone() };
            let session = exchange.login(&mut writer, &credentials).await.context("sending the credentials")?;
            (Reader::Sealed(Box::new(FrameReader::new(reader, session.opener))), Writer { inner: writer, sealer: Some(session.sealer) })
        }
        None => {
            if !offered.contains(&SECURITY_NONE) {
                bail!("the server wants a password: it offers security types {offered:?} and none of them is None");
            }
            writer.write_all(&[SECURITY_NONE]).await?;
            (Reader::Plain(reader), Writer { inner: writer, sealer: None })
        }
    };

    let init = match &mut reader {
        Reader::Plain(r) => finish_handshake(r, &mut writer).await,
        Reader::Sealed(r) => finish_handshake(r, &mut writer).await,
    }?;
    Ok((reader, writer, init))
}

async fn finish_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(reader: &mut R, writer: &mut Writer<W>) -> anyhow::Result<client::ServerInit> {
    client::read_security_result(reader).await.context("the login")?;
    writer.send(&client::client_init()).await?;
    let init = client::read_server_init(reader).await.context("reading ServerInit")?;
    if init.format.bits_per_pixel != 32 {
        bail!("the server's framebuffer is {} bits per pixel, and this client only reads 32", init.format.bits_per_pixel);
    }
    log::info!("connected to {:?}, {}x{}", init.name, init.width, init.height);
    Ok(init)
}

/// The session once it is past the handshake: what it needs to turn the
/// server's messages into pixels and the window's events into messages.
struct Live {
    shared: Arc<Shared>,
    zrle: ZrleDecoder,
    /// What the window asked for, which decides whether a VP9 rectangle is one.
    encoding: Encoding,
    /// The connection's VP9 decoder, made at its first frame: every frame is
    /// coded against the ones before it, so one decoder takes them all.
    vp9: Option<Vp9Decoder>,
    /// A VP9 frame decoded, before it is copied into the framebuffer: the
    /// decode is too long to hold the window's lock for.
    scratch: Vec<u8>,
    format: PixelFormat,
    /// The window's surface as it last said.
    surface: Surface,
    asked_scale: Option<f64>,
    asked_size: Option<(u16, u16)>,
    /// Set while a declared density is waiting for the `OutputScale` that
    /// answers it. Nothing else may go out until it arrives ([`Live::ask_for`]).
    awaiting_scale: bool,
    /// The window's latest surface, waiting for the density in flight to be
    /// answered ([`Live::ask_for`]).
    held_surface: Option<Surface>,
    cursor_generation: u64,
    /// The fence the server asked for, held until the window has taken the
    /// frame the update in front of it decoded.
    owed_fence: Option<OwedFence>,
    /// The paint count when a held fence last ran out its grace, for as long as
    /// the window has not drawn since. `None` while the window is keeping up,
    /// which is every fence's starting state.
    gave_up_at: Option<u64>,
    /// What the server takes of the clipboard, from its caps. `None` until they
    /// arrive, and for a server that never sends them: no clipboard is shared.
    server_clipboard: Option<Caps>,
    /// The window's clipboard as the window last gave it, which is what the
    /// desktop is notified of and sent when it asks — never more than the
    /// window chose to hand over.
    local_clipboard: Option<String>,
    clipboard_generation: u64,
    audio: Audio,
}

/// A fence echo waiting for the window, as [`Live::owe_fence`] left it.
struct OwedFence {
    /// The server's flags without [`FENCE_REQUEST`]: what an echo is.
    flags: u32,
    /// Echoed back untouched — it is the server's own, and how it tells one
    /// frame's round trip from another's.
    payload: Vec<u8>,
    /// When [`PAINT_GRACE`] runs out.
    deadline: Instant,
}

/// What became of a fence the window was given time to paint for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Painted {
    /// The window took the frame, which is what the fence was waiting for.
    Took,
    /// The grace ran out first, with the paint count as it stood then — read
    /// where a paint could not have slipped past it, so that a window drawing
    /// again is told apart from the draw that was already counted.
    GaveUp { paints: u64 },
}

/// Wait until the window has taken everything decoded, or until the grace runs
/// out. Borrows the session's shared state and not the session, so the `select!`
/// this sits in can still hand the answer back to it.
async fn wait_for_paint(shared: &Shared, deadline: Instant) -> Painted {
    loop {
        // Registered before the damage is read, so a paint between the two is a
        // permit waiting here rather than a wake-up missed.
        let painted = shared.painted.notified();
        tokio::pin!(painted);
        painted.as_mut().enable();
        if !shared.framebuffer.lock().unwrap().has_damage() {
            return Painted::Took;
        }
        if tokio::time::timeout_at(deadline, painted).await.is_err() {
            // Under the lock the paint itself takes, which is what makes the
            // count worth carrying: a window drawing as the grace ran out
            // either cleared the damage — and this is no giving up at all — or
            // has not reached `took_frame` yet, so its paint counts as the
            // drawing that puts it back in the loop rather than as the one
            // that was given up on.
            let framebuffer = shared.framebuffer.lock().unwrap();
            if !framebuffer.has_damage() {
                return Painted::Took;
            }
            return Painted::GaveUp { paints: shared.paints() };
        }
    }
}

/// How far the audio extension has got on this connection.
enum Audio {
    /// Not asked for, so never announced.
    Off,
    /// Listed, and waiting for the server to say it speaks it.
    Asked,
    /// Announced and enabled. The decoder is the stream's, from its begin to
    /// its end; frames outside one are dropped.
    On(Option<Box<FlacDecoder>>),
}

impl Live {
    /// Ask the desktop to be this window: drawn at the scale the window chose,
    /// and as many pixels across as the window has device pixels. Together those
    /// make the framebuffer a device-pixel-for-device-pixel match.
    ///
    /// A change of density is one `ClientDensity` carrying the size with it,
    /// which the server applies as one output configuration. A size at an
    /// unchanged density is a `SetDesktopSize`.
    ///
    /// **One at a time.** The server applies both through
    /// wlr-output-management, whose configurations carry a serial the
    /// compositor bumps on every commit, so the second of two in flight is
    /// cancelled and comes back as an invalid layout. While a density is in
    /// flight the window's latest surface — its size, or a density of its own
    /// — waits in `held_surface` for the `OutputScale` the
    /// extension promises for every declaration; [`Live::released_by`]
    /// recognises it, and the surface is then asked for as if just posted.
    async fn ask_for<W: AsyncWrite + Unpin>(&mut self, writer: &mut Writer<W>, surface: Surface) -> anyhow::Result<()> {
        if !surface.is_usable() {
            return Ok(());
        }
        if self.awaiting_scale {
            // Replaces whatever older surface was waiting.
            self.held_surface = Some(surface);
            return Ok(());
        }
        let size = (surface.width, surface.height);
        if self.asked_scale != Some(surface.scale) {
            writer.send(&client::client_density(surface.width, surface.height, surface.scale)).await?;
            self.asked_scale = Some(surface.scale);
            self.asked_size = Some(size);
            self.awaiting_scale = true;
            return Ok(());
        }
        if self.asked_size != Some(size) {
            writer.send(&client::set_desktop_size(surface.width, surface.height)).await?;
            self.asked_size = Some(size);
        }
        Ok(())
    }

    /// Whether an `OutputScale` of this scale is the answer to the density that
    /// was declared, rather than the one every `SetEncodings` is answered with
    /// or one another client caused. Only the answer releases the size.
    fn released_by(&self, scale: f64) -> bool {
        self.asked_scale.is_some_and(|asked| (asked - scale).abs() < 0.001)
    }

    /// Turn on continuous updates over the whole framebuffer and ask for one
    /// full repaint to start from. Called again after every size change: the
    /// region enabled is the framebuffer that was, and the new one is bigger or
    /// smaller than it.
    async fn stream_whole_desktop<W: AsyncWrite + Unpin>(&mut self, writer: &mut Writer<W>) -> anyhow::Result<()> {
        let (width, height) = {
            let fb = self.shared.framebuffer.lock().unwrap();
            (fb.width(), fb.height())
        };
        writer.send(&client::enable_continuous_updates(true, 0, 0, width, height)).await?;
        writer.send(&client::framebuffer_update_request(false, 0, 0, width, height)).await?;
        Ok(())
    }

    /// The window's device pixels as framebuffer pixels. They are the same
    /// thing once the desktop has followed a resize, and briefly not while it
    /// is still catching up, which is exactly when a click must still land
    /// where it was aimed.
    fn to_framebuffer(&self, x: u16, y: u16) -> (u16, u16) {
        let fb = self.shared.framebuffer.lock().unwrap();
        let map = |value: u16, from: u16, to: u16| -> u16 {
            if from == 0 || to == 0 {
                return 0;
            }
            let scaled = u32::from(value) * u32::from(to) / u32::from(from);
            scaled.min(u32::from(to) - 1) as u16
        };
        (map(x, self.surface.width, fb.width()), map(y, self.surface.height, fb.height()))
    }

    /// Answer a fence the server asked for, holding the echo until the window
    /// has taken what the update in front of it decoded.
    ///
    /// The server sends one update at a time and waits for this echo, and it
    /// times the round trip to decide the quality the next frame is coded at.
    /// Answering from the decoder would time the link alone and call a window
    /// that cannot keep up a healthy one; answering from the paint is what
    /// makes the picture follow the whole path a frame takes to the screen.
    /// Nothing is held for a window with nothing to take — an update that drew
    /// no pixels, or one the window has already been through.
    async fn owe_fence<W: AsyncWrite + Unpin>(
        &mut self,
        flags: u32,
        payload: Vec<u8>,
        writer: &mut Writer<W>,
    ) -> anyhow::Result<()> {
        // A second request with one still held cannot happen while the server
        // keeps one update in flight, and if it ever did, the held one goes out
        // first: an echo dropped would stop the desktop for good.
        if let Some(owed) = self.owed_fence.take() {
            writer.send(&client::fence(owed.flags, &owed.payload)).await?;
        }
        let drawing = match self.gave_up_at {
            // The window has drawn since the fence that gave up on it, so it is
            // back in the loop.
            Some(paints) => self.shared.paints() > paints,
            None => true,
        };
        if drawing {
            self.gave_up_at = None;
            if self.shared.framebuffer.lock().unwrap().has_damage() {
                self.owed_fence = Some(OwedFence { flags, payload, deadline: Instant::now() + PAINT_GRACE });
                return Ok(());
            }
        }
        writer.send(&client::fence(flags, &payload)).await?;
        Ok(())
    }

    /// When the fence being held runs out of grace, for the loop to wait on.
    /// `None` when none is held.
    fn owed_fence_deadline(&self) -> Option<Instant> {
        self.owed_fence.as_ref().map(|owed| owed.deadline)
    }

    /// Echo the held fence, now that the window has taken the frame or has
    /// shown that it is not going to.
    async fn fence_painted<W: AsyncWrite + Unpin>(&mut self, painted: Painted, writer: &mut Writer<W>) -> anyhow::Result<()> {
        let Some(owed) = self.owed_fence.take() else { return Ok(()) };
        match painted {
            Painted::Took => self.gave_up_at = None,
            // The count the wait read, not one read here: the window may have
            // drawn since, and that paint is what puts it back in the loop.
            Painted::GaveUp { paints } => {
                log::debug!("the window has taken no frame in {PAINT_GRACE:?}; the desktop is not paced by it until it draws again");
                self.gave_up_at = Some(paints);
            }
        }
        writer.send(&client::fence(owed.flags, &owed.payload)).await?;
        Ok(())
    }

    async fn handle<W: AsyncWrite + Unpin>(&mut self, msg: ServerMsg, writer: &mut Writer<W>) -> anyhow::Result<()> {
        match msg {
            ServerMsg::Update(rects) => {
                log::debug!("an update of {} rectangles", rects.len());
                let mut resized = false;
                let mut drew = false;
                let mut announced = false;
                for rect in rects {
                    match self.apply(rect)? {
                        Applied::Nothing => {}
                        Applied::Drew => drew = true,
                        Applied::Resized => resized = true,
                        Applied::AudioAnnounced => announced = true,
                    }
                }
                // Answered after the update rather than inside it, so the
                // enable goes out once however the update was framed.
                if announced {
                    self.turn_audio_on(writer).await?;
                }
                if resized {
                    self.stream_whole_desktop(writer).await?;
                }
                if drew || resized {
                    self.shared.status.lock().unwrap().frames += 1;
                    self.shared.wake();
                }
            }
            // Echoed as the same fence without the request bit, which is what
            // lets the server send the next update — once the window has the
            // frame this one followed.
            ServerMsg::Fence { flags, payload } => {
                if flags & FENCE_REQUEST != 0 {
                    self.owe_fence(flags & !FENCE_REQUEST, payload, writer).await?;
                }
            }
            ServerMsg::OutputScale { width, height, fixed } => {
                let scale = from_fixed(fixed);
                log::debug!("the desktop is {width}x{height} at scale {scale}");
                self.shared.status.lock().unwrap().scale = scale;
                self.shared.wake();
                if self.awaiting_scale && self.released_by(scale) {
                    self.awaiting_scale = false;
                    if let Some(held) = self.held_surface.take() {
                        self.ask_for(writer, held).await?;
                    }
                }
            }
            // Latin-1, which this client does not speak.
            ServerMsg::CutText(_) => log::debug!("a latin-1 cut text; ignored"),
            ServerMsg::ExtendedCutText(body) => self.handle_clipboard(&body, writer).await?,
            // The server sends this once to say it understands them, and again
            // if they are ever turned off. Either way there is nothing to do.
            ServerMsg::EndOfContinuousUpdates => log::debug!("the server acknowledged continuous updates"),
            ServerMsg::AudioBegin => match &mut self.audio {
                Audio::On(decoder) => {
                    log::debug!("the desktop's sound begins");
                    *decoder = match FlacDecoder::new(audio::FORMAT) {
                        Ok(made) => Some(Box::new(made)),
                        Err(error) => {
                            log::warn!("{error}; the session runs silent");
                            None
                        }
                    };
                }
                _ => log::debug!("an audio begin that was never asked for; ignored"),
            },
            ServerMsg::AudioEnd => {
                log::debug!("the desktop's sound ends");
                if let Audio::On(decoder) = &mut self.audio {
                    *decoder = None;
                }
            }
            // One frame that does not decode costs its own 20 ms and no more:
            // each decodes on its own.
            ServerMsg::AudioFrame(frame) => match &mut self.audio {
                Audio::On(Some(decoder)) => match decoder.decode(&frame) {
                    Ok(samples) => {
                        self.shared.audio.lock().unwrap().push(&samples);
                        self.shared.status.lock().unwrap().sound += 1;
                    }
                    Err(error) => log::warn!("{error}; the frame is dropped"),
                },
                _ => log::debug!("a FLAC frame outside a stream; dropped"),
            },
        }
        Ok(())
    }

    /// The server speaks the audio extension: set the format and turn it on,
    /// once, and only if it was asked for.
    async fn turn_audio_on<W: AsyncWrite + Unpin>(&mut self, writer: &mut Writer<W>) -> anyhow::Result<()> {
        match self.audio {
            Audio::Asked => {
                log::info!("the server has sound; asking for {:?}", audio::FORMAT);
                writer.send(&client::audio_set_format(&audio::FORMAT)?).await?;
                writer.send(&client::audio_enable()).await?;
                self.audio = Audio::On(None);
                self.shared.status.lock().unwrap().audio = true;
                self.shared.wake();
            }
            Audio::Off => log::debug!("the server announced sound nobody asked for; ignored"),
            Audio::On(_) => {}
        }
        Ok(())
    }

    /// One of the server's Extended Clipboard messages. One that cannot be read
    /// is dropped rather than the session: it was framed, and is consumed.
    async fn handle_clipboard<W: AsyncWrite + Unpin>(&mut self, body: &[u8], writer: &mut Writer<W>) -> anyhow::Result<()> {
        let message = match clipboard::parse(body) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("{error}; ignored");
                return Ok(());
            }
        };
        if let ClipboardMessage::Caps(theirs) = message {
            log::debug!("the server's clipboard caps are {theirs:?}");
            // Answered every time, as the server sends them on every
            // SetEncodings; the first time, it is also when the desktop can be
            // told of a clipboard the window gave before there was a desktop.
            writer.send(&client::client_extended_cut_text(&clipboard::caps(&Caps::WLSHARE))).await?;
            let first = self.server_clipboard.is_none();
            self.server_clipboard = Some(theirs);
            if first {
                self.announce_clipboard(writer).await?;
            }
            return Ok(());
        }
        let Some(caps) = self.server_clipboard else {
            log::debug!("a clipboard message before the server's caps; ignored");
            return Ok(());
        };
        match message {
            ClipboardMessage::Caps(_) => unreachable!("handled above"),
            // Asked for at once rather than when something pastes: the window
            // puts it on its own clipboard as it arrives — the Mac's
            // pasteboard has no way to fetch it when something pastes. A
            // notify of nothing is a desktop clipboard emptied or holding what
            // is not text, and the window's is left as it is.
            ClipboardMessage::Notify { formats } => {
                if formats & clipboard::FORMAT_TEXT != 0 && caps.takes(clipboard::ACTION_REQUEST) {
                    writer.send(&client::client_extended_cut_text(&clipboard::request())).await?;
                }
            }
            ClipboardMessage::Provide { text: Some(text) } => {
                log::debug!("{} bytes of the desktop's clipboard", text.len());
                self.clipboard_generation += 1;
                *self.shared.clipboard.lock().unwrap() = (self.clipboard_generation, Some(text));
                self.shared.wake();
            }
            ClipboardMessage::Provide { text: None } => log::debug!("a clipboard with no text; ignored"),
            ClipboardMessage::Request { formats } => {
                if let Some(text) = &self.local_clipboard
                    && formats & clipboard::FORMAT_TEXT != 0
                    && caps.takes(clipboard::ACTION_PROVIDE)
                {
                    match clipboard::provide(text) {
                        Ok(body) => writer.send(&client::client_extended_cut_text(&body)).await?,
                        Err(error) => log::warn!("the clipboard is not sent: {error}"),
                    }
                }
            }
            ClipboardMessage::Peek => {
                if caps.takes(clipboard::ACTION_NOTIFY) {
                    writer.send(&client::client_extended_cut_text(&clipboard::notify(self.local_clipboard.is_some()))).await?;
                }
            }
        }
        Ok(())
    }

    /// Tell the desktop the window's clipboard changed: a notify, or — to a
    /// server that takes no notify — the text itself, if it takes that unasked.
    /// Nothing before the server's caps, which is when this is called again.
    async fn announce_clipboard<W: AsyncWrite + Unpin>(&mut self, writer: &mut Writer<W>) -> anyhow::Result<()> {
        let (Some(caps), Some(text)) = (self.server_clipboard, &self.local_clipboard) else { return Ok(()) };
        if caps.takes(clipboard::ACTION_NOTIFY) {
            writer.send(&client::client_extended_cut_text(&clipboard::notify(true))).await?;
        } else if caps.takes(clipboard::ACTION_PROVIDE)
            && caps.takes_text()
            && caps.text_size.is_some_and(|size| text.len() <= size as usize)
            && let Ok(body) = clipboard::provide(text)
        {
            writer.send(&client::client_extended_cut_text(&body)).await?;
        }
        Ok(())
    }

    fn apply(&mut self, rect: client::Rect) -> anyhow::Result<Applied> {
        let client::Rect { x, y, width, height, body } = rect;
        log::trace!("a {} rectangle of {width}x{height} at {x},{y}", match &body {
            RectBody::Raw(_) => "Raw",
            RectBody::Zrle(_) => "ZRLE",
            RectBody::Vp9(_) => "VP9",
            RectBody::DesktopSize => "DesktopSize",
            RectBody::ExtendedDesktopSize { .. } => "ExtendedDesktopSize",
            RectBody::Cursor { .. } => "Cursor",
            RectBody::AlphaCursor(_) => "AlphaCursor",
            RectBody::Audio => "Audio",
        });
        match body {
            RectBody::Raw(_) | RectBody::Zrle(_) if self.encoding == Encoding::Vp9 => {
                bail!("the server does not have wlshare's VP9 encoding — it is not wlshare, or a wlshare older than 0.0.30; choose ZRLE to connect to it")
            }
            RectBody::Raw(pixels) => {
                let mut fb = self.shared.framebuffer.lock().unwrap();
                if !fb.put_raw(x, y, width, height, &pixels) {
                    bail!("a Raw rectangle of {width}x{height} at {x},{y} is not inside a {}x{} framebuffer", fb.width(), fb.height());
                }
                Ok(Applied::Drew)
            }
            RectBody::Zrle(payload) => {
                let mut fb = self.shared.framebuffer.lock().unwrap();
                let (fb_width, fb_height) = (fb.width(), fb.height());
                let Some((out, stride)) = fb.rect_mut(x, y, width, height) else {
                    bail!("a ZRLE rectangle of {width}x{height} at {x},{y} is not inside a {fb_width}x{fb_height} framebuffer");
                };
                self.zrle
                    .decode_rect(&payload, usize::from(width), usize::from(height), &self.format, out, stride)
                    .with_context(|| format!("decoding a {width}x{height} ZRLE rectangle at {x},{y}"))?;
                fb.damage_rect(x, y, width, height);
                Ok(Applied::Drew)
            }
            RectBody::Vp9(frame) => {
                self.apply_vp9(x, y, width, height, &frame)?;
                Ok(Applied::Drew)
            }
            RectBody::DesktopSize => {
                self.resize(width, height)?;
                Ok(Applied::Resized)
            }
            // The rectangle's y is the status: anything but zero answers a
            // request of this client's and changes nothing.
            RectBody::ExtendedDesktopSize { .. } if y != 0 => {
                log::info!("the server refused a {width}x{height} desktop with status {y}");
                Ok(Applied::Nothing)
            }
            RectBody::ExtendedDesktopSize { .. } => {
                self.resize(width, height)?;
                Ok(Applied::Resized)
            }
            RectBody::Cursor { pixels, mask } => {
                let expected = (usize::from(width) * usize::from(height) * 4, usize::from(width).div_ceil(8) * usize::from(height));
                if (pixels.len(), mask.len()) != expected {
                    bail!("a {width}x{height} cursor of {} pixel bytes and {} mask bytes", pixels.len(), mask.len());
                }
                self.set_cursor(CursorImage::from_masked_rect(&self.format, width, height, (x, y), &pixels, &mask));
                Ok(Applied::Nothing)
            }
            RectBody::AlphaCursor(rgba) => {
                if rgba.len() != usize::from(width) * usize::from(height) * 4 {
                    bail!("a {width}x{height} cursor with alpha of {} bytes", rgba.len());
                }
                self.set_cursor(CursorImage::from_alpha_rect(width, height, (x, y), rgba));
                Ok(Applied::Nothing)
            }
            // The audio extension's announcement, which only comes to a client
            // that listed it.
            RectBody::Audio => Ok(Applied::AudioAnnounced),
        }
    }

    /// One frame of the VP9 stream, which is the whole framebuffer. It is
    /// decoded with the framebuffer's lock let go — the session is the only
    /// thing that resizes it, so its size cannot change meanwhile — and copied
    /// in under it.
    fn apply_vp9(&mut self, x: u16, y: u16, width: u16, height: u16, frame: &[u8]) -> anyhow::Result<()> {
        if self.encoding != Encoding::Vp9 {
            bail!("a VP9 rectangle, which this session never asked for");
        }
        let size = {
            let fb = self.shared.framebuffer.lock().unwrap();
            (fb.width(), fb.height())
        };
        if (x, y, width, height) != (0, 0, size.0, size.1) {
            bail!("a VP9 rectangle of {width}x{height} at {x},{y}, where the framebuffer is {}x{}", size.0, size.1);
        }
        let first = self.vp9.is_none();
        let decoder = match &mut self.vp9 {
            Some(decoder) => decoder,
            None => self.vp9.insert(Vp9Decoder::new().context("starting the VP9 decoder")?),
        };
        let (w, h) = (usize::from(width), usize::from(height));
        // As fallible as the framebuffer's own: a desktop there is not the
        // memory for twice ends the session, not the process.
        let len = w * h * 4;
        self.scratch
            .try_reserve_exact(len.saturating_sub(self.scratch.len()))
            .with_context(|| format!("allocating a {width}x{height} VP9 frame"))?;
        self.scratch.resize(len, 0);
        decoder.decode_rect(frame, w, h, &mut self.scratch, w * 4).with_context(|| format!("decoding a {width}x{height} VP9 frame"))?;
        let mut fb = self.shared.framebuffer.lock().unwrap();
        if !fb.put_raw(0, 0, width, height, &self.scratch) {
            bail!("a {width}x{height} VP9 frame does not fit a {}x{} framebuffer", fb.width(), fb.height());
        }
        drop(fb);
        if first {
            log::info!("the desktop arrives as VP9");
        }
        Ok(())
    }

    fn resize(&mut self, width: u16, height: u16) -> anyhow::Result<()> {
        log::info!("the desktop is now {width}x{height}");
        self.shared.framebuffer.lock().unwrap().resize(width, height)
    }

    fn set_cursor(&mut self, image: Option<CursorImage>) {
        self.cursor_generation += 1;
        *self.shared.cursor.lock().unwrap() = (self.cursor_generation, image);
        self.shared.wake();
    }
}

/// What a rectangle did, which decides whether the window is woken and whether
/// continuous updates have to be turned on over a framebuffer of a new size.
enum Applied {
    Nothing,
    Drew,
    Resized,
    /// The server speaks the audio extension.
    AudioAnnounced,
}

#[cfg(test)]
mod tests {
    use wlshare_rfb::density::to_fixed;
    use wlshare_rfb::msg::{ClientMsg, Screen};

    use super::*;
    use crate::framebuffer::Region;

    /// What the session wrote, read back with the server's own parser rather
    /// than by comparing bytes to bytes.
    fn sent(writer: &mut Writer<Vec<u8>>) -> Vec<ClientMsg> {
        let bytes = std::mem::take(&mut writer.inner);
        let mut messages = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let (message, used) = wlshare_rfb::msg::parse(&bytes[at..]).expect("a message").expect("a whole message");
            messages.push(message);
            at += used;
        }
        messages
    }

    fn writer() -> Writer<Vec<u8>> {
        Writer { inner: Vec::new(), sealer: None }
    }

    fn live() -> Live {
        Live {
            shared: Arc::new(Shared::default()),
            zrle: ZrleDecoder::default(),
            encoding: Encoding::Zrle,
            vp9: None,
            scratch: Vec::new(),
            format: Framebuffer::FORMAT,
            surface: Surface { width: 800, height: 600, scale: 2.0 },
            asked_scale: None,
            asked_size: None,
            awaiting_scale: false,
            held_surface: None,
            cursor_generation: 0,
            owed_fence: None,
            gave_up_at: None,
            server_clipboard: None,
            local_clipboard: None,
            clipboard_generation: 0,
            audio: Audio::Off,
        }
    }

    /// The bug this guards against is not hypothetical: sending both at once
    /// has the compositor cancel the second configuration, and the desktop
    /// answers a perfectly good size with *invalid layout*.
    #[tokio::test]
    async fn a_density_carries_its_size_and_a_resize_waits_for_its_answer() {
        let mut live = live();
        let mut writer = writer();
        let doubled = Surface { width: 1600, height: 1200, scale: 2.0 };

        live.ask_for(&mut writer, doubled).await.unwrap();
        assert_eq!(
            sent(&mut writer),
            vec![ClientMsg::ClientDensity { width: 1600, height: 1200, fixed: to_fixed(2.0) }],
            "the density carries the size, and nothing goes out beside it"
        );

        // The OutputScale that answers every SetEncodings, carrying the scale
        // the desktop is at now, is not the answer: a resize still waits.
        live.handle(ServerMsg::OutputScale { width: 1024, height: 768, fixed: to_fixed(1.0) }, &mut writer).await.unwrap();
        live.ask_for(&mut writer, Surface { width: 1400, height: 1000, scale: 2.0 }).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);

        live.handle(ServerMsg::OutputScale { width: 1600, height: 1200, fixed: to_fixed(2.0) }, &mut writer).await.unwrap();
        assert_eq!(
            sent(&mut writer),
            vec![ClientMsg::SetDesktopSize { width: 1400, height: 1000, screens: vec![Screen::whole(1400, 1000)] }],
            "the answer releases the size"
        );
        live.ask_for(&mut writer, doubled).await.unwrap();
        assert_eq!(
            sent(&mut writer),
            vec![ClientMsg::SetDesktopSize { width: 1600, height: 1200, screens: vec![Screen::whole(1600, 1200)] }]
        );

        // The same surface again is a window redrawing, not a window changing.
        live.ask_for(&mut writer, doubled).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);

        // A size change at the same density needs no configuration ahead of it.
        live.ask_for(&mut writer, Surface { width: 800, height: 600, scale: 2.0 }).await.unwrap();
        assert_eq!(
            sent(&mut writer),
            vec![ClientMsg::SetDesktopSize { width: 800, height: 600, screens: vec![Screen::whole(800, 600)] }]
        );
    }

    /// The window's 1×/2× switch is one density and nothing else: the size it
    /// carries is the one the window already has.
    #[tokio::test]
    async fn switching_between_1x_and_2x_at_one_size_sends_only_the_density() {
        let mut live = live();
        let mut writer = writer();
        live.ask_for(&mut writer, Surface { width: 1600, height: 1000, scale: 1.0 }).await.unwrap();
        live.handle(ServerMsg::OutputScale { width: 1600, height: 1000, fixed: to_fixed(1.0) }, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::ClientDensity { width: 1600, height: 1000, fixed: to_fixed(1.0) }]);

        live.ask_for(&mut writer, Surface { width: 1600, height: 1000, scale: 2.0 }).await.unwrap();
        live.handle(ServerMsg::OutputScale { width: 1600, height: 1000, fixed: to_fixed(2.0) }, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::ClientDensity { width: 1600, height: 1000, fixed: to_fixed(2.0) }], "one message per switch");

        live.ask_for(&mut writer, Surface { width: 1600, height: 1000, scale: 1.0 }).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::ClientDensity { width: 1600, height: 1000, fixed: to_fixed(1.0) }], "and back");
    }

    /// The same one at a time, from the other side: a window that is resized
    /// while a density is in flight must not slip a size past it, and what goes
    /// out when the answer comes is the size the window is now.
    #[tokio::test]
    async fn a_resize_that_catches_up_with_a_density_waits_for_it_as_well() {
        let mut live = live();
        let mut writer = writer();

        live.ask_for(&mut writer, Surface { width: 1600, height: 1200, scale: 2.0 }).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::ClientDensity { width: 1600, height: 1200, fixed: to_fixed(2.0) }]);

        live.ask_for(&mut writer, Surface { width: 1500, height: 1100, scale: 2.0 }).await.unwrap();
        live.ask_for(&mut writer, Surface { width: 1400, height: 1000, scale: 2.0 }).await.unwrap();
        assert_eq!(sent(&mut writer), vec![], "a size at the density still in flight waits with it");

        live.handle(ServerMsg::OutputScale { width: 1600, height: 1200, fixed: to_fixed(2.0) }, &mut writer).await.unwrap();
        assert_eq!(
            sent(&mut writer),
            vec![ClientMsg::SetDesktopSize { width: 1400, height: 1000, screens: vec![Screen::whole(1400, 1000)] }],
            "the size that goes out is the window's latest, not the one it held first"
        );

        // The density is answered, so the next size needs nothing ahead of it.
        live.ask_for(&mut writer, Surface { width: 1200, height: 900, scale: 2.0 }).await.unwrap();
        assert_eq!(
            sent(&mut writer),
            vec![ClientMsg::SetDesktopSize { width: 1200, height: 900, screens: vec![Screen::whole(1200, 900)] }]
        );
    }

    /// A window that changes scale again before the first density is answered
    /// must not have a second density slip past it either: the latest waits,
    /// and goes out once the first is answered.
    #[tokio::test]
    async fn a_density_that_catches_up_with_a_density_waits_for_it() {
        let mut live = live();
        let mut writer = writer();

        live.ask_for(&mut writer, Surface { width: 1600, height: 1200, scale: 2.0 }).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::ClientDensity { width: 1600, height: 1200, fixed: to_fixed(2.0) }]);

        live.ask_for(&mut writer, Surface { width: 800, height: 600, scale: 1.0 }).await.unwrap();
        assert_eq!(sent(&mut writer), vec![], "one density in flight at a time");

        live.handle(ServerMsg::OutputScale { width: 1600, height: 1200, fixed: to_fixed(2.0) }, &mut writer).await.unwrap();
        assert_eq!(
            sent(&mut writer),
            vec![ClientMsg::ClientDensity { width: 800, height: 600, fixed: to_fixed(1.0) }],
            "the latest density follows the answer, with its size"
        );

        live.handle(ServerMsg::OutputScale { width: 800, height: 600, fixed: to_fixed(1.0) }, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![], "and nothing follows it");
    }

    #[tokio::test]
    async fn a_fence_is_echoed_without_the_bit_that_asked_for_it() {
        let mut live = live();
        let mut writer = writer();
        // A framebuffer with nothing in it owes the window no paint, so this
        // one is not held for one.
        live.handle(ServerMsg::Fence { flags: FENCE_REQUEST | 4, payload: vec![1, 2, 3] }, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::Fence { flags: 4, payload: vec![1, 2, 3] }]);

        // One that asks for nothing is not echoed, or the two ends would echo
        // each other for as long as the session lasted.
        live.handle(ServerMsg::Fence { flags: 4, payload: vec![1] }, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);
    }

    /// The server sends one update at a time and codes the next one at the
    /// quality this round trip says the client can take, so what it has to
    /// measure is the window: an echo from the decoder would call a window
    /// that cannot upload as fast as the desktop moves a healthy one.
    #[tokio::test]
    async fn a_fence_waits_for_the_window_to_take_the_frame() {
        let mut live = live();
        let mut writer = writer();
        // A frame decoded into the framebuffer and not taken yet.
        live.shared.framebuffer.lock().unwrap().resize(64, 32).unwrap();

        live.handle(ServerMsg::Fence { flags: FENCE_REQUEST | 4, payload: vec![1, 2, 3] }, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![], "nothing goes back while the window still owes a paint");
        let deadline = live.owed_fence_deadline().expect("the fence is being held");

        // The window draws: it takes the damage and says so, as `with_frame`
        // does.
        let painter = tokio::spawn({
            let shared = Arc::clone(&live.shared);
            async move {
                shared.framebuffer.lock().unwrap().take_damage();
                shared.took_frame();
            }
        });
        assert_eq!(wait_for_paint(&live.shared, deadline).await, Painted::Took);
        painter.await.unwrap();

        live.fence_painted(Painted::Took, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::Fence { flags: 4, payload: vec![1, 2, 3] }]);
        assert!(live.owed_fence_deadline().is_none(), "and nothing is held after it");
    }

    /// A hidden window draws nothing, and a session that waited for it would
    /// be a desktop stopped by a window nobody is looking at.
    #[tokio::test(start_paused = true)]
    async fn a_window_that_is_not_drawing_does_not_hold_the_desktop_up() {
        let mut live = live();
        let mut writer = writer();
        live.shared.framebuffer.lock().unwrap().resize(64, 32).unwrap();

        live.handle(ServerMsg::Fence { flags: FENCE_REQUEST | 4, payload: vec![1] }, &mut writer).await.unwrap();
        let deadline = live.owed_fence_deadline().expect("the fence is being held");
        let painted = wait_for_paint(&live.shared, deadline).await;
        assert_eq!(painted, Painted::GaveUp { paints: 0 }, "the grace runs out, with the paints there had been");
        live.fence_painted(painted, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::Fence { flags: 4, payload: vec![1] }]);

        // The frame is still untaken, and the next fence goes back at once all
        // the same: a window that is not drawing is not something to pace to.
        live.handle(ServerMsg::Fence { flags: FENCE_REQUEST | 4, payload: vec![2] }, &mut writer).await.unwrap();
        assert!(live.shared.framebuffer.lock().unwrap().has_damage());
        assert_eq!(sent(&mut writer), vec![ClientMsg::Fence { flags: 4, payload: vec![2] }]);
        assert!(live.owed_fence_deadline().is_none());

        // One paint puts the window back in the loop.
        live.shared.took_frame();
        live.handle(ServerMsg::Fence { flags: FENCE_REQUEST | 4, payload: vec![3] }, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);
        assert!(live.owed_fence_deadline().is_some());
    }

    /// The window runs on its own thread, so a draw can land between the grace
    /// running out and the fence going back. Counting the paints at that point
    /// rather than at the wait's would swallow that draw, and the window would
    /// owe another one before the session paced to it again.
    #[tokio::test(start_paused = true)]
    async fn a_paint_that_lands_as_the_grace_runs_out_still_counts() {
        let mut live = live();
        let mut writer = writer();
        live.shared.framebuffer.lock().unwrap().resize(64, 32).unwrap();

        live.handle(ServerMsg::Fence { flags: FENCE_REQUEST | 4, payload: vec![1] }, &mut writer).await.unwrap();
        let deadline = live.owed_fence_deadline().expect("the fence is being held");
        let painted = wait_for_paint(&live.shared, deadline).await;
        assert_eq!(painted, Painted::GaveUp { paints: 0 });

        // The window draws between the two: the frame this fence waited for is
        // taken after all, just too late for the grace.
        live.shared.framebuffer.lock().unwrap().take_damage();
        live.shared.took_frame();
        live.fence_painted(painted, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::Fence { flags: 4, payload: vec![1] }]);

        // That draw is what puts the window back in the loop, so the next
        // frame's fence is held for it rather than echoed at once.
        live.shared.framebuffer.lock().unwrap().damage_all();
        live.handle(ServerMsg::Fence { flags: FENCE_REQUEST | 4, payload: vec![2] }, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);
        assert!(live.owed_fence_deadline().is_some());
    }

    #[tokio::test]
    async fn a_resize_turns_continuous_updates_on_over_the_framebuffer_that_is_now() {
        let mut live = live();
        let mut writer = writer();
        live.shared.framebuffer.lock().unwrap().resize(1024, 768).unwrap();

        let rect = client::Rect { x: 0, y: 0, width: 800, height: 600, body: RectBody::DesktopSize };
        live.handle(ServerMsg::Update(vec![rect]), &mut writer).await.unwrap();
        assert_eq!(
            sent(&mut writer),
            vec![
                ClientMsg::EnableContinuousUpdates { enable: true, x: 0, y: 0, width: 800, height: 600 },
                ClientMsg::FramebufferUpdateRequest { incremental: false, x: 0, y: 0, width: 800, height: 600 },
            ],
            "the region enabled was the old framebuffer's, and the new one is not that size"
        );
    }

    #[test]
    fn only_what_this_client_speaks_is_listed() {
        assert!(ENCODINGS.contains(&clipboard::ENCODING), "the clipboard");
        assert!(ENCODINGS.contains(&ENCODING_DENSITY), "the density is what 1× and 2× are");
    }

    fn audio_announcement() -> ServerMsg {
        ServerMsg::Update(vec![client::Rect { x: 0, y: 0, width: 0, height: 0, body: RectBody::Audio }])
    }

    #[test]
    fn sound_is_listed_only_when_the_window_asked_for_it() {
        assert!(!encodings(Encoding::Zrle, false).contains(&ENCODING_AUDIO));
        assert_eq!(encodings(Encoding::Zrle, true).last(), Some(&ENCODING_AUDIO));
        assert_eq!(&encodings(Encoding::Zrle, true)[2..2 + ENCODINGS.len()], ENCODINGS);
    }

    /// VP9 goes alone when chosen, with no pixel encoding behind it to fall
    /// back to — and not at all when not.
    #[test]
    fn vp9_is_listed_alone_when_the_window_chose_it() {
        let vp9 = encodings(Encoding::Vp9, true);
        assert_eq!(vp9[0], ENCODING_VP9);
        assert_eq!(&vp9[1..=ENCODINGS.len()], ENCODINGS);
        assert!(!vp9.contains(&ENCODING_ZRLE) && !vp9.contains(&ENCODING_RAW));
        let zrle = encodings(Encoding::Zrle, true);
        assert_eq!(&zrle[..2], [ENCODING_ZRLE, ENCODING_RAW]);
        assert!(!zrle.contains(&ENCODING_VP9));
    }

    /// A server without VP9 answers a list that names only it with Raw or
    /// ZRLE, and that ends the session rather than being shown.
    #[test]
    fn pixels_other_than_vp9_end_a_vp9_session() {
        let mut live = live();
        live.encoding = Encoding::Vp9;
        live.shared.framebuffer.lock().unwrap().resize(4, 2).unwrap();
        live.shared.framebuffer.lock().unwrap().take_damage();
        let raw = client::Rect { x: 0, y: 0, width: 4, height: 2, body: RectBody::Raw(vec![0x7F; 4 * 2 * 4]) };
        let error = live.apply(raw.clone()).err().expect("Raw in a VP9 session");
        assert!(error.to_string().contains("choose ZRLE"), "{error}");
        assert!(live.apply(client::Rect { body: RectBody::Zrle(vec![0; 8]), ..raw.clone() }).is_err());
        assert_eq!(live.shared.framebuffer.lock().unwrap().take_damage(), None, "nothing was drawn");

        live.encoding = Encoding::Zrle;
        assert!(live.apply(raw).is_ok(), "the same Raw rectangle is fine where it was listed");
    }

    /// A VP9 stream as the daemon sends it, frame by frame, lands in the
    /// framebuffer and is said to have arrived.
    #[tokio::test]
    async fn vp9_frames_are_decoded_into_the_framebuffer() {
        use wlshare_rfb::vp9::{Chroma, QUALITY_MAX, Vp9Encoder};

        let mut live = live();
        live.encoding = Encoding::Vp9;
        live.shared.framebuffer.lock().unwrap().resize(64, 32).unwrap();
        live.shared.framebuffer.lock().unwrap().take_damage();
        let mut writer = writer();
        let mut encoder = Vp9Encoder::new(64, 32, Chroma::Full, QUALITY_MAX).unwrap();
        for colour in [[200u8, 40, 10, 0], [10, 220, 60, 0]] {
            let mut wire = wlshare_rfb::msg::update_header(1).to_vec();
            wire.extend_from_slice(&wlshare_rfb::msg::rect_header(0, 0, 64, 32, ENCODING_VP9));
            encoder.encode_rect(&colour.repeat(64 * 32), 64 * 4, false, &mut wire).unwrap();
            let (message, used) = client::parse(&wire).unwrap().unwrap();
            assert_eq!(used, wire.len());
            live.handle(message, &mut writer).await.unwrap();

            let fb = live.shared.framebuffer.lock().unwrap();
            for pixel in fb.pixels().as_chunks::<4>().0 {
                assert!(pixel[..3].iter().zip(&colour).all(|(a, b)| a.abs_diff(*b) <= 4), "{pixel:?} for {colour:?}");
            }
        }
        assert_eq!(live.shared.framebuffer.lock().unwrap().take_damage(), Some(Region { x: 0, y: 0, width: 64, height: 32 }));
        assert_eq!(live.shared.status.lock().unwrap().frames, 2);
        assert_eq!(sent(&mut writer), vec![], "a frame is not answered");
    }

    #[test]
    fn a_vp9_rectangle_nobody_asked_for_or_not_the_whole_desktop_ends_the_session() {
        let mut live = live();
        live.shared.framebuffer.lock().unwrap().resize(64, 32).unwrap();
        let rect = |x, width| client::Rect { x, y: 0, width, height: 32, body: RectBody::Vp9(vec![0; 8]) };
        assert!(live.apply(rect(0, 64)).is_err(), "ZRLE was asked for");
        live.encoding = Encoding::Vp9;
        assert!(live.apply(rect(8, 56)).is_err(), "a VP9 frame is the whole framebuffer");
        assert!(live.apply(rect(0, 64)).is_err(), "and one that is not a frame is an error, not a panic");
    }

    /// The announcement is answered with the format and an enable, once; the
    /// frames of a stream are decoded into the playback buffer, and a frame
    /// outside a stream is not.
    #[tokio::test]
    async fn an_announced_stream_is_turned_on_and_its_frames_are_played() {
        use wlshare_rfb::audio::FlacEncoder;

        let mut live = live();
        live.audio = Audio::Asked;
        let mut writer = writer();
        live.handle(audio_announcement(), &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![ClientMsg::AudioFormat(audio::FORMAT), ClientMsg::AudioEnable]);
        assert!(live.shared.status.lock().unwrap().audio);
        live.handle(audio_announcement(), &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![], "turned on once");

        let tone: Vec<u8> = (0..audio::FORMAT.block_frames() as i16).flat_map(|n| [n.to_le_bytes(), (-n).to_le_bytes()]).flatten().collect();
        let frames = FlacEncoder::new(audio::FORMAT).unwrap().push(&[tone.clone(), tone.clone(), tone.clone(), tone].concat()).unwrap();
        let frame = |i: usize| ServerMsg::AudioFrame(frames[i][8..].to_vec());

        live.handle(frame(0), &mut writer).await.unwrap();
        assert_eq!(live.shared.audio.lock().unwrap().waiting(), 0, "a frame before a begin is dropped");

        live.handle(ServerMsg::AudioBegin, &mut writer).await.unwrap();
        for i in 0..3 {
            live.handle(frame(i), &mut writer).await.unwrap();
        }
        // Not a frame, and dropped on its own: the stream goes on.
        live.handle(ServerMsg::AudioFrame(vec![0xFF, 0xF8, 0, 0]), &mut writer).await.unwrap();
        assert_eq!(live.shared.audio.lock().unwrap().waiting(), 3 * 960);
        assert_eq!(live.shared.status.lock().unwrap().sound, 3);

        let (mut left, mut right) = (vec![0.0; 3], vec![0.0; 3]);
        live.shared.audio.lock().unwrap().read(&mut left, &mut right);
        assert_eq!(left, vec![0.0, 1.0 / 32768.0, 2.0 / 32768.0]);
        assert_eq!(right, vec![0.0, -1.0 / 32768.0, -2.0 / 32768.0]);

        live.handle(ServerMsg::AudioEnd, &mut writer).await.unwrap();
        live.handle(frame(3), &mut writer).await.unwrap();
        assert_eq!(live.shared.status.lock().unwrap().sound, 3, "and one after the end is dropped");
        assert_eq!(sent(&mut writer), vec![], "none of it is answered");
    }

    /// Not asked for, the extension's messages are dropped rather than the
    /// session, and none of them is answered.
    #[tokio::test]
    async fn sound_nobody_asked_for_is_not_turned_on() {
        let mut live = live();
        let mut writer = writer();
        live.handle(audio_announcement(), &mut writer).await.unwrap();
        live.handle(ServerMsg::AudioBegin, &mut writer).await.unwrap();
        live.handle(ServerMsg::AudioFrame(vec![0xFF, 0xF8, 0, 0]), &mut writer).await.unwrap();
        live.handle(ServerMsg::AudioEnd, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);
        assert!(!live.shared.status.lock().unwrap().audio);
    }

    /// The server's side of the clipboard, as the daemon frames it.
    fn from_server(body: &[u8]) -> ServerMsg {
        let wire = wlshare_rfb::msg::server_extended_cut_text(body);
        client::parse(&wire).expect("a message").expect("a whole message").0
    }

    /// What the session sent, as the clipboard messages they carry.
    fn clipboard_sent(writer: &mut Writer<Vec<u8>>) -> Vec<ClipboardMessage> {
        sent(writer)
            .into_iter()
            .map(|message| match message {
                ClientMsg::ExtendedCutText(body) => clipboard::parse(&body).expect("a clipboard message"),
                other => panic!("{other:?} where a clipboard message was expected"),
            })
            .collect()
    }

    fn server_caps() -> ServerMsg {
        from_server(&clipboard::caps(&Caps::WLSHARE))
    }

    #[tokio::test]
    async fn a_latin1_cut_text_is_not_answered() {
        let mut live = live();
        let mut writer = writer();
        let latin1 = [3, 0, 0, 0, 0, 0, 0, 2, 0xE9, b'a'];
        let (message, _) = client::parse(&latin1).unwrap().unwrap();
        live.handle(message, &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);
        assert_eq!(live.shared.clipboard.lock().unwrap().0, 0);
    }

    /// Nothing about the window's clipboard goes out before the server has said
    /// it takes one, and the text itself only when the desktop asks.
    #[tokio::test]
    async fn the_window_clipboard_is_notified_once_the_server_takes_it_and_sent_when_asked() {
        let mut live = live();
        let mut writer = writer();
        live.local_clipboard = Some("画面\nnaïve ☕".to_owned());
        live.announce_clipboard(&mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![], "no caps yet, so nothing is said");

        live.handle(server_caps(), &mut writer).await.unwrap();
        assert_eq!(
            clipboard_sent(&mut writer),
            vec![ClipboardMessage::Caps(Caps::WLSHARE), ClipboardMessage::Notify { formats: clipboard::FORMAT_TEXT }],
            "caps answered, then the clipboard the window gave while there was none"
        );

        live.handle(from_server(&clipboard::request()), &mut writer).await.unwrap();
        assert_eq!(clipboard_sent(&mut writer), vec![ClipboardMessage::Provide { text: Some("画面\nnaïve ☕".to_owned()) }]);

        live.handle(from_server(&clipboard::peek()), &mut writer).await.unwrap();
        assert_eq!(clipboard_sent(&mut writer), vec![ClipboardMessage::Notify { formats: clipboard::FORMAT_TEXT }]);

        // Caps again, as every SetEncodings brings: answered, and the clipboard
        // is not notified a second time for it.
        live.handle(server_caps(), &mut writer).await.unwrap();
        assert_eq!(clipboard_sent(&mut writer), vec![ClipboardMessage::Caps(Caps::WLSHARE)]);
    }

    #[tokio::test]
    async fn the_desktops_clipboard_is_asked_for_when_notified_and_handed_to_the_window() {
        let mut live = live();
        let mut writer = writer();
        live.handle(from_server(&clipboard::notify(true)), &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![], "a notify before caps is not answered");

        live.handle(server_caps(), &mut writer).await.unwrap();
        sent(&mut writer);
        live.handle(from_server(&clipboard::notify(true)), &mut writer).await.unwrap();
        assert_eq!(clipboard_sent(&mut writer), vec![ClipboardMessage::Request { formats: clipboard::FORMAT_TEXT }]);

        // A desktop clipboard emptied, or holding no text: nothing to ask for,
        // and the window's is left alone.
        live.handle(from_server(&clipboard::notify(false)), &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);

        live.handle(from_server(&clipboard::provide("one\ntwo 🚀").unwrap()), &mut writer).await.unwrap();
        assert_eq!(*live.shared.clipboard.lock().unwrap(), (1, Some("one\ntwo 🚀".to_owned())));

        // A request with nothing given by the window is not answered with
        // anything invented.
        live.handle(from_server(&clipboard::request()), &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);
    }

    #[tokio::test]
    async fn a_clipboard_message_that_cannot_be_read_is_dropped_and_not_the_session() {
        let mut live = live();
        let mut writer = writer();
        live.handle(server_caps(), &mut writer).await.unwrap();
        sent(&mut writer);
        live.handle(from_server(&[0x10, 0, 0, 0x01, 1, 2, 3]), &mut writer).await.unwrap();
        live.handle(from_server(&[0x08]), &mut writer).await.unwrap();
        assert_eq!(sent(&mut writer), vec![]);
        assert_eq!(live.shared.clipboard.lock().unwrap().0, 0);
    }

    #[test]
    fn a_pointer_lands_where_it_was_aimed_whatever_the_desktop_is_doing() {
        let live = live();
        // Matched, which is the steady state: the position is itself.
        live.shared.framebuffer.lock().unwrap().resize(800, 600).unwrap();
        assert_eq!(live.to_framebuffer(0, 0), (0, 0));
        assert_eq!(live.to_framebuffer(400, 300), (400, 300));
        assert_eq!(live.to_framebuffer(799, 599), (799, 599));

        // Mid-resize, the desktop still half the window: the fractions hold and
        // nothing lands outside the framebuffer.
        live.shared.framebuffer.lock().unwrap().resize(400, 300).unwrap();
        assert_eq!(live.to_framebuffer(400, 300), (200, 150));
        assert_eq!(live.to_framebuffer(799, 599), (399, 299));
        assert_eq!(live.to_framebuffer(65535, 65535), (399, 299));

        // No desktop at all, which is every event before ServerInit.
        live.shared.framebuffer.lock().unwrap().resize(0, 0).unwrap();
        assert_eq!(live.to_framebuffer(10, 10), (0, 0));
    }

    #[test]
    fn a_rectangle_outside_the_framebuffer_ends_the_session_rather_than_the_process() {
        let mut live = live();
        live.shared.framebuffer.lock().unwrap().resize(64, 64).unwrap();
        let rect = client::Rect { x: 60, y: 0, width: 8, height: 8, body: RectBody::Raw(vec![0; 8 * 8 * 4]) };
        assert!(live.apply(rect).is_err());

        let rect = client::Rect { x: 0, y: 0, width: 8, height: 8, body: RectBody::Zrle(vec![0; 4]) };
        assert!(live.apply(rect).is_err(), "a ZRLE payload that is not one is an error, not a panic");
    }

    #[test]
    fn a_cursor_whose_body_is_the_wrong_size_is_refused_before_it_can_panic() {
        // CursorImage asserts on a mismatch, so the check has to be here.
        let mut live = live();
        let rect = client::Rect { x: 0, y: 0, width: 4, height: 4, body: RectBody::AlphaCursor(vec![0; 15]) };
        assert!(live.apply(rect).is_err());
        let rect = client::Rect { x: 0, y: 0, width: 4, height: 4, body: RectBody::Cursor { pixels: vec![0; 64], mask: vec![0; 3] } };
        assert!(live.apply(rect).is_err());
    }

    #[test]
    fn an_empty_cursor_rectangle_is_a_pointer_that_is_not_there() {
        let mut live = live();
        let rect = client::Rect { x: 0, y: 0, width: 0, height: 0, body: RectBody::AlphaCursor(Vec::new()) };
        assert!(matches!(live.apply(rect), Ok(Applied::Nothing)));
        let (generation, image) = &*live.shared.cursor.lock().unwrap();
        assert_eq!(*generation, 1);
        assert!(image.is_none());
    }

    #[test]
    fn a_refused_resize_leaves_the_framebuffer_alone() {
        let mut live = live();
        live.shared.framebuffer.lock().unwrap().resize(64, 64).unwrap();
        // Status 1 in the rectangle's y: the request failed.
        let rect = client::Rect { x: 1, y: 1, width: 800, height: 600, body: RectBody::ExtendedDesktopSize { screens: Vec::new() } };
        assert!(matches!(live.apply(rect), Ok(Applied::Nothing)));
        assert_eq!(live.shared.framebuffer.lock().unwrap().width(), 64);

        let rect = client::Rect { x: 1, y: 0, width: 800, height: 600, body: RectBody::ExtendedDesktopSize { screens: Vec::new() } };
        assert!(matches!(live.apply(rect), Ok(Applied::Resized)));
        assert_eq!(live.shared.framebuffer.lock().unwrap().width(), 800);
    }

    #[test]
    fn a_surface_the_window_is_still_being_born_with_is_not_asked_for() {
        assert!(!Surface { width: 0, height: 600, scale: 2.0 }.is_usable());
        assert!(!Surface { width: 800, height: 0, scale: 2.0 }.is_usable());
        assert!(!Surface { width: 800, height: 600, scale: 0.0 }.is_usable());
        // The density extension's own bounds.
        assert!(!Surface { width: 800, height: 600, scale: 9.0 }.is_usable());
        assert!(Surface { width: 800, height: 600, scale: 1.0 }.is_usable());
        assert!(Surface { width: 800, height: 600, scale: 2.0 }.is_usable());
    }
}
