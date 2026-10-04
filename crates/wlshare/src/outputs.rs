//! The compositor's outputs: which one is shared, how big it is in pixels, and
//! the exact scale it is drawn at.
//!
//! Two protocols describe an output. `wl_output` gives its name, pixel mode and
//! an integer scale (wlroots reports the ceiling of a fractional one).
//! wlr-output-management gives the exact scale as the compositor has it, and is
//! the only way to *change* anything: a custom mode for a client's resize, a
//! mode and a scale together for a client's density. Heads are matched to
//! outputs by name.
//!
//! Only a headless output is ever reconfigured, the way wayvnc has it: a real
//! monitor's mode belongs to the person sitting at it.
//!
//! Which output is shared is the configuration's to begin with and the client's
//! from there: a client that speaks the outputs extension is sent the list and
//! may name another one ([`wlshare_rfb::outputs`], [`Outputs::entries`]). The
//! configured output need not be there: another is shared while it is not, and
//! the desktop moves to it when it appears ([`Outputs::wanted`]).

use std::collections::HashMap;

use log::{debug, info, warn};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use wlshare_rfb::outputs::OutputEntry;

use crate::compositor::Compositor;
use crate::shared::{ClientId, Event, FIRST};

pub struct OutputInfo {
    pub output: WlOutput,
    pub global: u32,
    pub name: Option<String>,
    /// The current mode, in pixels, before the transform.
    pub mode: (i32, i32),
    pub transform: wl_output::Transform,
    pub wl_scale: i32,
    pub done: bool,
}

impl OutputInfo {
    /// The framebuffer size a capture of this output has.
    pub fn size(&self) -> (u16, u16) {
        let (w, h) = self.mode;
        let (w, h) = match self.transform {
            wl_output::Transform::_90 | wl_output::Transform::_270 | wl_output::Transform::Flipped90 | wl_output::Transform::Flipped270 => (h, w),
            _ => (w, h),
        };
        (w.clamp(0, i32::from(u16::MAX)) as u16, h.clamp(0, i32::from(u16::MAX)) as u16)
    }

    pub fn is_headless(&self) -> bool {
        self.name.as_deref().is_some_and(|n| n.starts_with("HEADLESS-"))
    }
}

pub struct Head {
    pub head: ZwlrOutputHeadV1,
    pub name: Option<String>,
    pub enabled: bool,
    pub scale: Option<f64>,
    pub current_mode: Option<ObjectId>,
    scale_changed: bool,
}

#[derive(Default)]
pub struct ModeInfo {
    pub size: (i32, i32),
    pub refresh: i32,
}

/// What a configuration was for, so its outcome can be reported.
#[derive(Debug, Clone, Copy)]
pub enum ConfigKind {
    /// A SetDesktopSize: the mode alone.
    Resize { client: ClientId },
    /// A density declaration: the mode and the scale together, whichever differ.
    /// `id` tells its events from a declaration's before it.
    Declare { id: u64 },
}

/// A `wl_display.sync` after declaration `id`'s configuration succeeded: when it
/// comes back with that declaration still unsettled, no head scale change
/// arrived — the compositor changed the mode alone, or nothing — and the
/// declaration still needs its answer.
pub struct ScaleSettle {
    id: u64,
}

#[derive(Default)]
pub struct Outputs {
    pub outputs: Vec<OutputInfo>,
    pub heads: Vec<Head>,
    pub modes: HashMap<ObjectId, ModeInfo>,
    pub manager: Option<ZwlrOutputManagerV1>,
    pub serial: u32,
    /// The name of each desk's shared output, once chosen.
    pub selected: [Option<String>; 2],
    /// The output the configuration names. The desktop is on it whenever the
    /// compositor has it and the client has not named another: it is taken at
    /// the start when it is there, and each time it appears.
    pub wanted: Option<String>,
    /// The declaration whose configuration is out and not settled yet, and the
    /// desk it is for. One at a time, whichever desk asked: its settling is
    /// told apart from nothing else, and a configuration names every head.
    pub declaring: Option<(u64, usize)>,
    /// The id the next declaration takes.
    pub next_declaration: u64,
}

impl Outputs {
    pub fn selected(&self, desk: usize) -> Option<&OutputInfo> {
        let name = self.selected[desk].as_deref()?;
        self.outputs.iter().find(|o| o.name.as_deref() == Some(name))
    }

    pub fn selected_head(&self, desk: usize) -> Option<&Head> {
        let name = self.selected[desk].as_deref()?;
        self.head(name)
    }

    fn head(&self, name: &str) -> Option<&Head> {
        self.heads.iter().find(|h| h.name.as_deref() == Some(name))
    }

