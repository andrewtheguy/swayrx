//! The VP9 quality and frame rate a session's link will bear.
//!
//! The configured `vp9_quality` is a ceiling and [`QUALITY_FLOOR`] the floor;
//! between them the dial walks down when the client falls behind and back up
//! when it keeps up, and past the floor it is the frame rate that goes. The
//! signal is how long a frame takes to be delivered, decoded and — for a
//! client that answers its fence from its window, as wlshare's own does —
//! drawn: the round trip of the fence that follows it, or, for a client
//! without Fence, how long writing it blocked — less the link's own floor, the
//! shortest delivery seen lately, so a distant link that keeps up reads as
//! keeping up. What is left is queueing: time spent behind frames the link or
//! the client could not take as fast as they came.
//!
//! One-directional by construction: the walk never goes above the ceiling,
//! since a link with room to spare shows no more of it than one that is merely
//! keeping up, and a finer picture than the operator asked for was never the
//! goal. Quick to give quality up — by more the further behind the link is —
//! and slow to take it back, in steps that double while the link keeps taking
//! them and stop short of a quality it refused, so a link that is
//! intermittently bad settles at a quality it can hold rather than oscillating
//! around one it cannot. Two knobs in a fixed order: quality down to the floor,
//! then the frame interval doubled up to [`SLOW_MAX`] times; frames back first
//! and quality after. The same walk remotex runs for its own VP9 streams.
//!
//! What the walk holds is what a *moving* picture is coded at. A desktop that
//! went quiet below the ceiling is sharpened there once — [`QualityWalk::settle`]
//! — and the walk keeps its place through it: the link did not get any wider
//! because the screen stopped.
//!
//! Pure, and takes `now` rather than reading a clock, so every decision is
//! testable without waiting for one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Queueing past which a frame counts as one the link could not keep up with.
const LAG_BEHIND: Duration = Duration::from_millis(60);

/// Queueing that is not a little behind but a lot: a step down here gives up
/// twice [`STEP_DOWN`], and past [`LAG_SEVERE`] three times and halves the
/// frame rate besides, whatever the quality. A link this far behind has a
/// queue growing by the frame, and ten points a second is too slow for it.
const LAG_HEAVY: Duration = Duration::from_millis(150);
/// See [`LAG_HEAVY`].
const LAG_SEVERE: Duration = Duration::from_millis(400);

/// Queueing below which a frame counts as clear. Between this and
/// [`LAG_BEHIND`] is hysteresis: a link hovering there earns neither a coarser
/// picture nor its quality back.
const LAG_CLEAR: Duration = Duration::from_millis(30);

/// Behind frames among the last [`VERDICT_WINDOW`] before quality is given up,
/// the latest among them. Two rather than one, so a single unlucky frame is not
/// a verdict about the link; a window rather than a run, because the queueing
/// a link that is barely too small shows is intermittent, and a run that one
/// clear frame reset took eleven seconds per step on a link plainly behind.
const BEHIND_FRAMES: u32 = 2;
/// See [`BEHIND_FRAMES`].
const VERDICT_WINDOW: u32 = 4;

/// How long the link must have been clear before quality is taken back, and
/// the fewest clear frames that span must hold: deliberately far more than
/// [`BEHIND_FRAMES`]. A span rather than a count of frames, because the
/// frames come as slowly as the link has been slowed to: thirty of them at
/// four a second was a wait no burst of motion ever finished.
const CLEAR_SPAN: Duration = Duration::from_secs(1);
/// See [`CLEAR_SPAN`].
const CLEAR_FRAMES: u32 = 4;

/// How long after a keyframe the verdicts wait. A keyframe is the whole
/// picture and no verdict itself, but the frames behind it queue behind its
/// transmission, and that queue says how big the keyframe was, not what the
/// link bears. Twice [`ADJUST_COOLDOWN`], for a keyframe that takes a second
/// and a half to cross a 2 Mbit/s link.
const KEYFRAME_HOLD: Duration = Duration::from_secs(2);

/// The least time between two moves of the dial, so a burst of slow frames is
/// one decision rather than one per frame.
const ADJUST_COOLDOWN: Duration = Duration::from_secs(1);

/// The least time between a step up and the step down that walks it back. Far
/// shorter than [`ADJUST_COOLDOWN`]: a step up the link refuses shows in the
/// next few frames, and every frame it is left in place is queue.
const REFUSAL_COOLDOWN: Duration = Duration::from_millis(300);

