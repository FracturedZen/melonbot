//! Where a schematic goes, from the controller instance's Litematica: its saved placements
//! (config/litematica/litematica_<server>_dim_<dimension>.json - schematic file, origin, rotation,
//! mirror, and each sub-region's own placement), turned into the world blocks the swarm prints.
//!
//! Litematica writes that file only when the old world unloads (WorldLoadListener.onWorldLoadPre,
//! 0.28.3 bytecode): a relog, a dimension change or a server transfer - a placement made since is
//! not on disk yet.
//!
//! The schematic file itself: from the swarm's drop folder (swarm/schematics/, by file name) if it
//! is there, else the path Litematica saved, else the instance's schematics folder by file name.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use azalea::{BlockPos, block::BlockState};
use serde_json::Value;

use super::{
    schem::Schem,
    transform::{self, Mirror, Rotation},
};

#[derive(Clone, Debug, PartialEq)]
pub struct SubPlacement {
    pub name: String,
    pub pos: (i32, i32, i32),
    pub rotation: Rotation,
    pub mirror: Mirror,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    /// The schematic file as Litematica saved it.
    pub schematic: PathBuf,
    pub name: String,
    pub origin: (i32, i32, i32),
    pub rotation: Rotation,
    pub mirror: Mirror,
    pub enabled: bool,
    pub subs: Vec<SubPlacement>,
    /// "minecraft:overworld" etc., from the file name.
    pub dimension: String,
    /// The Litematica file it came from, and when that was written.
    pub file: PathBuf,
    pub saved: Option<SystemTime>,
}

fn triple(v: &Value) -> Option<(i32, i32, i32)> {
    let a = v.as_array()?;
    Some((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32, a.get(2)?.as_i64()? as i32))
}

/// The placements in one Litematica per-dimension file.
pub fn read_file(file: &Path) -> Result<Vec<Placement>> {
    let text = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    let saved = fs::metadata(file).and_then(|m| m.modified()).ok();
    let dimension = dimension_of(file);
    let mut out = Vec::new();
    for p in v["placements"]["placements"].as_array().into_iter().flatten() {
        let Some(origin) = triple(&p["origin"]) else { continue };
        let subs = p["placements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let sp = &s["placement"];
                Some(SubPlacement {
                    name: s["name"].as_str().or(sp["name"].as_str())?.to_string(),
                    pos: triple(&sp["pos"]).unwrap_or((0, 0, 0)),
                    rotation: Rotation::parse(sp["rotation"].as_str().unwrap_or("NONE")),
                    mirror: Mirror::parse(sp["mirror"].as_str().unwrap_or("NONE")),
                    enabled: sp["enabled"].as_bool().unwrap_or(true),
                })
            })
            .collect();
        out.push(Placement {
            schematic: PathBuf::from(p["schematic"].as_str().unwrap_or_default()),
            name: p["name"].as_str().unwrap_or_default().to_string(),
            origin,
            rotation: Rotation::parse(p["rotation"].as_str().unwrap_or("NONE")),
            mirror: Mirror::parse(p["mirror"].as_str().unwrap_or("NONE")),
            enabled: p["enabled"].as_bool().unwrap_or(true),
            subs,
            dimension: dimension.clone(),
            file: file.to_path_buf(),
            saved,
        });
    }
    Ok(out)
}

/// "litematica_example.com_dim_minecraft_the_nether.json" -> "minecraft:the_nether".
fn dimension_of(file: &Path) -> String {
    let stem = file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    match stem.rsplit_once("_dim_") {
        Some((_, d)) => match d.split_once('_') {
            Some((ns, path)) => format!("{ns}:{path}"),
            None => d.to_string(),
        },
        None => String::new(),
    }
}

