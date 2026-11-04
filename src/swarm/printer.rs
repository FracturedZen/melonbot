//! The swarm printer (module "printer"): every swarm bot prints its share of the schematic the
//! controller has placed in Litematica - every block in the orientation the schematic wants
//! (print::place, the standalone Printer's rules), the states a placement cannot set clicked into
//! place (fix-block-states), farmland and paths finished with a tool (auto-till), different blocks
//! mined out first (clear-wrong-blocks), and materials fetched from the controller's supply chests.
//!
//! - **What**: the controller instance's live placement (melonhack/swarm-printer.json, written by
//!   its controller mod) or else Litematica's saved file for this server; the schematic from swarm/schematics
//!   (drop a file there by the same name) or where Litematica found it. Shared by the swarm (Job).
//! - **Who prints where**: the area (the controller's PRINTER_SYNC strip for this bot, else the
//!   whole build) is cut into 8x8 columns; a bot claims the nearest free one and prints it bottom
//!   up - the nuker's board, so no two bots share a column and an early finisher takes more.
//! - **Settings**: the controller instance's printer settings (strict-state-match, fix-block-states,
//!   auto-till, clear-wrong-blocks, places-per-window...), under the swarm panel's and each bot's own.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use azalea::{BlockPos, Client, Vec3, block::BlockState, registry::builtin::BlockKind};
use serde_json::Value;
use tracing::{debug, info, warn};

use super::{
    BotLink, SwarmCtx,
    nuker::{Box3, CELL},
};
use crate::{
    inv, movement,
    modules::ModuleConfig,
    print::{
        self, act, block_types, items,
        litematica::{self, Placement, SubPlacement, Target},
        place::{self, Dir},
        transform::{Mirror, Rotation},
    },
};

pub const MODULE: &str = "printer";
/// Eye to the clicked block's nearest point, as the server measures (4.5, less a margin).
const REACH: f64 = 4.4;
/// A clicked cell waits this long for the server's answer before it is judged again.
const AWAIT_TICKS: u64 = 12;
/// Clicks on one cell before it is left for a while.
const GIVE_UP_CLICKS: u32 = 5;
const DEFER_TICKS: u64 = 100;
/// The look goes out this many ticks before a click that reads it (the server takes the head's
/// yaw, which follows the body only on the player's tick - LivingEntity.getViewYRot).
const LOOK_AHEAD_TICKS: u64 = 3;
const RESCAN_TICKS: u64 = 5;
/// At most this many clicks a tick (the placement window is the real limit).
const PER_TICK: usize = 4;

// ------------------------------------------------------------------ the job

/// The schematic the swarm prints, where Litematica puts it.
pub struct Job {
    /// What it was built from (file, origin, turns): a change means a new job.
    pub key: String,
    pub name: String,
    /// "live from <instance>" or "Litematica file saved <time>".
    pub source: String,
    pub schematic: PathBuf,
    pub dimension: String,
    pub target: Target,
    pub area: Box3,
    /// The target's cells (8x8 columns), each cell's positions bottom up.
    pub by_cell: HashMap<(i32, i32), Vec<BlockPos>>,
}

impl Job {
    fn cell_of(&self, x: i32, z: i32) -> (i32, i32) {
        (self.area.min.x + (x - self.area.min.x).div_euclid(CELL) * CELL, self.area.min.z + (z - self.area.min.z).div_euclid(CELL) * CELL)
    }

    /// The build's positions inside `b` (a claimed cell - the job's own cells, or a strip's that
    /// cut across them), bottom up.
    fn positions_in(&self, b: &Box3) -> Vec<BlockPos> {
        let mut out = Vec::new();
        let (x0, z0) = self.cell_of(b.min.x, b.min.z);
        let mut x = x0;
        while x <= b.max.x {
            let mut z = z0;
            while z <= b.max.z {
                for p in self.by_cell.get(&(x, z)).into_iter().flatten() {
                    if p.x >= b.min.x && p.x <= b.max.x && p.z >= b.min.z && p.z <= b.max.z && p.y >= b.min.y && p.y <= b.max.y {
                        out.push(*p);
                    }
                }
                z += CELL;
            }
            x += CELL;
        }
        out.sort_by_key(|p| (p.y, p.x, p.z));
        out
    }
}

/// What the controller instance offers: its live export, else Litematica's own saved file.
struct Found {
    placements: Vec<Placement>,
    supply_chests: Vec<BlockPos>,
    mapart_chests: Vec<(String, BlockPos)>,
    source: String,
}

fn triple(v: &Value) -> Option<(i32, i32, i32)> {
    let a = v.as_array()?;
    Some((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32, a.get(2)?.as_i64()? as i32))
}

/// melonhack/swarm-printer.json (SwarmPrinterExport.java).
fn read_export(mc: &Path) -> Option<(Found, SystemTime)> {
    let file = mc.join("melonhack").join("swarm-printer.json");
    let saved = fs::metadata(&file).and_then(|m| m.modified()).ok()?;
    let v: Value = serde_json::from_str(&fs::read_to_string(&file).ok()?).ok()?;
    let dimension = v["dimension"].as_str().unwrap_or_default().to_string();
    let placements = v["placements"]
        .as_array()?
        .iter()
        .filter_map(|p| {
            Some(Placement {
                schematic: PathBuf::from(p["schematic"].as_str()?),
                name: p["name"].as_str().unwrap_or_default().to_string(),
                origin: triple(&p["origin"])?,
                rotation: Rotation::parse(p["rotation"].as_str().unwrap_or("NONE")),
                mirror: Mirror::parse(p["mirror"].as_str().unwrap_or("NONE")),
                enabled: p["enabled"].as_bool().unwrap_or(true),
                subs: p["subs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| {
                        Some(SubPlacement {
                            name: s["name"].as_str()?.to_string(),
                            pos: triple(&s["pos"]).unwrap_or((0, 0, 0)),
                            rotation: Rotation::parse(s["rotation"].as_str().unwrap_or("NONE")),
                            mirror: Mirror::parse(s["mirror"].as_str().unwrap_or("NONE")),
                            enabled: s["enabled"].as_bool().unwrap_or(true),
                        })
                    })
                    .collect(),
                dimension: dimension.clone(),
                file: file.clone(),
                saved: Some(saved),
            })
        })
        .collect();
    let supply_chests = v["supply_chests"].as_array().into_iter().flatten().filter_map(triple).map(|(x, y, z)| BlockPos::new(x, y, z)).collect();
    let mapart_chests = v["mapart_chests"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, p)| triple(p).map(|(x, y, z)| (k.to_lowercase(), BlockPos::new(x, y, z))))
        .collect();
    Some((Found { placements, supply_chests, mapart_chests, source: "live from the controller".into() }, saved))
}

