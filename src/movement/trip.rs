//! One trip to a coordinate, a hop a tick. Port of GotoPilot.goCharged + ChargedTravel.step:
//! long trips climb to the cruise height first (open sky above the build limit), cross, and come
//! down over the target; the speed starts at 30 b/s and backs off 40% on every server setback
//! (and creeps back up after three clean seconds); no progress for 2 s backs off upwards, for 5 s
//! gives up with a reason - never a silent livelock.

use std::{
    collections::VecDeque,
    f64::consts::TAU,
    time::{Duration, Instant},
};

use azalea::{Vec3, world::World};

use super::{
    budget::{Budget, STEP_ALWAYS_LEGAL},
    geom,
};

pub const CRUISE_Y: f64 = 330.0;
/// Trips shorter than this go straight, not via the cruise height.
const CRUISE_MIN_DISTANCE: f64 = 48.0;
const ARRIVE_DIST: f64 = 1.25;
/// Down per tick when sinking onto a target (FlightMover.MAX_SAFE_DOWN).
pub const MAX_SAFE_DOWN: f64 = 2.25;
/// The most one grounded packet can drop without the server billing fall damage (26.2 bills
/// ceil(drop - 3)); a little under 3 for rounding.
const FREE_DROP: f64 = 2.95;
/// The most fall damage one clip down may be billed, and the health it must leave. 21-22 Oct:
/// laying floor round land and scanning under its own builds, the bot clipped down through
/// canopies and solid melon (billed up to 30) and died 22 times in six hours - every hit came
/// within a second of a clip down. Ordinary days never needed more than 4.
const MAX_CLIP_BILL: f64 = 4.0;
const CLIP_KEEP_HEALTH: f32 = 6.0;
const STUCK_BACKOFF_TICKS: u32 = 40;
const STUCK_GIVEUP_TICKS: u32 = 100;
/// Ticks of positions the stuck test looks back over (a trip bouncing between spots is stuck).
const RECENT_TICKS: usize = 8;
const MAX_BACKOFFS: u32 = 4;
const MIN_BPS: f64 = 2.0;
/// Setbacks speed cannot explain before a trip gives up.
const MAX_REFUSALS: u32 = 6;
pub const DEFAULT_MAX_BPS: f64 = 30.0;
/// How far a roof clip looks above.
const ESCAPE_UP: f64 = 48.0;
/// How long a column whose clip up the server refused is left alone.
const CLIP_REFUSED_FOR: Duration = Duration::from_secs(30 * 60);
/// How long a way out of a spot the server kept pulling us back from is left alone: longer than
/// the routine's longest wait between stuck passes (30 min), so a retry never repeats one.
const WAY_REFUSED_FOR: Duration = Duration::from_secs(2 * 60 * 60);
/// Steps aside round refused clips one trip makes before it gives up.
const MAX_SIDESTEPS: u32 = 6;
/// How far out a step aside looks, nearest first.
const SIDESTEP_DISTANCES: [f64; 4] = [8.0, 12.0, 16.0, 24.0];
/// Heights a step aside may drop to (under a ragged underside the way across can be lower down).
const SIDESTEP_DROPS: [f64; 3] = [0.0, 3.0, 6.0];

/// Within this angle (15 degrees) of a refused way counts as the same way; the 16 step-aside
/// headings are 22.5 degrees apart, so each is a way of its own.
const SAME_WAY_COS: f64 = 0.966;

/// Ways the server refused. Clips up: 16 Oct, one column over the farm (y255.7 -> 259.17, under a
/// structure this client sees thinner than the server does) was refused 9,500 times in five hours -
/// the bot was parked under it, and every new trip tried the same column again. Ways out of a
/// spot: 17 Oct, a climb from the farm was pulled back to y144.5 under something this client sees
/// as open air, and every trip for 16 hours flew the same hops straight up into it.
#[derive(Debug, Default)]
pub struct RefusedClips {
    clips: Vec<RefusedClip>,
    ways: Vec<RefusedWay>,
}

#[derive(Debug, Clone, Copy)]
struct RefusedClip {
    from: Vec3,
    to_y: f64,
    until: Instant,
}

#[derive(Debug, Clone, Copy)]
struct RefusedWay {
    from: Vec3,
    /// Unit vector.
    dir: Vec3,
    until: Instant,
}

impl RefusedClips {
    pub fn add(&mut self, from: Vec3, to_y: f64) {
        let now = Instant::now();
        self.clips.retain(|c| c.until > now);
        self.clips.push(RefusedClip { from, to_y, until: now + CLIP_REFUSED_FOR });
    }

    /// A refused clip within `r` of `at`'s column that started at most 4 below `at` and aimed
    /// above it: a clip up from `at` goes through the same thing.
    pub fn near(&self, at: Vec3, r: f64) -> bool {
        let now = Instant::now();
        self.clips.iter().any(|c| c.until > now && horiz(c.from, at) <= r && at.y >= c.from.y - 4.0 && at.y < c.to_y)
    }

    /// The server kept pulling us back to `from` while we went toward `to`: that way out of there
    /// is not taken again for a while. Returns false when it was already known.
    pub fn add_way(&mut self, from: Vec3, to: Vec3) -> bool {
        let Some(dir) = unit(to - from) else { return false };
        let now = Instant::now();
        self.ways.retain(|w| w.until > now);
        if self.ways.iter().any(|w| w.from.distance_to(from) < 1.0 && w.dir.dot(dir) >= SAME_WAY_COS) {
            return false;
        }
        self.ways.push(RefusedWay { from, dir, until: now + WAY_REFUSED_FOR });
        true
    }

