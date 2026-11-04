//! What a click places, and the click that places what the schematic wants.
//!
//! - **Prediction**: vanilla's getStateForPlacement for every block type that chooses an
//!   orientation (read from the 26.2 bytecode), the BlockPlaceContext it reads (clicked position,
//!   face, click height, the look's nearest directions - Mth's table sine included), canBeReplaced,
//!   and the item layer above it (a torch, sign, banner, head or coral fan item tries the standing
//!   or the wall block by the look; a hanging sign the ceiling or the wall one).
//! - **Search**: the standalone Printer's resolveOrientation - every face x click height {.25, .5,
//!   .75} x yaw {0, 90, 180, 270} x pitch {-90, 0, 90} until one gives the required orientation -
//!   with the yaw that gives a 16-step rotation added for signs, banners and heads (the standalone
//!   could only reach 4 of their 16).
//!
//! Compared, as the standalone compares them: facing, axis, the 16-step rotation, orientation,
//! attach face, stair/trapdoor half (top/bottom), slab type; a chest's type only once its partner
//! stands next to it. Shapes and connections follow from the neighbours and are not compared.

use azalea::{
    BlockPos, Vec3,
    block::BlockState,
    core::aabb::Aabb,
    physics::collision::BlockWithShape,
    registry::{builtin::BlockKind, tags},
    world::World,
};

use super::{block_types, items};

// ------------------------------------------------------------------ directions and the look

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    Down,
    Up,
    North,
    South,
    West,
    East,
}

impl Dir {
    /// Direction.values() order (the standalone's face search order).
    pub const ALL: [Dir; 6] = [Dir::Down, Dir::Up, Dir::North, Dir::South, Dir::West, Dir::East];

    pub fn opposite(self) -> Dir {
        match self {
            Dir::Down => Dir::Up,
            Dir::Up => Dir::Down,
            Dir::North => Dir::South,
            Dir::South => Dir::North,
            Dir::West => Dir::East,
            Dir::East => Dir::West,
        }
    }

    pub fn horizontal(self) -> bool {
        !matches!(self, Dir::Down | Dir::Up)
    }

    pub fn axis(self) -> &'static str {
        match self {
            Dir::Down | Dir::Up => "y",
            Dir::North | Dir::South => "z",
            Dir::West | Dir::East => "x",
        }
    }

    pub fn step(self) -> BlockPos {
        match self {
            Dir::Down => BlockPos::new(0, -1, 0),
            Dir::Up => BlockPos::new(0, 1, 0),
            Dir::North => BlockPos::new(0, 0, -1),
            Dir::South => BlockPos::new(0, 0, 1),
            Dir::West => BlockPos::new(-1, 0, 0),
            Dir::East => BlockPos::new(1, 0, 0),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Dir::Down => "down",
            Dir::Up => "up",
            Dir::North => "north",
            Dir::South => "south",
            Dir::West => "west",
            Dir::East => "east",
        }
    }

    pub fn parse(s: &str) -> Option<Dir> {
        Some(match s {
            "down" => Dir::Down,
            "up" => Dir::Up,
            "north" => Dir::North,
            "south" => Dir::South,
            "west" => Dir::West,
            "east" => Dir::East,
            _ => return None,
        })
    }

    pub fn clockwise(self) -> Dir {
        match self {
            Dir::North => Dir::East,
            Dir::East => Dir::South,
            Dir::South => Dir::West,
            Dir::West => Dir::North,
            v => v,
        }
    }

    pub fn counter_clockwise(self) -> Dir {
        match self {
            Dir::North => Dir::West,
            Dir::West => Dir::South,
            Dir::South => Dir::East,
            Dir::East => Dir::North,
            v => v,
        }
    }

    fn from_2d(i: i32) -> Dir {
        [Dir::South, Dir::West, Dir::North, Dir::East][i.rem_euclid(4) as usize]
    }

    fn data_2d(self) -> i32 {
        match self {
            Dir::South => 0,
            Dir::West => 1,
            Dir::North => 2,
            Dir::East => 3,
            _ => 0,
        }
    }

    pub fn to_azalea(self) -> azalea::core::direction::Direction {
        use azalea::core::direction::Direction as D;
        match self {
            Dir::Down => D::Down,
            Dir::Up => D::Up,
            Dir::North => D::North,
            Dir::South => D::South,
            Dir::West => D::West,
            Dir::East => D::East,
        }
    }
}

/// Mth.sin / Mth.cos: a 65536-entry sine table, index = (long)(x * 10430.378350470453) & 65535.
fn table_sin(i: i64) -> f32 {
    ((i as f64) * std::f64::consts::PI * 2.0 / 65536.0).sin() as f32
}
fn mth_sin(x: f64) -> f32 {
    table_sin((x * 10430.378350470453) as i64 & 65535)
}
fn mth_cos(x: f64) -> f32 {
    table_sin((x * 10430.378350470453 + 16384.0) as i64 & 65535)
}

/// Direction.orderedByNearest(player): the six directions, nearest to the look first.
pub fn ordered_by_nearest(yaw: f32, pitch: f32) -> [Dir; 6] {
    let f = pitch * 0.017453292f32;
    let f1 = -yaw * 0.017453292f32;
    let f2 = mth_sin(f as f64);
    let f3 = mth_cos(f as f64);
    let f4 = mth_sin(f1 as f64);
    let f5 = mth_cos(f1 as f64);
    let flag = f4 > 0.0;
    let flag1 = f2 < 0.0;
    let flag2 = f5 > 0.0;
    let f6 = if flag { f4 } else { -f4 };
    let f7 = if flag1 { -f2 } else { f2 };
    let f8 = if flag2 { f5 } else { -f5 };
    let f9 = f6 * f3;
    let f10 = f8 * f3;
    let d = if flag { Dir::East } else { Dir::West };
    let d1 = if flag1 { Dir::Up } else { Dir::Down };
    let d2 = if flag2 { Dir::South } else { Dir::North };
    let arr = |a: Dir, b: Dir, c: Dir| [a, b, c, c.opposite(), b.opposite(), a.opposite()];
    if f6 > f8 {
        if f7 > f9 {
            arr(d1, d, d2)
        } else if f10 > f7 {
            arr(d, d2, d1)
        } else {
            arr(d, d1, d2)
        }
    } else if f7 > f10 {
        arr(d1, d2, d)
    } else if f9 > f7 {
        arr(d2, d, d1)
    } else {
        arr(d2, d1, d)
    }
}

