//! The builder's plan for one map: a colour at a time - every carpet of one colour is placed before
//! the next colour is begun (the user, 4 Nov: "100% of one color at a time ... all black, then blue
//! and so on"), the most-used colour first - each colour's pixels in the order one flight over the
//! art takes them, and the nearest chest of a colour to fetch it from (several chests of each
//! colour stand round the area; the nearest to where the bot is, every time).

use azalea::{BlockPos, Vec3, registry::builtin::BlockKind};

use super::art::{Map, SIDE};

/// Rows one pass over the art covers: the carpets within reach either side of its line.
pub const STRIP: usize = 8;

/// The colours of a map in the order they are placed (most-used first, ties by name), with how
/// many of each.
pub fn colours(map: &Map) -> Vec<(BlockKind, usize)> {
    map.counts()
}

/// One colour's pixels (x, z on the map) in flying order: strips of STRIP rows from the north,
/// crossed west to east and then back east to west, so the next carpet is always near the last.
pub fn route(map: &Map, kind: BlockKind) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for s in 0..SIDE.div_ceil(STRIP) {
        let rows = s * STRIP..((s + 1) * STRIP).min(SIDE);
        let xs: Box<dyn Iterator<Item = usize>> = if s % 2 == 0 { Box::new(0..SIDE) } else { Box::new((0..SIDE).rev()) };
        for x in xs {
            out.extend(rows.clone().filter(|z| map.at(x, *z) == Some(kind)).map(|z| (x, z)));
        }
    }
    out
}

/// The chest of `chests` that makes the shortest trip from `from` to it and on to `to` (where the
/// laying goes on), leaving out the ones found `empty` (6 Nov, the user: "its going to farther
/// chests for carpets than it needs to" - the nearest to where the bot ran out can be the far side
/// of the art from where it was laying).
pub fn chest_on_the_way(chests: &[BlockPos], empty: &[BlockPos], from: Vec3, to: Vec3) -> Option<BlockPos> {
    let mid = |c: &BlockPos| Vec3::new(c.x as f64 + 0.5, c.y as f64 + 0.5, c.z as f64 + 0.5);
    let d = |c: &BlockPos| mid(c).distance_to(from) + mid(c).distance_to(to);
    chests.iter().filter(|c| !empty.contains(c)).min_by(|a, b| d(a).total_cmp(&d(b))).copied()
}

/// The nearest of `chests` to `at`, leaving out the ones found `empty`.
pub fn nearest_chest(chests: &[BlockPos], empty: &[BlockPos], at: Vec3) -> Option<BlockPos> {
    let d = |c: &BlockPos| Vec3::new(c.x as f64 + 0.5, c.y as f64 + 0.5, c.z as f64 + 0.5).distance_to(at);
    chests.iter().filter(|c| !empty.contains(c)).min_by(|a, b| d(a).total_cmp(&d(b))).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapart::art::PIXELS;

    /// Black in the left half, blue in a 10x10 square, red in three scattered pixels; the rest white.
    fn map() -> Map {
        let mut pixels = vec![Some(BlockKind::WhiteCarpet); PIXELS];
        for z in 0..SIDE {
            for x in 0..64 {
                pixels[z * SIDE + x] = Some(BlockKind::BlackCarpet);
            }
        }
        for z in 100..110 {
            for x in 100..110 {
                pixels[z * SIDE + x] = Some(BlockKind::BlueCarpet);
            }
        }
        for (x, z) in [(70, 3), (127, 127), (90, 64)] {
            pixels[z * SIDE + x] = Some(BlockKind::RedCarpet);
        }
        Map { col: 0, row: 0, pixels }
    }

    #[test]
    fn one_colour_at_a_time_the_most_used_first() {
        let order: Vec<BlockKind> = colours(&map()).into_iter().map(|c| c.0).collect();
        assert_eq!(order, vec![BlockKind::BlackCarpet, BlockKind::WhiteCarpet, BlockKind::BlueCarpet, BlockKind::RedCarpet]);
    }

    #[test]
    fn a_colours_route_takes_each_of_its_carpets_once_in_strips() {
        let m = map();
        for (kind, n) in colours(&m) {
            let r = route(&m, kind);
            assert_eq!(r.len(), n, "{kind:?}");
            let mut seen = r.clone();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), n, "{kind:?}: a pixel twice");
            assert!(r.iter().all(|(x, z)| m.at(*x, *z) == Some(kind)));
        }
        // Inside a strip the next carpet is never far: black's first strip is rows 0-7, west to
        // east, so its first pixels run down column 0 and then column 1.
        let black = route(&m, BlockKind::BlackCarpet);
        assert_eq!(&black[..9], &[(0, 0), (0, 1), (0, 2), (0, 3), (0, 4), (0, 5), (0, 6), (0, 7), (1, 0)]);
        // The second strip (rows 8-15) comes back east to west: it starts at column 63.
        assert_eq!(black[64 * 8], (63, 8));
        // The scattered red ones, north strip first.
        assert_eq!(route(&m, BlockKind::RedCarpet), vec![(70, 3), (90, 64), (127, 127)]);
    }

    #[test]
    fn the_chest_on_the_way_back_to_the_laying() {
        // Out of carpet over the east side, the laying going on in the west: the west chest,
        // though the east one is nearer to where it ran out.
        let chests = [BlockPos::new(140, 64, 60), BlockPos::new(-5, 64, 60)];
        let (here, laying) = (Vec3::new(120.0, 66.0, 60.0), Vec3::new(2.0, 66.0, 60.0));
        assert_eq!(chest_on_the_way(&chests, &[], here, laying), Some(chests[1]));
        assert_eq!(chest_on_the_way(&chests, &[chests[1]], here, laying), Some(chests[0]));
    }

    #[test]
    fn the_nearest_chest_with_carpet_in_it() {
        let chests = [BlockPos::new(-5, 64, 0), BlockPos::new(140, 64, 0), BlockPos::new(60, 64, -5)];
        assert_eq!(nearest_chest(&chests, &[], Vec3::new(120.0, 66.0, 10.0)), Some(chests[1]));
        assert_eq!(nearest_chest(&chests, &[], Vec3::new(55.0, 66.0, 3.0)), Some(chests[2]));
        assert_eq!(nearest_chest(&chests, &[chests[2]], Vec3::new(55.0, 66.0, 3.0)), Some(chests[0]), "that one was empty");
        assert_eq!(nearest_chest(&chests, &chests, Vec3::ZERO), None);
    }
}