    /// Going from `from` toward `to` is (about) a way the server refused out of (about) this spot.
    pub fn way_refused(&self, from: Vec3, to: Vec3) -> bool {
        let Some(dir) = unit(to - from) else { return false };
        let now = Instant::now();
        self.ways
            .iter()
            .any(|w| w.until > now && horiz(w.from, from) <= 1.5 && (w.from.y - from.y).abs() <= 2.0 && w.dir.dot(dir) >= SAME_WAY_COS)
    }

    pub fn minutes() -> u64 {
        CLIP_REFUSED_FOR.as_secs() / 60
    }

    pub fn way_minutes() -> u64 {
        WAY_REFUSED_FOR.as_secs() / 60
    }
}

fn unit(d: Vec3) -> Option<Vec3> {
    let len = d.length();
    (len >= 0.05).then(|| d * (1.0 / len))
}

fn horiz(a: Vec3, b: Vec3) -> f64 {
    ((a.x - b.x).powi(2) + (a.z - b.z).powi(2)).sqrt()
}

/// What one tick of a trip wants done.
#[derive(Debug)]
pub enum Step {
    /// Move here this tick (charge, then one position packet).
    Hop(Vec3),
    /// Nothing affordable this tick; try again next tick.
    Wait,
    Arrived,
    Failed(String),
}

#[derive(Debug)]
pub struct Trip {
    pub id: u64,
    pub dest: Vec3,
    goal: Vec3,
    legs: VecDeque<Vec3>,
    ticks: u32,
    limit: u32,
    stuck: u32,
    /// Where it was the last RECENT_TICKS ticks (the stuck test).
    recent: VecDeque<Vec3>,
    backoffs: u32,
    max_bps: f64,
    bps: f64,
    clean_ticks: u32,
    seen_setbacks: u32,
    seen_speed_setbacks: u32,
    /// Setbacks speed cannot explain, this trip.
    refusals: u32,
    setbacks_at_floor: u32,
    fallback_tried: bool,
    /// The floor a shaft was last looked for under (once per floor: it is a big search).
    shaft_floor: Option<f64>,
    /// Leg arrivals still to make exactly, not within ARRIVE_DIST (a shaft's legs: 1.25 off its
    /// column the descent hits the floor's edge, and 0.3 high under the floor the way across is
    /// blocked - both seen on 15 Oct).
    precise: u32,
    /// Headroom left over a floor clipped down through; widened when the server refuses a clip.
    clip_margin: f64,
    /// The setback count when the last clip went out.
    clip_setbacks: Option<u32>,
    /// The last clip up sent: from, the height it aimed for, the trip tick.
    clip_sent: Option<(Vec3, f64, u32)>,
    /// Steps aside made round refused clips, this trip.
    sidesteps: u32,
    pub label: String,
    /// No trip/arrival log lines (short hops a module makes all the time).
    pub quiet: bool,
    /// Via the cruise height when long, and allowed to back off upwards.
    cruise: bool,
    /// A flat box under a ceiling: never clip or escape UPWARD to get round an obstacle (that only
    /// strands it above a floor it then has to hurt itself clipping back down through) - go around
    /// at this height, or give up.
    contained: bool,
    /// How close counts as there on the last leg (ARRIVE_DIST; a formation slot wants tighter).
    arrive: f64,
}

impl Trip {
    /// `start_bps`: the speed the last trip ended at - setbacks are not forgotten by starting a
    /// new trip (15 Oct: a new collect trip a second, each back at 30 b/s into the same refusal).
    pub fn new(id: u64, from: Vec3, dest: Vec3, max_bps: f64, start_bps: f64, budget: &Budget, label: &str, cruise: bool, contained: bool) -> Self {
        let cruise = cruise && !contained;
        let dist = from.distance_to(dest);
        let mut legs = VecDeque::new();
        if cruise && dist >= CRUISE_MIN_DISTANCE {
            legs.push_back(Vec3::new(from.x, CRUISE_Y, from.z));
            legs.push_back(Vec3::new(dest.x, CRUISE_Y, dest.z));
        }
        legs.push_back(dest);
        let goal = legs.pop_front().unwrap();
        // Five minutes, or the time the whole route takes at the slowest speed plus five.
        let route = dist + 2.0 * CRUISE_Y;
        let limit = (20 * 60 * 5).max((route / (MIN_BPS / 20.0)) as u32 + 20 * 60 * 5);
        Self {
            id,
            dest,
            goal,
            legs,
            ticks: 0,
            limit,
            stuck: 0,
            recent: VecDeque::new(),
            backoffs: 0,
            max_bps,
            bps: start_bps.clamp(MIN_BPS, max_bps),
            clean_ticks: 0,
            seen_setbacks: budget.setbacks,
            seen_speed_setbacks: budget.speed_setbacks,
            refusals: 0,
            setbacks_at_floor: 0,
            fallback_tried: false,
            shaft_floor: None,
            precise: 0,
            clip_margin: 0.1,
            clip_setbacks: None,
            clip_sent: None,
            sidesteps: 0,
            label: label.to_string(),
            quiet: false,
            cruise,
            contained,
            arrive: ARRIVE_DIST,
        }
    }

    /// Arrival distance on the last leg (a steered trip: a formation slot).
    pub fn set_arrive(&mut self, d: f64) {
        self.arrive = d.max(0.05);
    }

    /// On its last leg (no cruise climb or shaft still ahead): the destination can move under it.
    pub fn on_last_leg(&self) -> bool {
        self.legs.is_empty() && self.precise == 0 && self.backoffs == 0
    }

