//! Printing a melon build layer by layer, and checking a finished one against the world. Port of
//! AiMelonRoutine.print/check plus the parts of the Printer module the melon builds use.
//!
//! - Cells are placed from the bags with the melon in hand: a solid neighbour in reach is clicked
//!   on its face (the cell below first - bottom-up printing means it is nearly always there),
//!   otherwise the empty cell itself (air-place, Litematica Easy Place's way).
//! - A cell clicked is left alone for a moment (the server's answer takes a tick or three), and a
//!   cell that will not take after 5 clicks is deferred for the rest of the pass.
//! - The layer reached is saved, so a farm trip resumes it. "Done" is only a claim until check()
//!   has flown over the build and counted the cells from the world (12-13 Oct on the Java side:
//!   builds "finished" with the printer standing 250 blocks away all night).

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    block::BlockState,
    core::direction::Direction,
    interact::BlockStatePredictionHandler,
    protocol::packets::game::{
        s_interact::InteractionHand,
        s_use_item_on::{BlockHit, ServerboundUseItemOn},
    },
    registry::builtin::BlockKind,
    world::World,
};
use tracing::{debug, info, warn};

use crate::{bot::Shared, inv, movement, schematic::Plan};

/// Eye to the nearest point of the clicked block, as the server measures it (4.5, less margin).
const REACH: f64 = 4.4;
const PER_STEP: usize = 4;
const PENDING_TICKS: u64 = 10;
const GIVE_UP_CLICKS: u32 = 5;
const STALL: Duration = Duration::from_secs(180);
/// Blocks out from the footprint past which the printer has left the build.
const AWAY: f64 = 80.0;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum End {
    Done,
    Out,
    Stuck,
    Hungry,
    Stopped,
}

/// A cell still waiting for its melon: open - air, water, a plant. Another block there is not ours
/// to clear.
pub fn gap(st: BlockState) -> bool {
    if st.is_air() {
        return true;
    }
    let k = BlockKind::from(st);
    if k == BlockKind::Melon {
        return false;
    }
    matches!(
        k,
        BlockKind::Water
            | BlockKind::ShortGrass
            | BlockKind::TallGrass
            | BlockKind::Fern
            | BlockKind::LargeFern
            | BlockKind::Seagrass
            | BlockKind::TallSeagrass
            | BlockKind::DeadBush
            | BlockKind::Vine
            | BlockKind::Snow
            | BlockKind::Light
            | BlockKind::Fire
            | BlockKind::BubbleColumn
            | BlockKind::Kelp
            | BlockKind::KelpPlant
    )
}

fn solid(st: BlockState) -> bool {
    !gap(st) && !st.is_air()
}

/// Eye to the nearest point of a block's box.
fn block_dist(eye: Vec3, b: BlockPos) -> f64 {
    let cx = eye.x.clamp(b.x as f64, b.x as f64 + 1.0);
    let cy = eye.y.clamp(b.y as f64, b.y as f64 + 1.0);
    let cz = eye.z.clamp(b.z as f64, b.z as f64 + 1.0);
    ((eye.x - cx).powi(2) + (eye.y - cy).powi(2) + (eye.z - cz).powi(2)).sqrt()
}

pub struct PrintResult {
    pub end: End,
    pub placed: i64,
    pub layer: i32,
}

