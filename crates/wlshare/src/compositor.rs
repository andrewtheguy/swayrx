//! The thread that owns the Wayland connection.
//!
//! Everything the compositor is asked or tells happens here, on one calloop:
//! the Wayland socket, the command channel from the sessions, and the capture
//! timer. Sessions never touch a Wayland object; they send [`Command`]s and read
//! the [`Shared`] state and [`Event`]s this thread produces.

use std::sync::Arc;

use anyhow::Context as _;
use calloop::channel;
use calloop::{EventLoop, LoopHandle};
use calloop_wayland_source::WaylandSource;
use log::{debug, info, warn};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;

use crate::capture::Capture;
use crate::clipboard::Clipboard;
use crate::config::{Config, Xkb};
use crate::cursor::CursorCapture;
use crate::framebuffer::Framebuffer;
use crate::input::Input;
use crate::framebuffer::ResizeOrigin;
use crate::outputs::{ConfigKind, OutputInfo, Outputs};
use crate::shared::{BESIDE, ClientId, Command, Displays, Event, FIRST, Geometry, Shared};

/// What the compositor thread keeps for one desk: who is on it, and what
/// shares its output with them.
#[derive(Default)]
pub struct DeskState {
    /// The client on this desk. The first desk's is the one on the desktop: a
    /// connection that finishes the handshake takes it from whoever holds it.
    pub client: Option<ClientId>,
    pub capture: Capture,
    pub cursor: CursorCapture,
    /// The client's SetDesktopSize the compositor accepted, until the frame at
    /// that size arrives.
    pub pending_resize: Option<(ClientId, u16, u16)>,
    /// A declaration that arrived while another's configuration was out, run
    /// once that one settles: client, width, height, scale.
    queued_declaration: Option<(ClientId, u16, u16, f64)>,
}

pub struct Compositor {
    shared: Option<Arc<Shared>>,
    pub qh: QueueHandle<Compositor>,
    pub handle: LoopHandle<'static, Compositor>,
    registry: WlRegistry,
    pub shm: WlShm,
    pub seat: Option<WlSeat>,
    keyboards: Option<ZwpVirtualKeyboardManagerV1>,
    pointers: Option<ZwlrVirtualPointerManagerV1>,
    pub outputs: Outputs,
    pub screencopy: Option<ZwlrScreencopyManagerV1>,
    pub capture_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub image_copy: Option<ExtImageCopyCaptureManagerV1>,
    /// The seat has named a pointer among its capabilities at least once, which
    /// is what makes `wl_seat.get_pointer` legal.
    pub seat_had_pointer: bool,
    /// [`FIRST`] and [`BESIDE`].
    pub desks: [DeskState; 2],
    input: Option<Input>,
    pub clipboard: Clipboard,
    pub max_fps: u32,
    resize_allowed: bool,
    xkb: Xkb,
    /// The connection and queue, held between `connect` and `discover`.
    pending_queue: Option<(Connection, wayland_client::EventQueue<Compositor>)>,
    exit: Option<anyhow::Result<()>>,
}

/// The running compositor thread, as the main thread sees it.
pub struct Handle {
    done: tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
}

impl Handle {
    /// Resolves when the thread ends: `Ok` for a compositor that closed the
    /// connection, `Err` for anything that went wrong.
    pub async fn exited(&mut self) -> anyhow::Result<()> {
        match (&mut self.done).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("the compositor thread ended without a word")),
        }
    }
}

/// Connect to the compositor, learn the outputs, and run the thread. Returns
/// once the shared state exists, so the listener can announce a framebuffer.
pub fn start(config: &Config) -> anyhow::Result<(Handle, Arc<Shared>)> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<Arc<Shared>>>();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let output = config.output.clone();
    let (max_fps, resize, xkb) = (config.max_fps, config.resize, config.xkb.clone());
    std::thread::Builder::new()
        .name("compositor".into())
        .spawn(move || {
            let result = run(output.as_deref(), max_fps, resize, xkb, ready_tx);
            let _ = done_tx.send(result);
        })
        .context("spawning the compositor thread")?;
    let shared = ready_rx.recv().map_err(|_| anyhow::anyhow!("the compositor thread died during startup"))??;
    Ok((Handle { done: done_rx }, shared))
}