    /// A moving destination (a formation slot, a followed player): the same trip, a new end - no
    /// restart, so the speed, the stuck counter and the refusal count carry on.
    pub fn retarget(&mut self, dest: Vec3) {
        // A slow target (a formation at 1-2 b/s) is followed at its own pace: under the stuck
        // test's 0.1 a tick, which is not being stuck.
        if dest.distance_to(self.dest) > 0.01 {
            self.stuck = 0;
        }
        self.dest = dest;
        self.goal = dest;
        self.fallback_tried = false;
    }

    pub fn goal(&self) -> Vec3 {
        self.goal
    }

    pub fn speed(&self) -> f64 {
        self.bps
    }

    /// Given up because the server kept refusing moves that speed cannot explain.
    pub fn refused_out(&self) -> bool {
        self.refusals >= MAX_REFUSALS
    }

    /// The server pulled us back to `server_pos`: the clip up just sent (from, aimed-for height),
    /// when that is where it started - the clip itself was refused, not a hop after it.
    pub fn refused_clip(&mut self, server_pos: Vec3) -> Option<(Vec3, f64)> {
        let (from, y, at) = self.clip_sent.take()?;
        (self.ticks.saturating_sub(at) <= 40 && server_pos.distance_to(from) < 0.5).then_some((from, y))
    }

    /// One tick. `pos` is where the server last accepted us; `health` decides what a clip down
    /// through a floor may cost.
    pub fn tick(&mut self, world: &World, pos: Vec3, budget: &Budget, clips: &RefusedClips, health: f32) -> Step {
        // Leg reached: next leg, or done.
        let reach = if self.precise > 0 { 0.05 } else if self.legs.is_empty() { self.arrive } else { ARRIVE_DIST };
        if pos.distance_to(self.goal) <= reach {
            self.precise = self.precise.saturating_sub(1);
            match self.legs.pop_front() {
                Some(next) => {
                    self.goal = next;
                    self.stuck = 0;
                    self.recent.clear();
                }
                None => return Step::Arrived,
            }
        }
        self.ticks += 1;
        if self.ticks > self.limit {
            return Step::Failed(format!("trip exceeded {} minutes", self.limit / 1200));
        }
        if let Some(why) = self.adapt_speed(budget) {
            return Step::Failed(why);
        }
        // Back where it was a few ticks ago is no progress either: 3 Nov, by the perimeter's food
        // chest, up 2 over something and straight back down every tick for minutes - never the
        // same spot two ticks running, so a test against the last tick alone never saw it stuck.
        if self.recent.iter().any(|l| l.distance_to(pos) < 0.1) {
            self.stuck += 1;
        } else {
            self.stuck = 0;
        }
        self.recent.push_back(pos);
        if self.recent.len() > RECENT_TICKS {
            self.recent.pop_front();
        }
        if self.stuck == STUCK_BACKOFF_TICKS && self.back_off_up(pos, "no progress") {
            return Step::Wait;
        }
        if self.stuck > STUCK_GIVEUP_TICKS {
            if self.back_off_up(pos, "no progress") {
                return Step::Wait;
            }
            return Step::Failed(format!("no progress for {} ticks - no legal route from here", self.stuck));
        }

        let cap = (self.bps / 20.0).max(0.05).min(budget.best_hop().max(STEP_ALWAYS_LEGAL));
        match self.charged_step(world, pos, cap, budget, clips, health) {
            Some(step) => step,
            None => {
                // Blocked. A destination inside blocks: the nearest clear spot, once.
                if !geom::stand_clear(world, self.goal) && !self.fallback_tried {
                    self.fallback_tried = true;
                    if let Some(free) = geom::nearest_clear(world, self.goal, 3) {
                        if self.quiet {
                            tracing::debug!(target: "goto", "destination {} is inside a block - using {}", fmt(self.goal), fmt(free));
                        } else {
                            tracing::info!(target: "goto", "destination {} is inside a block - using {}", fmt(self.goal), fmt(free));
                        }
                        self.goal = free;
                        if self.legs.is_empty() {
                            self.dest = free;
                        }
                        return Step::Wait;
                    }
                    return Step::Failed("the destination is inside blocks, with no clear space within 3 blocks of it".into());
                }
                if self.back_off_up(pos, "blocked") {
                    return Step::Wait;
                }
                Step::Failed("no clear way through to the destination from here".into())
            }
        }
    }

    /// Speed setbacks (after a bought hop) slow the trip 40% (floor 2 b/s); 60 clean ticks speed
    /// it back up 10%. Eight at the floor: something other than speed is refusing the moves. Any
    /// other setback is a move the server would not let through: MAX_REFUSALS of them in one trip
    /// and it gives up rather than push on into it.
    fn adapt_speed(&mut self, budget: &Budget) -> Option<String> {
        if budget.setbacks != self.seen_setbacks {
            self.seen_setbacks = budget.setbacks;
            if self.clip_setbacks.take().is_some() && self.clip_margin < 0.3 {
                self.clip_margin = 0.3;
                tracing::warn!(target: "goto", "a clip down was refused - leaving 0.3 headroom under floors from now on");
            }
            if budget.speed_setbacks == self.seen_speed_setbacks {
                self.refusals += 1;
                tracing::info!(target: "goto", "REFUSED (a short hop, not speed): {} this trip, speed stays {:.1} b/s", self.refusals, self.bps);
                if self.refused_out() {
                    return Some(format!("the server refused {} short moves on this trip - something in the way that this client does not see", self.refusals));
                }
                return None;
            }
            self.seen_speed_setbacks = budget.speed_setbacks;
            if self.bps <= MIN_BPS {
                self.setbacks_at_floor += 1;
                if self.setbacks_at_floor >= 8 {
                    return Some(format!(
                        "the server keeps pulling back even at {MIN_BPS:.0} b/s - something other than speed is refusing the moves"
                    ));
                }
            }
            let was = self.bps;
            self.bps = (self.bps * 0.6).max(MIN_BPS);
            self.clean_ticks = 0;
            tracing::info!(target: "goto", "SETBACK: {was:.1} -> {:.1} b/s", self.bps);
        } else {
            self.clean_ticks += 1;
            if self.clean_ticks >= 60 && self.bps < self.max_bps {
                self.bps = (self.bps * 1.1).min(self.max_bps);
                self.clean_ticks = 0;
            }
        }
        None
    }

