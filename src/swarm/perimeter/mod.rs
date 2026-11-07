//! The perimeter bots: clear a box (800x800, say) from build height down to bedrock, top down,
//! sealing water and lava as they go: water and lava sources inside the box get a block and are
//! dug out, ones outside it are walled off so nothing flows in.
//!
//! The botswarm's runtime in a process of its own (`melonbot perimeter`, start-perimeter.cmd): its
//! own folder (perimeter/), its own panel (http://127.0.0.1:7100/), the botswarm's accounts
//! (swarm/bots/) - an account runs in one or the other, never both (in-use.json, swarm/mod.rs).
//!
//! How the box is cut:
//! - **Cells and bands.** The box is cut into 8x8 columns ("cells"); a bot claims one and cuts it a
//!   band (4 blocks) at a time from the top. A cell's floor is how far down it is clear. Air is
//!   skipped without a visit (dig::fast_forward), so only real ground costs time.
//! - **Level steps.** A cell starts its next band only when no neighbour is higher than it, and
//!   never more than `spread` bands below the highest ground left anywhere: the highest ground goes
//!   first, the floor stays nearly flat, and nothing is ever left above a bot. The box is scouted
//!   (every cell's ground found) before any digging, so the highest ground is known.
//! - **Fluids.** A block with water or lava beside it or above it is never broken: the fluid gets
//!   a block first (dig::plan). Inside the box that block is dug out later like any other; outside
//!   the box it stays - the edge wall, only where fluid touches.
//! - **Fill** is what they dig (cobblestone, dirt, deepslate...), held in the offhand. The rest of
//!   what they dig is left lying.
//! - **Tools** from the chest signed "tools" by the box, worn ones into the one signed "repair" (as
//!   the melon farmer does), food from the one signed "food" - recorded by each bot where it is logged
//!   in (base.rs).
//! - Players are ignored: they keep digging.
//!
//! Files: perimeter/job.json (the box and how it is worked), progress.json (every cell's floor),
//! accounts.json (which accounts this launcher opens), bases.json (each bot's chests).

pub mod base;
pub mod control;
pub mod dig;
pub mod guard;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use azalea::{BlockPos, Client, Vec3};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{BotLink, SwarmCtx, nuker::Box3};
use crate::{bot::Shared, movement};

/// Columns a cell spans (x and z).
pub const CELL: i32 = 8;
/// A claim not refreshed for this long belongs to a bot that stopped or left.
const CLAIM_SECS: u64 = 30;
/// progress.json is written at most this often while cells change.
const SAVE_SECS: u64 = 10;
/// Blocks of one side of the box the server would not let go: the whole side is left standing.
const SIDE_REFUSALS: usize = 3;

/// What goes into water and lava, best first: full blocks a pickaxe or shovel digs up that neither
/// fall (sand and gravel sink through water) nor burn.
pub const FILL: &[&str] = &[
    "cobblestone", "cobbled_deepslate", "netherrack", "dirt", "coarse_dirt", "rooted_dirt", "stone", "deepslate", "andesite", "diorite", "granite",
    "tuff", "calcite", "blackstone", "basalt", "smooth_basalt", "end_stone", "sandstone", "red_sandstone", "mossy_cobblestone", "dripstone_block",
    "terracotta", "packed_mud", "mud",
];

/// perimeter/job.json: the box and how it is worked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobConfig {
    /// Work it (Pause on the panel holds every bot where it is).
    pub running: bool,
    pub x1: i32,
    pub z1: i32,
    pub x2: i32,
    pub z2: i32,
    /// The highest and lowest layers cleared (inclusive). None: the world's build height and its
    /// bottom (bedrock is left - nothing breaks it).
    pub top: Option<i32>,
    pub bottom: Option<i32>,
    /// Layers a band is deep.
    pub band: i32,
    /// No cell goes more than this many bands below the highest ground left (0: only the
    /// neighbour rule).
    pub spread: i32,
    /// What goes into water and lava, best first (ids without "minecraft:").
    pub fill: Vec<String>,
    /// Stacks of fill kept when room has to be made; everything else dug is thrown out then.
    pub keep_fill_stacks: usize,
    pub max_breaks_per_tick: usize,
    pub places_per_tick: usize,
    /// A slow block's STOP goes once its progress passes this (the server accepts 0.7).
    pub stop_progress: f64,
    /// A tool with this many uses left or fewer is worn: not used, and goes into the repair chest.
    pub retire_uses: i32,
    /// Chests, barrels and shulkers in the box are left standing (off: broken like the rest).
    pub protect_containers: bool,
    pub tools_sign: String,
    pub repair_sign: String,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            running: false,
            // A placeholder: set the box on the panel (or in job.json), then press Run.
            x1: 0,
            z1: 0,
            x2: 799,
            z2: 799,
            top: None,
            bottom: None,
            band: 4,
            spread: 2,
            fill: FILL.iter().map(|s| s.to_string()).collect(),
            keep_fill_stacks: 4,
            max_breaks_per_tick: 5,
            places_per_tick: 2,
            stop_progress: 0.88,
            retire_uses: 30,
            protect_containers: false,
            tools_sign: "tools".into(),
            repair_sign: "repair".into(),
        }
    }
}

