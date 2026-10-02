//! The scroll message: how a client scrolls by a distance.
//!
//! Standard RFB has no scroll magnitude. A wheel is buttons 4 to 7 of the
//! pointer mask, and all a client can vary is how many times it presses one, so
//! a touchpad glide or two fingers on a phone arrive as whole notches or not at
//! all. The compositor's own vocabulary is richer: a `wl_pointer` axis carries a
//! distance, which is what a touchpad plugged into the machine sends.
//!
//! This private message is that distance. It has no pseudo-encoding, because the
//! server holds no state for it and has nothing to answer: a client that knows
//! the server is wlshare sends a [`ClientMsg::Scroll`](crate::msg::ClientMsg::Scroll)
//! where it would have pressed a wheel button, after the `PointerEvent` that
//! says where. The wheel buttons stay what they are, a notch apiece, for a
//! client that has only notches to send.
//!
//! The distance is in the output's logical pixels, the unit an axis is in and
//! the one a desktop's applications scroll by — not framebuffer pixels, which
//! are that many times the output's scale. Positive is to the right and
//! downward.

/// The message type; outside every registered type.
pub const MSG_SCROLL: u8 = 0xE5;

/// The bytes of a `Scroll`, type included.
///
/// | Offset | Type | Field |
/// |---|---|---|
/// | 0 | U8 | `0xE5` |
/// | 1 | U8 | padding |
/// | 2 | S16 | horizontal distance, logical pixels, positive rightward |
/// | 4 | S16 | vertical distance, logical pixels, positive downward |
pub const SCROLL_LEN: usize = 6;
