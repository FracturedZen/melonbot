//! One perimeter bot's digging, a tick at a time: reading how far down the cells around it are
//! clear (fast_forward_near), its claim on the board, the band it is cutting - what to break and
//! what water or lava to seal first (plan) - and where to hover.
//!
//! Breaking is the client's FastNuker's (6 Nov, the user: "the double break mechanic in client on
//! the bots too"), with the best tool in the bags:
//! - a block one tick takes past SEMI (stone with a netherite pick: 0.78) - START and STOP
//!   together, several a tick: the server takes a STOP at 0.7. It was START, two ticks, STOP, one
//!   at a time, the bot doing nothing else meanwhile: 14,701 of 17,500 slow breaks in a day.
//! - slower ones two at once. One goes in the server's delayed-destroy slot: START and STOP in one
//!   tick, its progress short of 0.7, and the server keeps it and breaks it itself once its
//!   progress reaches 1 - counted from its own START, so other breaks meanwhile do not touch it.
//!   The other is mined here: START, then STOP once its progress passes stop-progress. Nothing else
//!   is broken while that one is mined - every START restarts the server's count for it.
//! Sealing is LavaClear/SpongeDrain's: the fill block in the offhand, clicked onto the fluid's own
//! cell - water and lava are replaceable, so the block goes into it. Out of fill, it mines some
//! close by, any y in the box (fill_step).

use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

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
/// A block a tick takes past this is broken by START and STOP together (the client's
/// stop-progress: the server takes 0.7, this leaves a little over).
const SEMI: f32 = 0.72;
/// A START and STOP together is looked at again when it may be tried again: still there, that
/// block goes the slow way.
const SEMI_CHECK: u64 = RETRY_TICKS;
/// Refused this often, and more often than not: START and STOP together is left off.
const SEMI_GIVE_UP: u32 = 20;
/// Packets a break leaves free for moving, of the server's 1250 in 4 s.
const MOVE_RESERVE: u32 = 150;
/// Ticks at a stand with nothing broken before it hovers as before, and for how long.
const STAND_IDLE: u32 = 40;
const HOVER_TICKS: u64 = 100;
/// Out of fill, blocks that give it are mined this far off (blocks, any y, in the box) - this many
/// found at a look, nearest first.
const FILL_RADIUS: i32 = 32;
const FILL_FOUND: usize = 32;
/// Flying to one this long and not in reach: it is given up.
const FILL_GOAL_TICKS: u64 = 200;
/// None close by: looked for again after this long.
const FILL_LOOK_TICKS: u64 = 200;

