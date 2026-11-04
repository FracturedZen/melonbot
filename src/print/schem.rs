//! Schematics with every block state, for the swarm printer: Litematica `.litematic` (every region),
//! Sponge `.schem` (v2 and v3) and vanilla structure `.nbt`. The melon farmer's melon-only reader is
//! crate::schematic; this one keeps what the standalone printer needs - the full state of every
//! block, by region, as Litematica sees it.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{Cursor, Read},
    path::Path,
    str::FromStr,
};

use anyhow::{Context, Result, anyhow, bail};
use azalea::{block::BlockState, registry::builtin::BlockKind};
use simdnbt::owned::{NbtCompound, NbtTag};

/// One region of a schematic. Sponge and structure files have one, at the origin.
#[derive(Debug, Clone)]
pub struct Region {
    pub name: String,
    /// Litematica's region Position, relative to the schematic's origin (its default sub-region
    /// placement). The blocks start at its min corner: `pos + (size < 0 ? size + 1 : 0)` per axis.
    pub pos: (i32, i32, i32),
    /// As stored: Litematica sizes may be negative (the region grows back from `pos`).
    pub size: (i32, i32, i32),
    /// Every block that is not air or structure void, at its container position (0..|size|).
    pub blocks: Vec<((i32, i32, i32), BlockState)>,
}

impl Region {
    /// The container's offset from `pos`: where index (0,0,0) sits relative to the region Position.
    pub fn min_offset(&self) -> (i32, i32, i32) {
        let f = |s: i32| if s < 0 { s + 1 } else { 0 };
        (f(self.size.0), f(self.size.1), f(self.size.2))
    }
}

#[derive(Debug, Clone)]
pub struct Schem {
    pub name: String,
    pub regions: Vec<Region>,
    /// Palette names this client does not know (another version's or a mod's), with how many
    /// blocks used them - left out of `regions`.
    pub unknown: BTreeMap<String, usize>,
}

impl Schem {
    pub fn block_count(&self) -> usize {
        self.regions.iter().map(|r| r.blocks.len()).sum()
    }
}

pub fn load(path: &Path) -> Result<Schem> {
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    // All three are gzipped NBT (a structure file may also be plain).
    let mut data = Vec::new();
    if flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut data).is_err() {
        data = raw;
    }
    let nbt = simdnbt::owned::read(&mut Cursor::new(&data[..])).map_err(|e| anyhow!("bad NBT: {e:?}"))?;
    let simdnbt::owned::Nbt::Some(root) = nbt else { bail!("empty NBT") };
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut unknown = BTreeMap::new();
    let regions = if root.compound("Regions").is_some() {
        litematic(&root, &mut unknown)?
    } else if root.list("palette").is_some() || root.list("palettes").is_some() {
        vec![structure(&root, &mut unknown)?]
    } else {
        vec![sponge(root.compound("Schematic").unwrap_or(&root), &mut unknown)?]
    };
    Ok(Schem { name, regions, unknown })
}

/// A palette entry as a block state: the block by id, then each property that block has (an
/// unknown property or value is left at the block's default, as Minecraft's own reader does).
pub fn state_of<'a>(name: &str, props: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<BlockState> {
    let kind = BlockKind::from_str(name.trim()).ok()?;
    let mut b = BlockState::from(kind).to_trait().boxed();
    for (k, v) in props {
        let _ = b.set_property(k, v);
    }
    Some(b.as_block_state())
}

/// "minecraft:oak_stairs[facing=north,half=top]" (Sponge palettes, Litematica-style strings).
pub fn parse_state(s: &str) -> Option<BlockState> {
    let s = s.trim();
    match s.split_once('[') {
        None => state_of(s, []),
        Some((name, rest)) => {
            let props = rest.trim_end_matches(']');
            state_of(name, props.split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.trim(), v.trim())))
        }
    }
}

