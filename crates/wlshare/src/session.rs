//! One client connection: the handshake, then the message loop.
//!
//! A session is one tokio task that reads client messages, watches the
//! framebuffer, and listens for events, and does one thing at a time so the
//! bytes it writes are always whole messages in the order they were decided.
//!
//! ## One at a time
//!
//! The desktop is one client's. Finishing the handshake takes it, and the
//! session that held it ends — it watches the compositor's `active` client
//! rather than an [`Event`](crate::shared::Event), so a session too far behind
//! to read the broadcast cannot go on holding a desktop it no longer has. The
//! watch races the message loop as a whole rather than being looked at between
//! passes of it, so a client that has stopped reading its socket is cut off in
//! the middle of the write it is blocking on. Client ids only ever go up, and
//! that is what makes the test a comparison rather than an acknowledgement: an
//! `active` above this session's own is a connection that joined after it,
//! whether or not this one ever saw itself there. Nothing a superseded session
//! sent in the meantime is acted on: the compositor hears input, resize,
//! density, output selection and clipboard from the active client alone.
//!
//! ## Sending pixels
//!
//! A client gets a FramebufferUpdate when it has asked for one — with a
//! `FramebufferUpdateRequest`, or once for all by enabling continuous updates —
//! and there is something to send: damage since the generation it last saw, or
//! everything after a non-incremental request or a resize. With Fence
//! negotiated, one update is in flight at a time: each ends with a fence the
//! client echoes, and the next waits for the echo, so a slow link is never
//! flooded and the compositor's frames coalesce in the framebuffer meanwhile.
//! Without Fence, TCP's own backpressure paces it.
//!
//! A client that lists the VP9 encoding gets every update as one rectangle
//! over the whole framebuffer, the next frame of one VP9 stream
//! ([`wlshare_rfb::vp9`]). The encoder is made again at every new size, whose
//! first frame is a keyframe, and a non-incremental request or the encoding's
//! being listed anew asks for one. The encode runs on this task's worker, told
//! it is blocking, and the fence keeps it to one frame in flight like any other
//! update. Its quality starts at the configured one and follows the link
//! ([`crate::quality`]): each frame's fence, answered once the client has the
//! frame — wlshare's own client answers once its window has drawn it, so the
//! walk reads the whole path to the screen — is how long that frame took, and
//! a client without Fence is measured by how long writing the frame blocked. The dial moves on the
//! running encoder, so a move costs no keyframe. The walk only runs when a
//! frame goes out, and a frame only goes out when something changed, so a
//! desktop that stops right after the link coarsened it would keep that
//! picture: once a frame below the configured quality has been delivered and
//! nothing has changed for [`SETTLE_IDLE`], the dial is taken back to the
//! configured quality and the unchanged picture is sent again as one inter
//! frame, which sharpens every block without a keyframe. A list that drops
//! VP9 is answered with the whole framebuffer in ZRLE, since the picture the
//! client has is a lossy one.
//!
//! A size change goes out first, as its own update, and the whole framebuffer
//! follows in the next. A client that negotiated neither ExtendedDesktopSize nor
//! DesktopSize cannot be told and is disconnected at its next update instead of
//! being sent pixels at a size it does not know.
//!
//! The compositor pointer is never in those pixels. Every client must list the
//! standard Cursor pseudo-encoding before asking for an update, and is sent the
//! compositor's cursor image in its own update whenever it changes — with its
//! alpha when the client also listed Cursor With Alpha, cut to a mask otherwise,
//! and as an empty rectangle while there is no pointer on the shared output. The
//! client positions it without framebuffer latency.
//!
//! ## Sending sound
//!
//! A client that listed the audio pseudo-encoding is told so by an empty
//! rectangle in an update of its own, which waits for a request like any other
//! announcement. Its enable opens a PipeWire capture in the format it set
//! ([`crate::audio`]), answered with *begin*; its disable, or its leaving,
//! closes the capture, answered with *end*. The capture's FLAC frames ride the
//! same connection as the pixels, and go first: every pass of the loop drains
//! what the capture has queued before it considers a framebuffer update, so
//! sound waits for at most the update already being written, never for the
//! next one.
//!
//! ## Lending a camera
//!
//! A client that listed the camera pseudo-encoding is answered with the
//! extension's announcement at once, as the density and outputs extensions are.
//! Its plug makes a PipeWire video source ([`crate::camera`]), its unplug or its
//! leaving removes it, and a second plug replaces the first. The desktop's
//! decisions about that source — an application opened it, the last one closed
//! it, a keyframe is needed — are written to the client as they happen, and its
//! samples go to the camera's thread without waiting on anything the session
//! writes.
//!
//! ## Lending a microphone
//!
//! The camera's twin. A client that listed the microphone pseudo-encoding is
//! answered with the extension's announcement at once; its plug makes a PipeWire
//! audio source ([`crate::microphone`]), its unplug or its leaving removes it. An
//! application starting to record is written to the client as a start naming the
//! format the source takes, the last one stopping as a stop, and the client's
//! samples go to the source's buffer without waiting on anything.
//!
//! ## Sharing the clipboard
//!
//! Extended Clipboard is the only clipboard spoken ([`wlshare_rfb::clipboard`]);
//! a latin-1 cut text is dropped, and a client that did not list the extension
//! has no clipboard. Every SetEncodings that lists it is answered with this
//! server's caps, which take text, every action, and no unsolicited text. A
//! change on the desktop is notified to the client and provided when the client
//! asks — or provided at once, to a client whose caps take no notify but take
//! the text unasked. A client's notify is answered with a request, and what it
//! provides becomes the desktop's clipboard. The text a request is answered
//! with is [`Shared::clipboard`] as it is then, whoever put it there.
//!
//! ## The transport
//!
//! The handshake decides what the socket carries afterwards: RFB bytes as they
//! are, or — after RSA-AES — RFB bytes inside AES-EAX frames. The session sees
//! a [`Reader`] and a [`Writer`] either way; the writer takes whole messages,
//! which is what a frame is cut from.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use log::{debug, info, warn};
use wlshare_rfb::audio::{AudioFormat, audio_begin, audio_end, audio_rect};
use wlshare_rfb::camera::{CameraFormat, camera_available, camera_keyframe, camera_start, camera_stop};
use wlshare_rfb::clipboard::{self, Caps, Message as ClipboardMessage};
use wlshare_rfb::cursor::{alpha_cursor_rect, cursor_rect};
use wlshare_rfb::density::{from_fixed, output_scale};
use wlshare_rfb::microphone::{microphone_available, microphone_start, microphone_stop};
use wlshare_rfb::msg::{self, ClientMsg, Screen};
use wlshare_rfb::outputs::output_list;
use wlshare_rfb::pixel::PixelFormat;
use wlshare_rfb::rsa_aes::{self, FrameReader, Sealer, ServerKey};
use wlshare_rfb::vp9::Vp9Encoder;
use wlshare_rfb::zrle::{ZrleEncoder, encode_raw_rect};
use wlshare_rfb::{
    ENCODING_AUDIO, ENCODING_CAMERA, ENCODING_CONTINUOUS_UPDATES, ENCODING_DENSITY, ENCODING_DESKTOP_SIZE, ENCODING_EXTENDED_DESKTOP_SIZE, ENCODING_FENCE, ENCODING_MICROPHONE,
    ENCODING_OUTPUTS, ENCODING_CURSOR, ENCODING_CURSOR_WITH_ALPHA, ENCODING_RAW, ENCODING_VP9, ENCODING_ZRLE,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, ReadBuf};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, watch};