/// Direction.fromYRot: the horizontal direction a yaw faces.
pub fn from_yaw(yaw: f32) -> Dir {
    Dir::from_2d(((yaw as f64) / 90.0 + 0.5).floor() as i32 & 3)
}

/// RotationSegment.convertToSegment(float): Math.round(deg * 16/360) & 15.
fn segment(deg: f32) -> i32 {
    let x = deg * (16.0f32 / 360.0f32);
    ((x + 0.5).floor() as i32) & 15
}

// ------------------------------------------------------------------ block shapes

const EPS: f64 = 1.0e-7;

fn covers(b: &Aabb, face: Dir, u: f64, v: f64) -> bool {
    let within = |lo: f64, hi: f64, p: f64| lo <= p + EPS && p <= hi + EPS;
    match face {
        Dir::Up => b.max.y >= 1.0 - EPS && within(b.min.x, b.max.x, u) && within(b.min.z, b.max.z, v),
        Dir::Down => b.min.y <= EPS && within(b.min.x, b.max.x, u) && within(b.min.z, b.max.z, v),
        Dir::North => b.min.z <= EPS && within(b.min.x, b.max.x, u) && within(b.min.y, b.max.y, v),
        Dir::South => b.max.z >= 1.0 - EPS && within(b.min.x, b.max.x, u) && within(b.min.y, b.max.y, v),
        Dir::West => b.min.x <= EPS && within(b.min.z, b.max.z, u) && within(b.min.y, b.max.y, v),
        Dir::East => b.max.x >= 1.0 - EPS && within(b.min.z, b.max.z, u) && within(b.min.y, b.max.y, v),
    }
}

fn face_has(state: BlockState, face: Dir, points: &[(f64, f64)]) -> bool {
    if state.is_collision_shape_full() {
        return true;
    }
    if state.is_collision_shape_empty() {
        return false;
    }
    let boxes = state.base_collision_shape().to_aabbs();
    points.iter().all(|&(u, v)| boxes.iter().any(|b| covers(b, face, u, v)))
}

/// isFaceSturdy(FULL): the block's face is covered edge to edge.
pub fn sturdy(state: BlockState, face: Dir) -> bool {
    const GRID: [(f64, f64); 16] = [
        (0.125, 0.125), (0.125, 0.375), (0.125, 0.625), (0.125, 0.875),
        (0.375, 0.125), (0.375, 0.375), (0.375, 0.625), (0.375, 0.875),
        (0.625, 0.125), (0.625, 0.375), (0.625, 0.625), (0.625, 0.875),
        (0.875, 0.125), (0.875, 0.375), (0.875, 0.625), (0.875, 0.875),
    ];
    face_has(state, face, &GRID)
}

/// canSupportCenter: the middle of the face is covered (a fence post holds a lantern).
pub fn center_support(state: BlockState, face: Dir) -> bool {
    face_has(state, face, &[(0.4375, 0.4375), (0.4375, 0.5625), (0.5625, 0.4375), (0.5625, 0.5625), (0.5, 0.5)])
}

/// BlockState.isSolid (the legacy flag signs and banners hang on): a collision box nearly a block
/// in size, or a full block tall.
pub fn is_solid(state: BlockState) -> bool {
    if let Some(f) = state.to_trait().behavior().force_solid {
        return f;
    }
    if state.is_collision_shape_full() {
        return true;
    }
    if state.is_collision_shape_empty() {
        return false;
    }
    let b = state.base_collision_shape().bounds();
    let (x, y, z) = (b.max.x - b.min.x, b.max.y - b.min.y, b.max.z - b.min.z);
    (x + y + z) / 3.0 >= 0.729_166_666_666_666_6 || y >= 1.0
}

fn kind(state: BlockState) -> BlockKind {
    BlockKind::from(state)
}

fn id_of(k: BlockKind) -> &'static str {
    let s = k.to_str();
    s.strip_prefix("minecraft:").unwrap_or(s)
}

fn prop(state: BlockState, name: &str) -> Option<&'static str> {
    state.to_trait().get_property(name)
}

fn with(state: BlockState, props: &[(&str, &str)]) -> BlockState {
    let mut b = state.to_trait().boxed();
    for (k, v) in props {
        let _ = b.set_property(k, v);
    }
    b.as_block_state()
}

// ------------------------------------------------------------------ the click

/// A right-click on a block: what the server receives (clicked block, face, hit point) and the
/// look and sneak it is judged with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Click {
    pub clicked: BlockPos,
    pub face: Dir,
    pub hit: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub sneak: bool,
}

/// The BlockPlaceContext a click makes.
struct Ctx<'a> {
    world: &'a World,
    /// getClickedPos: where the block goes.
    pos: BlockPos,
    face: Dir,
    hit: Vec3,
    yaw: f32,
    pitch: f32,
    sneak: bool,
    replace_clicked: bool,
    /// The block the item in hand places (for "same block here" checks).
    item_block: BlockKind,
}

impl Ctx<'_> {
    fn at(&self, p: BlockPos) -> BlockState {
        self.world.get_block_state(p).unwrap_or(BlockState::AIR)
    }
    fn rel(&self, d: Dir) -> BlockState {
        self.at(self.pos + d.step())
    }
    fn horizontal(&self) -> Dir {
        from_yaw(self.yaw)
    }
    fn nearest(&self) -> Dir {
        ordered_by_nearest(self.yaw, self.pitch)[0]
    }
    /// getNearestLookingDirections: clicking a neighbour puts the way into it first.
    fn nearest_all(&self) -> [Dir; 6] {
        let mut d = ordered_by_nearest(self.yaw, self.pitch);
        if !self.replace_clicked {
            let want = self.face.opposite();
            if let Some(i) = d.iter().position(|x| *x == want)
                && i > 0
            {
                d.copy_within(0..i, 1);
                d[0] = want;
            }
        }
        d
    }
    fn hit_y(&self) -> f64 {
        self.hit.y - self.pos.y as f64
    }
    /// canBeReplaced(ctx) of whatever is at `p`, with `replacing` as replacingClickedOnBlock.
    fn replaceable(&self, p: BlockPos, replacing: bool) -> bool {
        can_be_replaced(self.at(p), self.item_block, self.face, self.hit.y - p.y as f64, self.sneak, replacing)
    }
}