/// Print `plan` at `origin` from `start_layer` (world y; the lowest open layer when 0). `only`: the
/// floor's open cells. `on_layer` is told each new layer reached (saved by the caller).
pub async fn print(
    ctx: &Shared,
    bot: &Client,
    plan: &Plan,
    origin: BlockPos,
    only: Option<&HashSet<BlockPos>>,
    start_layer: i32,
    mut on_layer: impl FnMut(i32),
) -> PrintResult {
    let top = origin.y + plan.layers.keys().last().copied().unwrap_or(0);
    let bottom = origin.y + plan.layers.keys().next().copied().unwrap_or(0);
    let mut layer = if start_layer > 0 { start_layer.max(bottom) } else { bottom };
    let melons_start = melons_all(bot);
    let (x0, z0, x1, z1) = (origin.x, origin.z, origin.x + plan.w - 1, origin.z + plan.d - 1);
    let mut pending: HashMap<BlockPos, u64> = HashMap::new();
    let mut clicks: HashMap<BlockPos, u32> = HashMap::new();
    let mut deferred: HashSet<BlockPos> = HashSet::new();
    let mut step: u64 = 0;
    let mut last_place = Instant::now();
    let mut away_since: Option<Instant> = None;
    let mut melons_seen = melons_start;
    let mut sent = 0u64;
    let mut cells: Vec<BlockPos> = Vec::new();
    let mut cells_layer = i32::MIN;
    // The server's placement limiter (the Java printer's paper-rate-limit): the server allows 9
    // placements in a 300 ms window and Paper DROPS the rest silently - at 80 a second on 15 Oct,
    // the dropped clicks were the cells that "would not take".
    let per_window: usize = ctx.modules.get("printer", "places-per-window", 9);
    let window = Duration::from_millis(ctx.modules.get::<u64>("printer", "place-window-ms", 300) + 20);
    let mut recent: std::collections::VecDeque<Instant> = std::collections::VecDeque::new();
    // The cell the current trip is for, and how many trips it has had.
    let mut chasing: Option<(BlockPos, u32)> = None;
    info!(target: "printer", "printing {} at {} from layer {layer} (top {top})", plan.name, origin);

    let end = loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        step += 1;
        if ctx.quit.load(std::sync::atomic::Ordering::SeqCst) || bot.position().is_err() {
            break End::Stopped;
        }
        if crate::routine::is_dead(bot) {
            break End::Stopped;
        }
        if crate::routine::is_hungry(ctx, bot) {
            break End::Hungry;
        }
        if ctx.autoeat.as_ref().is_some_and(|a| a.eating()) {
            continue;
        }
        let melons = melons_all(bot);
        if melons == 0 {
            break End::Out;
        }
        if melons < melons_seen {
            last_place = Instant::now();
        }
        melons_seen = melons;
        if layer > top {
            break End::Done;
        }
        if cells_layer != layer {
            let rel = layer - origin.y;
            cells = plan
                .layers
                .get(&rel)
                .map(|v| v.iter().map(|(x, y, z)| BlockPos::new(origin.x + x, origin.y + y, origin.z + z)).collect())
                .unwrap_or_default();
            if let Some(only) = only {
                cells.retain(|c| only.contains(c));
            }
            cells_layer = layer;
            deferred.clear();
            debug!(target: "printer", "layer {layer}: {} cells", cells.len());
        }

        let Ok(pos) = bot.position() else { break End::Stopped };
        let eye = pos + Vec3::new(0.0, 1.62, 0.0);
        // Off the build: fly back (the caller's next pass), never "finish" from afar.
        let off_x = (x0 as f64 - pos.x).max(pos.x - (x1 as f64 + 1.0)).max(0.0);
        let off_z = (z0 as f64 - pos.z).max(pos.z - (z1 as f64 + 1.0)).max(0.0);
        if off_x.hypot(off_z) > AWAY {
            let since = *away_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_secs(30) {
                warn!(target: "printer", "{:.0} blocks off {} for 30 s - leaving it for the next pass", off_x.hypot(off_z), plan.name);
                break End::Stuck;
            }
        } else {
            away_since = None;
        }

        // What the world shows of this layer.
        let (missing, unknown) = {
            let Ok(world) = bot.world() else { break End::Stopped };
            let w = world.read();
            let mut missing = Vec::new();
            let mut unknown = Vec::new();
            for c in &cells {
                match w.get_block_state(*c) {
                    None => unknown.push(*c),
                    Some(st) if gap(st) && !deferred.contains(c) => missing.push(*c),
                    _ => {}
                }
            }
            (missing, unknown)
        };
        if missing.is_empty() && unknown.is_empty() {
            debug!(target: "printer", "layer {layer} complete - up one");
            layer += 1;
            on_layer(layer);
            last_place = Instant::now();
            continue;
        }

        // Melons in the OFFHAND: the server drops main-hand clicks on empty air (the Java AirPlace
        // note), and with the offhand every click - neighbour face or air - is the same click. The
        // main hand stays free for the axe and meals.
        let offhand = inv::menu(bot).map(|m| m.slots().get(45).cloned().unwrap_or_default()).unwrap_or_default();
        let in_hand = if inv::stack_id(&offhand) == "melon" { offhand.count() } else { 0 };
        if in_hand < PER_STEP as i32 {
            // Only for a bigger stack: every swap leaves the hand's few behind in the bag, and once
            // the bags held nothing but those, 3 swapped for 3 forever (15 Oct: 8 min, 1,898 swaps,
            // nothing placed). What is left in the hand is placed, the rest of the bag after it.
            let best = inv::bags(bot).into_iter().filter(|(_, s)| inv::stack_id(s) == "melon").max_by_key(|(_, s)| s.count());
            match best {
                Some((slot, s)) if s.count() > in_hand => {
                    inv::swap_with(bot, slot, inv::OFFHAND_BUTTON);
                    debug!(target: "printer", "melons from slot {slot} ({}) into the offhand ({in_hand} there)", s.count());
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    continue;
                }
                _ if in_hand == 0 => break End::Out,
                _ => {}
            }
        }

        // Place what is in reach, nearest first.
        pending.retain(|_, until| *until > step);
        let mut in_reach: Vec<BlockPos> = missing.iter().copied().filter(|c| !pending.contains_key(c) && block_dist(eye, *c) <= REACH).collect();
        in_reach.sort_by(|a, b| block_dist(eye, *a).total_cmp(&block_dist(eye, *b)));
        let mut placed_now = 0;
        if !in_reach.is_empty() {
            let Ok(world) = bot.world() else { break End::Stopped };
            let now = Instant::now();
            while recent.front().is_some_and(|t| now.duration_since(*t) >= window) {
                recent.pop_front();
            }
            // Never more clicks than melons in the hand: a click with none is a "would not take".
            for c in in_reach.into_iter().take(PER_STEP.min(in_hand as usize)) {
                if recent.len() >= per_window {
                    break; // the window is spent: the rest next step
                }
                recent.push_back(Instant::now());
                let hit = {
                    let w = world.read();
                    click_for(&w, eye, c)
                };
                place(bot, hit.clone());
                sent += 1;
                pending.insert(c, step + PENDING_TICKS / 2 + 1);
                let n = clicks.entry(c).or_insert(0);
                *n += 1;
                if *n >= GIVE_UP_CLICKS {
                    let (here, clicked) = {
                        let w = world.read();
                        let name = |p: BlockPos| w.get_block_state(p).map(|s| format!("{s:?}")).unwrap_or_else(|| "unloaded".into());
                        (name(c), name(hit.block_pos))
                    };
                    // An entity standing in the cell blocks the placement (a mob in the dark under an
                    // overhang): name whatever is within a block of it.
                    let centre = Vec3::new(c.x as f64 + 0.5, c.y as f64 + 0.5, c.z as f64 + 0.5);
                    let occupants: Vec<String> = crate::entities::near(bot)
                        .into_iter()
                        .filter(|(_, e)| e.pos.distance_to(centre) < 1.6)
                        .map(|(_, e)| e.kind.to_string())
                        .collect();
                    debug!(target: "printer", "{c} would not take after {n} clicks - deferred for this pass (cell holds {here}; entities in it: {occupants:?}; last click {} {:?} {} = {clicked}, eye {:.1} from the cell)",
                        if hit.block_pos == c { "AIR at" } else { "face of" }, hit.direction, hit.block_pos, block_dist(eye, c));
                    deferred.insert(c);
                }
                placed_now += 1;
            }
        }
        if placed_now > 0 {
            continue;
        }

        // A trip that ended out of reach of its cell: that cell is hard to get at from here - two
        // such tries and it waits for the next pass.
        if !movement::busy(bot)
            && let Some((c, tries)) = chasing
            && missing.contains(&c)
            && block_dist(eye, c) > REACH
        {
            if tries >= 2 {
                debug!(target: "printer", "{c}: no way to a spot within reach after {tries} trips - deferred for this pass");
                deferred.insert(c);
                chasing = None;
            } else {
                chasing = Some((c, tries + 1));
            }
        }
        // Nothing in reach: to the nearest open cell (or toward an unread part of the layer).
        let target = missing
            .iter()
            .filter(|c| !pending.contains_key(c) && !deferred.contains(c))
            .chain(unknown.iter())
            .min_by(|a, b| block_dist(eye, **a).total_cmp(&block_dist(eye, **b)))
            .copied();
        // A new trip only for a new target, or when the last one has ended: restarting one every
        // second sent a long trip back up to the cruise height each time, and it never came down
        // (15 Oct, the first Rust print hovered at y 321 for a minute).
        if let Some(c) = target {
            // A clear spot within reach of the cell: over it, under it (an overhang - the open ring
            // of a sphere's lower half), or beside it. Unread cells: over them, to load the chunk.
            let hover = {
                let Ok(world) = bot.world() else { break End::Stopped };
                let w = world.read();
                if w.get_block_state(c).is_some() { stand_for(&w, c) } else { None }
            }
            .unwrap_or(Vec3::new(c.x as f64 + 0.5, layer as f64 + 1.0, c.z as f64 + 0.5));
            if !movement::busy(bot) {
                if chasing.is_none_or(|(p, _)| p != c) {
                    chasing = Some((c, 0));
                }
                debug!(target: "printer", "to {} ({} open, {} unread on layer {layer})", c, missing.len(), unknown.len());
                // Within the build: straight there over the unbuilt layers, not via the cruise height.
                if pos.distance_to(hover) < 120.0 {
                    movement::go_direct(bot, hover, "print");
                } else {
                    movement::go(bot, hover, "print");
                }
            }
        }
        if last_place.elapsed() > STALL && off_x.hypot(off_z) < 8.0 {
            warn!(target: "printer", "no block placed for 3 min on layer {layer} of {} ({} open, {} unread) - moving up a layer",
                plan.name, missing.len(), unknown.len());
            layer += 1;
            on_layer(layer);
            last_place = Instant::now();
        }
    };
    movement::stop(bot);
    let placed = (melons_start - melons_all(bot)).max(0) as i64;
    info!(target: "printer", "{} pass ended {end:?} at layer {layer}: {placed} melons placed ({sent} clicks)", plan.name);
    PrintResult { end, placed, layer }
}

