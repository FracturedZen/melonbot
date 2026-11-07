//! The bags: what is carried, where, and getting a stack into the hand. Port of the useful core of
//! the standalone's Inv. Slot numbers are the player menu's: 9-35 main, 36-44 hotbar, 45 offhand.

use azalea::{
    Client,
    entity::inventory::Inventory,
    inventory::{
        ItemStack, Menu,
        components::{Damage, Enchantments, MaxDamage, StoredEnchantments},
        operations::{ClickType, ThrowClick},
    },
    protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick},
    registry::builtin::ItemKind,
};
use tracing::debug;

/// The offhand, as a swap button (and slot 45 of the player menu).
pub const OFFHAND_BUTTON: u8 = 40;

/// Swap a bag slot with a hotbar slot (button 0-8) or the offhand (button 40), the number-key / F
/// swap - predicted RIGHT. azalea's own SwapClick prediction reads the button as a plain menu slot,
/// so "into hotbar 2" moved the stack into the crafting grid on the client and "into the offhand"
/// into hotbar slot 4; the client then never saw the swap land and swapped again and again (15 Oct:
/// the printer swapped melons in and out of the offhand every 0.2 s). Here the client's inventory is
/// changed the way the server will change it, and the packet carries that, so the two agree.
///
/// With a container open (the swarm's ender chest link stays open for minutes) the server only
/// takes clicks on THAT container, so the bag slot is clicked at its place in the container
/// (its player part: main, then hotbar), with the container's id.
pub fn swap_with(bot: &Client, slot: usize, button: u8) -> bool {
    if !(9..=44).contains(&slot) || !(button <= 8 || button == OFFHAND_BUTTON) {
        return false;
    }
    let open = bot.query_self::<&Inventory, _>(|inv| inv.container_menu.as_ref().map(|c| *c.player_slots_range().start())).ok().flatten();
    match open {
        // Player menu: 9-35 main, 36-44 hotbar, 45 offhand.
        None => swap_in(bot, slot, button, Some(if button == OFFHAND_BUTTON { 45 } else { 36 + button as usize })),
        // A container: its player part starts at `start` (main), hotbar 27 later; the offhand is
        // not in it (the swap still reaches it, the client's view of it is not updated).
        Some(start) => {
            let at = start + (slot - 9);
            swap_in(bot, at, button, (button <= 8).then(|| start + 27 + button as usize))
        }
    }
}

/// A slot of the OPEN menu (the container's own slots included) swapped with hotbar `button`
/// (0-8) or the offhand (40): one SWAP click, the client's menu changed the way the server will
/// change it. `target` is the button's slot in that menu, if it has one.
pub fn swap_in(bot: &Client, slot: usize, button: u8, target: Option<usize>) -> bool {
    if target == Some(slot) {
        return true;
    }
    let Ok(world) = bot.world() else { return false };
    let snapshot = bot.query_self::<&Inventory, _>(|inv| {
        let slots = inv.menu().slots();
        (inv.id, inv.state_id, slots.get(slot).cloned(), target.and_then(|t| slots.get(t).cloned()), inv.carried.clone(), inv.inventory_menu.slots().get(45).cloned())
    });
    let Ok((id, state_id, Some(a), b, carried, offhand)) = snapshot else { return false };
    // What lands in the clicked slot: the button's stack (the offhand's, when it is the button).
    let incoming = match (&b, button) {
        (Some(b), _) => b.clone(),
        (None, OFFHAND_BUTTON) => offhand.unwrap_or_default(),
        (None, _) => return false,
    };
    let (changed, carried_hash) = {
        let w = world.read();
        let mut changed = indexmap::IndexMap::new();
        changed.insert(slot as u16, HashedStack::from_item_stack(&incoming, &w.registries));
        if let Some(t) = target {
            changed.insert(t as u16, HashedStack::from_item_stack(&a, &w.registries));
        }
        (changed, HashedStack::from_item_stack(&carried, &w.registries))
    };
    let _ = bot.query_self::<&mut Inventory, _>(|mut inv| {
        let m = inv.menu_mut();
        if let Some(s) = m.slot_mut(slot) {
            *s = incoming.clone();
        }
        if let Some(t) = target
            && let Some(s) = m.slot_mut(t)
        {
            *s = a.clone();
        }
        if target.is_none()
            && button == OFFHAND_BUTTON
            && let Some(s) = inv.inventory_menu.slot_mut(45)
        {
            *s = a.clone();
        }
    });
    bot.write_packet(ServerboundContainerClick {
        container_id: id,
        state_id,
        slot_num: slot as i16,
        button_num: button,
        click_type: ClickType::Swap,
        changed_slots: changed,
        carried_item: carried_hash,
    });
    debug!(target: "inventory", "swap {}slot {slot} ({}) <-> {} ({})", if id != 0 { format!("container {id} ") } else { String::new() }, stack_id(&a), if button == OFFHAND_BUTTON { "offhand".to_string() } else { format!("hotbar {button}") }, stack_id(&incoming));
    true
}

