//! The mapart site, read off the world, whatever it is built of: the art is the platform one map
//! takes in - a map's 128 x 128 on the map grid, flat (find_art) - and the carpets go on it. Within
//! MARGIN of it, by their signs: the carpet chests (the colour alone, "RED", "LIGHT BLUE":
//! art::carpet_for_sign), "maps", "glass pane", "XP", "finished", "food", "used carpet", and
//! "clear" by the water button; by their blocks: the cartography table, the anvil and the bed.

use std::collections::BTreeMap;

use azalea::{
    BlockPos, Client, Vec3,
    registry::{builtin::BlockKind, tags},
    world::World,
};

use super::art::{self, SIDE};
use crate::chests;

/// Columns of a map's area looked at for its platform: every STEP-th each way.
const STEP: usize = 2;
/// Of those, the share in sight (loaded) to say, and the share at one height that makes it flat.
const SEEN: f64 = 0.8;
const FLAT: f64 = 0.9;
/// The platform is looked for this far above and below the bot.
const LOOK_UP: i32 = 48;
const LOOK_DOWN: i32 = 96;
/// The site's chests, signs, table, anvil, bed and button: this far round the art (x and z), and
/// this far above or below the carpet layer.
const MARGIN: i32 = 32;
const SPAN_Y: i32 = 32;

pub const MAPS: &str = "maps";
pub const GLASS: &str = "glass pane";
pub const XP: &str = "XP";
pub const FINISHED: &str = "finished";
pub const FOOD: &str = "food";
pub const USED: &str = "used carpet";
const CLEAR: &str = "clear";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Site {
    /// Where its chests and the rest are looked for: the art and MARGIN round it.
    pub min: BlockPos,
    pub max: BlockPos,
    /// The art's north-west carpet; its y is the carpet layer.
    pub art: BlockPos,
    /// Blocks in the lip round the art at carpet height: north, south, west, east, of 130 each.
    pub lip: [usize; 4],
    pub carpets: BTreeMap<BlockKind, Vec<BlockPos>>,
    /// The other chests, by their sign's word (MAPS, GLASS...).
    pub chests: BTreeMap<&'static str, Vec<BlockPos>>,
    pub table: Option<BlockPos>,
    pub anvil: Option<BlockPos>,
    pub button: Option<BlockPos>,
    pub bed: Option<BlockPos>,
}

impl Site {
    /// The carpet's place for pixel (x, z) of a map.
    pub fn at(&self, x: usize, z: usize) -> BlockPos {
        BlockPos::new(self.art.x + x as i32, self.art.y, self.art.z + z as i32)
    }

    /// The art's middle, `up` blocks over the carpet layer.
    pub fn middle(&self, up: f64) -> Vec3 {
        Vec3::new(self.art.x as f64 + SIDE as f64 / 2.0, self.art.y as f64 + up, self.art.z as f64 + SIDE as f64 / 2.0)
    }

    pub fn chests(&self, word: &str) -> &[BlockPos] {
        self.chests.get(word).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// What every map needs that was not found.
    pub fn missing(&self) -> Vec<&'static str> {
        let chests = [MAPS, GLASS, FINISHED].into_iter().filter(|w| self.chests(w).is_empty());
        let blocks = [("cartography table", self.table), ("anvil", self.anvil), ("water button", self.button), ("bed", self.bed)].into_iter().filter(|(_, at)| at.is_none()).map(|(what, _)| what);
        chests.chain(blocks).collect()
    }

    /// What it found and where, for the log only.
    pub fn describe(&self) -> String {
        let pos = |p: BlockPos| format!("{} {} {}", p.x, p.y, p.z);
        let mut out = format!(
            "art {} to {} (carpet layer y {}), site box {} to {}",
            pos(self.art),
            pos(BlockPos::new(self.art.x + SIDE as i32 - 1, self.art.y, self.art.z + SIDE as i32 - 1)),
            self.art.y,
            pos(self.min),
            pos(self.max)
        );
        let side = SIDE + 2;
        if self.lip.iter().any(|n| *n < side) {
            out += &format!(
                "
lip at carpet height: north {}/{side}, south {}/{side}, west {}/{side}, east {}/{side} - the north side shades the top row",
                self.lip[0], self.lip[1], self.lip[2], self.lip[3]
            );
        }
        let colours: Vec<String> =
            self.carpets.iter().map(|(k, v)| if v.len() > 1 { format!("{} x{}", art::colour(*k).to_uppercase(), v.len()) } else { art::colour(*k).to_uppercase() }).collect();
        out += &format!("\ncarpet chests ({} colours): {}", self.carpets.len(), if colours.is_empty() { "none".into() } else { colours.join(", ") });
        let have = |w: &str| if self.chests(w).is_empty() { format!("{w} MISSING") } else if self.chests(w).len() > 1 { format!("{w} x{}", self.chests(w).len()) } else { w.to_string() };
        out += &format!("\nchests: {}", [MAPS, GLASS, XP, FINISHED, FOOD].map(have).join(", "));
        out += &format!(", {}", if self.chests(USED).is_empty() { "no used carpet chest (washed carpets lie on the platform till they despawn)".to_string() } else { USED.to_string() });
        let found = |name: &str, p: Option<BlockPos>| match p {
            Some(p) => format!("{name} {}", pos(p)),
            None => format!("{name} MISSING"),
        };
        out += &format!(
            "\n{}, {}, {}, {}",
            found("cartography table", self.table),
            found("anvil", self.anvil),
            found("water button", self.button),
            found("bed", self.bed)
        );
        out
    }
}

