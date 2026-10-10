//! A client that takes the daemon's VP9 stream and throws it away: what the
//! capture container (`docker/capture/`) connects to the daemon with, so that
//! a session runs — the stream is coded, the walk hears its fences, and a
//! daemon started with `--capture-frames` writes what the encoder was handed —
//! without the remotex gateway.
//!
//! ```text
//! vp9-sink ADDR --size WxH [--quality 1..100] [--held] [--seconds N]
//!          [--frames N] [--resize SECS:WxH] [--slow SECS:SECS:MILLIS]
//!          [--keyframe SECS] [--keys]
//! ```
//!
//! The handshake is RFB 3.8 with security None, so the daemon is one started
//! without a `[pam]` or `[password]` table. The list names VP9 at the
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
//! let go as a KeyEvent each, so that what plays the desktop types through
//! the daemon as a person at a client does, on the daemon's own keyboard.
//! Every second a line
//! says how many frames and bytes came and how many were keyframes.

use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
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
    let mut parsed = Args { addr, size: (0, 0), quality: 90, held: false, seconds: 30, frames: 0, resize: None, slow: None, keyframe: None, keys: false };
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
    let mut socket = TcpStream::connect(&args.addr).await?;
    socket.set_nodelay(true)?;
    client::read_version(&mut socket).await?;
    socket.write_all(wlshare_rfb::msg::PROTOCOL_VERSION).await?;
    let types = client::read_security_types(&mut socket).await?;
    if !types.contains(&SECURITY_NONE) {
        return Err(format!("the daemon offers security {types:?}, not None: start it without [pam] and [password]").into());
    }
    socket.write_all(&[SECURITY_NONE]).await?;
    client::read_security_result(&mut socket).await?;
    socket.write_all(&[1]).await?;
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
    socket.write_all(&client::set_encodings(&encodings)).await?;
    let (mut width, mut height) = (init.width, init.height);
    if (width, height) != args.size {
        socket.write_all(&client::set_desktop_size(args.size.0, args.size.1)).await?;
    }
    socket.write_all(&client::enable_continuous_updates(true, 0, 0, width, height)).await?;
    socket.write_all(&client::framebuffer_update_request(false, 0, 0, width, height)).await?;

    let began = Instant::now();
    let end = began + Duration::from_secs(args.seconds);
    let last = began + Duration::from_secs(args.seconds * 10);
    let (mut resize, mut keyframe) = (args.resize, args.keyframe);
    let mut slowed = false;
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
            socket.write_all(&client::set_desktop_size(to.0, to.1)).await?;
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
            socket.write_all(&client::framebuffer_update_request(false, 0, 0, width, height)).await?;
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
                        let keysym = u32::from_str_radix(line.trim(), 16).map_err(|_| format!("{line}: not a keysym in hexadecimal"))?;
                        socket.write_all(&client::key_event(true, keysym)).await?;
                        socket.write_all(&client::key_event(false, keysym)).await?;
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
                            }
                            RectBody::ExtendedDesktopSize { .. } => {
                                (width, height) = (rect.width, rect.height);
                                eprintln!("vp9-sink: {:.1}s desktop {width}x{height}", began.elapsed().as_secs_f64());
                                socket.write_all(&client::enable_continuous_updates(true, 0, 0, width, height)).await?;
                                socket.write_all(&client::framebuffer_update_request(false, 0, 0, width, height)).await?;
                            }
                            _ => {}
                        }
                    }
                }
                ServerMsg::Fence { flags, payload } if flags & FENCE_REQUEST != 0 => {
                    socket.write_all(&client::fence(flags & !FENCE_REQUEST, &payload)).await?;
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
    eprintln!("vp9-sink: done, {frames} frames {bytes} bytes {keyframes} keyframes");
    Ok(())
}
