//! Which item places a block - the standalone Printer's itemFor and ITEM_OVERRIDES - and, for the
//! items that place one of two blocks (a torch on the floor or a wall), both of them.

use std::str::FromStr;

use azalea::{
    block::BlockState,
    registry::builtin::{BlockKind, ItemKind},
};

/// An item id without the namespace, if the game has that item.
fn item(id: &str) -> Option<String> {
    ItemKind::from_str(id).ok().map(|_| id.to_string())
}

/// The item that places `state`, or None when nothing should be placed there (a door's or a
/// plant's upper half and a bed's head come with the other half; fire, portals and the like are
/// not placed at all).
pub fn item_for(state: BlockState) -> Option<String> {
    if state.is_air() {
        return None;
    }
    let t = state.to_trait();
    let id = t.id();
    if t.get_property("half") == Some("upper") || (t.get_property("part") == Some("head") && id.ends_with("_bed")) {
        return None;
    }
    if let Some(over) = override_for(id) {
        return over.map(str::to_string);
    }
    if let Some(rest) = id.strip_prefix("potted_") {
        // The pot first; the plant is another click (not done yet).
        let _ = rest;
        return item("flower_pot");
    }
    // Wall forms: the standing form's item (the game registers both blocks to that one item).
    for (wall, standing) in [
        ("_wall_torch", "_torch"),
        ("_wall_hanging_sign", "_hanging_sign"),
        ("_wall_sign", "_sign"),
        ("_wall_banner", "_banner"),
        ("_wall_head", "_head"),
        ("_wall_skull", "_skull"),
        ("_wall_fan", "_fan"),
    ] {
        if id.ends_with(wall) {
            return item(&id.replace(wall, standing));
        }
    }
    if let Some(rest) = id.strip_prefix("wall_") {
        return item(rest);
    }
    if let Some(rest) = id.strip_prefix("attached_") {
        return item(rest);
    }
    item(id)
}

/// ITEM_OVERRIDES: Some(item) for a block placed by a differently named item, Some(None) for
/// nothing to place.
fn override_for(id: &str) -> Option<Option<&'static str>> {
    Some(match id {
        "redstone_wire" => Some("redstone"),
        "wheat" => Some("wheat_seeds"),
        "carrots" => Some("carrot"),
        "potatoes" => Some("potato"),
        "beetroots" => Some("beetroot_seeds"),
        "melon_stem" | "attached_melon_stem" => Some("melon_seeds"),
        "pumpkin_stem" | "attached_pumpkin_stem" => Some("pumpkin_seeds"),
        "cocoa" => Some("cocoa_beans"),
        "sweet_berry_bush" => Some("sweet_berries"),
        "torchflower_crop" => Some("torchflower_seeds"),
        "pitcher_crop" => Some("pitcher_pod"),
        "nether_wart" => Some("nether_wart"),
        "tripwire" => Some("string"),
        "cave_vines" | "cave_vines_plant" => Some("glow_berries"),
        // Placed as dirt; a hoe or shovel finishes them (tool finish).
        "farmland" | "dirt_path" => Some("dirt"),
        "water" => Some("water_bucket"),
        "lava" => Some("lava_bucket"),
        "water_cauldron" | "lava_cauldron" | "powder_snow_cauldron" => Some("cauldron"),
        "bamboo_sapling" => Some("bamboo"),
        "piston_head" | "moving_piston" | "fire" | "soul_fire" | "nether_portal" | "end_portal" | "end_gateway"
        | "bubble_column" | "void_air" | "cave_air" | "air" => None,
        _ if id == "candle_cake" || id.ends_with("_candle_cake") => Some("cake"),
        _ => return None,
    })
}

/// The block an item really puts down when it differs from what the schematic asked for (the
/// standalone's PLACED_FORM): farmland and paths go down as dirt, filled cauldrons as cauldrons,
/// candle cakes as cake. Such a block counts as done.
pub fn placed_form(required: BlockKind) -> Option<BlockKind> {
    let id = required.to_str().strip_prefix("minecraft:").unwrap_or(required.to_str());
    match id {
        "farmland" | "dirt_path" => Some(BlockKind::Dirt),
        "water_cauldron" | "lava_cauldron" | "powder_snow_cauldron" => Some(BlockKind::Cauldron),
        "attached_melon_stem" => Some(BlockKind::MelonStem),
        "attached_pumpkin_stem" => Some(BlockKind::PumpkinStem),
        _ if id == "candle_cake" || id.ends_with("_candle_cake") => Some(BlockKind::Cake),
        _ => None,
    }
}

