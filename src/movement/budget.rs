//! The server's movement allowance, mirrored so a hop is never sent that the server would pull
//! back. Port of MoveBudget + AllowedTicks (Auto) + PacketRate.GLOBAL.
//!
//! The server (26.2, ServerGamePacketListenerImpl) counts move packets per tick and measures each
//! one from where the tick started; on the Paper server it was tuned on, the field-proven rule is that a hop of `d` blocks
//! is accepted once its packet lands at index `ceil(d / 10)` - so distance is bought by sending
//! `ceil(d / 10) - 1` "charge" packets (status-only, no movement) first. The vanilla "past five
//! packets the multiplier collapses to one" clamp did not bite there (bursts of 600+ seen), so the
//! ceiling starts optimistic at 512 and halves whenever the server corrects a tick that spent more
//! than five - optimistic with a floor, as AllowedTicks.Auto does.

use std::{collections::VecDeque, time::Instant};

use azalea::Vec3;

/// Blocks bought per packet index (the autoplay rule).
pub const LINEAR_STEP: f64 = 10.0;
/// Margin on the server's allowance.
pub const SAFETY: f64 = 0.85;
/// A hop every packet may make, whatever the counter says.
pub const STEP_ALWAYS_LEGAL: f64 = LINEAR_STEP * SAFETY;
/// Server distance allowance per counted packet (squared metres; 300 while gliding).
const SQ_PER_PACKET: f64 = 100.0;
const VANILLA_CEILING: u32 = 5;
const MAX_CEILING: u32 = 512;
/// Paper's packet limiter (as the server it was tuned on has it): 1250 packets per 4 s (one under, plus a grace period).
const RATE_LIMIT: usize = 1249;
const RATE_WINDOW_MS: u128 = 4_020;

#[derive(Debug)]
pub struct Budget {
    /// Move packets sent this tick (ours and azalea's).
    sent: u32,
    /// Where this tick started - the server measures every packet of the tick from here.
    tick_start: Option<Vec3>,
    /// Packets past which the server's multiplier collapses (Auto: starts at 512, halves on a
    /// correction of a tick that spent more than five).
    ceiling: u32,
    locked_vanilla: bool,
    /// Cap on charge packets for one hop (ChargeFlight max-charge-burst).
    pub max_charge_burst: u32,
    /// Every packet sent, for the rate limiter's 4-second window.
    stamps: VecDeque<Instant>,
    pub setbacks: u32,
    /// The setbacks that came after a bought hop (charged, or longer than STEP_ALWAYS_LEGAL) -
    /// the only ones speed can explain. The rest are moves the server would not let through.
    pub speed_setbacks: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            sent: 0,
            tick_start: None,
            ceiling: MAX_CEILING,
            locked_vanilla: false,
            max_charge_burst: 32,
            stamps: VecDeque::new(),
            setbacks: 0,
            speed_setbacks: 0,
        }
    }
}

impl Budget {
    /// Start of a tick, at the position the server last accepted.
    pub fn new_tick(&mut self, pos: Vec3) {
        self.sent = 0;
        self.tick_start = Some(pos);
    }

    /// Packets sent this tick so far.
    pub fn sent(&self) -> u32 {
        self.sent
    }

    /// One or more packets went out (any kind counts toward the rate limit; move packets also
    /// toward the per-tick index).
    pub fn record(&mut self, n: u32, movement: bool) {
        if movement {
            self.sent += n;
        }
        let now = Instant::now();
        for _ in 0..n {
            self.stamps.push_back(now);
        }
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        while self.stamps.front().is_some_and(|t| now.duration_since(*t).as_millis() > RATE_WINDOW_MS) {
            self.stamps.pop_front();
        }
    }

    /// Room under the packet limiter for `n` more packets.
    pub fn can_afford(&mut self, n: u32) -> bool {
        self.prune(Instant::now());
        self.stamps.len() + n as usize <= RATE_LIMIT
    }

    /// The server pulled us back. If this tick had spent past vanilla's five, the clamp may be
    /// real: halve toward five (one stray correction does not cost the session its charge).
    pub fn on_rubberband(&mut self, server_pos: Vec3) {
        self.setbacks += 1;
        if self.sent > VANILLA_CEILING && !self.locked_vanilla {
            self.ceiling = (self.ceiling.min(self.sent) / 2).max(VANILLA_CEILING);
            if self.ceiling <= VANILLA_CEILING {
                self.locked_vanilla = true;
            }
        }
        self.sent = self.ceiling;
        self.tick_start = Some(server_pos);
    }

