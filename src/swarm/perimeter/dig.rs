//! One perimeter bot's digging, a tick at a time: reading how far down the cells around it are
//! clear (fast_forward_near), its claim on the board, the band it is cutting - what to break and
//! what water or lava to seal first (plan) - and where to hover.
//!
//! Breaking is the nuker's (nuker.rs): the best tool in the bags, an instant block START+STOP and
//! several a tick, a slow one START then STOP once its progress passes stop-progress, one at a
//! time. Sealing is LavaClear/SpongeDrain's: the fill block in the offhand, clicked onto the
//! fluid's own cell - water and lava are replaceable, so the block goes into it.

use std::collections::{HashMap, HashSet};

use azalea::{
    BlockPos, Client, Vec3,
    block::{
        BlockState,
        fluid_state::{FluidKind, FluidState},
        properties::Persistent,
    },
    core::position::ChunkPos,
    registry::{builtin::BlockKind, tags},
    world::World,
};
use tracing::{debug, info};

use super::{CELL, Claim, JobConfig, Perimeter, Refused, base};
use crate::{
    chests, inv,
    modules::harvest,
    movement,
    print::{
        act,
        place::{Choice, Click, Dir, block_dist},
    },
    swarm::{BotLink, SwarmCtx, nuker::{self, Box3}},
};

/// Server reach, eye to block (4.5 plus its slack, less a margin).
const REACH: f64 = 4.5;
/// A break or a placement that did not show in the world is tried again after this many ticks.
const RETRY_TICKS: u64 = 20;
/// Tried this often in one band and still there: the server will not let it go (WorldGuard, the
/// world border) - it is left standing. A block only slow to go took 5 at most (4 Nov).
const REFUSED_TRIES: u32 = 10;
/// Rescan the band this often (ticks).
const SCAN_TICKS: u64 = 5;
/// Block reads a bot spends (every 10 ticks) finding how far down the cells around it are clear.
const READ_BUDGET: usize = 40_000;
/// Known cells this close (in cells) are read again for air below their floor.
const NEAR_CELLS: i32 = 3;
/// Ticks with sealing to do, no fill and nothing else done: the cell is put off for a while.
const NO_FILL_TICKS: u32 = 200;
const PARK_SECS: u64 = 120;
/// Further than this from the box (blocks, flat) a bot holds rather than fly back: it has
/// respawned at spawn, far off - hours of flying through unknown ground.
const FAR: f64 = 10_000.0;
/// No tools trip in a bot's first seconds after joining.
const JOIN_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

/// One bot's digging state.
#[derive(Default)]
pub struct BotDig {
    /// A slow block being broken: where, the tick its STOP goes, and whether its water is sealed
    /// straight after (a waterlogged block, kelp).
    mining: Option<(BlockPos, u64, bool)>,
    /// Breaks and placements sent, and when they may be tried again.
    fired: HashMap<BlockPos, u64>,
    /// How often each was sent in this band (REFUSED_TRIES).
    tries: HashMap<BlockPos, u32>,
    /// What is left in the band, top first.
    queue: Vec<BlockPos>,
    last_scan: u64,
    /// The cell and floor the queue is for.
    scanned: Option<((i32, i32), i32)>,
    tool_ready_at: u64,
    /// Not before: the next look for a cell, after there was none.
    next_claim_at: u64,
    /// The cell whose band it just finished (taken again if it may go on).
    last_cell: Option<(i32, i32)>,
    /// Not before: the next tools trip for a shovel or an axe (base::KIT_RECHECK).
    kit_check_at: u64,
    no_fill_ticks: u32,
    pub cell: Option<(i32, i32)>,
    pub status: String,
    pub broken: u64,
    pub placed: u64,
}

/// What the digging wants of the tick.
pub enum Want {
    Nothing,
    /// It is moving the bot.
    Moving,
    /// No usable pickaxe: a trip to the tools chest.
    Tools,
}

// ------------------------------------------------------------------ what is where

/// What a position holds, for the digging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// Nothing to do: air, what nothing breaks (bedrock, portals, barriers), a protected container.
    Clear,
    /// Water or lava a block can go into: a source, a flow, seagrass, a bubble column.
    Fluid,
    /// A block to break - a waterlogged one or kelp too (its water is sealed when it goes).
    Block,
}

pub fn what(state: BlockState, protect_containers: bool) -> What {
    let kind = BlockKind::from(state);
    if matches!(kind, BlockKind::Air | BlockKind::CaveAir | BlockKind::VoidAir) {
        return What::Clear;
    }
    if holds_fluid(state) && tags::blocks::REPLACEABLE.contains(&kind) {
        return What::Fluid;
    }
    if state.to_trait().behavior().destroy_time < 0.0 || matches!(kind, BlockKind::Light | BlockKind::StructureVoid | BlockKind::MovingPiston) {
        return What::Clear;
    }
    if kind.to_str().contains("portal") || (protect_containers && chests::is_container(kind)) {
        return What::Clear;
    }
    if !holds_fluid(state) && left_growing(state, kind) {
        return What::Clear;
    }
    What::Block
}

