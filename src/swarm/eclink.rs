//! The ender chest link: swarm bots take their tools, weapons, food and gear out of the ender
//! chest, swapped in silently - always, unless the bot is printing a schematic.
//!
//! - **The link** (core/EnderLink.java, checked there against the bytecode): two clicks on an ender
//!   chest in one go. The second open closes the first, and that close clears the chest's block
//!   entity - with it gone the server never checks the distance again, so the container stays open
//!   at any range until something closes it: another container, death, a relog.
//! - **Always**: a bot links as soon as it can, and again whenever the link is lost (a chest trip,
//!   a death, a relog). Only a print job closes it - the printer never uses it.
//! - **Finding a chest**: an ender chest it carries is placed first, before
//!   anything (it seldom carries one); else the nearest in the loaded world (every swarm
//!   bot's render distance) or remembered (swarm/places.json, the panel's) within the search range;
//!   else a spiral search out to that range (2,000 blocks), one bot at a time. Nothing found: the
//!   search stops and the panel says so - the bot's jobs carry on with what it carries. Then it
//!   flies back to where it was working.
//! - **Borrowing into the hand** is one SWAP click - an ender chest slot with the HELD hotbar slot,
//!   so the hand never changes slot (silent) - and the same click puts it back: tools for the
//!   nuker, weapons in a fight. A hand loan goes back when it has not been used for 5 s, before any
//!   chest trip, when a print job starts, and at once at low health (a death drops it).
//! - **Worn**: a totem into an empty offhand and armor into empty armor slots (a right click puts
//!   it on), kept on while it works - they keep it alive, so low health does not send them back. A
//!   print job puts them back: the totem by a swap, armor by a trip to the chest (worn armor cannot
//!   be clicked while a container is open).
//! - **Food**: a stack into the bags when it runs low (eaten, so not a loan).
//!
//! While linked the container stays open, so the bags are read from its player part and every bag
//! swap is clicked in it (inv.rs).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    app::{App, Plugin, Update},
    block::BlockState,
    core::{entity_id::MinecraftEntityId, position::ChunkPos},
    ecs::prelude::*,
    entity::{LocalEntity, inventory::Inventory},
    inventory::{CloseContainerEvent, ItemStack, Menu, components::Food},
    local_player::WorldHolder,
    packet::game::ReceiveGamePacketEvent,
    physics::collision::BlockWithShape,
    protocol::packets::game::ClientboundGamePacket,
    registry::builtin::BlockKind,
};
use azalea::ecs as bevy_ecs; // for #[derive(Component)]
use tracing::{debug, info, warn};

use super::{BotLink, SwarmCtx};
use crate::{
    chests,
    config::Pos,
    inv, movement,
    print::{act, place::Dir},
};

/// A hand loan not used for this long goes back in the chest.
const IDLE_RETURN_TICKS: u64 = 100;
/// After a failed link, the next try waits this long.
const RETRY_TICKS: u64 = 20 * 30;
/// The places.json word the swarm's ender chests are kept under.
pub const PLACE_WORD: &str = "ender chest";
/// Borrowing waits for this much health over the panic level: a bot sent back to the chest at 5
/// borrowed again at 7.8 (17 Oct test) - one more hit from dying with it.
const BORROW_MARGIN: f32 = 4.0;
/// The player menu's armor slots, and what goes in each.
const ARMOR: [(u8, &str); 4] = [(5, "_helmet"), (6, "_chestplate"), (7, "_leggings"), (8, "_boots")];
const TOTEM: &str = "totem_of_undying";
/// The entity event a totem saving its holder sends.
const TOTEM_POP_EVENT: u8 = 35;

pub struct EcLinkPlugin;

impl Plugin for EcLinkPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, record_totem_pops);
    }
}

/// Totems that saved this bot. The server never tells a client with a container open that its
/// offhand emptied, so the pop itself is counted.
#[derive(Component, Default)]
pub struct TotemPops(pub u32);

fn record_totem_pops(mut events: MessageReader<ReceiveGamePacketEvent>, mut us: Query<(&MinecraftEntityId, Option<&mut TotemPops>), With<LocalEntity>>, mut commands: Commands) {
    for ev in events.read() {
        let ClientboundGamePacket::EntityEvent(p) = ev.packet.as_ref() else { continue };
        if p.event_id != TOTEM_POP_EVENT {
            continue;
        }
        let Ok((id, pops)) = us.get_mut(ev.entity) else { continue };
        if p.entity_id != *id {
            continue;
        }
        match pops {
            Some(mut n) => n.0 += 1,
            None => {
                commands.entity(ev.entity).insert(TotemPops(1));
            }
        }
    }
}

/// Where a loan went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Worn {
    /// Into the held hotbar slot (a tool, a weapon).
    Hand,
    /// Into the offhand (a totem).
    Offhand,
    /// Put on: the player menu's armor slot (5 head - 8 feet).
    Armor(u8),
}

/// A borrowed stack: where it went, where it came from, what it is, when it was last used.
#[derive(Clone, Debug)]
pub struct Loan {
    pub how: Worn,
    /// The hotbar slot of a hand loan.
    pub hotbar: u8,
    pub ec_slot: usize,
    pub item: &'static str,
    pub used: u64,
}

/// One bot's link.
#[derive(Default)]
pub struct EcLink {
    /// The open ender chest's container id while linked.
    pub id: Option<i32>,
    pub chest: Option<BlockPos>,
    pub loans: Vec<Loan>,
    pub status: String,
    pub retry_at: u64,
    pub borrows: u64,
    /// The bot's death count when last ticked (a death ends the link).
    pub deaths_seen: Option<u32>,
    /// Totem pops when last ticked.
    pub pops_seen: u32,
}