/// The click that puts a melon in `c`: a solid neighbour's face in reach (below first), else the
/// cell itself.
fn click_for(w: &World, eye: Vec3, c: BlockPos) -> BlockHit {
    let dirs = [Direction::Down, Direction::North, Direction::South, Direction::East, Direction::West, Direction::Up];
    for d in dirs {
        let n = c + offset(d);
        let Some(st) = w.get_block_state(n) else { continue };
        if !solid(st) || block_dist(eye, n) > REACH {
            continue;
        }
        // The neighbour's face that touches our cell, at its centre.
        let face = opposite(d);
        let o = offset(face);
        let centre = Vec3::new(n.x as f64 + 0.5 + o.x as f64 * 0.5, n.y as f64 + 0.5 + o.y as f64 * 0.5, n.z as f64 + 0.5 + o.z as f64 * 0.5);
        return BlockHit { block_pos: n, direction: face, location: centre, inside: false, world_border: false };
    }
    BlockHit {
        block_pos: c,
        direction: Direction::Up,
        location: Vec3::new(c.x as f64 + 0.5, c.y as f64 + 0.5, c.z as f64 + 0.5),
        inside: false,
        world_border: false,
    }
}

fn offset(d: Direction) -> BlockPos {
    match d {
        Direction::Down => BlockPos::new(0, -1, 0),
        Direction::Up => BlockPos::new(0, 1, 0),
        Direction::North => BlockPos::new(0, 0, -1),
        Direction::South => BlockPos::new(0, 0, 1),
        Direction::West => BlockPos::new(-1, 0, 0),
        Direction::East => BlockPos::new(1, 0, 0),
    }
}