use crate::audio::{Capture, Lease};
use crate::auth::Login;
use crate::camera::{Camera, Signal as CameraSignal};
use crate::framebuffer::{Rect, ResizeOrigin};
use crate::microphone::{Microphone, Signal as MicrophoneSignal};
use crate::quality::{Pace, QualityWalk};
use crate::shared::{ClientId, Command, Event, Shared};

/// What the server offers at the security step, and what it checks the client
/// against: RSA-AES with a login, or nothing at all, in which case anyone who
/// reaches the port is in. Classic VncAuth is deliberately not offered — it
/// names nobody and leaves the session in the clear.
#[derive(Default)]
pub struct Security {
    /// RSA-AES at both widths, the credentials checked by [`Login`].
    pub rsa_aes: Option<RsaAes>,
}

/// RSA-AES's half of [`Security`]: the server key, and what the credentials it
/// carries are checked against.
pub struct RsaAes {
    pub key: Arc<ServerKey>,
    pub login: Login,
}

impl Security {
    /// The security types to list: RSA-AES at its wider width first, or `None`
    /// alone when nothing is configured.
    pub fn offered(&self) -> Vec<u8> {
        match self.rsa_aes {
            Some(_) => vec![rsa_aes::SECURITY_RSA_AES_256, rsa_aes::SECURITY_RSA_AES_128],
            None => vec![msg::SECURITY_NONE],
        }
    }
}

pub struct SessionConfig {
    pub security: Security,
    pub name: String,
    pub resize: bool,
    /// The VP9 encoding's finest quality, 1–100, where a session starts.
    pub vp9_quality: u8,
    /// The interval the capture is paced to, one over `max_fps`: what a
    /// slowed link's frames are spaced from ([`QualityWalk::interval`]).
    pub capture: Duration,
    /// Whether the audio extension is announced and served.
    pub audio: bool,
    /// Whether the camera extension is announced and served.
    pub camera: bool,
    /// Whether the microphone extension is announced and served.
    pub microphone: bool,
    /// How long a connection has to finish the handshake before it is dropped.
    pub handshake_timeout: Duration,
}

/// The most rectangles one update carries before they collapse into one.
const MAX_RECTS: usize = 32;

/// The socket's read side, opened frame by frame after RSA-AES.
pub enum Reader {
    Plain(OwnedReadHalf),
    Sealed(FrameReader<OwnedReadHalf>),
}

impl AsyncRead for Reader {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(r) => Pin::new(r).poll_read(cx, buf),
            Self::Sealed(r) => Pin::new(r).poll_read(cx, buf),
        }
    }
}

/// The socket's write side: whole messages, each sealed into frames after
/// RSA-AES.
pub struct Writer {
    inner: OwnedWriteHalf,
    sealer: Option<Sealer>,
}

impl Writer {
    async fn send(&mut self, message: &[u8]) -> std::io::Result<()> {
        match &mut self.sealer {
            Some(sealer) => self.inner.write_all(&sealer.frame(message)).await,
            None => self.inner.write_all(message).await,
        }
    }
}

pub async fn run(id: ClientId, socket: TcpStream, shared: Arc<Shared>, config: Arc<SessionConfig>) -> anyhow::Result<()> {
    socket.set_nodelay(true)?;
    let peer = socket.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let (reader, writer) = socket.into_split();
    let (reader, mut writer) = tokio::time::timeout(config.handshake_timeout, handshake(reader, writer, &config, &peer))
        .await
        .map_err(|_| anyhow::anyhow!("the handshake took over {:?}", config.handshake_timeout))??;
    let (width, height) = {
        let fb = shared.framebuffer.lock().unwrap();
        (fb.width, fb.height)
    };
    writer.send(&msg::server_init(width, height, &PixelFormat::NATIVE, &config.name)).await?;
    info!("client {}: authenticated; desktop {width}x{height}", id.0);
    let mut active = shared.active.subscribe();
    shared.command(Command::ClientJoined(id));

    let events = shared.events.subscribe();
    let frames = shared.frame_tx.subscribe();
    let walk = QualityWalk::new(config.vp9_quality, config.capture);
    let mut session = Session {
        id,
        shared,
        config,
        format: PixelFormat::NATIVE,
        zrle: None,
        use_zrle: false,
        vp9: None,
        use_vp9: false,
        keyframe_owed: false,
        walk,
        vp9_in_flight: None,
        frame_after: None,
        coarse_since: None,
        settle_owed: false,
        continuous_supported: false,
        fence_supported: false,
        eds_supported: false,
        desktop_size_supported: false,
        cursor_supported: false,
        alpha_cursor: false,
        announce_cursor: false,
        density: false,
        outputs: false,
        audio_supported: false,
        announce_audio: false,
        audio_format: AudioFormat::DEFAULT,
        audio: None,
        camera_supported: false,
        camera: None,
        microphone_supported: false,
        microphone: None,
        clipboard: None,
        continuous: false,
        pending: None,
        seen: 0,
        known_size: (width, height),
        fence_outstanding: false,
        fence_seq: 0,
        announce_eds: false,
        events,
        frames,
        scratch: Vec::new(),
        out: Vec::new(),
    };
    let result = tokio::select! {
        result = session.pump(reader, writer) => result,
        taken = superseded(&mut active, id) => Err(anyhow::anyhow!("client {} took the desktop", taken?)),
    };
    // However the session ended — the client left, an error, or a takeover
    // cancelling the loop mid-write — the capture it may still hold is closed
    // here rather than by dropping `session`: closing joins PipeWire's thread,
    // and that blocking wait does not belong on a runtime worker. The session
    // is over either way, so the result stands whatever the join does.
    if let Some(capture) = session.audio.take()
        && let Err(e) = tokio::task::spawn_blocking(move || drop(capture)).await
    {
        warn!("client {}: the audio thread did not stop: {e}", id.0);
    }
    // The camera the same way: unplugging joins its PipeWire thread.
    if let Some(camera) = session.camera.take()
        && let Err(e) = tokio::task::spawn_blocking(move || drop(camera)).await
    {
        warn!("client {}: the camera thread did not stop: {e}", id.0);
    }
    // And the microphone.
    if let Some(microphone) = session.microphone.take()
        && let Err(e) = tokio::task::spawn_blocking(move || drop(microphone)).await
    {
        warn!("client {}: the microphone thread did not stop: {e}", id.0);
    }
    result
}

/// Resolves once a later connection has taken the desktop. Ids only go up, so
/// an `active` above this session's is a client that joined after it; anything
/// below it — 0 included — is the compositor not having read this session's own
/// join yet, which is what the wait is for.
async fn superseded(active: &mut watch::Receiver<u64>, id: ClientId) -> anyhow::Result<u64> {
    loop {
        let current = *active.borrow_and_update();
        if current > id.0 {
            return Ok(current);
        }
        active.changed().await.context("the compositor thread is gone")?;
    }
}