/// How much the lag must have fallen since the last step down for the queue to
/// count as draining, as a fraction of what it was: a fifth. A step down that
/// put the stream under the link leaves the queue it built to drain, and the
/// lag stays high while it does — for seconds, on a link only a little wider
/// than the stream — without saying the step was too small. So while the lag
/// is falling this fast the walk waits, and steps again only when it has
/// stopped falling.
const DRAIN_FRACTION: u32 = 5;

/// How far one step down moves the dial on a link a little behind, and the
/// first step up. Bigger down than up, for the same reason [`CLEAR_FRAMES`] is
/// bigger than [`BEHIND_FRAMES`].
const STEP_DOWN: u8 = 10;
const STEP_UP: u8 = 3;

/// The most one step up reclaims. Every step up the link takes doubles the
/// next, from [`STEP_UP`] to this, and a step down puts it back: a link that
/// has recovered is on the ceiling again within a few seconds of motion.
const STEP_UP_MAX: u8 = 24;

/// How long a quality the link refused stays out of reach. A step down within
/// [`REFUSAL_WINDOW`] of a step up says the link would not take the quality it
/// was just given, and the walk keeps under it for this long before probing
/// there again, with the smallest step — TCP's slow-start threshold, on the
/// dial. Without it a walk whose steps double on the way up would spend a
/// session bouncing off the same quality.
const REFUSAL_HOLD: Duration = Duration::from_secs(15);
/// See [`REFUSAL_HOLD`].
const REFUSAL_WINDOW: Duration = Duration::from_secs(4);

/// The coarsest a moving picture is coded at before the frames go: the walk's
/// floor, and the point where it hands off to the frame rate. Fixed rather
/// than configured, as every adaptive stream's is — WebRTC's quality scaler
/// hands off to resolution and frame rate at an internal quantizer threshold
/// in the coarsest fifth of VP9's range, TigerVNC's AutoSelect has a built-in
/// bottom rung — because past it a finer quantizer step buys nothing a viewer
/// can see, and the settle sharpens a quiet desktop at the ceiling whatever
/// the walk holds. A ceiling below it is on the floor from the start and
/// gives up frames alone.
const QUALITY_FLOOR: u8 = 20;

/// How many times the capture's frame interval may be doubled — 60 Hz down to
/// 7.5 at the default `max_fps` — for a link still behind on the floor. Bytes
/// on the wire are bytes per frame times frames per second, and the floor
/// bounds only the first; fewer frames are fresh frames. The quality goes
/// first because a coarser picture of every movement reads better than a
/// sharp one of every fourth, and comes back last for the same reason. An
/// unslowed link is paced by the capture alone, and each step halves what
/// the capture gives, so the first one is the next rung down and not a jump
/// over it ([`QualityWalk::interval`]).
const SLOW_MAX: u8 = 3;

/// How far back the deliveries go whose minimum is the link's floor. A window
/// rather than an all-time minimum so that a route change is eventually
/// believed — and a minute of one rather than the last few dozen deliveries,
/// because a link that is behind is behind on every one of them: with
/// thirty-two, a standing queue became the floor within seconds of forming,
/// its queueing then read as distance, and the walk that was to drain it
/// never moved. A minute is long enough for the walk to have drained the
/// queue before it is believed.
const BASELINE_WINDOW: Duration = Duration::from_secs(60);

/// Where the walk stands after a move: what to code the next frame at, and the
/// least gap before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    pub quality: u8,
    pub interval: Duration,
}

pub struct QualityWalk {
    /// The configured quality: the finest this ever asks for.
    ceiling: u8,
    /// The interval the capture paces frames to, which a slowed link's is
    /// doubled from.
    capture: Duration,
    /// The quality in force for a moving picture.
    quality: u8,
    /// How many times the frame interval is doubled, at most [`SLOW_MAX`].
    slow: u8,
    /// The deliveries of the last [`BASELINE_WINDOW`], fenced ones only, and
    /// when each came, for the link's floor.
    recent: VecDeque<(Instant, Duration)>,
    /// The last [`VERDICT_WINDOW`] verdicts, newest in the low bit, a set bit
    /// for a frame the link was behind on.
    verdicts: u8,
    /// The clear frames since the link was last behind: when the first came,
    /// and how many.
    clear: Option<(Instant, u32)>,
    /// When the dial last moved, for [`ADJUST_COOLDOWN`].
    changed_at: Option<Instant>,
    /// Until when the verdicts wait, after a keyframe ([`KEYFRAME_HOLD`]).
    held_until: Option<Instant>,
    /// What the next step up reclaims.
    reclaim: u8,
    /// When the last step up was taken and the quality it left, while it is
    /// the last move made: a step down within [`REFUSAL_WINDOW`] of it is a
    /// refusal, walked back to that quality.
    reclaimed: Option<(u8, Instant)>,
    /// The highest quality the walk may reach while a refusal holds, and
    /// when it was refused: halfway between the quality the link bore and
    /// the one it would not take.
    refused: Option<(u8, Instant)>,
    /// The lag the last step down was taken on, while no step up has
    /// followed: what the next step down waits to see stop falling.
    stepped_on: Option<Duration>,
}