fn find(mc: &Path, server: &str) -> Option<Found> {
    let export = read_export(mc);
    let saved_file = litematica::instance_placements(mc, server);
    let file_time = saved_file.iter().filter_map(|p| p.saved).max();
    match export {
        // The export is written live; Litematica's own file only on a relog. The newer wins.
        Some((f, t)) if file_time.is_none_or(|ft| t >= ft) => Some(f),
        Some((f, _)) if saved_file.is_empty() => Some(f),
        e => {
            let chests = e.map(|(f, _)| (f.supply_chests, f.mapart_chests)).unwrap_or_default();
            (!saved_file.is_empty()).then(|| Found {
                placements: saved_file,
                supply_chests: chests.0,
                mapart_chests: chests.1,
                source: format!("Litematica's saved file ({})", file_time.map(age).unwrap_or_default()),
            })
        }
    }
}

fn age(t: SystemTime) -> String {
    let s = t.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    if s < 120 { format!("{s} s ago") } else if s < 7200 { format!("{} min ago", s / 60) } else { format!("{} h ago", s / 3600) }
}

fn key_of(p: &Placement) -> String {
    format!("{}|{:?}|{:?}|{:?}|{:?}", p.schematic.display(), p.origin, p.rotation, p.mirror, p.subs)
}

/// Every few seconds: what the controller has placed. A new or moved placement is loaded (in the
/// background - a big build takes a moment) and replaces the job; the claims start over.
pub async fn watch_job(ctx: Arc<SwarmCtx>) {
    let mut failed_key = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(4)).await;
        // TEST ONLY: a stand-in controller .minecraft (the offline test rig has no Java instance).
        let test_mc = std::env::var_os("MELONBOT_CONTROLLER_MC").map(PathBuf::from);
        let Some(mc) = test_mc.or_else(|| ctx.controller_instance.lock().as_ref().and_then(|c| c.file.parent().and_then(Path::parent).map(Path::to_path_buf))) else {
            continue;
        };
        let server = ctx.cfg.read().server.clone();
        let mc2 = mc.clone();
        let Ok(Some(found)) = tokio::task::spawn_blocking(move || find(&mc2, &server)).await else { continue };
        // The placement: the one the panel picked, else the enabled one nearest the controller
        // player (in the overworld when it is not known).
        let want = ctx.cfg.read().printer_placement.clone();
        let near = ctx.controller.lock().pos.as_ref().map(|p| p.0);
        let mut enabled: Vec<&Placement> = found.placements.iter().filter(|p| p.enabled).collect();
        if let Some(w) = want.as_deref().filter(|w| !w.is_empty()) {
            enabled.retain(|p| p.name == w);
        }
        enabled.sort_by(|a, b| {
            let d = |p: &Placement| near.map_or(0.0, |n| Vec3::new(p.origin.0 as f64, p.origin.1 as f64, p.origin.2 as f64).distance_to(n));
            d(a).total_cmp(&d(b))
        });
        *ctx.printer_offers.lock() = found.placements.iter().map(|p| (p.name.clone(), p.enabled, p.dimension.clone())).collect();
        // The chests change without the placement: kept apart from the job.
        *ctx.print_chests.lock() = (found.supply_chests.clone(), found.mapart_chests.clone());
        let Some(p) = enabled.first().map(|p| (*p).clone()) else {
            if ctx.print_job.read().is_some() {
                info!(target: "printer", "no enabled placement in the controller's Litematica any more");
            }
            *ctx.print_job.write() = None;
            continue;
        };
        let key = key_of(&p);
        if ctx.print_job.read().as_ref().is_some_and(|j| j.key == key) || key == failed_key {
            continue;
        }
        let drop_dir = ctx.dir.join("schematics");
        let _ = fs::create_dir_all(&drop_dir);
        let Some(file) = litematica::find_schematic(&p, &drop_dir, Some(&mc)) else {
            ctx.warn_once(&format!(
                "the controller's placement \"{}\" uses {} - not found (drop it into {})",
                p.name,
                p.schematic.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(),
                drop_dir.display()
            ));
            failed_key = key;
            continue;
        };
        let p2 = p.clone();
        let file2 = file.clone();
        let built = tokio::task::spawn_blocking(move || -> Result<Job, String> {
            let schem = print::schem::load(&file2).map_err(|e| format!("{e:#}"))?;
            let target = litematica::target(&schem, &p2);
            if target.blocks.is_empty() {
                return Err("the placement has no blocks (every sub-region off?)".into());
            }
            let area = Box3::new(target.min, target.max);
            let mut by_cell: HashMap<(i32, i32), Vec<BlockPos>> = HashMap::new();
            for pos in target.blocks.keys() {
                let cx = area.min.x + (pos.x - area.min.x).div_euclid(CELL) * CELL;
                let cz = area.min.z + (pos.z - area.min.z).div_euclid(CELL) * CELL;
                by_cell.entry((cx, cz)).or_default().push(*pos);
            }
            for v in by_cell.values_mut() {
                v.sort_by_key(|p| (p.y, p.x, p.z));
            }
            Ok(Job {
                key: key_of(&p2),
                name: p2.name.clone(),
                source: String::new(),
                schematic: file2,
                dimension: p2.dimension.clone(),
                target,
                area,
                by_cell,
            })
        })
        .await;
        match built {
            Ok(Ok(mut job)) => {
                job.source = found.source.clone();
                info!(
                    target: "printer",
                    "print job: \"{}\" from {} ({}) - {} blocks in {} cells, {:?}..{:?}{}{}",
                    job.name,
                    job.schematic.display(),
                    job.source,
                    job.target.blocks.len(),
                    job.by_cell.len(),
                    job.area.min,
                    job.area.max,
                    if job.target.disabled.is_empty() { String::new() } else { format!(", sub-regions off: {:?}", job.target.disabled) },
                    if job.target.unknown.is_empty() { String::new() } else { format!(", blocks this client does not know (left out): {:?}", job.target.unknown) },
                );
                ctx.feed("event", &format!("printer: \"{}\" - {} blocks ({})", job.name, job.target.blocks.len(), job.source));
                ctx.print_board.lock().reset();
                *ctx.print_job.write() = Some(Arc::new(job));
                failed_key.clear();
            }
            Ok(Err(e)) => {
                ctx.warn_once(&format!("the schematic {} could not be read: {e}", file.display()));
                failed_key = key;
            }
            Err(e) => warn!(target: "printer", "loading the job failed: {e}"),
        }
    }
}

