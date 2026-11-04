//! kill-aura: hit hostile mobs in range - the standalone's KillAura, the settings the melon routine
//! ran with (targets = the hostile list, range, max-targets 1, closest first). Never a player, never
//! anything not on the list. Waits for a full attack cooldown, and pauses while a meal has the hand.
//!
//! Why the bot needs it beyond its own skin: mobs spawn in the dark under a build's overhang and
//! stand in the very cells the printer is filling - a melon cannot be placed into a mob (15 Oct:
//! cells of core_planet's layer 118 "would not take", the Java printer gave up on that layer too).

use std::collections::HashSet;

use azalea::{Client, Vec3};
use tracing::{debug, info};

use super::ModuleConfig;

pub struct KillAura {
    targets: HashSet<String>,
    range: f64,
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
        info!(target: "kill-aura", "on: {} hostile kinds, range {}", targets.len(), cfg.get::<f64>(m, "range", 4.5));
        Some(Self { targets, range: cfg.get(m, "range", 4.5) })
    }

    pub fn tick(&self, bot: &Client, eating: bool) {
        if eating || bot.has_attack_cooldown() {
            return;
        }
        let Ok(pos) = bot.position() else { return };
        let eye = pos + Vec3::new(0.0, 1.62, 0.0);
        // One lock for the lot (azalea's nearest_entities_by nests two and deadlocked here).
        for (_, e) in crate::entities::near(bot) {
            let kind = e.kind;
            if e.player || !self.targets.contains(&kind.to_string()) {
                continue;
            }
            let p = e.pos;
            // Eye to the mob's middle (its box is about 0.6 wide, 1.8 tall).
            let d = eye.distance_to(p + Vec3::new(0.0, 0.9, 0.0));
            if d > self.range {
                // nearest first: nothing further on is in range either
                break;
            }
            if e.dead {
                continue;
            }
            debug!(target: "kill-aura", "hit {kind} at {:.1} {:.1} {:.1} ({d:.1} away)", p.x, p.y, p.z);
            bot.attack(e.entity);
            return;
        }
    }
}