/// An item that places one of two blocks by where it is put (StandingAndWallBlockItem): the
/// standing block, the wall block, and the side the standing one hangs from (Down for torches,
/// signs, banners, heads and coral fans; Up for hanging signs).
pub struct DualItem {
    pub standing: BlockKind,
    pub wall: BlockKind,
    pub attach_up: bool,
}

pub fn dual(item_id: &str) -> Option<DualItem> {
    let b = |s: &str| BlockKind::from_str(s).ok();
    let (standing, wall, up) = if item_id.ends_with("_hanging_sign") {
        (item_id.to_string(), item_id.replace("_hanging_sign", "_wall_hanging_sign"), true)
    } else if item_id == "torch" || item_id.ends_with("_torch") {
        (item_id.to_string(), item_id.replace("torch", "wall_torch"), false)
    } else if item_id.ends_with("_sign") {
        (item_id.to_string(), item_id.replace("_sign", "_wall_sign"), false)
    } else if item_id.ends_with("_banner") {
        (item_id.to_string(), item_id.replace("_banner", "_wall_banner"), false)
    } else if item_id.ends_with("_head") {
        (item_id.to_string(), item_id.replace("_head", "_wall_head"), false)
    } else if item_id.ends_with("_skull") {
        (item_id.to_string(), item_id.replace("_skull", "_wall_skull"), false)
    } else if item_id.ends_with("_coral_fan") {
        (item_id.to_string(), item_id.replace("_fan", "_wall_fan"), false)
    } else {
        return None;
    };
    Some(DualItem { standing: b(&standing)?, wall: b(&wall)?, attach_up: up })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::schem::parse_state;

    fn it(s: &str) -> Option<String> {
        item_for(parse_state(s).unwrap())
    }

    #[test]
    fn items_like_the_standalone() {
        assert_eq!(it("oak_stairs[facing=east]").as_deref(), Some("oak_stairs"));
        assert_eq!(it("wall_torch[facing=east]").as_deref(), Some("torch"));
        assert_eq!(it("soul_wall_torch").as_deref(), Some("soul_torch"));
        assert_eq!(it("redstone_wall_torch").as_deref(), Some("redstone_torch"));
        assert_eq!(it("oak_wall_sign").as_deref(), Some("oak_sign"));
        assert_eq!(it("oak_wall_hanging_sign").as_deref(), Some("oak_hanging_sign"));
        assert_eq!(it("red_wall_banner").as_deref(), Some("red_banner"));
        assert_eq!(it("creeper_wall_head").as_deref(), Some("creeper_head"));
        assert_eq!(it("skeleton_wall_skull").as_deref(), Some("skeleton_skull"));
        assert_eq!(it("brain_coral_wall_fan").as_deref(), Some("brain_coral_fan"));
        assert_eq!(it("redstone_wire").as_deref(), Some("redstone"));
        assert_eq!(it("farmland").as_deref(), Some("dirt"));
        assert_eq!(it("potted_poppy").as_deref(), Some("flower_pot"));
        assert_eq!(it("oak_door[half=upper]"), None);
        assert_eq!(it("oak_door[half=lower]").as_deref(), Some("oak_door"));
        assert_eq!(it("red_bed[part=head]"), None);
        assert_eq!(it("red_bed[part=foot]").as_deref(), Some("red_bed"));
        assert_eq!(it("fire"), None);
        assert_eq!(it("white_candle_cake").as_deref(), Some("cake"));
        let d = dual("torch").unwrap();
        assert_eq!((d.standing, d.wall, d.attach_up), (BlockKind::Torch, BlockKind::WallTorch, false));
        let d = dual("oak_hanging_sign").unwrap();
        assert_eq!((d.standing, d.wall, d.attach_up), (BlockKind::OakHangingSign, BlockKind::OakWallHangingSign, true));
        assert!(dual("oak_planks").is_none());
        assert_eq!(dual("redstone_torch").unwrap().wall, BlockKind::RedstoneWallTorch);
        assert_eq!(dual("dead_brain_coral_fan").unwrap().wall, BlockKind::DeadBrainCoralWallFan);
    }
}
