//! The compositor's cursor image, through ext-image-copy-capture's pointer
//! cursor session.
//!
//! wlr-screencopy leaves the pointer out of a frame but says nothing of it: not
//! its shape, not whether it is there at all. wlroots 0.19 keeps the cursor of
//! every output, the headless ones included, on a plane of its own, and exports
//! that plane as the pointer cursor of the output's image capture source. A
//! session on it produces frames of the cursor image in the output's pixels, so
//! what arrives is exactly the shape the application under the pointer chose, at
//! the size a client should draw it. A frame of a rotated or flipped output's
//! cursor comes turned the way the output's buffer is, and says so; it is turned
//! back upright, the way the pointer coordinates a client sends are.
//!
//! One frame is always in flight. wlroots answers it only when the cursor buffer
//! changes — a new shape, a new scale — so a pointer that just moves costs
//! nothing and a frame in flight is the whole subscription. The session's
//! `enter` and `leave` say whether the cursor is on the output and showing: the
//! image is published only in between, and an image captured while it was away
//! is kept for when it comes back unchanged, which wlroots does not announce with
//! a frame. A hotspot takes effect with the frame after it, as the protocol has
//! it.
//!
//! The session needs a `wl_pointer`, which is the seat's to hand out: asking
//! before the seat has ever had a pointer is a protocol error, so the session
//! waits for its capabilities to name one — wlshare's own virtual pointer
//! provides it. And a pointer taken while the seat has one goes inert when the
//! seat loses it, which retargeting the virtual pointer does for a moment, so
//! every session takes a fresh one: the session outlives the pointer it was made
//! with, but not being made with a dead one.

use std::sync::Arc;
use std::time::Duration;

use calloop::RegistrationToken;
use calloop::timer::{TimeoutAction, Timer};
use log::{debug, error};
use wayland_client::protocol::wl_output::Transform;
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_shm;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
    ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wlshare_rfb::cursor::CursorImage;

use crate::capture::ShmBuffer;
use crate::compositor::Compositor;

/// How long a frame that failed for no reason the session can fix waits before
/// the next one is asked for.
const RETRY: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct CursorCapture {
    /// The compositor stopped the session on the shared output: none is opened
    /// on it again, or every capture would reopen one only to see it stop.
    /// Sharing another output clears it.
    pub stopped: bool,
    session: Option<Session>,
    retry: Option<RegistrationToken>,
}

/// A cursor session on a desk's shared output, with everything it was made from.
struct Session {
    pointer: WlPointer,
    source: ExtImageCaptureSourceV1,
    cursor: ExtImageCopyCaptureCursorSessionV1,
    capture: ExtImageCopyCaptureSessionV1,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    buffer: Option<ShmBuffer>,
    /// The buffer size being announced, until `done`.
    size: (u32, u32),
    /// The shm formats being announced, until `done`.
    formats: Vec<wl_shm::Format>,
    /// What a frame is captured into, as of the last `done`: size and format.
    constraints: Option<(u32, u32, wl_shm::Format)>,
    /// The cursor is on the output and showing.
    entered: bool,
    /// The hotspot the next frame takes effect with.
    hotspot: (i32, i32),
    /// The transform the compositor applied to the frame in flight.
    transform: Transform,
    /// The last image captured, or `None` for one that painted nothing.
    image: Option<Arc<CursorImage>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.capture.destroy();
        self.cursor.destroy();
        self.source.destroy();
        if self.pointer.version() >= 3 {
            self.pointer.release();
        }
    }
}

/// Where red, green, blue and alpha sit in a pixel of `format`, or `None` for a
/// format without alpha or not at eight bits a channel. A cursor needs its alpha,
/// so XRGB8888 and its kin are as useless here as a 10-bit format.
///
/// The DRM names read most significant byte first, so each is its own memory
/// order reversed on a little-endian machine, which is the only kind wl_shm
/// describes.
fn rgba_bytes(format: wl_shm::Format) -> Option<[usize; 4]> {
    match format {
        // In memory: B, G, R, A.
        wl_shm::Format::Argb8888 => Some([2, 1, 0, 3]),
        // R, G, B, A
        wl_shm::Format::Abgr8888 => Some([0, 1, 2, 3]),
        // A, B, G, R
        wl_shm::Format::Rgba8888 => Some([3, 2, 1, 0]),
        // A, R, G, B
        wl_shm::Format::Bgra8888 => Some([1, 2, 3, 0]),
        _ => None,
    }
}

