//! The desktop's sound: a PipeWire capture of the monitor of wlshare's own
//! sink, one per client that enabled audio, in the format that client asked for
//! and encoded as FLAC or as Opus, whichever it asked for.
//!
//! wlshare runs inside the user's session, where the audio graph is, so it
//! reads what the desktop plays the same way it reads what the desktop draws:
//! from the compositor's side, with nothing configured on the client. While any
//! client listens, the desktop plays into a [`Speaker`], a null sink of
//! wlshare's own, rather than into the host's: the sound goes where the client
//! is, and the room the host is in stays silent, the way a remote desktop's
//! sound does. The capture is a `Stream/Input/Audio` node with
//! `stream.capture.sink` set and the speaker as its target, which is
//! PipeWire's word for "the monitor of that sink". PipeWire converts the
//! graph's own rate and sample format into the client's, and is asked for
//! buffers of twenty milliseconds — one FLAC frame's worth, and one Opus
//! packet's.
//!
//! Per client rather than shared, because the format and the codec are the
//! client's choice and two clients may choose differently; a capture stream is cheap, and a
//! desktop with two listeners is rare. The speaker is shared: every capture
//! reads the one sink the desktop plays into. Nothing runs while no client has
//! enabled audio: a capture's stream, its thread and its PipeWire connection
//! exist between an enable and the disable or disconnect that ends it, and the
//! speaker from the first listener's enable to the last one's end.
//!
//! PipeWire's loop wants a thread of its own, so each capture is one, and the
//! process callback runs on that loop rather than on the graph's real-time
//! thread — `RT_PROCESS` is deliberately not set. The callback encodes the
//! buffer ([`AudioEncoder`]) and queues the frames it completes, which
//! allocates, takes a mutex and wakes a task, and none of that is real-time
//! safe: run on the data thread it could stall the whole audio graph and give
//! every application on the host an xrun. The thread this capture already owns
//! is the right place for it, and off the session's task, which has pixels to
//! compress; a twenty-millisecond buffer takes a fraction of a millisecond to
//! encode. A session that falls behind loses the oldest frames, never the
//! newest, so what it does send is live, and each FLAC frame decodes on its
//! own, so a lost one costs the client nothing but its own samples; a lost
//! Opus packet is concealed by the client's decoder.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use log::{debug, info, warn};
use pipewire as pw;
use pw::spa;
use tokio::sync::Notify;
use wlshare_rfb::audio::{AudioEncoder, AudioFormat, Codec, SampleFormat};

use crate::shared::ClientId;

/// Frames kept for a session slow to send them; the oldest goes first.
const QUEUE_DEPTH: usize = 16;
/// How long PipeWire may take to come up before an enable is refused.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// The buffer PipeWire is asked for, in milliseconds of the client's rate.
const BUFFER_MS: u32 = 20;
/// The speaker's node name, which every capture targets.
const SPEAKER_NAME: &str = "wlshare-speaker";
/// The speaker's session priority. WirePlumber makes the available sink with
/// the highest one the default, after adding 30000 to the sink the user
/// configured and up to 20000 to the ones configured before it; a hardware
/// sink's own is in the low thousands, so this is past every sink the host has,
/// chosen or not, and the desktop plays into the speaker while it exists.
const SPEAKER_PRIORITY: u32 = 100_000;

/// The speaker, while any capture holds a [`Lease`] on it.
static SPEAKER: Mutex<Option<Speaker>> = Mutex::new(None);

/// One client's capture: the thread running PipeWire's loop, and the frame
/// messages it has produced.
pub struct Capture {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    queue: Arc<Queue>,
    /// Dropped after the thread has been joined, so the speaker outlives the
    /// capture of its monitor.
    _speaker: Lease,
}

struct Queue {
    buffers: Mutex<VecDeque<Vec<u8>>>,
    /// Signalled when a frame is queued; a permit is kept when nobody waits.
    ready: Notify,
    dropped: AtomicU64,
    /// The rate Opus is to be coded at, in bits per second, which the capture
    /// thread moves its encoder to before the next buffer.
    bitrate: AtomicU32,
}