fn run(
    output: Option<&str>,
    max_fps: u32,
    resize: bool,
    xkb: Xkb,
    ready: std::sync::mpsc::Sender<anyhow::Result<Arc<Shared>>>,
) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<'static, Compositor> = EventLoop::try_new().context("creating the event loop")?;
    let (commands_tx, commands_rx) = channel::channel::<Command>();
    let mut compositor = match connect(event_loop.handle(), max_fps, resize, xkb) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    let queue = match compositor.discover(output, commands_tx) {
        Ok(queue) => queue,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    let shared = compositor.shared().clone();

    WaylandSource::new(queue.0, queue.1)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("inserting the Wayland source: {e}"))?;
    event_loop
        .handle()
        .insert_source(commands_rx, |event, _, state: &mut Compositor| match event {
            channel::Event::Msg(command) => state.handle_command(command),
            channel::Event::Closed => state.exit = Some(Err(anyhow::anyhow!("the command channel closed"))),
        })
        .map_err(|e| anyhow::anyhow!("inserting the command channel: {e}"))?;
    let _ = ready.send(Ok(shared));

    let signal = event_loop.get_signal();
    event_loop
        .run(None, &mut compositor, |state| {
            if state.exit.is_some() {
                signal.stop();
            }
        })
        .context("the Wayland event loop")?;
    compositor.exit.take().unwrap_or(Ok(()))
}

/// Connect and bind the globals. The queue is returned separately because the
/// registry roundtrip needs `&mut Compositor` beside it.
fn connect(handle: LoopHandle<'static, Compositor>, max_fps: u32, resize: bool, xkb: Xkb) -> anyhow::Result<Compositor> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;
    let (globals, queue) = registry_queue_init::<Compositor>(&conn).context("reading the registry")?;
    let qh = queue.handle();

    let shm: WlShm = globals.bind(&qh, 1..=1, ()).context("wl_shm")?;
    let seat: Option<WlSeat> = globals.bind(&qh, 1..=7, ()).ok();
    let screencopy: Option<ZwlrScreencopyManagerV1> = globals.bind(&qh, 2..=3, ()).ok();
    let output_manager: Option<ZwlrOutputManagerV1> = globals.bind(&qh, 1..=4, ()).ok();
    let keyboards: Option<ZwpVirtualKeyboardManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let pointers: Option<ZwlrVirtualPointerManagerV1> = globals.bind(&qh, 2..=2, ()).ok();
    let data_control: Option<ZwlrDataControlManagerV1> = globals.bind(&qh, 1..=2, ()).ok();
    let capture_sources: Option<ExtOutputImageCaptureSourceManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let image_copy: Option<ExtImageCopyCaptureManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    anyhow::ensure!(screencopy.is_some(), "the compositor does not offer wlr-screencopy version 2 or later");
    anyhow::ensure!(
        capture_sources.is_some() && image_copy.is_some(),
        "the compositor does not offer ext-image-copy-capture with output capture sources, which the cursor image is taken from (wlroots 0.19 or later)"
    );
    if seat.is_none() {
        warn!("no seat: the cursor image cannot be captured, and clients draw their own");
    }
    if output_manager.is_none() {
        warn!("no wlr-output-management: the output cannot be resized or rescaled, and its scale is read from wl_output only");
    }
    if keyboards.is_none() || pointers.is_none() {
        warn!("no virtual keyboard or pointer protocol: input will be dropped");
    }
    if data_control.is_none() {
        warn!("no wlr-data-control: the clipboard is not shared");
    }

    let mut compositor = Compositor {
        shared: None,
        qh: qh.clone(),
        handle,
        registry: globals.registry().clone(),
        shm,
        seat,
        keyboards,
        pointers,
        outputs: Outputs::default(),
        screencopy,
        capture_sources,
        image_copy,
        seat_had_pointer: false,
        desks: Default::default(),
        input: None,
        clipboard: Clipboard::default(),
        max_fps,
        resize_allowed: resize,
        xkb,
        pending_queue: None,
        exit: None,
    };
    compositor.outputs.manager = output_manager;
    compositor.clipboard.manager = data_control;
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            compositor.bind_output(global.name, global.version);
        }
    }
    compositor.pending_queue = Some((conn, queue));
    Ok(compositor)
}