/// Plants left where they are: grass and ferns (not the grass block), leaf litter - they drop when the ground under
/// them is dug - and leaves, which decay once the logs are gone. Not leaves a player placed
/// (persistent): those never decay. A waterlogged one is not left either - its water would stay.
fn left_growing(state: BlockState, kind: BlockKind) -> bool {
    use BlockKind as K;
    if tags::blocks::LEAVES.contains(&kind) {
        return state.property::<Persistent>() != Some(true);
    }
    matches!(kind, K::ShortGrass | K::TallGrass | K::Fern | K::LargeFern | K::ShortDryGrass | K::TallDryGrass | K::Bush | K::LeafLitter)
}

fn holds_fluid(state: BlockState) -> bool {
    FluidState::from(state).kind != FluidKind::Empty
}

fn is_lava(state: BlockState) -> bool {
    FluidState::from(state).kind == FluidKind::Lava
}

/// What to do now about something that has to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// Break it; `seal`: it holds water (waterlogged, kelp), which gets a block straight after.
    Break { pos: BlockPos, seal: bool },
    /// Put a block into the fluid here.
    Seal(BlockPos),
}

/// Water and lava flow sideways and down, so the fluid that matters to a block is beside it or
/// above it.
const FLOWS_IN_FROM: [(i32, i32, i32); 5] = [(0, 1, 0), (0, 0, -1), (0, 0, 1), (1, 0, 0), (-1, 0, 0)];

/// What to do about `p`: a fluid gets a block; a block is broken - unless water or lava is beside
/// or above it, which is sealed first (a fluid held in a waterlogged block or kelp is broken out
/// and sealed in one go). Inside the box a seal is dug out later like any block; outside it, it
/// is the edge wall. Nothing while a neighbour is not loaded (never break blind), nothing to what
/// the server will not let go (`left`).
pub fn plan(w: &World, p: BlockPos, protect: bool, left: &Refused) -> Vec<Act> {
    let Some(st) = w.get_block_state(p) else { return Vec::new() };
    if left.has(p) {
        return Vec::new();
    }
    match what(st, protect) {
        What::Clear => Vec::new(),
        What::Fluid => vec![Act::Seal(p)],
        What::Block => {
            let mut first = Vec::new();
            for (dx, dy, dz) in FLOWS_IN_FROM {
                let q = BlockPos::new(p.x + dx, p.y + dy, p.z + dz);
                let Some(qs) = w.get_block_state(q) else { return Vec::new() };
                // Water where the server will not let a block go in: nothing to be done about it.
                if !holds_fluid(qs) || left.has(q) {
                    continue;
                }
                match what(qs, protect) {
                    What::Fluid => first.push(Act::Seal(q)),
                    What::Block => first.push(Act::Break { pos: q, seal: true }),
                    // Water in something nothing breaks: nothing to be done about it.
                    What::Clear => {}
                }
            }
            if first.is_empty() { vec![Act::Break { pos: p, seal: holds_fluid(st) }] } else { first }
        }
    }
}

/// Everything in `b` that has to go (not what is `left`). None while part of it is not loaded.
fn scan(w: &World, b: &Box3, protect: bool, left: &Refused) -> Option<Vec<BlockPos>> {
    let mut out = Vec::new();
    for y in (b.min.y..=b.max.y).rev() {
        for x in b.min.x..=b.max.x {
            for z in b.min.z..=b.max.z {
                let p = BlockPos::new(x, y, z);
                if what(w.get_block_state(p)?, protect) != What::Clear && !left.has(p) {
                    out.push(p);
                }
            }
        }
    }
    Some(out)
}

/// The highest thing to clear in a cell's columns (`b`'s x and z) from `from` down to `bottom`:
/// Some(Some(y)); Some(None) when there is nothing down to the bottom; None when part of it is
/// not loaded or the reads ran out. Sections the server counts empty are skipped whole; what is
/// `left` does not count.
fn highest_material(w: &World, b: &Box3, from: i32, bottom: i32, protect: bool, left: &Refused, budget: &mut usize) -> Option<Option<i32>> {
    let min_sec = w.chunks.min_y().div_euclid(16);
    let mut corners: Vec<ChunkPos> = Vec::new();
    for (x, z) in [(b.min.x, b.min.z), (b.max.x, b.min.z), (b.min.x, b.max.z), (b.max.x, b.max.z)] {
        let c = ChunkPos::from(BlockPos::new(x, 0, z));
        if !corners.contains(&c) {
            corners.push(c);
        }
    }
    let chunks = corners.iter().map(|c| w.chunks.get(c)).collect::<Option<Vec<_>>>()?;
    let mut y = from;
    while y >= bottom {
        let sec = y.div_euclid(16);
        let empty = chunks.iter().all(|ch| ch.read().sections.get((sec - min_sec) as usize).is_none_or(|s| s.block_count == 0));
        if empty {
            y = sec * 16 - 1;
            continue;
        }
        if *budget == 0 {
            return None;
        }
        for x in b.min.x..=b.max.x {
            for z in b.min.z..=b.max.z {
                *budget = budget.saturating_sub(1);
                let p = BlockPos::new(x, y, z);
                if what(w.get_block_state(p)?, protect) != What::Clear && !left.has(p) {
                    return Some(Some(y));
                }
            }
        }
        y -= 1;
    }
    Some(None)
}

