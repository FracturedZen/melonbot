//! kill-aura: hit hostile mobs in range - the standalone's KillAura, the settings the melon routine
//! ran with (targets = the hostile list, range, max-targets 1, closest first). Never a player, never
//! anything not on the list. Waits for a full attack cooldown, and pauses while a meal has the hand.
//!
//! Why the bot needs it beyond its own skin: mobs spawn in the dark under a build's overhang and
//! stand in the very cells the printer is filling - a melon cannot be placed into a mob (15 Oct:
//! cells of core_planet's layer 118 "would not take", the Java printer gave up on that layer too).
//!
//! auto-weapon (the swarm's bots; the user, 5 Nov: a mace in the hotbar "to kill mobs that get
//! close enough to damage"): with a mob in range the best weapon carried - a mace, else a sword -
//! is taken in hand and the hit waits for its full charge (the server starts the charge again when
//! the hand changes, so a hit straight after the swap does a fraction of the damage).
//!
//! With a mace, mobs out to mace-range are struck instead (movement::Strike): the bot drops onto
//! one from above in a single tick and back, so the mace hits as a smash - a one-hit kill for most
//! mobs - and nothing gets near enough to hurt it (5 Nov: a pillager shot a bot dead from beyond
//! reach). The drop is kept to what it lives through if the smash misses.
//!
//! Strikes stay where the bot may go: inside the world border, and inside the box a perimeter bot
//! clears (6 Nov, the user: "trying to attack mobs outside of border, meaning it cant get it and is
//! stuck in a loop" - a bot struck at pillagers past the border 26,000 times in a day, the
//! server putting it back each time). A mob still standing after a few strikes is left alone a
//! while, whatever kept them from landing.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use azalea::{Client, Vec3, inventory::ItemStack, world::World};
use parking_lot::Mutex;
use tracing::{debug, info};

use super::ModuleConfig;
use crate::{
    inv,
    movement::{self, Strike, geom},
};

/// The highest drop a strike makes: a smash of 6 + 12 + 10 + 7 = 35 damage at 15 blocks.
const MAX_DROP: f64 = 15.0;
/// Below this drop no strike: the smash would add too little.
const MIN_DROP: f64 = 4.0;
/// Health a missed smash must leave - it is billed as a fall of the drop less 3.
const KEEP_HEALTH: f64 = 8.0;
/// Not the same mob again sooner (it is hurt-proof for half a second), nor any strike sooner.
const SAME_MOB_GAP: Duration = Duration::from_millis(600);
const STRIKE_GAP: Duration = Duration::from_millis(250);
/// The weapon is in hand this long before a strike (the server has it there by then).
const HAND_SETTLE: Duration = Duration::from_millis(120);
/// A weapon with this many uses left or fewer is not used (a broken one is gone).
const WEAPON_RETIRE: i32 = 10;
/// Strikes at one mob that leave it standing before it is left alone (a smash kills most in one,
/// a ravager in three), and for how long.
const MOB_STRIKES: u32 = 4;
const LEFT_ALONE: Duration = Duration::from_secs(60);
/// A strike's spot keeps this far inside the world border, and this far inside the bounds.
const BORDER_MARGIN: f64 = 1.0;
const BOUNDS_MARGIN: f64 = 0.5;

/// Where strikes may go, flat: a perimeter bot's box. The world border bounds them always.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub min_x: f64,
    pub min_z: f64,
    pub max_x: f64,
    pub max_z: f64,
}

impl Bounds {
    pub fn holds(&self, p: Vec3, margin: f64) -> bool {
        p.x >= self.min_x + margin && p.x <= self.max_x - margin && p.z >= self.min_z + margin && p.z <= self.max_z - margin
    }
}