/// Every placement the instance's Litematica saved for `server` (its host, as in the file names),
/// in any dimension.
pub fn instance_placements(instance_minecraft: &Path, server: &str) -> Vec<Placement> {
    let host = server.split(':').next().unwrap_or(server).to_lowercase();
    let dir = instance_minecraft.join("config").join("litematica");
    let Ok(rd) = fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if !name.starts_with("litematica_") || !name.contains("_dim_") || !name.ends_with(".json") {
            continue;
        }
        if !host.is_empty() && !name.contains(&host) {
            continue;
        }
        if let Ok(ps) = read_file(&e.path()) {
            out.extend(ps);
        }
    }
    out
}

/// The schematic file for a placement: the drop folder first (by file name), then where Litematica
/// saved it, then anywhere under the instance's schematics folder.
pub fn find_schematic(p: &Placement, drop_dir: &Path, instance_minecraft: Option<&Path>) -> Option<PathBuf> {
    let file_name = p.schematic.file_name()?.to_os_string();
    let dropped = drop_dir.join(&file_name);
    if dropped.is_file() {
        return Some(dropped);
    }
    if p.schematic.is_file() {
        return Some(p.schematic.clone());
    }
    let root = instance_minecraft?.join("schematics");
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name() == Some(file_name.as_os_str()) {
                return Some(path);
            }
        }
    }
    None
}

/// What the placement puts in the world: every block of every enabled sub-region, where
/// Litematica puts it (SchematicPlacingUtils.placeBlocksWithinChunk, 0.28.3 bytecode).
pub struct Target {
    pub name: String,
    pub blocks: HashMap<BlockPos, BlockState>,
    pub min: BlockPos,
    pub max: BlockPos,
    /// Sub-regions switched off in Litematica (left out).
    pub disabled: Vec<String>,
    pub unknown: BTreeMap<String, usize>,
}

