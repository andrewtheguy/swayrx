//! A client that takes the daemon's VP9 stream and throws it away: what the
//! capture container (`docker/capture/`) connects to the daemon with, so that
//! a session runs — the stream is coded, the walk hears its fences, and a
//! daemon started with `--capture-frames` writes what the encoder was handed —
//! without the remotex gateway.
//!
//! ```text
//! vp9-sink ADDR --size WxH [--quality 1..100] [--held] [--seconds N]
//!          [--frames N] [--resize SECS:WxH] [--slow SECS:SECS:MILLIS]
//!          [--keyframe SECS] [--keys] [--ivf FILE] [--login USER]
//! ```
//!
//! The handshake is RFB 3.8 with security None, so the daemon is one started
//! without a `[pam]` or `[password]` table, or with `--login` RSA-AES: the
//! exchange `wlshare_rfb::rsa_aes` has both ends of, answered with that
//! username and the password in the environment's `VP9_SINK_PASSWORD`, the
//! server's fingerprint printed and not pinned. The list names VP9 at the
//! quality, Fence and ContinuousUpdates so frames flow and the walk is
//! answered, and ExtendedDesktopSize so the desktop can be resized: `--size`
//! is sent as a SetDesktopSize before the first frame, so the headless output
//! takes the size the capture is to be at, and `--resize` sends another at
//! that many seconds in. `--slow FROM:FOR:MILLIS` makes every read of the
//! socket wait that many milliseconds, from that second for that many
//! seconds: on a loopback link that is what moves the walk, since every
//! fence then comes back late and the dial goes down a step a second; the
//! fences answered promptly after it bring it back up, and a desktop that
//! then goes quiet is settled.
//! `--keyframe` asks for the whole framebuffer, non-incrementally, at that
//! second, which the daemon answers with a keyframe. `--frames` keeps the
//! session past `--seconds` until that many frames have come, for content
//! the encoder takes long over, and for ten times `--seconds` at most: a
//! desktop that has gone still never sends them. With `--keys` the sink types:
//! every line of its standard input is a keysym in hexadecimal, pressed and
//! let go as a KeyEvent each, or `@X,Y,BUTTONS`, a PointerEvent at that pixel
//! with that button mask, so that what plays the desktop types through
//! the daemon as a person at a client does, on the daemon's own keyboard.
//! With `--ivf` the frames are not thrown away but written to that file as
//! an IVF, in the order they came and one a tick, for a decoder that is not
//! this repository's to be run on what a session was sent.
//! Every second a line
//! says how many frames and bytes came and how many were keyframes.

use std::io::Write as _;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use wlshare_rfb::rsa_aes::{self, ClientKey, Credentials, FrameReader, SECURITY_RSA_AES_128, SECURITY_RSA_AES_256, Sealer, Strength};
use wlshare_rfb::client::{self, RectBody, ServerMsg};
use wlshare_rfb::msg::{FENCE_REQUEST, SECURITY_NONE};
use wlshare_rfb::vp9::{ENCODING_VP9_HELD, ENCODING_VP9_QUALITY_BASE};
use wlshare_rfb::{ENCODING_CONTINUOUS_UPDATES, ENCODING_CURSOR, ENCODING_EXTENDED_DESKTOP_SIZE, ENCODING_FENCE, ENCODING_VP9};

struct Args {
    addr: String,
    size: (u16, u16),
    quality: u8,
    held: bool,
    seconds: u64,
    frames: u64,
    resize: Option<(f64, (u16, u16))>,
    slow: Option<(f64, (f64, u64))>,
    keyframe: Option<f64>,
    keys: bool,
    ivf: Option<String>,
    login: Option<String>,
}

/// The way out to the daemon: in the clear, or in the frames of the RSA-AES
/// session a login left.
struct Out {
    half: OwnedWriteHalf,
    sealer: Option<Sealer>,
}

impl Out {
    async fn send(&mut self, msg: &[u8]) -> std::io::Result<()> {
        match &mut self.sealer {
            Some(sealer) => self.half.write_all(&sealer.frame(msg)).await,
            None => self.half.write_all(msg).await,
        }
    }
}

/// A line of `--keys` that is a pointer's: `@X,Y,BUTTONS`.
fn pointer(line: &str) -> Option<(u16, u16, u8)> {
    let mut fields = line.strip_prefix('@')?.split(',');
    let at = (fields.next()?.parse().ok()?, fields.next()?.parse().ok()?, fields.next()?.parse().ok()?);
    fields.next().is_none().then_some(at)
}

/// An IVF's header for a VP9 stream of this size. The frame count is left at
/// zero: a session ends when it ends, and no decoder needs it.
fn ivf_header(width: u16, height: u16) -> [u8; 32] {
    let mut header = [0u8; 32];
    header[..4].copy_from_slice(b"DKIF");
    header[6..8].copy_from_slice(&32u16.to_le_bytes());
    header[8..12].copy_from_slice(b"VP90");
    header[12..14].copy_from_slice(&width.to_le_bytes());
    header[14..16].copy_from_slice(&height.to_le_bytes());
    header[16..20].copy_from_slice(&60u32.to_le_bytes());
    header[20..24].copy_from_slice(&1u32.to_le_bytes());
    header
}