impl EcLink {
    pub fn linked(&self) -> bool {
        self.id.is_some()
    }

    /// Hotbar slots holding a loan (nothing else may be swapped into them).
    pub fn loan_hotbars(&self) -> Vec<u8> {
        self.loans.iter().filter(|l| l.how == Worn::Hand).map(|l| l.hotbar).collect()
    }

    fn worn(&self) -> Vec<&'static str> {
        self.loans.iter().filter(|l| l.how != Worn::Hand).map(|l| l.item).collect()
    }

    fn held(&self) -> Vec<&'static str> {
        self.loans.iter().filter(|l| l.how == Worn::Hand).map(|l| l.item).collect()
    }
}

/// The swarm's search for an ender chest: one bot at a time.
#[derive(Default)]
pub struct Search {
    /// The bot searching now.
    pub by: Option<String>,
    pub progress: String,
    /// The last search that found nothing: where from, how far. No new search near there until a
    /// chest turns up, the panel says search again, or the range changes.
    pub none_found: Option<(Vec3, f64)>,
}

/// Clears the searcher when the search ends - also when its errand is stopped mid-way.
struct Searching(Arc<SwarmCtx>);

impl Drop for Searching {
    fn drop(&mut self) {
        let mut s = self.0.ec_search.lock();
        s.by = None;
        s.progress.clear();
    }
}

/// The open ender chest: its id, its 27 slots, and where its player part starts.
fn open_chest(bot: &Client) -> Option<(i32, Vec<ItemStack>, usize)> {
    bot.query_self::<&Inventory, _>(|inv| {
        let c = inv.container_menu.as_ref()?;
        if !matches!(c, Menu::Generic9x3 { .. }) {
            return None;
        }
        let slots = c.slots();
        Some((inv.id, slots[..27].to_vec(), *c.player_slots_range().start()))
    })
    .ok()
    .flatten()
}

/// The ender chest's contents while linked: (slot 0-26, stack), filled ones only. Empty when not
/// linked.
pub fn contents(bot: &Client, l: &BotLink) -> Vec<(usize, ItemStack)> {
    let id = l.eclink.lock().id;
    match (id, open_chest(bot)) {
        (Some(id), Some((open, slots, _))) if open == id => slots.into_iter().enumerate().filter(|(_, s)| !s.is_empty()).collect(),
        _ => Vec::new(),
    }
}

/// Hotbar slots holding a loan.
pub fn loan_hotbars(l: &BotLink) -> Vec<u8> {
    l.eclink.lock().loan_hotbars()
}

/// Every ender chest the swarm remembers: what its bots saw in their chunks, places.json, the
/// panel's.
fn known(bot: &Client, ctx: &SwarmCtx) -> Vec<BlockPos> {
    let mut out: Vec<BlockPos> = {
        let mut ecs = bot.ecs.write();
        let mut q = ecs.query::<&chests::EnderChests>();
        q.iter(&ecs).flat_map(|e| e.0.iter().copied()).collect()
    };
    out.extend(ctx.places.lock().signed.get(PLACE_WORD).into_iter().flatten().map(|p| BlockPos::new(p.x, p.y, p.z)));
    if let Some(p) = ctx.cfg.read().ender_chest {
        out.push(BlockPos::new(p.x, p.y, p.z));
    }
    out
}

/// Ender chests in the loaded world - every swarm bot's chunks (they share one world), nearest
/// `near` first. A chest placed after its chunk arrived sends no block entity packet, so this
/// reads the blocks themselves.
fn loaded(bot: &Client, near: Vec3) -> Vec<BlockPos> {
    let Ok(world) = bot.world() else { return Vec::new() };
    let w = world.read();
    let states = BlockKind::EnderChest.into();
    w.find_blocks(BlockPos::from(near), &states).take(32).collect()
}

/// The ender chests to try from `home`: loaded or remembered, within `range`, not marked bad,
/// nearest first.
fn candidates(bot: &Client, ctx: &SwarmCtx, home: Vec3, range: f64) -> Vec<BlockPos> {
    let mut out = loaded(bot, home);
    out.extend(known(bot, ctx));
    out.sort_by_key(|p| (p.x, p.y, p.z));
    out.dedup();
    let now = Instant::now();
    let mut bad = ctx.bad_ender_chests.lock();
    bad.retain(|(_, until)| now < *until);
    out.retain(|p| !bad.iter().any(|(b, _)| b == p) && centre(*p).distance_to(home) <= range);
    out.sort_by(|a, b| centre(*a).distance_to(home).total_cmp(&centre(*b).distance_to(home)));
    out
}

/// Keep (or forget) an ender chest in swarm/places.json.
fn remember(ctx: &SwarmCtx, at: BlockPos, keep: bool) {
    let mut places = ctx.places.lock();
    let list = places.signed.entry(PLACE_WORD.to_string()).or_default();
    let p = Pos { x: at.x, y: at.y, z: at.z };
    let changed = if keep {
        !list.contains(&p) && {
            list.push(p);
            true
        }
    } else {
        let n = list.len();
        list.retain(|q| *q != p);
        n != list.len()
    };
    if changed {
        let text = serde_json::to_string_pretty(&*places).unwrap_or_default();
        drop(places);
        let _ = std::fs::write(ctx.dir.join("places.json"), text + "\n");
    }
}

fn centre(p: BlockPos) -> Vec3 {
    Vec3::new(p.x as f64 + 0.5, p.y as f64 + 0.5, p.z as f64 + 0.5)
}