    fn remaining(&self) -> u32 {
        self.ceiling.saturating_sub(self.sent)
    }

    fn effective(&self, k: u32) -> u32 {
        if k > self.ceiling { 1 } else { k.max(1) }
    }

    /// Furthest a packet at index `k` may land from the tick start.
    fn allowed_radius(&self, k: u32) -> f64 {
        (SQ_PER_PACKET * self.effective(k) as f64).sqrt() * SAFETY
    }

    /// The longest hop this tick could still pay for.
    pub fn best_hop(&self) -> f64 {
        if self.remaining() == 0 {
            return STEP_ALWAYS_LEGAL;
        }
        let ceil = self.ceiling.min(MAX_CEILING);
        self.allowed_radius(ceil).min(LINEAR_STEP * (self.max_charge_burst as f64 + 1.0))
    }

    /// Charge packets to send before a move to `dest` (measured from the tick start, as the
    /// server measures it): 0 when the next index already covers it.
    pub fn charge_to(&self, from: Vec3, dest: Vec3) -> u32 {
        let leg = from.distance_to(dest);
        let cumulative = self.tick_start.map_or(leg, |s| s.distance_to(dest));
        let need_k = ((leg.max(cumulative) / LINEAR_STEP).ceil() as u32).clamp(1, self.ceiling.min(MAX_CEILING));
        let have_k = self.sent + 1;
        let charge = need_k.saturating_sub(have_k);
        let room = self.remaining().saturating_sub(1);
        charge.min(room).min(self.max_charge_burst)
    }

    pub fn describe(&self) -> String {
        format!(
            "sent {} ceil {}{} burst {} best {:.1} setbacks {} (speed {})",
            self.sent,
            self.ceiling,
            if self.locked_vanilla { " (locked)" } else { "" },
            self.max_charge_burst,
            self.best_hop(),
            self.setbacks,
            self.speed_setbacks
        )
    }
}

#[cfg(test)]
mod tests {
    use azalea::Vec3;

    use super::*;

    #[test]
    fn charge_buys_ten_blocks_per_index() {
        let mut b = Budget::default();
        b.new_tick(Vec3::ZERO);
        // 35 blocks: index ceil(35/10) = 4, so 3 charge packets before the move.
        assert_eq!(b.charge_to(Vec3::ZERO, Vec3::new(0.0, 35.0, 0.0)), 3);
        // Within ten: none.
        assert_eq!(b.charge_to(Vec3::ZERO, Vec3::new(1.5, 0.0, 0.0)), 0);
        // Packets already sent this tick count toward the index.
        b.record(3, true);
        assert_eq!(b.charge_to(Vec3::ZERO, Vec3::new(0.0, 35.0, 0.0)), 0);
        // Capped by max-charge-burst.
        b.new_tick(Vec3::ZERO);
        assert_eq!(b.charge_to(Vec3::ZERO, Vec3::new(0.0, 1000.0, 0.0)), 32);
    }

    #[test]
    fn rubberband_halves_the_ceiling_only_after_a_big_burst() {
        let mut b = Budget::default();
        b.new_tick(Vec3::ZERO);
        let best = b.best_hop();
        assert!(best > 190.0 && best < 195.0, "best hop {best}");
        b.record(2, true);
        b.on_rubberband(Vec3::ZERO); // a small tick: geometry, not the clamp
        b.new_tick(Vec3::ZERO);
        assert!(b.best_hop() > 190.0);
        b.record(10, true);
        b.on_rubberband(Vec3::ZERO); // a 10-packet tick corrected: ceiling 5, locked
        b.new_tick(Vec3::ZERO);
        assert!(b.best_hop() < 20.0, "best hop after clamp {}", b.best_hop());
    }

    #[test]
    fn smooth_rounds_and_steps_off_the_seam() {
        use crate::movement::geom::smooth;
        assert!(smooth(1.234) > 1.23 && smooth(1.234) < 1.2300001);
        assert!(smooth(-1.234) < -1.23 && smooth(-1.234) > -1.2300001);
        assert_eq!(smooth(0.0), 0.0);
    }
}