/// How far down the cells around the bot are clear: unknown cells first (nearest first), then
/// known ones close by (air under a band just cut, a cave). Cells someone is cutting are left to
/// them; an unknown cell is read whoever claimed it (its scout is waiting for exactly that - 3 Nov:
/// skipped as claimed, the first scout hovered over its cell for minutes).
fn fast_forward_near(w: &World, p: &Perimeter, at: Vec3, protect: bool) {
    let (area, todo, left) = {
        let b = p.board.lock();
        let Some(area) = b.area else { return };
        let near = (NEAR_CELLS * CELL) as f64 + 4.0;
        let mut unknown: Vec<((i32, i32), f64)> = Vec::new();
        let mut known: Vec<((i32, i32), i32)> = Vec::new();
        for c in b.cells() {
            let d = ((c.0 as f64 + 4.0 - at.x).powi(2) + (c.1 as f64 + 4.0 - at.z).powi(2)).sqrt();
            match b.floor(c) {
                None => unknown.push((c, d)),
                Some(f) if f > area.min.y && d <= near && !b.is_claimed(c) => known.push((c, f)),
                _ => {}
            }
        }
        unknown.sort_by(|a, b| a.1.total_cmp(&b.1));
        let todo: Vec<((i32, i32), i32)> = unknown.into_iter().map(|(c, _)| (c, area.max.y)).chain(known.into_iter().map(|(c, f)| (c, f - 1))).collect();
        (area, todo, b.refused.clone())
    };
    let mut budget = READ_BUDGET;
    let mut found = Vec::new();
    for (c, from) in todo {
        if budget < 1024 {
            break;
        }
        let cb = Box3 { min: BlockPos::new(c.0, area.min.y, c.1), max: BlockPos::new((c.0 + CELL - 1).min(area.max.x), area.max.y, (c.1 + CELL - 1).min(area.max.z)) };
        if let Some(h) = highest_material(w, &cb, from, area.min.y, protect, &left, &mut budget) {
            found.push((c, h));
        }
    }
    if !found.is_empty() {
        let mut b = p.board.lock();
        for (c, h) in found {
            b.found(c, h);
        }
    }
}

// ------------------------------------------------------------------ the tick