impl JobConfig {
    /// The box on a world `bottom`..=`top` high.
    pub fn area(&self, world: (i32, i32)) -> Box3 {
        let (wb, wt) = world;
        let top = self.top.unwrap_or(wt).clamp(wb, wt);
        let bottom = self.bottom.unwrap_or(wb).clamp(wb, top);
        Box3::new(BlockPos::new(self.x1, bottom, self.z1), BlockPos::new(self.x2, top, self.z2))
    }

    pub fn is_fill(&self, id: &str) -> bool {
        self.fill.iter().any(|f| f == id)
    }
}

// ------------------------------------------------------------------ the board

/// What the server will not let go - it puts it back every time (WorldGuard, the world border; 4
/// Nov: two bots spent 8 hours breaking the same 7 blocks of the box's west side, which
/// WorldGuard guards). Left standing: those blocks, and a whole side of the box once SIDE_REFUSALS
/// of its blocks were (a side next to one left counts as a side too - a guarded strip wider than 1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Refused {
    pub blocks: BTreeSet<(i32, i32, i32)>,
    /// The sides left whole: their x (west, east) or z (north, south).
    pub xs: BTreeSet<i32>,
    pub zs: BTreeSet<i32>,
}

impl Refused {
    pub fn has(&self, p: BlockPos) -> bool {
        self.xs.contains(&p.x) || self.zs.contains(&p.z) || self.blocks.contains(&(p.x, p.y, p.z))
    }
}

/// What a bot should do next, from the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// Cut this cell's next band.
    Dig((i32, i32)),
    /// Fly over this cell so its ground can be read.
    Scout((i32, i32)),
    Wait(String),
    Done,
}

/// The cells' progress. A cell's floor: everything from it up to the top of the box is clear -
/// nothing to break, no water or lava. Floors sit on band edges (top + 1 - k * band), or on the
/// bottom once the cell is done. A cell whose ground has not been read yet has no floor (unknown).
pub struct Board {
    /// The box the floors belong to (on the world's height); None until a bot has seen the world.
    pub area: Option<Box3>,
    pub band: i32,
    /// The world's bottom and top, as a bot last saw them.
    pub world: Option<(i32, i32)>,
    floors: HashMap<(i32, i32), i32>,
    claims: HashMap<(i32, i32), (String, Instant)>,
    /// Cells put off until then (no fill to seal them with).
    parked: HashMap<(i32, i32), Instant>,
    pub broken: u64,
    pub placed: u64,
    /// Of those placed, the ones outside the box: the edge wall.
    pub walled: u64,
    /// What the server will not let go: left standing.
    pub refused: Refused,
    dirty: bool,
    saved_at: Option<Instant>,
    /// The box was cleared (said once).
    finished: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct BoardFile {
    area: Option<[i32; 6]>,
    band: i32,
    world: Option<(i32, i32)>,
    floors: Vec<(i32, i32, i32)>,
    broken: u64,
    placed: u64,
    walled: u64,
    refused: Refused,
}

impl Board {
    fn empty() -> Self {
        Self {
            area: None,
            band: 4,
            world: None,
            floors: HashMap::new(),
            claims: HashMap::new(),
            parked: HashMap::new(),
            broken: 0,
            placed: 0,
            walled: 0,
            refused: Refused::default(),
            dirty: false,
            saved_at: None,
            finished: false,
        }
    }

    fn load(dir: &Path) -> Self {
        let mut b = Self::empty();
        let Some(f) = fs::read_to_string(dir.join("progress.json")).ok().and_then(|t| serde_json::from_str::<BoardFile>(&t).ok()) else { return b };
        b.area = f.area.map(|a| Box3::new(BlockPos::new(a[0], a[1], a[2]), BlockPos::new(a[3], a[4], a[5])));
        b.band = f.band.max(1);
        b.world = f.world;
        b.floors = f.floors.into_iter().map(|(x, z, fl)| ((x, z), fl)).collect();
        (b.broken, b.placed, b.walled) = (f.broken, f.placed, f.walled);
        b.refused = f.refused;
        b
    }

    fn save(&mut self, dir: &Path) {
        let f = BoardFile {
            area: self.area.map(|a| [a.min.x, a.min.y, a.min.z, a.max.x, a.max.y, a.max.z]),
            band: self.band,
            world: self.world,
            floors: self.floors.iter().map(|((x, z), f)| (*x, *z, *f)).collect(),
            broken: self.broken,
            placed: self.placed,
            walled: self.walled,
            refused: self.refused.clone(),
        };
        match serde_json::to_string(&f) {
            Ok(t) => {
                if let Err(e) = fs::write(dir.join("progress.json"), t) {
                    warn!(target: "perimeter", "could not save progress.json: {e}");
                }
            }
            Err(e) => warn!(target: "perimeter", "could not write the progress: {e}"),
        }
        self.dirty = false;
        self.saved_at = Some(Instant::now());
    }

    /// Written when something changed, at most every SAVE_SECS.
    pub fn save_if_due(&mut self, dir: &Path) {
        if self.dirty && self.saved_at.is_none_or(|t| t.elapsed().as_secs() >= SAVE_SECS) {
            self.save(dir);
        }
    }

