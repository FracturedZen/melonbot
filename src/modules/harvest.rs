//! melon-farmer, the harvest half: fly over the plot, break melon BLOCKS with a Silk Touch axe,
//! and pick up every drop within the collect deadline. Port of the standalone's MelonFarmer
//! (harvest + collect; no replant/till - the melon routine never used them), same settings:
//! collect-deadline, delay-ticks, cruise-height, retire-tool-at.
//!
//! Breaking is the Java farmer's raw START+STOP destroy pair, not a "mine" call: the server works
//! the break speed out from our on-ground claim (always on-ground - no 5x airborne penalty), so an
//! efficient axe breaks a melon on the START packet; anything slower parks in the server's single
//! delayed-destroy slot and is waited out before the next, since a second park throws the first
//! one away.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    core::direction::Direction,
    entity::{ActiveEffects, Attributes, FluidOnEyes, Physics, mining::get_mine_progress},
    interact::BlockStatePredictionHandler,
    protocol::packets::game::s_player_action::{Action, ServerboundPlayerAction},
    registry::builtin::{BlockKind, ItemKind},
};
use parking_lot::Mutex;
use tracing::{debug, info};

use super::ModuleConfig;
use crate::{config::Area, inv, movement};

/// Server reach, eye to block centre (4.5 + its 1.0 slack, less a margin).
const REACH: f64 = 4.5;
/// A drop chased this long without being picked up is skipped for DROP_SKIP (one wedged in a
/// block cell was re-targeted every 0.1 s on 15 Oct).
const DROP_TRY: Duration = Duration::from_secs(10);
const DROP_SKIP: Duration = Duration::from_secs(60);
/// A melon fired at is left alone this long: the server's block update takes a tick or three, and
/// re-firing at it meanwhile was 3 packets a melon (15 Oct: 448 break packets for ~150 melons).
const ECHO_TICKS: u64 = 10;
/// A goal this close to one the server refused counts as refused (a drop's stand spot moves a
/// little from one tick to the next).
const REFUSED_RADIUS: f64 = 1.5;
/// A melon the server will not let us over is left alone this long (a minute).
const REFUSED_SKIP_TICKS: u64 = 1200;

pub struct Harvester {
    farm: Area,
    cruise: f64,
    deadline: Duration,
    delay_ticks: u64,
    retire_at: i32,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    on: bool,
    /// No action before this tick (a parked break, or the delay between breaks).
    busy_until: u64,
    broken: u64,
    /// When each melon drop was first seen (by entity id), for the collect deadline.
    drops: HashMap<u32, Instant>,
    goal: Option<Vec3>,
    status: String,
    /// No break before this tick: an axe swap has to land on the server first, or the melon is
    /// broken with whatever it still thinks is in hand - and drops slices.
    axe_ready_at: u64,
    /// Melon slices carried when harvesting started (following meals down). Slices appearing
    /// while harvesting = melons broken without Silk Touch: stop at once.
    slices_base: Option<i32>,
    /// Melons fired at, and the tick until which they are left alone.
    fired: HashMap<BlockPos, u64>,
    /// The drop being chased (entity id) and since when.
    chasing: Option<(u32, Instant)>,
    /// Drops given up on, until when.
    skipped: HashMap<u32, Instant>,
}

impl Harvester {
    pub fn new(farm: Area, cfg: &ModuleConfig) -> Self {
        let m = "melon-farmer";
        Self {
            farm,
            cruise: cfg.get(m, "cruise-height", 2.0),
            // A little under the setting, so an overdue drop is fetched before it is late.
            deadline: Duration::from_secs_f64(cfg.get::<f64>(m, "collect-deadline", 45.0) * 0.8),
            delay_ticks: cfg.get(m, "delay-ticks", 2),
            retire_at: cfg.get(m, "retire-tool-at", 25),
            state: Mutex::new(State::default()),
        }
    }

    pub fn set_on(&self, on: bool) {
        let mut s = self.state.lock();
        s.on = on;
        s.goal = None;
        s.slices_base = None;
        s.status = if on { "on".into() } else { "off".into() };
    }

    pub fn on(&self) -> bool {
        self.state.lock().on
    }

    pub fn status(&self) -> String {
        let s = self.state.lock();
        format!("{} ({} broken, {} drops tracked)", s.status, s.broken, s.drops.len())
    }

    pub fn retire_at(&self) -> i32 {
        self.retire_at
    }

    pub fn broken(&self) -> u64 {
        self.state.lock().broken
    }

