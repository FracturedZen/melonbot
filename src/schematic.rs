//! The schematics, read straight from the files: Litematica `.litematic` and Sponge `.schem`
//! (v2 and v3). Only what the melon routine needs - the melon cells, by layer - and the size the
//! Java side measured (w/h/d over every non-air block), so the build sites it saved still fit.

use std::{collections::BTreeMap, fs, io::Cursor, io::Read, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use simdnbt::owned::{NbtCompound, NbtTag};

/// A loaded build: melon cells relative to its origin, by layer (relative y).
pub struct Plan {
    pub name: String,
    pub w: i32,
    pub h: i32,
    pub d: i32,
    pub layers: BTreeMap<i32, Vec<(i32, i32, i32)>>,
    pub total: usize,
}

pub fn load(path: &Path) -> Result<Plan> {
    load_as(path, false)
}

/// Every block of the file a melon cell - for art drawn in another block (ceiling art drawn in
/// emerald blocks, say).
pub fn load_any(path: &Path) -> Result<Plan> {
    load_as(path, true)
}

fn load_as(path: &Path, any: bool) -> Result<Plan> {
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    // Both formats are gzipped NBT.
    let mut data = Vec::new();
    flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut data).context("not gzipped NBT")?;
    let nbt = simdnbt::owned::read(&mut Cursor::new(&data[..])).map_err(|e| anyhow!("bad NBT: {e:?}"))?;
    let simdnbt::owned::Nbt::Some(root) = nbt else { bail!("empty NBT") };
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut cells: Vec<((i32, i32, i32), bool)> = Vec::new(); // (rel pos, is melon) for every non-air block
    if root.compound("Regions").is_some() {
        litematic(&root, &mut cells)?;
    } else {
        sponge(root.compound("Schematic").unwrap_or(&root), &mut cells)?;
    }
    let (mut w, mut h, mut d) = (0, 0, 0);
    let mut layers: BTreeMap<i32, Vec<(i32, i32, i32)>> = BTreeMap::new();
    let mut total = 0;
    for ((x, y, z), melon) in cells {
        w = w.max(x + 1);
        h = h.max(y + 1);
        d = d.max(z + 1);
        if melon || any {
            layers.entry(y).or_default().push((x, y, z));
            total += 1;
        }
    }
    Ok(Plan { name, w, h, d, layers, total })
}

fn is_air(name: &str) -> bool {
    matches!(name, "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air" | "minecraft:structure_void")
}

fn base_name(name: &str) -> &str {
    name.split('[').next().unwrap_or(name)
}

/// Litematica: each region has a Position, a (possibly negative) Size, a palette and a tightly
/// packed long array (entries may span two longs), index = (y * sizeZ + z) * sizeX + x.
fn litematic(root: &NbtCompound, out: &mut Vec<((i32, i32, i32), bool)>) -> Result<()> {
    let regions = root.compound("Regions").unwrap();
    for (_, region) in regions.iter() {
        let NbtTag::Compound(r) = region else { continue };
        let pos = r.compound("Position").ok_or(anyhow!("region without Position"))?;
        let size = r.compound("Size").ok_or(anyhow!("region without Size"))?;
        let (px, py, pz) = (pos.int("x").unwrap_or(0), pos.int("y").unwrap_or(0), pos.int("z").unwrap_or(0));
        let (sx, sy, sz) = (size.int("x").unwrap_or(0), size.int("y").unwrap_or(0), size.int("z").unwrap_or(0));
        let (ax, ay, az) = (sx.abs(), sy.abs(), sz.abs());
        // A negative size grows back from the position.
        let (mx, my, mz) = (
            px + if sx < 0 { sx + 1 } else { 0 },
            py + if sy < 0 { sy + 1 } else { 0 },
            pz + if sz < 0 { sz + 1 } else { 0 },
        );
        let palette: Vec<String> = r
            .list("BlockStatePalette")
            .and_then(|l| l.compounds())
            .ok_or(anyhow!("region without BlockStatePalette"))?
            .iter()
            .map(|c| c.string("Name").map(|s| s.to_str().to_string()).unwrap_or_default())
            .collect();
        let longs = r.long_array("BlockStates").ok_or(anyhow!("region without BlockStates"))?;
        let bits = (usize::BITS - (palette.len().max(1) - 1).leading_zeros()).max(2) as usize;
        let mask: u64 = (1u64 << bits) - 1;
        let n = (ax as usize) * (ay as usize) * (az as usize);
        for i in 0..n {
            let start = i * bits;
            let (si, ei) = (start >> 6, (start + bits - 1) >> 6);
            let off = start & 63;
            let a = *longs.get(si).unwrap_or(&0) as u64;
            let v = if si == ei {
                (a >> off) & mask
            } else {
                let b = *longs.get(ei).unwrap_or(&0) as u64;
                ((a >> off) | (b << (64 - off))) & mask
            };
            let Some(name) = palette.get(v as usize) else { continue };
            if is_air(name) {
                continue;
            }
            let x = (i % ax as usize) as i32;
            let z = ((i / ax as usize) % az as usize) as i32;
            let y = (i / (ax as usize * az as usize)) as i32;
            out.push(((mx + x, my + y, mz + z), base_name(name) == "minecraft:melon"));
        }
    }
    Ok(())
}