/// The word on one of the other chests' signs (any case, on one line or across them), or CLEAR.
fn word(lines: &[String]) -> Option<&'static str> {
    let key: String = lines.concat().chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    match key.as_str() {
        "maps" | "map" | "emptymaps" | "emptymap" => Some(MAPS),
        "glasspane" | "glasspanes" | "glass" | "panes" => Some(GLASS),
        "xp" | "exp" | "xpbottles" | "experience" => Some(XP),
        "finished" | "finishedmaps" => Some(FINISHED),
        "food" => Some(FOOD),
        "usedcarpet" | "usedcarpets" => Some(USED),
        "clear" => Some(CLEAR),
        _ => None,
    }
}

/// What a column of a map's area shows from above.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Column {
    /// Not loaded.
    Unseen,
    /// Water or lava on top, or nothing in the heights looked at: no platform there.
    Open,
    /// The carpet layer over it: a carpet's own height, else one over its top block.
    Layer(i32),
}

/// A map's area as looked at: columns looked at and in sight, and its commonest carpet layer.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Look {
    looked: usize,
    seen: usize,
    layer: Option<(i32, usize)>,
}

impl Look {
    fn in_sight(&self) -> bool {
        self.seen as f64 >= self.looked as f64 * SEEN
    }

    /// Its carpet layer, if what is in sight of it is flat.
    fn flat(&self) -> Option<i32> {
        self.layer.filter(|(_, n)| *n > 0 && *n as f64 >= self.seen as f64 * FLAT).map(|(y, _)| y)
    }
}

/// The north-west corner (x, z) of the map that takes in x z: 64 past a multiple of 128.
pub fn map_corner(x: i32, z: i32) -> (i32, i32) {
    let edge = |v: i32| (v - 64).div_euclid(128) * 128 + 64;
    (edge(x), edge(z))
}

/// Look over the map whose north-west corner is `corner`, a column at a time.
fn look(corner: (i32, i32), mut column: impl FnMut(i32, i32) -> Column) -> Look {
    let mut layers: BTreeMap<i32, usize> = BTreeMap::new();
    let (mut looked, mut seen) = (0, 0);
    for dx in (0..SIDE as i32).step_by(STEP) {
        for dz in (0..SIDE as i32).step_by(STEP) {
            looked += 1;
            match column(corner.0 + dx, corner.1 + dz) {
                Column::Unseen => {}
                Column::Open => seen += 1,
                Column::Layer(y) => {
                    seen += 1;
                    *layers.entry(y).or_default() += 1;
                }
            }
        }
    }
    Look { looked, seen, layer: layers.into_iter().max_by_key(|(_, n)| *n) }
}

/// The column at x z from above, between `from` and `to` (heights).
fn column(w: &World, x: i32, z: i32, from: i32, to: i32) -> Column {
    for y in (to..=from).rev() {
        let Some(st) = w.get_block_state(BlockPos::new(x, y, z)) else { return Column::Unseen };
        if st.is_air() {
            continue;
        }
        let kind = BlockKind::from(st);
        if matches!(kind, BlockKind::Water | BlockKind::Lava | BlockKind::BubbleColumn) {
            return Column::Open;
        }
        return Column::Layer(if art::is_carpet(kind) { y } else { y + 1 });
    }
    Column::Open
}