// ------------------------------------------------------------------ settings

/// The printer's settings, in the standalone Printer's names.
#[derive(Clone, Debug)]
pub struct Cfg {
    pub strict: bool,
    pub fix_states: bool,
    pub auto_till: bool,
    pub clear_wrong: bool,
    pub restock: bool,
    pub per_window: usize,
    pub window: Duration,
    /// The controller's strip for this bot (PRINTER_SYNC), when it split the build.
    pub region: Option<Box3>,
}

impl Cfg {
    pub fn from_settings(m: &ModuleConfig) -> Option<Self> {
        if !m.enabled(MODULE) {
            return None;
        }
        Some(Self {
            strict: m.get(MODULE, "strict-state-match", false),
            fix_states: m.get(MODULE, "fix-block-states", true),
            auto_till: m.get(MODULE, "auto-till", true),
            clear_wrong: m.get(MODULE, "clear-wrong-blocks", false),
            restock: m.get(MODULE, "use-storage-chests", true),
            per_window: m.get::<usize>(MODULE, "places-per-window", 9).clamp(1, 64),
            window: Duration::from_millis(m.get::<u64>(MODULE, "place-window-ms", 300) + 20),
            region: None,
        })
    }
}

/// PRINTER_SYNC|autoMove|clusterR|maxCluster|dwell|station|cruise|bpt|printRange|rotate|strict|restock|mapart|regOn|minX|minY|minZ|maxX|maxY|maxZ|carpetCsv
/// - the controller's strip for this bot (and its strict setting).
pub fn region_from_sync(raw: &str) -> Option<(Option<Box3>, bool)> {
    let p: Vec<&str> = raw.split('|').collect();
    if p.first() != Some(&"PRINTER_SYNC") || p.len() < 20 {
        return None;
    }
    let b = |i: usize| p.get(i).is_some_and(|s| s.trim().eq_ignore_ascii_case("true"));
    let n = |i: usize| p.get(i).and_then(|s| s.trim().parse::<i32>().ok());
    let region = if b(13) {
        match (n(14), n(15), n(16), n(17), n(18), n(19)) {
            (Some(a), Some(bb), Some(c), Some(d), Some(e), Some(f)) => Some(Box3::new(BlockPos::new(a, bb, c), BlockPos::new(d, e, f))),
            _ => None,
        }
    } else {
        None
    };
    Some((region, b(10)))
}

// ------------------------------------------------------------------ one bot

/// What is to be done at a position.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Work {
    Place,
    /// Right-clicks to bring a placed block to the schematic's state.
    Fix(u32),
    /// A hoe or shovel on the dirt that went down for farmland / a path.
    Till,
    /// A different block there: mine it first.
    Clear,
}

/// One bot's printer state.
#[derive(Default)]
pub struct BotPrinter {
    pub cell: Option<(i32, i32)>,
    pub status: String,
    pub placed: u64,
    pub fixed: u64,
    pub cleared: u64,
    /// Items the cell wants that the bags do not have.
    pub missing: BTreeSet<String>,
    job_key: String,
    work: Vec<(BlockPos, BlockState, Work)>,
    unread: usize,
    last_scan: u64,
    awaiting: HashMap<BlockPos, u64>,
    attempts: HashMap<BlockPos, u32>,
    deferred: HashMap<BlockPos, u64>,
    /// A click that needs its look a tick or more ahead: where, the choice, the tick it may go.
    look: Option<(BlockPos, place::Choice, u64)>,
    recent: VecDeque<Instant>,
    /// A wrong block being mined: where, and the tick its STOP goes.
    mining: Option<(BlockPos, u64)>,
    /// The offhand swap in flight: no clicks until this tick.
    swap_until: u64,
    /// Why a cell was deferred, said once each.
    said: BTreeSet<String>,
    /// Times each position has been left for a while; after MAX_DEFERS it is given up on.
    defers: HashMap<BlockPos, u32>,
    /// Positions given up on this job (counted on the panel; the rest of the cell goes on).
    pub given_up: std::collections::HashSet<BlockPos>,
    /// Clicks a state fix needed last time (progress resets the click count).
    fix_need: HashMap<BlockPos, u32>,
}

