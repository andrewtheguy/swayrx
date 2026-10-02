//! What the compositor thread and the client sessions share, and the two
//! channels between them.
//!
//! The compositor side is one thread that owns the Wayland connection
//! ([`crate::compositor`]); clients are tokio tasks ([`crate::session`]). They
//! meet in three places, all here: the [`Framebuffer`] behind a mutex, a
//! `watch` that ticks when it changes, and a broadcast of [`Event`]s for what a
//! frame cannot carry. Sessions talk back through [`Command`]s on a calloop
//! channel the compositor thread polls beside the Wayland socket.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};

use wlshare_rfb::cursor::CursorImage;
use wlshare_rfb::outputs::OutputEntry;

use crate::framebuffer::Framebuffer;

/// One connection, numbered from 1 for the life of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u64);

/// What the compositor thread is asked to do.
#[derive(Debug)]
pub enum Command {
    /// A client finished the handshake and takes the desktop: capture runs
    /// while it is on it, and a client already there is superseded, RFB's
    /// ClientInit shared flag notwithstanding.
    ClientJoined(ClientId),
    /// A client left: its input is let go and capture stops. Sent for every
    /// connection, joined or not, and ignored for one already superseded.
    ClientLeft(ClientId),
    Key { client: ClientId, keysym: u32, down: bool },
    Pointer { client: ClientId, buttons: u8, x: u16, y: u16 },
    /// Scroll: the client scrolls by this distance in logical pixels.
    Scroll { client: ClientId, dx: i16, dy: i16 },
    /// SetDesktopSize: the client wants the desktop `width`×`height` pixels.
    Resize { client: ClientId, width: u16, height: u16 },
    /// ClientDensity: the client wants the output `width`×`height` pixels drawn
    /// at `scale`, in one configuration.
    Declare { client: ClientId, width: u16, height: u16, scale: f64 },
    /// SelectOutput: the client wants the output with this id shared.
    SelectOutput { client: ClientId, id: u32 },
    /// Text for the compositor's clipboard.
    SetClipboard { client: ClientId, text: String },
}

/// What the compositor thread tells the sessions, beyond the framebuffer.
#[derive(Debug, Clone)]
pub enum Event {
    /// The output's scale or size changed, or a declaration was answered: send an
    /// OutputScale to every density client (`to` is `None`) or to one of them.
    Geometry { to: Option<ClientId> },
    /// A client's SetDesktopSize was refused with an ExtendedDesktopSize status.
    ResizeRefused { client: ClientId, status: u16 },
    /// The outputs, or which of them is shared, changed — or a client's
    /// SelectOutput was answered: send the list to every client that asked for
    /// it. Not addressed to one client the way a geometry answer is, because a
    /// switch is the whole desktop's news and there is one client on it.
    Outputs,
    /// The compositor's clipboard changed: notify every client of
    /// [`Shared::clipboard`]. The text is read when a client asks for it rather
    /// than carried here, so it is the latest when it goes.
    Clipboard,
    /// The cursor image changed: send [`Shared::cursor`] to every client. The
    /// image is read when it is sent rather than carried here, so a client
    /// behind on a cursor that changes quickly is sent the latest shape once.
    Cursor,
}

/// The captured output as the sessions describe it to clients: the framebuffer's
/// size in pixels and the scale it is drawn at. Kept apart from the framebuffer
/// because a report can precede the frame that carries the size it names.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub width: u16,
    pub height: u16,
    pub scale: f64,
}

/// The compositor's outputs as the sessions list them, and which one is shared.
/// Beside [`Geometry`] and for the same reason: a session sends it from its own
/// task, and the compositor thread keeps it current.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Displays {
    /// The shared output's id, or 0 while there is none.
    pub active: u32,
    pub entries: Vec<OutputEntry>,
}

pub struct Shared {
    pub framebuffer: Mutex<Framebuffer>,
    /// Ticks with [`Framebuffer::generation`] on every change.
    pub frame_tx: watch::Sender<u64>,
    /// The client on the desktop, or 0 for nobody. A `watch` and not an
    /// [`Event`]: a session too far behind to read the broadcast would miss
    /// being superseded, and this it cannot miss. Ids come from
    /// [`Shared::next_client`] and so only go up, which is what lets a session
    /// read one value and know where it stands: anything above its own id is a
    /// client that joined after it.
    pub active: watch::Sender<u64>,
    pub geometry: Mutex<Geometry>,
    pub displays: Mutex<Displays>,
    /// The compositor's cursor image on the shared output, or `None` while there
    /// is no pointer to draw there.
    cursor: Mutex<Option<Arc<CursorImage>>>,
    /// The compositor's clipboard as text, empty while it holds none — from the
    /// desktop's selection, or from the client that set it last.
    clipboard: Mutex<Arc<str>>,
    pub events: broadcast::Sender<Event>,
    pub commands: calloop::channel::Sender<Command>,
    next_client: AtomicU64,
}

impl Shared {
    pub fn new(framebuffer: Framebuffer, geometry: Geometry, displays: Displays, commands: calloop::channel::Sender<Command>) -> Self {
        let (frame_tx, _) = watch::channel(framebuffer.generation);
        let (active, _) = watch::channel(0);
        let (events, _) = broadcast::channel(64);
        Self {
            framebuffer: Mutex::new(framebuffer),
            frame_tx,
            active,
            geometry: Mutex::new(geometry),
            displays: Mutex::new(displays),
            cursor: Mutex::new(None),
            clipboard: Mutex::new(Arc::from("")),
            events,
            commands,
            next_client: AtomicU64::new(1),
        }
    }

    pub fn next_client(&self) -> ClientId {
        ClientId(self.next_client.fetch_add(1, Ordering::Relaxed))
    }

    pub fn geometry(&self) -> Geometry {
        *self.geometry.lock().unwrap()
    }

    pub fn displays(&self) -> Displays {
        self.displays.lock().unwrap().clone()
    }

    pub fn cursor(&self) -> Option<Arc<CursorImage>> {
        self.cursor.lock().unwrap().clone()
    }

    /// Replace the cursor image; tell the sessions if it changed.
    pub fn set_cursor(&self, image: Option<Arc<CursorImage>>) {
        {
            let mut cursor = self.cursor.lock().unwrap();
            if *cursor == image {
                return;
            }
            *cursor = image;
        }
        self.emit(Event::Cursor);
    }

    pub fn clipboard(&self) -> Arc<str> {
        self.clipboard.lock().unwrap().clone()
    }

    /// Replace the clipboard's text. `announce` tells the sessions, which a
    /// selection the desktop made does and one a client made does not: that
    /// client has it already, and no other client is on the desktop.
    pub fn set_clipboard(&self, text: Arc<str>, announce: bool) {
        *self.clipboard.lock().unwrap() = text;
        if announce {
            self.emit(Event::Clipboard);
        }
    }

    /// Say who is on the desktop; every other session ends.
    pub fn set_active(&self, client: Option<ClientId>) {
        self.active.send_replace(client.map_or(0, |c| c.0));
    }

    /// Tell the sessions the framebuffer changed.
    pub fn frame_changed(&self, generation: u64) {
        self.frame_tx.send_replace(generation);
    }

    pub fn emit(&self, event: Event) {
        // No receivers is not an error: nobody is connected.
        let _ = self.events.send(event);
    }

    pub fn command(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::error!("the compositor thread is gone");
        }
    }
}