    /// The box (and band) to work. Another box than the floors belong to: they start over.
    pub fn set_area(&mut self, area: Box3, band: i32) {
        let band = band.max(1);
        if self.area == Some(area) && self.band == band {
            return;
        }
        if self.area.is_some() {
            info!(target: "perimeter", "new box {:?} - {:?} (band {band}): progress starts over", area.min, area.max);
        }
        self.area = Some(area);
        self.band = band;
        self.refused = Refused::default();
        self.reset();
    }

    /// Every cell unknown again (its ground is read afresh).
    pub fn reset(&mut self) {
        self.floors.clear();
        self.claims.clear();
        self.parked.clear();
        self.finished = false;
        self.dirty = true;
    }

    pub fn top(&self) -> i32 {
        self.area.map_or(0, |a| a.max.y)
    }

    pub fn bottom(&self) -> i32 {
        self.area.map_or(0, |a| a.min.y)
    }

    /// Every cell (its lowest x/z corner).
    pub fn cells(&self) -> Vec<(i32, i32)> {
        let Some(a) = self.area else { return Vec::new() };
        let mut out = Vec::new();
        let mut x = a.min.x;
        while x <= a.max.x {
            let mut z = a.min.z;
            while z <= a.max.z {
                out.push((x, z));
                z += CELL;
            }
            x += CELL;
        }
        out
    }

    /// The cell a column is in, if it is in the box.
    pub fn cell_of(&self, x: i32, z: i32) -> Option<(i32, i32)> {
        let a = self.area?;
        if x < a.min.x || x > a.max.x || z < a.min.z || z > a.max.z {
            return None;
        }
        Some((a.min.x + (x - a.min.x) / CELL * CELL, a.min.z + (z - a.min.z) / CELL * CELL))
    }

    fn neighbours(&self, c: (i32, i32)) -> Vec<(i32, i32)> {
        let Some(a) = self.area else { return Vec::new() };
        [(c.0 - CELL, c.1), (c.0 + CELL, c.1), (c.0, c.1 - CELL), (c.0, c.1 + CELL)]
            .into_iter()
            .filter(|n| n.0 >= a.min.x && n.0 <= a.max.x && n.1 >= a.min.z && n.1 <= a.max.z)
            .collect()
    }

    pub fn floor(&self, c: (i32, i32)) -> Option<i32> {
        self.floors.get(&c).copied()
    }

    pub fn done(&self, c: (i32, i32)) -> bool {
        self.floor(c).is_some_and(|f| f <= self.bottom())
    }

    /// The band edge a layer's band starts above: everything from there up is a band boundary away.
    fn edge_above(&self, y: i32) -> i32 {
        let top = self.top();
        top + 1 - (top - y).max(0) / self.band * self.band
    }

    /// What a read of the cell's ground found below its floor: the highest thing to clear
    /// (Some(y)), or nothing down to the bottom (None). Only ever lowers the floor.
    pub fn found(&mut self, c: (i32, i32), highest: Option<i32>) {
        let f = match highest {
            Some(y) => self.edge_above(y),
            None => self.bottom(),
        };
        let cur = self.floor(c);
        if cur.is_none_or(|cur| f < cur) {
            self.floors.insert(c, f);
            self.dirty = true;
        }
        // A scout's claim ends with its ground known: the cell is dug when the level rules let it,
        // not straight away by whoever scouted it.
        if cur.is_none() {
            self.claims.remove(&c);
        }
    }

    /// Something to clear turned up at `y` in a cell, above its floor (a block placed there, a
    /// fluid that flowed in): the floor goes back up over it.
    pub fn raise(&mut self, c: (i32, i32), y: i32) {
        let Some(cur) = self.floor(c) else { return };
        if y >= cur {
            let f = self.edge_above(y);
            self.floors.insert(c, f);
            self.finished = false;
            self.dirty = true;
        }
    }

    /// The cell's band is cut: its floor goes down a band, and the claim is let go.
    pub fn finish_band(&mut self, c: (i32, i32)) {
        if let Some(f) = self.floor(c) {
            self.floors.insert(c, (f - self.band).max(self.bottom()));
            self.dirty = true;
        }
        self.claims.remove(&c);
    }

    /// May the cell cut its next band now: its ground known, not done, no neighbour higher (all
    /// known), and not more than `spread` bands below `highest`.
    fn can_dig(&self, c: (i32, i32), highest: i32, spread: i32) -> bool {
        let Some(f) = self.floor(c) else { return false };
        if f <= self.bottom() {
            return false;
        }
        if spread > 0 && (f - self.band).max(self.bottom()) < highest - spread * self.band {
            return false;
        }
        self.neighbours(c).into_iter().all(|n| self.floor(n).is_some_and(|nf| nf <= f))
    }

    /// The highest floor among the cells not done; an unknown cell counts as the top.
    pub fn highest(&self) -> i32 {
        self.cells().into_iter().filter(|c| !self.done(*c)).map(|c| self.floor(c).unwrap_or(self.top() + 1)).max().unwrap_or(self.bottom())
    }

