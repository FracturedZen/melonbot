//! auto-eat: eat when hungry (or hurt), pausing whatever needs the hand. Port of the standalone's
//! AutoEat, same settings: hunger-threshold, health-threshold, mode + items (blacklist or
//! whitelist), prefer, move-to-hotbar, restore-slot, give-up-ticks.
//!
//! Differences that matter on a bot:
//! - Food goes into an empty or tool-free hotbar slot, never over the Silk Touch axe (see
//!   inv::hold) - the Java swap into the held slot is what stranded the axe on 14 Oct.
//! - It looks straight up before the bite: azalea's use-item clicks the block under the crosshair
//!   first, and a chest there opens instead of the meal starting (the Java class note).
//! - No food at all backs off for give-up-ticks rather than asking every tick, so nothing that
//!   waits on a meal is held still by one that cannot happen.

use azalea::{Client, inventory::{ItemStack, components::Food}};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use super::ModuleConfig;
use crate::inv;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Prefer {
    Saturation,
    Nutrition,
    LeastWaste,
}

/// Auto-eat's idea of food, detached (the swarm's food errand runs outside the tick).
#[derive(Clone, Debug)]
pub struct FoodRule {
    whitelist: bool,
    items: Vec<String>,
}

impl FoodRule {
    pub fn edible(&self, st: &ItemStack) -> bool {
        if st.is_empty() || st.get_component::<Food>().is_none() {
            return false;
        }
        let listed = self.items.iter().any(|i| i == inv::stack_id(st));
        if self.whitelist { listed } else { !listed }
    }
}

pub struct AutoEat {
    hunger_threshold: u32,
    health_threshold: f32,
    whitelist: bool,
    items: Vec<String>,
    prefer: Prefer,
    move_to_hotbar: bool,
    restore_slot: bool,
    give_up_ticks: u64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The tick the current meal started, while one is under way.
    since: Option<u64>,
    prev_slot: Option<u8>,
    food_before: u32,
    held_before: i32,
    backoff_until: u64,
    warned_no_food: bool,
    /// Eat at the next tick whatever the thresholds say (tests, and the routine topping up).
    force: bool,
}

impl AutoEat {
    /// None when auto-eat is switched off in modules.json.
    pub fn from_config(cfg: &ModuleConfig) -> Option<Self> {
        if !cfg.enabled("auto-eat") {
            return None;
        }
        let m = "auto-eat";
        let prefer = match cfg.get::<String>(m, "prefer", "Saturation".into()).as_str() {
            "Nutrition" => Prefer::Nutrition,
            "LeastWaste" => Prefer::LeastWaste,
            _ => Prefer::Saturation,
        };
        let default_items = ["rotten_flesh", "spider_eye", "poisonous_potato", "pufferfish", "chorus_fruit", "suspicious_stew", "chicken"]
            .map(|s| format!("minecraft:{s}"))
            .to_vec();
        Some(Self {
            hunger_threshold: cfg.get(m, "hunger-threshold", 16),
            health_threshold: cfg.get(m, "health-threshold", 10.0),
            whitelist: cfg.get::<String>(m, "mode", "Blacklist".into()) == "Whitelist",
            items: cfg
                .get::<Vec<String>>(m, "items", default_items)
                .into_iter()
                .map(|s| s.strip_prefix("minecraft:").unwrap_or(&s).to_string())
                .collect(),
            prefer,
            move_to_hotbar: cfg.get(m, "move-to-hotbar", true),
            restore_slot: cfg.get(m, "restore-slot", true),
            give_up_ticks: cfg.get::<u64>(m, "give-up-ticks", 80),
            state: Mutex::new(State::default()),
        })
    }

    /// Auto-eat with sensible defaults, for a bot that has no modules.json (the stick-seller). Same
    /// values `from_config` would use with auto-eat enabled and nothing overridden.
    pub fn default_on() -> Self {
        Self {
            hunger_threshold: 16,
            health_threshold: 10.0,
            whitelist: false,
            items: ["rotten_flesh", "spider_eye", "poisonous_potato", "pufferfish", "chorus_fruit", "suspicious_stew", "chicken"]
                .map(|s| s.to_string())
                .to_vec(),
            prefer: Prefer::Saturation,
            move_to_hotbar: true,
            restore_slot: true,
            give_up_ticks: 80,
            state: Mutex::new(State::default()),
        }
    }

    /// Something auto-eat would eat.
    pub fn edible(&self, st: &ItemStack) -> bool {
        self.rule().edible(st)
    }

    /// What it eats, as a value an errand can carry off the tick.
    pub fn rule(&self) -> FoodRule {
        FoodRule { whitelist: self.whitelist, items: self.items.clone() }
    }