pub fn target(schem: &Schem, p: &Placement) -> Target {
    let mut blocks = HashMap::with_capacity(schem.block_count());
    let mut disabled = Vec::new();
    let (ox, oy, oz) = p.origin;
    for r in &schem.regions {
        let sub = p.subs.iter().find(|s| s.name == r.name).cloned().unwrap_or(SubPlacement {
            name: r.name.clone(),
            pos: r.pos,
            rotation: Rotation::None,
            mirror: Mirror::None,
            enabled: true,
        });
        if !sub.enabled {
            disabled.push(r.name.clone());
            continue;
        }
        // The region's corner, turned with the whole placement, then each block turned by the
        // placement and then by its own sub-region.
        let (rx, ry, rz) = transform::pos(sub.pos, p.mirror, p.rotation);
        let (mx, my, mz) = r.min_offset();
        let sub_mirror = if sub.mirror != Mirror::None && p.rotation.is_quarter() { sub.mirror.swapped() } else { sub.mirror };
        let rot = p.rotation.then(sub.rotation);
        let mut turned: HashMap<BlockState, BlockState> = HashMap::new();
        for ((x, y, z), st) in &r.blocks {
            let rel = transform::pos((x + mx, y + my, z + mz), p.mirror, p.rotation);
            let (tx, ty, tz) = transform::pos(rel, sub.mirror, sub.rotation);
            let at = BlockPos::new(ox + rx + tx, oy + ry + ty, oz + rz + tz);
            let state = *turned.entry(*st).or_insert_with(|| {
                let s = transform::mirror_state(*st, p.mirror);
                let s = transform::mirror_state(s, sub_mirror);
                transform::rotate_state(s, rot)
            });
            blocks.insert(at, state);
        }
    }
    let mut min = BlockPos::new(i32::MAX, i32::MAX, i32::MAX);
    let mut max = BlockPos::new(i32::MIN, i32::MIN, i32::MIN);
    for b in blocks.keys() {
        min = BlockPos::new(min.x.min(b.x), min.y.min(b.y), min.z.min(b.z));
        max = BlockPos::new(max.x.max(b.x), max.y.max(b.y), max.z.max(b.z));
    }
    Target { name: p.name.clone(), blocks, min, max, disabled, unknown: schem.unknown.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::schem::{Region, parse_state};

    fn placement(rotation: Rotation, mirror: Mirror) -> Placement {
        Placement {
            schematic: PathBuf::new(),
            name: "t".into(),
            origin: (100, 64, 200),
            rotation,
            mirror,
            enabled: true,
            subs: vec![],
            dimension: "minecraft:overworld".into(),
            file: PathBuf::new(),
            saved: None,
        }
    }

    fn schem() -> Schem {
        // Two blocks along x: a stair facing north at the corner, a log on the x axis next to it.
        Schem {
            name: "t".into(),
            regions: vec![Region {
                name: "r".into(),
                pos: (0, 0, 0),
                size: (2, 1, 1),
                blocks: vec![((0, 0, 0), parse_state("oak_stairs[facing=north]").unwrap()), ((1, 0, 0), parse_state("oak_log[axis=x]").unwrap())],
            }],
            unknown: BTreeMap::new(),
        }
    }

    #[test]
    fn a_turned_placement_puts_blocks_where_litematica_does() {
        let t = target(&schem(), &placement(Rotation::None, Mirror::None));
        assert_eq!(t.blocks[&BlockPos::new(101, 64, 200)].get_property("axis"), Some("x"));
        // A quarter turn clockwise: +x becomes +z, the stair faces east, the log runs along z.
        let t = target(&schem(), &placement(Rotation::Cw90, Mirror::None));
        assert_eq!(t.blocks[&BlockPos::new(100, 64, 200)].get_property("facing"), Some("east"));
        assert_eq!(t.blocks[&BlockPos::new(100, 64, 201)].get_property("axis"), Some("z"));
        // Mirrored front to back: +x becomes -x.
        let t = target(&schem(), &placement(Rotation::None, Mirror::FrontBack));
        assert!(t.blocks.contains_key(&BlockPos::new(99, 64, 200)));
        assert_eq!((t.min, t.max), (BlockPos::new(99, 64, 200), BlockPos::new(100, 64, 200)));
    }

    #[test]
    fn a_negative_size_grows_back_from_the_region_position() {
        let mut s = schem();
        s.regions[0].size = (-2, 1, 1);
        s.regions[0].pos = (5, 0, 0);
        let t = target(&s, &placement(Rotation::None, Mirror::None));
        // Container x 0 is at pos.x + size.x + 1 = 4.
        assert!(t.blocks.contains_key(&BlockPos::new(104, 64, 200)) && t.blocks.contains_key(&BlockPos::new(105, 64, 200)));
    }

    #[test]
    fn reads_the_users_saved_placement_shape() {
        let mc = std::env::temp_dir().join(format!("lit-test-{}", std::process::id()));
        let dir = mc.join("config").join("litematica");
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("litematica_example.com_dim_minecraft_overworld.json");
        fs::write(&f, r#"{"placements":{"placements":[{"schematic":"C:\\x\\EndCrystalTower.schem","name":"Tower","origin":[1200,50,-3400],
            "rotation":"CLOCKWISE_90","mirror":"NONE","enabled":true,
            "placements":[{"name":"Tower","placement":{"pos":[0,0,0],"name":"Tower","rotation":"NONE","mirror":"LEFT_RIGHT","enabled":false}}]}]}}"#).unwrap();
        assert!(instance_placements(&mc, "6b6t.org").is_empty(), "another server's placements are not ours");
        let ps = instance_placements(&mc, "example.com:25565");
        assert_eq!(ps.len(), 1);
        let p = &ps[0];
        assert_eq!((p.origin, p.rotation, p.dimension.as_str()), ((1200, 50, -3400), Rotation::Cw90, "minecraft:overworld"));
        assert_eq!(p.subs[0], SubPlacement { name: "Tower".into(), pos: (0, 0, 0), rotation: Rotation::None, mirror: Mirror::LeftRight, enabled: false });
        let _ = fs::remove_dir_all(&mc);
    }
}