    fn in_farm(&self, x: f64, y: f64, z: f64, margin: f64) -> bool {
        let (a, b) = (self.farm.min, self.farm.max);
        x >= a.x as f64 - margin
            && x <= b.x as f64 + 1.0 + margin
            && z >= a.z as f64 - margin
            && z <= b.z as f64 + 1.0 + margin
            && y >= a.y as f64 - 2.0
            && y <= b.y as f64 + 3.0
    }

    /// The best usable Silk Touch axe in the bags: (menu slot, uses left).
    pub fn silk_axe(&self, bot: &Client) -> Option<(usize, i32)> {
        inv::bags(bot)
            .into_iter()
            .filter(|(_, s)| inv::stack_id(s).ends_with("_axe") && inv::enchant_level(bot, s, "silk_touch") > 0)
            .filter_map(|(i, s)| inv::uses_left(&s).map(|u| (i, u)))
            .filter(|(i, _)| !crate::routine::worn(&inv::bags(bot).into_iter().find(|(j, _)| j == i).map(|(_, s)| s).unwrap_or_default(), self.retire_at))
            .max_by_key(|(_, u)| *u)
    }

    fn stop(&self, why: &str) {
        let mut s = self.state.lock();
        if s.on {
            info!(target: "melon-farmer", "stopping: {why} ({} broken)", s.broken);
        }
        s.on = false;
        s.goal = None;
        s.status = why.to_string();
    }