pub fn tick(bot: &Client, ctx: &SwarmCtx, p: &Perimeter, link: &BotLink, ticks: u64) -> Want {
    let job = p.job.read().clone();
    let mut st = link.perimeter.lock();
    if !job.running {
        if st.cell.take().is_some() {
            p.board.lock().release(&link.name);
        }
        st.status = "paused".into();
        return Want::Nothing;
    }
    let Ok(pos) = bot.position() else { return Want::Nothing };
    let Ok(world) = bot.world() else { return Want::Nothing };
    let height = {
        let w = world.read();
        (w.chunks.min_y(), w.chunks.min_y() + w.chunks.height() as i32 - 1)
    };
    if !p.saw_world(height) {
        if st.cell.take().is_some() {
            p.board.lock().release(&link.name);
        }
        st.status = "in another dimension than the box - not digging".into();
        return Want::Nothing;
    }
    // Far off (respawned at spawn, say): it is not flown back across the world.
    let away = p.board.lock().area.map(|a| {
        let dx = (a.min.x as f64 - pos.x).max(pos.x - a.max.x as f64 - 1.0).max(0.0);
        let dz = (a.min.z as f64 - pos.z).max(pos.z - a.max.z as f64 - 1.0).max(0.0);
        (dx * dx + dz * dz).sqrt()
    });
    if let Some(d) = away.filter(|d| *d > FAR) {
        if st.cell.take().is_some() {
            p.board.lock().release(&link.name);
        }
        st.status = format!("{d:.0} blocks from the box - bring it back near the base (it does not fly that far)");
        return Want::Nothing;
    }
    st.fired.retain(|_, until| *until > ticks);

    // A slow block under way: STOP when it is due - the server breaks it then - and seal its
    // water straight after if it held some.
    if let Some((b, stop_at, seal)) = st.mining {
        if ticks < stop_at {
            st.status = format!("breaking {} {} {}", b.x, b.y, b.z);
            return Want::Moving;
        }
        harvest::send_action(bot, b, false);
        debug!(target: "perimeter", "STOP {} {} {}", b.x, b.y, b.z);
        st.mining = None;
        st.broken += 1;
        let sealed = seal && base::fill_ready(bot, &job) == base::Fill::Ready && {
            place(bot, b);
            st.placed += 1;
            true
        };
        let mut board = p.board.lock();
        board.broken += 1;
        if sealed {
            board.placed += 1;
        }
    }

    if !base::has_pickaxe(bot, job.retire_uses) {
        if st.cell.take().is_some() {
            p.board.lock().release(&link.name);
        }
        // Not in its first seconds in the world: the chests are not in yet (3 Nov: the second bot
        // opened the tools chest 0.1 s after joining, it "did not open", and two minutes passed
        // before the retry).
        if link.joined_at.lock().is_none_or(|t| t.elapsed() < JOIN_SETTLE) {
            st.status = "just joined - waiting for the chests to load".into();
            return Want::Nothing;
        }
        st.status = "no usable pickaxe - fetching tools".into();
        return Want::Tools;
    }
    // A shovel or an axe short (worn out, or none in the chest last time): a tools trip for it now,
    // and again every KIT_RECHECK while the chest has none (4 Nov: a bot dug grass and dirt by
    // hand, 16 ticks a block, for hours - a trip only ever went for a missing pickaxe).
    if ticks >= st.kit_check_at
        && link.joined_at.lock().is_some_and(|t| t.elapsed() >= JOIN_SETTLE)
        && let Some(kind) = base::kit_short(bot, job.retire_uses)
    {
        st.kit_check_at = ticks + base::KIT_RECHECK.as_secs() * 20;
        if st.cell.take().is_some() {
            p.board.lock().release(&link.name);
        }
        st.status = format!("no usable {kind} - fetching tools");
        return Want::Tools;
    }

    if ticks.is_multiple_of(10) {
        fast_forward_near(&world.read(), p, pos, job.protect_containers);
    }

    let holding = st.cell.is_some();
    if !holding && ticks < st.next_claim_at {
        return Want::Nothing;
    }
    let prefer = st.last_cell.take();
    let claim = p.board.lock().claim(&link.name, pos, job.spread, prefer);
    match claim {
        Claim::Done => {
            st.cell = None;
            st.next_claim_at = ticks + 100;
            if p.board.lock().just_finished() {
                info!(target: "perimeter", "the box is cleared");
                ctx.feed("event", "the perimeter box is cleared, top to bottom");
            }
            st.status = "box cleared".into();
            Want::Nothing
        }
        Claim::Wait(why) => {
            st.cell = None;
            st.next_claim_at = ticks + 20;
            st.status = why;
            Want::Nothing
        }
        Claim::Scout(c) => {
            st.cell = Some(c);
            // Close enough and its chunks come in: its ground is read (fast_forward_near).
            let spot = Vec3::new(c.0 as f64 + 4.5, pos.y, c.1 as f64 + 4.5);
            movement::steer(bot, spot, "perimeter-scout", 16.0);
            st.status = format!("scouting the box (cell {} {})", c.0, c.1);
            Want::Moving
        }
        Claim::Dig(c) => {
            st.cell = Some(c);
            dig_cell(bot, ctx, p, &mut st, &job, c, pos, &world, ticks)
        }
    }
}