impl Capture {
    /// Open a capture in `format` for `client`, coded as `codec`, Opus at
    /// `bitrate` bits per second. Blocks until PipeWire has taken the stream or
    /// refused it, so it belongs on a blocking thread.
    pub fn start(client: ClientId, codec: Codec, format: AudioFormat, bitrate: u32) -> anyhow::Result<Self> {
        let speaker = Lease::take()?;
        let encoder = AudioEncoder::new(codec, format, bitrate).with_context(|| format!("setting up the {codec:?} encoder"))?;
        let queue = Arc::new(Queue {
            buffers: Mutex::new(VecDeque::with_capacity(QUEUE_DEPTH)),
            ready: Notify::new(),
            dropped: AtomicU64::new(0),
            bitrate: AtomicU32::new(bitrate),
        });
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread_queue = queue.clone();
        let thread = std::thread::Builder::new()
            .name(format!("audio-{}", client.0))
            .spawn(move || run(client, codec, format, encoder, thread_queue, quit_rx, ready_tx))
            .context("spawning the audio thread")?;
        let mut capture = Self { quit, thread: Some(thread), queue, _speaker: speaker };
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(e)) => {
                // The thread has already returned.
                capture.thread.take().map(JoinHandle::join);
                Err(e)
            }
            Err(_) => {
                // Stuck coming up: ask it to quit and let it go rather than
                // wait for a loop that may never run.
                let _ = capture.quit.send(());
                capture.thread.take();
                anyhow::bail!("PipeWire did not take the capture within {START_TIMEOUT:?}")
            }
        }
    }

    /// Move the rate Opus is coded at to `bitrate` bits per second, from the
    /// capture's next buffer on. Nothing to a FLAC stream.
    pub fn set_bitrate(&self, bitrate: u32) {
        self.queue.bitrate.store(bitrate, Ordering::Relaxed);
    }

    /// The oldest frame message not yet sent, if any.
    pub fn take(&self) -> Option<Vec<u8>> {
        self.queue.buffers.lock().unwrap().pop_front()
    }

    /// Resolves once a frame has been queued since the last wait.
    pub async fn ready(&self) {
        self.queue.ready.notified().await;
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let dropped = self.queue.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            debug!("audio: {dropped} frame(s) were dropped for a session that fell behind");
        }
    }
}

fn spa_format(sample: SampleFormat) -> spa::param::audio::AudioFormat {
    use spa::param::audio::AudioFormat as F;
    match sample {
        SampleFormat::U8 => F::U8,
        SampleFormat::S8 => F::S8,
        SampleFormat::U16 => F::U16LE,
        SampleFormat::S16 => F::S16LE,
    }
}

