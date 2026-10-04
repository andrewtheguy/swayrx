//! What the compositor thread and the client sessions share, and the two
//! channels between them.
//!
//! The compositor side is one thread that owns the Wayland connection
//! ([`crate::compositor`]); clients are tokio tasks ([`crate::session`]). They
//! meet in three places, all here: the [`Framebuffer`] behind a mutex, a
//! `watch` that ticks when it changes, and a broadcast of [`Event`]s for what a
//! frame cannot carry. Sessions talk back through [`Command`]s on a calloop
//! channel the compositor thread polls beside the Wayland socket.
//!
//! There are two of everything an output is shared through, a [`Desk`] each:
//! the client on the desktop has the first, and a connection that joined it to
//! show another output beside has the second ([`BESIDE`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};

use wlshare_rfb::cursor::CursorImage;
use wlshare_rfb::outputs::OutputEntry;

use crate::framebuffer::Framebuffer;

/// One connection, numbered from 1 for the life of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u64);

/// The desk of the client on the desktop.
pub const FIRST: usize = 0;
/// The desk of the connection showing another output beside it.
pub const BESIDE: usize = 1;

/// What the compositor thread is asked to do.
#[derive(Debug)]
pub enum Command {
    /// A client finished the handshake and takes the desktop: capture runs
    /// while it is on it, and a client already there is superseded, RFB's
    /// ClientInit shared flag notwithstanding. Sent before the client's
    /// ServerInit, which may wait on what the desktop being held brings about.
    ClientJoined(ClientId),
    /// A client finished the handshake asking to show another output beside the
    /// client on the desktop, which stays. It is told the outcome through
    /// [`Shared::seats`]: with nobody on the desktop, or no output that client
    /// is not on, it is ended.
    BesideJoined(ClientId),
    /// A client left: its input is let go and its capture stops, and a display
    /// beside it ends with it. Sent for every connection, joined or not, and
    /// ignored for one already superseded.
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
    /// A desk's output changed scale or size, or a declaration was answered: send
    /// an OutputScale to every density client on that desk (`to` is `None`) or to
    /// one of them.
    Geometry { desk: usize, to: Option<ClientId> },
    /// A client's SetDesktopSize was refused with an ExtendedDesktopSize status.
    ResizeRefused { client: ClientId, status: u16 },
    /// The outputs, or which of them is shared, changed — or a client's
    /// SelectOutput was answered: send the list to every client that asked for
    /// it, each with its own desk's output as the shared one. Not addressed to
    /// one client the way a geometry answer is, because a switch is the whole
    /// desktop's news.
    Outputs,
    /// The compositor's clipboard changed: notify every client of
    /// [`Shared::clipboard`]. The text is read when a client asks for it rather
    /// than carried here, so it is the latest when it goes.
    Clipboard,
    /// A desk's cursor image changed: send [`Desk::cursor`] to the client on it.
    /// The image is read when it is sent rather than carried here, so a client
    /// behind on a cursor that changes quickly is sent the latest shape once.
    Cursor { desk: usize },
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

/// The compositor's outputs as the sessions list them, and which one each desk
/// shares. Beside [`Geometry`] and for the same reason: a session sends it from
/// its own task, and the compositor thread keeps it current.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Displays {
    /// Each desk's shared output's id, or 0 while it has none.
    pub active: [u32; 2],
    pub entries: Vec<OutputEntry>,
}

impl Displays {
    /// Whether a desk's shared output is the one of this name.
    pub fn shares(&self, desk: usize, name: &str) -> bool {
        self.entries.iter().any(|entry| entry.id == self.active[desk] && entry.name == name)
    }
}

/// Who is where. A `watch` and not an [`Event`]: a session too far behind to
/// read the broadcast would miss being superseded, and this it cannot miss. Ids
/// come from [`Shared::next_client`] and so only go up, which is what lets a
/// session read one value and know where it stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seats {
    /// The client on the desktop, or 0 for nobody: anything above a session's
    /// own id is a client that joined after it.
    pub holder: u64,
    /// The connection showing another output beside it, or 0 for none.
    pub beside: u64,
    /// Every connection that asked to be beside with an id up to this one has
    /// ended, or was refused.
    pub beside_ended: u64,
}