    /// The desks sharing the output of this name.
    pub fn desks_on(&self, name: Option<&str>) -> Vec<usize> {
        (0..self.selected.len()).filter(|&desk| name.is_some() && self.selected[desk].as_deref() == name).collect()
    }

    /// The exact scale of a desk's shared output: the head's, or `wl_output`'s.
    pub fn scale(&self, desk: usize) -> f64 {
        self.selected(desk).map_or(1.0, |o| self.scale_of(o))
    }

    /// The exact scale `output` is drawn at: its head's, or `wl_output`'s.
    pub fn scale_of(&self, output: &OutputInfo) -> f64 {
        if let Some(name) = output.name.as_deref()
            && let Some(scale) = self.head(name).and_then(|h| h.scale)
        {
            return scale;
        }
        f64::from(output.wl_scale.max(1))
    }

    /// A desk's shared output's size in pixels as the head reports it, falling
    /// back to `wl_output`'s mode.
    pub fn size(&self, desk: usize) -> (u16, u16) {
        self.selected(desk).map_or((0, 0), |o| self.size_of(o))
    }

    /// The framebuffer size a capture of `output` has, as its head reports the
    /// mode, falling back to `wl_output`'s.
    pub fn size_of(&self, output: &OutputInfo) -> (u16, u16) {
        let mode = output
            .name
            .as_deref()
            .and_then(|name| self.head(name))
            .and_then(|h| h.current_mode.as_ref())
            .and_then(|m| self.modes.get(m));
        let Some(mode) = mode else { return output.size() };
        let (w, h) = mode.size;
        let (w, h) = match output.transform {
            wl_output::Transform::_90 | wl_output::Transform::_270 | wl_output::Transform::Flipped90 | wl_output::Transform::Flipped270 => (h, w),
            _ => (w, h),
        };
        (w.clamp(0, i32::from(u16::MAX)) as u16, h.clamp(0, i32::from(u16::MAX)) as u16)
    }

    /// A desk's shared output's id, or 0 while there is none: the `wl_output`
    /// global, which is unique for as long as the output exists and is what a
    /// client names in a `SelectOutput`.
    pub fn active_id(&self, desk: usize) -> u32 {
        self.selected(desk).map_or(0, |o| o.global)
    }