/// One tick of cutting cell `c`'s next band.
#[allow(clippy::too_many_arguments)]
fn dig_cell(
    bot: &Client,
    ctx: &SwarmCtx,
    p: &Perimeter,
    st: &mut BotDig,
    job: &JobConfig,
    c: (i32, i32),
    pos: Vec3,
    world: &std::sync::Arc<parking_lot::RwLock<World>>,
    ticks: u64,
) -> Want {
    let eye = pos + Vec3::new(0.0, 1.62, 0.0);
    let protect = job.protect_containers;
    let (area, floor, band, left) = {
        let b = p.board.lock();
        match (b.area, b.floor(c)) {
            (Some(a), Some(f)) => (a, f, b.band, b.refused.clone()),
            _ => return Want::Nothing,
        }
    };
    let lo = (floor - band).max(area.min.y);
    let cell = Box3 { min: BlockPos::new(c.0, lo, c.1), max: BlockPos::new((c.0 + CELL - 1).min(area.max.x), floor - 1, (c.1 + CELL - 1).min(area.max.z)) };
    let middle = |y: f64| Vec3::new((cell.min.x + cell.max.x) as f64 / 2.0 + 0.5, y, (cell.min.z + cell.max.z) as f64 / 2.0 + 0.5);

    if st.scanned != Some((c, floor)) || ticks.saturating_sub(st.last_scan) >= SCAN_TICKS || st.queue.is_empty() {
        if st.scanned != Some((c, floor)) {
            st.tries.clear();
        }
        st.last_scan = ticks;
        st.scanned = Some((c, floor));
        let w = world.read();
        // Something back above the band (water that flowed in, a block that landed): the floor
        // goes back up over it first.
        let above = Box3 { min: BlockPos::new(cell.min.x, floor, cell.min.z), max: BlockPos::new(cell.max.x, (floor + band - 1).min(area.max.y), cell.max.z) };
        if floor <= area.max.y
            && let Some(found) = scan(&w, &above, protect, &left)
            && let Some(top) = found.first()
        {
            let y = top.y;
            drop(w);
            debug!(target: "perimeter", "cell {c:?}: something at y {y} above its floor {floor} - back up");
            p.board.lock().raise(c, y);
            st.queue.clear();
            return Want::Moving;
        }
        match scan(&w, &cell, protect, &left) {
            None => {
                drop(w);
                // Nothing left from another cell to hover over meanwhile: 3 Nov, back from the tools
                // chest with a cell 500 blocks off, a bot steered to the new cell one tick in five and
                // to the old cell's queue the other four - two hours climbing and dropping at Y 330.
                st.queue.clear();
                movement::steer(bot, middle(floor as f64 + 1.0), "perimeter", 1.0);
                st.status = format!("cell {} {}: waiting for it to load", c.0, c.1);
                return Want::Moving;
            }
            Some(found) if found.is_empty() => {
                // The band is cut: down a band, and on through any air below it.
                let mut budget = 8192;
                let below = (lo > area.min.y).then(|| highest_material(&w, &cell, lo - 1, area.min.y, protect, &left, &mut budget)).flatten();
                drop(w);
                let mut b = p.board.lock();
                b.finish_band(c);
                if let Some(h) = below {
                    b.found(c, h);
                }
                st.queue.clear();
                st.cell = None;
                st.last_cell = Some(c);
                st.status = format!("cell {} {}: band down to y {lo} cut", c.0, c.1);
                return Want::Moving;
            }
            Some(mut found) => {
                found.sort_by(|a, b| b.y.cmp(&a.y).then(eye.distance_to(centre(*a)).total_cmp(&eye.distance_to(centre(*b)))));
                st.queue = found;
            }
        }
    }

    if ticks < st.tool_ready_at {
        return Want::Moving;
    }

    // Break and seal what is in reach, the top layer first.
    let w = world.read();
    let mut breaks = 0;
    let mut placed: Vec<BlockPos> = Vec::new();
    let mut need_fill = false;
    let mut this_tick: HashSet<BlockPos> = HashSet::new();
    let near: Vec<BlockPos> = st.queue.iter().filter(|q| eye.distance_to(centre(**q)) <= REACH + 2.0).take(48).copied().collect();
    let max_places = job.places_per_tick.max(1);
    let max_breaks = job.max_breaks_per_tick.clamp(1, 20);
    'work: for q in near {
        for act in plan(&w, q, protect, &left) {
            match act {
                Act::Seal(f) => {
                    if placed.len() >= max_places || st.fired.contains_key(&f) || block_dist(eye, f) > REACH || in_body(pos, f) || !this_tick.insert(f) {
                        continue;
                    }
                    if refused(st, ctx, p, f) {
                        continue;
                    }
                    match base::fill_ready(bot, job) {
                        base::Fill::Ready => {}
                        base::Fill::Swapping => {
                            st.tool_ready_at = ticks + 2;
                            break 'work;
                        }
                        base::Fill::Out => {
                            need_fill = true;
                            continue;
                        }
                    }
                    place(bot, f);
                    debug!(target: "perimeter", "seal {} {} {}", f.x, f.y, f.z);
                    st.fired.insert(f, ticks + RETRY_TICKS);
                    *st.tries.entry(f).or_default() += 1;
                    placed.push(f);
                }
                Act::Break { pos: b, seal } => {
                    if breaks >= max_breaks || st.fired.contains_key(&b) || block_dist(eye, b) > REACH || !this_tick.insert(b) {
                        continue;
                    }
                    let Some(state) = w.get_block_state(b) else { continue };
                    let (slot, per_tick) = nuker::best_tool_keeping(bot, state, job.retire_uses + 1);
                    if per_tick <= 0.0 {
                        st.fired.insert(b, ticks + RETRY_TICKS * 30);
                        continue;
                    }
                    if refused(st, ctx, p, b) {
                        continue;
                    }
                    // A tool to take in hand first: the break waits for the swap to land.
                    if let Some(slot) = slot
                        && bot.selected_hotbar_slot().map(|h| h as usize + 36).ok() != Some(slot)
                    {
                        inv::hold(bot, slot, &[]);
                        st.tool_ready_at = ticks + 3;
                        break 'work;
                    }
                    if per_tick >= 1.0 {
                        harvest::send_break(bot, b);
                        st.fired.insert(b, ticks + RETRY_TICKS);
                        *st.tries.entry(b).or_default() += 1;
                        breaks += 1;
                        // Its water gets a block in the same tick, before it can flow.
                        if seal && placed.len() < max_places && base::fill_ready(bot, job) == base::Fill::Ready {
                            place(bot, b);
                            placed.push(b);
                        }
                    } else {
                        if breaks > 0 {
                            break 'work;
                        }
                        // START now, STOP once the progress passes stop-progress.
                        harvest::send_action(bot, b, true);
                        let wait = (job.stop_progress.clamp(0.7, 1.0) / per_tick as f64).ceil() as u64;
                        debug!(target: "perimeter", "START {} {} {} perTick {per_tick:.3} -> STOP in {wait} ticks", b.x, b.y, b.z);
                        st.mining = Some((b, ticks + wait, seal));
                        st.fired.insert(b, ticks + wait + RETRY_TICKS);
                        *st.tries.entry(b).or_default() += 1;
                        breaks += 1;
                        break 'work;
                    }
                }
            }
        }
    }
    // Where to hover: over the highest thing left (a little higher over lava).
    let next = st.queue.iter().find(|q| !st.fired.contains_key(*q)).or(st.queue.first()).copied();
    let spot = next.map(|n| {
        let lift = if w.get_block_state(n).is_some_and(is_lava) { 2.0 } else { 1.0 };
        let s = Vec3::new(n.x as f64 + 0.5, n.y as f64 + lift, n.z as f64 + 0.5);
        if movement::geom::stand_clear(&w, s) { s } else { movement::geom::nearest_clear(&w, s, 3).unwrap_or(s) }
    });
    drop(w);

    let instant_breaks = if st.mining.is_some() { breaks.saturating_sub(1) } else { breaks };
    st.broken += instant_breaks as u64;
    st.placed += placed.len() as u64;
    {
        let mut b = p.board.lock();
        b.broken += instant_breaks as u64;
        b.placed += placed.len() as u64;
        for f in &placed {
            if f.x < area.min.x || f.x > area.max.x || f.z < area.min.z || f.z > area.max.z || f.y > area.max.y {
                b.walled += 1;
            } else if let Some(fc) = b.cell_of(f.x, f.z) {
                // A block put above a cell's floor (water had come back in there): it goes back up.
                b.raise(fc, f.y);
            }
        }
    }
    let acted = breaks > 0 || !placed.is_empty();

    // Sealing to do and no fill: pick some up, make room for it, or leave the cell for a while.
    if need_fill && !acted {
        st.no_fill_ticks += 1;
        if let Some(item) = base::fill_lying_near(bot, job, 24.0) {
            movement::steer(bot, item + Vec3::new(0.0, 0.2, 0.0), "perimeter-fill", 0.5);
            st.status = format!("cell {} {}: picking up fill", c.0, c.1);
            return Want::Moving;
        }
        if inv::empty_slots(bot) == 0
            && ticks.is_multiple_of(10)
            && let Some(slot) = base::junk_slot(bot, job)
        {
            base::throw(bot, slot);
        }
        if st.no_fill_ticks > NO_FILL_TICKS {
            st.no_fill_ticks = 0;
            p.board.lock().park(c, PARK_SECS);
            st.cell = None;
            st.queue.clear();
            ctx.warn_once(&format!("perimeter: no fill to seal the water or lava in cell {} {} - it is left for {PARK_SECS} s (dig some cobblestone or dirt into the bags)", c.0, c.1));
            st.status = format!("cell {} {}: out of fill - left for now", c.0, c.1);
            return Want::Nothing;
        }
    } else if acted {
        st.no_fill_ticks = 0;
    }

    st.status = format!("cell {} {}: y {lo}..{} ({} left{})", c.0, c.1, floor - 1, st.queue.len(), if need_fill { ", needs fill" } else { "" });
    if let Some(s) = spot
        && !acted
        && eye.distance_to(s + Vec3::new(0.0, 0.5, 0.0)) > 1.0
    {
        movement::steer(bot, s, "perimeter", 0.5);
    }
    Want::Moving
}