impl QualityWalk {
    /// A walk that starts at `ceiling` and goes down to [`QUALITY_FLOOR`], on a
    /// capture paced to `capture`.
    pub fn new(ceiling: u8, capture: Duration) -> Self {
        Self {
            ceiling,
            capture,
            quality: ceiling,
            slow: 0,
            recent: VecDeque::new(),
            verdicts: 0,
            clear: None,
            changed_at: None,
            held_until: None,
            reclaim: STEP_UP,
            reclaimed: None,
            refused: None,
            stepped_on: None,
        }
    }

    /// The quality in force.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// The least gap between two frames: none on a link that is not slowed,
    /// where the capture paces, and the capture's interval doubled once per
    /// step the link has been slowed.
    pub fn interval(&self) -> Duration {
        if self.slow == 0 {
            Duration::ZERO
        } else {
            self.capture * (1u32 << self.slow)
        }
    }

    fn pace(&self) -> Pace {
        Pace { quality: self.quality, interval: self.interval() }
    }

    /// A frame's fence came back `delivery` after the frame was written.
    /// Every delivery counts towards the link's floor; only a `verdict` frame's
    /// is one — a delta frame at the walk's quality. A keyframe is the whole
    /// picture again and slow by its nature, and so is the settle's frame at
    /// the ceiling; the frames behind either carry any lag they cause. Returns
    /// where the walk stands if the dial moved.
    pub fn fenced(&mut self, delivery: Duration, verdict: bool, now: Instant) -> Option<Pace> {
        while self.recent.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) > BASELINE_WINDOW) {
            self.recent.pop_front();
        }
        self.recent.push_back((now, delivery));
        if !verdict {
            return None;
        }
        let floor = self.recent.iter().map(|(_, delivery)| *delivery).min().unwrap_or_default();
        self.observe(delivery.saturating_sub(floor), now)
    }

    /// A desktop that went quiet below the ceiling is being sharpened there
    /// with one frame: the walk keeps its place, since the screen stopping
    /// says nothing about the link, and starts its verdicts over from here
    /// with the cooldown restarted like any other move, so the frames queued
    /// behind that one large picture are not read as the link giving way. The
    /// clear run starts over too: the quiet is no evidence of room, and a run
    /// that spanned it would take quality back on the first frame of every
    /// burst.
    pub fn settle(&mut self, now: Instant) {
        self.verdicts = 0;
        self.clear = None;
        self.changed_at = Some(now);
        self.stepped_on = None;
    }

    /// A keyframe went out: the verdicts wait [`KEYFRAME_HOLD`] for it to
    /// cross the link, and start over after it.
    pub fn keyframe(&mut self, now: Instant) {
        self.verdicts = 0;
        self.clear = None;
        self.stepped_on = None;
        self.held_until = Some(now + KEYFRAME_HOLD);
    }

    /// Whether `quality` is below the ceiling: a frame encoded there is one a
    /// quiet desktop owes a settle for.
    pub fn coarse(&self, quality: u8) -> bool {
        quality < self.ceiling
    }

    /// A delta frame, for a client without Fence, took `blocked` to write:
    /// time the socket had no room for it, which is queueing already.
    pub fn written(&mut self, blocked: Duration, now: Instant) -> Option<Pace> {
        self.observe(blocked, now)
    }

    fn observe(&mut self, lag: Duration, now: Instant) -> Option<Pace> {
        if self.held_until.is_some_and(|until| now < until) {
            return None;
        }
        self.held_until = None;
        let behind = lag >= LAG_BEHIND;
        self.verdicts = ((self.verdicts << 1) | u8::from(behind)) & ((1 << VERDICT_WINDOW) - 1);
        if behind {
            self.clear = None;
        } else if lag <= LAG_CLEAR {
            // Between the two thresholds a frame neither counts nor ends the
            // run: not evidence of room, not evidence against it either.
            self.clear = Some(self.clear.map_or((now, 1), |(since, count)| (since, count + 1)));
        }
        // Walking back a step up the link refused does not wait out the full
        // cooldown: the refusal is in the next few frames, and every one is queue.
        // Only while that step is the last move, though: a settle after it is a
        // move too, and the frames behind its picture are owed the full cooldown.
        let cooldown = if behind && self.reclaimed.is_some_and(|(_, at)| self.changed_at == Some(at)) {
            REFUSAL_COOLDOWN
        } else {
            ADJUST_COOLDOWN
        };
        if self.changed_at.is_some_and(|at| now.saturating_duration_since(at) < cooldown) {
            return None;
        }
        let moved = if behind && self.verdicts.count_ones() >= BEHIND_FRAMES {
            // The queue the last step left behind is still draining: the step
            // was enough, and the verdicts wait for the lag to stop falling.
            if self.stepped_on.is_some_and(|on| lag + on / DRAIN_FRACTION < on) {
                // Looked at again a cooldown from now, against the lag as it is.
                self.stepped_on = Some(lag);
                self.changed_at = Some(now);
                return None;
            }
            self.give_up(lag, now)
        } else if self.clear.is_some_and(|(since, count)| count >= CLEAR_FRAMES && now.saturating_duration_since(since) >= CLEAR_SPAN) {
            self.take_back(now)
        } else {
            return None;
        };
        if !moved {
            return None;
        }
        self.verdicts = 0;
        self.clear = None;
        self.changed_at = Some(now);
        Some(self.pace())
    }

    /// Give quality up, or frames once there is no quality left to give.
    /// `false` when there is nothing left of either.
    fn give_up(&mut self, lag: Duration, now: Instant) -> bool {
        let steps = if lag >= LAG_SEVERE {
            3
        } else if lag >= LAG_HEAVY {
            2
        } else {
            1
        };
        let mut moved = false;
        let on_floor = self.quality <= QUALITY_FLOOR;
        if let Some((from, _)) = self.reclaimed.filter(|(_, at)| now.saturating_duration_since(*at) <= REFUSAL_WINDOW) {
            // A step up the link refused: back to the quality it bore, and the
            // walk may come halfway back up towards the one it would not take.
            // A link that is still behind there steps down from it as usual.
            let cap = from + (self.quality - from) / 2;
            self.refused = Some((cap, now));
            if self.quality > from {
                self.quality = from;
                moved = true;
            }
        } else if !on_floor {
            self.quality = self.quality.saturating_sub(STEP_DOWN * steps).max(QUALITY_FLOOR);
            moved = true;
        }
        if (on_floor || steps == 3) && self.slow < SLOW_MAX {
            self.slow += 1;
            moved = true;
        }
        self.reclaim = STEP_UP;
        self.reclaimed = None;
        self.stepped_on = Some(lag);
        moved
    }

    /// Take frames back first, then quality: a step that doubles while the
    /// link keeps taking them, held under a quality the link refused, and
    /// never past the ceiling. `false` when there is nothing to take back.
    fn take_back(&mut self, now: Instant) -> bool {
        if self.slow > 0 {
            self.slow -= 1;
            self.stepped_on = None;
            return true;
        }
        if self.refused.is_some_and(|(_, at)| now.saturating_duration_since(at) >= REFUSAL_HOLD) {
            self.refused = None;
        }
        let ceiling = self.refused.map_or(self.ceiling, |(cap, _)| cap).min(self.ceiling);
        let wanted = self.quality.saturating_add(self.reclaim).min(ceiling);
        if wanted <= self.quality {
            return false;
        }
        // A step the refusal cut short is the last of its run: the probe past the
        // refused quality, once the hold is over, starts small again.
        self.reclaim = if wanted == ceiling && ceiling < self.ceiling { STEP_UP } else { self.reclaim.saturating_mul(2).min(STEP_UP_MAX) };
        self.reclaimed = Some((self.quality, now));
        self.quality = wanted;
        self.stepped_on = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    /// Clear frames 33 ms apart that make up a [`CLEAR_SPAN`], and then one.
    const CLEAR_RUN: u32 = 32;
    /// A 60 Hz capture, the default: slowed once, 30 Hz.
    const CAPTURE: Duration = Duration::from_micros(16_667);

    /// A walk that has learnt a 40 ms link floor from one clear delivery.
    fn walk(ceiling: u8, start: Instant) -> QualityWalk {
        let mut walk = QualityWalk::new(ceiling, CAPTURE);
        assert_eq!(walk.fenced(40 * MS, true, start), None);
        walk
    }

    /// `count` clear deliveries 33 ms apart from `at`: the last move, and when
    /// they ended.
    fn clear(walk: &mut QualityWalk, count: u32, mut at: Instant) -> (Option<Pace>, Instant) {
        let mut moved = None;
        for _ in 0..count {
            at += 33 * MS;
            if let Some(pace) = walk.fenced(40 * MS, true, at) {
                moved = Some(pace);
            }
        }
        (moved, at)
    }

    #[test]
    fn a_link_that_keeps_up_stays_at_the_ceiling_however_far_away() {
        let start = Instant::now();
        let mut walk = walk(60, start);
        for i in 0..200 {
            assert_eq!(walk.fenced(45 * MS, true, start + i * 10 * MS), None);
        }
        assert_eq!(walk.quality(), 60);
        assert_eq!(walk.interval(), Duration::ZERO);
    }

    #[test]
    fn falling_behind_gives_quality_up_down_to_the_floor_once_a_second() {
        let start = Instant::now();
        // Two steps above the floor, so the walk reaches it in three.
        let mut walk = walk(QUALITY_FLOOR + 2 * STEP_DOWN + 5, start);
        assert_eq!(walk.fenced(140 * MS, true, start), None, "one slow frame is not a verdict");
        assert_eq!(walk.fenced(140 * MS, true, start), Some(Pace { quality: QUALITY_FLOOR + STEP_DOWN + 5, interval: Duration::ZERO }));
        for _ in 0..4 {
            assert_eq!(walk.fenced(140 * MS, true, start + 500 * MS), None, "inside the cooldown");
        }
        assert_eq!(walk.fenced(140 * MS, true, start + 1100 * MS).map(|pace| pace.quality), Some(QUALITY_FLOOR + 5));
        walk.fenced(140 * MS, true, start + 2200 * MS);
        assert_eq!(walk.fenced(140 * MS, true, start + 2200 * MS).map(|pace| pace.quality), Some(QUALITY_FLOOR), "stops at the floor");
        // On the floor, the frames go: doubled once per verdict, to the limit.
        walk.fenced(140 * MS, true, start + 3300 * MS);
        assert_eq!(
            walk.fenced(140 * MS, true, start + 3300 * MS),
            Some(Pace { quality: QUALITY_FLOOR, interval: CAPTURE * 2 })
        );
        for i in 2..=u32::from(SLOW_MAX) + 1 {
            let at = start + (2200 + 1100 * i) * MS;
            walk.fenced(140 * MS, true, at);
            walk.fenced(140 * MS, true, at);
        }
        assert_eq!(walk.interval(), CAPTURE * 8);
        let at = start + 20_000 * MS;
        walk.fenced(140 * MS, true, at);
        assert_eq!(walk.fenced(140 * MS, true, at), None, "nothing left to give");
        assert_eq!(walk.quality(), QUALITY_FLOOR);
    }

    #[test]
    fn a_link_far_behind_gives_up_more_at_once() {
        let start = Instant::now();
        let mut heavy = walk(90, start);
        heavy.fenced(40 * MS + LAG_HEAVY, true, start);
        assert_eq!(heavy.fenced(40 * MS + LAG_HEAVY, true, start), Some(Pace { quality: 70, interval: Duration::ZERO }));
        let mut severe = walk(90, start);
        severe.fenced(40 * MS + LAG_SEVERE, true, start);
        assert_eq!(severe.fenced(40 * MS + LAG_SEVERE, true, start), Some(Pace { quality: 60, interval: CAPTURE * 2 }));
    }

    #[test]
    fn quality_comes_back_slowly_then_faster_and_never_past_the_ceiling() {
        let start = Instant::now();
        let mut walk = walk(60, start);
        walk.fenced(140 * MS, true, start);
        assert_eq!(walk.fenced(140 * MS, true, start).map(|pace| pace.quality), Some(50));
        let mut at = start + ADJUST_COOLDOWN;
        let mut seen = Vec::new();
        for _ in 0..6 {
            let (moved, then) = clear(&mut walk, CLEAR_RUN, at);
            seen.extend(moved.map(|pace| pace.quality));
            at = then + ADJUST_COOLDOWN;
        }
        assert_eq!(seen, [53, 59, 60]);
    }

    #[test]
    fn the_frames_come_back_before_the_quality() {
        let start = Instant::now();
        // One step above the floor: the first verdict puts the walk on it, the
        // next two take the frames.
        let mut walk = walk(QUALITY_FLOOR + STEP_DOWN, start);
        for i in 0..3 {
            let at = start + ADJUST_COOLDOWN * i;
            walk.fenced(140 * MS, true, at);
            walk.fenced(140 * MS, true, at);
        }
        assert_eq!(walk.pace(), Pace { quality: QUALITY_FLOOR, interval: CAPTURE * 4 });
        let at = start + ADJUST_COOLDOWN * 4;
        let (moved, at) = clear(&mut walk, CLEAR_RUN, at);
        assert_eq!(moved, Some(Pace { quality: QUALITY_FLOOR, interval: CAPTURE * 2 }));
        let (moved, at) = clear(&mut walk, CLEAR_RUN, at + ADJUST_COOLDOWN);
        assert_eq!(moved, Some(Pace { quality: QUALITY_FLOOR, interval: Duration::ZERO }));
        let (moved, _) = clear(&mut walk, CLEAR_RUN, at + ADJUST_COOLDOWN);
        assert_eq!(moved, Some(Pace { quality: QUALITY_FLOOR + STEP_UP, interval: Duration::ZERO }));
    }

    #[test]
    fn a_refused_quality_is_held_out_of_reach() {
        let start = Instant::now();
        let mut walk = walk(90, start);
        for i in 0..3 {
            let at = start + ADJUST_COOLDOWN * i;
            walk.fenced(140 * MS, true, at);
            walk.fenced(140 * MS, true, at);
        }
        assert_eq!(walk.quality(), 60);
        let mut at = start + ADJUST_COOLDOWN * 4;
        let (moved, then) = clear(&mut walk, CLEAR_RUN, at);
        assert_eq!(moved.map(|pace| pace.quality), Some(63));
        at = then + ADJUST_COOLDOWN;
        let (moved, then) = clear(&mut walk, CLEAR_RUN, at);
        assert_eq!(moved.map(|pace| pace.quality), Some(69));
        // 69 is refused: the link falls behind within the window of that step,
        // and the walk goes back to the 63 the link bore.
        at = then + ADJUST_COOLDOWN;
        walk.fenced(140 * MS, true, at);
        assert_eq!(walk.fenced(140 * MS, true, at).map(|pace| pace.quality), Some(63));
        // Back up to 66, halfway, and no further, however clear the link — five
        // spells, well inside the hold.
        let mut seen = Vec::new();
        for _ in 0..5 {
            at += ADJUST_COOLDOWN;
            let (moved, then) = clear(&mut walk, CLEAR_RUN, at);
            seen.extend(moved.map(|pace| pace.quality));
            at = then;
        }
        assert_eq!(seen, [66]);
        // Once the hold is over the walk probes past it, small first.
        at += REFUSAL_HOLD;
        let (moved, _) = clear(&mut walk, CLEAR_RUN, at);
        assert_eq!(moved.map(|pace| pace.quality), Some(69));
    }

    #[test]
    fn intermittent_lag_is_still_a_verdict() {
        let start = Instant::now();
        let mut walk = walk(90, start);
        assert_eq!(walk.fenced(140 * MS, true, start), None);
        assert_eq!(walk.fenced(40 * MS, true, start + 33 * MS), None);
        assert_eq!(walk.fenced(140 * MS, true, start + 66 * MS).map(|pace| pace.quality), Some(80));
        // But one behind frame four ago is forgotten.
        let mut once = self::walk(90, start);
        once.fenced(140 * MS, true, start);
        for i in 1..=VERDICT_WINDOW {
            once.fenced(40 * MS, true, start + i * 33 * MS);
        }
        assert_eq!(once.fenced(140 * MS, true, start + 5 * 33 * MS), None);
    }

    #[test]
    fn a_draining_queue_is_not_stepped_on_again() {
        let start = Instant::now();
        let mut walk = walk(90, start);
        walk.fenced(40 * MS + LAG_SEVERE, true, start);
        assert_eq!(walk.fenced(40 * MS + LAG_SEVERE, true, start).map(|pace| pace.quality), Some(60));
        // Still well behind a second later, but a third less than it was.
        let draining = 40 * MS + LAG_SEVERE * 2 / 3;
        let at = start + ADJUST_COOLDOWN;
        assert_eq!(walk.fenced(draining, true, at), None);
        assert_eq!(walk.fenced(draining, true, at), None);
        // Then the drain stalls: the same lag a second on is a step.
        let at = at + ADJUST_COOLDOWN;
        assert_eq!(walk.fenced(draining, true, at).map(|pace| pace.quality), Some(40));
    }

    #[test]
    fn a_refused_step_up_is_walked_back_quickly() {
        let start = Instant::now();
        let mut walk = walk(90, start);
        walk.fenced(140 * MS, true, start);
        assert_eq!(walk.fenced(140 * MS, true, start).map(|pace| pace.quality), Some(80));
        let (moved, at) = clear(&mut walk, CLEAR_RUN, start + ADJUST_COOLDOWN);
        assert_eq!(moved.map(|pace| pace.quality), Some(83));
        // Behind within the ordinary cooldown of that step: walked back anyway,
        // to where the step came from.
        let soon = at + REFUSAL_COOLDOWN;
        walk.fenced(140 * MS, true, soon);
        assert_eq!(walk.fenced(140 * MS, true, soon).map(|pace| pace.quality), Some(80));
    }

    /// A settle after a step up is the last move: the frames behind its picture
    /// wait out the full cooldown, not the refusal's.
    #[test]
    fn a_settle_after_a_step_up_restores_the_full_cooldown() {
        let start = Instant::now();
        let mut walk = walk(90, start);
        walk.fenced(140 * MS, true, start);
        assert_eq!(walk.fenced(140 * MS, true, start).map(|pace| pace.quality), Some(80));
        let (moved, at) = clear(&mut walk, CLEAR_RUN, start + ADJUST_COOLDOWN);
        assert_eq!(moved.map(|pace| pace.quality), Some(83));
        walk.settle(at + 33 * MS);
        let soon = at + 33 * MS + REFUSAL_COOLDOWN;
        walk.fenced(140 * MS, true, soon);
        assert_eq!(walk.fenced(140 * MS, true, soon), None, "the settle's frames were walked back on the refusal's cooldown");
        assert_eq!(walk.quality(), 83);
    }

    #[test]
    fn a_keyframe_holds_the_verdicts_while_it_crosses() {
        let start = Instant::now();
        let mut walk = walk(90, start);
        walk.keyframe(start);
        let mut at = start;
        while at + 33 * MS < start + KEYFRAME_HOLD {
            at += 33 * MS;
            assert_eq!(walk.fenced(40 * MS + LAG_SEVERE, true, at), None, "a verdict inside the hold");
        }
        assert_eq!(walk.quality(), 90);
        at += 33 * MS;
        assert_eq!(walk.fenced(40 * MS + LAG_SEVERE, true, at), None);
        assert_eq!(walk.fenced(40 * MS + LAG_SEVERE, true, at).map(|pace| pace.quality), Some(60));
    }

    #[test]
    fn a_second_of_clear_frames_is_enough_however_slow_they_come() {
        let start = Instant::now();
        let mut walk = walk(90, start);
        walk.fenced(40 * MS + LAG_SEVERE, true, start);
        assert_eq!(walk.fenced(40 * MS + LAG_SEVERE, true, start), Some(Pace { quality: 60, interval: CAPTURE * 2 }));
        let mut at = start + ADJUST_COOLDOWN;
        let mut moved = None;
        for _ in 0..CLEAR_FRAMES + 1 {
            at += 250 * MS;
            moved = walk.fenced(40 * MS, true, at).or(moved);
        }
        assert_eq!(moved, Some(Pace { quality: 60, interval: Duration::ZERO }));
    }

    #[test]
    fn a_frame_that_is_no_verdict_can_still_teach_the_floor() {
        let start = Instant::now();
        let mut walk = QualityWalk::new(60, CAPTURE);
        for _ in 0..5 {
            assert_eq!(walk.fenced(400 * MS, false, start), None);
        }
        assert_eq!(walk.quality(), 60);
        // The floor is the smallest delivery, whichever frame it was.
        walk.fenced(10 * MS, false, start);
        walk.fenced(100 * MS, true, start);
        assert_eq!(walk.fenced(100 * MS, true, start).map(|pace| pace.quality), Some(50));
    }

    #[test]
    fn the_hysteresis_band_neither_gives_up_nor_takes_back() {
        let start = Instant::now();
        let mut walk = walk(60, start);
        walk.fenced(140 * MS, true, start);
        walk.fenced(140 * MS, true, start);
        // Inside one baseline window: longer, and 85 ms would be the floor.
        for i in 0..200 {
            assert_eq!(walk.fenced(85 * MS, true, start + ADJUST_COOLDOWN + i * 50 * MS), None);
        }
        assert_eq!(walk.quality(), 50);
    }

    #[test]
    fn settling_keeps_the_walks_place_and_restarts_the_cooldown() {
        let start = Instant::now();
        let mut walk = walk(60, start);
        walk.fenced(140 * MS, true, start);
        assert_eq!(walk.fenced(140 * MS, true, start).map(|pace| pace.quality), Some(50));
        assert!(walk.coarse(walk.quality()));
        walk.settle(start + 1200 * MS);
        assert_eq!(walk.quality(), 50, "the settle moved the walk");
        walk.fenced(140 * MS, true, start + 1300 * MS);
        assert_eq!(walk.fenced(140 * MS, true, start + 1300 * MS), None, "the settle was a move");
        // Counted all the same: the cooldown over, the verdict is already in.
        assert_eq!(walk.fenced(140 * MS, true, start + 2300 * MS).map(|pace| pace.quality), Some(40));
    }

    /// The clear frames before a desktop went quiet do not span the quiet: a
    /// burst of motion after a settle earns its step back up from its own frames.
    #[test]
    fn a_settle_starts_the_clear_run_over() {
        let start = Instant::now();
        let mut walk = walk(60, start);
        walk.fenced(140 * MS, true, start);
        assert_eq!(walk.fenced(140 * MS, true, start).map(|pace| pace.quality), Some(50));
        // Three clear frames, the cooldown over, and then the desktop goes quiet.
        let (moved, at) = clear(&mut walk, CLEAR_FRAMES - 1, start + ADJUST_COOLDOWN);
        assert_eq!(moved, None);
        walk.settle(at + 5000 * MS);
        // The first frame of the next burst, a clear span and more after those
        // three, does not make the fourth.
        let at = at + 5000 * MS + ADJUST_COOLDOWN;
        assert_eq!(walk.fenced(40 * MS, true, at), None, "the quiet was counted as clear");
        // Its own run is what takes quality back.
        let (moved, _) = clear(&mut walk, CLEAR_RUN, at);
        assert_eq!(moved.map(|pace| pace.quality), Some(50 + STEP_UP));
    }

    #[test]
    fn a_blocked_write_is_lag_without_a_floor() {
        let start = Instant::now();
        let mut walk = QualityWalk::new(60, CAPTURE);
        assert_eq!(walk.written(80 * MS, start), None);
        assert_eq!(walk.written(80 * MS, start).map(|pace| pace.quality), Some(50));
    }

    #[test]
    fn the_floor_forgets_a_route_that_is_gone() {
        let start = Instant::now();
        let mut walk = walk(60, start);
        // A new route, 200 ms further: slow at first, then its own floor.
        let mut at = start;
        for _ in 0..12 {
            at += ADJUST_COOLDOWN;
            walk.fenced(240 * MS, true, at);
            walk.fenced(240 * MS, true, at);
        }
        assert_eq!(walk.quality(), QUALITY_FLOOR, "the old floor made the new route read as queueing");
        assert_eq!(walk.interval(), CAPTURE * 8);
        // Once the old floor has left the window, the new route is a link that
        // keeps up: the frames come back, then the quality.
        at += BASELINE_WINDOW;
        let mut moved = Vec::new();
        for _ in 0..5 {
            let (last, then) = (0..CLEAR_RUN).fold((None, at), |(last, at), _| {
                let at = at + 33 * MS;
                (walk.fenced(240 * MS, true, at).or(last), at)
            });
            moved.extend(last);
            at = then + ADJUST_COOLDOWN;
        }
        // Three runs take the frames back, then two steps up: three, then six.
        assert_eq!(moved.last(), Some(&Pace { quality: QUALITY_FLOOR + 3 * STEP_UP, interval: Duration::ZERO }));
    }
}
