//! Screen capture through wlr-screencopy into a shared-memory buffer, and from
//! there into the framebuffer.
//!
//! One frame is in flight at a time. `copy_with_damage` makes the compositor
//! answer only when something changed since the frame before, so an idle desktop
//! costs nothing and a busy one is paced by [`crate::config::Config::max_fps`].
//! Capture runs only while a client is connected, and each desk has a capture of
//! its own, of its own output.
//!
//! The exception is a framebuffer holding no pixels yet -- freshly made, resized,
//! or switched to another output. There is no frame before for damage to be
//! measured against, so the frame that fills it is asked for outright and taken
//! whole; waiting for damage there would hold a blank screen for as long as the
//! output happened to be still.

use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use calloop::RegistrationToken;
use calloop::timer::{TimeoutAction, Timer};
use log::{debug, error, info, warn};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::compositor::Compositor;
use crate::framebuffer::{BGRX, FrameLayout, Rect, ResizeOrigin};

/// A `wl_shm` buffer the compositor copies a frame into: a screen frame here, a
/// cursor frame in [`crate::cursor`].
pub(crate) struct ShmBuffer {
    _fd: OwnedFd,
    pool: WlShmPool,
    pub buffer: WlBuffer,
    pub map: memmap2::MmapMut,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    format: wl_shm::Format,
}

impl ShmBuffer {
    pub fn new(shm: &WlShm, qh: &QueueHandle<Compositor>, width: u32, height: u32, stride: u32, format: wl_shm::Format) -> anyhow::Result<Self> {
        let size = stride as usize * height as usize;
        let fd = rustix::fs::memfd_create("wlshare-frame", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, size as u64)?;
        // SAFETY: the mapping covers a memfd this process owns, sized just above;
        // the compositor writes it only between `copy` and `ready`, when this
        // side does not read it.
        let map = unsafe { memmap2::MmapMut::map_mut(&fd)? };
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
        Ok(Self { _fd: fd, pool, buffer, map, width, height, stride, format })
    }

    pub fn matches(&self, width: u32, height: u32, stride: u32, format: wl_shm::Format) -> bool {
        (self.width, self.height, self.stride, self.format) == (width, height, stride, format)
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

#[derive(Default)]
pub struct Capture {
    frame: Option<ZwlrScreencopyFrameV1>,
    buffer: Option<ShmBuffer>,
    /// The frame the compositor announced, before its buffer is ready.
    announced: Option<(u32, u32, u32, wl_shm::Format)>,
    damage: Vec<Rect>,
    /// How the frame in flight differs from the framebuffer's own layout, from
    /// the format announced for it and its y-invert flag.
    layout: FrameLayout,
    last_ready: Option<Instant>,
    timer: Option<RegistrationToken>,
    failures: u32,
}

impl Compositor {
    /// Begin capturing a desk's output if a client wants frames and nothing is
    /// in flight — the cursor image beside the frames, on its own session.
    pub fn start_capture(&mut self, desk: usize) {
        self.start_cursor(desk);
        if self.desks[desk].client.is_none() || self.desks[desk].capture.frame.is_some() {
            return;
        }
        let Some(manager) = &self.screencopy else { return };
        let Some(output) = self.outputs.selected(desk) else { return };
        // Keep the compositor's pointer out of the framebuffer. wlroots 0.19
        // gives the headless backend a cursor plane, so a screencopy without
        // overlay_cursor can finally leave it behind; the plane's image is
        // captured on its own ([`crate::cursor`]) and sent as a Cursor
        // pseudo-rectangle, so the client moves the pointer without waiting for
        // a captured frame.
        let frame = manager.capture_output(0, &output.output, &self.qh, ());
        let capture = &mut self.desks[desk].capture;
        capture.damage.clear();
        capture.layout = FrameLayout::default();
        capture.announced = None;
        capture.frame = Some(frame);
    }

    /// Stop after the frame in flight, if any: nobody is watching. The cursor
    /// session closes with it.
    pub fn stop_capture(&mut self, desk: usize) {
        self.stop_cursor(desk);
        let capture = &mut self.desks[desk].capture;
        if let Some(token) = capture.timer.take() {
            self.handle.remove(token);
        }
        if let Some(frame) = capture.frame.take() {
            frame.destroy();
        }
        capture.buffer = None;
        capture.last_ready = None;
    }

    /// Capture again after `delay`, replacing any timer already set.
    fn capture_after(&mut self, desk: usize, delay: Duration) {
        if let Some(token) = self.desks[desk].capture.timer.take() {
            self.handle.remove(token);
        }
        let timer = if delay.is_zero() { Timer::immediate() } else { Timer::from_duration(delay) };
        match self.handle.insert_source(timer, move |_, _, state: &mut Compositor| {
            state.desks[desk].capture.timer = None;
            state.start_capture(desk);
            TimeoutAction::Drop
        }) {
            Ok(token) => self.desks[desk].capture.timer = Some(token),
            Err(e) => error!("cannot schedule a capture: {e}"),
        }
    }

    /// The delay that keeps captures under the configured frame rate.
    fn pace(&self, desk: usize) -> Duration {
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.max_fps));
        match self.desks[desk].capture.last_ready {
            Some(at) => interval.saturating_sub(at.elapsed()),
            None => Duration::ZERO,
        }
    }