/// Times a position is left for a while before it is given up on (its cell is done without it).
const MAX_DEFERS: u32 = 3;

/// What the printer wants this tick.
pub enum Want {
    Nothing,
    Moving,
    /// Materials from the supply chests: item, how many the cell still needs.
    Restock(Vec<(String, i32)>),
}

fn kind(s: BlockState) -> BlockKind {
    BlockKind::from(s)
}

fn prop(s: BlockState, k: &str) -> Option<&'static str> {
    s.to_trait().get_property(k)
}

fn num(s: BlockState, k: &str) -> Option<i32> {
    prop(s, k).and_then(|v| v.parse().ok())
}

/// Printer.interactionsNeeded: right-clicks from `cur` to `req` - repeater delay, comparator mode,
/// open/closed, a lever, a daylight detector, and (not in the standalone) a note block's pitch.
fn clicks_needed(cur: BlockState, req: BlockState) -> u32 {
    if kind(cur) != kind(req) {
        return 0;
    }
    let id = kind(cur).to_str();
    if let (Some(c), Some(r)) = (num(cur, "delay"), num(req, "delay")) {
        return (r - c).rem_euclid(4) as u32;
    }
    if let (Some(c), Some(r)) = (prop(cur, "mode"), prop(req, "mode"))
        && id.ends_with("comparator")
    {
        return (c != r) as u32;
    }
    if let (Some(c), Some(r)) = (prop(cur, "open"), prop(req, "open")) {
        // Iron doors and trapdoors only open by redstone; a barrel only by being in use.
        if id.contains("iron_") || id.ends_with("barrel") {
            return 0;
        }
        return (c != r) as u32;
    }
    // Before "powered": a note block has that too.
    if let (Some(c), Some(r)) = (num(cur, "note"), num(req, "note"))
        && id.ends_with("note_block")
    {
        return (r - c).rem_euclid(25) as u32;
    }
    if let (Some(c), Some(r)) = (prop(cur, "powered"), prop(req, "powered")) {
        return if id.ends_with("lever") { (c != r) as u32 } else { 0 };
    }
    if let (Some(c), Some(r)) = (prop(cur, "inverted"), prop(req, "inverted")) {
        return (c != r) as u32;
    }
    0
}

/// The schematic wants more of a stacking block here (candles, pickles, eggs, layers, petals).
fn count_short(cur: BlockState, req: BlockState) -> bool {
    ["candles", "pickles", "eggs", "layers", "flower_amount", "segment_amount"].iter().any(|p| matches!((num(cur, p), num(req, p)), (Some(c), Some(r)) if c < r))
}

fn has_block_entity(s: BlockState) -> bool {
    let id = kind(s).to_str();
    let t = block_types::of(id.strip_prefix("minecraft:").unwrap_or(id));
    t.contains("chest")
        || t.contains("sign")
        || t.contains("banner")
        || t.contains("skull")
        || t.contains("head")
        || t.contains("shulker")
        || matches!(
            t,
            "barrel" | "furnace" | "smoker" | "blast_furnace" | "hopper" | "dispenser" | "dropper" | "brewing_stand" | "beacon" | "enchantment_table"
                | "bed" | "lectern" | "jukebox" | "campfire" | "beehive" | "spawner" | "command" | "structure" | "jigsaw" | "conduit" | "bell"
                | "decorated_pot" | "chiseled_book_shelf" | "crafter" | "vault" | "trial_spawner" | "sculk_sensor" | "calibrated_sculk_sensor"
                | "sculk_catalyst" | "sculk_shrieker" | "comparator" | "daylight_detector" | "end_gateway" | "end_portal" | "shelf" | "copper_golem_statue"
                | "weathering_copper_golem_statue" | "creaking_heart" | "test" | "test_instance"
        )
}

fn replaceable(s: BlockState) -> bool {
    azalea::registry::tags::blocks::REPLACEABLE.contains(&kind(s))
}

/// Printer.isPathable / isTillable, and a clear cell above (both tools refuse otherwise).
fn tool_finish(w: &azalea::world::World, pos: BlockPos, cur: BlockState, req: BlockState) -> bool {
    let above = w.get_block_state(pos.up(1)).unwrap_or(BlockState::AIR);
    if !above.is_air() {
        return false;
    }
    let ck = kind(cur);
    match kind(req) {
        BlockKind::Farmland => matches!(ck, BlockKind::Dirt | BlockKind::GrassBlock | BlockKind::DirtPath | BlockKind::CoarseDirt | BlockKind::RootedDirt),
        BlockKind::DirtPath => matches!(ck, BlockKind::Dirt | BlockKind::GrassBlock | BlockKind::CoarseDirt | BlockKind::RootedDirt | BlockKind::Podzol | BlockKind::Mycelium),
        _ => false,
    }
}

/// What a position needs, or None when it is done (or nothing can be done there).
fn judge(w: &azalea::world::World, pos: BlockPos, req: BlockState, cfg: &Cfg, has_hoe: bool, has_shovel: bool) -> Option<Work> {
    let cur = w.get_block_state(pos)?;
    let (ck, rk) = (kind(cur), kind(req));
    if ck == rk {
        if cfg.fix_states {
            let n = clicks_needed(cur, req);
            if n > 0 {
                return Some(Work::Fix(n));
            }
        }
        if count_short(cur, req) || prop(req, "type") == Some("double") && prop(cur, "type") != Some("double") {
            return Some(Work::Place);
        }
        // A wrong orientation cannot be clicked right; like the standalone, it is not re-placed.
        return None;
    }
    if items::placed_form(rk) == Some(ck) {
        let tool = if rk == BlockKind::Farmland { has_hoe } else { has_shovel };
        return (cfg.auto_till && tool && tool_finish(w, pos, cur, req)).then_some(Work::Till);
    }
    if cur.is_air() || replaceable(cur) {
        return Some(Work::Place);
    }
    if cfg.clear_wrong && !has_block_entity(cur) && cur.to_trait().behavior().destroy_time >= 0.0 {
        return Some(Work::Clear);
    }
    None
}