/// Sponge v2 (Palette/BlockData at the top) and v3 (under Blocks): varint palette indices,
/// index = (y * Length + z) * Width + x.
fn sponge(s: &NbtCompound, out: &mut Vec<((i32, i32, i32), bool)>) -> Result<()> {
    let w = s.short("Width").ok_or(anyhow!("no Width"))? as u16 as i32;
    let h = s.short("Height").ok_or(anyhow!("no Height"))? as u16 as i32;
    let l = s.short("Length").ok_or(anyhow!("no Length"))? as u16 as i32;
    let (palette_c, data) = match s.compound("Blocks") {
        Some(b) => (b.compound("Palette"), b.byte_array("Data")),
        None => (s.compound("Palette"), s.byte_array("BlockData")),
    };
    let palette_c = palette_c.ok_or(anyhow!("no Palette"))?;
    let data = data.ok_or(anyhow!("no block data"))?;
    let mut palette: Vec<String> = Vec::new();
    for (k, v) in palette_c.iter() {
        if let NbtTag::Int(i) = v {
            let i = *i as usize;
            if palette.len() <= i {
                palette.resize(i + 1, String::new());
            }
            palette[i] = k.to_str().to_string();
        }
    }
    let (mut i, mut idx) = (0usize, 0usize);
    while i < data.len() {
        let mut v: u32 = 0;
        let mut shift = 0;
        loop {
            let b = data[i];
            i += 1;
            v |= ((b & 0x7f) as u32) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let name = palette.get(v as usize).map(String::as_str).unwrap_or("minecraft:air");
        if !is_air(name) {
            let x = (idx % w as usize) as i32;
            let z = ((idx / w as usize) % l as usize) as i32;
            let y = (idx / (w as usize * l as usize)) as i32;
            if y < h {
                out.push(((x, y, z), base_name(name) == "minecraft:melon"));
            }
        }
        idx += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ceiling art as the routine reads it - run when the file is on this machine.
    #[test]
    fn the_ceiling_art_loads_every_block_as_a_melon() {
        let Some(path) = std::env::var_os("MELONBOT_TEST_SCHEM").map(std::path::PathBuf::from) else { return };
        if !path.exists() {
            return;
        }
        let p = load_any(&path).unwrap();
        let counts: Vec<(i32, usize)> = p.layers.iter().map(|(y, v)| (*y, v.len())).collect();
        println!("{} {}x{}x{}, {} cells, by layer {counts:?}", p.name, p.w, p.h, p.d, p.total);
        assert!(p.total > 0);
        assert_eq!(load(&path).unwrap().total, 0, "no melon blocks in it: only load_any can print it");
    }
}
