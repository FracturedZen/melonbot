//! A perimeter bot's base and kit. The base: the chests signed "tools", "repair" and "food" by the
//! box, recorded where the bot is logged in (each account starts there, so it can record the
//! chests and the place). The kit: a pickaxe that is not worn, a shovel and an
//! axe, one of each - worn tools go into Repair, spares back into Tools, and fresh ones come out
//! of Tools, as the melon farmer does with its axes - fill in the offhand, and room in the bags
//! (what is dug is not kept).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    core::entity_id::MinecraftEntityId,
    inventory::{
        ItemStack,
        components::{Food, MaxDamage},
    },
    protocol::packets::game::s_player_command::{Action, ServerboundPlayerCommand},
    registry::builtin::BlockKind,
};
use tracing::{info, warn};

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
    let all = |w: &str| if w.is_empty() { Vec::new() } else { chests::signed_all(bot, spot, w).into_iter().map(pos).collect::<Vec<_>>() };
    let all_tools = all(&tools_word);
    let Some(&tools) = all_tools.first() else {
        if say {
            ctx.feed("say", &format!("{name}: no chest signed \"{tools_word}\" within 24 blocks - its base stays as it was"));
        }
        return;
    };
    let all_repair = all(&repair_word);
    let food = all(&food_word).first().copied();
    let new = Base { tools, repair: all_repair.first().copied(), food, at: pos(spot), when: now_ms(), all_tools, all_repair };
    let changed = p
        .bases
        .lock()
        .get(name)
        .is_none_or(|b| b.tools_chests() != new.tools_chests() || b.repair_chests() != new.repair_chests() || b.food != new.food);
    let list = |cs: &[Pos]| if cs.is_empty() { "none".to_string() } else { cs.iter().map(|c| at(Some(*c))).collect::<Vec<_>>().join(" / ") };
    let text = format!("{name}: base - tools {}, repair {}, food {}", list(&new.all_tools), list(&new.all_repair), at(new.food));
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
/// The kit's kinds: one usable tool of each is carried, no more.
const KINDS: [&str; 3] = ["_pickaxe", "_shovel", "_axe"];

/// The tools a bot does not need, as bag slots: (worn - into Repair; usable - back into Tools).
/// It keeps the best usable pickaxe, shovel and axe, and nothing else that digs (5 Nov: tools were
/// going missing from the chests - one of each, the user said).
pub fn spares(bags: &[(usize, ItemStack)], retire: i32) -> (Vec<usize>, Vec<usize>) {
    let keep: Vec<usize> = KINDS
        .iter()
        .filter_map(|k| bags.iter().filter(|(_, s)| inv::stack_id(s).ends_with(k) && !worn(s, retire)).max_by_key(|(_, s)| errand::tool_rank(s)).map(|(i, _)| *i))
        .collect();
    let (mut worn_out, mut extra) = (Vec::new(), Vec::new());
    for (i, s) in bags.iter().filter(|(i, s)| errand::is_digging_tool(s) && !keep.contains(i)) {
        if worn(s, retire) { worn_out.push(*i) } else { extra.push(*i) }
    }
    (worn_out, extra)
}

/// Why a tools trip is due for the rest of the kit: a shovel or an axe short, or tools carried
/// that are not needed.
pub fn kit_trip(bot: &Client, retire: i32) -> Option<String> {
    if let Some(kind) = kit_short(bot, retire) {
        return Some(format!("no usable {kind}"));
    }
    let (w, e) = spares(&inv::bags(bot), retire);
    (w.len() + e.len() > 0).then(|| format!("{} tool(s) it does not need", w.len() + e.len()))
}

fn named(bags: &[(usize, ItemStack)], slots: &[usize]) -> String {
    let name = |s: &ItemStack| format!("{} ({} uses left)", inv::stack_id(s), inv::uses_left(s).unwrap_or(0));
    bags.iter().filter(|(i, _)| slots.contains(i)).map(|(_, s)| name(s)).collect::<Vec<_>>().join(", ")
}
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
    let (tools, repair): (Vec<BlockPos>, Vec<BlockPos>) = match p.base_for(&link.name, here) {
        Some(b) => (b.tools_chests().into_iter().map(bp).collect(), b.repair_chests().into_iter().map(bp).collect()),
        None => (
            errand::signed_chest(bot, ctx, &job.tools_sign, f64::MAX).into_iter().collect(),
            errand::signed_chest(bot, ctx, &job.repair_sign, f64::MAX).into_iter().collect(),
        ),
    };
    if tools.is_empty() {
        ctx.warn_once(&format!(
            "{} has no usable pickaxe and no base recorded - log it in by the chest signed \"{}\" (or press Record base there)",
            link.name, job.tools_sign
        ));
        return;
    }
    link.start_errand("fetching tools", fetch_tools(bot.clone(), tools, repair, job));
}