impl Compositor {
    pub fn shared(&self) -> &Arc<Shared> {
        self.shared.as_ref().expect("the shared state exists once discovery is done")
    }

    fn bind_output(&mut self, name: u32, version: u32) {
        if version < 4 {
            warn!("wl_output {name} is version {version}; version 4 is needed to learn its name");
        }
        let output: WlOutput = self.registry.bind(name, version.min(4), &self.qh, ());
        self.outputs.outputs.push(OutputInfo {
            output,
            global: name,
            name: None,
            mode: (0, 0),
            transform: wayland_client::protocol::wl_output::Transform::Normal,
            wl_scale: 1,
            done: false,
        });
    }

    /// Learn the outputs, pick one, and build the shared state.
    fn discover(&mut self, wanted: Option<&str>, commands: channel::Sender<Command>) -> anyhow::Result<(Connection, wayland_client::EventQueue<Compositor>)> {
        let (conn, mut queue) = self.pending_queue.take().expect("the queue from connect");
        // Two round trips: the first delivers the outputs and heads, the second the
        // events of the mode objects the heads created.
        queue.roundtrip(self).context("learning the outputs")?;
        queue.roundtrip(self).context("learning the outputs")?;
        self.outputs.select(wanted)?;
        let (width, height) = self.outputs.size(FIRST);
        anyhow::ensure!(width > 0 && height > 0, "the shared output has no mode");
        let geometry = Geometry { width, height, scale: self.outputs.scale(FIRST) };
        let displays = Displays { active: [self.outputs.active_id(FIRST), 0], entries: self.outputs.entries() };
        let shared = Arc::new(Shared::new(Framebuffer::new(width, height), geometry, displays, commands));
        self.shared = Some(shared);

        if let (Some(seat), Some(keyboards), Some(pointers)) = (&self.seat, &self.keyboards, &self.pointers) {
            let output = self.outputs.selected(FIRST).expect("selected").output.clone();
            let mut input = Input::new(&self.qh, keyboards, seat, &self.xkb)?;
            input.point(FIRST, &self.qh, pointers, seat, &output);
            self.input = Some(input);
        }
        if let Some(seat) = &self.seat {
            self.clipboard.attach(&self.qh, seat);
        }
        Ok((conn, queue))
    }

    /// The desk a client is on, or `None` for one that never joined or was
    /// superseded: its last messages arrive after it lost its desk.
    pub fn desk_of(&self, client: ClientId) -> Option<usize> {
        self.desks.iter().position(|desk| desk.client == Some(client))
    }

    /// Refresh a desk's shared geometry from the outputs; tell the sessions if
    /// it changed.
    pub fn geometry_changed(&mut self, desk: usize) {
        if self.refresh_geometry(desk) {
            self.shared().emit(Event::Geometry { desk, to: None });
        }
    }

    /// Refresh a desk's shared geometry and report it whether or not it
    /// changed: a declaration is owed an answer.
    pub fn answer_geometry(&mut self, desk: usize, to: Option<ClientId>) {
        self.refresh_geometry(desk);
        self.shared().emit(Event::Geometry { desk, to });
    }

    /// Refresh the list of outputs; tell the sessions if it changed. A no-op
    /// during discovery, when there is nobody to tell yet.
    pub fn outputs_changed(&mut self) {
        if self.shared.is_none() {
            return;
        }
        if self.refresh_displays() {
            self.shared().emit(Event::Outputs);
        }
    }

    /// Refresh the list and report it whether or not it changed: a SelectOutput
    /// is owed an answer, and one that changes nothing is answered with the list
    /// as it is.
    fn answer_outputs(&mut self) {
        self.refresh_displays();
        self.shared().emit(Event::Outputs);
    }