/// One bot's digging state.
#[derive(Default)]
pub struct BotDig {
    /// A slow block being broken: where, the tick its STOP goes, and whether its water is sealed
    /// straight after (a waterlogged block, kelp).
    mining: Option<(BlockPos, u64, bool)>,
    /// The block in the server's delayed-destroy slot, and the tick it is given up on.
    delayed: Option<(BlockPos, u64)>,
    /// START and STOP together sent: where, when (looked at again SEMI_CHECK later), and whether
    /// the bot stood on a block then.
    semi_sent: HashMap<BlockPos, (u64, bool)>,
    /// Blocks START and STOP together did not break: the slow way for them.
    semi_failed: HashSet<BlockPos>,
    /// START and STOP together taken and refused, and left off - hovering [0], standing [1]. The
    /// server mines at a fifth of the speed for a player it has in the air, and the bot's word that
    /// it stands is not always taken (6 Nov: half refused - the client's FastNuker has
    /// "prefer-ground" for the same).
    semi_ok: [u32; 2],
    semi_refused: [u32; 2],
    semi_off: [bool; 2],
    /// Blocks the delayed slot broke, and ones it was given up on; STOPs sent for mined ones.
    delayed_ok: u32,
    delayed_lost: u32,
    mined: u32,
    /// Until this tick it hovers as before, its own footing broken too: no stand reached anything
    /// left, or nothing broke from the one it was on.
    footing_ok_at: u64,
    /// Ticks at a stand with nothing broken.
    idle_at_stand: u32,
    /// Breaks and placements sent, and when they may be tried again.
    fired: HashMap<BlockPos, u64>,
    /// How often each was sent in this band (REFUSED_TRIES).
    tries: HashMap<BlockPos, u32>,
    /// Seals placed from the hotbar (the totem holding the offhand), counted apart: one that never
    /// takes is not the server guarding the place, so it never counts toward leaving it standing.
    hand_tries: HashMap<BlockPos, u32>,
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
    /// Out of fill: mining some close by (fill_step) - the blocks found for it, the one it is
    /// flying to and the tick it is given up on, the ones given up, and the next look for more.
    fill_run: bool,
    fill_targets: Vec<BlockPos>,
    fill_goal: Option<(BlockPos, u64)>,
    fill_skip: HashSet<BlockPos>,
    fill_look_at: u64,
    pub cell: Option<(i32, i32)>,
    pub status: String,
    pub broken: u64,
    pub placed: u64,
    /// When the guard logged it out (guard.rs).
    pub logged_out_at: Option<std::time::Instant>,
    /// When it last looked for its bed (base::bed_check: once a join).
    pub bed_checked_at: Option<std::time::Instant>,
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
/// the server will not let go (`left`), nor to a block holding back fluid there (walls_left_fluid).
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
                if !holds_fluid(qs) {
                    continue;
                }
                // Water where the server will not let a block go in: this block is the wall.
                if left.has(q) {
                    return Vec::new();
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

/// `p` holds back water or lava where the server will not let a block go in (`left`): it stays,
/// the wall against it. Broken, the fluid flowed in, was sealed, and the seal broken again - ten
/// times round, and the row was "left standing" too (6 Nov: the north side, eighteen rows of it
/// one after another).
fn walls_left_fluid(w: &World, p: BlockPos, left: &Refused) -> bool {
    FLOWS_IN_FROM.iter().any(|(dx, dy, dz)| {
        let q = BlockPos::new(p.x + dx, p.y + dy, p.z + dz);
        left.has(q) && w.get_block_state(q).is_some_and(holds_fluid)
    })
}

/// `p` is something to clear: not clear already, not left, not a wall against fluid that is.
/// None while it is not loaded.
fn to_go(w: &World, p: BlockPos, protect: bool, left: &Refused) -> Option<bool> {
    let st = w.get_block_state(p)?;
    Some(match what(st, protect) {
        What::Clear => false,
        _ if left.has(p) => false,
        What::Block => !walls_left_fluid(w, p, left),
        What::Fluid => true,
    })
}

/// Everything in `b` that has to go (to_go). None while part of it is not loaded.
fn scan(w: &World, b: &Box3, protect: bool, left: &Refused) -> Option<Vec<BlockPos>> {
    let mut out = Vec::new();
    for y in (b.min.y..=b.max.y).rev() {
        for x in b.min.x..=b.max.x {
            for z in b.min.z..=b.max.z {
                let p = BlockPos::new(x, y, z);
                if to_go(w, p, protect, left)? {
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
                if to_go(w, p, protect, left)? {
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
    settle_breaks(&mut st, ctx, p, &world.read(), job.protect_containers, ticks);

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
        st.mined += 1;
        st.broken += 1;
        let sealed = seal
            && match base::fill_ready(bot, &job) {
                base::Fill::Ready(from) => {
                    place(bot, b, from);
                    st.placed += 1;
                    true
                }
                _ => false,
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
    // A shovel or an axe short (worn out, or none in the chest last time), or tools carried it does
    // not need (one of each is kept): a tools trip now, and again every KIT_RECHECK while it stays
    // so (4 Nov: a bot dug grass and dirt by hand, 16 ticks a block, for hours - a trip only ever
    // went for a missing pickaxe).
    if ticks >= st.kit_check_at
        && link.joined_at.lock().is_some_and(|t| t.elapsed() >= JOIN_SETTLE)
        && let Some(why) = base::kit_trip(bot, job.retire_uses)
    {
        st.kit_check_at = ticks + base::KIT_RECHECK.as_secs() * 20;
        if st.cell.take().is_some() {
            p.board.lock().release(&link.name);
        }
        st.status = format!("{why} - fetching tools");
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
            st.hand_tries.clear();
            st.semi_failed.clear();
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

    // Out of fill and mining some: what it carries and what lies about, read before the world is.
    let fill_in = st.fill_run.then(|| FillIn {
        carried: base::fill_carried(bot, job),
        lying: base::fill_lying_near(bot, job, 24.0),
        room: inv::empty_slots(bot) > 0,
        junk: base::dug_junk_slot(bot, job),
    });
    if let Some(f) = &fill_in
        && !f.room
        && ticks.is_multiple_of(10)
        && let Some(slot) = f.junk
    {
        base::throw(bot, slot);
    }

    // Break and seal what is in reach, the top layer first.
    let w = world.read();
    let standing = movement::geom::supported(&w, pos);
    let mut fill_reach = None;
    let mut fill_land = None;
    if let Some(f) = fill_in {
        match fill_step(st, job, &w, &area, protect, &left, pos, standing, ticks, f) {
            FillStep::Done => {}
            FillStep::Mine(reach, land) => {
                fill_reach = Some(reach);
                fill_land = land;
            }
            FillStep::Fly(to, arrive) => {
                drop(w);
                movement::steer(bot, to, "perimeter-fill", arrive);
                return Want::Moving;
            }
        }
    }
    let filling = fill_reach.is_some();
    // Its footing goes last: nothing under its feet is broken while there is other work in reach
    // of a stand (the client's prefer-ground).
    // Hovering a while (footing_ok_at): no stand reached anything, or nothing broke from one.
    let hover = ticks < st.footing_ok_at;
    let footing = if standing && !hover { feet_columns(pos) } else { Vec::new() };
    let feet_y = (pos.y + 0.01).floor() as i32;
    let mut breaks = 0;
    // Breaks that take at once (counted now), and up to two slow blocks for after them.
    let mut fast = 0;
    let mut slow: Vec<(BlockPos, bool, f32)> = Vec::new();
    let mut placed: Vec<BlockPos> = Vec::new();
    let mut need_fill = false;
    let mut hand_stuck = None;
    let mut this_tick: HashSet<BlockPos> = HashSet::new();
    // In reach, the top layer first. (6 Nov: the first 48 within 6.5 of the eye - standing down in
    // a cut those were all top-layer blocks just out of reach, and both bots stood still.)
    // Then a few just past it, for fluid beside them that is in reach. Mining fill: what gives it
    // in reach instead.
    let near: Vec<BlockPos> = match fill_reach {
        Some(reach) => reach,
        None => {
            let mut near: Vec<BlockPos> = st.queue.iter().filter(|q| block_dist(eye, **q) <= REACH).take(48).copied().collect();
            near.extend(st.queue.iter().filter(|q| (REACH..=REACH + 1.0).contains(&block_dist(eye, **q))).take(16).copied());
            near
        }
    };
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
                    let from = match base::fill_ready(bot, job) {
                        base::Fill::Ready(from) => from,
                        base::Fill::Swapping => {
                            st.tool_ready_at = ticks + 2;
                            break 'work;
                        }
                        base::Fill::Out => {
                            need_fill = true;
                            continue;
                        }
                    };
                    if from.is_some() && st.hand_tries.get(&f).is_some_and(|n| *n >= REFUSED_TRIES) {
                        hand_stuck = Some(f);
                        continue;
                    }
                    place(bot, f, from);
                    debug!(target: "perimeter", "seal {} {} {}{}", f.x, f.y, f.z, if from.is_some() { " (from the hotbar)" } else { "" });
                    st.fired.insert(f, ticks + RETRY_TICKS);
                    *if from.is_some() { st.hand_tries.entry(f) } else { st.tries.entry(f) }.or_default() += 1;
                    placed.push(f);
                }
                Act::Break { pos: b, seal } => {
                    if b.y < feet_y && footing.contains(&(b.x, b.z)) {
                        continue;
                    }
                    if breaks >= max_breaks || st.fired.contains_key(&b) || block_dist(eye, b) > REACH || !this_tick.insert(b) {
                        continue;
                    }
                    let Some(state) = w.get_block_state(b) else { continue };
                    let (slot, per_tick) = nuker::best_tool_keeping(bot, state, job.retire_uses + 1);
                    if per_tick <= 0.0 {
                        st.fired.insert(b, ticks + RETRY_TICKS * 30);
                        continue;
                    }
                    // Off a block, the server mines at a fifth of the speed, whatever the bot says
                    // (6 Nov: START+STOP together taken 70% standing, 30% hovering): STOPs timed
                    // for standing fell short, and only the delayed slot ever finished them.
                    let per_tick = if standing { per_tick } else { per_tick * 0.2 };
                    if refused(st, ctx, p, b) {
                        continue;
                    }
                    // A tool to take in hand first: the break waits for the swap to land - unless a
                    // slow block is set to go with the one in hand: it keeps it (the delayed slot's
                    // progress is counted with whatever is in hand).
                    if let Some(slot) = slot
                        && bot.selected_hotbar_slot().map(|h| h as usize + 36).ok() != Some(slot)
                    {
                        if !slow.is_empty() {
                            continue;
                        }
                        inv::hold(bot, slot, &super::guard::weapon_hotbar(bot));
                        st.tool_ready_at = ticks + 3;
                        break 'work;
                    }
                    let semi = per_tick < 1.0 && per_tick >= SEMI && !st.semi_off[standing as usize] && !st.semi_failed.contains(&b);
                    if per_tick >= 1.0 || semi {
                        if !movement::packet_room(bot, 2 + MOVE_RESERVE) {
                            break 'work;
                        }
                        harvest::send_break(bot, b);
                        if semi {
                            st.semi_sent.insert(b, (ticks, standing));
                        }
                        st.fired.insert(b, ticks + RETRY_TICKS);
                        *st.tries.entry(b).or_default() += 1;
                        breaks += 1;
                        fast += 1;
                        // Its water gets a block in the same tick, before it can flow.
                        if seal
                            && placed.len() < max_places
                            && let base::Fill::Ready(from) = base::fill_ready(bot, job)
                        {
                            place(bot, b, from);
                            placed.push(b);
                        }
                    } else if slow.len() < 2 {
                        slow.push((b, seal, per_tick));
                    }
                }
            }
        }
    }
    // Slow blocks, two at once, after this tick's other breaks. Into the delayed slot: one with no
    // water to seal (the server breaks it at a tick nobody sees). Mined here: the other - while
    // nothing that breaks at once waits for room this tick, as nothing is broken until its STOP.
    if !slow.is_empty() && movement::packet_room(bot, 3 + MOVE_RESERVE) {
        if st.delayed.is_none()
            && let Some(k) = slow.iter().position(|(_, seal, _)| !seal)
        {
            let (b, _, per_tick) = slow.remove(k);
            harvest::send_break(bot, b);
            let natural = (1.0 / per_tick as f64).ceil() as u64;
            let until = ticks + natural * 2 + 60;
            debug!(target: "perimeter", "DELAYED {} {} {} perTick {per_tick:.3} -> the server breaks it in {natural} ticks", b.x, b.y, b.z);
            st.delayed = Some((b, until));
            st.fired.insert(b, until);
            *st.tries.entry(b).or_default() += 1;
            breaks += 1;
        }
        if breaks < max_breaks
            && let Some((b, seal, per_tick)) = slow.first().copied()
        {
            // START now, STOP once the progress passes stop-progress.
            harvest::send_action(bot, b, true);
            let wait = (job.stop_progress.clamp(0.7, 1.0) / per_tick as f64).ceil() as u64;
            debug!(target: "perimeter", "START {} {} {} perTick {per_tick:.3} -> STOP in {wait} ticks", b.x, b.y, b.z);
            st.mining = Some((b, ticks + wait, seal));
            st.fired.insert(b, ticks + wait + RETRY_TICKS);
            *st.tries.entry(b).or_default() += 1;
            breaks += 1;
        }
    }
    // Where to be, when nothing was done here this tick: standing on a block that reaches what is
    // left (stand_spot); hovering over the highest thing left only when no stand reaches any of it,
    // or it is lava (a little higher over that). Everything left waiting on the server: it stays.
    let next = st.queue.iter().find(|q| !st.fired.contains_key(*q)).or(st.queue.first()).copied();
    let lava = next.is_some_and(|n| w.get_block_state(n).is_some_and(is_lava));
    let work: Vec<BlockPos> = st.queue.iter().filter(|q| !st.fired.contains_key(*q)).copied().collect();
    let idle = breaks == 0 && placed.is_empty();
    let stand = if idle && !filling && !hover && !lava && !work.is_empty() { stand_spot(&w, &cell, lo, floor, &work, pos) } else { None };
    let spot = next.map(|n| {
        let lift = if lava { 2.0 } else { 1.0 };
        let s = Vec3::new(n.x as f64 + 0.5, n.y as f64 + lift, n.z as f64 + 0.5);
        if movement::geom::stand_clear(&w, s) { s } else { movement::geom::nearest_clear(&w, s, 3).unwrap_or(s) }
    });
    drop(w);

    let instant_breaks = fast;
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
    if filling {
        if let Some(s) = fill_land {
            movement::steer(bot, s, "perimeter-fill", 0.3);
        }
        st.status = format!("cell {} {}: out of fill - mining some ({} carried)", c.0, c.1, base::fill_carried(bot, job));
        return Want::Moving;
    }

    // Sealing to do and no fill: pick some up, mine some close by, make room for it, or leave the
    // cell for a while.
    // Seals from the hotbar that never take (the server may only take them from the offhand, where
    // the totem stays): the cell is left a while, as for no fill, and nothing is left standing.
    if let Some(f) = hand_stuck {
        p.board.lock().park(c, PARK_SECS);
        st.cell = None;
        st.queue.clear();
        st.hand_tries.clear();
        ctx.warn_once(&format!(
            "perimeter: water or lava at {} {} {} will not take a block from the hotbar ({REFUSED_TRIES} tries; the totem keeps the offhand) - cell {} {} is left for {PARK_SECS} s",
            f.x, f.y, f.z, c.0, c.1
        ));
        st.status = format!("cell {} {}: seals from the hotbar not taking - left for now", c.0, c.1);
        return Want::Nothing;
    }
    if need_fill && !acted {
        st.no_fill_ticks += 1;
        if let Some(item) = base::fill_lying_near(bot, job, 24.0) {
            movement::steer(bot, item + Vec3::new(0.0, 0.2, 0.0), "perimeter-fill", 0.5);
            st.status = format!("cell {} {}: picking up fill", c.0, c.1);
            return Want::Moving;
        }
        // 6 Nov: both bots hovered for hours at sea level over cells of water, the nearest cell
        // after each one put off was water too, and nothing dug brought fill in.
        if ticks >= st.fill_look_at {
            st.fill_run = true;
            st.fill_targets.clear();
            st.fill_goal = None;
            st.fill_skip.clear();
            info!(target: "perimeter", "out of fill with water or lava to seal - mining some close by");
            st.status = format!("cell {} {}: out of fill - looking for some to mine", c.0, c.1);
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
            ctx.warn_once(&format!(
                "perimeter: no fill to seal the water or lava in cell {} {}, and none to mine within {FILL_RADIUS} blocks - it is left for {PARK_SECS} s",
                c.0, c.1
            ));
            st.status = format!("cell {} {}: out of fill - left for now", c.0, c.1);
            return Want::Nothing;
        }
    } else if acted {
        st.no_fill_ticks = 0;
    }

    st.status = format!("cell {} {}: y {lo}..{} ({} left{})", c.0, c.1, floor - 1, st.queue.len(), if need_fill { ", needs fill" } else { "" });
    if acted {
        st.idle_at_stand = 0;
    }
    if !acted && !work.is_empty() {
        match stand {
            Some(s) if pos.distance_to(s) > 0.2 => {
                st.idle_at_stand = 0;
                movement::steer(bot, s, "perimeter", 0.1);
            }
            Some(_) => {
                // At the stand and nothing breaks from it (fluid to seal out of reach, say): a while
                // hovering, as before there were stands.
                st.idle_at_stand += 1;
                if st.idle_at_stand > STAND_IDLE {
                    debug!(target: "perimeter", "nothing breaks from the stand at {:.1} {:.1} {:.1} - hovering a while", pos.x, pos.y, pos.z);
                    st.idle_at_stand = 0;
                    st.footing_ok_at = ticks + HOVER_TICKS;
                }
            }
            None => {
                // No stand reaches what is left (or hovering a while): its footing may go too.
                if !hover {
                    st.footing_ok_at = ticks + 40;
                }
                if let Some(s) = spot
                    && eye.distance_to(s + Vec3::new(0.0, 0.5, 0.0)) > 1.0
                {
                    movement::steer(bot, s, "perimeter", 0.5);
                }
            }
        }
    }
    Want::Moving
}

/// Where to stand cutting the band `lo..floor` of `cell`: on a block, as the server mines at a
/// fifth of the speed for a player off one (6 Nov). A column's stand is on its highest block - the
/// band's top, until the band is cut through there, then the block under the band, which nothing
/// breaks this band (the user: "it stands on the 4th block and clears outwards"). The stand
/// reaching the most of `work` wins - one under the band counting double - then the nearest to
/// `from`. None when no stand reaches any of it.
pub fn stand_spot(w: &World, cell: &Box3, lo: i32, floor: i32, work: &[BlockPos], from: Vec3) -> Option<Vec3> {
    let mut best: Option<(usize, f64, Vec3)> = None;
    for x in cell.min.x..=cell.max.x {
        for z in cell.min.z..=cell.max.z {
            let Some(feet) = column_stand(w, x, z, lo, floor) else { continue };
            let eye = feet + Vec3::new(0.0, 1.62, 0.0);
            // Its own column under it is not what it can break from there.
            let reach = work.iter().filter(|q| (q.x, q.z) != (x, z) && block_dist(eye, **q) <= REACH).count();
            if reach == 0 {
                continue;
            }
            let score = if feet.y as i32 == lo { reach * 2 } else { reach };
            let d = feet.distance_to(from);
            if best.is_none_or(|(s, bd, _)| score > s || (score == s && d < bd)) {
                best = Some((score, d, feet));
            }
        }
    }
    best.map(|(_, _, feet)| feet)
}

/// The stand on column x z between the band's top (feet at `floor`) and the block under the band
/// (feet at `lo`): on its highest block, out of any water or lava.
fn column_stand(w: &World, x: i32, z: i32, lo: i32, floor: i32) -> Option<Vec3> {
    for y in (lo..=floor).rev() {
        let feet = Vec3::new(x as f64 + 0.5, y as f64, z as f64 + 0.5);
        if !movement::geom::stand_clear(w, feet) {
            return None;
        }
        let wet = |p: BlockPos| w.get_block_state(p).is_some_and(|s| what(s, false) == What::Fluid);
        if wet(BlockPos::new(x, y, z)) || wet(BlockPos::new(x, y + 1, z)) {
            return None;
        }
        if movement::geom::supported(w, feet) {
            return Some(feet);
        }
    }
    None
}

/// The columns the bot's box stands over (one to four).
fn feet_columns(feet: Vec3) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for dx in [-0.3, 0.3] {
        for dz in [-0.3, 0.3] {
            let c = ((feet.x + dx).floor() as i32, (feet.z + dz).floor() as i32);
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

// ------------------------------------------------------------------ mining fill

/// Out of fill, what it carries and what lies about (read before the world).
struct FillIn {
    carried: usize,
    lying: Option<Vec3>,
    /// An empty bag slot.
    room: bool,
    /// A stack dug that is not fill, to throw out for room.
    junk: Option<usize>,
}

enum FillStep {
    /// The run is over: enough fill carried, no room for more, or none close by to mine.
    Done,
    /// Mine these, in reach (none: wait a tick) - landing here meanwhile, off the ground.
    Mine(Vec<BlockPos>, Option<Vec3>),
    /// Fly here, arriving within this.
    Fly(Vec3, f64),
}

/// A tick of a run mining fill (6 Nov, the user: "when it doesnt have blocks to fill it can mine
/// blocks nearby, any y level but still inside the permiter it is clearing"): what gives it in
/// reach, else what was mined picked up, else on to the nearest - until it carries keep-fill-stacks
/// of it, or has no room for more.
#[allow(clippy::too_many_arguments)]
fn fill_step(st: &mut BotDig, job: &JobConfig, w: &World, area: &Box3, protect: bool, left: &Refused, pos: Vec3, standing: bool, ticks: u64, f: FillIn) -> FillStep {
    let enough = job.keep_fill_stacks.max(1) * 64;
    let full = !f.room && f.junk.is_none();
    if f.carried >= enough || full {
        st.fill_run = false;
        st.fill_goal = None;
        info!(target: "perimeter", "{} fill in the bags{} - back to sealing", f.carried, if f.carried < enough { " (no room for more)" } else { "" });
        return FillStep::Done;
    }
    let eye = pos + Vec3::new(0.0, 1.62, 0.0);
    let feet = feet_columns(pos);
    let feet_y = (pos.y + 0.01).floor() as i32;
    // What it stands on goes last: it is mined from beside.
    let underfoot = |b: &BlockPos| standing && b.y < feet_y && feet.contains(&(b.x, b.z));
    let skip = &st.fill_skip;
    st.fill_targets.retain(|b| !skip.contains(b) && gives_fill(w, *b, job, protect, left));
    if st.fill_targets.is_empty() {
        if ticks < st.fill_look_at {
            return FillStep::Mine(Vec::new(), None);
        }
        st.fill_targets = find_fill(w, job, area, protect, left, eye, &st.fill_skip);
        st.fill_look_at = ticks + 20;
        if st.fill_targets.is_empty() {
            st.fill_run = false;
            st.fill_goal = None;
            st.fill_look_at = ticks + FILL_LOOK_TICKS;
            info!(target: "perimeter", "nothing that gives fill within {FILL_RADIUS} blocks ({} carried)", f.carried);
            return FillStep::Done;
        }
    }
    let reach: Vec<BlockPos> = st.fill_targets.iter().filter(|b| block_dist(eye, **b) <= REACH && !underfoot(b)).copied().collect();
    if !reach.is_empty() {
        st.fill_goal = None;
        // In reach on the way there: it mines, and lands meanwhile (a fifth of the speed off the
        // ground - 6 Nov, the first runs: 307 fill in 11 minutes, almost all of it hovering).
        let land = (!standing)
            .then(|| reach.iter().min_by(|a, b| eye.distance_to(centre(**a)).total_cmp(&eye.distance_to(centre(**b)))))
            .flatten()
            .map(|b| fill_stand(w, *b));
        return FillStep::Mine(reach, land);
    }
    if let Some(item) = f.lying {
        st.status = format!("out of fill - picking up what was mined ({} carried)", f.carried);
        return FillStep::Fly(item + Vec3::new(0.0, 0.2, 0.0), 0.5);
    }
    let Some(t) = st.fill_targets.iter().min_by(|a, b| underfoot(a).cmp(&underfoot(b)).then(eye.distance_to(centre(**a)).total_cmp(&eye.distance_to(centre(**b))))).copied() else {
        return FillStep::Mine(Vec::new(), None);
    };
    match st.fill_goal {
        Some((g, until)) if g == t => {
            if ticks > until {
                debug!(target: "perimeter", "fill at {} {} {} not reached - given up", t.x, t.y, t.z);
                st.fill_skip.insert(t);
                st.fill_targets.retain(|b| *b != t);
                st.fill_goal = None;
                return FillStep::Mine(Vec::new(), None);
            }
        }
        _ => st.fill_goal = Some((t, ticks + FILL_GOAL_TICKS)),
    }
    st.status = format!("out of fill - on to mine some ({} carried)", f.carried);
    FillStep::Fly(fill_stand(w, t), 0.3)
}

/// The item a block drops to the bots' tools (no silk touch): its own, but for these.
fn fill_drop(kind: BlockKind) -> &'static str {
    use BlockKind as K;
    let s = match kind {
        K::Stone => "cobblestone",
        K::Deepslate => "cobbled_deepslate",
        K::GrassBlock | K::Podzol | K::Mycelium | K::DirtPath | K::Farmland => "dirt",
        k => k.to_str(),
    };
    s.strip_prefix("minecraft:").unwrap_or(s)
}

/// `b` is mined for fill: it drops fill, it has open air on a side to be mined from, and it breaks
/// with nothing to seal first (no water or lava beside or above it, none in it).
fn gives_fill(w: &World, b: BlockPos, job: &JobConfig, protect: bool, left: &Refused) -> bool {
    let Some(state) = w.get_block_state(b) else { return false };
    if !job.is_fill(fill_drop(BlockKind::from(state))) {
        return false;
    }
    let open = [(0, 1, 0), (0, -1, 0), (1, 0, 0), (-1, 0, 0), (0, 0, 1), (0, 0, -1)]
        .iter()
        .any(|(dx, dy, dz)| w.get_block_state(BlockPos::new(b.x + dx, b.y + dy, b.z + dz)).is_some_and(|s| s.is_air()));
    open && plan(w, b, protect, left) == [Act::Break { pos: b, seal: false }]
}

/// What gives fill (gives_fill) in the box within FILL_RADIUS of `eye`, any y - nearest first, at
/// most FILL_FOUND of them, none in `skip`.
fn find_fill(w: &World, job: &JobConfig, area: &Box3, protect: bool, left: &Refused, eye: Vec3, skip: &HashSet<BlockPos>) -> Vec<BlockPos> {
    static OFFSETS: OnceLock<Vec<(i32, i32, i32)>> = OnceLock::new();
    let offsets = OFFSETS.get_or_init(|| {
        let r = FILL_RADIUS;
        let mut v = Vec::new();
        for dx in -r..=r {
            for dy in -r..=r {
                for dz in -r..=r {
                    if dx * dx + dy * dy + dz * dz <= r * r {
                        v.push((dx, dy, dz));
                    }
                }
            }
        }
        v.sort_by_key(|(x, y, z)| x * x + y * y + z * z);
        v
    });
    let at = BlockPos::new(eye.x.floor() as i32, eye.y.floor() as i32, eye.z.floor() as i32);
    let mut out = Vec::new();
    for (dx, dy, dz) in offsets {
        let b = BlockPos::new(at.x + dx, at.y + dy, at.z + dz);
        let inside = (area.min.x..=area.max.x).contains(&b.x) && (area.min.y..=area.max.y).contains(&b.y) && (area.min.z..=area.max.z).contains(&b.z);
        if inside && !skip.contains(&b) && gives_fill(w, b, job, protect, left) {
            out.push(b);
            if out.len() >= FILL_FOUND {
                break;
            }
        }
    }
    out
}

/// Where to be to mine `b` for fill: beside it in the open and out of water - on the ground there
/// if it can (the server mines at a fifth of the speed off it) - or under it; never on it.
fn fill_stand(w: &World, b: BlockPos) -> Vec3 {
    let at = |x: i32, y: i32, z: i32| Vec3::new(x as f64 + 0.5, y as f64, z as f64 + 0.5);
    let wet = |p: BlockPos| w.get_block_state(p).is_some_and(|s| what(s, false) == What::Fluid);
    let mut spots = Vec::new();
    for dy in [1, 0, -1] {
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            spots.push(BlockPos::new(b.x + dx, b.y + dy, b.z + dz));
        }
    }
    spots.push(BlockPos::new(b.x, b.y - 2, b.z));
    let clear: Vec<Vec3> = spots
        .into_iter()
        .filter(|s| !wet(*s) && !wet(BlockPos::new(s.x, s.y + 1, s.z)))
        .map(|s| at(s.x, s.y, s.z))
        .filter(|s| movement::geom::stand_clear(w, *s))
        .collect();
    let above = at(b.x, b.y + 1, b.z);
    clear
        .iter()
        .find(|s| movement::geom::supported(w, **s))
        .or(clear.first())
        .copied()
        .or_else(|| movement::geom::nearest_clear(w, above, 3))
        .unwrap_or(above)
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

/// What became of the breaks the server answers in its own time: the block in the delayed slot
/// (broken, or given up on), and each START and STOP together (still there: the slow way for that
/// block - and with too many refused, START and STOP together is left off).
fn settle_breaks(st: &mut BotDig, ctx: &SwarmCtx, p: &Perimeter, w: &World, protect: bool, ticks: u64) {
    if let Some((b, until)) = st.delayed {
        if w.get_block_state(b).is_some_and(|s| what(s, protect) != What::Block) {
            debug!(target: "perimeter", "delayed {} {} {} broken", b.x, b.y, b.z);
            st.delayed = None;
            st.delayed_ok += 1;
            st.broken += 1;
            p.board.lock().broken += 1;
        } else if ticks > until {
            debug!(target: "perimeter", "delayed {} {} {} still there - given up", b.x, b.y, b.z);
            st.delayed = None;
            st.delayed_lost += 1;
        }
    }
    if ticks.is_multiple_of(1200) && st.semi_ok.iter().chain(&st.semi_refused).sum::<u32>() + st.delayed_ok + st.delayed_lost + st.mined > 0 {
        let off = |k: usize| if st.semi_off[k] { " (left off)" } else { "" };
        info!(
            target: "perimeter",
            "breaks so far: START+STOP together standing {} taken, {} refused{}, hovering {} taken, {} refused{}; delayed slot {} broken, {} given up; {} mined and STOPped",
            st.semi_ok[1],
            st.semi_refused[1],
            off(1),
            st.semi_ok[0],
            st.semi_refused[0],
            off(0),
            st.delayed_ok,
            st.delayed_lost,
            st.mined
        );
    }
    let due: Vec<(BlockPos, bool)> = st.semi_sent.iter().filter(|(_, (t, _))| ticks >= *t + SEMI_CHECK).map(|(b, (_, s))| (*b, *s)).collect();
    for (b, standing) in due {
        st.semi_sent.remove(&b);
        let k = standing as usize;
        match w.get_block_state(b).map(|s| what(s, protect)) {
            Some(What::Block) => {
                debug!(target: "perimeter", "START+STOP {} {} {} refused ({}) - the slow way next", b.x, b.y, b.z, if standing { "standing" } else { "hovering" });
                st.semi_failed.insert(b);
                st.semi_refused[k] += 1;
            }
            Some(_) => st.semi_ok[k] += 1,
            None => {}
        }
        if !st.semi_off[k] && st.semi_refused[k] >= SEMI_GIVE_UP && st.semi_refused[k] > st.semi_ok[k] {
            st.semi_off[k] = true;
            let how = if standing { "standing on a block" } else { "hovering" };
            info!(target: "perimeter", "START+STOP together {how}: refused {} times, taken {} - left off {how}", st.semi_refused[k], st.semi_ok[k]);
            ctx.warn_once(&format!(
                "perimeter: the server refused {} of {} START+STOP breaks made {how} - those go START, wait, STOP now",
                st.semi_refused[k],
                st.semi_refused[k] + st.semi_ok[k]
            ));
        }
    }
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
/// Fill into `f`: from the offhand, or from hotbar slot `from` (the totem keeps the offhand).
fn place(bot: &Client, f: BlockPos, from: Option<u8>) {
    let hit = Vec3::new(f.x as f64 + 0.5, f.y as f64 + 1.0, f.z as f64 + 0.5);
    let choice = Choice { click: Click { clicked: f, face: Dir::Up, hit, yaw: 0.0, pitch: 90.0, sneak: false }, needs_look: false };
    match from {
        None => act::click(bot, &choice),
        Some(h) => act::click_from_hotbar(bot, h, &choice),
    }
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
        let (w, _keep) = world(&[(at(0, 10, 0), STONE), (at(1, 10, 0), STONE), (at(1, 10, 1), WATER), (at(5, 12, 5), STONE), (at(3, 9, 3), STONE)]);
        let mut left = Refused::default();
        left.blocks.insert((0, 10, 0));
        assert!(plan(&w, at(0, 10, 0), false, &left).is_empty());
        assert_eq!(plan(&w, at(1, 10, 0), false, &left), vec![Act::Seal(at(1, 10, 1))]);
        // Water in a guarded spot gets no block: the stone holding it back stays - broken, the
        // water flowed in, was sealed, and the seal broken again, round and round.
        left.blocks.insert((1, 10, 1));
        assert!(plan(&w, at(1, 10, 0), false, &left).is_empty());
        left.xs.insert(5);
        let b = Box3 { min: at(0, 0, 0), max: at(7, 15, 7) };
        assert_eq!(scan(&w, &b, false, &left), Some(vec![at(3, 9, 3)]), "the wall is not left to cut");
        let mut budget = 10_000;
        assert_eq!(highest_material(&w, &b, 15, 0, false, &left, &mut budget), Some(Some(9)));
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
    fn it_stands_on_the_band_then_under_it_never_in_the_air() {
        // A cell 0..7 x 0..7, its band y 61..63 (floor 64, lo 61), stone under it at y 60.
        let cell = Box3 { min: BlockPos::new(0, 61, 0), max: BlockPos::new(7, 63, 7) };
        let mut blocks: Vec<(BlockPos, &str)> = Vec::new();
        for x in 0..8 {
            for z in 0..8 {
                for y in 60..64 {
                    blocks.push((BlockPos::new(x, y, z), STONE));
                }
            }
        }
        let (w, _keep) = world(&blocks);
        let work: Vec<BlockPos> = blocks.iter().map(|(p, _)| *p).filter(|p| p.y >= 61).collect();
        let from = Vec3::new(3.5, 64.0, 3.5);
        let s = stand_spot(&w, &cell, 61, 64, &work, from).expect("a stand on the band's top");
        assert_eq!(s.y, 64.0, "on the top layer, not hovering");
        // Column 3 3 cut through the band: down in it, on the block under the band.
        for y in 61..64 {
            w.set_block_state(BlockPos::new(3, y, 3), parse_state("minecraft:air").unwrap());
        }
        let work: Vec<BlockPos> = work.into_iter().filter(|p| (p.x, p.z) != (3, 3)).collect();
        let s = stand_spot(&w, &cell, 61, 64, &work, from).expect("a stand in the cut");
        assert_eq!((s.x, s.y, s.z), (3.5, 61.0, 3.5));
        // A cave under the cut: nothing to stand on down there - back on the top.
        w.set_block_state(BlockPos::new(3, 60, 3), parse_state("minecraft:air").unwrap());
        let s = stand_spot(&w, &cell, 61, 64, &work, from).expect("a stand on the top");
        assert_eq!(s.y, 64.0);
        // Water in the cut: not stood in (the server mines slower under water too).
        let wet = [(BlockPos::new(3, 60, 3), STONE), (BlockPos::new(3, 61, 3), WATER)];
        for (p, s) in wet {
            w.set_block_state(p, parse_state(s).unwrap());
        }
        assert_eq!(column_stand(&w, 3, 3, 61, 64), None);
        // Nothing left in reach of any stand: none.
        assert_eq!(stand_spot(&w, &cell, 61, 64, &[BlockPos::new(30, 61, 30)], from), None);
    }

    #[test]
    fn out_of_fill_it_mines_what_gives_some_nearest_first() {
        let job = JobConfig::default();
        let area = Box3 { min: at(-20, -64, -20), max: at(20, 100, 20) };
        let (w, _keep) = world(&[
            (at(2, 10, 0), STONE),
            (at(0, 10, 3), "minecraft:grass_block[snowy=false]"),
            (at(1, 10, 1), "minecraft:sand"),
            (at(-4, 10, 0), STONE),
            (at(-5, 10, 0), WATER),
            (at(30, 10, 0), STONE),
        ]);
        let eye = Vec3::new(0.5, 11.62, 0.5);
        let found = find_fill(&w, &job, &area, false, &Refused::default(), eye, &HashSet::new());
        // Sand is no fill; stone with water beside it is sealed first, not mined; x 30 is outside.
        assert_eq!(found, vec![at(2, 10, 0), at(0, 10, 3)], "stone gives cobblestone, grass dirt");
        let skip: HashSet<BlockPos> = [at(2, 10, 0)].into();
        assert_eq!(find_fill(&w, &job, &area, false, &Refused::default(), eye, &skip), vec![at(0, 10, 3)]);
        assert_eq!(fill_drop(BlockKind::Deepslate), "cobbled_deepslate");
        assert_eq!(fill_drop(BlockKind::Andesite), "andesite");
    }

    #[test]
    fn fill_buried_in_stone_or_under_water_is_not_mined() {
        let job = JobConfig::default();
        let mut blocks = vec![(at(0, 10, 0), STONE)];
        for (dx, dy, dz) in [(0, 1, 0), (0, -1, 0), (1, 0, 0), (-1, 0, 0), (0, 0, 1), (0, 0, -1)] {
            blocks.push((at(dx, 10 + dy, dz), STONE));
        }
        blocks.push((at(5, 10, 5), STONE));
        blocks.push((at(5, 11, 5), WATER));
        let (w, _keep) = world(&blocks);
        assert!(!gives_fill(&w, at(0, 10, 0), &job, false, &Refused::default()), "no open side to mine it from");
        assert!(gives_fill(&w, at(0, 11, 0), &job, false, &Refused::default()));
        assert!(!gives_fill(&w, at(5, 10, 5), &job, false, &Refused::default()), "water above: sealed first");
    }

    #[test]
    fn fill_is_mined_from_the_ground_beside_it() {
        let mut blocks = Vec::new();
        for x in -2..=2 {
            for z in -2..=2 {
                blocks.push((at(x, 10, z), STONE));
            }
        }
        let (w, _keep) = world(&blocks);
        let s = fill_stand(&w, at(0, 10, 0));
        assert_eq!(s.y, 11.0, "on the ground: {s:?}");
        assert!(movement::geom::supported(&w, s));
        assert_ne!((s.x, s.z), (0.5, 0.5), "not on the block it mines");
        assert!(block_dist(s + Vec3::new(0.0, 1.62, 0.0), at(0, 10, 0)) <= REACH);
    }

    #[test]
    fn its_footing_is_the_columns_under_its_box() {
        assert_eq!(feet_columns(Vec3::new(3.5, 61.0, 3.5)), vec![(3, 3)]);
        assert_eq!(feet_columns(Vec3::new(4.0, 61.0, 3.5)).len(), 2);
        assert_eq!(feet_columns(Vec3::new(4.0, 61.0, 4.0)).len(), 4);
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