/// `at` was sent REFUSED_TRIES times in this band and is still there: the server will not let it
/// go. It is left standing - for every bot (the board), and after a restart (progress.json).
fn refused(st: &BotDig, ctx: &SwarmCtx, p: &Perimeter, at: BlockPos) -> bool {
    if st.tries.get(&at).is_none_or(|n| *n < REFUSED_TRIES) {
        return false;
    }
    if let Some((what, side)) = p.board.lock().refuse(at) {
        info!(target: "perimeter", "the server puts {what} back every time ({REFUSED_TRIES} tries) - left standing");
        if side {
            ctx.feed("event", &format!("perimeter: the server will not let {what} be dug (WorldGuard or the world border) - left standing"));
        }
    }
    true
}

fn centre(p: BlockPos) -> Vec3 {
    Vec3::new(p.x as f64 + 0.5, p.y as f64 + 0.5, p.z as f64 + 0.5)
}

/// The block at `b` would overlap the bot (feet at `feet`): the server refuses to place it.
fn in_body(feet: Vec3, b: BlockPos) -> bool {
    let (bx, by, bz) = (b.x as f64, b.y as f64, b.z as f64);
    feet.x + 0.3 > bx && feet.x - 0.3 < bx + 1.0 && feet.z + 0.3 > bz && feet.z - 0.3 < bz + 1.0 && feet.y + 1.8 > by && feet.y < by + 1.0
}