/// BlockState.canBeReplaced(BlockPlaceContext) for the blocks the printer meets: the #replaceable
/// ones (air, water, grass, snow...) and a same-item stack that grows (slabs, candles, pickles,
/// eggs, snow layers, petals).
pub fn can_be_replaced(state: BlockState, item_block: BlockKind, face: Dir, hit_y: f64, sneak: bool, replacing: bool) -> bool {
    let k = kind(state);
    let same = k == item_block;
    let t = block_types::of(id_of(k));
    match t {
        "slab" | "weathering_copper_slab" => {
            let ty = prop(state, "type").unwrap_or("bottom");
            if ty == "double" || !same {
                return false;
            }
            if !replacing {
                return true;
            }
            let upper = hit_y > 0.5;
            if ty == "bottom" { face == Dir::Up || (upper && face.horizontal()) } else { face == Dir::Down || (!upper && face.horizontal()) }
        }
        "candle" => (!sneak && same && prop(state, "candles").and_then(|c| c.parse::<i32>().ok()).is_some_and(|c| c < 4)) || tags::blocks::REPLACEABLE.contains(&k),
        "sea_pickle" => !sneak && same && prop(state, "pickles").and_then(|c| c.parse::<i32>().ok()).is_some_and(|c| c < 4),
        "turtle_egg" => !sneak && same && prop(state, "eggs").and_then(|c| c.parse::<i32>().ok()).is_some_and(|c| c < 4),
        "flower_bed" | "leaf_litter" => {
            let amount = prop(state, "flower_amount").or(prop(state, "segment_amount")).and_then(|c| c.parse::<i32>().ok()).unwrap_or(4);
            (!sneak && same && amount < 4) || (t == "leaf_litter" && tags::blocks::REPLACEABLE.contains(&k) && !same)
        }
        "snow_layer" => {
            let layers = prop(state, "layers").and_then(|c| c.parse::<i32>().ok()).unwrap_or(1);
            if same && layers < 8 {
                if replacing { face == Dir::Up } else { true }
            } else {
                layers == 1
            }
        }
        _ => tags::blocks::REPLACEABLE.contains(&k),
    }
}

// ------------------------------------------------------------------ the rules

fn stair_half(face: Dir, hit_y: f64) -> &'static str {
    if face != Dir::Down && (face == Dir::Up || hit_y <= 0.5) { "bottom" } else { "top" }
}

/// canSurvive for the blocks whose placement walks the look's directions until one holds.
fn survives(t: &str, state: BlockState, ctx: &Ctx) -> bool {
    let facing = prop(state, "facing").and_then(Dir::parse).unwrap_or(Dir::North);
    match t {
        "wall_torch" | "redstone_wall_torch" | "ladder" | "trip_wire_hook" | "base_coral_wall_fan" | "coral_wall_fan" => {
            sturdy(ctx.rel(facing.opposite()), facing)
        }
        "wall_sign" | "wall_banner" => is_solid(ctx.rel(facing.opposite())),
        "cocoa" => tags::blocks::JUNGLE_LOGS.contains(&kind(ctx.rel(facing))),
        "wall_hanging_sign" => {
            let side = |d: Dir| {
                let s = ctx.rel(d);
                if tags::blocks::WALL_HANGING_SIGNS.contains(&kind(s)) {
                    prop(s, "facing").and_then(Dir::parse).is_some_and(|f| f.axis() == facing.axis())
                } else {
                    sturdy(s, d.opposite())
                }
            };
            side(facing.clockwise()) || side(facing.counter_clockwise())
        }
        "button" | "lever" => {
            let connected = match prop(state, "face") {
                Some("ceiling") => Dir::Down,
                Some("floor") => Dir::Up,
                _ => facing,
            };
            sturdy(ctx.rel(connected.opposite()), connected)
        }
        "grindstone" => true,
        "lantern" | "weathering_lantern" => {
            if prop(state, "hanging") == Some("true") { center_support(ctx.rel(Dir::Up), Dir::Down) } else { center_support(ctx.rel(Dir::Down), Dir::Up) }
        }
        "bell" => match prop(state, "attachment") {
            Some("floor") => sturdy(ctx.rel(Dir::Down), Dir::Up),
            Some("ceiling") => center_support(ctx.rel(Dir::Up), Dir::Down),
            _ => sturdy(ctx.rel(facing), facing.opposite()),
        },
        // Standing forms (the dual items check these too).
        "torch" | "redstone_torch" => center_support(ctx.rel(Dir::Down), Dir::Up),
        "standing_sign" | "banner" => is_solid(ctx.rel(Dir::Down)),
        "ceiling_hanging_sign" => center_support(ctx.rel(Dir::Up), Dir::Down),
        "coral_fan" | "base_coral_fan" => sturdy(ctx.rel(Dir::Down), Dir::Up),
        // BlockItem.getPlacementState also asks canSurvive of what it would place (the harness:
        // a cluster with nothing behind it, a dripleaf on stone - nothing went down).
        "amethyst_cluster" => sturdy(ctx.rel(facing.opposite()), facing),
        "big_dripleaf" => {
            let below = kind(ctx.rel(Dir::Down));
            below == BlockKind::BigDripleaf || below == BlockKind::BigDripleafStem || tags::blocks::SUPPORTS_BIG_DRIPLEAF.contains(&below)
        }
        "carpet" | "wool_carpet" | "mossy_carpet" => !ctx.rel(Dir::Down).is_air(),
        "flower_bed" => tags::blocks::SUPPORTS_VEGETATION.contains(&kind(ctx.rel(Dir::Down))),
        "small_dripleaf" => tags::blocks::SUPPORTS_SMALL_DRIPLEAF.contains(&kind(ctx.rel(Dir::Down))),
        "leaf_litter" => sturdy(ctx.rel(Dir::Down), Dir::Up),
        "redstone_wire" => sturdy(ctx.rel(Dir::Down), Dir::Up) || kind(ctx.rel(Dir::Down)) == BlockKind::Hopper,
        "repeater" | "comparator" | "rail" | "powered_rail" | "detector_rail" => sturdy(ctx.rel(Dir::Down), Dir::Up),
        "pressure_plate" | "weighted_pressure_plate" | "candle" => center_support(ctx.rel(Dir::Down), Dir::Up),
        "door" | "weathering_copper_door" => sturdy(ctx.rel(Dir::Down), Dir::Up),
        _ => true,
    }
}

