//! A mapart file, read and checked (.litematic, .schem or a structure .nbt): map art in carpet,
//! completely flat - the carpets are laid on water, all at one height, so a staircased map cannot
//! be built. One map is 128 x 128; a bigger piece (rebane's mapartcraft makes 2x2 and up) is a whole
//! number of maps each way and is split into its maps, each built and locked on its own - flat, a
//! map's top row matches the row above it, so the pieces line up when hung together. Everything that
//! is not carpet is left out: the support layer mapart sites put under the carpets, and the extra
//! line along the north edge (for the top row's shading) - the site's water and its own edge blocks
//! do those jobs.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use azalea::registry::builtin::BlockKind;

use crate::print::schem;

/// A map is 128 x 128 blocks.
pub const SIDE: usize = 128;
pub const PIXELS: usize = SIDE * SIDE;
/// The most maps one file may hold (8 x 8): a day and more of building.
pub const MAX_MAPS: usize = 64;

/// One map of a piece: its place (column, row from the north-west) and its pixels.
#[derive(Clone, Debug)]
pub struct Map {
    pub col: usize,
    pub row: usize,
    /// Each pixel's carpet, row by row from the map's north-west corner (index z * SIDE + x);
    /// None where the file has none - the water shows there.
    pub pixels: Vec<Option<BlockKind>>,
}

impl Map {
    pub fn at(&self, x: usize, z: usize) -> Option<BlockKind> {
        self.pixels[z * SIDE + x]
    }

    pub fn carpets(&self) -> usize {
        self.pixels.iter().flatten().count()
    }

    /// How many of each carpet, most first.
    pub fn counts(&self) -> Vec<(BlockKind, usize)> {
        tally(self.pixels.iter().flatten().copied())
    }
}

/// A whole file: `cols` x `rows` maps.
#[derive(Clone, Debug)]
pub struct Art {
    pub cols: usize,
    pub rows: usize,
    /// Row by row from the north-west.
    pub maps: Vec<Map>,
    /// What was left out, by block id, with how many.
    pub ignored: BTreeMap<String, usize>,
}

impl Art {
    pub fn carpets(&self) -> usize {
        self.maps.iter().map(Map::carpets).sum()
    }

    pub fn counts(&self) -> Vec<(BlockKind, usize)> {
        tally(self.maps.iter().flat_map(|m| m.pixels.iter().flatten().copied()))
    }

    /// "flat 2x2 carpet map art (4 maps, built one after another), 65,536 carpets in 12 colours",
    /// the colours with their counts and stacks, and what was left out - for the answer on Discord.
    pub fn summary(&self) -> String {
        let counts = self.counts();
        let colours: Vec<String> = counts.iter().map(|(k, n)| format!("{} {} ({})", colour(*k), grouped(*n), stacks(*n))).collect();
        let holes = self.maps.len() * PIXELS - self.carpets();
        let size = if self.maps.len() == 1 {
            "flat 128x128 carpet map".to_string()
        } else {
            format!("flat {}x{} carpet map art ({} maps, built one after another)", self.cols, self.rows, self.maps.len())
        };
        let mut s = format!(
            "{size}, {} carpets in {} colour{}{}\n{}",
            grouped(self.carpets()),
            counts.len(),
            if counts.len() == 1 { "" } else { "s" },
            if holes > 0 { format!(" ({} pixel(s) left as water)", grouped(holes)) } else { String::new() },
            colours.join(" · "),
        );
        if !self.ignored.is_empty() {
            let left: Vec<String> = self.ignored.iter().map(|(id, n)| format!("{} {}", grouped(*n), id.replace('_', " "))).collect();
            s += &format!("\nleft out (not carpet - the support layer and the extra line): {}", left.join(", "));
        }
        s
    }
}

