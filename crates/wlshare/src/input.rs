//! Input injection: a virtual keyboard and a virtual pointer on the compositor's
//! seat.
//!
//! RFB carries X11 keysyms. The compositor takes evdev keycodes under whatever
//! keymap the virtual keyboard uploaded, so the same keymap serves twice: it is
//! uploaded to the compositor, and searched here for a keycode that produces
//! each incoming keysym. That is why a remapped modifier has to be a *layout*
//! that keeps every key's keysym rather than an option that rewrites it — see
//! the configuration.
//!
//! A character keysym names a character, not a key: the client resolved its own
//! Shift and Caps Lock before sending `A`, and the keycode that produces `A`
//! here is the same one that produces `a`. So the state the compositor will see
//! is checked before every press, and Shift is pressed or let go around the key
//! when the keycode alone would type the other case. A keysym that names a key
//! rather than a character -- Tab, Return, an arrow -- is left alone: it goes out
//! on its keycode under whatever the client holds, so Shift+Tab stays Shift+Tab
//! even though the keycode's shifted level is ISO_Left_Tab, not Tab.
//!
//! One client is on the desktop at a time, so what is held is simply what it
//! holds: a client leaving or being superseded lets go of all of it.
//!
//! Pointer positions arrive in framebuffer pixels and go to the compositor as
//! absolute positions against the framebuffer's extent, which the virtual
//! pointer maps onto the shared output.

use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd;
use std::time::Instant;

use anyhow::Context as _;
use log::{debug, warn};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::QueueHandle;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

use crate::compositor::Compositor;
use crate::config::Xkb;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
/// One wheel notch, in the units `wl_pointer.axis` uses.
const WHEEL_STEP: f64 = 15.0;
/// XKB keycodes are evdev codes plus eight.
const EVDEV_OFFSET: u32 = 8;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_SHIFT_R: u32 = 0xffe2;

/// What was done to Shift around a key so its keysym came out, undone with the key.
enum ShiftFix {
    /// Shift was pressed for the key.
    Pressed(u32),
    /// These held Shift keycodes were let go for the key.
    Released(Vec<u32>),
}

/// Whether `keysym` names a printable character -- one the client has cased --
/// rather than a key. libxkbcommon gives Tab, Return, BackSpace, Escape and
/// Delete control codepoints, so those count as keys here too.
fn is_character(keysym: u32) -> bool {
    xkb::Keysym::new(keysym).key_char().is_some_and(|c| !c.is_control())
}

/// The modifier state the compositor is told after every key: an `xkb_state`
/// fed each keycode's transitions and nothing else.
///
/// xkb counts the presses of a modifier key and holds the modifier until as many
/// releases have arrived, so a press of a key already down -- a held Control
/// auto-repeating in the browser -- would leave the modifier depressed after the
/// one release that follows, and every later key would be typed under it. Only
/// a keycode going down or coming up reaches the state.
struct Modifiers {
    state: xkb::State,
    /// The keycodes the compositor has been told are down.
    down: HashSet<u32>,
}

impl Modifiers {
    fn new(keymap: &xkb::Keymap) -> Self {
        Self { state: xkb::State::new(keymap), down: HashSet::new() }
    }

    fn key(&mut self, code: u32, down: bool) {
        let changed = if down { self.down.insert(code) } else { self.down.remove(&code) };
        if changed {
            self.state.update_key(xkb::Keycode::new(code), if down { xkb::KeyDirection::Down } else { xkb::KeyDirection::Up });
        }
    }
}

/// One key transition for the compositor, with the modifier state that follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sent {
    code: u32,
    down: bool,
    depressed: u32,
    latched: u32,
    locked: u32,
    layout: u32,
}

/// The client's keysyms turned into the key transitions the compositor is sent.
struct Keys {
    modifiers: Modifiers,
    /// Keysym → the keycode (and level) that produces it, preferring level 0.
    keycodes: HashMap<u32, (u32, u32)>,
    /// The keycodes of the Shift keys, for pressing one and recognising any.
    shift_codes: Vec<u32>,
    /// The keycodes the client holds.
    held: HashSet<u32>,
    /// Keys pressed with Shift corrected around them.
    fixes: HashMap<u32, ShiftFix>,
    /// The transitions of the call under way.
    sent: Vec<Sent>,
}