    /// Up and across instead (GotoPilot.backOffUp): 16 blocks higher per try, the cruise height
    /// on the last.
    fn back_off_up(&mut self, pos: Vec3, why: &str) -> bool {
        // A direct trip (a hop inside a build) fails fast: its caller picks another target. Backing
        // off to the sky from under a build's overhang took 12 s a hop on 15 Oct.
        if !self.cruise || self.backoffs >= MAX_BACKOFFS {
            return false;
        }
        let alt = if self.backoffs + 1 >= MAX_BACKOFFS { CRUISE_Y } else { CRUISE_Y.min(pos.y + 16.0 * (self.backoffs + 1) as f64) };
        self.backoffs += 1;
        self.legs.clear();
        self.precise = 0;
        self.goal = Vec3::new(pos.x, alt, pos.z);
        self.legs.push_back(Vec3::new(self.dest.x, alt, self.dest.z));
        self.legs.push_back(self.dest);
        self.stuck = 0;
        self.recent.clear();
        tracing::info!(target: "goto", "BACK OFF #{} ({why}): up to y={alt:.0} at {}, then across", self.backoffs, fmt(pos));
        true
    }

    /// ChargedTravel.stepInternal: the hop for this tick, or None when blocked.
    fn charged_step(&mut self, world: &World, pos: Vec3, max_per_tick: f64, budget: &Budget, clips: &RefusedClips, health: f32) -> Option<Step> {
        let goal = self.goal;
        if !geom::stand_clear(world, goal) {
            return None;
        }
        let remaining = pos.distance_to(goal);
        let dy = goal.y - pos.y;
        let horiz = ((goal.x - pos.x).powi(2) + (goal.z - pos.z).powi(2)).sqrt();
        let column = Vec3::new(goal.x, pos.y, goal.z);
        let over_column = horiz <= 1.25;

        // Going down: across first at this height while that is clear, then sink over the column.
        if dy < -1.0 && !over_column && geom::path_clear(world, pos, column) && !clips.way_refused(pos, column) {
            let hop = max_per_tick.min(horiz);
            let next = if horiz <= hop { column } else { pos + (column - pos).normalize() * hop };
            return Some(Step::Hop(next));
        }
        if dy < -1.0 && over_column {
            let remaining_down = pos.y - goal.y;
            let safe = remaining_down.min(MAX_SAFE_DOWN);
            let clear = geom::clear_down(world, pos, safe);
            let next = if clear < safe - 0.05 {
                if clear > 0.2 {
                    Vec3::new(pos.x, pos.y - clear, pos.z)
                } else {
                    // A floor in the way. Clipping down through it bills the whole drop as a fall:
                    // 26.2 Entity.checkFallDamage adds the packet's dy to fallDistance BEFORE its
                    // on-ground check, so ground-spoofing does not save it (15 Oct: two ~5-block
                    // roof clips over the farm cost 3-5 health). Come in under it from an open
                    // shaft nearby instead; clip only into a sealed space.
                    // A drop of 3 or less is billed nothing: clip straight through.
                    let below = geom::first_stand_below(world, pos, goal.y, self.clip_margin);
                    if let Some(y) = below
                        && pos.y - y > FREE_DROP
                        && self.shaft_floor.is_none_or(|f| (f - pos.y).abs() > 1.0)
                    {
                        // Look for the floor's edge just under it, not at the goal's height: round
                        // a base the goal's height is walled in, so every ray stopped at once (15 Oct:
                        // two floors over the base cost 3 health on one trip).
                        self.shaft_floor = Some(pos.y);
                        let under = Vec3::new(pos.x, y, pos.z);
                        if let Some((h, bottom)) = geom::find_shaft(world, pos, under, 96) {
                            let top = Vec3::new(bottom.x, h, bottom.z);
                            tracing::info!(target: "goto", "floor under {} - across at y={h:.1} and down an open shaft at {} instead of clipping", fmt(pos), fmt(bottom));
                            self.legs.push_front(goal);
                            self.legs.push_front(under);
                            self.legs.push_front(bottom);
                            self.legs.push_front(top);
                            if h > pos.y + 0.01 {
                                self.goal = Vec3::new(pos.x, h, pos.z);
                                self.precise = 4;
                            } else {
                                self.goal = self.legs.pop_front().unwrap();
                                self.precise = 3;
                            }
                            return Some(Step::Wait);
                        }
                    }
                    return Some(match below {
                        Some(y) if pos.y - y >= 0.5 => {
                            let drop = pos.y - y;
                            if drop > FREE_DROP {
                                let bill = (drop - 3.0).ceil();
                                if bill > MAX_CLIP_BILL || health - (bill as f32) < CLIP_KEEP_HEALTH {
                                    return Some(Step::Failed(format!(
                                        "no open shaft, and clipping down {drop:.1} blocks through the floor would cost ~{bill:.0} fall damage at {health:.0} health"
                                    )));
                                }
                                tracing::warn!(target: "goto", "no open shaft - clipping down {drop:.1} blocks through the floor (the server bills ~{bill:.0} fall damage)");
                            }
                            self.clip(world, pos, y, budget)
                        }
                        _ => return None,
                    });
                }
            } else if remaining_down <= MAX_SAFE_DOWN + 0.05 && geom::path_clear(world, pos, goal) {
                goal
            } else {
                Vec3::new(pos.x, pos.y - safe, pos.z)
            };
            return Some(Step::Hop(next));
        }

        let hop = max_per_tick.min(remaining);
        let mut next = if remaining <= max_per_tick { goal } else { pos + (goal - pos).normalize() * hop };
        // A way the server kept pulling us back from: this client sees it open, the server does
        // not - never the same hops into it again.
        let refused_way = clips.way_refused(pos, next);
        if refused_way || !geom::path_clear(world, pos, next) {
            // Something low in the way (a block beside us on the farm): rise a little and go over
            // it, rather than clipping through whatever roof is 40 blocks up (15 Oct test: a melon
            // beside the start sent the first hop to y 156 and back).
            for h in [1.0, 2.0, 3.0, 4.0] {
                if refused_way {
                    break;
                }
                let up = Vec3::new(pos.x, pos.y + h, pos.z);
                if geom::path_clear(world, pos, up) {
                    let ahead = up + (goal - up).normalize() * max_per_tick.min(goal.distance_to(up));
                    if geom::path_clear(world, up, ahead) {
                        return Some(Step::Hop(up));
                    }
                } else {
                    break;
                }
            }
            // The server refused the clip up from here, or this way out: across to another
            // column and on from there.
            if refused_way || clips.near(pos, 1.5) {
                return match self.sidestep(world, pos, clips) {
                    Step::Failed(why) if dy > 1.0 && horiz <= 1.25 => Some(Step::Failed(why)),
                    // Nothing clear across either: straight up and over, unless up is refused too
                    // (30 Oct: down among the crops when the axe wore out, the way to the base was
                    // refused, and every retry for two hours gave up on the spot).
                    Step::Failed(why) if refused_way => {
                        let up = Vec3::new(pos.x, pos.y + 1.0, pos.z);
                        if !clips.way_refused(pos, up) && self.back_off_up(pos, "refused, nothing clear across") {
                            Some(Step::Wait)
                        } else {
                            Some(Step::Failed(why))
                        }
                    }
                    Step::Failed(_) => None,
                    s => Some(s),
                };
            }
            // A roof over the way up to a higher goal: clip through it (bounded by the goal, so even
            // contained this only rises as far as the goal - e.g. up to a stacked chest).
            if dy > 1.0 && horiz <= 1.25 {
                return Some(self.punch_up(world, pos, goal.y, budget));
            }
            // Contained: do NOT launch upward to get round an obstacle. In a flat roofed box the
            // only answer is to go around at this height - and if there is none, give up cleanly
            // (None -> the caller tries the next spot) rather than clip over the roof and strand
            // itself above a floor it would then have to hurt itself clipping back down through.
            if self.contained {
                return None;
            }
            // Anything else in the way: up through what is above and across from there.
            let up = self.punch_up(world, pos, goal.y.max(pos.y) + ESCAPE_UP, budget);
            return match up {
                Step::Failed(_) => None,
                s => Some(s),
            };
        }
        // Never more than MAX_SAFE_DOWN down in one hop.
        if pos.y - next.y > MAX_SAFE_DOWN {
            let k = MAX_SAFE_DOWN / (pos.y - next.y);
            next = pos + (next - pos) * k;
        }
        Some(Step::Hop(next))
    }