fn skipped(name: &str) -> bool {
    let n = name.split('[').next().unwrap_or(name).trim();
    let n = n.strip_prefix("minecraft:").unwrap_or(n);
    // "empty": what some map-art generators write for nothing.
    matches!(n, "air" | "cave_air" | "void_air" | "structure_void" | "empty")
}

/// A palette resolved once: Some(state) to keep, None to skip (air, structure void, unknown).
fn resolve(names: &[(String, Option<BlockState>)], i: usize, unknown_used: &mut HashMap<usize, usize>) -> Option<BlockState> {
    let (_, st) = names.get(i)?;
    if st.is_none() {
        *unknown_used.entry(i).or_default() += 1;
    }
    *st
}

fn tally(names: &[(String, Option<BlockState>)], used: HashMap<usize, usize>, unknown: &mut BTreeMap<String, usize>) {
    for (i, n) in used {
        if let Some((name, None)) = names.get(i)
            && !skipped(name)
        {
            *unknown.entry(name.clone()).or_default() += n;
        }
    }
}

/// Litematica: each region has a Position, a (possibly negative) Size, a palette of {Name,
/// Properties} and a tightly packed long array (an entry may span two longs), index = (y * sizeZ +
/// z) * sizeX + x.
fn litematic(root: &NbtCompound, unknown: &mut BTreeMap<String, usize>) -> Result<Vec<Region>> {
    let regions = root.compound("Regions").unwrap();
    let mut out = Vec::new();
    for (rname, region) in regions.iter() {
        let NbtTag::Compound(r) = region else { continue };
        let pos = r.compound("Position").ok_or(anyhow!("region without Position"))?;
        let size = r.compound("Size").ok_or(anyhow!("region without Size"))?;
        let p = (pos.int("x").unwrap_or(0), pos.int("y").unwrap_or(0), pos.int("z").unwrap_or(0));
        let s = (size.int("x").unwrap_or(0), size.int("y").unwrap_or(0), size.int("z").unwrap_or(0));
        let (ax, ay, az) = (s.0.unsigned_abs() as usize, s.1.unsigned_abs() as usize, s.2.unsigned_abs() as usize);
        let palette: Vec<(String, Option<BlockState>)> = r
            .list("BlockStatePalette")
            .and_then(|l| l.compounds())
            .ok_or(anyhow!("region without BlockStatePalette"))?
            .iter()
            .map(|c| {
                let name = c.string("Name").map(|s| s.to_str().to_string()).unwrap_or_default();
                let props: Vec<(String, String)> = c
                    .compound("Properties")
                    .map(|p| p.iter().filter_map(|(k, v)| v.string().map(|v| (k.to_str().to_string(), v.to_str().to_string()))).collect())
                    .unwrap_or_default();
                let st = if skipped(&name) { None } else { state_of(&name, props.iter().map(|(k, v)| (k.as_str(), v.as_str()))) };
                (name, st)
            })
            .collect();
        let longs = r.long_array("BlockStates").ok_or(anyhow!("region without BlockStates"))?;
        let bits = (usize::BITS - (palette.len().max(1) - 1).leading_zeros()).max(2) as usize;
        let mask: u64 = (1u64 << bits) - 1;
        let n = ax * ay * az;
        let mut blocks = Vec::new();
        let mut used = HashMap::new();
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
            if v == 0 && palette.first().is_some_and(|(n, _)| skipped(n)) {
                continue;
            }
            let Some(st) = resolve(&palette, v as usize, &mut used) else { continue };
            let x = (i % ax) as i32;
            let z = ((i / ax) % az) as i32;
            let y = (i / (ax * az)) as i32;
            blocks.push(((x, y, z), st));
        }
        tally(&palette, used, unknown);
        out.push(Region { name: rname.to_str().to_string(), pos: p, size: s, blocks });
    }
    Ok(out)
}

