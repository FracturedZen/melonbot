//! The nuker (FastNuker, module "fast-nuker"): clear a box, or everything round the bot.
//!
//! - **ClearBox**: the box from the controller (its NUKER_SYNC gives each bot its own strip,
//!   split by the controller) or from the panel (one box for the whole swarm). Either way the box
//!   is cut into 8x8 columns ("cells"); a bot claims the nearest free one, clears it top down, then
//!   claims the next - no two bots on one cell (by region, no overlap), and a bot that
//!   finishes early takes cells the others have not reached.
//! - **Sphere**: everything targetable in reach, wherever the bot is (follow the controller and
//!   dig with it).
//!
//! Breaking: an instant block (with the best tool) is START+STOP, several a tick; a slower one is
//! START, then STOP once the progress passes stop-progress (the server accepts 0.7) - one at a
//! time, as the server keeps one. The best tool in the bags is taken for each block (auto-tool),
//! never one under min-durability. Bags are left alone unless "collect" is on.

use std::collections::{HashMap, HashSet};

use azalea::{
    BlockPos, Client, Vec3,
    block::BlockState,
    entity::{ActiveEffects, Attributes, FluidOnEyes, Physics, mining::get_mine_progress},
    inventory::ItemStack,
    registry::builtin::BlockKind,
};
use tracing::{debug, info};

use super::{BotLink, SwarmCtx};
use crate::{chests, inv, modules::ModuleConfig, movement};

pub const MODULE: &str = "fast-nuker";
/// Columns a cell spans (x and z).
pub const CELL: i32 = 8;
/// Server reach, eye to block centre (4.5 plus its slack, less a margin).
const REACH: f64 = 4.5;
/// A break that did not show in the world is tried again after this many ticks.
const RETRY_TICKS: u64 = 20;
/// Rescan the work area this often (ticks).
const SCAN_TICKS: u64 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Box3 {
    pub min: BlockPos,
    pub max: BlockPos,
}

impl Box3 {
    pub fn new(a: BlockPos, b: BlockPos) -> Self {
        Self {
            min: BlockPos::new(a.x.min(b.x), a.y.min(b.y), a.z.min(b.z)),
            max: BlockPos::new(a.x.max(b.x), a.y.max(b.y), a.z.max(b.z)),
        }
    }

    pub fn volume(&self) -> i64 {
        (self.max.x - self.min.x + 1) as i64 * (self.max.y - self.min.y + 1) as i64 * (self.max.z - self.min.z + 1) as i64
    }

    /// The cells (their lowest x/z corner) covering the box.
    pub fn cells(&self) -> Vec<(i32, i32)> {
        let mut out = Vec::new();
        let mut x = self.min.x;
        while x <= self.max.x {
            let mut z = self.min.z;
            while z <= self.max.z {
                out.push((x, z));
                z += CELL;
            }
            x += CELL;
        }
        out
    }

