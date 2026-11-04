//! Where a new build or floor piece goes. Port of AiMelonRoutine.findBuildSite / findFloorSite /
//! scanArea / scanChunk - the rules: the closest open spot, rings out from the base;
//! the footprint at least melon-home-clear (200) from the base and the farm; melon-gap (20) clear of
//! anything built - anyone's - and of the other sites; any height that fits under the build limit,
//! never below the floor height (above water, always); never flattening. The floor: 100x101 cells
//! on a grid round the base, open water or air at the floor height, no melon there yet.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    block::BlockState,
    core::position::{ChunkPos, ChunkSectionBlockPos},
    registry::{builtin::BlockKind, tags::blocks as tags},
    world::{World, palette::Palette},
};
use tracing::{debug, info};

use crate::{bot::Shared, config::Area, movement, progress::Progress};

/// Natural ground, water and plants - anything else counts as built (anyone's). AiMelonRoutine.natural.
pub fn natural(st: BlockState) -> bool {
    if st.is_air() {
        return true;
    }
    let k = BlockKind::from(st);
    if matches!(k, BlockKind::Water | BlockKind::Lava | BlockKind::BubbleColumn) {
        return true;
    }
    for t in [
        &tags::BASE_STONE_OVERWORLD,
        &tags::DIRT,
        &tags::SAND,
        &tags::LOGS,
        &tags::LEAVES,
        &tags::FLOWERS,
        &tags::SMALL_FLOWERS,
        &tags::ICE,
        &tags::SNOW,
        &tags::CORALS,
        &tags::CORAL_BLOCKS,
        &tags::WALL_CORALS,
        &tags::BADLANDS_TERRACOTTA,
        &tags::REPLACEABLE,
        &tags::REPLACEABLE_BY_TREES,
        &tags::CAVE_VINES,
    ] {
        if t.contains(&k) {
            return true;
        }
    }
    let id = k.to_str().strip_prefix("minecraft:").unwrap_or(k.to_str());
    id.ends_with("_ore")
        || id.starts_with("deepslate")
        || id.ends_with("sandstone")
        || id.ends_with("terracotta")
        || id.contains("mushroom")
        || id.contains("amethyst")
        || id.contains("dripstone")
        || id.contains("sapling")
        || id.contains("kelp")
        || id.contains("seagrass")
        || id.contains("bamboo")
        || id.contains("azalea")
        || matches!(
            id,
            "grass_block" | "gravel" | "clay" | "mud" | "mycelium" | "podzol" | "moss_block" | "moss_carpet" | "sea_pickle"
                | "sugar_cane" | "cactus" | "vine" | "lily_pad" | "sweet_berry_bush" | "pumpkin" | "dead_bush" | "bedrock"
                | "obsidian" | "magma_block" | "calcite" | "tuff" | "packed_ice" | "blue_ice" | "glow_lichen" | "hanging_roots"
                | "spore_blossom" | "big_dripleaf" | "big_dripleaf_stem" | "small_dripleaf" | "cobweb" | "snow" | "powder_snow"
                | "rooted_dirt" | "suspicious_sand" | "suspicious_gravel" | "cocoa" | "pointed_dripstone"
        )
}

/// What one chunk shows for a candidate: the top of the natural ground under the footprint (every
/// 4th column), and the heights of built blocks within the margin box.
pub struct Scan {
    pub foot_top: i32,
    pub built_min: i32,
    pub built_max: i32,
    /// Per y (from the world's bottom): a built block at that height.
    pub built_at: Vec<bool>,
    pub example: Option<String>,
}

/// Inclusive x/z box.
#[derive(Clone, Copy, Debug)]
pub struct Box2 {
    pub x0: i32,
    pub z0: i32,
    pub x1: i32,
    pub z1: i32,
}