pub struct KillAura {
    targets: HashSet<String>,
    range: f64,
    auto_weapon: bool,
    /// The hotbar slot and item in hand, and since when (its charge).
    held: Mutex<Option<(u8, &'static str, Instant)>>,
    /// Mobs out to this far are struck when a mace is carried.
    mace_range: f64,
    /// The last strike: at which mob, when.
    struck: Mutex<Option<(i32, Instant)>>,
    /// Strikes at each mob since it was last left alone: how many, the last, and until when it is
    /// left alone now.
    strikes_at: Mutex<HashMap<i32, (u32, Instant, Option<Instant>)>>,
}

/// A weapon kill-aura takes in hand: a mace first, then the best sword.
pub fn weapon_rank(st: &ItemStack) -> Option<i32> {
    let id = inv::stack_id(st);
    if id == "mace" {
        return Some(10);
    }
    let sword = id.strip_suffix("_sword")?;
    Some(match sword {
        "netherite" => 6,
        "diamond" => 5,
        "iron" => 4,
        "golden" => 3,
        "stone" => 2,
        _ => 1,
    })
}

/// The bag slot of the best weapon carried (not one about to break), and what it is.
pub fn best_weapon(bot: &Client) -> Option<(usize, &'static str)> {
    inv::bags(bot)
        .into_iter()
        .filter(|(_, s)| inv::uses_left(s).is_none_or(|u| u > WEAPON_RETIRE))
        .filter_map(|(i, s)| weapon_rank(&s).map(|r| (i, r, inv::stack_id(&s))))
        .max_by_key(|(_, r, _)| *r)
        .map(|(i, _, id)| (i, id))
}

/// Where a strike on a mob at `mob` (feet) drops onto - just above it, else beside it - and from
/// what height, when the whole way there from `me` is open air and the spot is one `ok` allows.
pub fn plan_strike(world: &World, me: Vec3, mob: Vec3, drop: f64, ok: impl Fn(Vec3) -> bool) -> Option<Strike> {
    let spots = [Vec3::new(0.0, 2.1, 0.0), Vec3::new(1.3, 0.0, 0.0), Vec3::new(-1.3, 0.0, 0.0), Vec3::new(0.0, 0.0, 1.3), Vec3::new(0.0, 0.0, -1.3)];
    spots.into_iter().map(|o| mob + o).filter(|spot| ok(*spot)).find_map(|spot| {
        let s = Strike { entity: 0, spot, top: spot.y + drop };
        let legs = movement::strike_legs(me, &s);
        let clear = geom::stand_clear(world, spot) && geom::path_clear(world, me, legs[0]) && geom::path_clear(world, legs[0], legs[1]) && geom::path_clear(world, legs[1], legs[2]);
        clear.then_some(s)
    })
}

/// Seconds a hit takes to charge fully with `id` in hand (20 ticks over the attack speed).
fn charge_secs(id: &str) -> f64 {
    let speed = if id == "mace" {
        0.6
    } else if id.ends_with("_sword") {
        1.6
    } else if id.ends_with("_axe") || id.ends_with("_shovel") {
        1.0
    } else if id.ends_with("_pickaxe") {
        1.2
    } else {
        4.0
    };
    1.0 / speed
}

impl KillAura {
    pub fn from_config(cfg: &ModuleConfig) -> Option<Self> {
        if !cfg.enabled("kill-aura") {
            return None;
        }
        let m = "kill-aura";
        let targets: HashSet<String> = cfg.get::<Vec<String>>(m, "targets", Vec::new()).into_iter().collect();
        if targets.is_empty() {
            return None;
        }
        let auto_weapon = cfg.get(m, "auto-weapon", false);
        info!(target: "kill-aura", "on: {} hostile kinds, range {}, auto-weapon {auto_weapon}", targets.len(), cfg.get::<f64>(m, "range", 4.5));
        let mace_range = cfg.get(m, "mace-range", 24.0);
        Some(Self {
            targets,
            range: cfg.get(m, "range", 4.5),
            auto_weapon,
            held: Mutex::new(None),
            mace_range,
            struck: Mutex::new(None),
            strikes_at: Mutex::new(HashMap::new()),
        })
    }

    /// Hits with whatever is in hand (a bot of its own: the farmer's hand is its axe).
    pub fn hand_only(mut self) -> Self {
        self.auto_weapon = false;
        self
    }