/// Walk the look's horizontal directions: the first `facing` (from `dir`) that holds.
fn wall(t: &str, base: BlockState, ctx: &Ctx, facing_of: impl Fn(Dir) -> Dir, skip_axis: Option<&str>) -> Option<BlockState> {
    for d in ctx.nearest_all() {
        if !d.horizontal() || skip_axis == Some(d.axis()) {
            continue;
        }
        let st = with(base, &[("facing", facing_of(d).name())]);
        if survives(t, st, ctx) {
            return Some(st);
        }
    }
    None
}

/// getStateForPlacement of `k` for this click (None: vanilla returns null and nothing is placed).
fn rule(k: BlockKind, ctx: &Ctx) -> Option<BlockState> {
    let id = id_of(k);
    let t = block_types::of(id);
    let base = BlockState::from(k);
    let h = ctx.horizontal();
    let set = |props: &[(&str, &str)]| with(base, props);
    let existing = ctx.at(ctx.pos);
    let here_same = kind(existing) == k;
    Some(match t {
        "furnace" | "smoker" | "blast_furnace" | "beehive" | "jack_o_lantern" | "chiseled_book_shelf" | "copper_golem_statue"
        | "weathering_copper_golem_statue" | "repeater" | "comparator" | "dried_ghast" | "end_portal_frame" | "ender_chest"
        | "glazed_terracotta" | "lectern" | "loom" | "shelf" | "stonecutter" | "vault" => set(&[("facing", h.opposite().name())]),
        "campfire" | "calibrated_sculk_sensor" | "decorated_pot" | "fence_gate" => set(&[("facing", h.name())]),
        "anvil" => set(&[("facing", h.clockwise().name())]),
        "barrel" | "command" | "dispenser" | "dropper" | "piston_base" => set(&[("facing", ctx.nearest().opposite().name())]),
        "observer" => set(&[("facing", ctx.nearest().name())]),
        "amethyst_cluster" | "lightning_rod" | "weathering_lightning_rod" | "shulker_box" => set(&[("facing", ctx.face.name())]),
        "end_rod" => {
            let behind = ctx.rel(ctx.face.opposite());
            let flip = kind(behind) == k && prop(behind, "facing") == Some(ctx.face.name());
            set(&[("facing", if flip { ctx.face.opposite() } else { ctx.face }.name())])
        }
        "hopper" => {
            let f = ctx.face.opposite();
            set(&[("facing", if f.horizontal() { f.name() } else { "down" })])
        }
        "rotated_pillar" | "hay" | "chain" | "weathering_copper_chain" | "creaking_heart" | "infested_rotated_pillar" => set(&[("axis", ctx.face.axis())]),
        "stair" | "weathering_copper_stair" => set(&[("facing", h.name()), ("half", stair_half(ctx.face, ctx.hit_y()))]),
        "slab" | "weathering_copper_slab" => {
            if here_same {
                with(existing, &[("type", "double")])
            } else {
                set(&[("type", stair_half(ctx.face, ctx.hit_y()))])
            }
        }
        "trapdoor" | "weathering_copper_trapdoor" => {
            if !ctx.replace_clicked && ctx.face.horizontal() {
                set(&[("facing", ctx.face.name()), ("half", if ctx.hit_y() > 0.5 { "top" } else { "bottom" })])
            } else {
                set(&[("facing", h.opposite().name()), ("half", if ctx.face == Dir::Up { "bottom" } else { "top" })])
            }
        }
        "door" | "weathering_copper_door" => {
            if !ctx.replaceable(ctx.pos + Dir::Up.step(), false) {
                return None;
            }
            set(&[("facing", h.name()), ("half", "lower")])
        }
        "bed" => {
            if !ctx.replaceable(ctx.pos + h.step(), false) {
                return None;
            }
            set(&[("facing", h.name())])
        }
        "double_plant" | "tall_flower" | "tall_seagrass" => {
            if !ctx.replaceable(ctx.pos + Dir::Up.step(), false) {
                return None;
            }
            base
        }
        "small_dripleaf" => {
            if !ctx.replaceable(ctx.pos + Dir::Up.step(), false) {
                return None;
            }
            set(&[("facing", h.opposite().name())])
        }
        "big_dripleaf" => {
            let below = ctx.rel(Dir::Down);
            let bk = kind(below);
            if bk == BlockKind::BigDripleaf || bk == BlockKind::BigDripleafStem {
                set(&[("facing", prop(below, "facing").unwrap_or("north"))])
            } else {
                set(&[("facing", h.opposite().name())])
            }
        }
        "flower_bed" | "leaf_litter" => {
            if here_same {
                let p = if t == "flower_bed" { "flower_amount" } else { "segment_amount" };
                let n = prop(existing, p).and_then(|c| c.parse::<i32>().ok()).unwrap_or(1);
                with(existing, &[(p, &(n + 1).min(4).to_string())])
            } else {
                set(&[("facing", h.opposite().name())])
            }
        }
        "banner" | "standing_sign" => set(&[("rotation", &segment(ctx.yaw + 180.0).to_string())]),
        "skull" | "player_head" | "wither_skull" => set(&[("rotation", &segment(ctx.yaw).to_string())]),
        "ceiling_hanging_sign" => {
            let above = ctx.rel(Dir::Up);
            let dir = from_yaw(ctx.yaw);
            let full = {
                // Block.isFaceFull(collision shape, DOWN)
                const G: [(f64, f64); 4] = [(0.125, 0.125), (0.125, 0.875), (0.875, 0.125), (0.875, 0.875)];
                face_has(above, Dir::Down, &G)
            };
            let mut attached = !full || ctx.sneak;
            if tags::blocks::ALL_HANGING_SIGNS.contains(&kind(above)) && !ctx.sneak {
                if let Some(f) = prop(above, "facing").and_then(Dir::parse) {
                    if f.axis() == dir.axis() {
                        attached = false;
                    }
                } else if let Some(r) = prop(above, "rotation").and_then(|r| r.parse::<i32>().ok())
                    && r % 4 == 0
                    && Dir::from_2d(r / 4).axis() == dir.axis()
                {
                    attached = false;
                }
            }
            let seg = if !attached { dir.opposite().data_2d() * 4 } else { segment(ctx.yaw + 180.0) };
            set(&[("rotation", &seg.to_string()), ("attached", if attached { "true" } else { "false" })])
        }
        "wall_torch" | "redstone_wall_torch" | "wall_sign" | "wall_banner" | "trip_wire_hook" | "base_coral_wall_fan" | "coral_wall_fan" => {
            return wall(t, base, ctx, |d| d.opposite(), None);
        }
        "ladder" => {
            if !ctx.replace_clicked {
                let behind = ctx.rel(ctx.face.opposite());
                if kind(behind) == k && prop(behind, "facing") == Some(ctx.face.name()) {
                    return None;
                }
            }
            return wall(t, base, ctx, |d| d.opposite(), None);
        }
        "cocoa" => return wall(t, base, ctx, |d| d, None),
        "wall_hanging_sign" => return wall(t, base, ctx, |d| d.opposite(), Some(ctx.face.axis())),
        "wall_skull" | "piglinwallskull" | "player_wall_head" | "wither_wall_skull" => {
            for d in ctx.nearest_all() {
                if d.horizontal() && !ctx.replaceable(ctx.pos + d.step(), false) {
                    return Some(set(&[("facing", d.opposite().name())]));
                }
            }
            return None;
        }
        "button" | "lever" | "grindstone" => {
            for d in ctx.nearest_all() {
                let st = if d.axis() == "y" {
                    set(&[("face", if d == Dir::Up { "ceiling" } else { "floor" }), ("facing", h.name())])
                } else {
                    set(&[("face", "wall"), ("facing", d.opposite().name())])
                };
                if survives(t, st, ctx) {
                    return Some(st);
                }
            }
            return None;
        }
        "lantern" | "weathering_lantern" => {
            for d in ctx.nearest_all() {
                if d.axis() == "y" {
                    let st = set(&[("hanging", if d == Dir::Up { "true" } else { "false" })]);
                    if survives(t, st, ctx) {
                        return Some(st);
                    }
                }
            }
            return None;
        }
        "bell" => {
            let f = ctx.face;
            if f.axis() == "y" {
                let st = set(&[("attachment", if f == Dir::Down { "ceiling" } else { "floor" }), ("facing", h.name())]);
                return survives(t, st, ctx).then_some(st);
            }
            let double = match f.axis() {
                "x" => sturdy(ctx.rel(Dir::West), Dir::East) && sturdy(ctx.rel(Dir::East), Dir::West),
                _ => sturdy(ctx.rel(Dir::North), Dir::South) && sturdy(ctx.rel(Dir::South), Dir::North),
            };
            let st = set(&[("facing", f.opposite().name()), ("attachment", if double { "double_wall" } else { "single_wall" })]);
            if survives(t, st, ctx) {
                return Some(st);
            }
            let st = with(st, &[("attachment", if sturdy(ctx.rel(Dir::Down), Dir::Up) { "floor" } else { "ceiling" })]);
            return survives(t, st, ctx).then_some(st);
        }
        "jigsaw" => {
            let top = if ctx.face.axis() == "y" { h.opposite() } else { Dir::Up };
            set(&[("orientation", &format!("{}_{}", ctx.face.name(), top.name()))])
        }
        "crafter" => {
            let front = ctx.nearest().opposite();
            let top = match front {
                Dir::Down => h.opposite(),
                Dir::Up => h,
                _ => Dir::Up,
            };
            set(&[("orientation", &format!("{}_{}", front.name(), top.name()))])
        }
        "chest" | "trapped_chest" | "copper_chest" | "weathering_copper_chest" => chest(k, base, ctx),
        "candle" | "sea_pickle" | "turtle_egg" | "snow_layer" if here_same => {
            let p = match t {
                "candle" => "candles",
                "sea_pickle" => "pickles",
                "turtle_egg" => "eggs",
                _ => "layers",
            };
            let max = if t == "snow_layer" { 8 } else { 4 };
            let n = prop(existing, p).and_then(|c| c.parse::<i32>().ok()).unwrap_or(1);
            with(existing, &[(p, &(n + 1).min(max).to_string())])
        }
        _ => base,
    })
}