    /// Hunger points carried in food it would eat.
    pub fn food_points(&self, bot: &Client) -> i32 {
        inv::bags(bot)
            .iter()
            .filter(|(_, s)| self.edible(s))
            .map(|(_, s)| s.count() * s.get_component::<Food>().map_or(0, |f| f.nutrition))
            .sum()
    }

    /// Eat one item at the next tick if not full, thresholds or not.
    pub fn force(&self) {
        let mut st = self.state.lock();
        st.force = true;
        st.backoff_until = 0;
    }

    /// A meal is under way: anything that swaps the held item must wait.
    pub fn eating(&self) -> bool {
        self.state.lock().since.is_some()
    }

    pub fn tick(&self, bot: &Client, tick: u64) {
        let Ok(hunger) = bot.hunger() else { return };
        let food = hunger.food;
        let health = bot.health().unwrap_or(20.0);
        let mut st = self.state.lock();

        if let Some(since) = st.since {
            let held = bot.get_held_item().unwrap_or_default();
            // A tick or two for the swap and the use to land before the hand is judged.
            if tick.saturating_sub(since) < 3 {
                return;
            }
            // Judged by hunger: the bags' counts lag the server by a tick or two, and a stale stack
            // count read as "eaten" ended a meal before the bite (15 Oct test).
            let ate = food > st.food_before;
            let hand_changed = tick.saturating_sub(since) >= 10 && !self.edible(&held);
            let gave_up = hand_changed || tick.saturating_sub(since) > self.give_up_ticks;
            if ate || gave_up {
                if gave_up && !ate {
                    warn!(target: "auto-eat", "meal did not finish ({}) - backing off {} ticks",
                        if hand_changed { "the food left the hand" } else { "timed out" }, self.give_up_ticks);
                    st.backoff_until = tick + self.give_up_ticks;
                } else {
                    info!(target: "auto-eat", "ate - hunger {} -> {food}", st.food_before);
                }
                if self.restore_slot
                    && let Some(p) = st.prev_slot.take()
                {
                    bot.set_selected_hotbar_slot(p);
                }
                st.since = None;
            }
            return;
        }
        if tick < st.backoff_until {
            return;
        }
        // Any hurt under hunger 18 too: the server (Hard) heals only from 18 up, and between the
        // thresholds nothing ate (22 Oct: 12/20 health at hunger 17 for 24 minutes, into a death).
        let hurt = (self.health_threshold > 0.0 && health <= self.health_threshold) || (health < 20.0 && food < 18);
        let forced = std::mem::take(&mut st.force);
        if !(food <= self.hunger_threshold || hurt || forced) || food >= 20 {
            return;
        }
        debug!(target: "auto-eat", "wants to eat: hunger {food}/20 (threshold {}), health {health:.0} (threshold {}), {} food points carried",
            self.hunger_threshold, self.health_threshold, self.food_points(bot));

        // The best food by the preferred measure.
        let missing = 20 - food as i32;
        let best = inv::bags(bot)
            .into_iter()
            .filter(|(i, s)| self.edible(s) && (self.move_to_hotbar || inv::hotbar_index(*i).is_some()))
            .max_by(|(_, a), (_, b)| {
                let score = |s: &ItemStack| {
                    let f = s.get_component::<Food>().unwrap();
                    match self.prefer {
                        Prefer::Saturation => f.saturation as f64,
                        Prefer::Nutrition => f.nutrition as f64,
                        Prefer::LeastWaste => -((f.nutrition - missing).abs() as f64),
                    }
                };
                score(a).total_cmp(&score(b))
            });
        let Some((slot, stack)) = best else {
            if !st.warned_no_food {
                st.warned_no_food = true;
                warn!(target: "auto-eat", "hungry ({food}/20) with nothing to eat");
            }
            st.backoff_until = tick + self.give_up_ticks;
            return;
        };
        st.warned_no_food = false;
        let prev = bot.selected_hotbar_slot().ok();
        let Some(h) = inv::hold(bot, slot, &[]) else { return };
        // Look straight up so the use goes to the food, not a block under the crosshair.
        let yaw = bot.direction().map(|d| d.y_rot()).unwrap_or(0.0);
        let _ = bot.set_direction(yaw, -90.0);
        bot.start_use_item();
        info!(target: "auto-eat", "eating {} (hunger {food}/20, health {health:.0})", inv::stack_id(&stack));
        st.since = Some(tick);
        st.prev_slot = prev.filter(|p| *p != h);
        st.food_before = food;
        st.held_before = stack.count();
    }
}