    fn punch_up(&mut self, world: &World, pos: Vec3, max_y: f64, budget: &Budget) -> Step {
        match geom::first_stand_above(world, pos, max_y.min(pos.y + ESCAPE_UP)) {
            Some(y) => self.clip(world, pos, y, budget),
            None => Step::Failed("boxed in - no open air above to clip into".into()),
        }
    }

    /// A vertical clip: legal through blocks because the server discards the vertical error, but
    /// it must fit in ONE packet (every midpoint would land inside the blocks).
    fn clip(&mut self, world: &World, pos: Vec3, y: f64, budget: &Budget) -> Step {
        let dest = Vec3::new(pos.x, y, pos.z);
        if !geom::stand_clear(world, dest) {
            return Step::Wait;
        }
        if (y - pos.y).abs() > budget.best_hop() {
            return Step::Wait;
        }
        tracing::info!(target: "goto", "clip {} through to y={y:.2}", if y > pos.y { "up" } else { "down" });
        if y < pos.y {
            self.clip_setbacks = Some(budget.setbacks);
        } else {
            self.clip_sent = Some((pos, y, self.ticks));
        }
        Step::Hop(dest)
    }

    /// Round a clip or a way the server refused: across to a column nearby - one open up to where
    /// this leg goes if there is one, else any whose clip has not been refused - and on from
    /// there. Toward the destination first, nearest first, at this height first; never a way out
    /// of here the server already refused, so each new try is a different one.
    fn sidestep(&mut self, world: &World, pos: Vec3, clips: &RefusedClips) -> Step {
        if self.sidesteps >= MAX_SIDESTEPS {
            return Step::Failed(format!("the server refused the way on at {} columns round {} - no way up from here", self.sidesteps + 1, fmt(pos)));
        }
        let goal = self.goal;
        let climbing = horiz(goal, pos) <= 1.25;
        // Contained: a roofed box has no 48 blocks of air overhead - only look for a column clear a
        // little above this height, not one open to the sky.
        let top = if climbing { goal.y } else if self.contained { goal.y.max(pos.y) + 2.0 } else { goal.y.max(pos.y) + ESCAPE_UP };
        let (dx, dz) = (self.dest.x - pos.x, self.dest.z - pos.z);
        let toward = if dx.abs() + dz.abs() < 1.0 { 0.0 } else { dz.atan2(dx) };
        // Straight at the destination, then fanning out either side.
        let mut order: Vec<i32> = (0..16).collect();
        order.sort_by_key(|i| (*i).min(16 - *i));
        let mut fallback = None;
        for d in SIDESTEP_DISTANCES {
            for drop in SIDESTEP_DROPS {
                for i in &order {
                    let a = toward + *i as f64 * TAU / 16.0;
                    let spot = Vec3::new(pos.x + a.cos() * d, pos.y - drop, pos.z + a.sin() * d);
                    if clips.near(spot, 2.5) || clips.way_refused(pos, spot) || !geom::path_clear(world, pos, spot) {
                        continue;
                    }
                    if geom::column_clear(world, Vec3::new(spot.x, top, spot.z), spot.y) {
                        return self.step_aside(pos, spot, climbing, "open above");
                    }
                    fallback.get_or_insert(spot);
                }
            }
        }
        match fallback {
            Some(spot) => self.step_aside(pos, spot, climbing, "a column not refused yet"),
            None => Step::Failed(format!("the server refused the way on at {} and there is no clear way across from there it has not refused", fmt(pos))),
        }
    }