pub struct Input {
    keyboard: ZwpVirtualKeyboardV1,
    pointer: ZwlrVirtualPointerV1,
    keys: Keys,
    /// The client's RFB button mask.
    buttons: u8,
    started: Instant,
}

impl Keys {
    fn new(keymap: &xkb::Keymap) -> Self {
        let mut keycodes = HashMap::new();
        let min = keymap.min_keycode().raw();
        let max = keymap.max_keycode().raw();
        for code in min..=max {
            let key = xkb::Keycode::new(code);
            for layout in 0..keymap.num_layouts_for_key(key) {
                for level in 0..keymap.num_levels_for_key(key, layout) {
                    for sym in keymap.key_get_syms_by_level(key, layout, level) {
                        let entry = keycodes.entry(sym.raw()).or_insert((code, level));
                        if level < entry.1 {
                            *entry = (code, level);
                        }
                    }
                }
            }
        }
        debug!("keymap resolves {} keysyms", keycodes.len());
        let shift_codes: Vec<u32> = [XK_SHIFT_L, XK_SHIFT_R].iter().filter_map(|sym| keycodes.get(sym).map(|&(code, _)| code)).collect();
        if shift_codes.is_empty() {
            warn!("the keymap has no Shift key: case cannot be corrected");
        }
        Self { modifiers: Modifiers::new(keymap), keycodes, shift_codes, held: HashSet::new(), fixes: HashMap::new(), sent: Vec::new() }
    }

    /// A KeyEvent, as the transitions to send for it.
    fn key(&mut self, keysym: u32, down: bool) -> Vec<Sent> {
        let Some(&(code, level)) = self.keycodes.get(&keysym) else {
            warn!("no key produces keysym {keysym:#x}; dropped");
            return Vec::new();
        };
        if down {
            if self.held.insert(code) {
                self.fix_shift(code, level, keysym);
            }
            // A repeat goes out too: the state is as the first press left it,
            // and stays so -- see [`Modifiers`].
            self.send_key(code, true);
        } else if self.held.remove(&code) {
            self.release(code);
        } else {
            // A release of something not held: the compositor counts the
            // state anyway, so send it as it is.
            self.send_key(code, false);
        }
        std::mem::take(&mut self.sent)
    }

    /// Press or let go of Shift so that `code` types `keysym`: the keycode
    /// that produces `A` at level 1 produces `a` at level 0, and the client has
    /// already decided which one it means.
    ///
    /// Only a character has a case to decide. Tab is level 0 of the key whose
    /// level 1 is ISO_Left_Tab, and a browser reports "Tab" for both, so a
    /// client sending Tab with Shift held would otherwise have its Shift let go
    /// and type a plain Tab. Such a keysym is sent on its keycode as it is.
    fn fix_shift(&mut self, code: u32, level: u32, keysym: u32) {
        if !is_character(keysym) {
            return;
        }
        let produced = self.modifiers.state.key_get_one_sym(xkb::Keycode::new(code)).raw();
        if produced == keysym {
            return;
        }
        let shift_down = self.modifiers.state.mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_DEPRESSED);
        if level == 1 && !shift_down {
            if let Some(&shift) = self.shift_codes.first() {
                debug!("keysym {keysym:#x} needs Shift on keycode {code}; pressing it");
                self.send_key(shift, true);
                self.fixes.insert(code, ShiftFix::Pressed(shift));
            }
        } else if level == 0 && shift_down {
            let shifts: Vec<u32> = self.shift_codes.iter().copied().filter(|s| self.held.contains(s)).collect();
            if !shifts.is_empty() {
                debug!("keysym {keysym:#x} needs keycode {code} without Shift; letting go of it");
                for &shift in &shifts {
                    self.send_key(shift, false);
                }
                self.fixes.insert(code, ShiftFix::Released(shifts));
            }
        } else {
            debug!("keysym {keysym:#x} is at level {level} of keycode {code}, which now produces {produced:#x}; sending the keycode as it is");
        }
    }

    /// A key the client let go of, or lost by leaving.
    ///
    /// The Shift pressed for a key is the client's own once the client presses
    /// that Shift key itself, and is then left down for the client to let go of.
    /// A Shift key the client lets go of is one a key's fix no longer holds
    /// either, so it goes up there and then, and not a second time with the key.
    fn release(&mut self, code: u32) {
        if self.shift_codes.contains(&code) {
            self.fixes.retain(|_, fix| !matches!(fix, ShiftFix::Pressed(shift) if *shift == code));
        }
        self.send_key(code, false);
        match self.fixes.remove(&code) {
            Some(ShiftFix::Pressed(shift)) => {
                if !self.held.contains(&shift) {
                    self.send_key(shift, false);
                }
            }
            Some(ShiftFix::Released(shifts)) => {
                for shift in shifts {
                    if self.held.contains(&shift) {
                        self.send_key(shift, true);
                    }
                }
            }
            None => {}
        }
    }

    fn send_key(&mut self, code: u32, down: bool) {
        self.modifiers.key(code, down);
        let state = &self.modifiers.state;
        self.sent.push(Sent {
            code,
            down,
            depressed: state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            latched: state.serialize_mods(xkb::STATE_MODS_LATCHED),
            locked: state.serialize_mods(xkb::STATE_MODS_LOCKED),
            layout: state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        });
    }

    /// Let go of every key the client left held.
    fn release_all(&mut self) -> Vec<Sent> {
        for code in std::mem::take(&mut self.held) {
            self.release(code);
        }
        std::mem::take(&mut self.sent)
    }
}