fn horiz(a: Vec3, b: Vec3) -> f64 {
    ((a.x - b.x).powi(2) + (a.z - b.z).powi(2)).sqrt()
}

fn fmt(p: BlockPos) -> String {
    format!("{} {} {}", p.x, p.y, p.z)
}

/// The ender chest at `at`, else the nearest within 4 blocks of it (the panel's spot is where the
/// controller player stood). None while that is not loaded.
fn ender_chest_near(bot: &Client, at: BlockPos) -> Option<BlockPos> {
    let world = bot.world().ok()?;
    let w = world.read();
    let mut best: Option<(i32, BlockPos)> = None;
    for dx in -4..=4 {
        for dy in -3..=3 {
            for dz in -4..=4 {
                let p = BlockPos::new(at.x + dx, at.y + dy, at.z + dz);
                if w.get_block_state(p).map(BlockKind::from) == Some(BlockKind::EnderChest) {
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

/// The bag slot of an ender chest it carries.
fn carried_ender_chest(bot: &Client) -> Option<usize> {
    inv::bags(bot).into_iter().find(|(_, s)| inv::stack_id(s) == "ender_chest").map(|(slot, _)| slot)
}

/// How far round itself this bot has chunks, counted out from its own chunk along the four axes.
/// The storage's size says nothing: azalea sizes it from the client's view distance plus 3
/// (17 Oct test: "11" on a view-distance-8 server - spiral legs that far apart leave gaps).
fn loaded_radius(bot: &Client) -> u32 {
    bot.query_self::<&WorldHolder, _>(|h| {
        let p = h.partial.read();
        let c = p.chunks.view_center();
        let max = (p.chunks.view_range().saturating_sub(1) / 2) as i32;
        let mut r = 0;
        for d in 1..=max {
            let all = [(d, 0), (-d, 0), (0, d), (0, -d)].iter().all(|(dx, dz)| p.chunks.limited_get(&ChunkPos::new(c.x + dx, c.z + dz)).is_some());
            if !all {
                break;
            }
            r = d;
        }
        r as u32
    })
    .unwrap_or(0)
    .clamp(2, 32)
}

// ------------------------------------------------------------------ the link errand

/// The link errand: find an ender chest (placing a carried one first, then the loaded world, then a
/// spiral search), the doubled click, gear on - and back to where it was working.
pub async fn link(bot: Client, ctx: Arc<SwarmCtx>, l: Arc<BotLink>) -> Result<String, String> {
    let home = bot.position().map_err(|e| e.to_string())?;
    let res = find_and_link(&bot, &ctx, &l, home).await;
    // Back to where it was working (the nuker's cell, its place in a formation).
    if bot.position().is_ok_and(|p| p.distance_to(home) > 6.0) {
        if let Err(e) = movement::fly_to(&bot, home, "back to work").await {
            warn!(target: "eclink", "could not fly back to where it was working: {e}");
        }
    }
    res
}

async fn find_and_link(bot: &Client, ctx: &Arc<SwarmCtx>, l: &Arc<BotLink>, home: Vec3) -> Result<String, String> {
    let range = ctx.cfg.read().ec_search_range;
    movement::stop(bot);
    // One it carries: placed first, before anything.
    let mut target = None;
    if carried_ender_chest(bot).is_some() {
        match place_carried(bot, ctx, l, home).await {
            Ok(p) => target = Some(p),
            Err(e) => warn!(target: "eclink", "could not place the ender chest it carries: {e} - looking for one instead"),
        }
    }
    // The loaded world and the remembered ones, then the search.
    if target.is_none() {
        target = candidates(bot, ctx, home, range).into_iter().next();
    }
    let target = match target {
        Some(t) => t,
        None => search(bot, ctx, l, home, range).await?,
    };
    let chest = open_link(bot, ctx, l, target).await?;
    // There is one now: an earlier search that found nothing no longer holds anyone back.
    ctx.ec_search.lock().none_found = None;
    let stacks = contents(bot, l).len();
    let worn = wear(bot, l).await;
    let worn = if worn.is_empty() { String::new() } else { format!(", wearing {}", worn.join(", ")) };
    Ok(format!("linked the ender chest at {} ({stacks} stacks in it){worn}", fmt(chest)))
}

/// Fly to the chest, click it twice in one go, and keep the container that stays open.
async fn open_link(bot: &Client, ctx: &SwarmCtx, l: &BotLink, target: BlockPos) -> Result<BlockPos, String> {
    chests::fly_within_reach(bot, target).await?;
    let Some(chest) = ender_chest_near(bot, target) else {
        remember(ctx, target, false);
        ctx.bad_ender_chests.lock().push((target, Instant::now() + Duration::from_secs(600)));
        return Err(format!("no ender chest at {} any more", fmt(target)));
    };
    if chest != target {
        chests::fly_within_reach(bot, chest).await?;
    }
    remember(ctx, chest, true);
    // Two clicks one after the other on the connection: the second open leaves the container with
    // no block entity behind it.
    let hit = Vec3::new(chest.x as f64 + 0.5, chest.y as f64 + 0.875, chest.z as f64 + 0.5);
    act::use_main(bot, chest, Dir::Up, hit);
    act::use_main(bot, chest, Dir::Up, hit);
    debug!(target: "eclink", "interact doubled at {chest}");
    // The first open, its close, the second open: settle on the last.
    let mut seen = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = open_chest(bot).map(|(id, ..)| id);
        if now.is_some() && now == seen {
            break;
        }
        seen = now;
    }
    let Some(id) = seen else {
        ctx.bad_ender_chests.lock().push((chest, Instant::now() + Duration::from_secs(120)));
        return Err(format!("the ender chest at {} did not open (a block on top of it?)", fmt(chest)));
    };
    {
        let mut e = l.eclink.lock();
        e.id = Some(id);
        e.chest = Some(chest);
    }
    info!(target: "eclink", "linked the ender chest at {chest} (container {id}, {} stacks in it)", contents(bot, l).len());
    Ok(chest)
}

/// Place the ender chest it carries near where it works - on a solid block, open above (a chest
/// with a block on it does not open), never inside the nuker's box - and remember it for the
/// swarm.
async fn place_carried(bot: &Client, ctx: &SwarmCtx, l: &BotLink, home: Vec3) -> Result<BlockPos, String> {
    let avoid = l.nuker_cfg().and_then(|c| c.area);
    let spot = {
        let world = bot.world().map_err(|e| e.to_string())?;
        let w = world.read();
        let air = |p: BlockPos| w.get_block_state(p).is_some_and(|s| s.is_air());
        let solid = |p: BlockPos| w.get_block_state(p).is_some_and(|s: BlockState| !s.is_air() && s.is_collision_shape_full());
        let in_box = |p: BlockPos| {
            avoid.is_some_and(|b| p.x >= b.min.x - 1 && p.x <= b.max.x + 1 && p.y >= b.min.y - 1 && p.y <= b.max.y + 1 && p.z >= b.min.z - 1 && p.z <= b.max.z + 1)
        };
        let at = BlockPos::from(home);
        let mut cells: Vec<BlockPos> = Vec::new();
        for dx in -8..=8 {
            for dz in -8..=8 {
                for dy in -12..=4 {
                    cells.push(BlockPos::new(at.x + dx, at.y + dy, at.z + dz));
                }
            }
        }
        cells.sort_by(|a, b| centre(*a).distance_to(home).total_cmp(&centre(*b).distance_to(home)));
        cells.into_iter().find(|c| {
            air(*c) && air(c.up(1)) && solid(c.down(1)) && !in_box(*c)
                // Not where it stands now.
                && centre(*c).distance_to(home) > 1.5
        })
    }
    .ok_or("no solid spot open above within 8 blocks")?;
    let Some(slot) = carried_ender_chest(bot) else { return Err("it no longer carries one".into()) };
    chests::fly_within_reach(bot, spot).await?;
    inv::hold(bot, slot, &loan_hotbars(l)).ok_or("could not take it in hand")?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    if inv::stack_id(&bot.get_held_item().unwrap_or_default()) != "ender_chest" {
        return Err("it did not reach the hand".into());
    }
    let below = spot.down(1);
    act::use_main(bot, below, Dir::Up, Vec3::new(spot.x as f64 + 0.5, spot.y as f64, spot.z as f64 + 0.5));
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let placed = bot.world().ok().and_then(|w| w.read().get_block_state(spot)).map(BlockKind::from) == Some(BlockKind::EnderChest);
        if placed {
            remember(ctx, spot, true);
            info!(target: "eclink", "placed the ender chest it carried at {spot}");
            ctx.feed("event", &format!("{} placed the ender chest it carried at {}", l.name, fmt(spot)));
            return Ok(spot);
        }
    }
    Err(format!("the ender chest did not go down at {}", fmt(spot)))
}

/// A square spiral at the cruise height out to `range` from `home`, legs one view apart less a
/// chunk, watching the loaded world as it goes. One bot at a time; what it finds is remembered for
/// the swarm.
async fn search(bot: &Client, ctx: &Arc<SwarmCtx>, l: &BotLink, home: Vec3, range: f64) -> Result<BlockPos, String> {
    {
        let mut s = ctx.ec_search.lock();
        if let Some(by) = s.by.as_ref().filter(|b| **b != l.name) {
            return Err(format!("{by} is already searching for one"));
        }
        s.by = Some(l.name.clone());
        s.progress = "setting off".into();
    }
    let _searching = Searching(ctx.clone());
    let r = loaded_radius(bot);
    let step = ((2 * r - 1) * 16) as f64;
    let y = movement::trip::CRUISE_Y;
    info!(target: "eclink", "no ender chest known within {range:.0} blocks - spiral search from {:.0} {:.0} {:.0}, legs {step:.0} apart (chunks loaded {r} out)", home.x, home.y, home.z);
    ctx.feed("event", &format!("{}: searching for an ender chest, out to {range:.0} blocks", l.name));
    // Up first: the spiral flies over everything.
    let found = |bot: &Client| candidates(bot, ctx, home, range).into_iter().next();
    movement::fly_to(bot, Vec3::new(home.x, y, home.z), "ender chest search").await?;
    if let Some(p) = found(bot) {
        return Ok(p);
    }
    let dirs = [(1.0, 0.0), (0.0, 1.0), (-1.0, 0.0), (0.0, -1.0)];
    let (mut x, mut z) = (home.x, home.z);
    let mut d = 0;
    let mut n = 1;
    loop {
        // Legs of n steps, two of each length: the square's edge is ceil(n/2) steps out.
        let out = (n as f64 / 2.0).ceil() * step;
        if out > range + step / 2.0 {
            break;
        }
        for _ in 0..2 {
            let (dx, dz) = dirs[d % 4];
            d += 1;
            let to = Vec3::new(x + dx * step * n as f64, y, z + dz * step * n as f64);
            ctx.ec_search.lock().progress = format!("{:.0} of {range:.0} blocks out", out.min(range));
            if let Some(p) = fly_watching(bot, to, &found).await? {
                info!(target: "eclink", "search found an ender chest at {p}");
                return Ok(p);
            }
            x = to.x;
            z = to.z;
        }
        n += 1;
    }
    ctx.ec_search.lock().none_found = Some((home, range));
    // The errand's failure line carries it to the panel's feed.
    Err(format!("no ender chest within {range:.0} blocks - set one on the panel, or place one in its loaded area"))
}

/// One spiral leg, watching for a chest every half second. Some(chest) as soon as one is seen.
async fn fly_watching(bot: &Client, to: Vec3, found: &impl Fn(&Client) -> Option<BlockPos>) -> Result<Option<BlockPos>, String> {
    let mut tries = 0;
    loop {
        if !movement::busy(bot) {
            let here = bot.position().map_err(|e| e.to_string())?;
            if horiz(here, to) < 4.0 {
                // The last chunks of this corner.
                tokio::time::sleep(Duration::from_millis(1500)).await;
                return Ok(found(bot));
            }
            tries += 1;
            if tries > 3 {
                return Err(format!("could not fly the search leg to {:.0} {:.0}", to.x, to.z));
            }
            movement::go_direct(bot, to, "ender chest search");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        bot.position().map_err(|e| e.to_string())?;
        if let Some(p) = found(bot) {
            movement::stop(bot);
            return Ok(Some(p));
        }
    }
}

// ------------------------------------------------------------------ worn gear

fn armor_rank(id: &str) -> i32 {
    for (i, m) in ["leather", "golden", "chainmail", "turtle", "iron", "diamond", "netherite"].iter().enumerate() {
        if id.starts_with(m) {
            return i as i32 + 1;
        }
    }
    0
}

/// What is in an armor slot (5-8) or the offhand (45) - the player menu's copy, which a container
/// left open keeps stale: the loans say what this bot put on itself since.
fn worn_slot(bot: &Client, slot: usize) -> ItemStack {
    bot.query_self::<&Inventory, _>(|inv| inv.inventory_menu.slots().get(slot).cloned().unwrap_or_default()).unwrap_or_default()
}

fn set_worn_slot(bot: &Client, slot: usize, st: ItemStack) {
    let _ = bot.query_self::<&mut Inventory, _>(|mut inv| {
        if let Some(s) = inv.inventory_menu.slot_mut(slot) {
            *s = st;
        }
    });
}

/// Gear on from the chest just linked: a totem into an empty offhand, the best piece into each
/// empty armor slot.
async fn wear(bot: &Client, l: &BotLink) -> Vec<&'static str> {
    let mut on = Vec::new();
    if totem_in(bot, l, 0) {
        on.push(TOTEM);
    }
    for (slot, kind) in ARMOR {
        let taken = l.eclink.lock().loans.iter().any(|x| x.how == Worn::Armor(slot));
        if taken || !worn_slot(bot, slot as usize).is_empty() {
            continue;
        }
        let best = contents(bot, l)
            .into_iter()
            .filter(|(_, s)| inv::stack_id(s).ends_with(kind) && armor_rank(inv::stack_id(s)) > 0)
            .max_by_key(|(_, s)| (armor_rank(inv::stack_id(s)), inv::uses_left(s).unwrap_or(0)));
        let Some((ec_slot, piece)) = best else { continue };
        let id = inv::stack_id(&piece);
        if put_on(bot, l, ec_slot, slot, piece).await {
            on.push(id);
        }
    }
    on
}

/// One piece on: into the hand by a swap (the hand's stack into its chest slot), a right click
/// puts it on, and the hand's stack swapped back out of the chest.
async fn put_on(bot: &Client, l: &BotLink, ec_slot: usize, slot: u8, piece: ItemStack) -> bool {
    let item = inv::stack_id(&piece);
    let Ok(held) = bot.selected_hotbar_slot() else { return false };
    let Some((_, _, start)) = open_chest(bot) else { return false };
    let target = Some(start + 27 + held as usize);
    let had = !bot.get_held_item().unwrap_or_default().is_empty();
    if !inv::swap_in(bot, ec_slot, held, target) {
        return false;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    act::use_item_main(bot);
    tokio::time::sleep(Duration::from_millis(400)).await;
    // On when it has left the hand (the armor slot is not in the chest's view).
    if inv::stack_id(&bot.get_held_item().unwrap_or_default()) == item {
        warn!(target: "eclink", "{item} did not go on - back in the chest");
        inv::swap_in(bot, ec_slot, held, target);
        tokio::time::sleep(Duration::from_millis(250)).await;
        return false;
    }
    if had {
        inv::swap_in(bot, ec_slot, held, target);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    set_worn_slot(bot, slot as usize, piece);
    let mut e = l.eclink.lock();
    e.loans.push(Loan { how: Worn::Armor(slot), hotbar: u8::MAX, ec_slot, item, used: 0 });
    e.borrows += 1;
    info!(target: "eclink", "wearing {item} from ender chest slot {ec_slot}");
    true
}

/// A totem into an empty offhand, from the chest. Not held back at low health: it is what saves
/// the bot.
fn totem_in(bot: &Client, l: &BotLink, ticks: u64) -> bool {
    let mut e = l.eclink.lock();
    if e.loans.iter().any(|x| x.how == Worn::Offhand) || !worn_slot(bot, 45).is_empty() || !check(bot, &mut e) {
        return false;
    }
    let Some((_, slots, _)) = open_chest(bot) else { return false };
    let Some(ec_slot) = slots.iter().position(|s| inv::stack_id(s) == TOTEM) else { return false };
    if !inv::swap_in(bot, ec_slot, inv::OFFHAND_BUTTON, None) {
        return false;
    }
    e.loans.push(Loan { how: Worn::Offhand, hotbar: u8::MAX, ec_slot, item: TOTEM, used: ticks });
    e.borrows += 1;
    info!(target: "eclink", "totem from ender chest slot {ec_slot} into the offhand");
    true
}

/// The totem back by a swap while linked. True when none is out.
fn totem_back(bot: &Client, e: &mut EcLink) -> bool {
    let Some(i) = e.loans.iter().position(|x| x.how == Worn::Offhand) else { return true };
    if !check(bot, e) {
        return false;
    }
    let Some((_, slots, _)) = open_chest(bot) else { return false };
    let loan = e.loans[i].clone();
    if inv::stack_id(&worn_slot(bot, 45)) != TOTEM {
        // Not in the offhand any more (a printer swap moved it to the bags): the trip takes it.
        return false;
    }
    let to = if slots.get(loan.ec_slot).is_some_and(|s| s.is_empty()) { Some(loan.ec_slot) } else { slots.iter().position(|s| s.is_empty()) };
    let Some(to) = to else {
        warn!(target: "eclink", "the ender chest is full - the totem stays in the offhand");
        return false;
    };
    if inv::swap_in(bot, to, inv::OFFHAND_BUTTON, None) {
        info!(target: "eclink", "returned the totem to ender chest slot {to}");
        e.loans.remove(i);
        true
    } else {
        false
    }
}

/// Everything worn back in the chest (a print job): the totem and armor off into the bags, the
/// link closed, and stored in the chest by an ordinary open - worn armor cannot be clicked while
/// a container is open.
pub async fn put_gear_back(bot: Client, ctx: Arc<SwarmCtx>, l: Arc<BotLink>) -> Result<String, String> {
    let here = bot.position().map_err(|e| e.to_string())?;
    let chest = l.eclink.lock().chest.or_else(|| candidates(&bot, &ctx, here, f64::MAX).into_iter().next()).ok_or("no ender chest known to put the gear back in")?;
    return_hand(&bot, &l);
    tokio::time::sleep(Duration::from_millis(300)).await;
    chests::fly_within_reach(&bot, chest).await?;
    if let Some(id) = l.eclink.lock().id.take() {
        if open_chest(&bot).is_some_and(|(open, ..)| open == id) {
            bot.ecs.write().trigger(CloseContainerEvent { entity: bot.entity, id });
        }
        info!(target: "eclink", "link closed to take the gear off");
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let gear: Vec<(Worn, &'static str)> = l.eclink.lock().loans.iter().filter(|x| x.how != Worn::Hand).map(|x| (x.how, x.item)).collect();
    let player = bot.get_inventory().map_err(|e| e.to_string())?;
    for (how, item) in &gear {
        let slot = match how {
            Worn::Armor(s) => *s as usize,
            Worn::Offhand => 45,
            Worn::Hand => continue,
        };
        if inv::stack_id(&worn_slot(&bot, slot)) == *item {
            player.shift_click(slot);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ids: Vec<&'static str> = gear.iter().map(|(_, i)| *i).collect();
    let moved = chests::store(&bot, chest, |s| ids.contains(&inv::stack_id(s)), ids.len()).await?;
    // What did not come off or go in is still worn (bags or chest full): kept as a loan.
    {
        let mut e = l.eclink.lock();
        let mut left = Vec::new();
        for x in e.loans.drain(..) {
            let still = match x.how {
                Worn::Hand => true,
                Worn::Armor(s) => inv::stack_id(&worn_slot(&bot, s as usize)) == x.item,
                Worn::Offhand => inv::stack_id(&worn_slot(&bot, 45)) == x.item || inv::count_id(&bot, x.item) > 0,
            };
            if still {
                left.push(x);
            }
        }
        e.loans = left;
    }
    Ok(format!("put {moved} piece(s) of gear back in the ender chest"))
}

// ------------------------------------------------------------------ hand loans

/// Still the same open container? If not, the link is gone (loans are kept, owed).
fn check(bot: &Client, e: &mut EcLink) -> bool {
    let Some(id) = e.id else { return false };
    if open_chest(bot).is_some_and(|(open, ..)| open == id) {
        return true;
    }
    e.id = None;
    info!(target: "eclink", "link lost (the container was closed or replaced){}", if e.loans.is_empty() { String::new() } else { format!(" - {} loan(s) out", e.loans.len()) });
    false
}

/// Take ender chest slot `ec_slot` into the HELD hotbar slot. False when it did not go out now
/// (no link, low health, or a loan in the hand went back first - ask again next tick).
pub fn borrow(bot: &Client, l: &BotLink, ec_slot: usize, panic_health: f32, ticks: u64) -> bool {
    if !healthy_enough(bot, panic_health) {
        return false;
    }
    let mut e = l.eclink.lock();
    if !check(bot, &mut e) {
        return false;
    }
    let Ok(held) = bot.selected_hotbar_slot() else { return false };
    if e.loans.iter().any(|x| x.how == Worn::Hand && x.hotbar == held) {
        return_one(bot, &mut e, held);
        return false;
    }
    let Some((_, slots, start)) = open_chest(bot) else { return false };
    let item = slots.get(ec_slot).map(inv::stack_id).unwrap_or("air");
    if item == "air" {
        return false;
    }
    if !inv::swap_in(bot, ec_slot, held, Some(start + 27 + held as usize)) {
        return false;
    }
    e.loans.push(Loan { how: Worn::Hand, hotbar: held, ec_slot, item, used: ticks });
    e.borrows += 1;
    info!(target: "eclink", "borrowed {item} (ender chest slot {ec_slot}) into hotbar {held}");
    true
}

/// Health enough to take something into the hand (the panic level and a margin over it).
pub fn healthy_enough(bot: &Client, panic_health: f32) -> bool {
    bot.health().is_ok_and(|h| h > panic_health + BORROW_MARGIN)
}

/// The loan in the hand is in use this tick (it stays out).
pub fn touch_held(bot: &Client, l: &BotLink, ticks: u64) {
    let Ok(held) = bot.selected_hotbar_slot() else { return };
    for x in l.eclink.lock().loans.iter_mut().filter(|x| x.how == Worn::Hand && x.hotbar == held) {
        x.used = ticks;
    }
}

/// Put one hand loan back (by the hotbar slot it went to). True when it is back.
fn return_one(bot: &Client, e: &mut EcLink, hotbar: u8) -> bool {
    let Some(i) = e.loans.iter().position(|x| x.how == Worn::Hand && x.hotbar == hotbar) else { return true };
    let Some((_, _, start)) = open_chest(bot) else { return false };
    let loan = e.loans[i].clone();
    let bags = inv::menu(bot).map(|m| m.slots()).unwrap_or_default();
    let is_it = |slot: usize| bags.get(slot).map(inv::stack_id) == Some(loan.item);
    // Where it is now: its hotbar slot, another hotbar slot, or moved into the main bags (a swap
    // that took its slot) - then back into a hotbar slot first, returned next tick.
    let at = if is_it(36 + loan.hotbar as usize) {
        Some(loan.hotbar)
    } else {
        (0..9u8).find(|h| is_it(36 + *h as usize))
    };
    let Some(h) = at else {
        if let Some(slot) = inv::MAIN.clone().find(|s| is_it(*s)) {
            debug!(target: "eclink", "{} was moved to bag slot {slot} - back to hotbar {} first", loan.item, loan.hotbar);
            inv::swap_with(bot, slot, loan.hotbar);
        } else {
            debug!(target: "eclink", "{} is not in the bags - kept owed", loan.item);
        }
        return false;
    };
    if inv::swap_in(bot, loan.ec_slot, h, Some(start + 27 + h as usize)) {
        info!(target: "eclink", "returned {} from hotbar {h} to ender chest slot {}", loan.item, loan.ec_slot);
        e.loans.remove(i);
        true
    } else {
        false
    }
}

/// Every hand loan back in the chest (most recent first). True when none is left out. Worn gear
/// stays on.
pub fn return_hand(bot: &Client, l: &BotLink) -> bool {
    let mut e = l.eclink.lock();
    if !e.loans.iter().any(|x| x.how == Worn::Hand) {
        return true;
    }
    if !check(bot, &mut e) {
        return false;
    }
    for h in e.loans.iter().rev().filter(|x| x.how == Worn::Hand).map(|x| x.hotbar).collect::<Vec<_>>() {
        return_one(bot, &mut e, h);
    }
    !e.loans.iter().any(|x| x.how == Worn::Hand)
}

/// Close the link (a print job, or the link switched off). Hand loans and the totem go back by
/// swaps first; armor (or gear out while the link is down) needs the trip to the chest.
fn close(bot: &Client, ctx: &Arc<SwarmCtx>, l: &Arc<BotLink>, why: &str) {
    let trip = {
        let e = l.eclink.lock();
        e.loans.iter().any(|x| matches!(x.how, Worn::Armor(_))) || (e.loans.iter().any(|x| x.how == Worn::Offhand) && (!e.linked() || inv::stack_id(&worn_slot(bot, 45)) != TOTEM))
    };
    if trip {
        if !l.errand_running() {
            l.start_errand("putting the borrowed gear back in the ender chest", put_gear_back(bot.clone(), ctx.clone(), l.clone()));
        }
        return;
    }
    if !return_hand(bot, l) {
        return;
    }
    let mut e = l.eclink.lock();
    if !totem_back(bot, &mut e) {
        return;
    }
    if let Some(id) = e.id.take() {
        if open_chest(bot).is_some_and(|(open, ..)| open == id) {
            bot.ecs.write().trigger(CloseContainerEvent { entity: bot.entity, id });
        }
        info!(target: "eclink", "link closed ({why})");
    }
}

// ------------------------------------------------------------------ food

/// Low on food and linked: the best stack the chest has that auto-eat would eat, shift-clicked
/// into the bags. True when one went.
pub fn take_food(bot: &Client, l: &BotLink, rule: &crate::modules::autoeat::FoodRule) -> bool {
    {
        let mut e = l.eclink.lock();
        if !check(bot, &mut e) {
            return false;
        }
    }
    if inv::empty_slots(bot) == 0 {
        return false;
    }
    let best = contents(bot, l)
        .into_iter()
        .filter(|(_, s)| rule.edible(s))
        .max_by_key(|(_, s)| s.count() * s.get_component::<Food>().map_or(0, |f| f.nutrition));
    let Some((slot, st)) = best else { return false };
    let Ok(c) = bot.get_inventory() else { return false };
    c.shift_click(slot);
    info!(target: "eclink", "took {} {} from the ender chest to eat", st.count(), inv::stack_id(&st));
    true
}

// ------------------------------------------------------------------ the tick

fn totem_pops(bot: &Client) -> u32 {
    bot.query_self::<Option<&TotemPops>, _>(|p| p.map_or(0, |p| p.0)).unwrap_or(0)
}

/// One tick, after the bot's jobs: notice a lost link, a death, a totem used; give back hand loans
/// not in use; return them at low health or for a chest trip; close it for a print job; and link
/// whenever it is not linked - not mid-fight.
pub fn tick(bot: &Client, ctx: &Arc<SwarmCtx>, l: &Arc<BotLink>, printing: bool, fighting: bool, deaths: u32, ticks: u64) {
    let (on, panic, range) = {
        let c = ctx.cfg.read();
        (c.ec_link, c.ec_panic_health, c.ec_search_range)
    };
    let health = bot.health().unwrap_or(20.0);
    let pops = totem_pops(bot);
    {
        let mut e = l.eclink.lock();
        // Died: what was borrowed dropped where it died (the recovery errand picks it up), and the
        // link died with it. azalea keeps the container open through a respawn (17 Oct test: the
        // bot "returned" a sword it had dropped, and its bags read the dead player's) - so it is
        // shut here, without a close packet: the server has nothing open.
        let died = e.deaths_seen.is_some_and(|d| d != deaths) || health <= 0.0;
        e.deaths_seen = Some(deaths);
        if died {
            if !e.loans.is_empty() {
                warn!(target: "eclink", "died with {} borrowed - dropped where it died", e.loans.iter().map(|x| x.item).collect::<Vec<_>>().join(", "));
                e.loans.clear();
            }
            if let Some(id) = e.id.take() {
                let _ = bot.query_self::<&mut Inventory, _>(|mut inv| {
                    if inv.id == id {
                        inv.container_menu = None;
                        inv.container_menu_title = None;
                        inv.id = 0;
                    }
                });
                info!(target: "eclink", "link lost (died)");
            }
            // Its dropped items first (they despawn): the recovery errand starts 3 s after a death.
            e.retry_at = e.retry_at.max(ticks + 200);
        }
        // A totem saved it: that one is used up (and the offhand empty - the server does not say
        // so while a container is open).
        if pops != e.pops_seen {
            e.pops_seen = pops;
            if let Some(i) = e.loans.iter().position(|x| x.how == Worn::Offhand) {
                e.loans.remove(i);
                info!(target: "eclink", "the borrowed totem saved it - another from the chest if there is one");
            }
            set_worn_slot(bot, 45, ItemStack::Empty);
        }
        check(bot, &mut e);
    }
    let errand = l.errand_running();
    let hand_out = l.eclink.lock().loans.iter().any(|x| x.how == Worn::Hand);
    if health > 0.0 && health <= panic && hand_out {
        info!(target: "eclink", "health {health:.0} - borrowed items back in the ender chest");
        return_hand(bot, l);
    } else if printing || !on {
        let open = {
            let e = l.eclink.lock();
            !e.loans.is_empty() || e.linked()
        };
        if open {
            close(bot, ctx, l, if printing { "print job" } else { "switched off" });
        }
    } else if errand {
        return_hand(bot, l);
    } else {
        let idle: Vec<u8> = l.eclink.lock().loans.iter().filter(|x| x.how == Worn::Hand && ticks.saturating_sub(x.used) > IDLE_RETURN_TICKS).map(|x| x.hotbar).collect();
        if !idle.is_empty() {
            let mut e = l.eclink.lock();
            if check(bot, &mut e) {
                for h in idle {
                    return_one(bot, &mut e, h);
                }
            }
        }
        // A totem used up (or never had one): the next from the chest.
        if ticks.is_multiple_of(10) && l.eclink.lock().linked() {
            totem_in(bot, l, ticks);
        }
    }

    // Link whenever it is not linked: a carried chest, one loaded or remembered within range, else
    // a search - unless another bot is searching, or a search from near here found nothing.
    let here = bot.position().unwrap_or(Vec3::ZERO);
    let linked = l.eclink.lock().linked();
    let (searcher, none_here) = {
        let s = ctx.ec_search.lock();
        (s.by.clone(), s.none_found.is_some_and(|(c, r)| (r - range).abs() < 1.0 && horiz(c, here) <= r / 2.0))
    };
    let can = on && !printing && !fighting && !errand && !linked && ticks >= l.eclink.lock().retry_at && ticks.is_multiple_of(20);
    let mut waiting = None;
    if can {
        let has = carried_ender_chest(bot).is_some() || !candidates(bot, ctx, here, range).is_empty();
        if has || (searcher.is_none() && !none_here) {
            l.eclink.lock().retry_at = ticks + RETRY_TICKS;
            l.start_errand("linking the ender chest", link(bot.clone(), ctx.clone(), l.clone()));
        } else if let Some(by) = &searcher {
            waiting = Some(format!("waiting for {by}'s ender chest search"));
        }
    }
    let mut e = l.eclink.lock();
    let mut has = String::new();
    if !e.held().is_empty() {
        has += &format!(", has {}", e.held().join(", "));
    }
    if !e.worn().is_empty() {
        has += &format!(", wearing {}", e.worn().join(", "));
    }
    let mine = searcher.as_ref() == Some(&l.name);
    e.status = match (e.id, e.chest) {
        (Some(_), Some(c)) => format!("linked {}{has}", fmt(c)),
        _ if !on => format!("off{has}"),
        _ if printing => format!("closed for the print job{has}"),
        _ if mine => format!("searching for an ender chest - {}{has}", ctx.ec_search.lock().progress),
        _ => match waiting {
            Some(w) => format!("{w}{has}"),
            None if none_here && !linked => format!("no ender chest within {range:.0} blocks - set one on the panel{has}"),
            None => format!("not linked{has}"),
        },
    };
}