    fn step_aside(&mut self, pos: Vec3, spot: Vec3, climbing: bool, why: &str) -> Step {
        self.sidesteps += 1;
        let goal = self.goal;
        // A straight climb moves to the new column; any other leg carries on from there.
        self.legs.push_front(if climbing { Vec3::new(spot.x, goal.y, spot.z) } else { goal });
        self.goal = spot;
        // Up exactly the column that was checked.
        self.precise += 1;
        self.stuck = 0;
        self.recent.clear();
        tracing::info!(target: "goto", "STEP ASIDE #{}: the server refused the way on at {} - across to {} ({why}) and on from there", self.sidesteps, fmt(pos), fmt(spot));
        Step::Wait
    }
}

pub fn fmt(v: Vec3) -> String {
    format!("{:.1} {:.1} {:.1}", v.x, v.y, v.z)
}

#[cfg(test)]
mod tests {
    use azalea::{
        BlockPos, Vec3,
        core::position::ChunkPos,
        world::{Chunk, ChunkStorage, PartialChunkStorage},
    };

    use super::*;

    /// Loaded, empty chunks round the origin with stone at `blocks`. The partial storage is
    /// returned to be kept alive: the world only holds weak references to its chunks.
    fn world_with(blocks: impl IntoIterator<Item = BlockPos>) -> (World, PartialChunkStorage) {
        let mut chunks = ChunkStorage::default();
        let mut partial = PartialChunkStorage::new(8);
        for x in -8..=8 {
            for z in -4..=4 {
                partial.set(&ChunkPos { x, z }, Some(Chunk::default()), &mut chunks);
            }
        }
        let world = World::from(chunks);
        for b in blocks {
            world.set_block_state(b, azalea::block::blocks::Stone.into());
        }
        (world, partial)
    }

    /// A trip run to its end with the server accepting every hop: the hops (from, to).
    fn run(t: &mut Trip, w: &World, pos: Vec3, clips: &RefusedClips) -> Vec<(Vec3, Vec3)> {
        run_hurt(t, w, pos, clips, 20.0).unwrap_or_else(|(at, why)| panic!("trip failed at {at:?}: {why}"))
    }

    /// The same at `health`, a failure returned: (where, why).
    fn run_hurt(t: &mut Trip, w: &World, mut pos: Vec3, clips: &RefusedClips, health: f32) -> Result<Vec<(Vec3, Vec3)>, (Vec3, String)> {
        let b = Budget::default();
        let mut hops = Vec::new();
        for _ in 0..3000 {
            match t.tick(w, pos, &b, clips, health) {
                Step::Hop(next) => {
                    hops.push((pos, next));
                    pos = next;
                }
                Step::Wait => {}
                Step::Arrived => return Ok(hops),
                Step::Failed(why) => return Err((pos, why)),
            }
        }
        panic!("trip still running after 3000 ticks, at {pos:?}");
    }

    /// The clips up (a hop of more than 2 up - flying hops are 1.5 at most).
    fn clips_up(hops: &[(Vec3, Vec3)]) -> Vec<(Vec3, Vec3)> {
        hops.iter().copied().filter(|(a, b)| b.y - a.y > 2.0).collect()
    }

    /// A roof two thick (y 102-103) over x/z -40..40, with a hole at block column `hole`.
    fn roof(hole: Option<(i32, i32)>) -> impl Iterator<Item = BlockPos> {
        (-40..=40)
            .flat_map(|x| (-40..=40).map(move |z| (x, z)))
            .filter(move |c| Some(*c) != hole)
            .flat_map(|(x, z)| [BlockPos::new(x, 102, z), BlockPos::new(x, 103, z)])
    }