fn chest(k: BlockKind, base: BlockState, ctx: &Ctx) -> BlockState {
    // chestCanConnectTo: the same chest (copper chests of any age connect - approximated by the
    // block type).
    let t = block_types::of(id_of(k));
    let partner = |d: Dir| -> Option<Dir> {
        let s = ctx.rel(d);
        let sk = kind(s);
        let connects = sk == k || (t.contains("copper_chest") && block_types::of(id_of(sk)).contains("copper_chest"));
        if connects && prop(s, "type") == Some("single") { prop(s, "facing").and_then(Dir::parse) } else { None }
    };
    let mut ty = "single";
    let mut facing = ctx.horizontal().opposite();
    if ctx.face.horizontal() && ctx.sneak
        && let Some(pf) = partner(ctx.face.opposite())
        && pf.axis() != ctx.face.axis()
    {
        facing = pf;
        ty = if pf.counter_clockwise() == ctx.face.opposite() { "right" } else { "left" };
    }
    if ty == "single" && !ctx.sneak {
        if partner(facing.clockwise()) == Some(facing) {
            ty = "left";
        } else if partner(facing.counter_clockwise()) == Some(facing) {
            ty = "right";
        }
    }
    with(base, &[("facing", facing.name()), ("type", ty)])
}

/// What the server places for this click with the item for `required` in hand, and where; None
/// when nothing would be placed.
pub fn simulate(world: &World, required: BlockState, click: &Click) -> Option<(BlockPos, BlockState)> {
    let item = items::item_for(required)?;
    let dual = items::dual(&item);
    let rk = kind(required);
    let item_block = match &dual {
        Some(d) => d.standing,
        None => items::placed_form(rk).unwrap_or(rk),
    };
    let clicked_state = world.get_block_state(click.clicked).unwrap_or(BlockState::AIR);
    let replace_clicked = can_be_replaced(clicked_state, item_block, click.face, click.hit.y - click.clicked.y as f64, click.sneak, true);
    let pos = if replace_clicked { click.clicked } else { click.clicked + click.face.step() };
    let mut ctx = Ctx { world, pos, face: click.face, hit: click.hit, yaw: click.yaw, pitch: click.pitch, sneak: click.sneak, replace_clicked, item_block };
    // BlockPlaceContext.canPlace: the clicked block itself, or what is where it goes.
    if !replace_clicked && !ctx.replaceable(pos, false) {
        return None;
    }
    match dual {
        Some(d) => {
            // StandingAndWallBlockItem.getPlacementState: along the look, skipping the side opposite
            // the attachment; the attachment side tries the standing block, the others the wall one.
            let wall_state = {
                ctx.item_block = d.wall;
                let s = rule(d.wall, &ctx);
                ctx.item_block = d.standing;
                s
            };
            let attach = if d.attach_up { Dir::Up } else { Dir::Down };
            for dir in ctx.nearest_all() {
                if dir == attach.opposite() {
                    continue;
                }
                let st = if dir == attach { rule(d.standing, &ctx) } else { wall_state };
                if let Some(st) = st {
                    let t = block_types::of(id_of(kind(st)));
                    if survives(t, st, &ctx) {
                        return Some((pos, st));
                    }
                }
            }
            None
        }
        None => {
            let st = rule(item_block, &ctx)?;
            // BlockItem.canPlace: what it would place must survive there.
            let t = block_types::of(id_of(kind(st)));
            survives(t, st, &ctx).then_some((pos, st))
        }
    }
}