    pub fn tick(&self, bot: &Client, tick: u64, eating: bool) {
        if !self.on() {
            return;
        }
        if eating {
            self.state.lock().status = "paused: eating".into();
            return;
        }
        // The axe in hand first - and never swing anything else at a melon.
        let Some((axe_slot, _)) = self.silk_axe(bot) else {
            return self.stop("no usable Silk Touch axe");
        };
        let held = bot.selected_hotbar_slot().unwrap_or(0) as usize + 36;
        if held != axe_slot {
            info!(target: "melon-farmer", "silk axe in slot {axe_slot}, holding slot {held} - taking it in hand");
            inv::hold(bot, axe_slot, &[]);
            self.state.lock().axe_ready_at = tick + 3;
            self.state.lock().status = "taking the axe in hand".into();
            return;
        }
        // Slices showing up: something other than the Silk Touch axe is breaking melons (on the Java
        // client 26 stacks of them filled the bags on 15 Oct). Stop rather than make more.
        let slices = inv::count_id(bot, "melon_slice");
        {
            let mut s = self.state.lock();
            let base = *s.slices_base.get_or_insert(slices);
            if slices < base {
                s.slices_base = Some(slices);
            } else if slices >= base + 3 {
                drop(s);
                return self.stop(&format!("melon slices appeared while harvesting ({base} -> {slices}) - melons are breaking without Silk Touch"));
            }
            if tick < s.axe_ready_at {
                return;
            }
        }
        // Full: a melon has nowhere to go.
        let room = inv::empty_slots(bot) > 0 || inv::bags(bot).iter().any(|(_, s)| inv::stack_id(s) == "melon" && s.count() < 64);
        if !room {
            return self.stop("bags full");
        }
        let Ok(pos) = bot.position() else { return };
        let eye = pos + Vec3::new(0.0, 1.62, 0.0);

        // Melon drops in the plot, oldest first past the deadline.
        let drops: Vec<(u32, Vec3)> = crate::entities::near(bot)
            .into_iter()
            .filter(|(_, e)| e.item == Some(ItemKind::Melon) && e.id >= 0)
            .map(|(_, e)| (e.id as u32, e.pos))
            .filter(|(_, p)| self.in_farm(p.x, p.y, p.z, 3.0))
            .collect();
        let drops: Vec<(u32, Vec3)> = {
            let mut s = self.state.lock();
            let now = Instant::now();
            s.skipped.retain(|_, until| *until > now);
            drops.into_iter().filter(|(id, _)| !s.skipped.contains_key(id)).collect()
        };
        let now = Instant::now();
        let overdue = {
            let mut s = self.state.lock();
            s.drops.retain(|id, _| drops.iter().any(|(d, _)| d == id));
            for (id, _) in &drops {
                s.drops.entry(*id).or_insert(now);
            }
            drops
                .iter()
                .filter(|(id, _)| s.drops.get(id).is_some_and(|t| now.duration_since(*t) >= self.deadline))
                .map(|(_, p)| *p)
                .next()
        };

        // The nearest melon in the plot not fired at a moment ago.
        let melon = {
            let Ok(world) = bot.world() else { return };
            let w = world.read();
            let states = BlockKind::Melon.into();
            let mut s = self.state.lock();
            s.fired.retain(|_, until| *until > tick);
            w.find_blocks(BlockPos::from(pos), &states)
                .filter(|b| self.in_farm(b.x as f64, b.y as f64, b.z as f64, 0.0) && !s.fired.contains_key(b))
                .take(1)
                .next()
        };

        if tick.is_multiple_of(40) {
            debug!(target: "melon-farmer", "at {:.1} {:.1} {:.1}: nearest melon {:?}, {} drops ({} overdue), {} empty slots, busy until {}",
                pos.x, pos.y, pos.z, melon, drops.len(), overdue.is_some() as u8, inv::empty_slots(bot), self.state.lock().busy_until);
        }
        // Collect as it goes: an overdue drop first; else, with no melon in reach, whichever is
        // nearer - the next melon or a drop (the drops lie where it has just been breaking).
        let melon_in_reach = melon.is_some_and(|m| eye.distance_to(Vec3::new(m.x as f64 + 0.5, m.y as f64 + 0.5, m.z as f64 + 0.5)) <= REACH);
        let nearest_drop = drops.iter().map(|(_, p)| *p).min_by(|a, b| a.distance_to(pos).total_cmp(&b.distance_to(pos)));
        let drop_id = |p: Vec3| drops.iter().find(|(_, q)| *q == p).map(|(id, _)| *id);
        let drop_first = !melon_in_reach
            && nearest_drop.is_some_and(|d| melon.is_none_or(|m| d.distance_to(pos) <= Vec3::new(m.x as f64 + 0.5, m.y as f64, m.z as f64 + 0.5).distance_to(pos)));
        if let Some(drop) = overdue.or(if drop_first { nearest_drop } else { None }) {
            // The same drop chased too long: skip it a while, say so once.
            if let Some(id) = drop_id(drop) {
                let mut s = self.state.lock();
                match s.chasing {
                    Some((c, since)) if c == id => {
                        if since.elapsed() > DROP_TRY {
                            info!(target: "melon-farmer", "drop at {:.1} {:.1} {:.1} not picked up after {}s of trying - skipping it for {}s",
                                drop.x, drop.y, drop.z, DROP_TRY.as_secs(), DROP_SKIP.as_secs());
                            s.skipped.insert(id, Instant::now() + DROP_SKIP);
                            s.chasing = None;
                            return;
                        }
                    }
                    _ => s.chasing = Some((id, Instant::now())),
                }
            }
            if self.state.lock().goal.is_none_or(|g| g.distance_to(drop) >= 1.0) {
                debug!(target: "melon-farmer", "collect drop at {:.1} {:.1} {:.1} ({})", drop.x, drop.y, drop.z, if overdue.is_some() { "overdue" } else { "nearer than the next melon" });
            }
            // Where the server's pickup box (the player's, grown 1 sideways and 0.5 up/down) takes
            // the item: feet at most 0.75 above it, within a block sideways. The nearest open
            // block was often one up - never in pickup range (15 Oct: drops "not picked up").
            let stand = bot.world().ok().and_then(|w| {
                let w = w.read();
                let mut best: Option<(f64, Vec3)> = None;
                for dy in [0.0, 0.25, 0.5, 0.7] {
                    for dx in [-0.9, -0.45, 0.0, 0.45, 0.9] {
                        for dz in [-0.9, -0.45, 0.0, 0.45, 0.9] {
                            let c = Vec3::new(drop.x + dx, drop.y + dy, drop.z + dz);
                            let d = dx * dx + dz * dz + dy * dy;
                            if best.is_none_or(|(bd, _)| d < bd) && movement::geom::stand_clear(&w, c) {
                                best = Some((d, c));
                            }
                        }
                    }
                }
                best.map(|(_, c)| c)
            });
            let goal = stand.unwrap_or(drop);
            // The server would not let us down to it a moment ago: skip it now, not after DROP_TRY
            // of being pulled back (16 Oct: 55 setbacks for one drop).
            if movement::refused_near(bot, goal, REFUSED_RADIUS) || movement::refused_near(bot, drop, REFUSED_RADIUS) {
                if let Some(id) = drop_id(drop) {
                    let mut s = self.state.lock();
                    info!(target: "melon-farmer", "drop at {:.1} {:.1} {:.1}: the server will not let us down to it - skipping it for {}s",
                        drop.x, drop.y, drop.z, DROP_SKIP.as_secs());
                    s.skipped.insert(id, Instant::now() + DROP_SKIP);
                    s.chasing = None;
                    s.goal = None;
                }
                return;
            }
            self.go(bot, goal, "collect");
            return;
        }
        let Some(m) = melon else {
            // Far from the plot it is not loaded, so it only looks empty: go and see.
            let (a, b) = (self.farm.min, self.farm.max);
            let centre = Vec3::new((a.x + b.x) as f64 / 2.0 + 0.5, b.y as f64 + 3.0, (a.z + b.z) as f64 / 2.0 + 0.5);
            if pos.distance_to(centre) > 64.0 {
                self.go(bot, centre, "to the farm");
                return;
            }
            self.state.lock().status = "plot empty - waiting for melons".into();
            return;
        };
        let centre = Vec3::new(m.x as f64 + 0.5, m.y as f64 + 0.5, m.z as f64 + 0.5);
        if eye.distance_to(centre) <= REACH {
            if tick >= self.state.lock().busy_until {
                self.break_block(bot, m, tick);
            }
            return;
        }
        let goal = Vec3::new(centre.x, m.y as f64 + self.cruise, centre.z);
        if movement::refused_near(bot, goal, REFUSED_RADIUS) {
            let mut s = self.state.lock();
            if !s.fired.contains_key(&m) {
                info!(target: "melon-farmer", "melon at {} {} {}: the server will not let us over it - leaving it a minute", m.x, m.y, m.z);
            }
            s.fired.insert(m, tick + REFUSED_SKIP_TICKS);
            s.goal = None;
            return;
        }
        self.go(bot, goal, "harvest");
    }