/// One shared output as the sessions on it see it.
pub struct Desk {
    pub framebuffer: Mutex<Framebuffer>,
    /// Ticks with [`Framebuffer::generation`] on every change.
    pub frame_tx: watch::Sender<u64>,
    pub geometry: Mutex<Geometry>,
    /// The compositor's cursor image on the desk's output, or `None` while there
    /// is no pointer to draw there.
    cursor: Mutex<Option<Arc<CursorImage>>>,
}

impl Desk {
    fn new(framebuffer: Framebuffer, geometry: Geometry) -> Self {
        let (frame_tx, _) = watch::channel(framebuffer.generation);
        Self { framebuffer: Mutex::new(framebuffer), frame_tx, geometry: Mutex::new(geometry), cursor: Mutex::new(None) }
    }

    pub fn geometry(&self) -> Geometry {
        *self.geometry.lock().unwrap()
    }

    pub fn cursor(&self) -> Option<Arc<CursorImage>> {
        self.cursor.lock().unwrap().clone()
    }

    /// Tell the sessions the framebuffer changed.
    pub fn frame_changed(&self, generation: u64) {
        self.frame_tx.send_replace(generation);
    }
}

pub struct Shared {
    /// [`FIRST`] and [`BESIDE`].
    pub desks: [Desk; 2],
    pub seats: watch::Sender<Seats>,
    pub displays: Mutex<Displays>,
    /// The compositor's clipboard as text, empty while it holds none — from the
    /// desktop's selection, or from the client that set it last.
    clipboard: Mutex<Arc<str>>,
    pub events: broadcast::Sender<Event>,
    pub commands: calloop::channel::Sender<Command>,
    next_client: AtomicU64,
}

impl Shared {
    pub fn new(framebuffer: Framebuffer, geometry: Geometry, displays: Displays, commands: calloop::channel::Sender<Command>) -> Self {
        let (seats, _) = watch::channel(Seats::default());
        let (events, _) = broadcast::channel(64);
        // The second desk holds nothing until a connection joins beside: it
        // takes its output's size then.
        let beside = Desk::new(Framebuffer::new(1, 1), geometry);
        Self {
            desks: [Desk::new(framebuffer, geometry), beside],
            seats,
            displays: Mutex::new(displays),
            clipboard: Mutex::new(Arc::from("")),
            events,
            commands,
            next_client: AtomicU64::new(1),
        }
    }

    pub fn next_client(&self) -> ClientId {
        ClientId(self.next_client.fetch_add(1, Ordering::Relaxed))
    }

    pub fn displays(&self) -> Displays {
        self.displays.lock().unwrap().clone()
    }

    /// Replace a desk's cursor image; tell the sessions if it changed.
    pub fn set_cursor(&self, desk: usize, image: Option<Arc<CursorImage>>) {
        {
            let mut cursor = self.desks[desk].cursor.lock().unwrap();
            if *cursor == image {
                return;
            }
            *cursor = image;
        }
        self.emit(Event::Cursor { desk });
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

    /// Say who is on the desktop; the session that was ends.
    pub fn set_holder(&self, client: Option<ClientId>) {
        self.seats.send_modify(|seats| seats.holder = client.map_or(0, |c| c.0));
    }

    /// Say which connection shows an output beside the client on the desktop.
    pub fn set_beside(&self, client: ClientId) {
        self.seats.send_modify(|seats| seats.beside = client.0);
    }

    /// End the connection that is, or asked to be, beside; its session ends.
    pub fn end_beside(&self, client: ClientId) {
        self.seats.send_modify(|seats| {
            seats.beside_ended = seats.beside_ended.max(client.0);
            if seats.beside == client.0 {
                seats.beside = 0;
            }
        });
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