    fn refresh_displays(&mut self) -> bool {
        let now = Displays { active: [self.outputs.active_id(FIRST), self.outputs.active_id(BESIDE)], entries: self.outputs.entries() };
        let mut displays = self.shared().displays.lock().unwrap();
        if *displays == now {
            return false;
        }
        *displays = now;
        true
    }

    fn refresh_geometry(&mut self, desk: usize) -> bool {
        let (width, height) = self.outputs.size(desk);
        let scale = self.outputs.scale(desk);
        let now = Geometry { width, height, scale };
        let mut g = self.shared().desks[desk].geometry.lock().unwrap();
        if *g == now {
            return false;
        }
        info!("output geometry: {width}x{height} at scale {scale:.2}");
        *g = now;
        true
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::ClientJoined(id) => {
                if let Some(previous) = self.desks[FIRST].client.replace(id) {
                    info!("client {} takes the desktop from client {}", id.0, previous.0);
                    self.let_go(FIRST, previous);
                    // A display beside the client that held it was that client's.
                    self.end_beside();
                }
                self.shared().set_holder(Some(id));
                self.start_capture(FIRST);
            }
            Command::BesideJoined(id) => self.join_beside(id),
            Command::ClientLeft(id) => {
                // A connection that never joined, or one already superseded,
                // holds nothing.
                match self.desk_of(id) {
                    Some(FIRST) => {
                        self.end_beside();
                        self.desks[FIRST].client = None;
                        self.shared().set_holder(None);
                        self.let_go(FIRST, id);
                        self.stop_capture(FIRST);
                    }
                    Some(_) => self.end_beside(),
                    None => {}
                }
            }
            // Everything below is a desk's, so only the client on one is heard:
            // a superseded session's last messages arrive after the new client
            // has taken over.
            Command::Key { client, keysym, down } => {
                let Some(desk) = self.desk_of(client) else { return };
                if let Some(input) = &mut self.input {
                    input.key(desk, keysym, down);
                }
            }
            Command::Pointer { client, buttons, x, y } => {
                let Some(desk) = self.desk_of(client) else { return };
                let extent = {
                    let fb = self.shared().desks[desk].framebuffer.lock().unwrap();
                    (fb.width, fb.height)
                };
                if let Some(input) = &mut self.input {
                    input.pointer(desk, buttons, x, y, extent);
                }
            }
            Command::Scroll { client, dx, dy } => {
                let Some(desk) = self.desk_of(client) else { return };
                if let Some(input) = &mut self.input {
                    input.scroll(desk, dx, dy);
                }
            }
            Command::Resize { client, width, height } => {
                if let Some(desk) = self.desk_of(client) {
                    self.resize(desk, client, width, height);
                }
            }
            Command::Declare { client, width, height, scale } => {
                if let Some(desk) = self.desk_of(client) {
                    self.declare(desk, client, width, height, scale);
                }
            }
            // Which output is where is the choice of the client on the desktop:
            // a display beside is answered with the list as it is.
            Command::SelectOutput { client, id } => match self.desk_of(client) {
                Some(FIRST) => self.select_output(client, id),
                Some(_) => self.answer_outputs(),
                None => {}
            },
            // The clipboard is the desktop's, and so its one client's.
            Command::SetClipboard { client, text } => {
                if self.desks[FIRST].client == Some(client) {
                    let text: Arc<str> = Arc::from(text);
                    self.shared().set_clipboard(text.clone(), false);
                    self.clipboard.set(&self.qh.clone(), text);
                }
            }
        }
    }

    /// Let go of everything a client held, whether it left or was superseded.
    fn let_go(&mut self, desk: usize, id: ClientId) {
        let state = &mut self.desks[desk];
        if state.pending_resize.is_some_and(|(c, _, _)| c == id) {
            state.pending_resize = None;
        }
        state.queued_declaration = None;
        if let Some(input) = &mut self.input {
            input.release_all(desk);
        }
    }

    /// A connection asks to show another output beside the client on the
    /// desktop: the first the list shows that this client is not on. It takes
    /// the place of a connection already beside. With nobody on the desktop, or
    /// no other output, it is ended instead.
    ///
    /// Handshakes finish out of order, so the connection asking may be older
    /// than one already ended. Its session would end the moment it was seated,
    /// so it is refused, and whoever is beside stays. One older than the client
    /// on the desktop was opened beside whoever was there before, and a display
    /// beside is its client's: it is refused too.
    fn join_beside(&mut self, id: ClientId) {
        if self.shared().seats.borrow().beside_ended >= id.0 {
            info!("client {}: a later connection beside has already ended", id.0);
            return self.shared().end_beside(id);
        }
        let Some(holder) = self.desks[FIRST].client else {
            info!("client {}: nobody is on the desktop to be beside", id.0);
            return self.shared().end_beside(id);
        };
        if holder.0 > id.0 {
            info!("client {}: client {} took the desktop since it connected", id.0, holder.0);
            return self.shared().end_beside(id);
        }
        self.end_beside();
        let first = self.outputs.selected[FIRST].clone();
        let Some(entry) = self.outputs.entries().into_iter().find(|entry| Some(&entry.name) != first.as_ref()) else {
            info!("client {}: no other output to show beside", id.0);
            return self.shared().end_beside(id);
        };
        let Some(output) = self.outputs.selectable(entry.id) else { return self.shared().end_beside(id) };
        let wl_output = output.output.clone();
        info!("client {}: showing output {} beside: {}x{} pixels at scale {:.2}", id.0, entry.name, entry.width, entry.height, entry.scale);
        self.desks[BESIDE].client = Some(id);
        self.share_output(BESIDE, entry.name, entry.width, entry.height, &wl_output);
        // After the framebuffer took the output's size: the session reads it
        // for its ServerInit once it sees itself here.
        self.shared().set_beside(id);
    }

    /// End the display beside, if there is one: its client left, the client on
    /// the desktop left or took its output, or the output went away. Its
    /// session ends, and its framebuffer is given back.
    fn end_beside(&mut self) {
        let Some(id) = self.desks[BESIDE].client.take() else { return };
        info!("client {} is no longer beside", id.0);
        self.shared().end_beside(id);
        self.let_go(BESIDE, id);
        self.stop_capture(BESIDE);
        if let Some(input) = &mut self.input {
            input.unpoint(BESIDE);
        }
        self.desks[BESIDE].cursor.stopped = false;
        self.outputs.selected[BESIDE] = None;
        self.shared().desks[BESIDE].framebuffer.lock().unwrap().resize(1, 1, ResizeOrigin::Server);
        self.outputs_changed();
    }

    fn resize(&mut self, desk: usize, client: ClientId, width: u16, height: u16) {
        let refuse = |this: &Self, status: u16| this.shared().emit(Event::ResizeRefused { client, status });
        if !self.resize_allowed {
            info!("client {} asked for {width}x{height}: resizing is disabled", client.0);
            return refuse(self, wlshare_rfb::msg::EDS_STATUS_PROHIBITED);
        }
        if width == 0 || height == 0 {
            return refuse(self, wlshare_rfb::msg::EDS_STATUS_INVALID_LAYOUT);
        }
        info!("client {} asks for a {width}x{height} desktop", client.0);
        self.desks[desk].pending_resize = Some((client, width, height));
        let qh = self.qh.clone();
        if !self.outputs.configure(&qh, desk, Some((width, height)), None, ConfigKind::Resize { client }) {
            self.desks[desk].pending_resize = None;
            refuse(self, wlshare_rfb::msg::EDS_STATUS_PROHIBITED);
        }
    }

    /// Share another output: the client on the desktop named one from the list
    /// it was sent.
    ///
    /// The capture stops, the virtual pointer moves with it — absolute positions
    /// are against the new output's extent — and the framebuffer takes the new
    /// size blank, so the client is sent nothing until a frame of the output it
    /// asked for has arrived. A same-sized output is a repaint rather than a
    /// resize, and the client is told the new geometry either way, before the
    /// frame, as a scale change is.
    ///
    /// A request naming an output the compositor no longer has is answered with
    /// the list as it is and nothing else: the client's menu then agrees with
    /// what is on the canvas rather than with what was clicked.
    fn select_output(&mut self, client: ClientId, id: u32) {
        let Some(output) = self.outputs.selectable(id) else {
            warn!("client {}: no output with id {id} to share; the list stands", client.0);
            return self.answer_outputs();
        };
        let name = output.name.clone().expect("selectable");
        if self.outputs.selected[FIRST].as_deref() == Some(name.as_str()) {
            debug!("client {}: output {name} is already the shared one", client.0);
            return self.answer_outputs();
        }
        let (width, height) = self.outputs.size_of(output);
        let wl_output = output.output.clone();
        info!("client {}: sharing output {name}: {width}x{height} pixels at scale {:.2}", client.0, self.outputs.scale_of(output));
        self.share_output(FIRST, name, width, height, &wl_output);
    }

    /// Take an output as a desk's shared one and start the desk again on it.
    /// The whole sequence, whoever asked for it: a client naming one from its
    /// list, the compositor taking the one being shared away, or a connection
    /// joining beside.
    ///
    /// An output is on one desk: where the client on the desktop goes, a
    /// display beside that was showing it ends.
    fn share_output(&mut self, desk: usize, name: String, width: u16, height: u16, output: &WlOutput) {
        if desk == FIRST && self.outputs.selected[BESIDE].as_deref() == Some(name.as_str()) {
            self.end_beside();
        }
        self.stop_capture(desk);
        // A resize accepted for the output being left is not this one's, nor is
        // a cursor session the compositor stopped there.
        self.desks[desk].pending_resize = None;
        self.desks[desk].cursor.stopped = false;
        self.outputs.selected[desk] = Some(name);
        {
            let mut fb = self.shared().desks[desk].framebuffer.lock().unwrap();
            fb.resize(width, height, ResizeOrigin::Server);
        }
        self.retarget_input(desk, output);
        self.geometry_changed(desk);
        self.answer_outputs();
        self.start_capture(desk);
    }

    /// Share whatever output is left, there being none shared: the one that was
    /// went away, or the desktop has not had one yet. The list's own order
    /// decides, so a client lands on the output its menu shows first rather than
    /// on whichever the compositor happened to announce first.
    ///
    /// With nothing left to share the capture stops and the list goes out empty.
    /// The geometry and the framebuffer stand as they were: a desktop of no size
    /// is not an answer any client can use, and the last picture is at least the
    /// one the person was looking at. An output appearing later is adopted here.
    pub fn adopt_output(&mut self) {
        // Nothing to tell and nothing to capture until discovery is done.
        if self.shared.is_none() {
            return;
        }
        let Some(entry) = self.outputs.entries().into_iter().next() else {
            warn!("no output is left to share; the capture stops until one appears");
            self.stop_capture(FIRST);
            return self.answer_outputs();
        };
        let Some(output) = self.outputs.selectable(entry.id) else { return };
        let wl_output = output.output.clone();
        info!("sharing output {}: {}x{} pixels at scale {:.2}", entry.name, entry.width, entry.height, entry.scale);
        self.share_output(FIRST, entry.name, entry.width, entry.height, &wl_output);
    }

    /// The output a desk shared went away. The first desk moves to whatever is
    /// left; a display beside ends.
    fn output_gone(&mut self, desk: usize) {
        if desk == FIRST {
            // The name would otherwise stand for an output the compositor no
            // longer has, which leaves nothing selected, the capture stopped and
            // nothing left to start it again -- a client on a picture that has
            // quietly stopped changing.
            self.outputs.selected[FIRST] = None;
            self.adopt_output();
        } else {
            self.end_beside();
        }
    }

    /// Point a desk's virtual pointer at another output. What the client holds
    /// is let go first: a press cannot outlive the pointer that made it.
    fn retarget_input(&mut self, desk: usize, output: &WlOutput) {
        let (Some(input), Some(pointers), Some(seat)) = (&mut self.input, &self.pointers, &self.seat) else { return };
        input.point(desk, &self.qh, pointers, seat, output);
    }

    /// Follow a client's density: its desk's output's mode and scale in one
    /// configuration, so every application on it redraws once. Only what differs
    /// is asked for, and a declaration that changes nothing, or cannot change
    /// anything, is answered at once with the output as it is.
    ///
    /// One declaration's configuration is out at a time, so each settles on its
    /// own and is answered once. One arriving meanwhile waits for that one to
    /// settle; a newer one from the same desk replaces it, and the one replaced
    /// is answered with the output as it is.
    fn declare(&mut self, desk: usize, client: ClientId, width: u16, height: u16, scale: f64) {
        if self.outputs.declaring.is_some() {
            debug!("client {}: a declaration waits for the one before it to settle", client.0);
            if let Some((waiting, ..)) = self.desks[desk].queued_declaration.replace((client, width, height, scale)) {
                self.answer_geometry(desk, Some(waiting));
            }
            return;
        }
        let current = self.outputs.scale(desk);
        let current_size = self.outputs.size(desk);
        let new_scale = ((current - scale).abs() >= 0.005).then_some(scale);
        let new_size = ((width, height) != current_size).then_some((width, height));
        if new_scale.is_none() && new_size.is_none() {
            return self.answer_geometry(desk, Some(client));
        }
        if !self.resize_allowed {
            info!("not following client {}'s density {scale:.2} at {width}x{height}: resizing is disabled", client.0);
            return self.answer_geometry(desk, Some(client));
        }
        info!(
            "following client {}'s density: output {}x{} at scale {current:.2} -> {width}x{height} at scale {scale:.2}",
            client.0, current_size.0, current_size.1
        );
        // The frame at the new size is this client's resize, as a SetDesktopSize's is.
        self.desks[desk].pending_resize = new_size.map(|(w, h)| (client, w, h));
        let qh = self.qh.clone();
        let id = self.outputs.next_declaration;
        self.outputs.next_declaration += 1;
        if !self.outputs.configure(&qh, desk, new_size, new_scale, ConfigKind::Declare { id }) {
            self.desks[desk].pending_resize = None;
            self.answer_geometry(desk, Some(client));
        }
    }

    /// The declaration out has been answered: run one that waited, if its
    /// client is still on its desk.
    pub fn declaration_settled(&mut self) {
        for desk in [FIRST, BESIDE] {
            if self.outputs.declaring.is_some() {
                return;
            }
            if let Some((client, width, height, scale)) = self.desks[desk].queued_declaration.take()
                && self.desks[desk].client == Some(client)
            {
                self.declare(desk, client, width, height, scale);
            }
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Compositor {
    fn event(state: &mut Self, _: &WlRegistry, event: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                debug!("a new output appeared");
                state.bind_output(name, version);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.outputs.iter().position(|o| o.global == name) {
                    let removed = state.outputs.outputs.remove(i);
                    // An output with no name yet is nobody's selection, not even
                    // when nothing is selected.
                    let desks = state.outputs.desks_on(removed.name.as_deref());
                    warn!("output {} went away{}", removed.name.unwrap_or_default(), if desks.is_empty() { "" } else { "; it is a shared one" });
                    removed.output.release();
                    if desks.is_empty() {
                        return state.outputs_changed();
                    }
                    for desk in desks {
                        state.output_gone(desk);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for Compositor {
    fn event(state: &mut Self, _: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // Only the first pointer matters: from then on `get_pointer` is legal,
        // whatever the seat has at the moment.
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(capabilities) } = event
            && capabilities.contains(wl_seat::Capability::Pointer)
            && !state.seat_had_pointer
        {
            debug!("the seat has a pointer: the cursor image can be captured");
            state.seat_had_pointer = true;
            state.start_cursor(FIRST);
            state.start_cursor(BESIDE);
        }
    }
}