    /// A cell's columns, clipped to the box: (min, max) inclusive, all heights of the box.
    pub fn cell_box(&self, c: (i32, i32)) -> Box3 {
        Box3 {
            min: BlockPos::new(c.0, self.min.y, c.1),
            max: BlockPos::new((c.0 + CELL - 1).min(self.max.x), self.max.y, (c.1 + CELL - 1).min(self.max.z)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Sphere,
    ClearBox,
}

/// The nuker's settings, in FastNuker's names.
#[derive(Clone, Debug)]
pub struct Cfg {
    pub mode: Mode,
    pub whitelist: bool,
    /// Block ids without the namespace ("stone").
    pub blocks: HashSet<String>,
    /// Sphere reach (capped at the server's).
    pub range: f64,
    pub max_breaks: usize,
    pub stop_progress: f64,
    pub auto_tool: bool,
    pub min_durability: i32,
    pub protect_containers: bool,
    pub auto_move: bool,
    pub area: Option<Box3>,
    /// Empty the bags into a storage chest when they fill.
    pub collect: bool,
    /// Where it came from, for the panel.
    pub source: &'static str,
}

fn strip_ns(s: &str) -> String {
    s.trim().strip_prefix("minecraft:").unwrap_or(s.trim()).to_string()
}

impl Cfg {
    /// From the module's settings (the swarm's, merged with the bot's own) - None when it is off.
    pub fn from_settings(m: &ModuleConfig) -> Option<Self> {
        if !m.enabled(MODULE) {
            return None;
        }
        let g = |k: &str| m.get::<i64>(MODULE, k, i64::MIN);
        let area = {
            let (x1, y1, z1, x2, y2, z2) = (g("x1"), g("y1"), g("z1"), g("x2"), g("y2"), g("z2"));
            if [x1, y1, z1, x2, y2, z2].contains(&i64::MIN) {
                None
            } else {
                Some(Box3::new(BlockPos::new(x1 as i32, y1 as i32, z1 as i32), BlockPos::new(x2 as i32, y2 as i32, z2 as i32)))
            }
        };
        Some(Self {
            mode: if m.get::<String>(MODULE, "target-mode", "ClearBox".into()) == "Sphere" { Mode::Sphere } else { Mode::ClearBox },
            whitelist: m.get::<String>(MODULE, "list-mode", "Blacklist".into()) == "Whitelist",
            blocks: m.get::<Vec<String>>(MODULE, "blocks", Vec::new()).iter().map(|s| strip_ns(s)).collect(),
            range: m.get::<f64>(MODULE, "range", REACH).min(REACH),
            max_breaks: m.get::<usize>(MODULE, "max-breaks-per-tick", 5).clamp(1, 20),
            stop_progress: m.get::<f64>(MODULE, "stop-progress", 0.88).clamp(0.7, 1.0),
            auto_tool: m.get(MODULE, "auto-tool", true),
            min_durability: m.get(MODULE, "min-durability", 10),
            protect_containers: m.get(MODULE, "protect-containers", true),
            auto_move: m.get(MODULE, "auto-move", true),
            area,
            collect: m.get(MODULE, "collect", false),
            source: "panel",
        })
    }

    /// The controller's NUKER_SYNC (the controller's FastNuker, with this bot's strip as the box).
    pub fn from_sync(raw: &str) -> Option<Self> {
        let p: Vec<&str> = raw.split('|').collect();
        if p.first() != Some(&"NUKER_SYNC") || p.len() < 27 {
            return None;
        }
        let (mode, o) = match p[1] {
            "Sphere" => (Mode::Sphere, 1),
            "ClearBox" => (Mode::ClearBox, 1),
            _ => (Mode::ClearBox, 0),
        };
        if p.len() < 27 + o {
            return None;
        }
        let f = |i: usize| p.get(i).and_then(|s| s.trim().parse::<f64>().ok());
        let b = |i: usize| p.get(i).is_some_and(|s| s.trim().eq_ignore_ascii_case("true"));
        let box_base = if o == 1 && p.len() >= 29 { 22 + o } else { 21 + o };
        let i = |k: usize| f(box_base + k).map(|v| v as i32);
        let area = match (i(0), i(1), i(2), i(3), i(4), i(5)) {
            (Some(x1), Some(y1), Some(z1), Some(x2), Some(y2), Some(z2)) if mode == Mode::ClearBox => {
                Some(Box3::new(BlockPos::new(x1, y1, z1), BlockPos::new(x2, y2, z2)))
            }
            _ => None,
        };
        let blocks = p.get(box_base + 6).map(|s| s.split(',').filter(|b| !b.trim().is_empty()).map(strip_ns).collect()).unwrap_or_default();
        Some(Self {
            mode,
            whitelist: p[1 + o].trim() == "Whitelist",
            blocks,
            range: f(2 + o).unwrap_or(REACH).min(REACH),
            max_breaks: f(8 + o).map_or(5, |v| (v as usize).clamp(1, 20)),
            stop_progress: f(14 + o).unwrap_or(0.88).clamp(0.7, 1.0),
            auto_tool: b(4 + o),
            min_durability: f(5 + o).map_or(10, |v| v as i32),
            protect_containers: true,
            auto_move: b(16 + o),
            area,
            collect: false,
            source: "controller",
        })
    }

    /// A block this nuker breaks.
    pub fn targets(&self, state: BlockState) -> bool {
        if state.is_air() {
            return false;
        }
        let kind = BlockKind::from(state);
        if matches!(kind, BlockKind::Water | BlockKind::Lava | BlockKind::Bedrock | BlockKind::Barrier | BlockKind::BubbleColumn) {
            return false;
        }
        // "stone", as the list has it (to_str is "minecraft:stone" - a whitelist never matched).
        let id = kind.to_str();
        let id = id.strip_prefix("minecraft:").unwrap_or(id);
        if id.contains("portal") || id.ends_with("command_block") || id == "end_portal_frame" || id == "structure_block" || id == "jigsaw" {
            return false;
        }
        if self.protect_containers && chests::is_container(kind) {
            return false;
        }
        let listed = self.blocks.contains(id);
        if self.whitelist { listed } else { !listed }
    }
}

/// The swarm's cell claims, per box.
#[derive(Default)]
pub struct Board {
    claims: HashMap<(Box3, (i32, i32)), (String, std::time::Instant)>,
    done: HashSet<(Box3, (i32, i32))>,
    /// Boxes finished, and when - said once.
    finished: HashSet<Box3>,
}

impl Board {
    /// The cell this bot works in: its claim if still open, else the nearest free one to `near`.
    pub fn claim(&mut self, area: Box3, bot: &str, near: Vec3) -> Option<(i32, i32)> {
        let now = std::time::Instant::now();
        // A claim not refreshed for 30 s belongs to a bot that stopped or left.
        self.claims.retain(|_, (_, at)| now.duration_since(*at).as_secs() < 30);
        if let Some((k, v)) = self.claims.iter_mut().find(|(k, (b, _))| k.0 == area && b == bot && !self.done.contains(k)) {
            v.1 = now;
            return Some(k.1);
        }
        let cell = area
            .cells()
            .into_iter()
            .filter(|c| !self.done.contains(&(area, *c)) && !self.claims.contains_key(&(area, *c)))
            .min_by(|a, b| {
                let ca = Vec3::new(a.0 as f64 + 4.0, near.y, a.1 as f64 + 4.0);
                let cb = Vec3::new(b.0 as f64 + 4.0, near.y, b.1 as f64 + 4.0);
                ca.distance_to(near).total_cmp(&cb.distance_to(near))
            })?;
        self.claims.insert((area, cell), (bot.to_string(), now));
        Some(cell)
    }

    pub fn mark_done(&mut self, area: Box3, cell: (i32, i32)) {
        self.claims.remove(&(area, cell));
        self.done.insert((area, cell));
    }

    pub fn release(&mut self, bot: &str) {
        self.claims.retain(|_, (b, _)| b != bot);
    }

    /// Cells done, over every box (the printer's strips are a box each).
    pub fn done_total(&self) -> usize {
        self.done.len()
    }

    /// Cells done / all, for the panel.
    pub fn progress(&self, area: Box3) -> (usize, usize) {
        (area.cells().iter().filter(|c| self.done.contains(&(area, **c))).count(), area.cells().len())
    }

    /// True once, when the last cell of `area` is done.
    pub fn just_finished(&mut self, area: Box3) -> bool {
        let (d, all) = self.progress(area);
        d == all && self.finished.insert(area)
    }

    /// A new box from the panel: start it clean.
    pub fn reset(&mut self) {
        self.claims.clear();
        self.done.clear();
        self.finished.clear();
    }
}

/// One bot's nuker state.
#[derive(Default)]
pub struct BotNuker {
    /// A slow block being broken: where, and the tick its STOP goes.
    mining: Option<(BlockPos, u64)>,
    /// Breaks sent, and when they may be tried again.
    fired: HashMap<BlockPos, u64>,
    /// Targets from the last scan, best first.
    queue: Vec<BlockPos>,
    last_scan: u64,
    tool_ready_at: u64,
    pub status: String,
    pub broken: u64,
    /// The cell worked in.
    pub cell: Option<(i32, i32)>,
}

/// What the nuker wants this tick.
pub enum Want {
    /// Leave movement to the order.
    Nothing,
    /// It is moving the bot (box work).
    Moving,
    /// Needs tools from a chest.
    Tools,
    /// Bags full while collecting.
    Store,
}

/// One tick of the nuker for this bot.
pub fn tick(bot: &Client, ctx: &SwarmCtx, link: &BotLink, cfg: &Cfg, ticks: u64) -> Want {
    let Ok(pos) = bot.position() else { return Want::Nothing };
    let eye = pos + Vec3::new(0.0, 1.62, 0.0);
    let Ok(world) = bot.world() else { return Want::Nothing };
    let mut st = link.nuker.lock();
    st.fired.retain(|_, until| *until > ticks);

    if cfg.collect && inv::empty_slots(bot) == 0 {
        st.status = "bags full - storing".into();
        return Want::Store;
    }

    // A slow block under way: STOP when it is due - the server breaks it then (its progress is
    // past 0.7) - and start the next one in the same tick, as FastNuker's queue does. No waiting
    // for the block to vanish: one that is still there when its retry time comes is found again
    // by the next scan.
    if let Some((p, stop_at)) = st.mining {
        if ticks < stop_at {
            super::eclink::touch_held(bot, link, ticks);
            st.status = format!("breaking {} {} {}", p.x, p.y, p.z);
            return if cfg.auto_move && cfg.mode == Mode::ClearBox { Want::Moving } else { Want::Nothing };
        }
        crate::modules::harvest::send_action(bot, p, false);
        debug!(target: "nuker", "STOP {} {} {}", p.x, p.y, p.z);
        st.mining = None;
        st.broken += 1;
    }

    // The work area: this bot's cell of the box, or everything in reach.
    let area_cell = match (cfg.mode, cfg.area) {
        (Mode::ClearBox, Some(area)) => {
            let cell = {
                let mut board = ctx.nuker_board.lock();
                board.claim(area, &link.name, pos)
            };
            st.cell = cell;
            match cell {
                Some(c) => Some((area, area.cell_box(c), c)),
                None => {
                    let mut board = ctx.nuker_board.lock();
                    if board.just_finished(area) {
                        info!(target: "nuker", "box {:?} - {:?} cleared", area.min, area.max);
                        ctx.feed("event", &format!("the nuker box is cleared ({} cells)", area.cells().len()));
                    }
                    st.status = "box cleared".into();
                    return Want::Nothing;
                }
            }
        }
        (Mode::ClearBox, None) => {
            st.status = "no box set".into();
            return Want::Nothing;
        }
        (Mode::Sphere, _) => None,
    };

    if ticks.saturating_sub(st.last_scan) >= SCAN_TICKS || st.queue.is_empty() {
        st.last_scan = ticks;
        let w = world.read();
        let mut found: Vec<BlockPos> = Vec::new();
        let mut unloaded = false;
        match &area_cell {
            Some((_, cell_box, _)) => {
                for x in cell_box.min.x..=cell_box.max.x {
                    for z in cell_box.min.z..=cell_box.max.z {
                        for y in cell_box.min.y..=cell_box.max.y {
                            let p = BlockPos::new(x, y, z);
                            match w.get_block_state(p) {
                                Some(s) if cfg.targets(s) => found.push(p),
                                Some(_) => {}
                                None => unloaded = true,
                            }
                        }
                    }
                }
            }
            None => {
                let r = cfg.range.ceil() as i32;
                let c = BlockPos::new(eye.x.floor() as i32, eye.y.floor() as i32, eye.z.floor() as i32);
                for dx in -r..=r {
                    for dy in -r..=r {
                        for dz in -r..=r {
                            let p = BlockPos::new(c.x + dx, c.y + dy, c.z + dz);
                            if let Some(s) = w.get_block_state(p)
                                && cfg.targets(s)
                                && eye.distance_to(centre(p)) <= cfg.range
                            {
                                found.push(p);
                            }
                        }
                    }
                }
            }
        }
        drop(w);
        // Top down (sand and gravel fall into what is already cleared), nearest first.
        found.sort_by(|a, b| b.y.cmp(&a.y).then(eye.distance_to(centre(*a)).total_cmp(&eye.distance_to(centre(*b)))));
        if let Some((area, _, cell)) = &area_cell
            && found.is_empty()
            && !unloaded
        {
            debug!(target: "nuker", "cell {cell:?} done");
            ctx.nuker_board.lock().mark_done(*area, *cell);
            st.cell = None;
            st.queue.clear();
            st.status = "cell done".into();
            return Want::Moving;
        }
        st.queue = found;
    }

    if ticks < st.tool_ready_at {
        return Want::Moving;
    }

    // Break what is in reach: the top layer first. Box work reaches as far as the server allows;
    // a sphere as far as its range.
    let reach = if area_cell.is_some() { REACH } else { cfg.range.clamp(1.0, REACH) };
    let in_reach: Vec<BlockPos> = st
        .queue
        .iter()
        .filter(|p| !st.fired.contains_key(*p) && eye.distance_to(centre(**p)) <= reach)
        .copied()
        .take(64)
        .collect();
    let mut sent = 0;
    let mut need_tools = false;
    for p in in_reach {
        if sent >= cfg.max_breaks {
            break;
        }
        let Some(state) = world.read().get_block_state(p) else { continue };
        if !cfg.targets(state) {
            continue;
        }
        let (slot, per_tick) = best_tool(bot, state, cfg);
        // A faster tool in the ender chest (eclink.rs): borrowed into the hand, and the break
        // waits for the swap to land - also when a loan in the hand had to go back first (no slow
        // break meanwhile, and no trip to a tools chest that would break the link).
        if cfg.auto_tool
            && let Some((ec_slot, s)) = best_in_ender_chest(bot, ctx, link, state, cfg)
            && s > per_tick * 1.05
        {
            if super::eclink::borrow(bot, link, ec_slot, ctx.cfg.read().ec_panic_health, ticks) {
                st.tool_ready_at = ticks + 3;
            }
            break;
        }
        if per_tick <= 0.0 {
            st.fired.insert(p, ticks + RETRY_TICKS * 10);
            continue;
        }
        // A tool to take in hand first: the break waits for the swap to land.
        if cfg.auto_tool
            && let Some(slot) = slot
            && bot.selected_hotbar_slot().map(|h| h as usize + 36).ok() != Some(slot)
        {
            inv::hold(bot, slot, &super::eclink::loan_hotbars(link));
            st.tool_ready_at = ticks + 3;
            break;
        }
        // Even the best it has is slow (stone by hand or with a shovel: 7.5 s a block): a tools
        // chest nearby may have the right tool. Asked for at most every 5 minutes.
        if per_tick < 0.1 {
            need_tools = true;
        }
        if per_tick >= 1.0 {
            crate::modules::harvest::send_break(bot, p);
            st.fired.insert(p, ticks + RETRY_TICKS);
            st.broken += 1;
            sent += 1;
        } else {
            if sent > 0 {
                break;
            }
            // START now, STOP once the progress passes stop-progress.
            crate::modules::harvest::send_action(bot, p, true);
            let wait = (cfg.stop_progress / per_tick as f64).ceil() as u64;
            debug!(target: "nuker", "START {} {} {} perTick {per_tick:.3} -> STOP in {wait} ticks, holding {}", p.x, p.y, p.z, inv::stack_id(&bot.get_held_item().unwrap_or_default()));
            st.mining = Some((p, ticks + wait));
            st.fired.insert(p, ticks + wait + RETRY_TICKS);
            sent += 1;
            break;
        }
    }
    if sent > 0 {
        super::eclink::touch_held(bot, link, ticks);
    }
    st.status = match &area_cell {
        Some((_, _, c)) => format!("cell {} {} ({} left)", c.0, c.1, st.queue.len()),
        None => format!("sphere ({} in reach)", st.queue.len()),
    };

    // Box work moves the bot: over the highest block left in its cell.
    let moving = if let Some((_, cell_box, _)) = &area_cell
        && cfg.auto_move
    {
        let next = st.queue.iter().find(|p| !st.fired.contains_key(*p)).or(st.queue.first()).copied();
        let spot = match next {
            Some(p) => Vec3::new(p.x as f64 + 0.5, p.y as f64 + 1.0, p.z as f64 + 0.5),
            // The cell's chunk not loaded yet: fly over its middle to load it.
            None => Vec3::new((cell_box.min.x + cell_box.max.x) as f64 / 2.0 + 0.5, cell_box.max.y as f64 + 2.0, (cell_box.min.z + cell_box.max.z) as f64 / 2.0 + 0.5),
        };
        if sent == 0 && eye.distance_to(spot + Vec3::new(0.0, 0.5, 0.0)) > 1.0 {
            movement::steer(bot, spot, "nuker", 0.5);
        }
        true
    } else {
        false
    };
    if need_tools {
        return Want::Tools;
    }
    if moving { Want::Moving } else { Want::Nothing }
}

fn centre(p: BlockPos) -> Vec3 {
    Vec3::new(p.x as f64 + 0.5, p.y as f64 + 0.5, p.z as f64 + 0.5)
}

/// The fastest tool for `state` (the printer clearing a wrong block): auto-tool on, tools with
/// fewer than 10 uses left kept back.
pub fn best_tool_for(bot: &Client, state: BlockState) -> (Option<usize>, f32) {
    best_tool_keeping(bot, state, 10)
}

/// As best_tool_for, keeping back tools with fewer than `keep` uses left (the perimeter bots send
/// theirs to be repaired before they break).
pub fn best_tool_keeping(bot: &Client, state: BlockState, keep: i32) -> (Option<usize>, f32) {
    let cfg = Cfg {
        mode: Mode::Sphere,
        whitelist: false,
        blocks: HashSet::new(),
        range: REACH,
        max_breaks: 1,
        stop_progress: 0.9,
        auto_tool: true,
        min_durability: keep,
        protect_containers: true,
        auto_move: false,
        area: None,
        collect: false,
        source: "tools",
    };
    best_tool(bot, state, &cfg)
}

/// The fastest way to break `state` with what is in the bags: (the tool's menu slot, or None for
/// the held item / bare hand; progress per tick). Tools under min-durability are not used.
fn best_tool(bot: &Client, state: BlockState, cfg: &Cfg) -> (Option<usize>, f32) {
    let held = bot.get_held_item().unwrap_or_default();
    let held_slot = bot.selected_hotbar_slot().map(|h| h as usize + 36).ok();
    let speed = |item: &ItemStack| speed_with(bot, state, item);
    let held_ok = inv::uses_left(&held).is_none_or(|u| u >= cfg.min_durability);
    let mut best: (Option<usize>, f32) = (None, if held_ok { speed(&held) } else { speed(&ItemStack::Empty) });
    if !cfg.auto_tool {
        return best;
    }
    for (slot, st) in inv::bags(bot) {
        if !super::errand::is_digging_tool(&st) || Some(slot) == held_slot {
            continue;
        }
        if inv::uses_left(&st).is_some_and(|u| u < cfg.min_durability) {
            continue;
        }
        let s = speed(&st);
        if s > best.1 * 1.05 {
            best = (Some(slot), s);
        }
    }
    best
}

/// Mining progress a tick for `state` with `item` in hand (standing).
fn speed_with(bot: &Client, state: BlockState, item: &ItemStack) -> f32 {
    bot.query_self::<(&FluidOnEyes, &Physics, &Attributes, &ActiveEffects), _>(|(f, p, a, e)| {
        let mut ground = p.clone();
        ground.set_on_ground(true);
        get_mine_progress(state.to_trait(), item, f, &ground, a, e)
    })
    .unwrap_or(0.0)
}

/// The fastest digging tool for `state` in the linked ender chest: (its chest slot, progress a
/// tick). None when not linked, the link is off, or the health is too low to borrow.
fn best_in_ender_chest(bot: &Client, ctx: &SwarmCtx, link: &BotLink, state: BlockState, cfg: &Cfg) -> Option<(usize, f32)> {
    let (on, panic) = {
        let c = ctx.cfg.read();
        (c.ec_link, c.ec_panic_health)
    };
    if !on || !super::eclink::healthy_enough(bot, panic) {
        return None;
    }
    super::eclink::contents(bot, link)
        .into_iter()
        .filter(|(_, st)| super::errand::is_digging_tool(st) && inv::uses_left(st).is_none_or(|u| u >= cfg.min_durability))
        .map(|(slot, st)| (slot, speed_with(bot, state, &st)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_cover_the_box_without_overlap() {
        let b = Box3::new(BlockPos::new(10, 60, -5), BlockPos::new(30, 70, 9));
        let cells = b.cells();
        assert_eq!(cells.len(), 3 * 2);
        let mut seen = HashSet::new();
        for c in &cells {
            let cb = b.cell_box(*c);
            for x in cb.min.x..=cb.max.x {
                for z in cb.min.z..=cb.max.z {
                    assert!(seen.insert((x, z)), "column {x} {z} in two cells");
                }
            }
        }
        assert_eq!(seen.len(), 21 * 15);
    }

    #[test]
    fn claims_never_share_a_cell_and_free_up() {
        let b = Box3::new(BlockPos::new(0, 0, 0), BlockPos::new(15, 5, 7));
        let mut board = Board::default();
        let a = board.claim(b, "A", Vec3::ZERO).unwrap();
        let c = board.claim(b, "B", Vec3::ZERO).unwrap();
        assert_ne!(a, c);
        assert_eq!(board.claim(b, "A", Vec3::ZERO), Some(a), "a bot keeps its cell");
        assert_eq!(board.claim(b, "C", Vec3::ZERO), None, "two cells, two bots");
        board.mark_done(b, a);
        assert_eq!(board.claim(b, "A", Vec3::ZERO), None);
        board.release("B");
        assert_eq!(board.claim(b, "C", Vec3::ZERO), Some(c));
        board.mark_done(b, c);
        assert!(board.just_finished(b));
        assert!(!board.just_finished(b), "said once");
    }

    #[test]
    fn reads_the_java_controllers_nuker_sync() {
        let line = "NUKER_SYNC|ClearBox|Whitelist|5.0|Closest|true|10|false|false|5|15|2|true|true|true|0.88|true|true|4.0|false|1.0|8|true|100|60|-50|107|70|-43|minecraft:stone,minecraft:dirt|false|3";
        let c = Cfg::from_sync(line).unwrap();
        assert_eq!(c.mode, Mode::ClearBox);
        assert!(c.whitelist && c.auto_move && c.auto_tool);
        assert_eq!(c.area, Some(Box3::new(BlockPos::new(100, 60, -50), BlockPos::new(107, 70, -43))));
        assert!(c.blocks.contains("stone") && c.blocks.contains("dirt"));
        assert!((c.stop_progress - 0.88).abs() < 1e-9);
        let sphere = Cfg::from_sync("NUKER_SYNC|Sphere|Blacklist|4.5|Closest|true|10|false|false|5|15|2|true|true|true|0.88|true|false|4.0|false|1.0|8|true|0|0|0|0|0|0|").unwrap();
        assert_eq!((sphere.mode, sphere.area), (Mode::Sphere, None));
    }
}