    /// Start (or keep) a trip to `goal`, without restarting it every tick.
    fn go(&self, bot: &Client, goal: Vec3, what: &str) {
        let mut s = self.state.lock();
        s.status = what.to_string();
        if s.goal.is_some_and(|g| g.distance_to(goal) < 1.0) && movement::busy(bot) {
            return;
        }
        s.goal = Some(goal);
        drop(s);
        movement::go(bot, goal, what);
    }

    fn break_block(&self, bot: &Client, pos: BlockPos, tick: u64) {
        let Ok(world) = bot.world() else { return };
        let block = world.read().get_block_state(pos).unwrap_or_default();
        if block.is_air() {
            return;
        }
        // The server's speed: as azalea works it out, but on the ground - our claim. The held item
        // is read BEFORE the query: inside it the world lock is held, and asking the client for
        // anything else there deadlocks (15 Oct: the first live harvest hung for 3 minutes).
        let held = bot.get_held_item().unwrap_or_default();
        let per_tick = bot
            .query_self::<(&FluidOnEyes, &Physics, &Attributes, &ActiveEffects), _>(|(f, p, a, e)| {
                let mut ground = p.clone();
                ground.set_on_ground(true);
                get_mine_progress(block.to_trait(), &held, f, &ground, a, e)
            })
            .unwrap_or(0.0);
        if per_tick <= 0.0 {
            debug!(target: "melon-farmer", "refuse: {} {} {} cannot be broken with what is held", pos.x, pos.y, pos.z);
            return;
        }
        send_break(bot, pos);
        let wait = if per_tick >= 1.0 { self.delay_ticks } else { (1.0 / per_tick).ceil() as u64 + 1 };
        debug!(target: "melon-farmer", "harvest {} {} {} perTick={per_tick:.2} {} - next in {wait} ticks", pos.x, pos.y, pos.z, if per_tick >= 1.0 { "instant" } else { "parked" });
        let mut s = self.state.lock();
        s.busy_until = tick + wait.max(1);
        s.fired.insert(pos, tick + ECHO_TICKS.max(wait));
        s.broken += 1;
        s.status = "harvest".into();
    }
}

/// The raw START+STOP destroy pair, with azalea's block-prediction sequence numbers. What breaks
/// depends on what the server thinks is in hand (the Silk Touch axe for blocks, an empty or
/// tool-free hand for slices).
/// START (true) or STOP (false) breaking alone: a slow block is START, then STOP once its
/// progress passes the server's 0.7 (the swarm nuker).
pub fn send_action(bot: &Client, pos: BlockPos, start: bool) {
    let seq = bot.query_self::<&mut BlockStatePredictionHandler, _>(|mut h| h.start_predicting()).unwrap_or(0);
    let action = if start { Action::StartDestroyBlock } else { Action::StopDestroyBlock };
    bot.write_packet(ServerboundPlayerAction { action, pos, direction: Direction::Up, seq });
}

pub fn send_break(bot: &Client, pos: BlockPos) {
    let seq = |bot: &Client| bot.query_self::<&mut BlockStatePredictionHandler, _>(|mut h| h.start_predicting()).unwrap_or(0);
    bot.write_packet(ServerboundPlayerAction { action: Action::StartDestroyBlock, pos, direction: Direction::Up, seq: seq(bot) });
    bot.write_packet(ServerboundPlayerAction { action: Action::StopDestroyBlock, pos, direction: Direction::Up, seq: seq(bot) });
}