    fn frame_ready(&mut self, desk: usize) {
        let shared = self.shared().clone();
        let state = &mut self.desks[desk];
        let Some(buffer) = &state.capture.buffer else { return };
        let (width, height) = (buffer.width as u16, buffer.height as u16);
        let origin = match state.pending_resize {
            Some((client, w, h)) if (w, h) == (width, height) => {
                state.pending_resize = None;
                ResizeOrigin::Client(client)
            }
            _ => ResizeOrigin::Server,
        };
        let damage = std::mem::take(&mut state.capture.damage);
        let generation = {
            let mut fb = shared.desks[desk].framebuffer.lock().unwrap();
            if (fb.width, fb.height) != (width, height) {
                info!("the framebuffer is now {width}x{height} ({origin:?})");
                fb.resize(width, height, origin);
            }
            let damage = damage_for(damage, fb.painted, width, height);
            fb.apply(&buffer.map, buffer.stride as usize, &damage, state.capture.layout);
            fb.generation
        };
        shared.desks[desk].frame_changed(generation);
        state.capture.last_ready = Some(Instant::now());
        state.capture.failures = 0;
    }

    /// The desk whose frame in flight this is, or `None` for a frame destroyed
    /// while its events were in flight.
    fn frame_desk(&self, frame: &ZwlrScreencopyFrameV1) -> Option<usize> {
        self.desks.iter().position(|desk| desk.capture.frame.as_ref() == Some(frame))
    }
}

/// What of a captured frame to copy in: the rectangles the compositor reported,
/// or the whole frame.
///
/// Damage is measured against the frame before, so it means nothing until there
/// has been one. A framebuffer that holds no pixels at its current size --
/// `painted` false, which is every framebuffer just made, just resized, or just
/// pointed at another output -- takes the frame whole; so does one whose frame
/// reported no damage at all, where everything may have changed.
fn damage_for(reported: Vec<Rect>, painted: bool, width: u16, height: u16) -> Vec<Rect> {
    if painted && !reported.is_empty() {
        return reported;
    }
    vec![Rect::whole(width, height)]
}

/// Where the framebuffer's `B, G, R, X` bytes sit in a pixel of `format`, or
/// `None` when this server cannot read it at all.
///
/// These are the eight 32-bit orders at eight bits a channel, which is every
/// format wlroots' screencopy can offer for one: GLES2 and Vulkan report
/// XRGB8888, ARGB8888, XBGR8888 or ABGR8888, and pixman additionally reports
/// the four with the unused byte first. Everything past this table -- 10-bit,
/// 16-bit, 565, 5551, and the packed 24-bit orders -- is a conversion rather
/// than a rearrangement, and none is reachable from a compositor an ordinary
/// desktop runs; see docs/architecture.md.
///
/// The DRM names read most significant byte first, so each one is its own
/// memory order reversed on a little-endian machine, which is the only kind
/// wl_shm describes.
fn channel_bytes(format: wl_shm::Format) -> Option<[u8; 4]> {
    match format {
        // In memory: B, G, R, X -- the framebuffer's own order.
        wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => Some(BGRX),
        // R, G, B, X
        wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => Some([2, 1, 0, 3]),
        // X, B, G, R
        wl_shm::Format::Rgbx8888 | wl_shm::Format::Rgba8888 => Some([1, 2, 3, 0]),
        // X, R, G, B
        wl_shm::Format::Bgrx8888 | wl_shm::Format::Bgra8888 => Some([3, 2, 1, 0]),
        _ => None,
    }
}