/// RFB 3.8 version, security and ClientInit. Returns the transport the rest of
/// the session runs over. The ClientInit shared flag is read and dropped: the
/// desktop is one client's either way.
async fn handshake(mut reader: OwnedReadHalf, mut writer: OwnedWriteHalf, config: &SessionConfig, peer: &str) -> anyhow::Result<(Reader, Writer)> {
    writer.write_all(msg::PROTOCOL_VERSION).await?;
    let mut version = [0u8; 12];
    reader.read_exact(&mut version).await.context("reading the client's version")?;
    if &version != msg::PROTOCOL_VERSION {
        let reason = "this server speaks RFB 3.8 only";
        writer.write_all(&msg::security_refusal(reason)).await?;
        anyhow::bail!("client version {:?}; {reason}", String::from_utf8_lossy(&version).trim_end());
    }
    let offered = config.security.offered();
    writer.write_all(&msg::security_types(&offered)).await?;
    let chosen = reader.read_u8().await.context("reading the security type")?;
    if !offered.contains(&chosen) {
        writer.write_all(&msg::security_failed("unsupported security type")).await?;
        anyhow::bail!("client chose security type {chosen}, not one of {offered:?}");
    }
    // `chosen` is one of `offered`, so the branch it names is configured.
    let (mut reader, mut writer) = match chosen {
        msg::SECURITY_NONE => (Reader::Plain(reader), Writer { inner: writer, sealer: None }),
        _ => {
            let RsaAes { key, login } = config.security.rsa_aes.as_ref().expect("RSA-AES was offered");
            let strength = rsa_aes::Strength::of(chosen).expect("one of the two offered");
            let (credentials, session) = rsa_aes::authenticate(&mut reader, &mut writer, strength, key, login.subtype()).await.context("RSA-AES key exchange")?;
            // Everything from here on is inside the frames, the refusal included.
            let mut writer = Writer { inner: writer, sealer: Some(session.sealer) };
            let reader = Reader::Sealed(FrameReader::new(reader, session.opener));
            let (login, peer) = (login.clone(), peer.to_owned());
            let checked = tokio::task::spawn_blocking(move || login.check(&credentials, &peer)).await.context("the login check did not finish")?;
            if let Err(refused) = checked {
                tokio::time::sleep(Duration::from_secs(1)).await;
                writer.send(&msg::security_failed("authentication failed")).await?;
                anyhow::bail!("login refused: {refused}");
            }
            (reader, writer)
        }
    };
    writer.send(&msg::security_ok()).await?;
    let _shared = reader.read_u8().await.context("reading ClientInit")?;
    Ok((reader, writer))
}

struct Session {
    id: ClientId,
    shared: Arc<Shared>,
    config: Arc<SessionConfig>,
    format: PixelFormat,
    zrle: Option<ZrleEncoder>,
    use_zrle: bool,
    /// The VP9 stream's encoder, at the framebuffer's size once a frame has
    /// been sent.
    vp9: Option<Vp9Encoder>,
    /// The client listed the VP9 encoding, which it then gets instead of ZRLE.
    use_vp9: bool,
    /// The next VP9 frame must be a keyframe.
    keyframe_owed: bool,
    /// The VP9 quality this client's link will bear.
    walk: QualityWalk,
    /// The VP9 frame whose fence is outstanding: when it was written, and
    /// whether its delivery is a verdict about the link — a delta frame at the
    /// walk's quality, so neither a keyframe nor the settle's frame.
    vp9_in_flight: Option<(Instant, bool)>,
    /// The earliest the next VP9 frame may go out, on a link the walk has
    /// slowed ([`QualityWalk::interval`]); `None` where the capture paces.
    frame_after: Option<Instant>,
    /// The last VP9 frame went out below the configured quality, and the
    /// client has had it since then: the desktop is settled at the configured
    /// quality once it has stayed quiet [`SETTLE_IDLE`] from there. `None`
    /// once a frame at the configured quality has gone out.
    coarse_since: Option<Instant>,
    /// The unchanged picture is owed as a frame at the configured quality.
    settle_owed: bool,
    continuous_supported: bool,
    fence_supported: bool,
    eds_supported: bool,
    desktop_size_supported: bool,
    /// The client listed the required standard Cursor pseudo-encoding.
    cursor_supported: bool,
    /// The client listed Cursor With Alpha, which the cursor goes out as instead.
    alpha_cursor: bool,
    /// The cursor image is owed: after an accepted SetEncodings, and whenever it
    /// changes.
    announce_cursor: bool,
    density: bool,
    /// The client listed the outputs pseudo-encoding: it is sent the list of
    /// outputs and may ask for another one.
    outputs: bool,
    /// The client listed the audio pseudo-encoding and the configuration
    /// allows it.
    audio_supported: bool,
    /// The audio announcement is owed: sent as its own update, once.
    announce_audio: bool,
    /// The sample format the client set, or the extension's default.
    audio_format: AudioFormat,
    /// The capture, while the client has audio enabled.
    audio: Option<Capture>,
    /// The client listed the camera pseudo-encoding and the configuration
    /// allows it.
    camera_supported: bool,
    /// The camera, while the client has one plugged.
    camera: Option<Camera>,
    /// The client listed the microphone pseudo-encoding and the configuration
    /// allows it.
    microphone_supported: bool,
    /// The microphone, while the client has one plugged.
    microphone: Option<Microphone>,
    /// What the client takes of the clipboard, once it listed Extended
    /// Clipboard: its caps, or the extension's default until it sends them.
    clipboard: Option<Caps>,
    /// The client enabled continuous updates.
    continuous: bool,
    /// An update request not yet answered: `true` for incremental.
    pending: Option<bool>,
    /// The framebuffer generation the client has.
    seen: u64,
    known_size: (u16, u16),
    fence_outstanding: bool,
    fence_seq: u32,
    announce_eds: bool,
    events: broadcast::Receiver<Event>,
    frames: watch::Receiver<u64>,
    /// Pixels copied out of the framebuffer, rect by rect, for encoding.
    scratch: Vec<u8>,
    out: Vec<u8>,
}

/// How long a desktop sent below the configured VP9 quality must stay quiet,
/// with the client holding the last frame, before it is settled at that
/// quality. Long enough that a pause in motion is not chased with a redundant
/// frame, short enough that the picture sharpens while the eye is still on it.
/// remotex's `CLEANUP_IDLE`.
const SETTLE_IDLE: Duration = Duration::from_millis(500);

/// A damaged rectangle and its pixels, taken out of the framebuffer.
struct Piece {
    rect: Rect,
    offset: usize,
}