fn offhand(bot: &Client) -> azalea::inventory::ItemStack {
    inv::menu(bot).and_then(|m| m.slots().get(45).cloned()).unwrap_or_default()
}

/// A clear spot (feet) within reach of `c`: over it first, then beside it, then under it.
fn stand_for(w: &azalea::world::World, c: BlockPos) -> Option<Vec3> {
    let (x, y, z) = (c.x as f64 + 0.5, c.y as f64, c.z as f64 + 0.5);
    let mut cands = vec![Vec3::new(x, y + 1.0, z), Vec3::new(x, y + 2.0, z)];
    for (dx, dz) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0), (1.0, 1.0), (-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0)] {
        for k in [1.0, 2.0] {
            for dy in [1.0, 2.0, 0.0, -1.0] {
                cands.push(Vec3::new(x + dx * k, y + dy, z + dz * k));
            }
        }
    }
    cands.push(Vec3::new(x, y - 2.0, z));
    cands.into_iter().find(|f| place::block_dist(*f + Vec3::new(0.0, 1.62, 0.0), c) <= REACH - 0.4 && movement::geom::stand_clear(w, *f))
}

/// A main-hand right click on a block (a state fix, a tool finish): the block's own use, not a
/// placement.
fn use_on(bot: &Client, pos: BlockPos, eye: Vec3, top: bool) {
    let centre = Vec3::new(pos.x as f64 + 0.5, pos.y as f64 + 0.5, pos.z as f64 + 0.5);
    let face = if top {
        Dir::Up
    } else {
        let d = eye - centre;
        let (ax, ay, az) = (d.x.abs(), d.y.abs(), d.z.abs());
        if ay >= ax && ay >= az {
            if d.y > 0.0 { Dir::Up } else { Dir::Down }
        } else if ax >= az {
            if d.x > 0.0 { Dir::East } else { Dir::West }
        } else if d.z > 0.0 {
            Dir::South
        } else {
            Dir::North
        }
    };
    let hit = if top { Vec3::new(centre.x, pos.y as f64 + 1.0, centre.z) } else { centre };
    act::use_main(bot, pos, face, hit);
}