    #[test]
    fn a_refused_clip_up_is_not_tried_again() {
        let (w, _keep) = world_with(roof(None));
        let b = Budget::default();
        let start = Vec3::new(0.5, 100.0, 0.5);
        let dest = Vec3::new(120.5, 100.0, 0.5);
        // Nothing refused: up through the roof where it stands.
        let hops = run(&mut Trip::new(1, start, dest, 30.0, 30.0, &b, "test", true, false), &w, start, &RefusedClips::default());
        let clips = clips_up(&hops);
        assert_eq!(clips.len(), 1, "{clips:?}");
        assert!(horiz(clips[0].0, start) < 0.01 && clips[0].1.y >= 104.0, "{clips:?}");
        // That clip refused: across under the roof first (toward the destination), and up there.
        let mut refused = RefusedClips::default();
        refused.add(start, clips[0].1.y);
        let hops = run(&mut Trip::new(2, start, dest, 30.0, 30.0, &b, "test", true, false), &w, start, &refused);
        let clips = clips_up(&hops);
        assert_eq!(clips.len(), 1, "{clips:?}");
        let (from, to) = clips[0];
        assert!(horiz(from, start) >= 7.9 && from.x > start.x, "clipped at {from:?}");
        assert!(to.y >= 104.0, "{to:?}");
    }

    #[test]
    fn a_step_aside_takes_an_open_column_when_there_is_one() {
        // A hole in the roof 16 out toward the destination.
        let (w, _keep) = world_with(roof(Some((16, 0))));
        let b = Budget::default();
        let start = Vec3::new(0.5, 100.0, 0.5);
        let dest = Vec3::new(120.5, 100.0, 0.5);
        let mut refused = RefusedClips::default();
        refused.add(start, 104.0);
        let hops = run(&mut Trip::new(1, start, dest, 30.0, 30.0, &b, "test", true, false), &w, start, &refused);
        assert!(clips_up(&hops).is_empty(), "clipped: {:?}", clips_up(&hops));
        assert!(hops.iter().any(|(_, p)| (p.x - 16.5).abs() < 0.01 && p.y > 102.0 && p.y < 104.0), "never went up through the hole");
    }

    #[test]
    fn a_refused_way_out_is_not_taken_again() {
        // 17 Oct: open air in this client's world, but the server pulled every climb from one spot
        // back to it, and for 16 hours every trip flew the same hops straight up again.
        let (w, _keep) = world_with(std::iter::empty());
        let b = Budget::default();
        let start = Vec3::new(0.5, 144.5, 0.5);
        let dest = Vec3::new(120.5, 140.0, 0.5);
        // Nothing refused: straight up from where it stands.
        let hops = run(&mut Trip::new(1, start, dest, 30.0, 30.0, &b, "test", true, false), &w, start, &RefusedClips::default());
        assert!(horiz(hops[0].1, start) < 0.01 && hops[0].1.y > start.y, "{:?}", hops[0]);
        // The way up refused: across first, and up from another column.
        let mut refused = RefusedClips::default();
        assert!(refused.add_way(start, Vec3::new(0.5, 147.5, 0.5)));
        assert!(!refused.add_way(start, Vec3::new(0.5, 150.5, 0.5)), "the same way twice");
        let hops = run(&mut Trip::new(2, start, dest, 30.0, 30.0, &b, "test", true, false), &w, start, &refused);
        let first = hops[0].1;
        assert!((first.y - start.y).abs() < 0.01 && horiz(first, start) > 0.1, "first hop {first:?}");
        assert!(hops.iter().any(|(a, p)| horiz(*a, start) >= 7.9 && p.y > a.y + 1.0), "never climbed elsewhere");
        // That way across refused as well: the next try is yet another way.
        assert!(refused.add_way(start, first));
        let hops = run(&mut Trip::new(3, start, dest, 30.0, 30.0, &b, "test", true, false), &w, start, &refused);
        let second = hops[0].1;
        assert!(!refused.way_refused(start, second), "{second:?}");
        assert!(unit(second - start).unwrap().dot(unit(first - start).unwrap()) < SAME_WAY_COS, "{first:?} then {second:?}");
        // Somewhere else entirely, the same heading is fine.
        assert!(!refused.way_refused(Vec3::new(20.5, 144.5, 0.5), Vec3::new(20.5, 147.5, 0.5)));
    }

    #[test]
    fn a_refused_way_with_nothing_clear_across_climbs_out() {
        // 30 Oct: down among the crops (melons at y100, glowstone at y101, all round its own
        // column), the way on refused - nothing clear across at any height it may step aside to.
        let crops = (-40..=40)
            .flat_map(|x| (-40..=40).map(move |z| (x, z)))
            .filter(|c| *c != (0, 0))
            .flat_map(|(x, z)| [BlockPos::new(x, 100, z), BlockPos::new(x, 101, z)]);
        let (w, _keep) = world_with(crops);
        let b = Budget::default();
        let start = Vec3::new(0.5, 100.2, 0.5);
        let dest = Vec3::new(15.5, 108.0, 0.5);
        let mut refused = RefusedClips::default();
        assert!(refused.add_way(start, dest));
        // Straight up out of the crops, then on.
        let hops = run(&mut Trip::new(1, start, dest, 30.0, 30.0, &b, "test", true, false), &w, start, &refused);
        assert!(horiz(hops[0].1, start) < 0.01 && hops[0].1.y > start.y, "{:?}", hops[0]);
        // Up refused as well: given up there, not flown into again.
        assert!(refused.add_way(start, Vec3::new(0.5, 103.2, 0.5)));
        let (at, _) = run_hurt(&mut Trip::new(2, start, dest, 30.0, 30.0, &b, "test", true, false), &w, start, &refused, 20.0).unwrap_err();
        assert!(at.distance_to(start) < 0.01, "{at:?}");
    }

    /// A sealed room (no shaft in), its inside x/z -2..2, y 100-103, under `thick` blocks of roof.
    fn sealed_room(thick: i32) -> impl Iterator<Item = BlockPos> {
        (-3..=3).flat_map(move |x| (-3..=3).flat_map(move |z| (99..104 + thick).map(move |y| BlockPos::new(x, y, z)))).filter(|b| {
            let inside = b.x.abs() <= 2 && b.z.abs() <= 2 && (100..=103).contains(&b.y);
            !inside
        })
    }