/// The art's north-west carpet (its y the carpet layer): of the maps round the bot - the one it is
/// over and the eight round that - the one whose area is a flat platform, whatever it is built of
/// (the flattest, then the nearest). Err: why there is none, and where to look from - over the
/// middle of a map that is flat as far as it is in sight, else None.
fn find_art(w: &World, at: BlockPos) -> Result<BlockPos, (String, Option<Vec3>)> {
    let (bottom, top) = (w.chunks.min_y(), w.chunks.min_y() + w.chunks.height() as i32 - 1);
    let (from, to) = ((at.y + LOOK_UP).min(top), (at.y - LOOK_DOWN).max(bottom));
    let own = map_corner(at.x, at.z);
    let mut corners: Vec<(i32, i32)> = (-1..=1).flat_map(|i| (-1..=1).map(move |j| (own.0 + i * SIDE as i32, own.1 + j * SIDE as i32))).collect();
    let half = SIDE as i32 / 2;
    corners.sort_by_key(|c| (c.0 + half - at.x).pow(2) + (c.1 + half - at.z).pow(2));
    let mut half_seen = None;
    // (share of it at its layer in thousandths, the art): the first of the flattest wins.
    let mut best: Option<(usize, BlockPos)> = None;
    for c in &corners {
        let l = look(*c, |x, z| column(w, x, z, from, to));
        match (l.flat(), l.layer) {
            (Some(y), Some((_, n))) if l.in_sight() => {
                let share = n * 1000 / l.seen.max(1);
                if best.is_none_or(|(b, _)| share > b) {
                    best = Some((share, BlockPos::new(c.0, y, c.1)));
                }
            }
            (Some(y), _) if half_seen.is_none() => half_seen = Some(Vec3::new((c.0 + half) as f64, y as f64 + 3.0, (c.1 + half) as f64)),
            _ => {}
        }
    }
    if let Some((_, art)) = best {
        return Ok(art);
    }
    let why = "no flat 128 x 128 platform on the map grid round the bot - the art goes on one map's area exactly (its north-west corner at an x and z 64 past a multiple of 128)";
    Err((if half_seen.is_some() { "the platform is not all in sight yet".into() } else { why.into() }, half_seen))
}

/// Blocks in the lip round the art at carpet height (what shades the top row - its north side):
/// north, south, west, east, each of 130 (the corners counted on both of theirs).
fn lip(w: &World, art: BlockPos) -> [usize; 4] {
    let side = SIDE as i32;
    let (west, east, north, south) = (art.x - 1, art.x + side, art.z - 1, art.z + side);
    let solid = |x: i32, z: i32| {
        w.get_block_state(BlockPos::new(x, art.y, z)).is_some_and(|s| {
            let k = BlockKind::from(s);
            !s.is_air() && !art::is_carpet(k) && !matches!(k, BlockKind::Water | BlockKind::Lava | BlockKind::BubbleColumn)
        })
    };
    let along = |at: &dyn Fn(i32) -> (i32, i32)| (0..side + 2).filter(|i| solid(at(*i).0, at(*i).1)).count();
    [along(&|i| (west + i, north)), along(&|i| (west + i, south)), along(&|i| (west, north + i)), along(&|i| (east, north + i))]
}

/// How far the art sits east and south of the map grid: (0, 0) when one map takes in exactly it.
/// A map's west and north edges are at 64 past a multiple of 128 (..., -64, 64, 192, ...).
pub fn off_grid(art: BlockPos) -> (i32, i32) {
    ((art.x - 64).rem_euclid(128), (art.z - 64).rem_euclid(128))
}

/// In words, for an art area off the grid: where its corner is and where a map's would be.
pub fn grid_advice(art: BlockPos) -> String {
    let (ox, oz) = off_grid(art);
    let way = |off: i32, plus: &str, minus: &str| {
        if off == 0 {
            String::new()
        } else if off <= 64 {
            format!("{off} {minus}")
        } else {
            format!("{} {plus}", 128 - off)
        }
    };
    let moves: Vec<String> = [way(ox, "east", "west"), way(oz, "south", "north")].into_iter().filter(|s| !s.is_empty()).collect();
    format!(
        "the art area is not on the map grid: its north-west carpet is at x {}, z {}, and a map's north-west corner is at an x and z that are 64 past a multiple of 128 - the platform would have to move {} for one map to take in exactly it",
        art.x,
        art.z,
        moves.join(" and ")
    )
}

