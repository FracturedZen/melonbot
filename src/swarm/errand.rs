//! Errands: a swarm bot leaves its order for a while to fetch food, fetch tools or empty its bags,
//! then carries on. While one runs, the bot's order and its nuker leave the movement alone.
//!
//! The chests: food from a food chest set by the controller, or a chest with
//! a sign saying a word you choose; tools from what they carry, or a tools chest if one is
//! provided nearby; storage only when collecting blocks. So a chest is either one the
//! panel set at the controller player's spot (food), or the nearest container under a sign with
//! the word - from the signs every swarm bot has seen, remembered in swarm/places.json.

use std::{
    collections::BTreeMap,
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::{BlockPos, Client, Vec3, inventory::ItemStack, inventory::components::Food};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, info, warn};

use super::{BotLink, SwarmCtx};
use crate::{chests, config::Pos, inv, movement};

/// What a bot is off doing.
pub struct Errand {
    pub what: String,
    pub since: Instant,
    pub task: tokio::task::JoinHandle<()>,
}

/// Chests found under signs, by sign word: kept so a chest any bot saw is known to all of them
/// after its chunk unloads, and after a restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Places {
    pub signed: BTreeMap<String, Vec<Pos>>,
}

impl Places {
    pub fn load(ctx_dir: &std::path::Path) -> Self {
        fs::read_to_string(ctx_dir.join("places.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn remember(&mut self, word: &str, at: BlockPos) -> bool {
        let list = self.signed.entry(word.to_string()).or_default();
        let p = Pos { x: at.x, y: at.y, z: at.z };
        if list.contains(&p) {
            return false;
        }
        list.push(p);
        true
    }
}

fn bp(p: Pos) -> BlockPos {
    BlockPos::new(p.x, p.y, p.z)
}

fn centre(p: BlockPos) -> Vec3 {
    Vec3::new(p.x as f64 + 0.5, p.y as f64 + 0.5, p.z as f64 + 0.5)
}

/// Every sign the swarm's bots have seen (each bot keeps the ones in its own chunks).
fn all_signs(bot: &Client) -> Vec<(BlockPos, String)> {
    let mut ecs = bot.ecs.write();
    let mut q = ecs.query::<&chests::Signs>();
    let mut out: Vec<(BlockPos, String)> = q.iter(&ecs).flat_map(|s| s.0.iter().map(|(p, t)| (*p, t.clone()))).collect();
    out.sort_by_key(|(p, _)| (p.x, p.y, p.z));
    out.dedup_by_key(|(p, _)| *p);
    out
}

/// The nearest container under a sign saying `word` within `radius` of the bot: from the signs
/// seen now, else the ones remembered. Found ones are remembered.
pub fn signed_chest(bot: &Client, ctx: &SwarmCtx, word: &str, radius: f64) -> Option<BlockPos> {
    let word = word.trim().to_lowercase();
    if word.is_empty() {
        return None;
    }
    let here = bot.position().ok()?;
    let mut found: Vec<BlockPos> = Vec::new();
    for (sign, text) in all_signs(bot) {
        // "repair tools" is not the tools chest.
        if !text.contains(&word) || (word == "tools" && text.contains("repair")) {
            continue;
        }
        if let Some(c) = chests::container_for_sign(bot, sign) {
            found.push(c);
        }
    }
    if !found.is_empty() {
        let mut places = ctx.places.lock();
        let mut changed = false;
        for c in &found {
            changed |= places.remember(&word, *c);
        }
        if changed {
            let text = serde_json::to_string_pretty(&*places).unwrap_or_default();
            drop(places);
            let _ = fs::write(ctx.dir.join("places.json"), text + "\n");
            info!(target: "swarm", "chest signed \"{word}\" remembered");
        }
    }
    let remembered: Vec<BlockPos> = ctx.places.lock().signed.get(&word).map(|v| v.iter().map(|p| bp(*p)).collect()).unwrap_or_default();
    found.into_iter().chain(remembered).filter(|c| centre(*c).distance_to(here) <= radius).min_by(|a, b| centre(*a).distance_to(here).total_cmp(&centre(*b).distance_to(here)))
}

/// The container nearest a point the panel set (the controller player's spot when it was set),
/// within 4 blocks. None while that spot is not loaded.
fn chest_near(bot: &Client, at: BlockPos) -> Option<BlockPos> {
    let world = bot.world().ok()?;
    let w = world.read();
    let mut best: Option<(i32, BlockPos)> = None;
    for dx in -4..=4 {
        for dy in -3..=3 {
            for dz in -4..=4 {
                let p = BlockPos::new(at.x + dx, at.y + dy, at.z + dz);
                if w.get_block_state(p).is_some_and(|s| chests::is_container(azalea::registry::builtin::BlockKind::from(s))) {
                    let d = dx * dx + dy * dy + dz * dz;
                    if best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, p));
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Where this bot gets food: the chest the panel set, else the nearest signed one. None = it eats
/// what it carries.
pub fn food_source(bot: &Client, ctx: &SwarmCtx) -> Option<FoodSource> {
    let (set, word) = {
        let c = ctx.cfg.read();
        (c.food_chest, c.food_sign.clone())
    };
    if let Some(p) = set {
        return Some(FoodSource::Set(bp(p)));
    }
    signed_chest(bot, ctx, &word, f64::MAX).map(FoodSource::Signed)
}

#[derive(Clone, Copy, Debug)]
pub enum FoodSource {
    /// The spot set from the controller (or typed): the chest is found there on arrival.
    Set(BlockPos),
    Signed(BlockPos),
}

impl BotLink {
    pub fn errand_running(&self) -> bool {
        self.errand.lock().as_ref().is_some_and(|e| !e.task.is_finished())
    }

    pub fn errand_what(&self) -> Option<String> {
        self.errand.lock().as_ref().filter(|e| !e.task.is_finished()).map(|e| e.what.clone())
    }

    /// Run `fut` as this bot's errand (one at a time); its order waits meanwhile.
    pub fn start_errand<F>(self: &Arc<Self>, what: &str, fut: F)
    where
        F: std::future::Future<Output = Result<String, String>> + Send + 'static,
    {
        let mut slot = self.errand.lock();
        if slot.as_ref().is_some_and(|e| !e.task.is_finished()) {
            return;
        }
        info!(target: "swarm", "errand: {what}");
        let span = tracing::info_span!("bot", name = %self.name);
        let what_s = what.to_string();
        let ctx = super::ctx().cloned();
        let name = self.name.clone();
        let task = tokio::spawn(
            async move {
                let r = fut.await;
                match &r {
                    Ok(done) => info!(target: "swarm", "errand done: {done}"),
                    Err(e) => warn!(target: "swarm", "errand \"{what_s}\" failed: {e}"),
                }
                if let Some(ctx) = ctx {
                    ctx.feed(if r.is_ok() { "event" } else { "say" }, &format!("{name}: {}", r.unwrap_or_else(|e| format!("{what_s} failed - {e}"))));
                }
            }
            .instrument(span),
        );
        *slot = Some(Errand { what: what.to_string(), since: Instant::now(), task });
    }

    pub fn stop_errand(&self) {
        if let Some(e) = self.errand.lock().take() {
            e.task.abort();
        }
    }
}

/// Fetch food from the food chest: up to `stacks` stacks of what auto-eat would eat.
pub async fn fetch_food(bot: Client, ctx: Arc<SwarmCtx>, source: FoodSource, rule: crate::modules::autoeat::FoodRule, stacks: usize) -> Result<String, String> {
    movement::stop(&bot);
    let chest = match source {
        FoodSource::Signed(c) => c,
        FoodSource::Set(at) => match chest_near(&bot, at) {
            Some(c) => c,
            None => {
                // Not loaded from here: go and look.
                movement::fly_to(&bot, centre(at) + Vec3::new(0.0, 1.0, 0.0), "food chest").await?;
                tokio::time::sleep(Duration::from_millis(500)).await;
                chest_near(&bot, at).ok_or_else(|| format!("no chest within 4 blocks of the food chest spot {} {} {}", at.x, at.y, at.z))?
            }
        },
    };
    let _ = &ctx;
    let got = chests::take(
        &bot,
        chest,
        |s| rule.edible(s),
        |s| s.count() as i64 * s.get_component::<Food>().map_or(0, |f| f.nutrition as i64),
        stacks,
    )
    .await?;
    if got == 0 {
        return Err(format!("the food chest at {} {} {} had nothing to eat (or the bags are full)", chest.x, chest.y, chest.z));
    }
    Ok(format!("took {got} stack(s) of food"))
}

/// After a death: fly to where it died and pass over every item lying there (they are picked up
/// within a block or so), a few rounds, then carry on. Counted, not assumed.
pub async fn recover_items(bot: Client, at: Vec3) -> Result<String, String> {
    movement::stop(&bot);
    let before: i32 = inv::bags(&bot).iter().map(|(_, s)| s.count()).sum();
    movement::fly_to(&bot, at + Vec3::new(0.0, 0.5, 0.0), "recover").await?;
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let items: Vec<Vec3> = crate::entities::near(&bot).into_iter().filter(|(d, e)| e.item.is_some() && *d <= 8.0).map(|(_, e)| e.pos).collect();
        if items.is_empty() {
            break;
        }
        for p in items.into_iter().take(12) {
            let _ = movement::fly_to(&bot, p + Vec3::new(0.0, 0.2, 0.0), "recover").await;
        }
    }
    let after: i32 = inv::bags(&bot).iter().map(|(_, s)| s.count()).sum();
    Ok(format!("picked up {} item(s) where it died", (after - before).max(0)))
}

/// Tool kinds a nuker uses.
pub fn is_digging_tool(st: &ItemStack) -> bool {
    let id = inv::stack_id(st);
    id.ends_with("_pickaxe") || id.ends_with("_shovel") || id.ends_with("_axe") || id.ends_with("_hoe") || id == "shears"
}

pub(super) fn tool_rank(st: &ItemStack) -> i64 {
    let id = inv::stack_id(st);
    let tier = if id.starts_with("netherite") {
        6
    } else if id.starts_with("diamond") {
        5
    } else if id.starts_with("iron") {
        4
    } else if id.starts_with("golden") {
        3
    } else if id.starts_with("stone") {
        2
    } else {
        1
    };
    tier * 10_000 + inv::uses_left(st).unwrap_or(0) as i64
}

/// Take digging tools from a tools chest: one of each kind the bags lack (pickaxe, shovel, axe) -
/// the best - so the next bot finds some too (16 Oct test: the first bot took three pickaxes).
pub async fn fetch_tools(bot: Client, chest: BlockPos) -> Result<String, String> {
    movement::stop(&bot);
    let have: Vec<&'static str> = inv::bags(&bot).iter().filter(|(_, s)| is_digging_tool(s)).map(|(_, s)| kind_of(inv::stack_id(s))).collect();
    let mut got = 0;
    for kind in ["_pickaxe", "_shovel", "_axe"] {
        if have.contains(&kind) {
            continue;
        }
        got += chests::take(&bot, chest, |s| inv::stack_id(s).ends_with(kind), tool_rank, 1).await?;
    }
    if got == 0 {
        return Err(format!("the tools chest at {} {} {} had no tools the bags lack", chest.x, chest.y, chest.z));
    }
    Ok(format!("took {got} tool(s)"))
}

fn kind_of(id: &'static str) -> &'static str {
    for k in ["_pickaxe", "_shovel", "_axe", "_hoe"] {
        if id.ends_with(k) {
            return k;
        }
    }
    id
}

/// Empty the bags into a storage chest: everything but tools, weapons and food.
pub async fn store_haul(bot: Client, chest: BlockPos, rule: Option<crate::modules::autoeat::FoodRule>) -> Result<String, String> {
    movement::stop(&bot);
    let keep = move |s: &ItemStack| {
        let id = inv::stack_id(s);
        is_digging_tool(s) || id.ends_with("_sword") || rule.as_ref().is_some_and(|r| r.edible(s))
    };
    let moved = chests::store(&bot, chest, |s| !s.is_empty() && !keep(s), 36).await?;
    Ok(format!("stored {moved} stack(s)"))
}