pub const MAIN: std::ops::RangeInclusive<usize> = 9..=35;
pub const HOTBAR: std::ops::RangeInclusive<usize> = 36..=44;
/// Main inventory and hotbar together - the 36 bag slots.
pub const BAGS: std::ops::RangeInclusive<usize> = 9..=44;

/// "melon" for minecraft:melon.
pub fn id(kind: ItemKind) -> &'static str {
    let s = kind.to_str();
    s.strip_prefix("minecraft:").unwrap_or(s)
}

pub fn stack_id(st: &ItemStack) -> &'static str {
    if st.is_empty() { "air" } else { id(st.kind()) }
}

/// The player's own menu (9-35 main, 36-44 hotbar, 45 offhand) - even with a container open,
/// when azalea's menu() is the container and its copy of the player menu goes stale until the
/// container closes: the bags are then read from the container's player part.
pub fn menu(bot: &Client) -> Option<Menu> {
    bot.query_self::<&Inventory, _>(|inv| {
        let mut m = inv.inventory_menu.clone();
        if let Some(c) = &inv.container_menu {
            let player = c.slots()[c.player_slots_range()].to_vec();
            if let Ok(arr) = <[ItemStack; 36]>::try_from(player) {
                *m.as_player_mut().inventory = arr;
            }
        }
        m
    })
    .ok()
}

/// (menu slot, stack) for the 36 bag slots.
pub fn bags(bot: &Client) -> Vec<(usize, ItemStack)> {
    let Some(m) = menu(bot) else { return Vec::new() };
    let slots = m.slots();
    BAGS.filter_map(|i| slots.get(i).map(|s| (i, s.clone()))).collect()
}

pub fn count(bot: &Client, mut want: impl FnMut(&ItemStack) -> bool) -> i32 {
    bags(bot).iter().filter(|(_, s)| !s.is_empty() && want(s)).map(|(_, s)| s.count()).sum()
}

pub fn count_id(bot: &Client, item: &str) -> i32 {
    count(bot, |s| stack_id(s) == item)
}

pub fn empty_slots(bot: &Client) -> usize {
    bags(bot).iter().filter(|(_, s)| s.is_empty()).count()
}

/// Throw a bag stack out (ctrl+Q), the way the bot faces. Not while a container is open.
pub fn throw(bot: &Client, slot: usize) -> bool {
    if bot.query_self::<&Inventory, _>(|i| i.id).ok() != Some(0) {
        return false;
    }
    match bot.get_inventory() {
        Ok(menu) => {
            menu.click(ThrowClick::All { slot: slot as u16 });
            true
        }
        Err(_) => false,
    }
}

/// The stack in the hand right now (the selected hotbar slot), if it can be read.
pub fn held(bot: &Client) -> Option<ItemStack> {
    let h = bot.selected_hotbar_slot().ok()? as usize;
    menu(bot)?.slots().get(36 + h).cloned()
}