/// Put the offhand's fill into the fluid at `f`: the click goes on the fluid's own cell, its top
/// face - the cell is replaceable, so the block goes there, with nothing solid needed beside it.
fn place(bot: &Client, f: BlockPos) {
    let hit = Vec3::new(f.x as f64 + 0.5, f.y as f64 + 1.0, f.z as f64 + 0.5);
    act::click(bot, &Choice { click: Click { clicked: f, face: Dir::Up, hit, yaw: 0.0, pitch: 90.0, sneak: false }, needs_look: false });
}

#[cfg(test)]
mod tests {
    use azalea::{
        core::position::ChunkPos,
        world::{Chunk, ChunkStorage, PartialChunkStorage},
    };

    use super::*;
    use crate::print::schem::parse_state;

    fn world(blocks: &[(BlockPos, &str)]) -> (World, PartialChunkStorage) {
        let mut chunks = ChunkStorage::default();
        let mut partial = PartialChunkStorage::new(4);
        for x in -2..=2 {
            for z in -2..=2 {
                partial.set(&ChunkPos { x, z }, Some(Chunk::default()), &mut chunks);
            }
        }
        let w = World::from(chunks);
        for (p, s) in blocks {
            w.set_block_state(*p, parse_state(s).unwrap());
        }
        (w, partial)
    }

    const STONE: &str = "minecraft:stone";
    const WATER: &str = "minecraft:water[level=0]";
    const LAVA: &str = "minecraft:lava[level=0]";

    fn at(x: i32, y: i32, z: i32) -> BlockPos {
        BlockPos::new(x, y, z)
    }

    #[test]
    fn what_needs_doing_to_each_kind_of_block() {
        let s = |id: &str| parse_state(id).unwrap();
        assert_eq!(what(s("minecraft:air"), false), What::Clear);
        assert_eq!(what(s("minecraft:cave_air"), false), What::Clear);
        assert_eq!(what(s("minecraft:bedrock"), false), What::Clear, "nothing breaks it");
        assert_eq!(what(s(STONE), false), What::Block);
        assert_eq!(what(s(WATER), false), What::Fluid);
        assert_eq!(what(s("minecraft:water[level=3]"), false), What::Fluid, "flowing water gets a block too");
        assert_eq!(what(s(LAVA), false), What::Fluid);
        assert_eq!(what(s("minecraft:seagrass"), false), What::Fluid, "seagrass is replaceable: the block goes straight in");
        assert_eq!(what(s("minecraft:kelp[age=0]"), false), What::Block, "kelp is broken out, then its water sealed");
        assert_eq!(what(s("minecraft:oak_slab[type=bottom,waterlogged=true]"), false), What::Block);
        assert_eq!(what(s("minecraft:chest[facing=north,type=single,waterlogged=false]"), true), What::Clear);
        assert_eq!(what(s("minecraft:chest[facing=north,type=single,waterlogged=false]"), false), What::Block);
        // Plants are left: they drop with the ground under them, leaves decay without their logs.
        assert_eq!(what(s("minecraft:short_grass"), false), What::Clear);
        assert_eq!(what(s("minecraft:tall_grass[half=upper]"), false), What::Clear);
        assert_eq!(what(s("minecraft:large_fern[half=lower]"), false), What::Clear);
        assert_eq!(what(s("minecraft:leaf_litter[facing=north,segment_amount=3]"), false), What::Clear);
        assert_eq!(what(s("minecraft:oak_leaves[distance=3,persistent=false,waterlogged=false]"), false), What::Clear);
        assert_eq!(what(s("minecraft:oak_leaves[distance=7,persistent=true,waterlogged=false]"), false), What::Block, "placed by a player: never decays");
        assert_eq!(what(s("minecraft:oak_leaves[distance=3,persistent=false,waterlogged=true]"), false), What::Block, "its water would stay behind");
        assert_eq!(what(s("minecraft:grass_block[snowy=false]"), false), What::Block, "the grass block is dug");
        assert_eq!(what(s("minecraft:oak_log[axis=y]"), false), What::Block);
    }

    #[test]
    fn a_dry_block_is_broken() {
        let (w, _keep) = world(&[(at(0, 10, 0), STONE)]);
        assert_eq!(plan(&w, at(0, 10, 0), false, &Refused::default()), vec![Act::Break { pos: at(0, 10, 0), seal: false }]);
    }