// ------------------------------------------------------------------ comparing

/// The properties a placement controls that `required` has, compared by value.
fn compared(required: BlockState, partner_present: bool) -> Vec<&'static str> {
    let t = required.to_trait();
    let mut out = Vec::new();
    for (k, v) in t.property_map() {
        let keep = match k {
            "facing" | "axis" | "rotation" | "orientation" => true,
            "face" => true,
            "half" => matches!(v, "top" | "bottom"),
            "type" => matches!(v, "top" | "bottom" | "double") || (partner_present && matches!(v, "single" | "left" | "right")),
            _ => false,
        };
        if keep {
            out.push(k);
        }
    }
    out
}

/// The schematic's state and the world's disagree on something a placement controls (Printer's
/// isMismatch, strict).
pub fn orientation_differs(current: BlockState, required: BlockState, partner_present: bool) -> bool {
    compared(required, partner_present).iter().any(|p| prop(current, p) != prop(required, p))
}

/// The double chest half `required` describes has its partner in place next to it.
/// ChestBlock.getConnectedDirection (26.2 bytecode): LEFT's partner is clockwise of its facing,
/// RIGHT's counter-clockwise. (The standalone's doubleChestPartnerPos has these swapped.)
pub fn chest_partner_present(world: &World, pos: BlockPos, required: BlockState) -> bool {
    let (Some(ty), Some(f)) = (prop(required, "type"), prop(required, "facing").and_then(Dir::parse)) else { return false };
    let d = match ty {
        "left" => f.clockwise(),
        "right" => f.counter_clockwise(),
        _ => return false,
    };
    world.get_block_state(pos + d.step()).is_some_and(|s| kind(s) == kind(required))
}

// ------------------------------------------------------------------ the search

/// Blocks never clicked as a neighbour: clicking them with a block item does something to them
/// (a pot takes the item, a cauldron a bucket...) instead of placing.
fn unsafe_to_click(state: BlockState) -> bool {
    let id = id_of(kind(state));
    let t = block_types::of(id);
    matches!(
        t,
        "decorated_pot" | "shelf" | "flower_pot" | "composter" | "cauldron" | "layered_cauldron" | "lava_cauldron" | "beehive"
            | "jukebox" | "lectern" | "chiseled_book_shelf" | "cake" | "candle_cake" | "respawn_anchor" | "end_portal_frame"
            | "campfire" | "vault" | "tnt" | "standing_sign" | "wall_sign" | "ceiling_hanging_sign" | "wall_hanging_sign"
    )
}

/// Eye to the nearest point of a block, as the server measures a click.
pub fn block_dist(eye: Vec3, b: BlockPos) -> f64 {
    let cx = eye.x.clamp(b.x as f64, b.x as f64 + 1.0);
    let cy = eye.y.clamp(b.y as f64, b.y as f64 + 1.0);
    let cz = eye.z.clamp(b.z as f64, b.z as f64 + 1.0);
    ((eye.x - cx).powi(2) + (eye.y - cy).powi(2) + (eye.z - cz).powi(2)).sqrt()
}

/// Printer.localHitOnFace: the face's middle, or `y_frac` up a side face.
fn local_hit(face: Dir, y_frac: f64) -> Vec3 {
    let s = face.step();
    Vec3::new(0.5 + s.x as f64 * 0.5, if face.axis() == "y" { 0.5 + s.y as f64 * 0.5 } else { y_frac }, 0.5 + s.z as f64 * 0.5)
}

/// The click that places `required` at `target`, and whether the look must be sent first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Choice {
    pub click: Click,
    pub needs_look: bool,
}