/// The capture thread: PipeWire's loop, from connect to quit. `ready` is told
/// once, when the stream is connected or when that failed.
fn run(
    client: ClientId,
    codec: Codec,
    format: AudioFormat,
    mut encoder: AudioEncoder,
    queue: Arc<Queue>,
    quit: pw::channel::Receiver<()>,
    ready: std::sync::mpsc::Sender<anyhow::Result<()>>,
) {
    macro_rules! up {
        ($e:expr, $what:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready.send(Err(anyhow::Error::from(e).context($what)));
                    return;
                }
            }
        };
    }
    pw::init();
    let mainloop = up!(pw::main_loop::MainLoopRc::new(None), "creating PipeWire's loop");
    let context = up!(pw::context::ContextRc::new(&mainloop, None), "creating PipeWire's context");
    let core = up!(context.connect_rc(None), "connecting to PipeWire");

    let frames = format.frequency * BUFFER_MS / 1000;
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::STREAM_CAPTURE_SINK => "true",
        *pw::keys::TARGET_OBJECT => SPEAKER_NAME,
        *pw::keys::APP_NAME => "wlshare",
    };
    props.insert(*pw::keys::NODE_NAME, format!("wlshare client {}", client.0));
    props.insert(*pw::keys::NODE_LATENCY, format!("{frames}/{}", format.frequency));
    props.insert(*pw::keys::NODE_RATE, format!("1/{}", format.frequency));
    let stream = up!(pw::stream::StreamBox::new(&core, "wlshare audio", props), "creating the capture stream");

    let frame_bytes = format.frame_bytes();
    let process_queue = queue.clone();
    let mut bitrate = queue.bitrate.load(Ordering::Relaxed);
    let _listener = up!(
        stream
            .add_local_listener::<()>()
            .state_changed(move |_, _, old, new| match &new {
                pw::stream::StreamState::Error(e) => warn!("client {}: audio capture: {e}", client.0),
                _ => debug!("client {}: audio capture {old:?} -> {new:?}", client.0),
            })
            // On this capture's own loop thread, not the graph's real-time one.
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                let Some(bytes) = data.data() else { return };
                let start = offset.min(bytes.len());
                let end = (offset + size).min(bytes.len());
                // Whole frames only: a split frame would swap the channels of
                // everything after it.
                let whole = (end - start) / frame_bytes * frame_bytes;
                if whole == 0 {
                    return;
                }
                let wanted = process_queue.bitrate.load(Ordering::Relaxed);
                if wanted != bitrate {
                    bitrate = wanted;
                    // A stream at the rate it had is still a stream.
                    match encoder.set_bitrate(wanted) {
                        Ok(()) => debug!("client {}: audio now at {wanted} bit/s", client.0),
                        Err(e) => warn!("client {}: audio stays at its rate: {e}", client.0),
                    }
                }
                let frames = match encoder.push(&bytes[start..start + whole]) {
                    Ok(frames) => frames,
                    Err(e) => {
                        warn!("client {}: audio: {e}", client.0);
                        return;
                    }
                };
                if frames.is_empty() {
                    return;
                }
                let mut buffers = process_queue.buffers.lock().unwrap();
                for frame in frames {
                    if buffers.len() >= QUEUE_DEPTH {
                        buffers.pop_front();
                        process_queue.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    buffers.push_back(frame);
                }
                drop(buffers);
                process_queue.ready.notify_one();
            })
            .register(),
        "listening to the capture stream"
    );

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa_format(format.sample));
    info.set_rate(format.frequency);
    info.set_channels(u32::from(format.channels));
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let pod = up!(
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(object)),
        "serializing the audio format"
    )
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&pod).expect("a serialized pod")];
    up!(
        stream.connect(
            spa::utils::Direction::Input,
            None,
            // No RT_PROCESS: the process callback allocates and locks, which
            // the graph's real-time thread must not do — see the module doc.
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        ),
        "connecting the capture stream"
    );

    let loop_ = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| loop_.quit());
    info!(
        "client {}: capturing the speaker's monitor as {:?} x{} at {} Hz, {frames} frames a buffer, for {codec:?}",
        client.0, format.sample, format.channels, format.frequency
    );
    let _ = ready.send(Ok(()));
    mainloop.run();
    debug!("client {}: audio capture closed", client.0);
}

/// A hold on the speaker: the first one makes it, and the last one to go takes
/// it away. Every capture has one; a session restarting its capture holds one
/// more across the restart, so the host is not heard between the two.
pub struct Lease;

impl Lease {
    /// Blocks until PipeWire has taken the speaker, when there is none yet, and
    /// dropping the last one joins its thread, so both belong on a blocking
    /// thread.
    pub fn take() -> anyhow::Result<Self> {
        let mut speaker = SPEAKER.lock().unwrap();
        match speaker.as_mut() {
            Some(speaker) => speaker.leases += 1,
            None => *speaker = Some(Speaker::start()?),
        }
        Ok(Self)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut speaker = SPEAKER.lock().unwrap();
        let Some(held) = speaker.as_mut() else { return };
        held.leases -= 1;
        if held.leases == 0 {
            // Under the lock, so a new first listener waits for this node to go
            // rather than making a second of the same name beside it.
            drop(speaker.take());
        }
    }
}