    #[test]
    fn water_beside_or_above_is_sealed_before_the_block_breaks() {
        let (w, _keep) = world(&[(at(0, 10, 0), STONE), (at(1, 10, 0), WATER), (at(0, 11, 0), LAVA), (at(0, 9, 0), WATER)]);
        let acts = plan(&w, at(0, 10, 0), false, &Refused::default());
        assert!(acts.contains(&Act::Seal(at(1, 10, 0))), "{acts:?}");
        assert!(acts.contains(&Act::Seal(at(0, 11, 0))), "lava above: {acts:?}");
        assert!(!acts.contains(&Act::Seal(at(0, 9, 0))), "water below cannot flow up into the hole");
        assert!(!acts.iter().any(|a| matches!(a, Act::Break { .. })), "not broken until they are sealed: {acts:?}");
    }

    #[test]
    fn a_fluid_on_its_own_gets_a_block() {
        let (w, _keep) = world(&[(at(3, 10, 3), WATER)]);
        assert_eq!(plan(&w, at(3, 10, 3), false, &Refused::default()), vec![Act::Seal(at(3, 10, 3))]);
    }

    #[test]
    fn water_in_kelp_beside_a_block_is_broken_out_and_sealed() {
        let (w, _keep) = world(&[(at(0, 10, 0), STONE), (at(0, 10, 1), "minecraft:kelp[age=3]")]);
        assert_eq!(plan(&w, at(0, 10, 0), false, &Refused::default()), vec![Act::Break { pos: at(0, 10, 1), seal: true }]);
    }

    #[test]
    fn a_waterlogged_block_with_dry_sides_is_broken_and_sealed_at_once() {
        let (w, _keep) = world(&[(at(0, 10, 0), "minecraft:oak_slab[type=bottom,waterlogged=true]")]);
        assert_eq!(plan(&w, at(0, 10, 0), false, &Refused::default()), vec![Act::Break { pos: at(0, 10, 0), seal: true }]);
    }

    #[test]
    fn what_the_server_will_not_let_go_is_left() {
        let (w, _keep) = world(&[(at(0, 10, 0), STONE), (at(1, 10, 0), STONE), (at(1, 10, 1), WATER), (at(5, 12, 5), STONE)]);
        let mut left = Refused::default();
        left.blocks.insert((0, 10, 0));
        assert!(plan(&w, at(0, 10, 0), false, &left).is_empty());
        assert_eq!(plan(&w, at(1, 10, 0), false, &left), vec![Act::Seal(at(1, 10, 1))]);
        // Water in a guarded spot gets no block: the stone beside it is broken.
        left.blocks.insert((1, 10, 1));
        assert_eq!(plan(&w, at(1, 10, 0), false, &left), vec![Act::Break { pos: at(1, 10, 0), seal: false }]);
        left.xs.insert(5);
        let b = Box3 { min: at(0, 0, 0), max: at(7, 15, 7) };
        assert_eq!(scan(&w, &b, false, &left), Some(vec![at(1, 10, 0)]));
        let mut budget = 10_000;
        assert_eq!(highest_material(&w, &b, 15, 0, false, &left, &mut budget), Some(Some(10)));
    }

    #[test]
    fn nothing_is_broken_beside_an_unloaded_chunk() {
        // x 47 is the last loaded column (chunks -2..=2); 48 is not loaded.
        let (w, _keep) = world(&[(at(47, 10, 0), STONE)]);
        assert!(plan(&w, at(47, 10, 0), false, &Refused::default()).is_empty());
    }

    #[test]
    fn the_highest_thing_in_a_cell_is_found_through_empty_sections() {
        let (w, _keep) = world(&[(at(2, 37, 3), STONE), (at(5, -20, 5), WATER)]);
        let cell = Box3 { min: at(0, -64, 0), max: at(7, 319, 7) };
        let mut budget = 100_000;
        assert_eq!(highest_material(&w, &cell, 319, -64, false, &Refused::default(), &mut budget), Some(Some(37)));
        assert!(budget > 100_000 - 64 * 16, "the empty sections above were not read block by block ({} reads)", 100_000 - budget);
        assert_eq!(highest_material(&w, &cell, 36, -64, false, &Refused::default(), &mut budget), Some(Some(-20)), "water counts");
        assert_eq!(highest_material(&w, &cell, -21, -64, false, &Refused::default(), &mut budget), Some(None));
        let unloaded = Box3 { min: at(100, -64, 0), max: at(107, 319, 7) };
        assert_eq!(highest_material(&w, &unloaded, 319, -64, false, &Refused::default(), &mut budget), None);
    }

    #[test]
    fn a_placed_block_never_goes_where_the_bot_stands() {
        let feet = Vec3::new(0.5, 10.0, 0.5);
        assert!(in_body(feet, at(0, 10, 0)));
        assert!(in_body(feet, at(0, 11, 0)));
        assert!(!in_body(feet, at(0, 12, 0)));
        assert!(!in_body(feet, at(0, 9, 0)));
        assert!(!in_body(feet, at(1, 10, 0)));
    }
}