/// Sponge v2 (Palette/BlockData at the top) and v3 (under Blocks): palette keys are state strings,
/// data is varint indices, index = (y * Length + z) * Width + x.
fn sponge(s: &NbtCompound, unknown: &mut BTreeMap<String, usize>) -> Result<Region> {
    let w = s.short("Width").ok_or(anyhow!("no Width"))? as u16 as i32;
    let h = s.short("Height").ok_or(anyhow!("no Height"))? as u16 as i32;
    let l = s.short("Length").ok_or(anyhow!("no Length"))? as u16 as i32;
    let (palette_c, data) = match s.compound("Blocks") {
        Some(b) => (b.compound("Palette"), b.byte_array("Data")),
        None => (s.compound("Palette"), s.byte_array("BlockData")),
    };
    let palette_c = palette_c.ok_or(anyhow!("no Palette"))?;
    let data = data.ok_or(anyhow!("no block data"))?;
    let mut palette: Vec<(String, Option<BlockState>)> = Vec::new();
    for (k, v) in palette_c.iter() {
        if let NbtTag::Int(i) = v {
            let i = *i as usize;
            if palette.len() <= i {
                palette.resize(i + 1, (String::new(), None));
            }
            let name = k.to_str().to_string();
            let st = if skipped(&name) { None } else { parse_state(&name) };
            palette[i] = (name, st);
        }
    }
    let (mut i, mut idx) = (0usize, 0usize);
    let mut blocks = Vec::new();
    let mut used = HashMap::new();
    while i < data.len() {
        let mut v: u32 = 0;
        let mut shift = 0;
        loop {
            let b = data[i];
            i += 1;
            v |= ((b & 0x7f) as u32) << shift;
            if b & 0x80 == 0 || i >= data.len() {
                break;
            }
            shift += 7;
        }
        let x = (idx % w as usize) as i32;
        let z = ((idx / w as usize) % l as usize) as i32;
        let y = (idx / (w as usize * l as usize)) as i32;
        idx += 1;
        if y >= h {
            continue;
        }
        if let Some(st) = resolve(&palette, v as usize, &mut used) {
            blocks.push(((x, y, z), st));
        }
    }
    tally(&palette, used, unknown);
    Ok(Region { name: "Sponge".into(), pos: (0, 0, 0), size: (w, h, l), blocks })
}

/// Vanilla structure block files: size [x,y,z], palette [{Name, Properties}] (or the first of
/// "palettes"), blocks [{pos [x,y,z], state}].
fn structure(root: &NbtCompound, unknown: &mut BTreeMap<String, usize>) -> Result<Region> {
    let size = root.list("size").and_then(|l| l.ints()).ok_or(anyhow!("no size"))?;
    let (sx, sy, sz) = (*size.first().unwrap_or(&0), *size.get(1).unwrap_or(&0), *size.get(2).unwrap_or(&0));
    let pal_list = match root.list("palette") {
        Some(p) => p.compounds(),
        None => root.list("palettes").and_then(|l| l.lists()).and_then(|ls| ls.first().and_then(|p| p.compounds())),
    };
    let palette: Vec<(String, Option<BlockState>)> = pal_list
        .unwrap_or(&[])
        .iter()
        .map(|c| {
            let name = c.string("Name").map(|s| s.to_str().to_string()).unwrap_or_default();
            let props: Vec<(String, String)> = c
                .compound("Properties")
                .map(|p| p.iter().filter_map(|(k, v)| v.string().map(|v| (k.to_str().to_string(), v.to_str().to_string()))).collect())
                .unwrap_or_default();
            let st = if skipped(&name) { None } else { state_of(&name, props.iter().map(|(k, v)| (k.as_str(), v.as_str()))) };
            (name, st)
        })
        .collect();
    let mut blocks = Vec::new();
    let mut used = HashMap::new();
    for b in root.list("blocks").and_then(|l| l.compounds()).unwrap_or(&[]) {
        let Some(p) = b.list("pos").and_then(|l| l.ints()) else { continue };
        let Some(si) = b.int("state") else { continue };
        if let Some(st) = resolve(&palette, si as usize, &mut used)
            && p.len() == 3
        {
            blocks.push(((p[0], p[1], p[2]), st));
        }
    }
    tally(&palette, used, unknown);
    Ok(Region { name: "Structure".into(), pos: (0, 0, 0), size: (sx, sy, sz), blocks })
}