/// One tick of the printer for this bot.
pub fn tick(bot: &Client, ctx: &SwarmCtx, link: &BotLink, cfg: &Cfg, ticks: u64) -> Want {
    let Some(job) = ctx.print_job.read().clone() else {
        link.printer.lock().status = "no placement from the controller's Litematica".into();
        return Want::Nothing;
    };
    let Ok(pos) = bot.position() else { return Want::Nothing };
    let eye = pos + Vec3::new(0.0, 1.62, 0.0);
    let Ok(world) = bot.world() else { return Want::Nothing };
    let mut st = link.printer.lock();
    if st.job_key != job.key {
        *st = BotPrinter { job_key: job.key.clone(), ..Default::default() };
    }
    st.awaiting.retain(|_, at| ticks.saturating_sub(*at) < AWAIT_TICKS);
    st.deferred.retain(|_, until| *until > ticks);

    // A wrong block being mined: STOP when its time comes.
    if let Some((p, stop_at)) = st.mining {
        if ticks < stop_at {
            st.status = format!("clearing {} {} {}", p.x, p.y, p.z);
            return Want::Moving;
        }
        crate::modules::harvest::send_action(bot, p, false);
        st.mining = None;
        st.cleared += 1;
        st.awaiting.insert(p, ticks);
    }

    // The area: the controller's strip for this bot, else the whole build; a cell of it.
    let area = match cfg.region {
        Some(r) => Box3::new(
            BlockPos::new(r.min.x.max(job.area.min.x), r.min.y.max(job.area.min.y), r.min.z.max(job.area.min.z)),
            BlockPos::new(r.max.x.min(job.area.max.x), r.max.y.min(job.area.max.y), r.max.z.min(job.area.max.z)),
        ),
        None => job.area,
    };
    let cell = {
        let mut board = ctx.print_board.lock();
        // Cells with nothing of the build in them are done at once.
        let c = board.claim(area, &link.name, pos);
        if let Some(c) = c
            && job.positions_in(&area.cell_box(c)).is_empty()
        {
            board.mark_done(area, c);
            st.cell = None;
            return Want::Moving;
        }
        c
    };
    let Some(cell) = cell else {
        st.cell = None;
        st.status = if ctx.print_board.lock().just_finished(area) {
            ctx.feed("event", &format!("printer: {} finished its part of \"{}\"", link.name, job.name));
            "its part is printed".into()
        } else {
            "its part is printed".into()
        };
        return Want::Nothing;
    };
    if st.cell != Some(cell) {
        st.cell = Some(cell);
        st.last_scan = 0;
        debug!(target: "printer", "cell {} {}", cell.0, cell.1);
    }

    // What the cell needs, from the world, every few ticks.
    let (has_hoe, has_shovel) = {
        let bags = inv::bags(bot);
        (bags.iter().any(|(_, s)| inv::stack_id(s).ends_with("_hoe")), bags.iter().any(|(_, s)| inv::stack_id(s).ends_with("_shovel")))
    };
    if ticks.saturating_sub(st.last_scan) >= RESCAN_TICKS || st.work.is_empty() {
        st.last_scan = ticks;
        let w = world.read();
        let mut work = Vec::new();
        let mut unread = 0;
        for p in job.positions_in(&area.cell_box(cell)) {
            if st.given_up.contains(&p) {
                continue;
            }
            let req = job.target.blocks[&p];
            if items::item_for(req).is_none() && !matches!(kind(req), BlockKind::Farmland | BlockKind::DirtPath) {
                continue;
            }
            if w.get_block_state(p).is_none() {
                unread += 1;
                continue;
            }
            if let Some(k) = judge(&w, p, req, cfg, has_hoe, has_shovel) {
                work.push((p, req, k));
            }
        }
        st.work = work;
        st.unread = unread;
        // Done when nothing is left but what was given up on (a deferred cell still holds it open).
        if st.work.is_empty() && unread == 0 {
            debug!(target: "printer", "cell {} {} done", cell.0, cell.1);
            ctx.print_board.lock().mark_done(area, cell);
            st.cell = None;
            st.status = "cell done".into();
            return Want::Moving;
        }
    }

    // The lowest open layer only: nothing goes down before what holds it (16 Oct e2e test: a sign
    // and a repeater one layer up were tried over a floor not yet placed, and left).
    let open: Vec<(BlockPos, BlockState, Work)> =
        st.work.iter().copied().filter(|(p, _, _)| !st.awaiting.contains_key(p) && !st.deferred.contains_key(p)).collect();
    let floor = open.iter().map(|(p, _, _)| p.y).min();
    let layer: Vec<(BlockPos, BlockState, Work)> = open.into_iter().filter(|(p, _, _)| floor.is_some_and(|f| p.y <= f)).collect();

    // Materials: what the layer needs that the bags do not have, and what the whole cell still
    // needs beyond what is carried (one trip fetches that - and no more, so the next bot finds
    // some: the same test, one bot took every stone stack and the other's floor never went down).
    let mut carried: HashMap<&'static str, i32> = HashMap::new();
    for s in inv::bags(bot).iter().map(|(_, s)| s.clone()).chain(std::iter::once(offhand(bot))) {
        *carried.entry(inv::stack_id(&s)).or_default() += s.count();
    }
    let mut missing: BTreeSet<String> = BTreeSet::new();
    for (_, req, w) in &layer {
        if matches!(w, Work::Place)
            && let Some(item) = items::item_for(*req)
            && carried.get(item.as_str()).copied().unwrap_or(0) <= 0
        {
            missing.insert(item);
        }
    }
    let mut short: HashMap<String, i32> = HashMap::new();
    for (_, req, w) in &st.work {
        if matches!(w, Work::Place)
            && let Some(item) = items::item_for(*req)
        {
            *short.entry(item).or_default() += 1;
        }
    }
    short.retain(|i, n| {
        *n -= carried.get(i.as_str()).copied().unwrap_or(0);
        *n > 0
    });
    st.missing = missing.clone();

    // The click a look was sent for: it may go now.
    let now = Instant::now();
    while st.recent.front().is_some_and(|t| now.duration_since(*t) >= cfg.window) {
        st.recent.pop_front();
    }
    if let Some((p, choice, at)) = st.look {
        if ticks >= at {
            st.look = None;
            if st.recent.len() < cfg.per_window && inv::stack_id(&offhand(bot)) == items::item_for(job.target.blocks[&p]).unwrap_or_default() {
                act::click(bot, &choice);
                st.recent.push_back(Instant::now());
                note_click(&mut st, p, ticks, ctx);
                st.placed += 1;
            }
        }
        st.status = format!("placing at {} {} {}", p.x, p.y, p.z);
        return Want::Moving;
    }

    // Clicks in reach: by kind, nearest first; one item kind in the offhand at a time.
    let mut in_reach: Vec<(BlockPos, BlockState, Work)> = layer.iter().copied().filter(|(p, _, _)| place::block_dist(eye, *p) <= REACH).collect();
    in_reach.sort_by(|a, b| place::block_dist(eye, a.0).total_cmp(&place::block_dist(eye, b.0)));
    let held = inv::stack_id(&offhand(bot)).to_string();
    let mut done_now = 0;
    if ticks >= st.swap_until {
        for (p, req, w) in in_reach.iter().copied() {
            if done_now >= PER_TICK || st.recent.len() >= cfg.per_window {
                break;
            }
            match w {
                Work::Clear => {
                    if done_now > 0 || st.mining.is_some() {
                        continue;
                    }
                    let Some(cur) = world.read().get_block_state(p) else { continue };
                    let (slot, per_tick) = super::nuker::best_tool_for(bot, cur);
                    if let Some(slot) = slot
                        && bot.selected_hotbar_slot().map(|h| h as usize + 36).ok() != Some(slot)
                    {
                        inv::hold(bot, slot, &[]);
                        st.swap_until = ticks + 3;
                        break;
                    }
                    if per_tick <= 0.0 {
                        defer(&mut st, p, ticks, "cannot be broken by anything carried", ctx);
                        continue;
                    }
                    debug!(target: "printer", "clearing {} at {p} (wants {})", cur.to_trait().id(), req.to_trait().id());
                    if per_tick >= 1.0 {
                        crate::modules::harvest::send_break(bot, p);
                        st.cleared += 1;
                        st.awaiting.insert(p, ticks);
                    } else {
                        crate::modules::harvest::send_action(bot, p, true);
                        st.mining = Some((p, ticks + (0.9 / per_tick as f64).ceil() as u64));
                    }
                    done_now += 1;
                    break;
                }
                Work::Fix(n) => {
                    // A note block can need 24 clicks: clicks that got it closer are not failures.
                    if st.fix_need.get(&p).is_some_and(|prev| n < *prev) {
                        st.attempts.remove(&p);
                    }
                    st.fix_need.insert(p, n);
                    // FenceGateBlock.useWithoutItem turns a gate opened from its front to face the
                    // player's way: face the gate's own way first (the body's yaw, read at once).
                    if block_types::of(kind(req).to_str().trim_start_matches("minecraft:")) == "fence_gate"
                        && let Some(f) = prop(req, "facing").and_then(Dir::parse)
                    {
                        let yaw = match f {
                            Dir::South => 0.0,
                            Dir::West => 90.0,
                            Dir::North => 180.0,
                            _ => 270.0,
                        };
                        movement::look_now(bot, yaw, 0.0);
                    }
                    use_on(bot, p, eye, false);
                    st.recent.push_back(Instant::now());
                    note_click(&mut st, p, ticks, ctx);
                    st.fixed += 1;
                    done_now += 1;
                }
                Work::Till => {
                    let want = if kind(req) == BlockKind::Farmland { "_hoe" } else { "_shovel" };
                    let held_main = bot.get_held_item().map(|s| inv::stack_id(&s).ends_with(want)).unwrap_or(false);
                    if !held_main {
                        if let Some((slot, _)) = inv::bags(bot).into_iter().find(|(_, s)| inv::stack_id(s).ends_with(want)) {
                            inv::hold(bot, slot, &[]);
                            st.swap_until = ticks + 3;
                        }
                        break;
                    }
                    use_on(bot, p, eye, true);
                    st.recent.push_back(Instant::now());
                    note_click(&mut st, p, ticks, ctx);
                    done_now += 1;
                }
                Work::Place => {
                    let Some(item) = items::item_for(req) else { continue };
                    if item != held {
                        // One swap at a time, for the nearest cell's item, if it is carried.
                        if done_now == 0
                            && let Some((slot, _)) = inv::bags(bot).into_iter().filter(|(_, s)| inv::stack_id(s) == item).max_by_key(|(_, s)| s.count())
                        {
                            inv::swap_with(bot, slot, inv::OFFHAND_BUTTON);
                            st.swap_until = ticks + 2;
                            break;
                        }
                        continue;
                    }
                    let choice = {
                        let w = world.read();
                        place::choose(&w, eye, p, req, REACH)
                    };
                    match choice {
                        Ok(c) if c.needs_look && place::head_yaw_matters(req) => {
                            if done_now > 0 {
                                break;
                            }
                            movement::look_now(bot, c.click.yaw, c.click.pitch);
                            st.look = Some((p, c, ticks + LOOK_AHEAD_TICKS));
                            st.status = format!("placing at {} {} {}", p.x, p.y, p.z);
                            return Want::Moving;
                        }
                        Ok(c) => {
                            act::click(bot, &c);
                            st.recent.push_back(Instant::now());
                            note_click(&mut st, p, ticks, ctx);
                            st.placed += 1;
                            done_now += 1;
                        }
                        Err(why) => defer(&mut st, p, ticks, &why, ctx),
                    }
                }
            }
        }
    }

    // Out of what this layer needs: fetch it.
    if done_now == 0 && !missing.is_empty() && layer.iter().all(|(_, req, w)| !matches!(w, Work::Place) || items::item_for(*req).is_some_and(|i| missing.contains(&i))) {
        st.status = format!("needs {}", missing.iter().cloned().collect::<Vec<_>>().join(", "));
        let mut want: Vec<(String, i32)> = short.into_iter().collect();
        want.sort();
        return if cfg.restock { Want::Restock(want) } else { Want::Nothing };
    }

    // Movement: within reach of the nearest open position of the layer (or over unread chunks).
    if done_now == 0 {
        let next = layer
            .iter()
            .filter(|(_, req, w)| !matches!(w, Work::Place) || items::item_for(*req).is_some_and(|i| !missing.contains(&i)))
            .min_by(|a, b| place::block_dist(eye, a.0).total_cmp(&place::block_dist(eye, b.0)))
            .map(|(p, _, _)| *p);
        let spot = match next {
            Some(p) => {
                let w = world.read();
                stand_for(&w, p)
            }
            None if st.unread > 0 => Some(Vec3::new(cell.0 as f64 + 4.0, job.area.max.y as f64 + 3.0, cell.1 as f64 + 4.0)),
            None => None,
        };
        if let Some(s) = spot
            && s.distance_to(pos) > 0.6
        {
            if s.distance_to(pos) > 120.0 {
                movement::go(bot, s, "print");
            } else {
                movement::steer(bot, s, "print", 0.4);
            }
        }
    }
    st.status = format!(
        "cell {} {}: {} to do{}{}{}",
        cell.0,
        cell.1,
        st.work.len(),
        if st.unread > 0 { format!(", {} unread", st.unread) } else { String::new() },
        if st.deferred.is_empty() { String::new() } else { format!(", {} waiting", st.deferred.len()) },
        if st.given_up.is_empty() { String::new() } else { format!(", {} given up", st.given_up.len()) }
    );
    Want::Moving
}