/// None when the chunk is not loaded.
fn scan_chunk(w: &World, cx: i32, cz: i32, foot: Box2, margin: Box2, floor_y: i32) -> Option<Scan> {
    let chunk = w.chunks.get(&ChunkPos::new(cx, cz))?;
    let chunk = chunk.read();
    let min_y = w.chunks.min_y();
    let height = w.chunks.height() as usize;
    let (bx, bz) = (cx * 16, cz * 16);
    let mut scan = Scan { foot_top: i32::MIN, built_min: i32::MAX, built_max: i32::MIN, built_at: vec![false; height], example: None };
    // The natural ground under the footprint: down from the top through built blocks and the air
    // under them (the heightmap alone read a floating build as the ground, 10 Oct).
    for lx in (0..16).step_by(4) {
        for lz in (0..16).step_by(4) {
            let (x, z) = (bx + lx, bz + lz);
            if x < foot.x0 || x > foot.x1 || z < foot.z0 || z > foot.z1 {
                continue;
            }
            let mut y = min_y + height as i32 - 1;
            while y > min_y {
                if let Some(st) = w.get_block_state(BlockPos::new(x, y, z))
                    && !st.is_air()
                    && natural(st)
                {
                    break;
                }
                y -= 1;
            }
            scan.foot_top = scan.foot_top.max(y);
        }
    }
    for (i, sec) in chunk.sections.iter().enumerate() {
        if sec.block_count == 0 {
            continue;
        }
        // A section of only natural blocks (all air, all stone, all water) has nothing built.
        let all_natural = match &sec.states.palette {
            Palette::SingleValue(v) => natural(*v),
            Palette::Linear(vs) | Palette::Hashmap(vs) => vs.iter().all(|v| natural(*v)),
            Palette::Global => false,
        };
        if all_natural {
            continue;
        }
        let base_y = min_y + i as i32 * 16;
        for ly in 0..16u8 {
            for lx in 0..16u8 {
                let x = bx + lx as i32;
                if x < margin.x0 || x > margin.x1 {
                    continue;
                }
                for lz in 0..16u8 {
                    let z = bz + lz as i32;
                    if z < margin.z0 || z > margin.z1 {
                        continue;
                    }
                    let st = sec.get_block_state(ChunkSectionBlockPos { x: lx, y: ly, z: lz });
                    if natural(st) {
                        continue;
                    }
                    let y = base_y + ly as i32;
                    if y == floor_y && BlockKind::from(st) == BlockKind::Melon {
                        continue; // its own floor is ground
                    }
                    scan.built_min = scan.built_min.min(y);
                    scan.built_max = scan.built_max.max(y);
                    scan.built_at[(y - min_y) as usize] = true;
                    if scan.example.is_none() {
                        scan.example = Some(format!("{} at {x} {y} {z}", BlockKind::from(st)));
                    }
                }
            }
        }
    }
    Some(scan)
}

/// Every chunk of the margin box, flying low over the nearest unread one until it loads (12
/// flights at most; 2 that bring nothing new = give up). None if some never loaded.
async fn scan_area(bot: &Client, foot: Box2, gap: i32, floor_y: i32) -> Option<HashMap<(i32, i32), Scan>> {
    let margin = Box2 { x0: foot.x0 - gap, z0: foot.z0 - gap, x1: foot.x1 + gap, z1: foot.z1 + gap };
    let chunks: Vec<(i32, i32)> = ((margin.x0 >> 4)..=(margin.x1 >> 4)).flat_map(|cx| ((margin.z0 >> 4)..=(margin.z1 >> 4)).map(move |cz| (cx, cz))).collect();
    let mut scans: HashMap<(i32, i32), Scan> = HashMap::new();
    let mut fruitless = 0;
    for flight in 0..12 {
        if fruitless >= 2 {
            break;
        }
        let had = scans.len();
        let missing: Vec<(i32, i32)> = {
            let world = bot.world().ok()?;
            let w = world.read();
            let mut out = Vec::new();
            for c in &chunks {
                if scans.contains_key(c) {
                    continue;
                }
                match scan_chunk(&w, c.0, c.1, foot, margin, floor_y) {
                    Some(s) => {
                        scans.insert(*c, s);
                    }
                    None => out.push(*c),
                }
            }
            out
        };
        if missing.is_empty() {
            return Some(scans);
        }
        if flight > 0 && scans.len() == had {
            fruitless += 1;
        }
        // The NEAREST unread chunk, from the cruise height: chunks load by x/z alone, and low down
        // the way in ran through land and its own floating builds (21-22 Oct: five deaths clipping
        // down through core_planet and a cathedral on the way to y 72 scan spots).
        let pos = bot.position().ok()?;
        let (px, pz) = ((pos.x as i32) >> 4, (pos.z as i32) >> 4);
        let near = *missing.iter().min_by_key(|(cx, cz)| (cx - px).pow(2) + (cz - pz).pow(2))?;
        if movement::fly_to(bot, Vec3::new((near.0 * 16 + 8) as f64, movement::trip::CRUISE_Y, (near.1 * 16 + 8) as f64), "scan").await.is_err() {
            fruitless += 1;
        }
        tokio::time::sleep(Duration::from_millis(2500)).await;
    }
    info!(target: "routine", "could not load all of the area round {} {} - trying the next spot", (foot.x0 + foot.x1) / 2, (foot.z0 + foot.z1) / 2);
    None
}