    /// The work for this bot: its claim while there is work in it, else the nearest cell that may
    /// go down (preferring `prefer`, the cell it just finished a band of), else the nearest unknown
    /// cell to scout.
    pub fn claim(&mut self, bot: &str, near: Vec3, spread: i32, prefer: Option<(i32, i32)>) -> Claim {
        if self.area.is_none() {
            return Claim::Wait("the box is not set up yet".into());
        }
        let now = Instant::now();
        self.claims.retain(|_, (_, at)| now.duration_since(*at).as_secs() < CLAIM_SECS);
        self.parked.retain(|_, until| *until > now);
        if let Some(c) = self.claims.iter().find(|(_, (b, _))| b == bot).map(|(c, _)| *c) {
            match self.floor(c) {
                Some(_) if !self.done(c) => {
                    self.claims.insert(c, (bot.to_string(), now));
                    return Claim::Dig(c);
                }
                None => {
                    self.claims.insert(c, (bot.to_string(), now));
                    return Claim::Scout(c);
                }
                Some(_) => {
                    self.claims.remove(&c);
                }
            }
        }
        let highest = self.highest();
        let free = |s: &Self, c: &(i32, i32)| !s.claims.contains_key(c) && !s.parked.contains_key(c);
        let cells = self.cells();
        if let Some(c) = prefer.filter(|c| free(self, c) && self.can_dig(*c, highest, spread)) {
            self.claims.insert(c, (bot.to_string(), now));
            return Claim::Dig(c);
        }
        let dist = |c: &(i32, i32), y: f64| Vec3::new(c.0 as f64 + 4.0, y, c.1 as f64 + 4.0).distance_to(near);
        let dig = cells
            .iter()
            .filter(|c| free(self, c) && self.can_dig(**c, highest, spread))
            .min_by(|a, b| dist(a, self.floor(**a).unwrap_or(0) as f64).total_cmp(&dist(b, self.floor(**b).unwrap_or(0) as f64)))
            .copied();
        if let Some(c) = dig {
            self.claims.insert(c, (bot.to_string(), now));
            return Claim::Dig(c);
        }
        let scout = cells.iter().filter(|c| self.floor(**c).is_none() && free(self, c)).min_by(|a, b| dist(a, near.y).total_cmp(&dist(b, near.y))).copied();
        if let Some(c) = scout {
            self.claims.insert(c, (bot.to_string(), now));
            return Claim::Scout(c);
        }
        if cells.iter().all(|c| self.done(*c)) {
            return Claim::Done;
        }
        if cells.iter().any(|c| self.floor(*c).is_none()) {
            return Claim::Wait("the others are scouting the last of the box".into());
        }
        Claim::Wait(if self.parked.is_empty() {
            format!("waiting for the higher ground to come down (highest left: y {})", highest - 1)
        } else {
            format!("waiting: {} cell(s) put off for want of fill", self.parked.len())
        })
    }

    pub fn release(&mut self, bot: &str) {
        self.claims.retain(|_, (b, _)| b != bot);
    }

    pub fn is_claimed(&self, c: (i32, i32)) -> bool {
        self.claims.contains_key(&c)
    }

    /// The server keeps putting `p` back: it is left standing - and with it the whole side of the
    /// box it is on, once SIDE_REFUSALS of that side's blocks have been. What was newly left, for
    /// the log: Some((what, a whole side)).
    pub fn refuse(&mut self, p: BlockPos) -> Option<(String, bool)> {
        let a = self.area?;
        if self.refused.has(p) {
            return None;
        }
        self.refused.blocks.insert((p.x, p.y, p.z));
        self.dirty = true;
        let r = &mut self.refused;
        let x_side = p.x == a.min.x || p.x == a.max.x || r.xs.contains(&(p.x - 1)) || r.xs.contains(&(p.x + 1));
        if x_side && r.blocks.iter().filter(|b| b.0 == p.x).count() >= SIDE_REFUSALS {
            r.xs.insert(p.x);
            r.blocks.retain(|b| b.0 != p.x);
            let name = if p.x - a.min.x <= a.max.x - p.x { "west" } else { "east" };
            return Some((format!("the {name} side of the box (x {})", p.x), true));
        }
        let z_side = p.z == a.min.z || p.z == a.max.z || r.zs.contains(&(p.z - 1)) || r.zs.contains(&(p.z + 1));
        if z_side && r.blocks.iter().filter(|b| b.2 == p.z).count() >= SIDE_REFUSALS {
            r.zs.insert(p.z);
            r.blocks.retain(|b| b.2 != p.z);
            let name = if p.z - a.min.z <= a.max.z - p.z { "north" } else { "south" };
            return Some((format!("the {name} side of the box (z {})", p.z), true));
        }
        Some((format!("{} {} {}", p.x, p.y, p.z), false))
    }

    /// Leave a cell alone for a while (no fill to seal it with).
    pub fn park(&mut self, c: (i32, i32), secs: u64) {
        self.claims.remove(&c);
        self.parked.insert(c, Instant::now() + std::time::Duration::from_secs(secs));
    }

    /// True once, when the last cell is done.
    pub fn just_finished(&mut self) -> bool {
        let cells = self.cells();
        let all = !cells.is_empty() && cells.iter().all(|c| self.done(*c));
        if all && !self.finished {
            self.finished = true;
            return true;
        }
        false
    }