fn tally(carpets: impl Iterator<Item = BlockKind>) -> Vec<(BlockKind, usize)> {
    let mut by: BTreeMap<BlockKind, usize> = BTreeMap::new();
    for k in carpets {
        *by.entry(k).or_default() += 1;
    }
    let mut v: Vec<(BlockKind, usize)> = by.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// "green_carpet" for minecraft:green_carpet.
pub fn block_id(kind: BlockKind) -> &'static str {
    let s = kind.to_str();
    s.strip_prefix("minecraft:").unwrap_or(s)
}

pub fn is_carpet(kind: BlockKind) -> bool {
    block_id(kind).ends_with("_carpet")
}

/// "green" for green carpet, "moss" for moss carpet - the word its chest's sign carries.
pub fn colour(kind: BlockKind) -> String {
    block_id(kind).trim_end_matches("_carpet").replace('_', " ")
}

/// Every carpet there is.
pub const CARPETS: [BlockKind; 18] = [
    BlockKind::WhiteCarpet,
    BlockKind::OrangeCarpet,
    BlockKind::MagentaCarpet,
    BlockKind::LightBlueCarpet,
    BlockKind::YellowCarpet,
    BlockKind::LimeCarpet,
    BlockKind::PinkCarpet,
    BlockKind::GrayCarpet,
    BlockKind::LightGrayCarpet,
    BlockKind::CyanCarpet,
    BlockKind::PurpleCarpet,
    BlockKind::BlueCarpet,
    BlockKind::BrownCarpet,
    BlockKind::GreenCarpet,
    BlockKind::RedCarpet,
    BlockKind::BlackCarpet,
    BlockKind::MossCarpet,
    BlockKind::PaleMossCarpet,
];

/// The carpet a chest's sign names. The sign is the colour alone, in any case ("RED", "LIME",
/// "LIGHT BLUE" - the user, 6 Nov), its lines read together, so "LIGHT" over "BLUE" is light blue;
/// "RED CARPET" and "LIGHT_BLUE" do too. The whole sign has to be the colour: "GRAY" is never
/// light gray's chest, nor "USED CARPET" anyone's.
pub fn carpet_for_sign(lines: &[&str]) -> Option<BlockKind> {
    let key = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let text = key(&lines.concat());
    let text = text.strip_suffix("carpet").unwrap_or(&text);
    CARPETS.into_iter().find(|k| key(&colour(*k)) == text)
}

