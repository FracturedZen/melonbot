//! A perimeter bot's base and kit. The base: the chests signed "tools", "repair" and "food" by the
//! box, recorded where the bot is logged in (each account starts there, so it can record the
//! chests and the place). The kit: a pickaxe that is not worn, a shovel and an
//! axe - worn tools go into Repair and fresh ones come out of Tools, as the melon farmer does with
//! its axes - fill in the offhand, and room in the bags (what is dug is not kept).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    entity::inventory::Inventory,
    inventory::{
        ItemStack,
        components::{Food, MaxDamage},
        operations::ThrowClick,
    },
};
use tracing::info;

use super::{Base, JobConfig, Perimeter};
use crate::{
    chests,
    config::Pos,
    inv, movement,
    swarm::{BotLink, SwarmCtx, errand, orders::now_ms},
};

fn bp(p: Pos) -> BlockPos {
    BlockPos::new(p.x, p.y, p.z)
}

fn pos(b: BlockPos) -> Pos {
    Pos { x: b.x, y: b.y, z: b.z }
}

fn at(p: Option<Pos>) -> String {
    p.map_or("none".into(), |p| format!("{} {} {}", p.x, p.y, p.z))
}

/// The signed chests within 24 blocks of where the bot is (chests::signed's reach): its base, when
/// one of them is the tools chest. Saved when it changed; `say`: the panel asked, so the answer
/// goes to the feed either way.
pub fn record(bot: &Client, ctx: &SwarmCtx, p: &Perimeter, name: &str, say: bool) {
    let Ok(here) = bot.position() else { return };
    let spot = BlockPos::new(here.x.floor() as i32, here.y.floor() as i32, here.z.floor() as i32);
    let (tools_word, repair_word) = {
        let j = p.job.read();
        (j.tools_sign.trim().to_lowercase(), j.repair_sign.trim().to_lowercase())
    };
    let food_word = ctx.cfg.read().food_sign.trim().to_lowercase();
    let Some(tools) = chests::signed(bot, spot, &tools_word) else {
        if say {
            ctx.feed("say", &format!("{name}: no chest signed \"{tools_word}\" within 24 blocks - its base stays as it was"));
        }
        return;
    };
    let signed = |w: &str| (!w.is_empty()).then(|| chests::signed(bot, spot, w)).flatten().map(pos);
    let new = Base { tools: pos(tools), repair: signed(&repair_word), food: signed(&food_word), at: pos(spot), when: now_ms() };
    let changed = p.bases.lock().get(name).is_none_or(|b| b.tools != new.tools || b.repair != new.repair || b.food != new.food);
    let text = format!("{name}: base - tools {}, repair {}, food {}", at(Some(new.tools)), at(new.repair), at(new.food));
    if changed {
        p.bases.lock().insert(name.to_string(), new);
        p.save_bases();
        // Remembered for every bot, wherever it is when it needs them (the food errand looks there).
        for w in [&tools_word, &repair_word, &food_word] {
            errand::signed_chest(bot, ctx, w, 32.0);
        }
        info!(target: "perimeter", "{text} (recorded)");
        ctx.feed("event", &format!("{text} (recorded)"));
    } else if say {
        ctx.feed("event", &format!("{text} (as recorded)"));
    }
}

/// Worn: this many uses left or fewer - not used, and into Repair at the next tools trip.
pub fn worn(st: &ItemStack, retire: i32) -> bool {
    inv::uses_left(st).is_some_and(|u| u <= retire)
}

pub fn has_pickaxe(bot: &Client, retire: i32) -> bool {
    inv::bags(bot).iter().any(|(_, s)| inv::stack_id(s).ends_with("_pickaxe") && !worn(s, retire))
}

fn has_usable(bot: &Client, kind: &str, retire: i32) -> bool {
    inv::bags(bot).iter().any(|(_, s)| inv::stack_id(s).ends_with(kind) && !worn(s, retire))
}

/// The rest of the kit: without them dirt, gravel and wood go by hand, 5 to 10 times slower.
const KIT: [&str; 2] = ["_shovel", "_axe"];
/// A shovel or an axe short and none in the tools chest at the last look: looked for again this often.
pub const KIT_RECHECK: Duration = Duration::from_secs(15 * 60);

/// The first of the kit the bags have no usable one of ("shovel", "axe").
pub fn kit_short(bot: &Client, retire: i32) -> Option<&'static str> {
    KIT.iter().find(|k| !has_usable(bot, k, retire)).map(|k| &k[1..])
}

/// No usable pickaxe: off to the base. Not more often than every 2 minutes, so an empty tools
/// chest is not visited over and over.
pub fn tools_errand(bot: &Client, ctx: &Arc<SwarmCtx>, p: &Perimeter, link: &Arc<BotLink>) {
    if link.errand_running() || link.tools_retry_at.lock().is_some_and(|t| Instant::now() < t) {
        link.perimeter.lock().status = "waiting to look in the tools chest again".into();
        return;
    }
    *link.tools_retry_at.lock() = Some(Instant::now() + Duration::from_secs(120));
    let job = p.job.read().clone();
    let here = bot.position().unwrap_or(Vec3::ZERO);
    let (tools, repair) = match p.base_for(&link.name, here) {
        Some(b) => (Some(bp(b.tools)), b.repair.map(bp)),
        None => (errand::signed_chest(bot, ctx, &job.tools_sign, f64::MAX), errand::signed_chest(bot, ctx, &job.repair_sign, f64::MAX)),
    };
    let Some(tools) = tools else {
        ctx.warn_once(&format!(
            "{} has no usable pickaxe and no base recorded - log it in by the chest signed \"{}\" (or press Record base there)",
            link.name, job.tools_sign
        ));
        return;
    };
    link.start_errand("fetching tools", fetch_tools(bot.clone(), tools, repair, job));
}