/// How much this server would rather have `format`: a straight copy over a
/// rearrangement over nothing it can use.
fn rank(format: wl_shm::Format) -> u8 {
    match channel_bytes(format) {
        Some(BGRX) => 2,
        Some(_) => 1,
        None => 0,
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Compositor {
    fn event(state: &mut Self, frame: &ZwlrScreencopyFrameV1, event: zwlr_screencopy_frame_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let Some(desk) = state.frame_desk(frame) else { return };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format, width, height, stride } => {
                let WEnum::Value(format) = format else { return };
                // The compositor lists every format it can copy into, in no
                // order this server may rely on. Keep the best one seen so far,
                // and the first of them regardless -- an unusable format still
                // has to reach the error below, which names it.
                let better = state.desks[desk].capture.announced.is_none_or(|(.., current)| rank(format) > rank(current));
                if better {
                    state.desks[desk].capture.announced = Some((width, height, stride, format));
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                let Some((width, height, stride, format)) = state.desks[desk].capture.announced else {
                    warn!("the compositor offered no buffer format");
                    state.capture_failed(desk, frame);
                    return;
                };
                let Some(bytes) = channel_bytes(format) else {
                    error!("the compositor offers frames only as {format:?}; this server needs a 32-bit format at eight bits a channel: XRGB8888, XBGR8888, RGBX8888, BGRX8888 or one of their alpha spellings");
                    state.capture_failed(desk, frame);
                    return;
                };
                state.desks[desk].capture.layout.bytes = bytes;
                if !state.desks[desk].capture.buffer.as_ref().is_some_and(|b| b.matches(width, height, stride, format)) {
                    match ShmBuffer::new(&state.shm, qh, width, height, stride, format) {
                        Ok(b) => {
                            debug!("frame buffer {width}x{height}, stride {stride}, {format:?}");
                            state.desks[desk].capture.buffer = Some(b);
                        }
                        Err(e) => {
                            error!("cannot allocate a {width}x{height} frame buffer: {e}");
                            state.capture_failed(desk, frame);
                            return;
                        }
                    }
                }
                // `copy_with_damage` waits for the output to change, which is
                // what keeps an idle desktop free -- but a blank framebuffer has
                // nothing to show in the meantime, and an output nobody is
                // touching can stay unchanged for minutes. So the frame that
                // fills a blank one is asked for outright -- and so is a frame
                // of a new size, which the framebuffer only takes when that
                // frame arrives: until then it still holds the old size's
                // pixels, and an idle output would never send the frame that
                // resizes it.
                let whole = {
                    let fb = state.shared().desks[desk].framebuffer.lock().unwrap();
                    !fb.painted || (fb.width, fb.height) != (width as u16, height as u16)
                };
                let buffer = state.desks[desk].capture.buffer.as_ref().unwrap();
                if whole {
                    debug!("asking for a whole {width}x{height} frame: the framebuffer holds no pixels at that size");
                    frame.copy(&buffer.buffer);
                } else {
                    frame.copy_with_damage(&buffer.buffer);
                }
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.desks[desk].capture.layout.flipped = flags.into_result().is_ok_and(|f| f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
            }
            zwlr_screencopy_frame_v1::Event::Damage { x, y, width, height } => {
                state.desks[desk].capture.damage.push(Rect {
                    x: x.min(u32::from(u16::MAX)) as u16,
                    y: y.min(u32::from(u16::MAX)) as u16,
                    width: width.min(u32::from(u16::MAX)) as u16,
                    height: height.min(u32::from(u16::MAX)) as u16,
                });
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                frame.destroy();
                state.desks[desk].capture.frame = None;
                state.frame_ready(desk);
                let delay = state.pace(desk);
                state.capture_after(desk, delay);
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.capture_failed(desk, frame);
            }
            _ => {}
        }
    }
}

impl Compositor {
    fn capture_failed(&mut self, desk: usize, frame: &ZwlrScreencopyFrameV1) {
        frame.destroy();
        self.desks[desk].capture.frame = None;
        self.desks[desk].capture.failures += 1;
        // An output being reconfigured fails a frame or two; give up loudly only
        // when it keeps failing.
        let delay = Duration::from_millis(100 * u64::from(self.desks[desk].capture.failures.min(20)));
        if self.desks[desk].capture.failures == 20 {
            error!("screen capture keeps failing; retrying every {delay:?}");
        } else {
            debug!("screen capture failed; retrying in {delay:?}");
        }
        self.capture_after(desk, delay);
    }
}

wayland_client::delegate_noop!(Compositor: ignore ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(Compositor: ignore WlShm);
wayland_client::delegate_noop!(Compositor: ignore WlShmPool);
wayland_client::delegate_noop!(Compositor: ignore WlBuffer);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_framebuffer_with_no_pixels_yet_takes_the_whole_frame() {
        let spot = vec![Rect { x: 4, y: 4, width: 8, height: 8 }];
        let whole = vec![Rect::whole(1280, 800)];
        // The frame that fills a blank framebuffer -- after a resize, or after a
        // switch to another output, where the size may not have changed at all.
        assert_eq!(damage_for(spot.clone(), false, 1280, 800), whole);
        // Once there are pixels to keep, only what the compositor reported is
        // copied over them.
        assert_eq!(damage_for(spot.clone(), true, 1280, 800), spot);
        // A frame that reported nothing says nothing about what is still good.
        assert_eq!(damage_for(Vec::new(), true, 1280, 800), whole);
    }
}