impl Session {
    async fn pump(&mut self, mut reader: Reader, mut writer: Writer) -> anyhow::Result<()> {
        let mut inbuf: Vec<u8> = Vec::with_capacity(4096);
        loop {
            self.flush_audio(&mut writer).await?;
            self.maybe_update(&mut writer).await?;
            let settle_at = self.settle_at();
            let frame_at = self.frame_at();
            tokio::select! {
                read = reader.read_buf(&mut inbuf) => {
                    let n = read.context("reading from the client")?;
                    if n == 0 {
                        return Ok(());
                    }
                    let mut consumed = 0;
                    while let Some((message, used)) = msg::parse(&inbuf[consumed..])? {
                        consumed += used;
                        self.handle(message, &mut writer).await?;
                    }
                    inbuf.drain(..consumed);
                }
                changed = self.frames.changed() => {
                    if changed.is_err() {
                        anyhow::bail!("the framebuffer is gone");
                    }
                }
                event = self.events.recv() => match event {
                    Ok(event) => self.handle_event(event, &mut writer).await?,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("client {}: missed {n} events", self.id.0);
                        // A cursor change may have been among them, and the
                        // image is read when it is sent, so sending it again is
                        // never wrong.
                        if self.cursor_supported {
                            self.announce_cursor = true;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => anyhow::bail!("the compositor thread is gone"),
                },
                () = until(settle_at) => self.settle()?,
                // The slowed frame's turn: the top of the loop sends it.
                () = until(frame_at) => {}
                // Woken to drain the capture at the top of the loop.
                () = audio_ready(&self.audio) => {}
                signal = camera_signal(&mut self.camera) => self.send_camera_signal(signal, &mut writer).await?,
                signal = microphone_signal(&mut self.microphone) => match signal {
                    MicrophoneSignal::Start => writer.send(&microphone_start(crate::microphone::FORMAT)).await?,
                    MicrophoneSignal::Stop => writer.send(&microphone_stop()).await?,
                },
            }
        }
    }

    /// Plug the client's microphone, replacing one already plugged. A microphone
    /// that cannot be made is logged and leaves the client without one, not
    /// without a desktop; the client is then never asked to start it.
    async fn plug_microphone(&mut self) -> anyhow::Result<()> {
        self.unplug_microphone().await?;
        let id = self.id;
        match tokio::task::spawn_blocking(move || Microphone::plug(id)).await.context("the microphone thread did not start")? {
            Ok(microphone) => self.microphone = Some(microphone),
            Err(e) => warn!("client {}: the microphone could not be plugged: {e:#}", self.id.0),
        }
        Ok(())
    }

    /// Unplug the client's microphone, if one is plugged.
    async fn unplug_microphone(&mut self) -> anyhow::Result<()> {
        if let Some(microphone) = self.microphone.take() {
            // Unplugging joins PipeWire's thread, which is a blocking wait.
            tokio::task::spawn_blocking(move || drop(microphone)).await.context("the microphone thread did not stop")?;
        }
        Ok(())
    }

    /// Tell the client what the desktop decided about its camera. A camera that
    /// failed is told to stop, so the client encodes nothing more for it, and
    /// unplugged.
    async fn send_camera_signal(&mut self, signal: CameraSignal, writer: &mut Writer) -> anyhow::Result<()> {
        let Some(camera) = &self.camera else { return Ok(()) };
        match signal {
            CameraSignal::Start => writer.send(&camera_start(camera.format)).await?,
            CameraSignal::Stop => writer.send(&camera_stop()).await?,
            CameraSignal::Keyframe => writer.send(&camera_keyframe()).await?,
            CameraSignal::Failed => {
                writer.send(&camera_stop()).await?;
                self.unplug_camera().await?;
            }
        }
        Ok(())
    }

    /// Plug the client's camera, replacing one already plugged. A camera that
    /// cannot be made is logged and leaves the client without one, not without
    /// a desktop; the client is then never asked to start it.
    async fn plug_camera(&mut self, format: CameraFormat) -> anyhow::Result<()> {
        self.unplug_camera().await?;
        let id = self.id;
        match tokio::task::spawn_blocking(move || Camera::plug(id, format)).await.context("the camera thread did not start")? {
            Ok(camera) => self.camera = Some(camera),
            Err(e) => warn!("client {}: the camera could not be plugged: {e:#}", self.id.0),
        }
        Ok(())
    }

    /// Unplug the client's camera, if one is plugged.
    async fn unplug_camera(&mut self) -> anyhow::Result<()> {
        if let Some(camera) = self.camera.take() {
            // Unplugging joins PipeWire's thread, which is a blocking wait.
            tokio::task::spawn_blocking(move || drop(camera)).await.context("the camera thread did not stop")?;
        }
        Ok(())
    }

    /// Send every FLAC frame the capture has queued.
    async fn flush_audio(&mut self, writer: &mut Writer) -> anyhow::Result<()> {
        while let Some(frame) = self.audio.as_ref().and_then(Capture::take) {
            writer.send(&frame).await.context("writing audio")?;
        }
        Ok(())
    }

    /// Open the capture in the client's format and say so. A capture that
    /// cannot be opened is logged and leaves the client without sound, not
    /// without a desktop.
    async fn start_audio(&mut self, writer: &mut Writer) -> anyhow::Result<()> {
        let (id, format) = (self.id, self.audio_format);
        match tokio::task::spawn_blocking(move || Capture::start(id, format)).await.context("the audio thread did not start")? {
            Ok(capture) => {
                self.audio = Some(capture);
                writer.send(&audio_begin()).await?;
            }
            Err(e) => warn!("client {}: audio could not be captured: {e:#}", self.id.0),
        }
        Ok(())
    }

    /// Close the capture, if one is open, and say so.
    async fn stop_audio(&mut self, writer: &mut Writer) -> anyhow::Result<()> {
        if let Some(capture) = self.audio.take() {
            // Closing joins PipeWire's thread, which is a blocking wait.
            tokio::task::spawn_blocking(move || drop(capture)).await.context("the audio thread did not stop")?;
            writer.send(&audio_end()).await?;
            info!("client {}: audio stopped", self.id.0);
        }
        Ok(())
    }

    async fn handle(&mut self, message: ClientMsg, writer: &mut Writer) -> anyhow::Result<()> {
        match message {
            ClientMsg::SetPixelFormat(format) => {
                format.check().map_err(|e| anyhow::anyhow!("the client's pixel format cannot be produced: {e}"))?;
                debug!("client {}: pixel format {format:?}", self.id.0);
                self.format = format;
            }
            ClientMsg::SetEncodings(encodings) => {
                let has = |e: i32| encodings.contains(&e);
                anyhow::ensure!(
                    has(ENCODING_CURSOR),
                    "client {} did not advertise the required RFB Cursor pseudo-encoding",
                    self.id.0
                );
                self.cursor_supported = true;
                self.alpha_cursor = has(ENCODING_CURSOR_WITH_ALPHA);
                self.announce_cursor = true;
                self.use_zrle = has(ENCODING_ZRLE);
                if self.use_zrle && self.zrle.is_none() {
                    self.zrle = Some(ZrleEncoder::default());
                }
                let vp9 = has(ENCODING_VP9);
                if vp9 && !self.use_vp9 {
                    info!("client {}: asked for VP9 at quality {}", self.id.0, self.walk.quality());
                    // A decoder that has seen nothing of this stream starts at a keyframe.
                    self.keyframe_owed = true;
                } else if !vp9 && self.use_vp9 {
                    // What the client holds is VP9's lossy picture, and damage
                    // alone would leave most of it on the screen.
                    self.vp9 = None;
                    self.seen = 0;
                    self.coarse_since = None;
                    self.settle_owed = false;
                }
                self.use_vp9 = vp9;
                if !self.use_vp9 && !self.use_zrle && !has(ENCODING_RAW) {
                    warn!("client {}: lists neither VP9, ZRLE nor Raw; sending Raw", self.id.0);
                }
                let announced_continuous = !self.continuous_supported && has(ENCODING_CONTINUOUS_UPDATES);
                self.continuous_supported |= has(ENCODING_CONTINUOUS_UPDATES);
                self.fence_supported = has(ENCODING_FENCE);
                if !self.eds_supported && has(ENCODING_EXTENDED_DESKTOP_SIZE) {
                    self.announce_eds = true;
                }
                self.eds_supported = has(ENCODING_EXTENDED_DESKTOP_SIZE);
                self.desktop_size_supported = has(ENCODING_DESKTOP_SIZE);
                let density = has(ENCODING_DENSITY);
                if density && !self.density {
                    info!("client {}: asked for density reports", self.id.0);
                }
                self.density = density;
                let outputs = has(ENCODING_OUTPUTS);
                if outputs && !self.outputs {
                    info!("client {}: asked for the output list", self.id.0);
                }
                self.outputs = outputs;
                let audio = has(ENCODING_AUDIO);
                if audio && !self.config.audio && !self.audio_supported {
                    info!("client {}: asked for audio, which the configuration turns off", self.id.0);
                }
                let audio = audio && self.config.audio;
                if audio && !self.audio_supported {
                    info!("client {}: asked for audio", self.id.0);
                    self.announce_audio = true;
                }
                self.audio_supported = audio;
                if !audio {
                    // A list without the extension withdraws it: nothing more is
                    // announced, and a running stream ends.
                    self.announce_audio = false;
                    self.stop_audio(writer).await?;
                }
                let camera = has(ENCODING_CAMERA);
                if camera && !self.config.camera && !self.camera_supported {
                    info!("client {}: offers a camera, which the configuration turns off", self.id.0);
                }
                let camera = camera && self.config.camera;
                if camera && !self.camera_supported {
                    info!("client {}: offers a camera", self.id.0);
                }
                self.camera_supported = camera;
                let microphone = has(ENCODING_MICROPHONE);
                if microphone && !self.config.microphone && !self.microphone_supported {
                    info!("client {}: offers a microphone, which the configuration turns off", self.id.0);
                }
                let microphone = microphone && self.config.microphone;
                if microphone && !self.microphone_supported {
                    info!("client {}: offers a microphone", self.id.0);
                }
                self.microphone_supported = microphone;
                let listed_clipboard = has(clipboard::ENCODING);
                if listed_clipboard && self.clipboard.is_none() {
                    info!("client {}: shares the clipboard", self.id.0);
                }
                self.clipboard = match listed_clipboard {
                    true => Some(self.clipboard.unwrap_or(Caps::CLIENT_DEFAULT)),
                    false => None,
                };
                debug!(
                    "client {}: encodings {encodings:?}; cursor={} alpha_cursor={} vp9={} zrle={} continuous={} fence={} eds={} density={} outputs={} audio={} camera={} microphone={} clipboard={}",
                    self.id.0,
                    self.cursor_supported,
                    self.alpha_cursor,
                    self.use_vp9,
                    self.use_zrle,
                    self.continuous_supported,
                    self.fence_supported,
                    self.eds_supported,
                    self.density,
                    self.outputs,
                    self.audio_supported,
                    self.camera_supported,
                    self.microphone_supported,
                    self.clipboard.is_some()
                );
                if announced_continuous {
                    // The only way support is ever announced.
                    writer.send(&msg::end_of_continuous_updates()).await?;
                }
                if self.density {
                    // Every SetEncodings that lists the extension is answered.
                    self.send_geometry(writer).await?;
                }
                if self.outputs {
                    // Likewise: the answer is how support is announced.
                    self.send_outputs(writer).await?;
                }
                if self.camera_supported {
                    // And for the camera.
                    writer.send(&camera_available()).await?;
                }
                if self.microphone_supported {
                    // And for the microphone.
                    writer.send(&microphone_available()).await?;
                }
                if self.clipboard.is_some() {
                    // The extension requires it of every SetEncodings listing it.
                    writer.send(&msg::server_extended_cut_text(&clipboard::caps(&Caps::WLSHARE))).await?;
                }
            }
            ClientMsg::FramebufferUpdateRequest { incremental, .. } => {
                anyhow::ensure!(
                    self.cursor_supported,
                    "client {} requested a framebuffer before advertising the required RFB Cursor pseudo-encoding",
                    self.id.0
                );
                self.pending = Some(match self.pending {
                    Some(false) => false,
                    _ => incremental,
                });
            }
            ClientMsg::KeyEvent { down, keysym } => self.shared.command(Command::Key { client: self.id, keysym, down }),
            ClientMsg::PointerEvent { buttons, x, y } => self.shared.command(Command::Pointer { client: self.id, buttons, x, y }),
            ClientMsg::CutText(_) => debug!("client {}: latin-1 cut text, which is not spoken here; ignored", self.id.0),
            ClientMsg::ExtendedCutText(body) => self.handle_clipboard(&body, writer).await?,
            ClientMsg::EnableContinuousUpdates { enable, .. } => {
                self.continuous = enable && self.continuous_supported;
                if !enable {
                    writer.send(&msg::end_of_continuous_updates()).await?;
                }
                debug!("client {}: continuous updates {}", self.id.0, if self.continuous { "on" } else { "off" });
            }
            ClientMsg::Fence { flags, payload } => {
                if flags & msg::FENCE_REQUEST != 0 {
                    // The client's own fence: nothing is reordered here, so every
                    // flag it asks for holds, and the echo is the whole answer.
                    let echo = flags & (msg::FENCE_BLOCK_BEFORE | msg::FENCE_BLOCK_AFTER | msg::FENCE_SYNC_NEXT);
                    writer.send(&msg::fence(echo, &payload)).await?;
                } else {
                    self.fence_outstanding = false;
                    if let Some((sent, verdict)) = self.vp9_in_flight.take() {
                        let now = Instant::now();
                        let delivery = now.saturating_duration_since(sent);
                        let moved = self.walk.fenced(delivery, verdict, now);
                        if moved.is_some() {
                            debug!("client {}: the frame before took {}ms to deliver", self.id.0, delivery.as_millis());
                        }
                        self.follow_walk(moved)?;
                        // Quiet is counted from when the client had the frame,
                        // not from when it was written.
                        if self.coarse_since.is_some() {
                            self.coarse_since = Some(now);
                        }
                    }
                }
            }
            ClientMsg::SetDesktopSize { width, height, screens } => {
                if !self.eds_supported {
                    warn!("client {}: SetDesktopSize without ExtendedDesktopSize; ignored", self.id.0);
                    return Ok(());
                }
                let current = {
                    let fb = self.shared.framebuffer.lock().unwrap();
                    (fb.width, fb.height)
                };
                if !self.config.resize {
                    return self.send_eds(writer, msg::EDS_REASON_THIS_CLIENT, msg::EDS_STATUS_PROHIBITED, current).await;
                }
                if screens.len() != 1 || width == 0 || height == 0 {
                    return self.send_eds(writer, msg::EDS_REASON_THIS_CLIENT, msg::EDS_STATUS_INVALID_LAYOUT, current).await;
                }
                if (width, height) == current {
                    return self.send_eds(writer, msg::EDS_REASON_THIS_CLIENT, msg::EDS_STATUS_OK, current).await;
                }
                self.shared.command(Command::Resize { client: self.id, width, height });
            }
            ClientMsg::ClientDensity { width, height, fixed } => {
                if !self.density {
                    debug!("client {}: a density declaration without the extension; ignored", self.id.0);
                    return Ok(());
                }
                let scale = from_fixed(fixed);
                info!("client {}: declares a display density of {scale:.2} for {width}x{height} pixels", self.id.0);
                if !(0.5..=8.0).contains(&scale) {
                    warn!("client {}: density {scale:.2} is out of range; reporting the output as it is", self.id.0);
                    return self.send_geometry(writer).await;
                }
                if width == 0 || height == 0 {
                    warn!("client {}: a declaration of {width}x{height} pixels; reporting the output as it is", self.id.0);
                    return self.send_geometry(writer).await;
                }
                if !self.eds_supported {
                    // The resize could not be told to it, as a SetDesktopSize's
                    // could not.
                    warn!("client {}: a density declaration without ExtendedDesktopSize; reporting the output as it is", self.id.0);
                    return self.send_geometry(writer).await;
                }
                self.shared.command(Command::Declare { client: self.id, width, height, scale });
            }
            ClientMsg::SelectOutput { id } => {
                if !self.outputs {
                    debug!("client {}: an output selection without the extension; ignored", self.id.0);
                    return Ok(());
                }
                info!("client {}: asks for output {id}", self.id.0);
                self.shared.command(Command::SelectOutput { client: self.id, id });
            }
            ClientMsg::AudioEnable => {
                if !self.audio_supported {
                    warn!("client {}: enables audio without the extension; ignored", self.id.0);
                } else if self.audio.is_none() {
                    self.start_audio(writer).await?;
                }
            }
            ClientMsg::AudioDisable => {
                if !self.audio_supported {
                    warn!("client {}: disables audio without the extension; ignored", self.id.0);
                } else {
                    self.stop_audio(writer).await?;
                }
            }
            ClientMsg::AudioFormat(format) => {
                if !self.audio_supported {
                    warn!("client {}: sets an audio format without the extension; ignored", self.id.0);
                    return Ok(());
                }
                debug!("client {}: wants audio as {:?} x{} at {} Hz", self.id.0, format.sample, format.channels, format.frequency);
                self.audio_format = format;
                // A format set while the stream runs restarts it in the new one,
                // with the speaker held across, so the desktop keeps playing
                // into it rather than on the host between the two captures.
                if self.audio.is_some() {
                    let speaker = tokio::task::spawn_blocking(Lease::take).await.context("the speaker thread did not start")??;
                    let restarted = async {
                        self.stop_audio(writer).await?;
                        self.start_audio(writer).await
                    }
                    .await;
                    // The last hold, when the new capture did not start, joins the
                    // speaker's thread.
                    tokio::task::spawn_blocking(move || drop(speaker)).await.context("the speaker thread did not stop")?;
                    restarted?;
                }
            }
            ClientMsg::CameraPlug(format) => {
                if !self.camera_supported {
                    warn!("client {}: plugs a camera without the extension; ignored", self.id.0);
                    return Ok(());
                }
                self.plug_camera(format).await?;
            }
            ClientMsg::CameraUnplug => self.unplug_camera().await?,
            ClientMsg::CameraSample { keyframe, data } => match &mut self.camera {
                Some(camera) => camera.sample(data, keyframe),
                // Samples already on the wire when the camera went.
                None => debug!("client {}: a camera sample with no camera plugged; dropped", self.id.0),
            },
            ClientMsg::MicrophonePlug => {
                if !self.microphone_supported {
                    warn!("client {}: plugs a microphone without the extension; ignored", self.id.0);
                    return Ok(());
                }
                self.plug_microphone().await?;
            }
            ClientMsg::MicrophoneUnplug => self.unplug_microphone().await?,
            ClientMsg::MicrophoneSample(pcm) => {
                // The format is the one every start names, so a length that is not
                // whole frames of it is a client that means something else.
                let frame = crate::microphone::FORMAT.frame_bytes();
                anyhow::ensure!(
                    pcm.len().is_multiple_of(frame),
                    "client {} sent {} bytes of microphone samples, not whole {frame}-byte frames",
                    self.id.0,
                    pcm.len()
                );
                match &self.microphone {
                    Some(microphone) => microphone.sample(&pcm),
                    // Samples already on the wire when the microphone went.
                    None => debug!("client {}: microphone samples with no microphone plugged; dropped", self.id.0),
                }
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: Event, writer: &mut Writer) -> anyhow::Result<()> {
        match event {
            Event::Geometry { to } => {
                if to.is_none_or(|c| c == self.id) && self.density {
                    self.send_geometry(writer).await?;
                }
            }
            Event::ResizeRefused { client, status } => {
                if client == self.id && self.eds_supported {
                    let current = {
                        let fb = self.shared.framebuffer.lock().unwrap();
                        (fb.width, fb.height)
                    };
                    self.send_eds(writer, msg::EDS_REASON_THIS_CLIENT, status, current).await?;
                }
            }
            Event::Outputs => {
                if self.outputs {
                    self.send_outputs(writer).await?;
                }
            }
            Event::Clipboard => self.announce_clipboard(writer).await?,
            Event::Cursor => {
                // Not before SetEncodings: that is when it is owed anyway.
                if self.cursor_supported {
                    self.announce_cursor = true;
                }
            }
        }
        Ok(())
    }

    /// One of the client's Extended Clipboard messages. One that cannot be read
    /// is dropped rather than the connection: it was framed, and is consumed.
    async fn handle_clipboard(&mut self, body: &[u8], writer: &mut Writer) -> anyhow::Result<()> {
        let Some(caps) = self.clipboard else {
            debug!("client {}: an extended cut text without listing the extension; ignored", self.id.0);
            return Ok(());
        };
        let message = match clipboard::parse(body) {
            Ok(message) => message,
            Err(e) => {
                warn!("client {}: {e}; ignored", self.id.0);
                return Ok(());
            }
        };
        match message {
            ClipboardMessage::Caps(theirs) => {
                debug!("client {}: clipboard caps {theirs:?}", self.id.0);
                self.clipboard = Some(theirs);
            }
            // A notify of nothing is the client's clipboard emptied or holding
            // what is not text; the desktop's is left as it is.
            ClipboardMessage::Notify { formats } => {
                if formats & clipboard::FORMAT_TEXT != 0 && caps.takes(clipboard::ACTION_REQUEST) {
                    writer.send(&msg::server_extended_cut_text(&clipboard::request())).await?;
                }
            }
            ClipboardMessage::Provide { text: Some(text) } => {
                debug!("client {}: {} bytes for the clipboard", self.id.0, text.len());
                self.shared.command(Command::SetClipboard { client: self.id, text });
            }
            ClipboardMessage::Provide { text: None } => debug!("client {}: a clipboard with no text; ignored", self.id.0),
            ClipboardMessage::Request { formats } => {
                if formats & clipboard::FORMAT_TEXT != 0 && caps.takes(clipboard::ACTION_PROVIDE) {
                    let text = self.shared.clipboard();
                    match clipboard::provide(&text) {
                        Ok(body) => writer.send(&msg::server_extended_cut_text(&body)).await?,
                        Err(e) => warn!("client {}: the clipboard is not sent: {e}", self.id.0),
                    }
                }
            }
            ClipboardMessage::Peek => {
                if caps.takes(clipboard::ACTION_NOTIFY) {
                    let has_text = !self.shared.clipboard().is_empty();
                    writer.send(&msg::server_extended_cut_text(&clipboard::notify(has_text))).await?;
                }
            }
        }
        Ok(())
    }

    /// Tell the client the desktop's clipboard changed: a notify, or — to a
    /// client that takes no notify — the text itself, if it takes that unasked.
    async fn announce_clipboard(&mut self, writer: &mut Writer) -> anyhow::Result<()> {
        let Some(caps) = self.clipboard else { return Ok(()) };
        let text = self.shared.clipboard();
        if caps.takes(clipboard::ACTION_NOTIFY) {
            writer.send(&msg::server_extended_cut_text(&clipboard::notify(!text.is_empty()))).await?;
        } else if caps.takes(clipboard::ACTION_PROVIDE)
            && caps.takes_text()
            && !text.is_empty()
            && caps.text_size.is_some_and(|size| text.len() <= size as usize)
            && let Ok(body) = clipboard::provide(&text)
        {
            writer.send(&msg::server_extended_cut_text(&body)).await?;
        }
        Ok(())
    }

    async fn send_geometry(&mut self, writer: &mut Writer) -> anyhow::Result<()> {
        let g = self.shared.geometry();
        debug!("client {}: reporting {}x{} at scale {:.2}", self.id.0, g.width, g.height, g.scale);
        writer.send(&output_scale(g.width, g.height, g.scale)).await?;
        Ok(())
    }

    async fn send_outputs(&mut self, writer: &mut Writer) -> anyhow::Result<()> {
        let displays = self.shared.displays();
        debug!("client {}: listing {} outputs, sharing id {}", self.id.0, displays.entries.len(), displays.active);
        writer.send(&output_list(displays.active, &displays.entries)).await?;
        Ok(())
    }

    /// One ExtendedDesktopSize rectangle as its own update.
    async fn send_eds(&mut self, writer: &mut Writer, reason: u16, status: u16, size: (u16, u16)) -> anyhow::Result<()> {
        let mut update = msg::update_header(1).to_vec();
        update.extend_from_slice(&msg::extended_desktop_size_rect(reason, status, size.0, size.1, &[Screen::whole(size.0, size.1)]));
        writer.send(&update).await?;
        Ok(())
    }

    /// Send pixels if the client wants some and something has changed.
    async fn maybe_update(&mut self, writer: &mut Writer) -> anyhow::Result<()> {
        let wants = self.continuous || self.pending.is_some();
        if !wants || self.fence_outstanding {
            return Ok(());
        }
        // The cursor, ExtendedDesktopSize and audio announcements are updates like any
        // other, so they wait for a request. Each is sent as its own update
        // ahead of the pixels; when there are none to send they are the
        // request's whole answer.
        let announced = self.announce_cursor || self.announce_eds || self.announce_audio;
        if self.announce_cursor {
            self.announce_cursor = false;
            let image = self.shared.cursor();
            let mut update = msg::update_header(1).to_vec();
            if self.alpha_cursor {
                update.extend_from_slice(&alpha_cursor_rect(image.as_deref()));
            } else {
                update.extend_from_slice(&cursor_rect(&self.format, image.as_deref()));
            }
            writer.send(&update).await?;
        }
        if self.announce_eds {
            self.announce_eds = false;
            let size = self.known_size;
            self.send_eds(writer, msg::EDS_REASON_SERVER, msg::EDS_STATUS_OK, size).await?;
        }
        if self.announce_audio {
            self.announce_audio = false;
            let mut update = msg::update_header(1).to_vec();
            update.extend_from_slice(&audio_rect());
            writer.send(&update).await?;
        }
        let answered = |this: &mut Self| {
            if announced {
                this.pending = None;
            }
        };
        // A slowed link's frame waits for its turn; the announcements above do
        // not, since none of them is a frame. A cursor or a resize a quarter of
        // a second late is the one lag the walk is not there to add.
        if self.frame_at().is_some() {
            answered(self);
            return Ok(());
        }

        // Under the lock: decide, and copy the pixels out. Encoding happens after.
        let mut pieces: Vec<Piece> = Vec::new();
        let mut resized = None;
        let generation;
        let size;
        let full;
        {
            let fb = self.shared.framebuffer.lock().unwrap();
            if !fb.painted {
                drop(fb);
                answered(self);
                return Ok(());
            }
            size = (fb.width, fb.height);
            generation = fb.generation;
            if size != self.known_size {
                resized = Some(fb.resize_origin);
            }
            full = resized.is_some() || self.pending == Some(false) || self.seen == 0;
            // A settle is the whole picture whether or not anything changed,
            // and an inter frame all the same.
            let rects = if full || self.settle_owed {
                vec![Rect::whole(fb.width, fb.height)]
            } else {
                match fb.damage_since(self.seen) {
                    Some(rects) if rects.is_empty() => {
                        drop(fb);
                        answered(self);
                        return Ok(());
                    }
                    Some(rects) => rects,
                    None => vec![Rect::whole(fb.width, fb.height)],
                }
            };
            let rects = if self.use_vp9 {
                // Something changed, and a VP9 frame is the whole picture.
                vec![Rect::whole(fb.width, fb.height)]
            } else if rects.len() > MAX_RECTS {
                crate::framebuffer::merge(rects, 1)
            } else {
                rects
            };
            self.scratch.clear();
            let stride = fb.stride();
            for rect in rects {
                let offset = self.scratch.len();
                let row_len = usize::from(rect.width) * 4;
                for row in usize::from(rect.y)..usize::from(rect.y) + usize::from(rect.height) {
                    let start = row * stride + usize::from(rect.x) * 4;
                    self.scratch.extend_from_slice(&fb.pixels[start..start + row_len]);
                }
                pieces.push(Piece { rect, offset });
            }
        }

        if let Some(origin) = resized {
            let (reason, status) = match origin {
                ResizeOrigin::Client(c) if c == self.id => (msg::EDS_REASON_THIS_CLIENT, msg::EDS_STATUS_OK),
                ResizeOrigin::Client(_) => (msg::EDS_REASON_OTHER_CLIENT, msg::EDS_STATUS_OK),
                ResizeOrigin::Server => (msg::EDS_REASON_SERVER, msg::EDS_STATUS_OK),
            };
            self.known_size = size;
            if self.eds_supported {
                self.send_eds(writer, reason, status, size).await?;
            } else if self.desktop_size_supported {
                let mut update = msg::update_header(1).to_vec();
                update.extend_from_slice(&msg::desktop_size_rect(size.0, size.1));
                writer.send(&update).await?;
            } else {
                anyhow::bail!(
                    "the desktop is now {}x{} and the client negotiated neither ExtendedDesktopSize nor DesktopSize to be told",
                    size.0,
                    size.1
                );
            }
        }

        self.out.clear();
        self.out.extend_from_slice(&msg::update_header(pieces.len() as u16));
        // Whether the update is a VP9 frame, and if so whether it is a keyframe
        // and whether its delivery is a verdict about the link: a delta frame at
        // the walk's quality. The settle's frame is at the ceiling, for the
        // rounds after it to leave.
        let settling = self.use_vp9 && self.settle_owed;
        let vp9 = if self.use_vp9 {
            let keyframe = self.encode_vp9(full, settling)?;
            Some((keyframe, !keyframe && !settling))
        } else {
            self.encode_pieces(&pieces);
            None
        };
        if self.fence_supported {
            self.fence_seq = self.fence_seq.wrapping_add(1);
            self.out.extend_from_slice(&msg::fence(msg::FENCE_REQUEST, &self.fence_seq.to_be_bytes()));
            self.fence_outstanding = true;
        }
        let sent = Instant::now();
        writer.send(&self.out).await.context("writing an update")?;
        if let Some((keyframe, _)) = vp9 {
            // Judged by the quality the frame was encoded at, which is what the
            // client is holding: the ceiling for a settle, whatever the walk holds.
            let quality = if settling { self.config.vp9_quality } else { self.vp9.as_ref().expect("a VP9 frame was encoded").quality() };
            if keyframe {
                // The frames behind it queue behind its crossing, a settle's too.
                self.walk.keyframe(sent);
            }
            self.coarse_since = self.walk.coarse(quality).then_some(sent);
            self.settle_owed = false;
            let interval = self.walk.interval();
            self.frame_after = (!interval.is_zero()).then(|| sent + interval);
        }
        match vp9.map(|(_, verdict)| verdict) {
            Some(verdict) if self.fence_supported => self.vp9_in_flight = Some((sent, verdict)),
            Some(verdict) => {
                let now = Instant::now();
                if verdict {
                    let moved = self.walk.written(now.saturating_duration_since(sent), now);
                    self.follow_walk(moved)?;
                }
                // Without Fence, a written frame is as delivered as it gets.
                if self.coarse_since.is_some() {
                    self.coarse_since = Some(now);
                }
            }
            None => {}
        }
        self.seen = generation;
        self.pending = None;
        Ok(())
    }

    /// The whole framebuffer, copied into `scratch`, as the next frame of the
    /// VP9 stream: a keyframe when the update is a full one or one is owed.
    /// Returns whether it was one. A `settling` frame is coded at the
    /// configured quality, with the encoder returned to the walk's after it —
    /// a retune and not a rebuild either way, so no keyframe is spent on it.
    fn encode_vp9(&mut self, full: bool, settling: bool) -> anyhow::Result<bool> {
        let (width, height) = self.known_size;
        if self.vp9.as_ref().is_none_or(|encoder| encoder.size() != (width, height)) {
            let encoder = Vp9Encoder::new(width, height, self.walk.quality())
                .with_context(|| format!("starting a VP9 stream for a {width}x{height} desktop"))?;
            self.vp9 = Some(encoder);
        }
        self.out.extend_from_slice(&msg::rect_header(0, 0, width, height, ENCODING_VP9));
        let (encoder, pixels, out) = (self.vp9.as_mut().expect("made above"), &self.scratch, &mut self.out);
        let keyframe = full || self.keyframe_owed;
        if settling {
            encoder.set_quality(self.config.vp9_quality).context("moving the VP9 quality for a settle")?;
        }
        // Tens of milliseconds for a large desktop, which is the worker's to
        // spend and not the runtime's to wait on.
        let encoded = tokio::task::block_in_place(|| encoder.encode_rect(pixels, usize::from(width) * 4, keyframe, out))
            .with_context(|| format!("encoding a {width}x{height} VP9 frame"));
        if settling {
            encoder.set_quality(self.walk.quality()).context("returning the VP9 quality after a settle")?;
        }
        encoded?;
        self.keyframe_owed = false;
        Ok(keyframe)
    }

    /// When a frame the walk slowed may go out, or `None` when the next frame
    /// is not held: nothing is wanted, a fence is outstanding, the link is not
    /// slowed, or its interval has passed.
    fn frame_at(&self) -> Option<Instant> {
        if !self.use_vp9 || self.fence_outstanding || !(self.continuous || self.pending.is_some()) {
            return None;
        }
        self.frame_after.filter(|at| Instant::now() < *at)
    }

    /// When the desktop is to be settled at the configured quality: a frame
    /// below it went out, the client has it, and nothing has been sent since.
    /// `None` while there is nothing to settle or the frame is still in
    /// flight — its fence coming back is what starts the quiet — and while the
    /// desktop has changed since that frame: it is not quiet, and the frame
    /// that carries the change, when the client asks for it, starts the quiet
    /// again.
    fn settle_at(&self) -> Option<Instant> {
        if !self.use_vp9 || self.settle_owed || self.vp9_in_flight.is_some() || *self.frames.borrow() > self.seen {
            return None;
        }
        self.coarse_since.map(|since| since + SETTLE_IDLE)
    }

    /// Owe the unchanged picture as one inter frame at the configured
    /// quality, which the next update sends. The walk keeps its place.
    fn settle(&mut self) -> anyhow::Result<()> {
        self.walk.settle(Instant::now());
        debug!(
            "client {}: the desktop went quiet below VP9 quality {}; settling it there (motion stays at {})",
            self.id.0,
            self.config.vp9_quality,
            self.walk.quality()
        );
        self.coarse_since = None;
        self.settle_owed = true;
        Ok(())
    }

    /// Move the running encoder's dial to where the walk went, if it went
    /// anywhere; an encoder made later starts there anyway. The interval the
    /// walk asks for is read when a frame goes out.
    fn follow_walk(&mut self, moved: Option<Pace>) -> anyhow::Result<()> {
        let Some(pace) = moved else {
            return Ok(());
        };
        if pace.interval.is_zero() {
            debug!("client {}: VP9 quality {}", self.id.0, pace.quality);
        } else {
            debug!("client {}: VP9 quality {}, at most one frame per {:?}", self.id.0, pace.quality, pace.interval);
        }
        if let Some(encoder) = &mut self.vp9 {
            encoder.set_quality(pace.quality).context("moving the VP9 quality")?;
        }
        Ok(())
    }

    /// The damaged rectangles, each as ZRLE or — for a client that never listed
    /// it — Raw.
    fn encode_pieces(&mut self, pieces: &[Piece]) {
        let encoding = if self.use_zrle { ENCODING_ZRLE } else { ENCODING_RAW };
        for piece in pieces {
            let r = piece.rect;
            self.out.extend_from_slice(&msg::rect_header(r.x, r.y, r.width, r.height, encoding));
            let stride = usize::from(r.width) * 4;
            let pixels = &self.scratch[piece.offset..piece.offset + stride * usize::from(r.height)];
            match &mut self.zrle {
                Some(zrle) if self.use_zrle => {
                    zrle.encode_rect(pixels, stride, usize::from(r.width), usize::from(r.height), &self.format, &mut self.out)
                }
                _ => encode_raw_rect(pixels, stride, usize::from(r.width), usize::from(r.height), &self.format, &mut self.out),
            }
        }
    }
}

/// Resolves at `at`; never, without one.
async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// Resolves with the desktop's next decision about the camera; never, while
/// none is plugged.
async fn camera_signal(camera: &mut Option<Camera>) -> CameraSignal {
    match camera {
        Some(camera) => camera.signal().await,
        None => std::future::pending().await,
    }
}

/// Resolves with the desktop's next decision about the microphone; never, while
/// none is plugged.
async fn microphone_signal(microphone: &mut Option<Microphone>) -> MicrophoneSignal {
    match microphone {
        Some(microphone) => microphone.signal().await,
        None => std::future::pending().await,
    }
}

/// Resolves when the capture has queued a buffer; never, while there is none.
async fn audio_ready(audio: &Option<Capture>) {
    match audio {
        Some(capture) => capture.ready().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    /// Either login lists RSA-AES at both widths, the wider first, and nothing
    /// else — the offer says how the credentials travel, not what checks them.
    /// A server with neither table lists None alone, and still does.
    #[test]
    fn the_offer_is_rsa_aes_or_none() {
        assert_eq!(Security::default().offered(), vec![msg::SECURITY_NONE]);
        let logins = [
            Login::Pam { service: "wlshare".into(), account: "me".into() },
            Login::Password { hash: "$argon2id$v=19$x".into() },
        ];
        for login in logins {
            let rsa_aes = RsaAes { key: Arc::new(ServerKey::generate().unwrap()), login };
            assert_eq!(
                Security { rsa_aes: Some(rsa_aes) }.offered(),
                vec![rsa_aes::SECURITY_RSA_AES_256, rsa_aes::SECURITY_RSA_AES_128]
            );
        }
    }
}
