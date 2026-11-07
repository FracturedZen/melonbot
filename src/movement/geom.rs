//! What the server will accept as a place to be: a player box clear of block collisions (the
//! server's own end check), sampled paths for horizontal moves, and the vertical searches the
//! clips use. Port of FlightMover.isVolumeClear / ServerMoveCheck.endClear / ChargedTravel's
//! firstStandAbove/Below.

use azalea::{
    BlockPos, Vec3,
    core::aabb::Aabb,
    physics::collision::world_collisions::get_block_collisions,
    registry::builtin::BlockKind,
    world::World,
};

pub const HALF_W: f64 = 0.3;
pub const HEIGHT: f64 = 1.8;
/// Vanilla deflates the end box by this before testing it against blocks.
const END_EPSILON: f64 = 1.0e-5;

pub fn player_box(feet: Vec3) -> Aabb {
    Aabb {
        min: Vec3::new(feet.x - HALF_W, feet.y, feet.z - HALF_W),
        max: Vec3::new(feet.x + HALF_W, feet.y + HEIGHT, feet.z + HALF_W),
    }
}

/// Would the server accept this as an end position? Above the build limit and below the floor
/// is open air.
pub fn stand_clear(world: &World, feet: Vec3) -> bool {
    let top = (world.chunks.min_y() + world.chunks.height() as i32) as f64;
    let bottom = world.chunks.min_y() as f64;
    if feet.y >= top || feet.y + HEIGHT <= bottom {
        return true;
    }
    let b = player_box(feet);
    let b = Aabb { min: b.min + Vec3::new(END_EPSILON, END_EPSILON, END_EPSILON), max: b.max - Vec3::new(END_EPSILON, END_EPSILON, END_EPSILON) };
    get_block_collisions(world, &b).is_empty()
}

/// A horizontal (or any) move the server re-simulates: every point along it must be clear. Sampled
/// every 0.25 blocks - finer than any block edge the player box could slip past.
pub fn path_clear(world: &World, from: Vec3, to: Vec3) -> bool {
    let d = to - from;
    let len = (d.x * d.x + d.y * d.y + d.z * d.z).sqrt();
    let steps = ((len / 0.25).ceil() as i32).max(1);
    (1..=steps).all(|i| {
        let t = i as f64 / steps as f64;
        stand_clear(world, from + d * t)
    })
}

/// The mover gets from `from` to `to` without a detour: a straight line, or across and then
/// straight up or down (the way it flies a change of height), or up or down and then across. It
/// does no routing, and contained it gives up rather than go round - so of two spots, pick this one.
pub fn way_clear(world: &World, from: Vec3, to: Vec3) -> bool {
    if path_clear(world, from, to) {
        return true;
    }
    let across = Vec3::new(to.x, from.y, to.z);
    let vertical = Vec3::new(from.x, to.y, from.z);
    (path_clear(world, from, across) && path_clear(world, across, to)) || (path_clear(world, from, vertical) && path_clear(world, vertical, to))
}

/// The first clear stand above `from` past something solid, up to `max_y` (a roof to clip through).
pub fn first_stand_above(world: &World, from: Vec3, max_y: f64) -> Option<f64> {
    let mut seen_solid = false;
    let mut y = from.y + 0.25;
    while y <= max_y + 1e-3 {
        if !stand_clear(world, Vec3::new(from.x, y, from.z)) {
            seen_solid = true;
        } else if seen_solid {
            return Some(y);
        }
        y += 0.25;
    }
    None
}

/// The first clear stand below `from` past something solid, down to `min_y` (a floor to clip through).
/// As high as it can be, to 0.01 as sent, less `margin`: the server bills the whole drop as a fall,
/// so a 1-block floor clipped tight costs nothing where the 0.25 grid's 3.05 cost a heart. Paper
/// refuses a head exactly at the floor's underside (15 Oct: feet 141.20 under a floor at 143 was
/// pulled back 5 times; 141.00 went through), hence the margin.
pub fn first_stand_below(world: &World, from: Vec3, min_y: f64, margin: f64) -> Option<f64> {
    let bottom = min_y.max(world.chunks.min_y() as f64 + 1.0);
    let mut seen_solid = false;
    let mut y = from.y - 0.25;
    while y >= bottom - 1e-3 {
        if !stand_clear(world, Vec3::new(from.x, y, from.z)) {
            seen_solid = true;
        } else if seen_solid {
            return Some(tighten_up(world, from, y, margin));
        }
        y -= 0.25;
    }
    None
}

/// From a clear stand `y` with something solid within 0.25 above, the highest stand on the 0.01
/// grid that is still clear `margin` higher, once smoothed as it is sent.
fn tighten_up(world: &World, at: Vec3, y: f64, margin: f64) -> f64 {
    let clear = |t: f64| stand_clear(world, Vec3::new(at.x, smooth(t + margin), at.z));
    let mut k = (y * 100.0).floor() as i64;
    if !clear(k as f64 / 100.0) {
        return y;
    }
    while ((k + 1) as f64 / 100.0) < y + 0.25 && clear((k + 1) as f64 / 100.0) {
        k += 1;
    }
    k as f64 / 100.0
}