    /// True while a mob is in range and kill-aura has the hand (auto-weapon): the caller leaves
    /// the hand alone this tick. Strikes stay inside `bounds` (a perimeter bot's box) and the
    /// world border.
    pub fn tick(&self, bot: &Client, eating: bool, bounds: Option<Bounds>) -> bool {
        if eating {
            return false;
        }
        let Ok(pos) = bot.position() else { return false };
        let eye = pos + Vec3::new(0.0, 1.62, 0.0);
        let weapon = if self.auto_weapon { best_weapon(bot) } else { None };
        // Strikes: a mace, no chest open (a strike would carry the bot out of its reach), and a
        // drop it lives through if the smash misses.
        let health = bot.health().unwrap_or(0.0) as f64;
        let drop = weapon
            .filter(|(_, id)| *id == "mace")
            .filter(|_| inv::no_container_open(bot))
            .map(|_| (health + 3.0 - KEEP_HEALTH).min(MAX_DROP))
            .filter(|d| *d >= MIN_DROP);
        let reach = if drop.is_some() { self.mace_range.max(self.range) } else { self.range };
        // One lock for the lot (azalea's nearest_entities_by nests two and deadlocked here).
        let mut targets = Vec::new();
        for (_, e) in crate::entities::near(bot) {
            if e.player || !self.targets.contains(&e.kind.to_string()) {
                continue;
            }
            // Eye to the mob's middle (its box is about 0.6 wide, 1.8 tall).
            let d = eye.distance_to(e.pos + Vec3::new(0.0, 0.9, 0.0));
            if d > reach {
                // nearest first: nothing further on is in range either
                break;
            }
            if !e.dead {
                targets.push((e, d));
            }
        }
        if targets.is_empty() {
            return false;
        }
        let (Ok(h), held) = (bot.selected_hotbar_slot(), inv::stack_id(&bot.get_held_item().unwrap_or_default())) else { return false };
        let since = {
            let mut g = self.held.lock();
            match *g {
                Some((s, i, t)) if s == h && i == held => t,
                _ => {
                    let now = Instant::now();
                    *g = Some((h, held, now));
                    now
                }
            }
        };

        if let (Some(drop), Some((slot, _))) = (drop, weapon) {
            let last = *self.struck.lock();
            let border = crate::border::get(bot);
            let allowed = |p: Vec3, margin: f64| border.is_none_or(|b| b.holds(p, margin.max(BORDER_MARGIN))) && bounds.is_none_or(|b| b.holds(p, margin));
            let now = Instant::now();
            let left_alone: HashSet<i32> = {
                let mut s = self.strikes_at.lock();
                s.retain(|_, (_, at, until)| until.is_some_and(|u| now < u) || now.duration_since(*at) < LEFT_ALONE);
                s.iter().filter(|(_, (_, _, until))| until.is_some_and(|u| now < u)).map(|(id, _)| *id).collect()
            };
            let plan = bot.world().ok().and_then(|world| {
                let w = world.read();
                targets
                    .iter()
                    .filter(|(e, _)| !last.is_some_and(|(id, at)| id == e.id && at.elapsed() < SAME_MOB_GAP))
                    .filter(|(e, _)| !left_alone.contains(&e.id) && allowed(e.pos, 0.0))
                    .find_map(|(e, d)| plan_strike(&w, pos, e.pos, drop, |spot| allowed(spot, BOUNDS_MARGIN)).map(|s| (Strike { entity: e.id, ..s }, e.kind, *d)))
            });
            if let Some((s, kind, d)) = plan {
                if inv::hotbar_index(slot) != Some(h) {
                    inv::hold(bot, slot, &[]);
                    return true;
                }
                if since.elapsed() < HAND_SETTLE || last.is_some_and(|(_, at)| at.elapsed() < STRIKE_GAP) {
                    return true;
                }
                debug!(target: "kill-aura", "strike {kind} at {:.1} {:.1} {:.1} ({d:.1} away, {:.0}-block drop)", s.spot.x, s.spot.y, s.spot.z, s.top - s.spot.y);
                let id = s.entity;
                if movement::strike(bot, s) {
                    *self.struck.lock() = Some((id, now));
                    let mut all = self.strikes_at.lock();
                    let e = all.entry(id).or_insert((0, now, None));
                    e.0 += 1;
                    e.1 = now;
                    if e.0 >= MOB_STRIKES {
                        info!(target: "kill-aura", "{kind} still standing after {} strikes - left alone for {} s", e.0, LEFT_ALONE.as_secs());
                        *e = (0, now, Some(now + LEFT_ALONE));
                    }
                }
                return true;
            }
        }

        // An ordinary hit on the nearest in reach.
        let Some((e, d)) = targets.into_iter().find(|(_, d)| *d <= self.range) else { return false };
        let mut fighting = false;
        if let Some((slot, _)) = weapon {
            fighting = true;
            if inv::hotbar_index(slot) != Some(h) {
                inv::hold(bot, slot, &[]);
                return true;
            }
        }
        if bot.has_attack_cooldown() || (fighting && since.elapsed().as_secs_f64() < charge_secs(held)) {
            return fighting;
        }
        let p = e.pos;
        debug!(target: "kill-aura", "hit {} with {held} at {:.1} {:.1} {:.1} ({d:.1} away)", e.kind, p.x, p.y, p.z);
        bot.attack(e.entity);
        fighting
    }
}

#[cfg(test)]
mod tests {
    use azalea::{
        BlockPos,
        core::position::ChunkPos,
        registry::builtin::ItemKind,
        world::{Chunk, ChunkStorage, PartialChunkStorage},
    };