/// The sink the desktop plays into while a client listens: a null sink, whose
/// only output is its monitor, with a priority that makes it the default.
///
/// Nothing on the host is changed to get there — no sink muted, no default
/// written — so nothing needs undoing. The node belongs to this thread's
/// PipeWire connection: when it quits, or the daemon dies, PipeWire removes the
/// node, WirePlumber makes the host's own sink the default again, and every
/// stream that followed the default follows it back. A stream an application
/// pinned to a sink of its own stays there, and is heard on the host.
struct Speaker {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// Captures holding a [`Lease`].
    leases: usize,
}

impl Speaker {
    fn start() -> anyhow::Result<Self> {
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("speaker".into())
            .spawn(move || serve(quit_rx, ready_tx))
            .context("spawning the speaker thread")?;
        let mut speaker = Self { quit, thread: Some(thread), leases: 1 };
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok(speaker),
            Ok(Err(e)) => {
                // The thread has already returned.
                speaker.thread.take().map(JoinHandle::join);
                Err(e)
            }
            Err(_) => {
                // Stuck waiting on the server: the quit was attached before the
                // loop ran, so it ends that wait, and dropping the speaker joins
                // the thread, its node and its connection gone with it.
                drop(speaker);
                anyhow::bail!("PipeWire did not take the speaker within {START_TIMEOUT:?}")
            }
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The speaker thread: PipeWire's loop, from the node's creation to quit.
/// `ready` is told once, when the server has made the node or refused it.
fn serve(quit: pw::channel::Receiver<()>, ready: std::sync::mpsc::Sender<anyhow::Result<()>>) {
    macro_rules! up {
        ($e:expr, $what:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready.send(Err(anyhow::Error::from(e).context($what)));
                    return;
                }
            }
        };
    }
    pw::init();
    let mainloop = up!(pw::main_loop::MainLoopRc::new(None), "creating PipeWire's loop");
    let context = up!(pw::context::ContextRc::new(&mainloop, None), "creating PipeWire's context");
    let core = up!(context.connect_rc(None), "connecting to PipeWire");

    let mut props = pw::properties::properties! {
        *pw::keys::FACTORY_NAME => "support.null-audio-sink",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => SPEAKER_NAME,
        // Named for where it plays, in the desktop's list of outputs.
        *pw::keys::NODE_DESCRIPTION => "wlshare remote audio",
        "audio.position" => "FL,FR",
    };
    props.insert(*pw::keys::PRIORITY_SESSION, SPEAKER_PRIORITY.to_string());
    let _node = up!(core.create_object::<pw::node::Node>("adapter", &props), "creating the speaker");

    // The node is made on the server's side; a round trip says whether it was.
    // Attached before the round trip, so a start that timed out can end a wait
    // the server never answers.
    let cancelled = Rc::new(Cell::new(false));
    let _quit = quit.attach(mainloop.loop_(), {
        let loop_ = mainloop.clone();
        let cancelled = cancelled.clone();
        move |()| {
            cancelled.set(true);
            loop_.quit();
        }
    });
    let pending = up!(core.sync(0), "waiting for PipeWire to make the speaker");
    let refused = Rc::new(RefCell::new(None::<String>));
    let settled = Rc::new(Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .done({
            let loop_ = mainloop.clone();
            let settled = settled.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    settled.set(true);
                    loop_.quit();
                }
            }
        })
        .error({
            let loop_ = mainloop.clone();
            let refused = refused.clone();
            let settled = settled.clone();
            move |id, _, res, message| {
                if settled.get() {
                    warn!("audio speaker: PipeWire error on object {id}: {message} ({res})");
                } else {
                    *refused.borrow_mut() = Some(format!("{message} ({res})"));
                    settled.set(true);
                    loop_.quit();
                }
            }
        })
        .register();
    mainloop.run();
    if cancelled.get() {
        debug!("audio: speaker start cancelled");
        return;
    }
    if let Some(message) = refused.take() {
        let _ = ready.send(Err(anyhow::anyhow!("PipeWire refused the speaker: {message}")));
        return;
    }

    info!("audio: the desktop plays into {SPEAKER_NAME} while a client listens");
    let _ = ready.send(Ok(()));
    mainloop.run();
    debug!("audio: speaker closed");
}