/// An open way down past the floor `from` stands on, so a descent never clips through it (the
/// server bills that as a fall). `under` is the clear spot just under the floor. Out from it along
/// 16 directions, under the floor at its height, to the nearest column that is open from the floor
/// down to there, and that can be reached across the top - at `from`'s height or up to 16 higher
/// (things stand on a floor; 15 Oct: the straight way across a roof over the base was blocked and
/// the back-off undid the detour). Returns the crossing height and the shaft's bottom.
pub fn find_shaft(world: &World, from: Vec3, under: Vec3, r: i32) -> Option<(f64, Vec3)> {
    // A roof over a whole 120x50 farm is common: rings round the goal alone never got out from
    // under it. Nearest open column wins.
    let goal = under;
    let mut best: Option<(f64, f64, Vec3)> = None;
    for i in 0..16 {
        let a = i as f64 * std::f64::consts::TAU / 16.0;
        let (sx, sz) = (a.cos(), a.sin());
        let mut prev = goal;
        let mut d = 2.0;
        while d <= r as f64 {
            if best.is_some_and(|(bd, _, _)| d >= bd) {
                break;
            }
            // The middle of the block column the ray is over, a block at a time: a 1-wide gap (the
            // ceiling's, between its tiles) takes the 0.6 body only there, and 2-block steps at any
            // point along the ray all but never landed in one.
            let bottom = Vec3::new((goal.x + sx * d).floor() + 0.5, goal.y, (goal.z + sz * d).floor() + 0.5);
            // Across under the roof: this ray is walled off from here on.
            if !path_clear(world, prev, bottom) {
                break;
            }
            prev = bottom;
            // A covered column fails within a step or two of the floor, so this stays cheap; only
            // open ones try the crossing heights.
            if column_clear(world, Vec3::new(bottom.x, from.y, bottom.z), goal.y) {
                let mut h = from.y;
                while h <= from.y + 16.0 + 1e-6 {
                    let above = Vec3::new(from.x, h, from.z);
                    if h > from.y && !path_clear(world, from, above) {
                        break;
                    }
                    let top = Vec3::new(bottom.x, h, bottom.z);
                    if (h == from.y || column_clear(world, top, from.y)) && path_clear(world, above, top) {
                        best = Some((d, h, bottom));
                        break;
                    }
                    h += 2.0;
                }
                if best.is_some_and(|(bd, _, _)| bd == d) {
                    break;
                }
            }
            d += 1.0;
        }
    }
    best.map(|(_, h, bottom)| (h, bottom))
}

/// Straight down from `top` to `bottom_y` is clear: 1.5 steps (overlapping 1.8-high boxes miss
/// nothing) and the bottom itself.
pub fn column_clear(world: &World, top: Vec3, bottom_y: f64) -> bool {
    let mut y = top.y;
    while y > bottom_y {
        if !stand_clear(world, Vec3::new(top.x, y, top.z)) {
            return false;
        }
        y -= 1.5;
    }
    stand_clear(world, Vec3::new(top.x, bottom_y, top.z))
}

/// What this client sees in the player box's footprint at `at`, from its feet up to `top_y`: the
/// blocks that are not air, by height (the evidence when the server refuses a clip).
pub fn describe_column(world: &World, at: Vec3, top_y: f64) -> String {
    let mut out = Vec::new();
    for y in at.y.floor() as i32..=top_y.floor() as i32 {
        let mut kinds: Vec<String> = Vec::new();
        for bx in (at.x - HALF_W).floor() as i32..=(at.x + HALF_W).floor() as i32 {
            for bz in (at.z - HALF_W).floor() as i32..=(at.z + HALF_W).floor() as i32 {
                let name = match world.get_block_state(BlockPos::new(bx, y, bz)) {
                    None => "unloaded".to_string(),
                    Some(s) if s.is_air() => continue,
                    Some(s) => BlockKind::from(s).to_string(),
                };
                if !kinds.contains(&name) {
                    kinds.push(name);
                }
            }
        }
        if !kinds.is_empty() {
            out.push(format!("y{y} {}", kinds.join("/")));
        }
    }
    if out.is_empty() { "all air".into() } else { out.join(", ") }
}

/// Standing on something: the box a twentieth of a block lower runs into a block.
pub fn supported(world: &World, feet: Vec3) -> bool {
    !stand_clear(world, Vec3::new(feet.x, feet.y - 0.05, feet.z))
}

/// How far straight down is clear, in 0.1 steps, up to `max_down`.
pub fn clear_down(world: &World, from: Vec3, max_down: f64) -> f64 {
    let mut last = 0.0;
    let steps = ((max_down / 0.1).ceil() as i32).max(1);
    for i in 1..=steps {
        let d = (i as f64 * 0.1).min(max_down);
        if !stand_clear(world, Vec3::new(from.x, from.y - d, from.z)) {
            return last;
        }
        last = d;
    }
    last
}

/// The nearest clear stand within `r` blocks of `to` (a destination inside a block).
pub fn nearest_clear(world: &World, to: Vec3, r: i32) -> Option<Vec3> {
    let mut best: Option<(f64, Vec3)> = None;
    for dy in -r..=r {
        for dx in -r..=r {
            for dz in -r..=r {
                let c = Vec3::new(to.x + dx as f64, to.y + dy as f64, to.z + dz as f64);
                let d = (dx * dx + dy * dy + dz * dz) as f64;
                if best.is_none_or(|(bd, _)| d < bd) && stand_clear(world, c) {
                    best = Some((d, c));
                }
            }
        }
    }
    best.map(|(_, c)| c)
}

/// Round to 0.01 and step off the seam, the way both reference addons send coordinates
/// (Smooth.coord).
pub fn smooth(v: f64) -> f64 {
    if !v.is_finite() {
        return v;
    }
    let r = (v * 100.0).round() / 100.0;
    if v > 0.0 { r.next_up() } else if v < 0.0 { r.next_down() } else { r }
}