fn opposite(d: Direction) -> Direction {
    match d {
        Direction::Down => Direction::Up,
        Direction::Up => Direction::Down,
        Direction::North => Direction::South,
        Direction::South => Direction::North,
        Direction::West => Direction::East,
        Direction::East => Direction::West,
    }
}

/// Melons carried, the offhand stack included.
fn melons_all(bot: &Client) -> i32 {
    let off = inv::menu(bot).and_then(|m| m.slots().get(45).cloned()).filter(|s| inv::stack_id(s) == "melon").map_or(0, |s| s.count());
    inv::count_id(bot, "melon") + off
}

fn place(bot: &Client, hit: BlockHit) {
    let seq = bot.query_self::<&mut BlockStatePredictionHandler, _>(|mut h| h.start_predicting()).unwrap_or(0);
    bot.write_packet(ServerboundUseItemOn { hand: InteractionHand::OffHand, block_hit: hit, seq });
}

/// A clear spot (feet) within reach of cell `c`: over it first, then under it, then beside it on
/// either side at its height or below.
fn stand_for(w: &World, c: BlockPos) -> Option<Vec3> {
    let (x, y, z) = (c.x as f64 + 0.5, c.y as f64, c.z as f64 + 0.5);
    let mut cands = vec![Vec3::new(x, y + 1.0, z), Vec3::new(x, y - 2.0, z), Vec3::new(x, y + 2.0, z), Vec3::new(x, y - 3.0, z)];
    for (dx, dz) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0), (1.0, 1.0), (-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0)] {
        for k in [1.0, 2.0] {
            for dy in [0.0, -1.0, -2.0, 1.0] {
                cands.push(Vec3::new(x + dx * k, y + dy, z + dz * k));
            }
        }
    }
    cands.into_iter().find(|f| block_dist(*f + Vec3::new(0.0, 1.62, 0.0), c) <= REACH - 0.3 && crate::movement::geom::stand_clear(w, *f))
}