/// A captured cursor frame as premultiplied RGBA, rows top down. Wayland
/// buffers are premultiplied already, so this only moves bytes.
fn to_rgba(map: &[u8], width: u32, height: u32, stride: u32, order: [usize; 4]) -> Vec<u8> {
    let (width, height, stride) = (width as usize, height as usize, stride as usize);
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in map.chunks_exact(stride).take(height) {
        for px in row[..width * 4].as_chunks::<4>().0 {
            rgba.extend_from_slice(&order.map(|i| px[i]));
        }
    }
    rgba
}

/// Undo `transform`, the one the compositor applied to a `width`×`height` image,
/// giving the image as the output shows it and its size, which a quarter turn
/// swaps.
fn upright(rgba: Vec<u8>, width: u32, height: u32, transform: Transform) -> (Vec<u8>, u32, u32) {
    let (w, h) = (width as usize, height as usize);
    let (uw, uh) = match transform {
        Transform::Normal => return (rgba, width, height),
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => (h, w),
        _ => (w, h),
    };
    let mut out = vec![0u8; rgba.len()];
    for (i, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
        let (x, y) = (i % w, i / w);
        let (ux, uy) = match transform {
            Transform::_90 => (h - 1 - y, x),
            Transform::_180 => (w - 1 - x, h - 1 - y),
            Transform::_270 => (y, w - 1 - x),
            Transform::Flipped => (w - 1 - x, y),
            Transform::Flipped90 => (y, x),
            Transform::Flipped180 => (x, h - 1 - y),
            Transform::Flipped270 => (h - 1 - y, w - 1 - x),
            _ => (x, y),
        };
        out[(uy * uw + ux) * 4..][..4].copy_from_slice(px);
    }
    (out, uw as u32, uh as u32)
}

impl Compositor {
    /// Open a cursor session on the shared output, if a client is on the desktop,
    /// none is open, and the seat can hand over a pointer to open it with.
    pub fn start_cursor(&mut self, desk: usize) {
        if self.desks[desk].client.is_none() || self.desks[desk].cursor.session.is_some() || !self.seat_had_pointer || self.desks[desk].cursor.stopped {
            return;
        }
        let (Some(sources), Some(manager), Some(seat)) = (&self.capture_sources, &self.image_copy, &self.seat) else { return };
        let Some(output) = self.outputs.selected(desk) else { return };
        let pointer = seat.get_pointer(&self.qh, ());
        let source = sources.create_source(&output.output, &self.qh, ());
        let cursor = manager.create_pointer_cursor_session(&source, &pointer, &self.qh, ());
        let capture = cursor.get_capture_session(&self.qh, ());
        debug!("capturing the cursor of output {}", output.name.as_deref().unwrap_or_default());
        self.desks[desk].cursor.session = Some(Session {
            pointer,
            source,
            cursor,
            capture,
            frame: None,
            buffer: None,
            size: (0, 0),
            formats: Vec::new(),
            constraints: None,
            entered: false,
            hotspot: (0, 0),
            transform: Transform::Normal,
            image: None,
        });
    }

    /// Close the cursor session, if one is open: nobody is watching, or the
    /// output it watches is no longer the shared one. The clients are told there
    /// is no pointer until a session says otherwise.
    pub fn stop_cursor(&mut self, desk: usize) {
        if let Some(token) = self.desks[desk].cursor.retry.take() {
            self.handle.remove(token);
        }
        if self.desks[desk].cursor.session.take().is_some() {
            self.shared().set_cursor(desk, None);
        }
    }

    /// Publish what a client should draw: the last image, while the cursor is on
    /// the output.
    fn show_cursor(&self, desk: usize) {
        let image = self.desks[desk].cursor.session.as_ref().filter(|s| s.entered).and_then(|s| s.image.clone());
        self.shared().set_cursor(desk, image);
    }