impl Input {
    pub fn new(
        qh: &QueueHandle<Compositor>,
        keyboards: &ZwpVirtualKeyboardManagerV1,
        pointers: &ZwlrVirtualPointerManagerV1,
        seat: &WlSeat,
        output: &WlOutput,
        xkb_config: &Xkb,
    ) -> anyhow::Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let options = if xkb_config.options.is_empty() { None } else { Some(xkb_config.options.clone()) };
        let keymap = xkb::Keymap::new_from_names(
            &context,
            &xkb_config.rules,
            &xkb_config.model,
            &xkb_config.layout,
            &xkb_config.variant,
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("no keymap compiles from {xkb_config:?}"))?;

        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let fd = rustix::fs::memfd_create("wlshare-keymap", rustix::fs::MemfdFlags::CLOEXEC).context("memfd for the keymap")?;
        let mut file = std::fs::File::from(fd);
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        std::io::Write::write_all(&mut file, b"\0")?;
        let keyboard = keyboards.create_virtual_keyboard(seat, qh, ());
        keyboard.keymap(1, file.as_fd(), (text.len() + 1) as u32);

        let pointer = pointers.create_virtual_pointer_with_output(Some(seat), Some(output), qh, ());
        Ok(Self {
            keyboard,
            pointer,
            keys: Keys::new(&keymap),
            buttons: 0,
            started: Instant::now(),
        })
    }

    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    pub fn key(&mut self, keysym: u32, down: bool) {
        let sent = self.keys.key(keysym, down);
        self.send_keys(&sent);
    }

    fn send_keys(&mut self, sent: &[Sent]) {
        let time = self.time();
        for key in sent {
            self.keyboard.key(time, key.code - EVDEV_OFFSET, u32::from(key.down));
            self.keyboard.modifiers(key.depressed, key.latched, key.locked, key.layout);
        }
    }

    /// The button mask the compositor sees.
    fn buttons_down(&self) -> u8 {
        self.buttons & 7
    }

    /// Press and release the buttons that changed, without a frame.
    fn set_buttons(&mut self, before: u8, after: u8) {
        let time = self.time();
        for (bit, code) in [(1u8, BTN_LEFT), (2, BTN_MIDDLE), (4, BTN_RIGHT)] {
            let now = after & bit != 0;
            if now != (before & bit != 0) {
                self.pointer.button(time, code, if now { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released });
            }
        }
    }

    /// A PointerEvent: position in framebuffer pixels and the RFB button mask.
    pub fn pointer(&mut self, buttons: u8, x: u16, y: u16, extent: (u16, u16)) {
        let time = self.time();
        self.pointer.motion_absolute(time, u32::from(x), u32::from(y), u32::from(extent.0.max(1)), u32::from(extent.1.max(1)));
        let before = self.buttons_down();
        let previous = std::mem::replace(&mut self.buttons, buttons);
        let after = self.buttons_down();
        self.set_buttons(before, after);
        // Wheel "buttons" scroll on the press; the release carries nothing.
        for (bit, axis, direction) in [
            (8u8, wl_pointer::Axis::VerticalScroll, -1.0),
            (16, wl_pointer::Axis::VerticalScroll, 1.0),
            (32, wl_pointer::Axis::HorizontalScroll, -1.0),
            (64, wl_pointer::Axis::HorizontalScroll, 1.0),
        ] {
            if buttons & bit != 0 && previous & bit == 0 {
                self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                self.pointer.axis_discrete(time, axis, WHEEL_STEP * direction, direction as i32);
            }
        }
        self.pointer.frame();
    }

    /// A Scroll: a distance in the output's logical pixels, the units an axis
    /// is in, positive rightward and downward, where the pointer is. It goes to
    /// the compositor as a continuous source: a distance the applications
    /// spend as it is, the way they spend a touchpad's, rather than a count of
    /// notches.
    pub fn scroll(&mut self, dx: i16, dy: i16) {
        if dx == 0 && dy == 0 {
            return;
        }
        let time = self.time();
        self.pointer.axis_source(wl_pointer::AxisSource::Continuous);
        for (axis, pixels) in [(wl_pointer::Axis::VerticalScroll, dy), (wl_pointer::Axis::HorizontalScroll, dx)] {
            if pixels != 0 {
                self.pointer.axis(time, axis, f64::from(pixels));
            }
        }
        self.pointer.frame();
    }

    /// Point the pointer at another output, for a client that asked for another
    /// screen: `zwlr_virtual_pointer` takes an output when it is made and never
    /// again, so the one bound to the old output is destroyed and a new one made
    /// against the new one. Whatever the client holds is let go by the caller
    /// first — a press cannot outlive the pointer that made it.
    pub fn retarget(&mut self, qh: &QueueHandle<Compositor>, pointers: &ZwlrVirtualPointerManagerV1, seat: &WlSeat, output: &WlOutput) {
        self.pointer.destroy();
        self.pointer = pointers.create_virtual_pointer_with_output(Some(seat), Some(output), qh, ());
    }

    /// Let go of everything the client left held.
    pub fn release_all(&mut self) {
        let sent = self.keys.release_all();
        self.send_keys(&sent);
        let before = self.buttons_down();
        self.buttons = 0;
        let after = self.buttons_down();
        if after != before {
            self.set_buttons(before, after);
            self.pointer.frame();
        }
    }
}