/// What a check of the world found: open cells still missing, the lowest y with one, the total.
pub struct Check {
    pub missing: usize,
    pub lowest: i32,
    pub total: usize,
}

/// Fly over the build and count, from the world, every cell still open. Chunks are read as they
/// load, hopping above the build to the nearest unread one (24 hops at most); any never read count
/// as missing.
pub async fn check(ctx: &Shared, bot: &Client, plan: &Plan, origin: BlockPos, only: Option<&HashSet<BlockPos>>, fly_y: f64) -> Option<Check> {
    let mut by_chunk: HashMap<(i32, i32), Vec<BlockPos>> = HashMap::new();
    let mut total = 0;
    for cells in plan.layers.values() {
        for (x, y, z) in cells {
            let at = BlockPos::new(origin.x + x, origin.y + y, origin.z + z);
            if only.is_some_and(|o| !o.contains(&at)) {
                continue;
            }
            by_chunk.entry((at.x >> 4, at.z >> 4)).or_default().push(at);
            total += 1;
        }
    }
    let mut missing = 0usize;
    let mut lowest = i32::MAX;
    for hop in 0..=24 {
        if ctx.quit.load(std::sync::atomic::Ordering::SeqCst) || crate::routine::is_hungry(ctx, bot) {
            return None;
        }
        let Ok(world) = bot.world() else { return None };
        let next = {
            let w = world.read();
            let mut read = Vec::new();
            for (k, cells) in &by_chunk {
                if w.get_block_state(cells[0]).is_none() {
                    continue;
                }
                for c in cells {
                    if w.get_block_state(*c).is_some_and(gap) {
                        missing += 1;
                        lowest = lowest.min(c.y);
                    }
                }
                read.push(*k);
            }
            for k in read {
                by_chunk.remove(&k);
            }
            let pos = bot.position().unwrap_or_default();
            let (pcx, pcz) = ((pos.x as i32) >> 4, (pos.z as i32) >> 4);
            by_chunk.keys().min_by_key(|(cx, cz)| (cx - pcx).pow(2) + (cz - pcz).pow(2)).copied()
        };
        let Some((cx, cz)) = next else { break };
        if hop == 24 {
            break;
        }
        let _ = movement::fly_to(bot, Vec3::new((cx * 16 + 8) as f64, fly_y, (cz * 16 + 8) as f64), "check").await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let unread: usize = by_chunk.values().map(|v| v.len()).sum();
    for cells in by_chunk.values() {
        for c in cells {
            lowest = lowest.min(c.y);
        }
    }
    info!(target: "printer", "check of {}: {} of {} melons missing{}", plan.name, missing + unread, total,
        if unread > 0 { format!(" ({unread} in chunks never loaded - counted as missing)") } else { String::new() });
    Some(Check { missing: missing + unread, lowest, total })
}