fn box_dist(b: Box2, px: i32, pz: i32) -> f64 {
    let dx = (b.x0 - px).max(px - b.x1).max(0) as f64;
    let dz = (b.z0 - pz).max(pz - b.z1).max(0) as f64;
    dx.hypot(dz)
}

fn box_box_dist(a: Box2, f: &Area) -> f64 {
    let dx = (a.x0 - f.max.x).max(f.min.x - a.x1).max(0) as f64;
    let dz = (a.z0 - f.max.z).max(f.min.z - a.z1).max(0) as f64;
    dx.hypot(dz)
}

/// Away from home: the footprint at least `clear` from the base and from the farm.
fn away_from_home(b: Box2, base: BlockPos, farm: Option<&Area>, clear: i32) -> bool {
    box_dist(b, base.x, base.z) >= clear as f64 && farm.is_none_or(|f| box_box_dist(b, f) >= clear as f64)
}

/// Candidate centres ring k out from the base, nearest first.
fn ring(bx: i32, bz: i32, k: i32, step: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for i in -k..=k {
        for j in -k..=k {
            if i.abs().max(j.abs()) == k {
                out.push((bx + i * step, bz + j * step));
            }
        }
    }
    out.sort_by_key(|(x, z)| ((x - bx) as i64).pow(2) + ((z - bz) as i64).pow(2));
    out
}

pub struct SiteRules {
    pub base: BlockPos,
    pub farm: Option<Area>,
    pub gap: i32,
    pub home_clear: i32,
    pub floor_y: i32,
    pub max_y: i32,
}

/// The closest open spot for a w x h x d build: its origin, or None.
pub async fn find_build_site(ctx: &Shared, bot: &Client, rules: &SiteRules, p: &Progress, w: i32, h: i32, d: i32) -> Option<BlockPos> {
    let (step, gap) = (32, rules.gap);
    let min_y = bot.world().ok()?.read().chunks.min_y();
    let (mut unreadable, mut unreadable_total) = (0, 0);
    for k in 0i32..400 {
        for (cx, cz) in ring(rules.base.x, rules.base.z, k, step) {
            if ctx.quit.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            let foot = Box2 { x0: cx - w / 2, z0: cz - d / 2, x1: cx - w / 2 + w - 1, z1: cz - d / 2 + d - 1 };
            if !away_from_home(foot, rules.base, rules.farm.as_ref(), rules.home_clear) {
                continue;
            }
            let clash = p.sites.iter().any(|s| foot.x1 + gap >= s.x && foot.x0 - gap <= s.x + s.w - 1 && foot.z1 + gap >= s.z && foot.z0 - gap <= s.z + s.d - 1);
            if clash {
                continue;
            }
            let far = ((cx - rules.base.x) as f64).hypot((cz - rules.base.z) as f64);
            let Some(scans) = scan_area(bot, foot, gap, rules.floor_y).await else {
                unreadable_total += 1;
                unreadable += 1;
                if unreadable >= 3 {
                    unreadable = 0;
                    info!(target: "routine", "three spots in a row out of reach - back to the base first");
                    let _ = movement::fly_to(bot, Vec3::new(rules.base.x as f64 + 0.5, rules.base.y as f64, rules.base.z as f64 + 0.5), "base").await;
                }
                if unreadable_total >= 6 {
                    return None;
                }
                continue;
            };
            unreadable = 0;
            let top = scans.values().map(|s| s.foot_top).max().unwrap_or(i32::MIN);
            let y0 = if top == i32::MIN { rules.floor_y } else { top + 1 }.max(rules.floor_y);
            if y0 + h - 1 > rules.max_y {
                debug!(target: "routine", "spot {far:.0} out: ground at {y0} leaves no room for {h} tall");
                continue;
            }
            let mut built_y = vec![false; (rules.max_y - min_y + 1) as usize];
            let mut example = None;
            for s in scans.values() {
                for (i, b) in s.built_at.iter().enumerate() {
                    if *b && i < built_y.len() {
                        built_y[i] = true;
                    }
                }
                if example.is_none() {
                    example = s.example.clone();
                }
            }
            // The lowest height with the whole build and the gap above and below clear of builds.
            let mut fit = None;
            let mut cand = y0;
            while cand + h - 1 <= rules.max_y {
                let lo = (cand - gap).max(min_y);
                let hi = (cand + h - 1 + gap).min(rules.max_y);
                match (lo..=hi).find(|y| built_y[(y - min_y) as usize]) {
                    Some(y) => cand = y + gap + 1,
                    None => {
                        fit = Some(cand);
                        break;
                    }
                }
            }
            let Some(fit) = fit else {
                info!(target: "routine", "spot {far:.0} out: no height clear of builds by {gap} (first built block: {example:?})");
                continue;
            };
            info!(target: "routine", "spot {far:.0} out is open at y {fit} (ground at {y0})");
            return Some(BlockPos::new(foot.x0, fit, foot.z0));
        }
    }
    None
}