/// Printer.resolveOrientation: the first click (face, height, look, sneak) that the server turns
/// into `required`'s orientation at `target`. A solid neighbour in reach is clicked on its face;
/// otherwise the target itself is (an air-place - from the offhand, which the server takes).
/// Err says why none does.
pub fn choose(world: &World, eye: Vec3, target: BlockPos, required: BlockState, reach: f64) -> Result<Choice, String> {
    // A double slab goes down as one slab, then the other: no click makes it from nothing.
    let here = world.get_block_state(target).unwrap_or(BlockState::AIR);
    let required = if prop(required, "type") == Some("double") && kind(here) != kind(required) { with(required, &[("type", "bottom")]) } else { required };
    let partner = chest_partner_present(world, target, required);
    let props = compared(required, partner);
    let needed = props.len();
    let t = block_types::of(id_of(kind(required)));
    // A 16-step rotation needs its own yaw (signs and banners face the player: +180).
    let mut yaws: Vec<f32> = vec![0.0, 90.0, 180.0, 270.0];
    if let Some(r) = prop(required, "rotation").and_then(|r| r.parse::<i32>().ok()) {
        let deg = r as f32 * 22.5;
        let y = if matches!(t, "skull" | "player_head" | "wither_skull") { deg } else { deg - 180.0 };
        yaws.insert(0, y.rem_euclid(360.0));
    }
    let sneaks: &[bool] = if t.contains("chest") || t.contains("hanging_sign") || items::item_for(required).is_some_and(|i| i.ends_with("_hanging_sign")) {
        &[false, true]
    } else {
        &[false]
    };
    let mut best: Option<(usize, Choice)> = None;
    for face in Dir::ALL {
        let neighbour = target + face.opposite().step();
        let ns = world.get_block_state(neighbour).unwrap_or(BlockState::AIR);
        let solid = !ns.is_air() && !ns.is_collision_shape_empty() && !tags::blocks::REPLACEABLE.contains(&kind(ns)) && !unsafe_to_click(ns);
        let clicked = if solid && block_dist(eye, neighbour) <= reach { neighbour } else { target };
        let fracs: &[f64] = if face.horizontal() { &[0.25, 0.5, 0.75] } else { &[0.5] };
        for &frac in fracs {
            let l = local_hit(face, frac);
            let hit = Vec3::new(clicked.x as f64 + l.x, clicked.y as f64 + l.y, clicked.z as f64 + l.z);
            for &yaw in &yaws {
                for pitch in [-90.0f32, 0.0, 90.0] {
                    for &sneak in sneaks {
                        let click = Click { clicked, face, hit, yaw, pitch, sneak };
                        let Some((pos, st)) = simulate(world, required, &click) else { continue };
                        if pos != target || kind(st) != kind(required) && items::placed_form(kind(required)) != Some(kind(st)) {
                            continue;
                        }
                        let score = props.iter().filter(|p| prop(st, p) == prop(required, p)).count();
                        let c = Choice { click, needs_look: false };
                        if score == needed {
                            // Even with nothing to compare the look can matter: a torch item
                            // makes a wall torch or a standing one by where it looks.
                            return Ok(Choice { click, needs_look: look_matters(world, required, &click, &props) });
                        }
                        if best.as_ref().is_none_or(|(s, _)| score > *s) {
                            best = Some((score, c));
                        }
                    }
                }
            }
        }
    }
    Err(match best {
        Some((s, _)) => format!("no click gives {} here ({s}/{needed} of its {} - its support may not be built yet)", id_of(kind(required)), props.join("/")),
        None => format!("nothing can be placed at {} {} {} (the cell is taken, or nothing holds it)", target.x, target.y, target.z),
    })
}

/// The placement reads the look's nearest directions (Direction.orderedByNearest), whose yaw the
/// server takes from the player's HEAD - LivingEntity.getViewYRot returns yHeadRot, which follows
/// the body only on the player's tick (Player.aiStep). Such a click needs its look sent a tick
/// ahead; everything else (getHorizontalDirection, getRotation - the body's yaw - and the pitch)
/// is read at once. Found by the conformance harness: a wall torch sent with its look faced the
/// previous click's way.
pub fn head_yaw_matters(required: BlockState) -> bool {
    let id = id_of(kind(required));
    if items::item_for(required).is_some_and(|i| items::dual(&i).is_some()) {
        return true;
    }
    matches!(
        block_types::of(id),
        "barrel" | "command" | "dispenser" | "dropper" | "piston_base" | "observer" | "crafter" | "wall_torch" | "redstone_wall_torch"
            | "wall_sign" | "wall_banner" | "trip_wire_hook" | "base_coral_wall_fan" | "coral_wall_fan" | "ladder" | "cocoa"
            | "wall_hanging_sign" | "wall_skull" | "piglinwallskull" | "player_wall_head" | "wither_wall_skull" | "button" | "lever"
            | "grindstone" | "lantern" | "weathering_lantern"
    )
}