    /// For the panel: counts, depths, and the map (one character a cell, '?' unknown, else
    /// '0' + bands cleared), every `step`th cell for a big box.
    pub fn summary(&self, with_map: bool) -> Value {
        let Some(a) = self.area else { return json!(null) };
        let cells = self.cells();
        let known = cells.iter().filter(|c| self.floor(**c).is_some()).count();
        let done = cells.iter().filter(|c| self.done(**c)).count();
        let open: Vec<i32> = cells.iter().filter(|c| !self.done(**c)).filter_map(|c| self.floor(*c)).collect();
        let w = (a.max.x - a.min.x) / CELL + 1;
        let h = (a.max.z - a.min.z) / CELL + 1;
        let mut o = json!({
            "min": [a.min.x, a.min.y, a.min.z],
            "max": [a.max.x, a.max.y, a.max.z],
            "band": self.band,
            "cells": cells.len(),
            "known": known,
            "done": done,
            "working": self.claims.len(),
            "parked": self.parked.len(),
            "highest": open.iter().max().map(|f| f - 1),
            "lowest": open.iter().min().map(|f| f - 1),
            "broken": self.broken,
            "placed": self.placed,
            "walled": self.walled,
            "leftSides": self.refused.xs.iter().map(|x| format!("x {x}")).chain(self.refused.zs.iter().map(|z| format!("z {z}"))).collect::<Vec<_>>(),
            "leftBlocks": self.refused.blocks.len(),
            "gridW": w,
            "gridH": h,
        });
        if with_map {
            let step = ((w as f64 * h as f64 / 40_000.0).sqrt().ceil() as i32).max(1);
            let bands = (a.max.y - a.min.y + 1 + self.band - 1) / self.band;
            let mut s = String::new();
            for row in (0..h).step_by(step as usize) {
                for col in (0..w).step_by(step as usize) {
                    let c = (a.min.x + col * CELL, a.min.z + row * CELL);
                    s.push(match self.floor(c) {
                        None => '?',
                        Some(f) => {
                            let k = if f <= a.min.y { bands } else { (a.max.y + 1 - f) / self.band };
                            char::from_u32(48 + k.clamp(0, bands) as u32).unwrap_or('0')
                        }
                    });
                }
            }
            o["map"] = json!(s);
            o["mapStep"] = json!(step);
            o["mapBands"] = json!(bands);
        }
        o
    }
}

// ------------------------------------------------------------------ the job

/// A bot's base: the signed chests by the box where it was logged in.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Base {
    pub tools: crate::config::Pos,
    pub repair: Option<crate::config::Pos>,
    pub food: Option<crate::config::Pos>,
    /// Where it stood when it saw them.
    pub at: crate::config::Pos,
    pub when: u64,
    /// Every chest signed for tools and for repair there, nearest first (the first is `tools` /
    /// `repair`): one without what is wanted, or full, is passed over for the next (6 Nov: the
    /// user can add more of either).
    #[serde(default)]
    pub all_tools: Vec<crate::config::Pos>,
    #[serde(default)]
    pub all_repair: Vec<crate::config::Pos>,
}

impl Base {
    /// The tools chests to try, in order (a base recorded before 6 Nov has only the one).
    pub fn tools_chests(&self) -> Vec<crate::config::Pos> {
        if self.all_tools.is_empty() { vec![self.tools] } else { self.all_tools.clone() }
    }

    pub fn repair_chests(&self) -> Vec<crate::config::Pos> {
        if self.all_repair.is_empty() { self.repair.into_iter().collect() } else { self.all_repair.clone() }
    }
}

pub struct Perimeter {
    pub dir: PathBuf,
    pub job: RwLock<JobConfig>,
    pub board: Mutex<Board>,
    /// The accounts this launcher opens (the panel's tick boxes).
    pub chosen: RwLock<BTreeSet<String>>,
    /// Each bot's base, by account.
    pub bases: Mutex<BTreeMap<String, Base>>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct AccountsFile {
    chosen: BTreeSet<String>,
}

impl Perimeter {
    pub fn load(dir: &Path) -> Self {
        let read = |f: &str| fs::read_to_string(dir.join(f)).ok();
        let job: JobConfig = read("job.json").and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let chosen = read("accounts.json").and_then(|t| serde_json::from_str::<AccountsFile>(&t).ok()).unwrap_or_default().chosen;
        let bases = read("bases.json").and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let p = Self { dir: dir.to_path_buf(), job: RwLock::new(job), board: Mutex::new(Board::load(dir)), chosen: RwLock::new(chosen), bases: Mutex::new(bases) };
        p.save_job();
        p.sync_area();
        p
    }

    fn write(&self, file: &str, v: &impl Serialize) {
        let text = serde_json::to_string_pretty(v).unwrap_or_default();
        if let Err(e) = fs::write(self.dir.join(file), text + "\n") {
            warn!(target: "perimeter", "could not save {file}: {e}");
        }
    }

    pub fn save_job(&self) {
        self.write("job.json", &*self.job.read());
    }

    pub fn save_bases(&self) {
        self.write("bases.json", &*self.bases.lock());
    }