/// At the base: worn tools into Repair (into Tools when there is no repair chest), then the best
/// pickaxe, shovel and axe the bags lack out of Tools. Room is made first.
async fn fetch_tools(bot: Client, tools: BlockPos, repair: Option<BlockPos>, job: JobConfig) -> Result<String, String> {
    movement::stop(&bot);
    make_room(&bot, &job, 4).await;
    let retire = job.retire_uses;
    let is_worn = |s: &ItemStack| errand::is_digging_tool(s) && worn(s, retire);
    let n = inv::bags(&bot).iter().filter(|(_, s)| is_worn(s)).count();
    let stored = if n > 0 { chests::store(&bot, repair.unwrap_or(tools), is_worn, n).await? } else { 0 };
    let mut got = 0;
    for kind in ["_pickaxe", "_shovel", "_axe"] {
        if has_usable(&bot, kind, retire) {
            continue;
        }
        got += chests::take(&bot, tools, |s| inv::stack_id(s).ends_with(kind) && !worn(s, retire), errand::tool_rank, 1).await?;
    }
    let into = if repair.is_some() { "Repair" } else { "Tools (no repair chest)" };
    if !has_pickaxe(&bot, retire) {
        return Err(format!(
            "no usable pickaxe in the tools chest at {} {} {} - waiting for repaired ones ({stored} worn tool(s) put in {into})",
            tools.x, tools.y, tools.z
        ));
    }
    let short: Vec<&str> = KIT.iter().filter(|k| !has_usable(&bot, k, retire)).map(|k| &k[1..]).collect();
    let short = if short.is_empty() {
        String::new()
    } else {
        format!(" - no usable {} in the tools chest (looked for again every {} min)", short.join(" or "), KIT_RECHECK.as_secs() / 60)
    };
    Ok(format!("took {got} tool(s); {stored} worn one(s) into {into}{short}"))
}

// ------------------------------------------------------------------ fill

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// The offhand holds fill.
    Ready,
    /// Fill is being swapped into the offhand (there next tick).
    Swapping,
    /// None in the bags.
    Out,
}

/// Fill in the offhand - the best in the bags is swapped in when it holds none.
pub fn fill_ready(bot: &Client, job: &JobConfig) -> Fill {
    let Some(menu) = inv::menu(bot) else { return Fill::Out };
    let offhand = menu.slots().get(45).cloned().unwrap_or_default();
    if !offhand.is_empty() && job.is_fill(inv::stack_id(&offhand)) {
        return Fill::Ready;
    }
    let rank = |s: &ItemStack| (job.fill.iter().position(|f| f == inv::stack_id(s)).unwrap_or(usize::MAX), -s.count());
    let best = inv::bags(bot).into_iter().filter(|(_, s)| !s.is_empty() && job.is_fill(inv::stack_id(s))).min_by_key(|(_, s)| rank(s));
    match best {
        Some((slot, _)) if inv::swap_with(bot, slot, inv::OFFHAND_BUTTON) => Fill::Swapping,
        _ => Fill::Out,
    }
}

/// The nearest dropped fill within `r` blocks.
pub fn fill_lying_near(bot: &Client, job: &JobConfig, r: f64) -> Option<Vec3> {
    crate::entities::near(bot).into_iter().find(|(d, e)| *d <= r && e.item.is_some_and(|k| job.is_fill(inv::id(k)))).map(|(_, e)| e.pos)
}

// ------------------------------------------------------------------ room

/// Kept whatever happens: tools, weapons and armour, food, and what a bot was given to carry.
fn kept(st: &ItemStack) -> bool {
    let id = inv::stack_id(st);
    st.get_component::<MaxDamage>().is_some() || st.get_component::<Food>().is_some() || id == "totem_of_undying" || id == "ender_chest" || id.ends_with("shulker_box")
}

/// A stack to throw out: anything dug that is not fill first, then the smallest fill stacks past
/// the ones kept.
pub fn junk_slot(bot: &Client, job: &JobConfig) -> Option<usize> {
    let bags = inv::bags(bot);
    if let Some((slot, _)) = bags.iter().find(|(_, s)| !s.is_empty() && !kept(s) && !job.is_fill(inv::stack_id(s))) {
        return Some(*slot);
    }
    let mut fill: Vec<(usize, i32)> = bags.iter().filter(|(_, s)| !s.is_empty() && job.is_fill(inv::stack_id(s))).map(|(i, s)| (*i, s.count())).collect();
    fill.sort_by_key(|(_, n)| -n);
    fill.get(job.keep_fill_stacks..).and_then(|rest| rest.iter().min_by_key(|(_, n)| *n)).map(|(i, _)| *i)
}

/// Throw a bag stack out (ctrl+Q). Not while a container is open.
pub fn throw(bot: &Client, slot: usize) -> bool {
    if bot.query_self::<&Inventory, _>(|i| i.id).ok() != Some(0) {
        return false;
    }
    match bot.get_inventory() {
        Ok(menu) => {
            menu.click(ThrowClick::All { slot: slot as u16 });
            true
        }
        Err(_) => false,
    }
}

/// Throw junk out until `free` bag slots are empty, or there is no more junk. Returns the stacks
/// thrown.
pub async fn make_room(bot: &Client, job: &JobConfig, free: usize) -> usize {
    let mut thrown = 0;
    while inv::empty_slots(bot) < free && thrown < 36 {
        let Some(slot) = junk_slot(bot, job) else { break };
        if !throw(bot, slot) {
            break;
        }
        thrown += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if thrown > 0 {
        info!(target: "perimeter", "threw out {thrown} stack(s) to make room");
    }
    thrown
}