/// "16,384"
pub fn grouped(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Stacks of 64 it takes, the last one partial.
pub fn stacks(n: usize) -> usize {
    n.div_ceil(64)
}

/// Read and check a file. Err: why it cannot be built, in words for the person who sent it.
pub fn read(path: &Path) -> Result<Art, String> {
    let s = schem::load(path).map_err(|e| format!("it cannot be read ({e}) - send a .litematic, .schem or .nbt"))?;
    let mut blocks = Vec::new();
    for r in &s.regions {
        let o = r.min_offset();
        for ((x, y, z), st) in &r.blocks {
            blocks.push(((r.pos.0 + o.0 + x, r.pos.1 + o.1 + y, r.pos.2 + o.2 + z), BlockKind::from(*st)));
        }
    }
    let mut art = check(blocks)?;
    for (name, n) in s.unknown {
        *art.ignored.entry(name.trim_start_matches("minecraft:").to_string()).or_default() += n;
    }
    Ok(art)
}

/// The check itself, on every block of the file (its position, its block).
pub fn check(blocks: impl IntoIterator<Item = ((i32, i32, i32), BlockKind)>) -> Result<Art, String> {
    let mut carpets = Vec::new();
    let mut ignored: BTreeMap<String, usize> = BTreeMap::new();
    for (p, k) in blocks {
        if is_carpet(k) {
            carpets.push((p, k));
        } else {
            *ignored.entry(block_id(k).to_string()).or_default() += 1;
        }
    }
    if carpets.is_empty() {
        return Err("there is no carpet in it - only carpet maps can be built".into());
    }
    let heights: BTreeSet<i32> = carpets.iter().map(|(p, _)| p.1).collect();
    if heights.len() > 1 {
        return Err(format!(
            "it is staircased: its carpets are on {} different heights. Only completely flat maps can be built (the carpets go on water) - export it flat",
            heights.len()
        ));
    }
    let (x0, x1) = (carpets.iter().map(|(p, _)| p.0).min().unwrap_or(0), carpets.iter().map(|(p, _)| p.0).max().unwrap_or(0));
    let (z0, z1) = (carpets.iter().map(|(p, _)| p.2).min().unwrap_or(0), carpets.iter().map(|(p, _)| p.2).max().unwrap_or(0));
    let (w, l) = ((x1 - x0 + 1) as usize, (z1 - z0 + 1) as usize);
    if !w.is_multiple_of(SIDE) || !l.is_multiple_of(SIDE) {
        return Err(format!("its carpets cover {w} x {l} blocks - a map is 128 x 128, so map art is a whole number of maps each way (128, 256, 384 ...)"));
    }
    let (cols, rows) = (w / SIDE, l / SIDE);
    if cols * rows > MAX_MAPS {
        return Err(format!("it is {cols}x{rows} - {} maps; send at most {MAX_MAPS} at a time (split it into smaller pieces)", cols * rows));
    }
    let mut maps: Vec<Map> = (0..rows).flat_map(|row| (0..cols).map(move |col| Map { col, row, pixels: vec![None; PIXELS] })).collect();
    for ((x, _, z), k) in carpets {
        let (dx, dz) = ((x - x0) as usize, (z - z0) as usize);
        maps[(dz / SIDE) * cols + dx / SIDE].pixels[(dz % SIDE) * SIDE + dx % SIDE] = Some(k);
    }
    Ok(Art { cols, rows, maps, ignored })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cols` x `rows` maps the way the mapart sites lay them out: cobblestone under every carpet and
    /// a two-high cobblestone line north of it all; carpet height from `y_of`.
    fn site_export(cols: i32, rows: i32, y_of: impl Fn(i32, i32) -> i32) -> Vec<((i32, i32, i32), BlockKind)> {
        let mut v = Vec::new();
        for x in 0..128 * cols {
            v.push(((x, 1, 0), BlockKind::Cobblestone));
            v.push(((x, 2, 0), BlockKind::Cobblestone));
            for z in 1..=128 * rows {
                let kind = if (x + z) % 7 == 0 { BlockKind::WhiteCarpet } else { BlockKind::GreenCarpet };
                v.push(((x, y_of(x, z) - 1, z), BlockKind::Cobblestone));
                v.push(((x, y_of(x, z), z), kind));
            }
        }
        v
    }

    #[test]
    fn a_flat_carpet_map_is_read_without_its_support_and_extra_line() {
        let art = check(site_export(1, 1, |_, _| 2)).unwrap();
        assert_eq!((art.cols, art.rows, art.maps.len()), (1, 1, 1));
        let map = &art.maps[0];
        assert_eq!(map.carpets(), PIXELS);
        // The north-west carpet (file z 1) is the map's first pixel.
        assert_eq!(map.at(0, 0), Some(BlockKind::GreenCarpet));
        assert_eq!(map.at(6, 0), Some(BlockKind::WhiteCarpet), "x 6 + z 1 = 7");
        assert_eq!(art.ignored.get("cobblestone"), Some(&(128 * 128 + 256)));
        let counts = art.counts();
        assert_eq!(counts[0].0, BlockKind::GreenCarpet);
        assert_eq!(counts.iter().map(|c| c.1).sum::<usize>(), PIXELS);
    }

    #[test]
    fn a_two_by_two_is_split_into_its_four_maps() {
        let art = check(site_export(2, 2, |_, _| 5)).unwrap();
        assert_eq!((art.cols, art.rows), (2, 2));
        assert_eq!(art.maps.iter().map(|m| (m.col, m.row)).collect::<Vec<_>>(), vec![(0, 0), (1, 0), (0, 1), (1, 1)]);
        assert!(art.maps.iter().all(|m| m.carpets() == PIXELS));
        // The south-east map's pixel (px, pz) is the file's x 128 + px, z 129 + pz (z 0 is the extra line).
        let expected = |x: i32, z: i32| Some(if (x + z) % 7 == 0 { BlockKind::WhiteCarpet } else { BlockKind::GreenCarpet });
        let se = &art.maps[3];
        for (px, pz) in [(0, 0), (5, 2), (127, 127), (3, 4)] {
            assert_eq!(se.at(px, pz), expected(128 + px as i32, 129 + pz as i32), "pixel {px},{pz}");
        }
        assert!(art.summary().contains("2x2") && art.summary().contains("4 maps"));
    }

    #[test]
    fn a_staircased_map_is_refused() {
        let e = check(site_export(1, 1, |x, _| if x < 64 { 2 } else { 3 })).unwrap_err();
        assert!(e.contains("staircased") && e.contains("2 different heights"), "{e}");
    }

    #[test]
    fn only_whole_maps_are_taken() {
        let small: Vec<_> = (0..64).flat_map(|x| (0..64).map(move |z| ((x, 0, z), BlockKind::RedCarpet))).collect();
        assert!(check(small).unwrap_err().contains("64 x 64"));
        let odd: Vec<_> = (0..200).flat_map(|x| (0..128).map(move |z| ((x, 0, z), BlockKind::RedCarpet))).collect();
        assert!(check(odd).unwrap_err().contains("200 x 128"));
        let none = vec![((0, 0, 0), BlockKind::Stone)];
        assert!(check(none).unwrap_err().contains("no carpet"));
    }

    #[test]
    fn numbers_read_the_way_people_write_them() {
        assert_eq!(grouped(16384), "16,384");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000_000), "1,000,000");
        assert_eq!(stacks(6957), 109);
        assert_eq!(colour(BlockKind::LightBlueCarpet), "light blue");
    }

    #[test]
    fn a_carpet_chest_is_signed_with_its_colour_alone() {
        assert_eq!(carpet_for_sign(&["RED"]), Some(BlockKind::RedCarpet));
        assert_eq!(carpet_for_sign(&["", "LIME", "", ""]), Some(BlockKind::LimeCarpet));
        assert_eq!(carpet_for_sign(&["GRAY"]), Some(BlockKind::GrayCarpet));
        assert_eq!(carpet_for_sign(&["LIGHT GRAY"]), Some(BlockKind::LightGrayCarpet));
        assert_eq!(carpet_for_sign(&["BLUE"]), Some(BlockKind::BlueCarpet));
        assert_eq!(carpet_for_sign(&["LIGHT", "BLUE"]), Some(BlockKind::LightBlueCarpet), "over two lines");
        assert_eq!(carpet_for_sign(&["light_blue"]), Some(BlockKind::LightBlueCarpet));
        assert_eq!(carpet_for_sign(&["Red Carpet"]), Some(BlockKind::RedCarpet));
        assert_eq!(carpet_for_sign(&["PALE MOSS"]), Some(BlockKind::PaleMossCarpet));
        assert_eq!(carpet_for_sign(&["USED CARPET"]), None);
        assert_eq!(carpet_for_sign(&["FINISHED"]), None);
        assert_eq!(carpet_for_sign(&["LIME GREEN"]), None);
        assert_eq!(carpet_for_sign(&[""]), None);
    }

    /// The example export from a mapart site - run when the file is in the folder.
    #[test]
    fn the_example_export_reads() {
        let path = Path::new("mapart example.nbt");
        if !path.exists() {
            return;
        }
        let art = read(path).unwrap();
        assert_eq!(art.maps.len(), 1);
        assert_eq!(art.carpets(), PIXELS);
        assert_eq!(art.counts().len(), 12);
        assert_eq!(art.counts()[0], (BlockKind::GreenCarpet, 6957));
        assert_eq!(art.ignored.get("cobblestone"), Some(&16640));
        println!("{}", art.summary());
    }
}