    /// Ask for the next cursor frame, if none is in flight and the session has
    /// said what to capture it into.
    fn capture_cursor(&mut self, desk: usize) {
        let cursor = &mut self.desks[desk].cursor;
        let Some(session) = &mut cursor.session else { return };
        if session.frame.is_some() || cursor.retry.is_some() {
            return;
        }
        let Some((width, height, format)) = session.constraints else { return };
        let stride = width * 4;
        if !session.buffer.as_ref().is_some_and(|b| b.matches(width, height, stride, format)) {
            match ShmBuffer::new(&self.shm, &self.qh, width, height, stride, format) {
                Ok(b) => {
                    debug!("cursor buffer {width}x{height}, {format:?}");
                    session.buffer = Some(b);
                }
                Err(e) => {
                    error!("cannot allocate a {width}x{height} cursor buffer: {e}");
                    return;
                }
            }
        }
        let buffer = session.buffer.as_ref().expect("allocated above");
        let frame = session.capture.create_frame(&self.qh, ());
        frame.attach_buffer(&buffer.buffer);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        session.frame = Some(frame);
        session.transform = Transform::Normal;
    }

    /// Ask for the next cursor frame after [`RETRY`].
    fn capture_cursor_later(&mut self, desk: usize) {
        if self.desks[desk].cursor.retry.is_some() {
            return;
        }
        match self.handle.insert_source(Timer::from_duration(RETRY), move |_, _, state: &mut Compositor| {
            state.desks[desk].cursor.retry = None;
            state.capture_cursor(desk);
            TimeoutAction::Drop
        }) {
            Ok(token) => self.desks[desk].cursor.retry = Some(token),
            Err(e) => error!("cannot schedule a cursor capture: {e}"),
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for Compositor {
    fn event(state: &mut Self, capture: &ExtImageCopyCaptureSessionV1, event: ext_image_copy_capture_session_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(desk) = state.desks.iter().position(|d| d.cursor.session.as_ref().is_some_and(|s| &s.capture == capture)) else { return };
        let session = state.desks[desk].cursor.session.as_mut().expect("found above");
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => session.size = (width, height),
            ext_image_copy_capture_session_v1::Event::ShmFormat { format: WEnum::Value(format) } => session.formats.push(format),
            ext_image_copy_capture_session_v1::Event::Done => {
                let (width, height) = session.size;
                let formats = std::mem::take(&mut session.formats);
                // wlroots announces the source before its cursor has a buffer
                // at all — no size and no format — and again once it has one.
                if width == 0 || height == 0 || formats.is_empty() {
                    session.constraints = None;
                    return;
                }
                // The four with alpha are equally good; the first wlroots
                // offers is the one it reads back without a conversion.
                let Some(&format) = formats.iter().find(|&&f| rgba_bytes(f).is_some()) else {
                    error!("the compositor offers the cursor only as {formats:?}; this server needs a 32-bit format with alpha: ARGB8888, ABGR8888, RGBA8888 or BGRA8888");
                    session.constraints = None;
                    return;
                };
                session.constraints = Some((width, height, format));
                state.capture_cursor(desk);
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                // The source is gone, which is the output going: sharing the
                // next one opens a session on that.
                debug!("the cursor session stopped");
                state.stop_cursor(desk);
                state.desks[desk].cursor.stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for Compositor {
    fn event(state: &mut Self, frame: &ExtImageCopyCaptureFrameV1, event: ext_image_copy_capture_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(desk) = state.desks.iter().position(|d| d.cursor.session.as_ref().is_some_and(|s| s.frame.as_ref() == Some(frame))) else { return };
        let session = state.desks[desk].cursor.session.as_mut().expect("found above");
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform { transform: WEnum::Value(t) } => session.transform = t,
            ext_image_copy_capture_frame_v1::Event::Ready => {
                frame.destroy();
                session.frame = None;
                let (Some(buffer), Some((.., format))) = (&session.buffer, session.constraints) else { return };
                let order = rgba_bytes(format).expect("only formats with alpha are chosen");
                let rgba = to_rgba(&buffer.map, buffer.width, buffer.height, buffer.stride, order);
                let (rgba, width, height) = upright(rgba, buffer.width, buffer.height, session.transform);
                // The hotspot is not turned with the pixels. The protocol puts
                // it in buffer coordinates, but wlroots reports the one its
                // output cursor was given, which is upright already.
                let image = CursorImage::cropped(width as u16, height as u16, session.hotspot, &rgba);
                match &image {
                    Some(i) => debug!("cursor {}x{}, hotspot {:?}", i.width(), i.height(), i.hotspot()),
                    None => debug!("the cursor paints nothing"),
                }
                session.image = image.map(Arc::new);
                state.show_cursor(desk);
                state.capture_cursor(desk);
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                frame.destroy();
                session.frame = None;
                match reason {
                    // The cursor changed size, and the session has announced
                    // the new one ahead of this failure.
                    WEnum::Value(FailureReason::BufferConstraints) => state.capture_cursor(desk),
                    reason => {
                        debug!("a cursor frame failed ({reason:?}); asking again in {RETRY:?}");
                        state.capture_cursor_later(desk);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for Compositor {
    fn event(state: &mut Self, cursor: &ExtImageCopyCaptureCursorSessionV1, event: ext_image_copy_capture_cursor_session_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(desk) = state.desks.iter().position(|d| d.cursor.session.as_ref().is_some_and(|s| &s.cursor == cursor)) else { return };
        let session = state.desks[desk].cursor.session.as_mut().expect("found above");
        match event {
            ext_image_copy_capture_cursor_session_v1::Event::Enter => {
                session.entered = true;
                state.show_cursor(desk);
            }
            ext_image_copy_capture_cursor_session_v1::Event::Leave => {
                session.entered = false;
                state.show_cursor(desk);
            }
            ext_image_copy_capture_cursor_session_v1::Event::Hotspot { x, y } => session.hotspot = (x, y),
            // The client places the pointer where it sent it.
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(Compositor: ignore WlPointer);
wayland_client::delegate_noop!(Compositor: ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(Compositor: ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(Compositor: ExtImageCopyCaptureManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_alpha_format_is_read_as_rgba_and_the_row_padding_is_skipped() {
        // One pixel, R=1 G=2 B=3 A=4, in each format's memory order, followed by
        // four bytes of stride padding that must not be read.
        let cases = [
            (wl_shm::Format::Argb8888, [3, 2, 1, 4]),
            (wl_shm::Format::Abgr8888, [1, 2, 3, 4]),
            (wl_shm::Format::Rgba8888, [4, 3, 2, 1]),
            (wl_shm::Format::Bgra8888, [4, 1, 2, 3]),
        ];
        for (format, memory) in cases {
            let mut map = memory.to_vec();
            map.extend_from_slice(&[9; 4]);
            map.extend_from_slice(&memory);
            map.extend_from_slice(&[9; 4]);
            let order = rgba_bytes(format).unwrap();
            assert_eq!(to_rgba(&map, 1, 2, 8, order), vec![1, 2, 3, 4, 1, 2, 3, 4], "{format:?}");
        }
        // A cursor without alpha is no cursor.
        assert_eq!(rgba_bytes(wl_shm::Format::Xrgb8888), None);
    }

    #[test]
    fn every_transform_is_undone() {
        // The upright image is
        //
        //     a b c
        //     d e f
        //
        // and each buffer below is it drawn by hand as wl_output.transform
        // describes: turned counter-clockwise, flipped about the vertical axis
        // first for the flipped ones.
        let px = |s: &str| s.split_whitespace().flat_map(|c| [c.as_bytes()[0]; 4]).collect::<Vec<u8>>();
        let cases = [
            (Transform::Normal, (3, 2), "a b c d e f"),
            (Transform::_90, (2, 3), "c f b e a d"),
            (Transform::_180, (3, 2), "f e d c b a"),
            (Transform::_270, (2, 3), "d a e b f c"),
            (Transform::Flipped, (3, 2), "c b a f e d"),
            (Transform::Flipped90, (2, 3), "a d b e c f"),
            (Transform::Flipped180, (3, 2), "d e f a b c"),
            (Transform::Flipped270, (2, 3), "f c e b d a"),
        ];
        for (transform, (w, h), buffer) in cases {
            assert_eq!(upright(px(buffer), w, h, transform), (px("a b c d e f"), 3, 2), "{transform:?}");
        }
    }
}