    #[test]
    fn a_clip_down_that_costs_too_much_health_is_not_made() {
        let b = Budget::default();
        let dest = Vec3::new(0.5, 100.0, 0.5);
        // 3 thick: a 4.9-block clip, ~2 billed - made at full health, not at 7.
        let (w, _keep) = world_with(sealed_room(3));
        let start = Vec3::new(0.5, 112.0, 0.5);
        let hops = run_hurt(&mut Trip::new(1, start, dest, 30.0, 30.0, &b, "test", false, false), &w, start, &RefusedClips::default(), 20.0).expect("clipped in");
        assert!(hops.iter().any(|(a, p)| a.y - p.y > FREE_DROP), "no clip down: {hops:?}");
        let (at, why) = run_hurt(&mut Trip::new(2, start, dest, 30.0, 30.0, &b, "test", false, false), &w, start, &RefusedClips::default(), 7.0).unwrap_err();
        assert!(at.y >= 106.9 && why.contains("fall damage"), "{at:?}: {why}");
        // 10 thick: ~9 billed - never, even at full health (22 Oct: clips billed 8-30 killed it).
        let (w, _keep) = world_with(sealed_room(10));
        let start = Vec3::new(0.5, 119.0, 0.5);
        let (at, why) = run_hurt(&mut Trip::new(3, start, dest, 30.0, 30.0, &b, "test", false, false), &w, start, &RefusedClips::default(), 20.0).unwrap_err();
        assert!(at.y >= 113.9 && why.contains("fall damage"), "{at:?}: {why}");
    }

    #[test]
    fn a_descent_goes_down_the_one_block_gap_between_ceiling_tiles() {
        // Two tiles three thick (y 102-104), the 1-block gap between them at x 20. Off the block
        // centre, so only a search over block columns finds the gap.
        let roof = (-40..=40)
            .flat_map(|x| (-40..=40).map(move |z| (x, z)))
            .filter(|(x, _)| *x != 20)
            .flat_map(|(x, z)| [102, 103, 104].map(|y| BlockPos::new(x, y, z)));
        let (w, _keep) = world_with(roof);
        let b = Budget::default();
        let start = Vec3::new(0.9, 110.0, 0.3);
        let dest = Vec3::new(0.9, 95.0, 0.3);
        let hops = run(&mut Trip::new(1, start, dest, 30.0, 30.0, &b, "test", false, false), &w, start, &RefusedClips::default());
        let clips: Vec<_> = hops.iter().filter(|(a, p)| a.y - p.y > FREE_DROP).collect();
        assert!(clips.is_empty(), "clipped down through the tile: {clips:?}");
        assert!(hops.iter().any(|(_, p)| (p.x - 20.5).abs() < 0.06 && p.y > 101.0 && p.y < 105.0), "never went down the gap");
    }

    #[test]
    fn a_trip_bouncing_between_two_spots_gives_up() {
        // 3 Nov: up 2 and straight back down by a chest, every tick for minutes.
        let (w, _keep) = world_with(std::iter::empty());
        let b = Budget::default();
        let (low, high) = (Vec3::new(0.4, 100.7, 0.4), Vec3::new(0.4, 102.7, 0.4));
        let mut t = Trip::new(1, low, Vec3::new(1.5, 100.0, 0.5), 30.0, 30.0, &b, "test", false, false);
        let clips = RefusedClips::default();
        let gave_up = (0..400u32).find(|i| matches!(t.tick(&w, if i % 2 == 0 { low } else { high }, &b, &clips, 20.0), Step::Failed(_)));
        assert!(gave_up.is_some_and(|i| i <= STUCK_GIVEUP_TICKS + RECENT_TICKS as u32), "gave up at {gave_up:?}");
    }

    #[test]
    fn refused_clips_only_cover_their_own_column_and_height() {
        let mut r = RefusedClips::default();
        r.add(Vec3::new(0.5, 255.7, 0.5), 259.2);
        assert!(r.near(Vec3::new(0.5, 255.7, 0.5), 1.5));
        assert!(r.near(Vec3::new(1.5, 253.0, 0.5), 1.5));
        assert!(!r.near(Vec3::new(3.5, 255.7, 0.5), 1.5));
        // Lower down the same column is a different roof; above the clip is past it.
        assert!(!r.near(Vec3::new(0.5, 120.0, 0.5), 1.5));
        assert!(!r.near(Vec3::new(0.5, 260.0, 0.5), 1.5));
    }

    #[test]
    fn only_speed_setbacks_slow_a_trip_and_refusals_end_it() {
        let mut b = Budget::default();
        let mut t = Trip::new(1, Vec3::ZERO, Vec3::new(5.0, 0.0, 0.0), 30.0, 30.0, &b, "test", false, false);
        // A refused short hop: the speed stays, and the trip carries on.
        b.setbacks += 1;
        assert!(t.adapt_speed(&b).is_none());
        assert_eq!(t.speed(), 30.0);
        // A pull-back after a bought hop: 40% slower.
        b.setbacks += 1;
        b.speed_setbacks += 1;
        assert!(t.adapt_speed(&b).is_none());
        assert!((t.speed() - 18.0).abs() < 1e-9, "speed {}", t.speed());
        // Five more refusals make six: given up, and marked as refused.
        for _ in 0..4 {
            b.setbacks += 1;
            assert!(t.adapt_speed(&b).is_none());
        }
        assert!(!t.refused_out());
        b.setbacks += 1;
        assert!(t.adapt_speed(&b).is_some());
        assert!(t.refused_out());
    }
}