fn note_click(st: &mut BotPrinter, p: BlockPos, ticks: u64, ctx: &SwarmCtx) {
    st.awaiting.insert(p, ticks);
    let n = st.attempts.entry(p).or_insert(0);
    *n += 1;
    if *n >= GIVE_UP_CLICKS {
        st.attempts.remove(&p);
        defer(st, p, ticks, "would not take after 5 clicks", ctx);
    }
}

fn defer(st: &mut BotPrinter, p: BlockPos, ticks: u64, why: &str, _ctx: &SwarmCtx) {
    let n = st.defers.entry(p).or_insert(0);
    *n += 1;
    if *n > MAX_DEFERS {
        st.defers.remove(&p);
        st.given_up.insert(p);
        info!(target: "printer", "{} {} {} given up on after {MAX_DEFERS} tries: {why}", p.x, p.y, p.z);
        return;
    }
    st.deferred.insert(p, ticks + DEFER_TICKS);
    let key = why.split(" (").next().unwrap_or(why).to_string();
    if st.said.insert(key) {
        info!(target: "printer", "{} {} {} left for a while: {why}", p.x, p.y, p.z);
    } else {
        debug!(target: "printer", "{} {} {} left for a while: {why}", p.x, p.y, p.z);
    }
}

// ------------------------------------------------------------------ restocking