    use super::*;
    use crate::print::schem::parse_state;

    fn world(stone: &[BlockPos]) -> (World, PartialChunkStorage) {
        let mut chunks = ChunkStorage::default();
        let mut partial = PartialChunkStorage::new(4);
        for x in -2..=2 {
            for z in -2..=2 {
                partial.set(&ChunkPos { x, z }, Some(Chunk::default()), &mut chunks);
            }
        }
        let w = World::from(chunks);
        for p in stone {
            w.set_block_state(*p, parse_state("minecraft:stone").unwrap());
        }
        (w, partial)
    }

    #[test]
    fn a_strike_drops_onto_a_mob_in_the_open_and_not_through_a_roof() {
        // Floor at y 63; the bot at 10 0 0, a pillager 12 blocks off at 22 0 0, both on the floor.
        let floor: Vec<BlockPos> = (0..32).flat_map(|x| (-4..4).map(move |z| BlockPos::new(x, 63, z))).collect();
        let (w, _keep) = world(&floor);
        let me = Vec3::new(10.5, 64.0, 0.5);
        let mob = Vec3::new(22.5, 64.0, 0.5);
        let s = plan_strike(&w, me, mob, 12.0, |_| true).expect("open air all the way");
        assert_eq!(s.spot, mob + Vec3::new(0.0, 2.1, 0.0), "onto the mob from above");
        assert!((s.top - (64.0 + 2.1 + 12.0)).abs() < 1e-9, "a 12-block drop");
        // A roof 4 blocks over the bot: no way up, no strike.
        let roof: Vec<BlockPos> = floor.iter().map(|p| BlockPos::new(p.x, 68, p.z)).chain(floor.iter().copied()).collect();
        let (w, _keep) = world(&roof);
        assert!(plan_strike(&w, me, mob, 12.0, |_| true).is_none());
    }

    #[test]
    fn no_strike_past_the_bounds() {
        let floor: Vec<BlockPos> = (0..32).flat_map(|x| (-4..4).map(move |z| BlockPos::new(x, 63, z))).collect();
        let (w, _keep) = world(&floor);
        let me = Vec3::new(10.5, 64.0, 0.5);
        let mob = Vec3::new(22.5, 64.0, 0.5);
        // The box ends at x 20: the mob is past it, and so is every spot by it.
        let b = Bounds { min_x: 0.0, min_z: -4.0, max_x: 20.0, max_z: 4.0 };
        assert!(!b.holds(mob, 0.0));
        assert!(plan_strike(&w, me, mob, 12.0, |spot| b.holds(spot, BOUNDS_MARGIN)).is_none());
        // Inside it: struck as before.
        let near = Vec3::new(15.5, 64.0, 0.5);
        assert!(plan_strike(&w, me, near, 12.0, |spot| b.holds(spot, BOUNDS_MARGIN)).is_some());
    }

    #[test]
    fn a_mace_before_any_sword_and_no_tools() {
        let r = |k| weapon_rank(&ItemStack::new(k, 1));
        assert!(r(ItemKind::Mace) > r(ItemKind::NetheriteSword));
        assert!(r(ItemKind::NetheriteSword) > r(ItemKind::IronSword));
        assert_eq!(r(ItemKind::NetheritePickaxe), None);
        assert_eq!(r(ItemKind::NetheriteAxe), None);
        assert!((charge_secs("mace") - 1.0 / 0.6).abs() < 1e-9);
    }
}