/// Printer.rotationAffectsPlacement: another yaw or pitch with the same click would miss.
fn look_matters(world: &World, required: BlockState, click: &Click, props: &[&str]) -> bool {
    let ok = |yaw: f32, pitch: f32| {
        let c = Click { yaw, pitch, ..*click };
        simulate(world, required, &c).is_some_and(|(_, st)| kind(st) == kind(required) && props.iter().all(|p| prop(st, p) == prop(required, p)))
    };
    !ok((click.yaw + 90.0).rem_euclid(360.0), click.pitch) || !ok(click.yaw, if click.pitch == 0.0 { 90.0 } else { 0.0 })
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

    #[test]
    fn the_look_orders_directions_like_the_game() {
        // Looking south, level: south first.
        assert_eq!(ordered_by_nearest(0.0, 0.0)[0], Dir::South);
        assert_eq!(ordered_by_nearest(90.0, 0.0)[0], Dir::West);
        assert_eq!(ordered_by_nearest(180.0, 0.0)[0], Dir::North);
        assert_eq!(ordered_by_nearest(270.0, 0.0)[0], Dir::East);
        assert_eq!(ordered_by_nearest(0.0, 90.0)[0], Dir::Down);
        assert_eq!(ordered_by_nearest(0.0, -90.0)[0], Dir::Up);
        assert_eq!(from_yaw(0.0), Dir::South);
        assert_eq!(from_yaw(90.0), Dir::West);
        assert_eq!(from_yaw(-90.0), Dir::East);
        assert_eq!(segment(180.0), 8);
        assert_eq!(segment(22.5 * 3.0 + 180.0), 11);
    }

    fn place(w: &World, at: BlockPos, req: &str) -> BlockState {
        let required = parse_state(req).unwrap();
        let eye = Vec3::new(at.x as f64 + 0.5, at.y as f64 + 2.6, at.z as f64 + 0.5);
        let c = choose(w, eye, at, required, 4.4).unwrap_or_else(|e| panic!("{req}: {e}"));
        let (pos, st) = simulate(w, required, &c.click).unwrap();
        assert_eq!(pos, at);
        st
    }

    #[test]
    fn every_orientation_family_is_reachable() {
        let floor: Vec<(BlockPos, &str)> = (-3..=3).flat_map(|x| (-3..=3).map(move |z| (BlockPos::new(x, 63, z), "stone"))).collect();
        let (w, _keep) = world(&floor);
        let at = BlockPos::new(0, 64, 0);
        for req in [
            "oak_stairs[facing=north,half=top]",
            "oak_stairs[facing=east,half=bottom]",
            "oak_log[axis=x]",
            "oak_log[axis=z]",
            "oak_slab[type=top]",
            "oak_slab[type=bottom]",
            "oak_trapdoor[facing=west,half=top]",
            "oak_trapdoor[facing=south,half=bottom]",
            "furnace[facing=east]",
            "observer[facing=up]",
            "observer[facing=north]",
            "piston[facing=down]",
            "hopper[facing=east]",
            "hopper[facing=down]",
            "end_rod[facing=west]",
            "shulker_box[facing=down]",
            "oak_sign[rotation=5]",
            "red_banner[rotation=13]",
            "creeper_head[rotation=7]",
            "anvil[facing=west]",
            "oak_fence_gate[facing=north]",
            "stone_button[face=floor,facing=east]",
            "lever[face=floor,facing=south]",
            "crafter[orientation=north_up]",
            "crafter[orientation=down_east]",
            "barrel[facing=up]",
            "dispenser[facing=south]",
            "repeater[facing=west]",
            "jigsaw[orientation=up_south]",
            "chest[facing=east,type=single]",
        ] {
            let required = parse_state(req).unwrap();
            let got = place(&w, at, req);
            assert!(!orientation_differs(got, required, false), "{req}: placed {got:?}");
        }
    }

    #[test]
    fn wall_things_need_their_wall() {
        // A wall to the east of the target: a torch facing west hangs on it.
        let (w, _keep) = world(&[(BlockPos::new(1, 64, 0), "stone"), (BlockPos::new(0, 63, 0), "stone")]);
        let at = BlockPos::new(0, 64, 0);
        let got = place(&w, at, "wall_torch[facing=west]");
        assert_eq!((kind(got), prop(got, "facing")), (BlockKind::WallTorch, Some("west")));
        let got = place(&w, at, "oak_wall_sign[facing=west]");
        assert_eq!((kind(got), prop(got, "facing")), (BlockKind::OakWallSign, Some("west")));
        let got = place(&w, at, "torch");
        assert_eq!(kind(got), BlockKind::Torch);
        // No wall to the north: a torch facing south cannot be placed yet.
        let req = parse_state("wall_torch[facing=south]").unwrap();
        assert!(choose(&w, Vec3::new(0.5, 66.6, 0.5), at, req, 4.4).is_err());
    }

    #[test]
    fn doors_and_beds_go_down_on_a_floor() {
        let floor: Vec<(BlockPos, &str)> = (-3..=3).flat_map(|x| (-3..=3).map(move |z| (BlockPos::new(x, 63, z), "stone"))).collect();
        let (w, _keep) = world(&floor);
        let at = BlockPos::new(0, 64, 0);
        let req = parse_state("acacia_door[facing=north]").unwrap();
        eprintln!("door: {:?} item {:?}", req, items::item_for(req));
        let got = place(&w, at, "acacia_door[facing=north]");
        assert_eq!(prop(got, "facing"), Some("north"));
        let got = place(&w, at, "black_bed[facing=east]");
        assert_eq!(prop(got, "facing"), Some("east"));
        let got = place(&w, at, "torch");
        assert_eq!(kind(got), BlockKind::Torch);
        // A double slab in an empty cell: a single one first.
        let got = place(&w, at, "oak_slab[type=double]");
        assert_eq!(prop(got, "type"), Some("bottom"));
    }

    #[test]
    fn a_torch_needs_its_look_when_walls_are_around() {
        // Walls on every side: looking down makes a standing torch, level a wall one.
        let mut b: Vec<(BlockPos, &str)> = vec![(BlockPos::new(0, 63, 0), "stone"), (BlockPos::new(0, 65, 0), "stone")];
        for (x, z) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            b.push((BlockPos::new(x, 64, z), "stone"));
        }
        let (w, _keep) = world(&b);
        let at = BlockPos::new(0, 64, 0);
        let c = choose(&w, Vec3::new(0.5, 66.6, 2.5), at, parse_state("torch").unwrap(), 4.4).unwrap();
        assert!(c.needs_look, "{c:?}");
        assert_eq!(kind(simulate(&w, parse_state("torch").unwrap(), &c.click).unwrap().1), BlockKind::Torch);
    }

    #[test]
    fn a_bottom_slab_becomes_a_double_one() {
        let (w, _keep) = world(&[(BlockPos::new(0, 63, 0), "stone"), (BlockPos::new(0, 64, 0), "oak_slab[type=bottom]")]);
        let got = place(&w, BlockPos::new(0, 64, 0), "oak_slab[type=double]");
        assert_eq!(prop(got, "type"), Some("double"));
    }

    #[test]
    fn a_chest_pairs_with_its_partner() {
        // Facing south, a LEFT half's partner is clockwise of south - west, at x = -1.
        let (w, _keep) = world(&[(BlockPos::new(0, 63, 0), "stone"), (BlockPos::new(-1, 63, 0), "stone"), (BlockPos::new(-1, 64, 0), "chest[facing=south,type=single]")]);
        let at = BlockPos::new(0, 64, 0);
        let req = parse_state("chest[facing=south,type=left]").unwrap();
        assert!(chest_partner_present(&w, at, req));
        let got = place(&w, at, "chest[facing=south,type=left]");
        assert_eq!((prop(got, "facing"), prop(got, "type")), (Some("south"), Some("left")));
    }
}