wayland_client::delegate_noop!(Compositor: ignore ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(Compositor: ignore ZwpVirtualKeyboardV1);
wayland_client::delegate_noop!(Compositor: ignore ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(Compositor: ignore ZwlrVirtualPointerV1);

#[cfg(test)]
mod tests {
    use xkbcommon::xkb;

    use super::{EVDEV_OFFSET, Keys, Modifiers, Sent, is_character};

    /// The evdev codes of Left Shift, Left Control, Left Alt and Left Meta.
    const MODIFIER_KEYS: [u32; 4] = [42, 29, 56, 125];

    /// The keycodes of Left Shift, Tab and A, and Shift's bit in the modifier mask.
    const SHIFT: u32 = 42 + EVDEV_OFFSET;
    const TAB: u32 = 15 + EVDEV_OFFSET;
    const A: u32 = 30 + EVDEV_OFFSET;
    const SHIFT_MASK: u32 = 1;
    const XK_SHIFT_L: u32 = 0xffe1;
    const XK_TAB: u32 = 0xff09;
    const XK_UPPER_A: u32 = 0x41;

    fn keymap() -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS).expect("the us layout compiles")
    }

    fn modifiers() -> Modifiers {
        Modifiers::new(&keymap())
    }

    /// What reached the compositor: each keycode, its direction, and the
    /// depressed modifiers reported after it.
    fn wire(sent: Vec<Sent>) -> Vec<(u32, bool, u32)> {
        sent.into_iter().map(|key| (key.code, key.down, key.depressed)).collect()
    }

    #[test]
    fn a_shift_the_client_takes_over_outlives_the_key_it_was_pressed_for() {
        let mut keys = Keys::new(&keymap());
        // `A` with no Shift held, as under Caps Lock: Shift is pressed for it.
        assert_eq!(wire(keys.key(XK_UPPER_A, true)), [(SHIFT, true, SHIFT_MASK), (A, true, SHIFT_MASK)]);
        // The client presses that Shift itself, and it repeats.
        for _ in 0..3 {
            assert_eq!(wire(keys.key(XK_SHIFT_L, true)), [(SHIFT, true, SHIFT_MASK)]);
        }
        // Letting go of `A` leaves the Shift the client now holds.
        assert_eq!(wire(keys.key(XK_UPPER_A, false)), [(A, false, SHIFT_MASK)]);
        // So the Tab that follows is Shift+Tab.
        assert_eq!(wire(keys.key(XK_TAB, true)), [(TAB, true, SHIFT_MASK)]);
        assert_eq!(wire(keys.key(XK_TAB, false)), [(TAB, false, SHIFT_MASK)]);
        assert_eq!(wire(keys.key(XK_SHIFT_L, false)), [(SHIFT, false, 0)]);
        assert_eq!(wire(keys.release_all()), []);
    }

    #[test]
    fn a_shift_pressed_for_a_key_goes_with_the_key() {
        let mut keys = Keys::new(&keymap());
        assert_eq!(wire(keys.key(XK_UPPER_A, true)), [(SHIFT, true, SHIFT_MASK), (A, true, SHIFT_MASK)]);
        assert_eq!(wire(keys.key(XK_UPPER_A, false)), [(A, false, SHIFT_MASK), (SHIFT, false, 0)]);
        assert_eq!(wire(keys.key(XK_TAB, true)), [(TAB, true, 0)]);
    }

    #[test]
    fn a_shift_the_client_took_over_and_let_go_of_goes_up_once() {
        let mut keys = Keys::new(&keymap());
        keys.key(XK_UPPER_A, true);
        keys.key(XK_SHIFT_L, true);
        assert_eq!(wire(keys.key(XK_SHIFT_L, false)), [(SHIFT, false, 0)]);
        assert_eq!(wire(keys.key(XK_UPPER_A, false)), [(A, false, 0)]);
    }

    fn depressed(modifiers: &Modifiers) -> u32 {
        modifiers.state.serialize_mods(xkb::STATE_MODS_DEPRESSED)
    }

    #[test]
    fn a_repeated_modifier_is_let_go_by_one_release() {
        for code in MODIFIER_KEYS.map(|evdev| evdev + EVDEV_OFFSET) {
            let mut modifiers = modifiers();
            modifiers.key(code, true);
            let held = depressed(&modifiers);
            assert_ne!(held, 0, "keycode {code} is a modifier");
            for _ in 0..3 {
                modifiers.key(code, true);
                assert_eq!(depressed(&modifiers), held, "keycode {code}");
            }
            modifiers.key(code, false);
            assert_eq!(depressed(&modifiers), 0, "keycode {code}");
        }
    }

    #[test]
    fn a_release_of_a_key_not_down_changes_nothing() {
        for code in MODIFIER_KEYS.map(|evdev| evdev + EVDEV_OFFSET) {
            let mut modifiers = modifiers();
            modifiers.key(code, false);
            assert_eq!(depressed(&modifiers), 0, "keycode {code}");
            modifiers.key(code, true);
            modifiers.key(code, false);
            assert_eq!(depressed(&modifiers), 0, "keycode {code}");
        }
    }

    #[test]
    fn cased_characters_are_corrected() {
        for keysym in [0x61, 0x41, 0x31, 0x21, 0x20, 0xe9, 0x0100_20ac] {
            assert!(is_character(keysym), "{keysym:#x}");
        }
    }

    #[test]
    fn keys_are_sent_as_they_are() {
        // Tab, ISO_Left_Tab, Return, BackSpace, Escape, Delete, Left, Shift_L, F1
        for keysym in [0xff09, 0xfe20, 0xff0d, 0xff08, 0xff1b, 0xffff, 0xff51, 0xffe1, 0xffbe] {
            assert!(!is_character(keysym), "{keysym:#x}");
        }
    }
}