    /// The outputs a client may choose between, by name so the order a menu
    /// shows them in does not depend on the order the compositor announced
    /// them. An output whose name or mode has not arrived yet is not in the
    /// list: it cannot be labelled, and capturing it would produce nothing.
    pub fn entries(&self) -> Vec<OutputEntry> {
        let mut entries: Vec<OutputEntry> = self
            .outputs
            .iter()
            .filter(|o| self.listable(o))
            .map(|o| {
                let (width, height) = self.size_of(o);
                OutputEntry {
                    id: o.global,
                    name: o.name.clone().expect("listable"),
                    width,
                    height,
                    scale: self.scale_of(o),
                    headless: o.is_headless(),
                }
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    /// Whether an output can be shared at all: its properties have arrived, it
    /// has a name to be labelled by, and a capture of it would produce pixels.
    /// The one rule behind both [`Outputs::entries`] and [`Outputs::selectable`],
    /// so what a client is offered and what it may ask for cannot drift apart.
    fn listable(&self, output: &OutputInfo) -> bool {
        let (width, height) = self.size_of(output);
        output.done && output.name.is_some() && width > 0 && height > 0
    }

    /// The output with this id, while the compositor has one a client may share.
    /// An id that names an output still arriving, or one left without a mode, is
    /// no more selectable than an id the compositor never had: sharing it would
    /// put a desktop of no size in front of the client and give the capture
    /// nothing to read.
    pub fn selectable(&self, id: u32) -> Option<&OutputInfo> {
        self.outputs.iter().find(|o| o.global == id).filter(|o| self.listable(o))
    }

    /// Whether this is the output the configuration names.
    pub fn is_wanted(&self, name: Option<&str>) -> bool {
        name.is_some() && self.wanted.as_deref() == name
    }

    /// Pick the shared output: the configured name, or the first one — also
    /// when the compositor has none of that name, which is an output not
    /// enabled yet rather than a mistake to stop on.
    pub fn select(&mut self, wanted: Option<&str>) -> anyhow::Result<()> {
        self.wanted = wanted.map(str::to_owned);
        let named = wanted.and_then(|name| self.outputs.iter().find(|o| o.name.as_deref() == Some(name)));
        if let (Some(name), None) = (wanted, named) {
            let have: Vec<_> = self.outputs.iter().filter_map(|o| o.name.clone()).collect();
            info!("no output named {name} yet; the compositor has {have:?}, and the first is shared until it appears");
        }
        let chosen = match named {
            Some(output) => output,
            // The first with a name: one without cannot be shared, and being
            // listed first should not keep the daemon off one that can.
            None => self
                .outputs
                .iter()
                .find(|o| o.name.is_some())
                .or(self.outputs.first())
                .ok_or_else(|| anyhow::anyhow!("the compositor has no outputs"))?,
        };
        let name = chosen.name.clone().ok_or_else(|| anyhow::anyhow!("the output has no name; wl_output version 4 is required"))?;
        info!(
            "sharing output {name}: {}x{} pixels at scale {:.2}{}",
            chosen.size().0,
            chosen.size().1,
            self.heads.iter().find(|h| h.name.as_deref() == Some(&name)).and_then(|h| h.scale).unwrap_or(f64::from(chosen.wl_scale)),
            if chosen.is_headless() { ", headless" } else { "" }
        );
        self.selected[crate::shared::FIRST] = Some(name);
        Ok(())
    }

    /// Apply a configuration that changes a desk's shared output's mode, when
    /// `size` is given, and its scale, when `scale` is, leaving every other
    /// property of every head as the compositor has it. `false` when nothing was
    /// sent.
    pub fn configure(
        &mut self,
        qh: &QueueHandle<Compositor>,
        desk: usize,
        size: Option<(u16, u16)>,
        scale: Option<f64>,
        kind: ConfigKind,
    ) -> bool {
        let Some(manager) = &self.manager else {
            info!("wlr-output-management is not available; not reconfiguring the output");
            return false;
        };
        let Some(selected) = self.selected(desk) else { return false };
        if !selected.is_headless() {
            info!("not reconfiguring {}: not a headless output", selected.name.as_deref().unwrap_or("?"));
            return false;
        }
        let name = selected.name.clone();
        let refresh = self
            .selected_head(desk)
            .and_then(|h| h.current_mode.as_ref())
            .and_then(|m| self.modes.get(m))
            .map_or(0, |m| m.refresh);

        let config = manager.create_configuration(self.serial, qh, kind);
        if let ConfigKind::Declare { id } = kind {
            self.declaring = Some((id, desk));
        }
        for head in &self.heads {
            if !head.enabled {
                config.disable_head(&head.head);
                continue;
            }
            let ch = config.enable_head(&head.head, qh, ());
            if head.name == name {
                if let Some((w, h)) = size {
                    debug!("asking for a {w}x{h} mode at {refresh} mHz");
                    ch.set_custom_mode(i32::from(w), i32::from(h), refresh);
                    // Rotation makes no sense on a headless output.
                    ch.set_transform(wl_output::Transform::Normal);
                }
                if let Some(s) = scale {
                    debug!("asking for scale {s:.2}");
                    ch.set_scale(s);
                }
            }
        }
        config.apply();
        true
    }
}

impl Dispatch<WlOutput, ()> for Compositor {
    fn event(state: &mut Self, output: &WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(info) = state.outputs.outputs.iter_mut().find(|o| o.output == *output) else { return };
        match event {
            wl_output::Event::Geometry { transform: WEnum::Value(t), .. } => info.transform = t,
            wl_output::Event::Mode { flags, width, height, .. } => {
                if flags.into_result().is_ok_and(|f| f.contains(wl_output::Mode::Current)) {
                    info.mode = (width, height);
                }
            }
            wl_output::Event::Scale { factor } => info.wl_scale = factor,
            wl_output::Event::Name { name } => info.name = Some(name),
            wl_output::Event::Done => {
                let appeared = !std::mem::replace(&mut info.done, true);
                let name = info.name.clone();
                for desk in state.outputs.desks_on(name.as_deref()) {
                    state.geometry_changed(desk);
                }
                state.outputs_changed();
                if state.outputs.selected[FIRST].is_none() {
                    // Nothing is shared, and this output has just become
                    // something that can be: the desktop starts again on it.
                    state.adopt_output();
                } else if appeared && state.outputs.is_wanted(name.as_deref()) && state.outputs.selected[FIRST] != name {
                    // The configured output is here, and the desktop was on
                    // another only for want of it. On its appearing and not on
                    // every `done`: a client that names another afterwards
                    // stays where it asked to be.
                    state.adopt_output();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for Compositor {
    fn event(state: &mut Self, _: &ZwlrOutputManagerV1, event: zwlr_output_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                state.outputs.heads.push(Head { head, name: None, enabled: false, scale: None, current_mode: None, scale_changed: false });
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                state.outputs.serial = serial;
                let mut changed = Vec::new();
                for head in &mut state.outputs.heads {
                    if std::mem::take(&mut head.scale_changed) {
                        changed.push(head.name.clone());
                    }
                }
                for name in changed {
                    for desk in state.outputs.desks_on(name.as_deref()) {
                        // The report this sends answers the declaration out
                        // for that desk, if any.
                        let settled = state.outputs.declaring.take_if(|(_, declaring)| *declaring == desk).is_some();
                        state.geometry_changed(desk);
                        if settled {
                            state.declaration_settled();
                        }
                    }
                }
                // A head's scale or mode is in every entry's label, not only the
                // shared one's.
                state.outputs_changed();
            }
            zwlr_output_manager_v1::Event::Finished => warn!("the compositor withdrew wlr-output-management"),
            _ => {}
        }
    }

    event_created_child!(Compositor, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for Compositor {
    fn event(state: &mut Self, head: &ZwlrOutputHeadV1, event: zwlr_output_head_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwlr_output_head_v1::Event::Mode { mode } = &event {
            state.outputs.modes.insert(mode.id(), ModeInfo::default());
        }
        let Some(h) = state.outputs.heads.iter_mut().find(|h| h.head == *head) else { return };
        match event {
            zwlr_output_head_v1::Event::Name { name } => h.name = Some(name),
            zwlr_output_head_v1::Event::Enabled { enabled } => h.enabled = enabled != 0,
            zwlr_output_head_v1::Event::Scale { scale } => {
                if h.scale != Some(scale) {
                    h.scale_changed = true;
                }
                h.scale = Some(scale);
            }
            zwlr_output_head_v1::Event::CurrentMode { mode } => h.current_mode = Some(mode.id()),
            zwlr_output_head_v1::Event::Finished => {
                let name = h.name.clone();
                state.outputs.heads.retain(|h| h.head != *head);
                if head.version() >= 3 {
                    head.release();
                }
                debug!("head {} finished", name.unwrap_or_default());
            }
            _ => {}
        }
    }

    event_created_child!(Compositor, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for Compositor {
    fn event(state: &mut Self, mode: &ZwlrOutputModeV1, event: zwlr_output_mode_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                state.outputs.modes.entry(mode.id()).or_default().size = (width, height);
            }
            zwlr_output_mode_v1::Event::Refresh { refresh } => {
                state.outputs.modes.entry(mode.id()).or_default().refresh = refresh;
            }
            zwlr_output_mode_v1::Event::Finished => {
                state.outputs.modes.remove(&mode.id());
                if mode.version() >= 3 {
                    mode.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ConfigKind> for Compositor {
    fn event(
        state: &mut Self,
        config: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        kind: &ConfigKind,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => {
                debug!("output configuration succeeded ({kind:?})");
                if let ConfigKind::Declare { id } = *kind {
                    // `succeeded` says nothing about whether a head changed: the
                    // changes and their `done` follow when there are any. Sway, the
                    // measured compositor, sends them first when it commits on the
                    // spot. One round trip later, whatever the compositor was going
                    // to send has arrived.
                    if state.outputs.declaring.is_some_and(|(declaring, _)| declaring == id) {
                        conn.display().sync(qh, ScaleSettle { id });
                    }
                }
            }
            zwlr_output_configuration_v1::Event::Failed | zwlr_output_configuration_v1::Event::Cancelled => {
                warn!("the compositor refused an output configuration ({kind:?})");
                match kind {
                    ConfigKind::Resize { client } => {
                        if let Some(desk) = state.desk_of(*client) {
                            state.desks[desk].pending_resize = None;
                        }
                        state.shared().emit(Event::ResizeRefused { client: *client, status: wlshare_rfb::msg::EDS_STATUS_INVALID_LAYOUT });
                    }
                    ConfigKind::Declare { id } => {
                        if let Some((_, desk)) = state.outputs.declaring.take_if(|(declaring, _)| declaring == id) {
                            state.desks[desk].pending_resize = None;
                            state.answer_geometry(desk, None);
                            state.declaration_settled();
                        }
                    }
                }
            }
            _ => {}
        }
        config.destroy();
    }
}

impl Dispatch<WlCallback, ScaleSettle> for Compositor {
    fn event(state: &mut Self, _: &WlCallback, event: wl_callback::Event, settle: &ScaleSettle, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event
            && let Some((_, desk)) = state.outputs.declaring.take_if(|(declaring, _)| *declaring == settle.id)
        {
            info!("the compositor applied the declaration's configuration without changing the scale; reporting the output as it is");
            state.answer_geometry(desk, None);
            state.declaration_settled();
        }
    }
}

wayland_client::delegate_noop!(Compositor: ignore ZwlrOutputConfigurationHeadV1);