fn size(s: &str) -> Result<(u16, u16), String> {
    let (w, h) = s.split_once('x').ok_or_else(|| format!("{s}: not WxH"))?;
    Ok((w.parse().map_err(|_| format!("{w}: not a width"))?, h.parse().map_err(|_| format!("{h}: not a height"))?))
}

fn at<T>(s: &str, parse: impl Fn(&str) -> Result<T, String>) -> Result<(f64, T), String> {
    let (secs, rest) = s.split_once(':').ok_or_else(|| format!("{s}: not SECS:..."))?;
    Ok((secs.parse().map_err(|_| format!("{secs}: not seconds"))?, parse(rest)?))
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().ok_or("an address to connect to")?;
    let mut parsed = Args { addr, size: (0, 0), quality: 90, held: false, seconds: 30, frames: 0, resize: None, slow: None, keyframe: None, keys: false, ivf: None, login: None };
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} takes a value"));
        match flag.as_str() {
            "--size" => parsed.size = size(&value()?)?,
            "--quality" => parsed.quality = value()?.parse().map_err(|_| "a quality 1..100")?,
            "--held" => parsed.held = true,
            "--seconds" => parsed.seconds = value()?.parse().map_err(|_| "a number of seconds")?,
            "--frames" => parsed.frames = value()?.parse().map_err(|_| "a number of frames")?,
            "--resize" => parsed.resize = Some(at(&value()?, size)?),
            "--slow" => parsed.slow = Some(at(&value()?, |s| at(s, |ms| ms.parse().map_err(|_| format!("{ms}: not milliseconds"))))?),
            "--keys" => parsed.keys = true,
            "--ivf" => parsed.ivf = Some(value()?),
            "--login" => parsed.login = Some(value()?),
            "--keyframe" => parsed.keyframe = Some(value()?.parse().map_err(|_| "seconds")?),
            other => return Err(format!("{other}: not a flag")),
        }
    }
    if parsed.size == (0, 0) {
        return Err("--size WxH is needed".into());
    }
    if !(1..=100).contains(&parsed.quality) {
        return Err("--quality takes 1..100".into());
    }
    Ok(parsed)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("vp9-sink: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args).await {
        eprintln!("vp9-sink: {e}");
        std::process::exit(1);
    }
    // Not a return: with `--keys` a thread is still reading the standard
    // input, which the runtime would wait on for as long as nobody types.
    std::process::exit(0);
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let socket = TcpStream::connect(&args.addr).await?;
    socket.set_nodelay(true)?;
    let (mut reader, half) = socket.into_split();
    let mut out = Out { half, sealer: None };
    client::read_version(&mut reader).await?;
    out.send(wlshare_rfb::msg::PROTOCOL_VERSION).await?;
    let types = client::read_security_types(&mut reader).await?;
    let mut socket: Box<dyn AsyncRead + Unpin> = match &args.login {
        None => {
            if !types.contains(&SECURITY_NONE) {
                return Err(format!("the daemon offers security {types:?}, not None: start it without [pam] and [password], or --login").into());
            }
            out.send(&[SECURITY_NONE]).await?;
            Box::new(reader)
        }
        Some(username) => {
            let chosen = [SECURITY_RSA_AES_256, SECURITY_RSA_AES_128]
                .into_iter()
                .find(|t| types.contains(t))
                .ok_or_else(|| format!("the daemon offers security {types:?}, not RSA-AES: it takes no login"))?;
            let strength = Strength::of(chosen).expect("both are RSA-AES types");
            let password = std::env::var("VP9_SINK_PASSWORD").map_err(|_| "--login takes its password from VP9_SINK_PASSWORD")?;
            out.send(&[chosen]).await?;
            let exchange = rsa_aes::begin(&mut reader, &mut out.half, strength, ClientKey::of_bits(2048)?).await?;
            eprintln!("vp9-sink: the server's key is {}, asking for {:?}", exchange.fingerprint(), exchange.subtype());
            let session = exchange.login(&mut out.half, &Credentials { username: username.clone(), password }).await?;
            out.sealer = Some(session.sealer);
            Box::new(FrameReader::new(reader, session.opener))
        }
    };
    client::read_security_result(&mut socket).await?;
    out.send(&[1]).await?;
    let init = client::read_server_init(&mut socket).await?;
    eprintln!("vp9-sink: {} is {}x{}", init.name, init.width, init.height);

    // Cursor because the daemon requires it of every client; its rectangles
    // are parsed and dropped like everything but the frames.
    let mut encodings = vec![
        ENCODING_VP9,
        ENCODING_VP9_QUALITY_BASE + i32::from(args.quality),
        ENCODING_FENCE,
        ENCODING_CONTINUOUS_UPDATES,
        ENCODING_EXTENDED_DESKTOP_SIZE,
        ENCODING_CURSOR,
    ];
    if args.held {
        encodings.push(ENCODING_VP9_HELD);
    }
    out.send(&client::set_encodings(&encodings)).await?;
    let (mut width, mut height) = (init.width, init.height);
    if (width, height) != args.size {
        out.send(&client::set_desktop_size(args.size.0, args.size.1)).await?;
    }
    out.send(&client::enable_continuous_updates(true, 0, 0, width, height)).await?;
    out.send(&client::framebuffer_update_request(false, 0, 0, width, height)).await?;

    let began = Instant::now();
    let end = began + Duration::from_secs(args.seconds);
    let last = began + Duration::from_secs(args.seconds * 10);
    let (mut resize, mut keyframe) = (args.resize, args.keyframe);
    let mut slowed = false;
    let mut ivf = match &args.ivf {
        Some(path) => {
            let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
            file.write_all(&ivf_header(args.size.0, args.size.1))?;
            Some(file)
        }
        None => None,
    };
    let mut keys = args.keys.then(|| BufReader::new(tokio::io::stdin()).lines());
    let mut buf = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 16];
    let (mut frames, mut bytes, mut keyframes, mut second) = (0u64, 0u64, 0u64, 0u64);
    while Instant::now() < end || (frames < args.frames && Instant::now() < last) {
        let now = began.elapsed().as_secs_f64();
        if let Some((secs, to)) = resize
            && now >= secs
        {
            resize = None;
            out.send(&client::set_desktop_size(to.0, to.1)).await?;
        }
        if let Some((from, (for_secs, millis))) = args.slow
            && now >= from
            && now < from + for_secs
        {
            if !slowed {
                eprintln!("vp9-sink: {now:.1}s reading {millis} ms late for {for_secs} s");
                slowed = true;
            }
            tokio::time::sleep(Duration::from_millis(millis)).await;
        }
        if let Some(secs) = keyframe
            && now >= secs
        {
            keyframe = None;
            out.send(&client::framebuffer_update_request(false, 0, 0, width, height)).await?;
        }
        let typed = async {
            match &mut keys {
                Some(lines) => lines.next_line().await,
                None => std::future::pending().await,
            }
        };
        let read = tokio::select! {
            read = socket.read(&mut chunk) => read?,
            line = typed => {
                match line? {
                    Some(line) => {
                        if let Some((x, y, buttons)) = pointer(line.trim()) {
                            out.send(&client::pointer_event(buttons, x, y)).await?;
                            continue;
                        }
                        let keysym = u32::from_str_radix(line.trim(), 16).map_err(|_| format!("{line}: not a keysym in hexadecimal"))?;
                        out.send(&client::key_event(true, keysym)).await?;
                        out.send(&client::key_event(false, keysym)).await?;
                    }
                    // Nobody is left to type.
                    None => keys = None,
                }
                continue;
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => continue,
        };
        if read == 0 {
            return Err("the daemon closed the connection".into());
        }
        buf.extend_from_slice(&chunk[..read]);
        let mut at = 0;
        while let Some((msg, used)) = client::parse(&buf[at..])? {
            at += used;
            match msg {
                ServerMsg::Update(rects) => {
                    for rect in rects {
                        match rect.body {
                            RectBody::Vp9(frame) => {
                                frames += 1;
                                bytes += frame.len() as u64;
                                if screen_vp9_native::frame_header(&frame).is_some_and(|h| h.keyframe) {
                                    keyframes += 1;
                                }
                                if let Some(file) = &mut ivf {
                                    file.write_all(&(frame.len() as u32).to_le_bytes())?;
                                    file.write_all(&(frames - 1).to_le_bytes())?;
                                    file.write_all(&frame)?;
                                }
                            }
                            RectBody::ExtendedDesktopSize { .. } => {
                                (width, height) = (rect.width, rect.height);
                                eprintln!("vp9-sink: {:.1}s desktop {width}x{height}", began.elapsed().as_secs_f64());
                                out.send(&client::enable_continuous_updates(true, 0, 0, width, height)).await?;
                                out.send(&client::framebuffer_update_request(false, 0, 0, width, height)).await?;
                            }
                            _ => {}
                        }
                    }
                }
                ServerMsg::Fence { flags, payload } if flags & FENCE_REQUEST != 0 => {
                    out.send(&client::fence(flags & !FENCE_REQUEST, &payload)).await?;
                }
                _ => {}
            }
        }
        buf.drain(..at);
        let elapsed = began.elapsed().as_secs();
        if elapsed > second {
            second = elapsed;
            eprintln!("vp9-sink: {second}s {frames} frames {bytes} bytes {keyframes} keyframes");
        }
    }
    if let Some(mut file) = ivf {
        file.flush()?;
    }
    eprintln!("vp9-sink: done, {frames} frames {bytes} bytes {keyframes} keyframes");
    Ok(())
}