/// Read the site off the loaded world. Err says what is missing or wrong.
pub fn survey(bot: &Client) -> Result<Site, String> {
    let at = BlockPos::from(bot.position().map_err(|e| e.to_string())?);
    let (art, lip) = {
        let world = bot.world().map_err(|e| e.to_string())?;
        let w = world.read();
        let art = find_art(&w, at).map_err(|(why, _)| why)?;
        (art, lip(&w, art))
    };
    let mid = (art.x + SIDE as i32 / 2, art.z + SIDE as i32 / 2);
    let far = SIDE as i32 - 1 + MARGIN;
    let min = BlockPos::new(art.x - MARGIN, art.y - SPAN_Y, art.z - MARGIN);
    let max = BlockPos::new(art.x + far, art.y + SPAN_Y, art.z + far);
    let in_span = |p: BlockPos| p.x >= min.x && p.x <= max.x && p.z >= min.z && p.z <= max.z && p.y >= min.y && p.y <= max.y;
    let mut site = Site { min, max, art, lip, ..Default::default() };

    let mut clear_signs = Vec::new();
    for (pos, lines) in chests::sign_texts(bot) {
        if !in_span(pos) {
            continue;
        }
        let refs: Vec<&str> = lines.iter().map(|l| l.as_str()).collect();
        if let Some(kind) = art::carpet_for_sign(&refs) {
            if let Some(c) = chests::container_for_sign(bot, pos)
                && !site.carpets.get(&kind).is_some_and(|v| v.contains(&c))
            {
                site.carpets.entry(kind).or_default().push(c);
            }
            continue;
        }
        match word(&lines) {
            Some(CLEAR) => clear_signs.push(pos),
            Some(w) => {
                if let Some(c) = chests::container_for_sign(bot, pos)
                    && !site.chests.get(w).is_some_and(|v| v.contains(&c))
                {
                    site.chests.entry(w).or_default().push(c);
                }
            }
            None => {}
        }
    }

    let world = bot.world().map_err(|e| e.to_string())?;
    let w = world.read();
    let centre = BlockPos::new(mid.0, art.y, mid.1);
    let home = |p: &BlockPos| (p.x - at.x).pow(2) + (p.y - at.y).pow(2) + (p.z - at.z).pow(2);
    let nearest_home = |states: azalea::block::BlockStates| -> Option<BlockPos> {
        w.find_blocks(centre, &states).filter(|p| in_span(*p)).min_by_key(home)
    };
    site.table = nearest_home(BlockKind::CartographyTable.into());
    site.anvil = nearest_home((&tags::blocks::ANVIL).into());
    site.bed = nearest_home((&tags::blocks::BEDS).into());
    // The button: the one nearest a "clear" sign, within two blocks of it.
    site.button = clear_signs
        .iter()
        .flat_map(|s| {
            let mut near = Vec::new();
            for dx in -2..=2 {
                for dy in -2..=2 {
                    for dz in -2..=2 {
                        let p = BlockPos::new(s.x + dx, s.y + dy, s.z + dz);
                        if w.get_block_state(p).is_some_and(|st| tags::blocks::BUTTONS.contains(&BlockKind::from(st))) {
                            near.push((dx * dx + dy * dy + dz * dz, p));
                        }
                    }
                }
            }
            near
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, p)| p);
    Ok(site)
}