/// Fetch the items the layer needs from the controller's supply chests: the ones known to hold
/// them first, then the unknown ones nearest; what each chest held is remembered for the swarm.
pub async fn restock(bot: Client, ctx: Arc<SwarmCtx>, wanted: Vec<(String, i32)>) -> Result<String, String> {
    let items_wanted: Vec<String> = wanted.iter().map(|(i, _)| i.clone()).collect();
    let mut needs: HashMap<String, i32> = wanted.into_iter().collect();
    let (supply, mapart) = ctx.print_chests.lock().clone();
    let mut chests: Vec<BlockPos> = supply;
    for (colour, c) in &mapart {
        // A colour's own chest first for its carpet.
        if items_wanted.iter().any(|i| i.starts_with(colour.as_str())) {
            chests.insert(0, *c);
        } else {
            chests.push(*c);
        }
    }
    chests.dedup();
    if chests.is_empty() {
        return Err(format!("needs {} - the controller's printer has no supply chests (register them with the printer's chest key)", items_wanted.join(", ")));
    }
    let here = bot.position().map_err(|e| e.to_string())?;
    let known = ctx.chest_contents.lock().clone();
    // Known to hold a wanted item, nearest first; then never looked in; never the ones known not to.
    let holds = |c: &BlockPos| known.get(c).map(|m| items_wanted.iter().any(|i| m.get(i).copied().unwrap_or(0) > 0));
    let dist = |c: &BlockPos| Vec3::new(c.x as f64, c.y as f64, c.z as f64).distance_to(here);
    let mut order: Vec<BlockPos> = chests.iter().copied().filter(|c| holds(c) == Some(true)).collect();
    order.sort_by(|a, b| dist(a).total_cmp(&dist(b)));
    let mut unknown: Vec<BlockPos> = chests.iter().copied().filter(|c| holds(c).is_none()).collect();
    unknown.sort_by(|a, b| dist(a).total_cmp(&dist(b)));
    order.extend(unknown);
    if order.is_empty() {
        return Err(format!("none of the {} supply chests has {}", chests.len(), items_wanted.join(", ")));
    }
    let mut got_total = 0;
    for chest in order.into_iter().take(4) {
        let before: HashMap<&'static str, i32> = count_bags(&bot);
        let (got, contents) = crate::chests::take_amounts(&bot, chest, &needs).await?;
        ctx.chest_contents.lock().insert(chest, contents);
        got_total += got;
        let after = count_bags(&bot);
        needs.retain(|i, n| {
            *n -= after.get(i.as_str()).copied().unwrap_or(0) - before.get(i.as_str()).copied().unwrap_or(0);
            *n > 0
        });
        if needs.is_empty() {
            break;
        }
    }
    if got_total == 0 {
        return Err(format!("the supply chests had none of {}", items_wanted.join(", ")));
    }
    let mut still: Vec<String> = needs.into_keys().collect();
    still.sort();
    Ok(format!("took {got_total} stack(s) for the print{}", if still.is_empty() { String::new() } else { format!(" (still short of {})", still.join(", ")) }))
}

fn count_bags(bot: &Client) -> HashMap<&'static str, i32> {
    let mut m = HashMap::new();
    for (_, s) in inv::bags(bot) {
        *m.entry(inv::stack_id(&s)).or_default() += s.count();
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::schem::parse_state;

    #[test]
    fn state_fixes_count_like_the_standalone() {
        let s = |t: &str| parse_state(t).unwrap();
        assert_eq!(clicks_needed(s("repeater[delay=1]"), s("repeater[delay=3]")), 2);
        assert_eq!(clicks_needed(s("repeater[delay=4]"), s("repeater[delay=1]")), 1);
        assert_eq!(clicks_needed(s("comparator[mode=compare]"), s("comparator[mode=subtract]")), 1);
        assert_eq!(clicks_needed(s("oak_door[open=false]"), s("oak_door[open=true]")), 1);
        assert_eq!(clicks_needed(s("iron_door[open=false]"), s("iron_door[open=true]")), 0);
        assert_eq!(clicks_needed(s("lever[powered=false]"), s("lever[powered=true]")), 1);
        assert_eq!(clicks_needed(s("stone_button[powered=false]"), s("stone_button[powered=true]")), 0);
        assert_eq!(clicks_needed(s("note_block[note=20]"), s("note_block[note=2]")), 7);
        assert!(count_short(s("candle[candles=1]"), s("candle[candles=3]")));
        assert!(!count_short(s("candle[candles=3]"), s("candle[candles=3]")));
    }

    #[test]
    fn reads_the_controllers_printer_sync() {
        let line = "PRINTER_SYNC|true|3|8|20|4.0|12.0|2|5|true|true|true|false|true|100|60|-50|131|90|-43|";
        let (region, strict) = region_from_sync(line).unwrap();
        assert!(strict);
        assert_eq!(region, Some(Box3::new(BlockPos::new(100, 60, -50), BlockPos::new(131, 90, -43))));
        let (none, _) = region_from_sync("PRINTER_SYNC|true|3|8|20|4.0|12.0|2|5|true|false|true|false|false|0|0|0|0|0|0|").unwrap();
        assert!(none.is_none());
    }
}