/// A floor cell to fill: open (air or water) at the floor height, with nothing over it within
/// `clear` blocks. Under a tree or an overhang the only way in is a clip down through it, which the
/// server bills as a fall (22 Oct: 16 deaths laying the 796 open cells of one piece by land).
pub fn floor_cell_open(w: &World, c: BlockPos, clear: i32) -> bool {
    w.get_block_state(c).is_some_and(|st| st.is_air() || BlockKind::from(st) == BlockKind::Water)
        && (1..=clear).all(|dy| w.get_block_state(c.up(dy)).is_none_or(|st| st.is_air()))
}

/// How long a floor spot found unusable stays turned down.
const FLOOR_SKIP_FOR: Duration = Duration::from_secs(6 * 3600);

/// The next floor piece: (x0, z0) of a w x d cell on a grid round the base, nearest first, not laid
/// before, away from home, nothing built within the gap of the floor height, open cells (see
/// floor_cell_open) and no melon there yet.
pub async fn find_floor_site(ctx: &Shared, bot: &Client, rules: &SiteRules, p: &Progress, w: i32, d: i32) -> Option<(i32, i32)> {
    let fy = rules.floor_y;
    let min_y = bot.world().ok()?.read().chunks.min_y();
    let (mut unreadable, mut unreadable_total) = (0, 0);
    for k in 0i32..400 {
        for i in -k..=k {
            for j in -k..=k {
                if i.abs().max(j.abs()) != k || ctx.quit.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let (x0, z0) = (rules.base.x - w / 2 + i * w, rules.base.z - d / 2 + j * d);
                if p.floors.iter().any(|f| f[0] == x0 && f[1] == z0) {
                    continue;
                }
                let foot = Box2 { x0, z0, x1: x0 + w - 1, z1: z0 + d - 1 };
                if !away_from_home(foot, rules.base, rules.farm.as_ref(), rules.home_clear) {
                    continue;
                }
                // Turned down by an earlier search: every search starts again at the base, and
                // re-flying over each spot it had already turned down took ~50 min per piece (23 Oct).
                if ctx.routine.lock().floor_skipped.get(&(x0, z0)).is_some_and(|t| t.elapsed() < FLOOR_SKIP_FOR) {
                    continue;
                }
                let Some(scans) = scan_area(bot, foot, rules.gap, fy).await else {
                    unreadable_total += 1;
                    unreadable += 1;
                    if unreadable >= 3 {
                        unreadable = 0;
                        let _ = movement::fly_to(bot, Vec3::new(rules.base.x as f64 + 0.5, rules.base.y as f64, rules.base.z as f64 + 0.5), "base").await;
                    }
                    if unreadable_total >= 6 {
                        return None;
                    }
                    continue;
                };
                unreadable = 0;
                // A built block AT a height within the gap - not the lowest and highest built heights
                // spanning it: a geode at y -44 and its own cathedral at y 102 in one chunk turned the
                // spot down, and ~60% of the turned-down spots had nothing near the floor (23 Oct).
                let (lo, hi) = (fy - rules.gap, fy + rules.gap);
                let built_y = scans.values().find_map(|s| (lo..=hi).find(|y| s.built_at.get((y - min_y) as usize).copied().unwrap_or(false)));
                if let Some(by) = built_y {
                    ctx.routine.lock().floor_skipped.insert((x0, z0), Instant::now());
                    debug!(target: "routine", "floor spot {x0} {z0} skipped: built at y {by}, within {} of the floor", rules.gap);
                    continue;
                }
                let (open, melon) = {
                    let world = bot.world().ok()?;
                    let w = world.read();
                    let (mut open, mut melon) = (0, 0);
                    for x in foot.x0..=foot.x1 {
                        for z in foot.z0..=foot.z1 {
                            let c = BlockPos::new(x, fy, z);
                            match w.get_block_state(c) {
                                Some(st) if BlockKind::from(st) == BlockKind::Melon => melon += 1,
                                _ if floor_cell_open(&w, c, rules.gap) => open += 1,
                                _ => {}
                            }
                        }
                    }
                    (open, melon)
                };
                if melon > 0 || open == 0 {
                    ctx.routine.lock().floor_skipped.insert((x0, z0), Instant::now());
                    continue; // floored already, or all land
                }
                info!(target: "routine", "floor piece at {x0} {z0}: {open} open cells");
                return Some((x0, z0));
            }
        }
    }
    None
}