/// Where to see the whole site from: over the middle of the art, or of the platform part in
/// sight - None when no platform is in sight at all.
pub fn lookout(bot: &Client) -> Option<Vec3> {
    let at = BlockPos::from(bot.position().ok()?);
    let world = bot.world().ok()?;
    let w = world.read();
    match find_art(&w, at) {
        Ok(art) => Some(Vec3::new((art.x + SIDE as i32 / 2) as f64, art.y as f64 + 3.0, (art.z + SIDE as i32 / 2) as f64)),
        Err((_, spot)) => spot,
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

    /// A world with chunks x 3..=12, z -5..=4 loaded (blocks 48..207, -80..79), and `blocks` in it.
    fn world(blocks: &[(BlockPos, &str)]) -> (World, PartialChunkStorage) {
        let mut chunks = ChunkStorage::default();
        let mut partial = PartialChunkStorage::new(32);
        for x in 3..=12 {
            for z in -5..=4 {
                partial.set(&ChunkPos { x, z }, Some(Chunk::default()), &mut chunks);
            }
        }
        let w = World::from(chunks);
        for (p, s) in blocks {
            w.set_block_state(*p, parse_state(s).unwrap());
        }
        (w, partial)
    }

    /// A map's area (north-west corner 64, -64) floored with `floor` at y 70.
    fn platform(floor: &'static str) -> Vec<(BlockPos, &'static str)> {
        (64..192).flat_map(|x| (-64..64).map(move |z| (BlockPos::new(x, 70, z), floor))).collect()
    }

    #[test]
    fn the_map_a_block_is_in() {
        assert_eq!(map_corner(64, 64), (64, 64));
        assert_eq!(map_corner(63, 63), (-64, -64));
        assert_eq!(map_corner(191, -1), (64, -64));
        assert_eq!(map_corner(-65, 0), (-192, -64));
    }

    #[test]
    fn the_platform_is_found_whatever_it_is_built_of() {
        for floor in ["minecraft:white_concrete", "minecraft:ochre_froglight", "minecraft:dirt"] {
            let mut blocks = platform(floor);
            // Part of a map laid already, a dispenser in the floor, a lip at carpet height.
            blocks.extend((64..100).map(|x| (BlockPos::new(x, 71, 0), "minecraft:green_carpet")));
            blocks.push((BlockPos::new(100, 70, 10), "minecraft:dispenser[facing=up,triggered=false]"));
            blocks.extend((63..193).map(|x| (BlockPos::new(x, 71, -65), "minecraft:stone")));
            let (w, _keep) = world(&blocks);
            // From the bed, north of the art (the map north of it barely loaded).
            let art = find_art(&w, BlockPos::new(120, 72, -70)).unwrap_or_else(|(why, _)| panic!("{floor}: {why}"));
            assert_eq!(art, BlockPos::new(64, 71, -64), "{floor}");
            assert_eq!(lip(&w, art), [130, 0, 1, 1], "the north side only (its ends count on west and east)");
            assert_eq!(off_grid(art), (0, 0));
        }
    }

    #[test]
    fn rough_ground_or_water_is_no_platform() {
        let rough: Vec<(BlockPos, &str)> = (64..192).flat_map(|x| (-64..64).map(move |z| (BlockPos::new(x, 60 + (x * 7 + z * 3).rem_euclid(9), z), "minecraft:stone"))).collect();
        let (w, _keep) = world(&rough);
        assert!(find_art(&w, BlockPos::new(120, 72, 0)).unwrap_err().0.contains("no flat"));
        let sea: Vec<(BlockPos, &str)> = (64..192).flat_map(|x| (-64..64).map(move |z| (BlockPos::new(x, 62, z), "minecraft:water[level=0]"))).collect();
        let (w, _keep) = world(&sea);
        assert!(find_art(&w, BlockPos::new(120, 72, 0)).is_err());
    }

    #[test]
    fn a_platform_half_in_sight_is_looked_at_again_from_over_it() {
        let l = look((64, -64), |x, _| if x < 128 { Column::Layer(71) } else { Column::Unseen });
        assert_eq!(l.flat(), Some(71));
        assert!(!l.in_sight());
        let all = look((64, -64), |x, z| if (x + z) % 50 == 0 { Column::Layer(72) } else { Column::Layer(71) });
        assert!(all.in_sight());
        assert_eq!(all.flat(), Some(71), "a chest or two on it");
    }

    #[test]
    fn the_map_grid() {
        assert_eq!(off_grid(BlockPos::new(64, 80, 64)), (0, 0));
        assert_eq!(off_grid(BlockPos::new(68, 80, 60)), (4, 124));
        let advice = grid_advice(BlockPos::new(68, 80, 60));
        assert!(advice.contains("4 west") && advice.contains("4 south"), "{advice}");
    }

    #[test]
    fn the_other_chests_by_their_word() {
        let l = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(word(&l(&["MAPS"])), Some(MAPS));
        assert_eq!(word(&l(&["Glass", "Pane"])), Some(GLASS));
        assert_eq!(word(&l(&["XP"])), Some(XP));
        assert_eq!(word(&l(&["FINISHED"])), Some(FINISHED));
        assert_eq!(word(&l(&["USED CARPET"])), Some(USED));
        assert_eq!(word(&l(&["clear"])), Some(CLEAR));
        assert_eq!(word(&l(&["RED"])), None);
    }

    /// A survey straight after a join, the north side not in yet, is short of what a map needs.
    #[test]
    fn what_a_map_needs_and_was_not_seen() {
        let mut s = Site::default();
        assert_eq!(s.missing(), vec![MAPS, GLASS, FINISHED, "cartography table", "anvil", "water button", "bed"]);
        for w in [MAPS, GLASS, FINISHED, XP] {
            s.chests.insert(w, vec![BlockPos::new(0, 0, 0)]);
        }
        s.table = Some(BlockPos::new(1, 0, 0));
        s.anvil = Some(BlockPos::new(2, 0, 0));
        s.button = Some(BlockPos::new(3, 0, 0));
        assert_eq!(s.missing(), vec!["bed"]);
        s.bed = Some(BlockPos::new(4, 0, 0));
        assert!(s.missing().is_empty());
    }
}