    /// Tick or untick an account for this launcher.
    pub fn choose(&self, name: &str, on: bool) {
        {
            let mut c = self.chosen.write();
            if on {
                c.insert(name.to_string());
            } else {
                c.remove(name);
            }
        }
        self.write("accounts.json", &AccountsFile { chosen: self.chosen.read().clone() });
    }

    /// The board on the job's box, once the world's height is known.
    pub fn sync_area(&self) {
        let job = self.job.read().clone();
        let mut b = self.board.lock();
        if let Some(world) = b.world {
            b.set_area(job.area(world), job.band);
        }
    }

    /// A bot is in a world this high (bottom, top): the box's world, unless one is known already.
    /// False: it is in another dimension than the box - it must not dig there. (Learnt once, so a
    /// bot that turns up in the nether never moves everyone's box; Re-read ground forgets it.)
    pub fn saw_world(&self, world: (i32, i32)) -> bool {
        let learnt = {
            let mut b = self.board.lock();
            match b.world {
                Some(w) => return w == world,
                None => {
                    b.world = Some(world);
                    true
                }
            }
        };
        if learnt {
            self.sync_area();
        }
        true
    }

    /// This bot's base, else the nearest any bot recorded.
    pub fn base_for(&self, name: &str, near: Vec3) -> Option<Base> {
        let bases = self.bases.lock();
        if let Some(b) = bases.get(name) {
            return Some(b.clone());
        }
        let d = |b: &Base| Vec3::new(b.tools.x as f64, b.tools.y as f64, b.tools.z as f64).distance_to(near);
        bases.values().min_by(|a, b| d(a).total_cmp(&d(b))).cloned()
    }
}

// ------------------------------------------------------------------ a bot's tick

/// A perimeter bot's tick, after the shared handler's (queue, status line): its panel commands,
/// its modules (auto-eat, kill-aura), its base and food - then an errand (food, tools, its items
/// after a death) or the digging.
pub fn tick(bot: &Client, s: &Arc<Shared>, ctx: &Arc<SwarmCtx>, p: &Arc<Perimeter>, link: &Arc<BotLink>, ticks: u64) {
    let cmds: Vec<Value> = link.inbox.lock().drain(..).collect();
    for c in cmds {
        match c["type"].as_str().unwrap_or("") {
            "record-base" => base::record(bot, ctx, p, &link.name, true),
            _ => super::tick::bot_command(bot, ctx, &c),
        }
    }
    // Staying alive first: a bot logging itself out does nothing else.
    if guard::tick(bot, s, ctx, link, ticks) {
        return;
    }
    let (eating, fighting) = super::tick::modules_tick(bot, ctx, link, ticks);
    if ticks.is_multiple_of(20) {
        movement::set_max_bps(bot, ctx.cfg.read().max_speed);
    }
    // Its base: the signed chests where it is logged in, seen once their chunks are in.
    if ticks.is_multiple_of(40) {
        base::record(bot, ctx, p, &link.name, false);
    }
    // Its respawn at the bed, once a join - before it is off to dig.
    base::bed_check(bot, link);
    if ticks.is_multiple_of(40) && !link.errand_running() {
        base::food_errand(bot, ctx, p, link);
    }
    super::tick::recover_after_death(bot, link, ticks);

    let state = if let Some(e) = link.errand_what() {
        p.board.lock().release(&link.name);
        link.perimeter.lock().cell = None;
        e
    } else if eating {
        "eating".to_string()
    } else if fighting {
        "fighting".to_string()
    } else if base::bed_pending(link) {
        "looking for its bed".to_string()
    } else {
        match dig::tick(bot, ctx, p, link, ticks) {
            dig::Want::Tools => base::tools_errand(bot, ctx, p, link),
            dig::Want::Moving | dig::Want::Nothing => {}
        }
        link.perimeter.lock().status.clone()
    };
    if ticks.is_multiple_of(5) {
        *link.row.lock() = row(bot, s, link, &state);
    }
}

/// This bot's line in the panel.
fn row(bot: &Client, s: &Shared, link: &BotLink, state: &str) -> Value {
    let r = |v: f64| (v * 10.0).round() / 10.0;
    let d = link.perimeter.lock();
    let mut o = json!({
        "state": state,
        "move": movement::status(bot),
        "deaths": s.deaths.load(Ordering::Relaxed),
        "queue": s.in_queue(),
        "cell": d.cell,
        "broken": d.broken,
        "placed": d.placed,
    });
    drop(d);
    if let Ok(pos) = bot.position() {
        o["x"] = json!(r(pos.x));
        o["y"] = json!(r(pos.y));
        o["z"] = json!(r(pos.z));
        o["dim"] = json!(super::tick::dimension(bot));
        o["health"] = json!(r(bot.health().unwrap_or(0.0) as f64));
        o["hunger"] = json!(bot.hunger().map(|h| h.food).unwrap_or(0));
        o["ping"] = json!(bot.tab_list().ok().and_then(|t| t.get(&bot.uuid()).map(|i| i.latency)).unwrap_or(-1));
    }
    o["food"] = json!(link.mods.lock().autoeat.as_ref().map(|a| a.food_points(bot)));
    // The tools it carries, with their uses left: one of each kind is all it should have.
    let tools: Vec<String> = crate::inv::bags(bot)
        .iter()
        .filter(|(_, st)| super::errand::is_digging_tool(st))
        .map(|(_, st)| format!("{} {}", crate::inv::stack_id(st), crate::inv::uses_left(st).unwrap_or(0)))
        .collect();
    o["tools"] = json!(tools);
    // Its gear: the weapon kill-aura uses, the offhand (the totem), the armor - "id uses-left".
    let named = |st: &azalea::inventory::ItemStack| {
        (!st.is_empty()).then(|| match crate::inv::uses_left(st) {
            Some(u) => format!("{} {u}", crate::inv::stack_id(st)),
            None => crate::inv::stack_id(st).to_string(),
        })
    };
    let worn = crate::inv::menu(bot).map(|m| m.slots()).unwrap_or_default();
    let worn = |i: usize| worn.get(i).and_then(named);
    let weapon = crate::inv::bags(bot).into_iter().filter_map(|(_, st)| crate::modules::killaura::weapon_rank(&st).map(|r| (r, st))).max_by_key(|(r, _)| *r);
    o["weapon"] = json!(weapon.and_then(|(_, st)| named(&st)));
    o["offhand"] = json!(worn(45));
    o["armor"] = json!([5, 6, 7, 8].map(worn));
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(min: (i32, i32, i32), max: (i32, i32, i32), band: i32) -> Board {
        let mut b = Board::empty();
        b.set_area(Box3::new(BlockPos::new(min.0, min.1, min.2), BlockPos::new(max.0, max.1, max.2)), band);
        b
    }

    #[test]
    fn the_default_box_is_800_by_800_and_paused() {
        assert!(!JobConfig::default().running, "nothing is dug before a box is set");
        let a = JobConfig::default().area((-64, 319));
        assert_eq!(a.min, BlockPos::new(0, -64, 0));
        assert_eq!(a.max, BlockPos::new(799, 319, 799));
        assert_eq!(a.max.x - a.min.x + 1, 800);
        assert_eq!(a.max.z - a.min.z + 1, 800);
        let mut b = Board::empty();
        b.set_area(a, 4);
        assert_eq!(b.cells().len(), 100 * 100);
    }

    #[test]
    fn floors_sit_on_band_edges() {
        // Top 19, bottom 0, band 4: edges 20, 16, 12, 8, 4, 0.
        let mut b = board((0, 0, 0), (15, 19, 15), 4);
        b.found((0, 0), Some(13));
        assert_eq!(b.floor((0, 0)), Some(16), "13 is in the band 12..15: clear from 16 up");
        b.found((0, 0), Some(17));
        assert_eq!(b.floor((0, 0)), Some(16), "a read never raises the floor");
        b.finish_band((0, 0));
        assert_eq!(b.floor((0, 0)), Some(12));
        b.raise((0, 0), 13);
        assert_eq!(b.floor((0, 0)), Some(16), "a block turning up above the floor puts it back up");
        b.raise((0, 0), 3);
        assert_eq!(b.floor((0, 0)), Some(16), "below the floor changes nothing");
        b.found((8, 0), None);
        assert!(b.done((8, 0)), "nothing down to the bottom: done");
    }

    #[test]
    fn the_last_band_stops_at_the_bottom() {
        // 10 layers (0..=9), band 4: 10, 6, 2, then the bottom.
        let mut b = board((0, 0, 0), (7, 9, 7), 4);
        b.found((0, 0), Some(9));
        assert_eq!(b.floor((0, 0)), Some(10));
        b.finish_band((0, 0));
        b.finish_band((0, 0));
        assert_eq!(b.floor((0, 0)), Some(2));
        assert!(!b.done((0, 0)));
        b.finish_band((0, 0));
        assert_eq!(b.floor((0, 0)), Some(0));
        assert!(b.done((0, 0)));
    }

    #[test]
    fn no_cell_goes_below_a_higher_neighbour() {
        // Three cells in a row; the middle one is lower ground.
        let mut b = board((0, 0, 0), (23, 99, 7), 4);
        b.found((0, 0), Some(80));
        b.found((8, 0), Some(60));
        b.found((16, 0), Some(80));
        let h = b.highest();
        assert!(!b.can_dig((8, 0), h, 0), "its neighbours are higher");
        assert!(b.can_dig((0, 0), h, 0));
        assert!(b.can_dig((16, 0), h, 0));
        let at = Vec3::new(10.0, 70.0, 4.0);
        let c = b.claim("A", at, 0, None);
        assert!(matches!(c, Claim::Dig((0, 0)) | Claim::Dig((16, 0))), "{c:?}");
        assert_eq!(b.claim("A", at, 0, None), c, "a bot keeps its claim");
        let other = b.claim("B", at, 0, None);
        assert!(matches!(other, Claim::Dig(_)) && other != c, "two bots, two cells");
        assert!(matches!(b.claim("C", at, 0, None), Claim::Wait(_)), "the low cell waits for its neighbours");
    }

    #[test]
    fn unknown_ground_is_scouted_before_anything_is_dug() {
        let mut b = board((0, 0, 0), (15, 99, 7), 4);
        b.found((0, 0), Some(64));
        let at = Vec3::new(0.0, 70.0, 0.0);
        assert_eq!(b.claim("A", at, 2, None), Claim::Scout((8, 0)), "its neighbour's ground is not known");
        b.found((8, 0), Some(64));
        b.release("A");
        assert!(matches!(b.claim("A", at, 2, None), Claim::Dig(_)));
    }

    #[test]
    fn a_scouted_cell_is_dug_only_when_the_level_rules_let_it() {
        let mut b = board((0, 0, 0), (15, 99, 7), 4);
        b.found((0, 0), Some(64));
        let at = Vec3::new(12.0, 70.0, 4.0);
        assert_eq!(b.claim("A", at, 2, None), Claim::Scout((8, 0)));
        // Lower than its neighbour: the scout's claim ends, and the higher ground goes first.
        b.found((8, 0), Some(40));
        assert!(!b.is_claimed((8, 0)));
        assert_eq!(b.claim("A", at, 2, None), Claim::Dig((0, 0)));
    }

    #[test]
    fn spread_keeps_every_cell_within_two_bands_of_the_highest_ground() {
        // A 3x1 strip; the far end's only neighbour is the middle cell.
        let mut b = board((0, 0, 0), (23, 99, 7), 4);
        b.floors.insert((0, 0), 64);
        b.floors.insert((8, 0), 60);
        b.floors.insert((16, 0), 60);
        let h = b.highest();
        assert_eq!(h, 64);
        assert!(b.can_dig((16, 0), h, 2), "56 is two bands below the highest ground left (64)");
        b.floors.insert((8, 0), 56);
        b.floors.insert((16, 0), 56);
        assert!(!b.can_dig((16, 0), h, 2), "52 would be three bands below it");
        assert!(b.can_dig((16, 0), h, 0), "with no spread only the neighbour rule holds");
    }

    #[test]
    fn a_finished_box_says_so_once() {
        let mut b = board((0, 0, 0), (7, 9, 7), 4);
        b.found((0, 0), None);
        assert_eq!(b.claim("A", Vec3::ZERO, 2, None), Claim::Done);
        assert!(b.just_finished());
        assert!(!b.just_finished());
    }

    #[test]
    fn progress_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("perimeter-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut b = board((0, -64, 0), (15, 319, 15), 4);
        b.world = Some((-64, 319));
        b.found((0, 0), Some(70));
        b.broken = 12;
        for y in 60..63 {
            b.refuse(BlockPos::new(0, y, 5));
        }
        b.refuse(BlockPos::new(7, 40, 7));
        b.save(&dir);
        let l = Board::load(&dir);
        assert_eq!(l.area, b.area);
        assert_eq!(l.floor((0, 0)), Some(72));
        assert_eq!(l.broken, 12);
        assert_eq!(l.world, Some((-64, 319)));
        assert_eq!(l.refused, b.refused);
        assert!(l.refused.has(BlockPos::new(0, 100, 9)) && l.refused.has(BlockPos::new(7, 40, 7)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_side_the_server_guards_is_left_whole() {
        // The box's west side guarded (WorldGuard along a border, say).
        let mut b = Board::empty();
        b.set_area(JobConfig::default().area((-64, 319)), 3);
        let west = |y, z| BlockPos::new(0, y, z);
        assert_eq!(b.refuse(west(87, 225)), Some(("0 87 225".into(), false)));
        assert_eq!(b.refuse(west(87, 225)), None, "said once");
        b.refuse(west(86, 225));
        assert!(!b.refused.has(west(90, 197)));
        assert_eq!(b.refuse(west(90, 197)), Some(("the west side of the box (x 0)".into(), true)));
        assert!(b.refused.has(west(-60, 799)) && b.refused.has(west(300, 0)));
        assert!(b.refused.blocks.is_empty(), "the side holds them now");
        assert!(!b.refused.has(BlockPos::new(1, 87, 225)));
        // A guarded strip wider than that: the next x in goes whole too.
        for y in 80..83 {
            b.refuse(BlockPos::new(1, y, 300));
        }
        assert!(b.refused.xs.contains(&1));
        // Inside the box a row of them is only those blocks (a claim there, not a side).
        for y in 80..85 {
            b.refuse(BlockPos::new(400, y, 400));
        }
        assert!(!b.refused.has(BlockPos::new(400, 79, 400)));
        // The south side, z 799.
        for x in [500, 501, 502] {
            b.refuse(BlockPos::new(x, 70, 799));
        }
        assert!(b.refused.zs.contains(&799));
        // Another box: what was left is learnt afresh.
        b.set_area(Box3::new(BlockPos::new(0, -64, 0), BlockPos::new(15, 319, 15)), 3);
        assert_eq!(b.refused, Refused::default());
    }

    #[test]
    fn the_map_has_a_character_a_cell() {
        let mut b = board((0, 0, 0), (23, 19, 15), 4);
        b.found((0, 0), Some(13));
        b.found((8, 0), None);
        let s = b.summary(true);
        let map: Vec<char> = s["map"].as_str().unwrap().chars().collect();
        assert_eq!(map.len(), 3 * 2);
        assert_eq!(map[0], '1', "one band (16..19) cleared");
        assert_eq!(map[1], char::from_u32(48 + 5).unwrap(), "done: every band");
        assert_eq!(map[2], '?');
    }
}