/// Low on food: fetch some - room made first, since what is dug fills the bags (4 Nov: both bots
/// flew to the food chest every 2 minutes with full bags and came back with nothing).
pub fn food_errand(bot: &Client, ctx: &Arc<SwarmCtx>, p: &Perimeter, link: &Arc<BotLink>) {
    let Some((source, rule)) = crate::swarm::tick::food_due(bot, ctx, link) else { return };
    let job = p.job.read().clone();
    let (bot, ctx) = (bot.clone(), ctx.clone());
    link.start_errand("fetching food", async move {
        make_room(&bot, &job, 2).await;
        errand::fetch_food(bot, ctx, source, rule, 1).await
    });
}

/// The kinds the bags have no usable one of, of `kinds`.
fn missing(bot: &Client, kinds: &[&'static str], retire: i32) -> Vec<&'static str> {
    kinds.iter().copied().filter(|k| !has_usable(bot, k, retire)).collect()
}

/// The best usable one of each of `kinds` among a container's slots, by slot.
pub fn best_of_each(slots: &[ItemStack], kinds: &[&str], retire: i32) -> Vec<usize> {
    kinds
        .iter()
        .filter_map(|k| {
            slots.iter().enumerate().filter(|(_, s)| inv::stack_id(s).ends_with(k) && !worn(s, retire)).max_by_key(|(_, s)| errand::tool_rank(s)).map(|(i, _)| i)
        })
        .collect()
}

/// What `pick` picks out of the bags, into the first of `chests`, then the next while some is left
/// (that one full, or out of reach). Returns the stacks stored; a chest that could not be used is
/// noted in `trouble`.
async fn store_in_turn(bot: &Client, chests: &[BlockPos], pick: impl Fn(&[(usize, ItemStack)]) -> Vec<usize>, trouble: &mut Vec<String>) -> usize {
    let mut n = 0;
    for c in chests {
        if pick(&inv::bags(bot)).is_empty() {
            break;
        }
        match chests::store_picked(bot, *c, &pick).await {
            Ok(m) => n += m,
            Err(e) => trouble.push(format!("{} {} {}: {e}", c.x, c.y, c.z)),
        }
    }
    n
}

/// At the base: worn tools into Repair (into Tools when there is no repair chest) and spare usable
/// ones back into Tools - one of each kind is kept - then the best pickaxe, shovel and axe the bags
/// lack out of Tools. Room is made first. With more than one chest of either (6 Nov: the user can
/// add more), the next is tried for what the first did not have or had no room for.
async fn fetch_tools(bot: Client, tools: Vec<BlockPos>, repair: Vec<BlockPos>, job: JobConfig) -> Result<String, String> {
    movement::stop(&bot);
    make_room(&bot, &job, 4).await;
    let retire = job.retire_uses;
    let bags = inv::bags(&bot);
    let (w, e) = spares(&bags, retire);
    if !w.is_empty() || !e.is_empty() {
        info!(target: "perimeter", "putting back: worn {}; spare {}", if w.is_empty() { "none".into() } else { named(&bags, &w) }, if e.is_empty() { "none".into() } else { named(&bags, &e) });
    }
    let mut trouble = Vec::new();
    let worn_into = if repair.is_empty() { &tools } else { &repair };
    let stored = store_in_turn(&bot, worn_into, |b| spares(b, retire).0, &mut trouble).await;
    let returned = store_in_turn(&bot, &tools, |b| spares(b, retire).1, &mut trouble).await;
    let mut got = 0;
    for c in &tools {
        let want = missing(&bot, &KINDS, retire);
        // No room: the next chest would not help (6 Nov: "no usable shovel in the tools chest"
        // with 12 in it, the bags full).
        if want.is_empty() || inv::empty_slots(&bot) == 0 {
            break;
        }
        match chests::take_picked(&bot, *c, |slots| best_of_each(slots, &want, retire)).await {
            Ok(n) => got += n,
            Err(e) => trouble.push(format!("{} {} {}: {e}", c.x, c.y, c.z)),
        }
    }
    let full = inv::empty_slots(&bot) == 0;
    let into = if repair.is_empty() { "Tools (no repair chest)" } else { "Repair" };
    let chests_text = if tools.len() == 1 { "the tools chest".to_string() } else { format!("any of the {} tools chests", tools.len()) };
    let trouble = if trouble.is_empty() { String::new() } else { format!(" - could not use the chest at {}", trouble.join("; ")) };
    if !has_pickaxe(&bot, retire) {
        let why = if full { "no room in the bags for a pickaxe".to_string() } else { format!("no usable pickaxe in {chests_text}") };
        return Err(format!(
            "{why} at {} {} {} - waiting for repaired ones ({stored} worn tool(s) put in {into}){trouble}",
            tools[0].x, tools[0].y, tools[0].z
        ));
    }
    let short = missing(&bot, &KIT, retire);
    let short = if short.is_empty() {
        String::new()
    } else {
        let names = short.iter().map(|k| &k[1..]).collect::<Vec<_>>().join(" or ");
        let why = if full { format!("no room in the bags for a {names}") } else { format!("no usable {names} in {chests_text}") };
        format!(" - {why} (looked for again every {} min)", KIT_RECHECK.as_secs() / 60)
    };
    let (w, e) = spares(&inv::bags(&bot), retire);
    let left = if w.len() + e.len() > 0 { format!(" - {} tool(s) it does not need did not go in (chests full?)", w.len() + e.len()) } else { String::new() };
    Ok(format!("took {got} tool(s); {stored} worn one(s) into {into}, {returned} spare(s) back into Tools{short}{left}{trouble}"))
}

// ------------------------------------------------------------------ bed

/// A bed this near where it joins is its bed.
const BED_NEAR: i32 = 12;
/// After joining: the chunks are in by this...
const BED_FROM: Duration = Duration::from_millis(2500);
/// ...and a bed not seen by this is not there.
const BED_UNTIL: Duration = Duration::from_secs(10);

/// The nearest bed within BED_NEAR blocks.
fn bed_near(bot: &Client) -> Option<BlockPos> {
    let here = bot.position().ok()?;
    let world = bot.world().ok()?;
    let w = world.read();
    let c = BlockPos::new(here.x.floor() as i32, here.y.floor() as i32, here.z.floor() as i32);
    let mut best: Option<(i32, BlockPos)> = None;
    for dx in -BED_NEAR..=BED_NEAR {
        for dy in -4..=4 {
            for dz in -BED_NEAR..=BED_NEAR {
                let p = BlockPos::new(c.x + dx, c.y + dy, c.z + dz);
                if w.get_block_state(p).is_some_and(|s| BlockKind::from(s).to_str().ends_with("_bed")) {
                    let d = dx * dx + dy * dy + dz * dz;
                    if best.is_none_or(|(b, _)| d < b) {
                        best = Some((d, p));
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Once a join, by a bed: right-click it so the respawn is there (the user, 5 Nov - it should be
/// set already, "but have it right click the bed to make sure"). Overworld only: a bed blows up
/// anywhere else.
pub fn bed_check(bot: &Client, link: &Arc<BotLink>) {
    let Some(joined) = *link.joined_at.lock() else { return };
    if joined.elapsed() < BED_FROM || link.errand_running() || link.perimeter.lock().bed_checked_at.is_some_and(|t| t > joined) {
        return;
    }
    if crate::swarm::tick::dimension(bot) != "minecraft:overworld" {
        link.perimeter.lock().bed_checked_at = Some(Instant::now());
        return;
    }
    match bed_near(bot) {
        Some(bed) => {
            link.perimeter.lock().bed_checked_at = Some(Instant::now());
            link.start_errand("setting its respawn at the bed", set_respawn(bot.clone(), bed));
        }
        None if joined.elapsed() >= BED_UNTIL => link.perimeter.lock().bed_checked_at = Some(Instant::now()),
        None => {}
    }
}

/// Its bed not looked for yet since it joined: the dig waits (it would be off to a cell first).
pub fn bed_pending(link: &BotLink) -> bool {
    let Some(joined) = *link.joined_at.lock() else { return false };
    link.perimeter.lock().bed_checked_at.is_none_or(|t| t <= joined)
}

/// Beside the bed, right-click it - the server sets the respawn before it looks at the time - and
/// out of it again at once if that put the bot to sleep (at night).
async fn set_respawn(bot: Client, bed: BlockPos) -> Result<String, String> {
    movement::stop(&bot);
    chests::fly_within_reach(&bot, bed).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hit = Vec3::new(bed.x as f64 + 0.5, bed.y as f64 + 0.5625, bed.z as f64 + 0.5);
    crate::print::act::use_main(&bot, bed, crate::print::place::Dir::Up, hit);
    tokio::time::sleep(Duration::from_millis(800)).await;
    if let Ok(id) = bot.query_self::<&MinecraftEntityId, _>(|id| *id) {
        bot.write_packet(ServerboundPlayerCommand { id, action: Action::StopSleeping, data: 0 });
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(format!("right-clicked the bed at {} {} {} - its respawn is there", bed.x, bed.y, bed.z))
}

// ------------------------------------------------------------------ fill

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// Fill to place: in the offhand (None), or in this hotbar slot while the totem has the offhand.
    Ready(Option<u8>),
    /// Fill is being swapped into the offhand or the hotbar (there next tick).
    Swapping,
    /// None in the bags.
    Out,
}

/// Fill in a hand to place from: the offhand - the best in the bags is swapped in when it holds
/// none - but never over the totem (the user, 5 Nov: it is in the offhand by default, "dont swap
/// it"): then from the hotbar, a slot with no tool or weapon in it, never the held one.
pub fn fill_ready(bot: &Client, job: &JobConfig) -> Fill {
    let Some(menu) = inv::menu(bot) else { return Fill::Out };
    let slots = menu.slots();
    let offhand = slots.get(45).cloned().unwrap_or_default();
    if !offhand.is_empty() && job.is_fill(inv::stack_id(&offhand)) {
        return Fill::Ready(None);
    }
    let rank = |s: &ItemStack| (job.fill.iter().position(|f| f == inv::stack_id(s)).unwrap_or(usize::MAX), -s.count());
    let fill: Vec<(usize, ItemStack)> = inv::bags(bot).into_iter().filter(|(_, s)| !s.is_empty() && job.is_fill(inv::stack_id(s))).collect();
    let best = fill.iter().min_by_key(|(_, s)| rank(s)).map(|(i, _)| *i);
    if inv::stack_id(&offhand) != super::guard::TOTEM {
        return match best {
            Some(slot) if inv::swap_with(bot, slot, inv::OFFHAND_BUTTON) => Fill::Swapping,
            _ => Fill::Out,
        };
    }
    let held = bot.selected_hotbar_slot().ok();
    if let Some(h) = fill.iter().filter_map(|(i, s)| inv::hotbar_index(*i).filter(|h| Some(*h) != held).map(|h| (rank(s), h))).min().map(|(_, h)| h) {
        return Fill::Ready(Some(h));
    }
    let Some(slot) = best else { return Fill::Out };
    // Into an empty hotbar slot, else one holding neither a tool nor food.
    let pick = (0..9u8)
        .filter(|h| Some(*h) != held)
        .filter_map(|h| {
            let st = &slots[36 + h as usize];
            let r = if st.is_empty() { 0 } else if inv::is_tool(st) || st.get_component::<Food>().is_some() { return None } else { 1 };
            Some((r, h))
        })
        .min();
    match pick {
        Some((_, h)) if inv::swap_with(bot, slot, h) => Fill::Swapping,
        _ => Fill::Out,
    }
}

/// Dropped fill within `r` blocks, the nearest first. Read with no world lock held (the entities
/// take the ECS): which of it there is a dry way to is the caller's to work out after.
pub fn fill_lying_near(bot: &Client, job: &JobConfig, r: f64) -> Vec<Vec3> {
    crate::entities::near(bot).into_iter().filter(|(d, e)| *d <= r && e.item.is_some_and(|k| job.is_fill(inv::id(k)))).map(|(_, e)| e.pos).collect()
}

// ------------------------------------------------------------------ room

/// Kept whatever happens: tools, weapons and armour, food, and what a bot was given to carry.
fn kept(st: &ItemStack) -> bool {
    let id = inv::stack_id(st);
    st.get_component::<MaxDamage>().is_some() || st.get_component::<Food>().is_some() || id == "totem_of_undying" || id == "ender_chest" || id.ends_with("shulker_box")
}

/// Fill carried in the bags.
pub fn fill_carried(bot: &Client, job: &JobConfig) -> usize {
    inv::bags(bot).iter().filter(|(_, s)| !s.is_empty() && job.is_fill(inv::stack_id(s))).map(|(_, s)| s.count().max(0) as usize).sum()
}

/// A stack dug that is not fill (and not kept).
pub fn dug_junk_slot(bot: &Client, job: &JobConfig) -> Option<usize> {
    inv::bags(bot).into_iter().find(|(_, s)| !s.is_empty() && !kept(s) && !job.is_fill(inv::stack_id(s))).map(|(slot, _)| slot)
}

/// A stack to throw out: anything dug that is not fill first, then the smallest fill stacks past
/// the ones kept.
pub fn junk_slot(bot: &Client, job: &JobConfig) -> Option<usize> {
    if let Some(slot) = dug_junk_slot(bot, job) {
        return Some(slot);
    }
    let bags = inv::bags(bot);
    let mut fill: Vec<(usize, i32)> = bags.iter().filter(|(_, s)| !s.is_empty() && job.is_fill(inv::stack_id(s))).map(|(i, s)| (*i, s.count())).collect();
    fill.sort_by_key(|(_, n)| -n);
    fill.get(job.keep_fill_stacks..).and_then(|rest| rest.iter().min_by_key(|(_, n)| *n)).map(|(i, _)| *i)
}

pub use crate::inv::throw;

/// Up this far off the dig before throwing: out of reach of what lies on the ground.
const ROOM_LIFT: f64 = 10.0;

/// Throw junk out until `free` bag slots are empty, or there is no more junk - up off the ground
/// first (6 Nov: with full bags what the dig digs lies at its feet, and every slot thrown empty
/// there filled again at once from the ground - 7 throws for 4 slots, and full bags at the tools
/// chest beside 12 usable shovels, "no usable shovel" four trips running). Returns the stacks thrown.
pub async fn make_room(bot: &Client, job: &JobConfig, free: usize) -> usize {
    if inv::empty_slots(bot) >= free || junk_slot(bot, job).is_none() {
        return 0;
    }
    if let Ok(here) = bot.position()
        && let Err(e) = movement::fly_to(bot, here + Vec3::new(0.0, ROOM_LIFT, 0.0), "room").await
    {
        warn!(target: "perimeter", "could not get up off the ground to throw junk out: {e}");
    }
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

#[cfg(test)]
mod tests {
    use azalea::{inventory::components::Damage, registry::builtin::ItemKind};

    use super::*;

    fn tool(kind: ItemKind, damage: i32) -> ItemStack {
        ItemStack::new(kind, 1).with_component(Damage { amount: damage })
    }

    #[test]
    fn one_of_each_tool_is_kept_the_rest_go_back() {
        // A netherite pickaxe has 2031 uses: damage 2010 leaves 21 (worn at 30 or fewer).
        let bags = vec![
            (9, tool(ItemKind::NetheritePickaxe, 1000)),
            (10, tool(ItemKind::NetheritePickaxe, 200)),
            (11, tool(ItemKind::NetheritePickaxe, 2010)),
            (12, tool(ItemKind::NetheriteShovel, 2010)),
            (13, ItemStack::new(ItemKind::Dirt, 64)),
            (36, tool(ItemKind::NetheriteAxe, 0)),
            (37, tool(ItemKind::IronShovel, 0)),
            (38, tool(ItemKind::IronHoe, 0)),
        ];
        let (worn_out, extra) = spares(&bags, 30);
        assert_eq!(worn_out, vec![11, 12], "the worn pickaxe and shovel go into Repair");
        // Kept: the pickaxe with the most uses (10), the iron shovel (the only usable one), the axe.
        assert_eq!(extra, vec![9, 38], "the other usable pickaxe and the hoe go back into Tools");
    }

    #[test]
    fn a_bot_with_its_kit_and_nothing_else_has_no_spares() {
        let bags = vec![(36, tool(ItemKind::NetheritePickaxe, 5)), (37, tool(ItemKind::NetheriteShovel, 5)), (38, tool(ItemKind::NetheriteAxe, 5))];
        assert_eq!(spares(&bags, 30), (vec![], vec![]));
    }

    #[test]
    fn from_a_tools_chest_the_best_usable_one_of_each_kind_wanted() {
        let slots = vec![
            tool(ItemKind::NetheriteShovel, 2010),
            ItemStack::Empty,
            tool(ItemKind::IronShovel, 0),
            tool(ItemKind::NetheriteShovel, 100),
            tool(ItemKind::NetheritePickaxe, 0),
            tool(ItemKind::NetheriteAxe, 0),
        ];
        assert_eq!(best_of_each(&slots, &["_shovel"], 30), vec![3], "not the worn one, and netherite over iron");
        assert_eq!(best_of_each(&slots, &["_pickaxe", "_shovel"], 30), vec![4, 3]);
        assert_eq!(best_of_each(&slots[..3], &["_pickaxe", "_shovel"], 30), vec![2], "no pickaxe in this chest: the next one is tried for it");
    }
}