/// The id of whatever is in the hand ("air" when empty or unreadable). "stick" is the one the
/// seller must never hold when it opens a villager on this server.
pub fn held_id(bot: &Client) -> &'static str {
    held(bot).map(|s| stack_id(&s)).unwrap_or("air")
}

/// The hotbar index (0-8) of a menu slot, if it is in the hotbar.
pub fn hotbar_index(slot: usize) -> Option<u8> {
    HOTBAR.contains(&slot).then(|| (slot - 36) as u8)
}

pub fn uses_left(st: &ItemStack) -> Option<i32> {
    let max = st.get_component::<MaxDamage>()?.amount;
    let dmg = st.get_component::<Damage>().map_or(0, |d| d.amount);
    Some(max - dmg)
}

/// An enchantment's level on a stack ("silk_touch", "efficiency"), read through the server's
/// enchantment registry (the ids are data-driven).
pub fn enchant_level(bot: &Client, st: &ItemStack, name: &str) -> i32 {
    let Some(e) = st.get_component::<Enchantments>() else { return 0 };
    for (ench, lvl) in &e.levels {
        let key = bot.resolve_registry_key(ench).ok().flatten();
        if key.is_some_and(|k| azalea::Identifier::from(k).path() == name) {
            return *lvl;
        }
    }
    0
}

/// The enchantments an enchanted book holds - by registry name ("mending", "silk_touch"), sorted.
/// A book keeps them as STORED enchantments, not the ones on a tool, so `enchant_level` reads
/// nothing on it. A name the registry cannot resolve shows as "?".
pub fn stored_enchants(bot: &Client, st: &ItemStack) -> Vec<(String, i32)> {
    let Some(e) = st.get_component::<StoredEnchantments>() else { return Vec::new() };
    let mut out: Vec<(String, i32)> = e
        .enchantments
        .iter()
        .map(|(ench, lvl)| {
            let key = bot.resolve_registry_key(ench).ok().flatten();
            (key.map_or_else(|| "?".to_string(), |k| azalea::Identifier::from(k).path().to_string()), *lvl)
        })
        .collect();
    out.sort();
    out
}

/// A stored enchantment's level on an enchanted book (0 if the book does not hold it).
pub fn stored_enchant_level(bot: &Client, st: &ItemStack, name: &str) -> i32 {
    stored_enchants(bot, st).into_iter().find(|(n, _)| n == name).map_or(0, |(_, l)| l)
}

/// Is this hotbar stack something we must not swap out from under (a tool or weapon)?
/// No container is open: the bags are clicked in the player's own menu.
pub fn no_container_open(bot: &Client) -> bool {
    bot.query_self::<&Inventory, _>(|i| i.id).ok() == Some(0)
}

/// Anything that wears out (tools, weapons, armour).
pub fn is_tool(st: &ItemStack) -> bool {
    !st.is_empty() && st.get_component::<MaxDamage>().is_some()
}

/// Put the stack in `slot` in the hand: select it if it is in the hotbar, else swap it into a
/// hotbar slot first - an empty one, else one holding no tool, else the held one - so a meal or a
/// tool change never strands the Silk Touch axe in the backpack (14 Oct: auto-eat's swap into the
/// held slot left the axe out of the hotbar, and the farmer stopped for hours). Returns the hotbar
/// index now held.
pub fn hold(bot: &Client, slot: usize, avoid: &[u8]) -> Option<u8> {
    if let Some(h) = hotbar_index(slot) {
        bot.set_selected_hotbar_slot(h);
        return Some(h);
    }
    let m = menu(bot)?;
    let slots = m.slots();
    let held = bot.selected_hotbar_slot().ok()?;
    let pick = (0..9u8)
        .filter(|h| !avoid.contains(h))
        .min_by_key(|h| {
            let st = &slots[36 + *h as usize];
            if st.is_empty() { 0 } else if !is_tool(st) { 1 } else if *h == held { 2 } else { 3 }
        })?;
    if !swap_with(bot, slot, pick) {
        return None;
    }
    bot.set_selected_hotbar_slot(pick);
    Some(pick)
}