/// "minecraft:oak_stairs[facing=east,half=top,...]" - a state as Sponge and Litematica write it.
pub fn state_string(s: BlockState) -> String {
    let t = s.to_trait();
    let mut props: Vec<(&str, &str)> = t.property_map().into_iter().collect();
    props.sort();
    let id = format!("minecraft:{}", t.id());
    if props.is_empty() { id } else { format!("{id}[{}]", props.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")) }
}

/// Write blocks (positions from 0) as a gzipped Sponge v2 schematic - for tests and test builds.
pub fn write_sponge(path: &Path, blocks: &[((i32, i32, i32), BlockState)]) -> Result<()> {
    let (w, h, l) = blocks.iter().fold((0, 0, 0), |(w, h, l), ((x, y, z), _)| (w.max(x + 1), h.max(y + 1), l.max(z + 1)));
    let mut palette: Vec<String> = vec!["minecraft:air".into()];
    let mut grid = vec![0usize; (w * h * l) as usize];
    for ((x, y, z), s) in blocks {
        let name = state_string(*s);
        let i = match palette.iter().position(|p| *p == name) {
            Some(i) => i,
            None => {
                palette.push(name);
                palette.len() - 1
            }
        };
        grid[((y * l + z) * w + x) as usize] = i;
    }
    let mut data = Vec::new();
    for v in grid {
        let mut v = v as u32;
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                data.push(b);
                break;
            }
            data.push(b | 0x80);
        }
    }
    let mut pal = NbtCompound::new();
    for (i, n) in palette.iter().enumerate() {
        pal.insert(n.as_str(), NbtTag::Int(i as i32));
    }
    let mut root = NbtCompound::new();
    root.insert("Version", NbtTag::Int(2));
    root.insert("DataVersion", NbtTag::Int(4325));
    root.insert("Width", NbtTag::Short(w as i16));
    root.insert("Height", NbtTag::Short(h as i16));
    root.insert("Length", NbtTag::Short(l as i16));
    root.insert("PaletteMax", NbtTag::Int(palette.len() as i32));
    root.insert("Palette", NbtTag::Compound(pal));
    root.insert("BlockData", NbtTag::ByteArray(data));
    let mut raw = Vec::new();
    simdnbt::owned::BaseNbt::new("Schematic", root).write(&mut raw);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, &raw)?;
    fs::write(path, gz.finish()?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_schematic_reads_back() {
        let blocks = vec![((0, 0, 0), parse_state("stone").unwrap()), ((2, 1, 1), parse_state("oak_stairs[facing=east,half=top]").unwrap())];
        let f = std::env::temp_dir().join(format!("melonbot-test-{}.schem", std::process::id()));
        write_sponge(&f, &blocks).unwrap();
        let s = load(&f).unwrap();
        let _ = fs::remove_file(&f);
        assert_eq!(s.regions[0].size, (3, 2, 2));
        let mut got = s.regions[0].blocks.clone();
        got.sort_by_key(|(p, _)| *p);
        assert_eq!(got, blocks);
    }

    #[test]
    fn states_parse_with_their_properties() {
        let st = parse_state("minecraft:oak_stairs[facing=east,half=top,shape=straight,waterlogged=false]").unwrap();
        assert_eq!(st.to_trait().id(), "oak_stairs");
        assert_eq!(st.get_property("facing"), Some("east"));
        assert_eq!(st.get_property("half"), Some("top"));
        let log = state_of("minecraft:oak_log", [("axis", "z")]).unwrap();
        assert_eq!(log.get_property("axis"), Some("z"));
        // An unknown property is ignored, an unknown block is None.
        assert_eq!(parse_state("stone[foo=bar]"), Some(BlockState::from(BlockKind::Stone)));
        assert!(parse_state("modname:widget").is_none());
    }
}
