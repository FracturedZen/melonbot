//! Villager trading: open a villager, read its offers off the wire, and sell the seller's goods
//! (`Good`: sticks, iron ingots, ...) for emeralds.
//!
//! azalea's own `merchant_offers` packet handler is a no-op (it stores nothing), so - exactly as
//! `chests.rs` does for sign text - the offers are caught here from `ClientboundMerchantOffers` and
//! kept on the local player. Once the villager is open its trade menu is an ordinary container
//! (`Menu::Merchant`: slot 0/1 = the two cost slots, slot 2 = the result), so the sale runs through
//! a `ContainerHandle` like any chest. Every sale is judged by counting emeralds before and after -
//! never assumed from a click.
//!
//! Nothing is clicked on trust: the offers must be the open villager's own (`fresh_offers`), and the
//! result slot must hold exactly the item expected before the click (`result_is`) - the result slot
//! is what the server hands over, whatever the list said. Buying uses the same path, for the
//! buyer's buy list (`Want`): blocks for emeralds, and enchanted books for emeralds and a book.

use std::{str::FromStr, time::Duration};

use azalea::{
    Client, Vec3,
    app::{App, Plugin, Update},
    container::{ContainerHandle, WaitingForInventoryOpen},
    ecs::entity::Entity,
    ecs::prelude::*,
    entity::LocalEntity,
    interact::EntityInteractEvent,
    packet::game::ReceiveGamePacketEvent,
    protocol::packets::game::{ClientboundGamePacket, c_merchant_offers::MerchantOffer, s_select_trade::ServerboundSelectTrade},
    registry::builtin::{ItemKind, VillagerProfession},
};
use azalea::ecs as bevy_ecs; // for #[derive(Component)]
use azalea::inventory::{ItemStack, components::Food};
use tracing::{debug, info, warn};

use crate::{inv, movement};

/// The merchant menu's result slot (slots 0 and 1 are the two cost slots).
const RESULT_SLOT: usize = 2;
/// How long to wait for a villager's trade menu to open, in client ticks.
const OPEN_TIMEOUT_TICKS: usize = 60;

pub struct TradePlugin;

impl Plugin for TradePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, record_offers);
    }
}

/// The last merchant offers this client was sent, and the container they belong to.
#[derive(Component, Default)]
pub struct Offers {
    pub container_id: i32,
    pub list: Vec<MerchantOffer>,
}

/// Catch `ClientboundMerchantOffers` off the packet stream (azalea drops it otherwise) and keep it
/// on the local player.
fn record_offers(mut events: MessageReader<ReceiveGamePacketEvent>, mut us: Query<Option<&mut Offers>, With<LocalEntity>>, mut commands: Commands) {
    for ev in events.read() {
        let ClientboundGamePacket::MerchantOffers(p) = ev.packet.as_ref() else { continue };
        let fresh = Offers { container_id: p.container_id, list: p.offers.clone() };
        let Ok(slot) = us.get_mut(ev.entity) else { continue };
        match slot {
            Some(mut o) => *o = fresh,
            None => {
                commands.entity(ev.entity).insert(fresh);
            }
        }
    }
}

/// The offers this client holds right now (container id, the list), if any.
pub fn offers(bot: &Client) -> Option<(i32, Vec<MerchantOffer>)> {
    bot.query_self::<Option<&Offers>, _>(|o| o.map(|o| (o.container_id, o.list.clone()))).ok().flatten()
}

/// The index of a usable "sell `item` for emeralds" offer: `item` as the only cost, emeralds out,
/// in stock and not locked.
pub fn sell_offer(list: &[MerchantOffer], item: ItemKind) -> Option<usize> {
    list.iter().position(|o| {
        o.base_cost_a.item == item && o.cost_b.is_none() && inv::stack_id(&o.result) == "emerald" && !o.out_of_stock && o.uses < o.max_uses
    })
}

/// The jobs that trade with the seller at all (a nitwit or a jobless villager never does).
const TRADING_JOBS: [VillagerProfession; 13] = [
    VillagerProfession::Armorer,
    VillagerProfession::Butcher,
    VillagerProfession::Cartographer,
    VillagerProfession::Cleric,
    VillagerProfession::Farmer,
    VillagerProfession::Fisherman,
    VillagerProfession::Fletcher,
    VillagerProfession::Leatherworker,
    VillagerProfession::Librarian,
    VillagerProfession::Mason,
    VillagerProfession::Shepherd,
    VillagerProfession::Toolsmith,
    VillagerProfession::Weaponsmith,
];

/// Does a villager with this job buy `item` for emeralds at some level (vanilla's trades)? An
/// armorer's iron is its second level's trade: a novice one opens with none.
fn job_buys(job: VillagerProfession, item: &str) -> bool {
    use VillagerProfession as J;
    match job {
        J::Armorer => matches!(item, "coal" | "iron_ingot" | "lava_bucket" | "diamond"),
        J::Butcher => matches!(item, "chicken" | "porkchop" | "rabbit" | "coal" | "mutton" | "beef" | "dried_kelp_block" | "sweet_berries"),
        J::Cartographer => matches!(item, "paper" | "glass_pane" | "compass"),
        J::Cleric => matches!(item, "rotten_flesh" | "gold_ingot" | "rabbit_foot" | "turtle_scute" | "glass_bottle" | "nether_wart"),
        J::Farmer => matches!(item, "wheat" | "potato" | "carrot" | "beetroot" | "pumpkin" | "melon"),
        J::Fisherman => matches!(item, "string" | "coal" | "cod" | "salmon" | "tropical_fish" | "pufferfish") || item.ends_with("_boat"),
        J::Fletcher => matches!(item, "stick" | "flint" | "string" | "feather" | "tripwire_hook"),
        J::Leatherworker => matches!(item, "leather" | "flint" | "rabbit_hide" | "turtle_scute"),
        J::Librarian => matches!(item, "paper" | "book" | "ink_sac" | "writable_book"),
        J::Mason => matches!(item, "clay_ball" | "stone" | "granite" | "andesite" | "diorite" | "quartz"),
        J::Shepherd => matches!(item, "white_wool" | "brown_wool" | "black_wool" | "gray_wool") || item.ends_with("_dye"),
        J::Toolsmith | J::Weaponsmith => matches!(item, "coal" | "iron_ingot" | "flint" | "diamond"),
        _ => false,
    }
}

/// One entry of the seller's sell list: the item it sells for emeralds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Good {
    pub item: ItemKind,
}

impl Good {
    /// Read a sell-list entry: an item id ("iron_ingot", "minecraft:stick", "rotten flesh"). "iron"
    /// and "gold" are their ingots - what their chests are signed with. Anything the game does not
    /// know is refused.
    pub fn parse(entry: &str) -> Result<Good, String> {
        let s = entry.trim().to_lowercase().replace(' ', "_");
        let s = s.strip_prefix("minecraft:").unwrap_or(&s);
        let id = match s {
            "" => return Err("an empty entry".into()),
            "iron" | "gold" => format!("{s}_ingot"),
            _ => s.to_string(),
        };
        match ItemKind::from_str(&id) {
            Ok(ItemKind::Air) | Err(_) => Err(format!("\"{}\" is not an item (an id such as iron_ingot or wheat)", entry.trim())),
            Ok(ItemKind::Emerald) => Err("emeralds are what it is paid in".into()),
            Ok(item) => Ok(Good { item }),
        }
    }

    pub fn id(&self) -> &'static str {
        inv::id(self.item)
    }

    /// The word its chests are signed with: its name, an ingot's metal ("iron"). Spaces or
    /// underscores on the sign both match.
    pub fn sign_word(&self) -> &'static str {
        self.id().strip_suffix("_ingot").unwrap_or(self.id())
    }

    /// The jobs known to buy it - empty for an item no vanilla villager buys.
    pub fn jobs(&self) -> Vec<VillagerProfession> {
        TRADING_JOBS.into_iter().filter(|j| job_buys(*j, self.id())).collect()
    }

    /// Worth opening a villager with this job (None: its job is not seen yet) to sell it: a job that
    /// buys it - or any job at all for an item no job is known to buy.
    pub fn bought_by(&self, job: Option<VillagerProfession>) -> bool {
        match job {
            None => true,
            Some(j) => TRADING_JOBS.contains(&j) && (job_buys(j, self.id()) || self.jobs().is_empty()),
        }
    }
}

impl std::fmt::Display for Good {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.id().replace('_', " "))
    }
}

/// One entry of the buyer's buy list. `item` is the item id; an enchanted book also names the
/// enchantment it must hold (registry name, "silk_touch") and the lowest level that counts.
#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    pub item: String,
    pub enchant: Option<(String, i32)>,
}

impl Want {
    /// Read a buy-list entry: an item id ("glowstone"), or `enchanted_book:<enchantment>` with an
    /// optional lowest level (`enchanted_book:sharpness:5`, or `:v`). A bare "enchanted_book" is
    /// refused - every enchanted book has that id, so it would buy any book at all.
    pub fn parse(entry: &str) -> Result<Want, String> {
        let s = entry.trim().to_lowercase();
        let s = s.strip_prefix("minecraft:").unwrap_or(&s);
        let mut parts = s.split(':').map(|p| p.trim().replace(' ', "_"));
        let item = parts.next().unwrap_or_default();
        let name = parts.next();
        let level = parts.next();
        if item.is_empty() || parts.next().is_some() {
            return Err(format!("cannot read \"{entry}\""));
        }
        if item != "enchanted_book" {
            return match name {
                None => Ok(Want { item, enchant: None }),
                Some(_) => Err(format!("\"{entry}\": only an enchanted_book takes an enchantment")),
            };
        }
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            return Err(format!("\"{entry}\" needs an enchantment, e.g. enchanted_book:mending"));
        };
        let min = match level.as_deref() {
            None => 1,
            Some(l) => level_number(l).ok_or_else(|| format!("\"{entry}\": cannot read the level \"{l}\""))?,
        };
        Ok(Want { item, enchant: Some((name, min)) })
    }

    /// The buy-list entry for this want, written the way `parse` reads it back.
    pub fn entry(&self) -> String {
        match &self.enchant {
            None => self.item.clone(),
            Some((name, 1)) => format!("{}:{name}", self.item),
            Some((name, min)) => format!("{}:{name}:{min}", self.item),
        }
    }

    /// The word this want's chest is signed with: the enchantment for a book ("mending"), else the
    /// item ("glowstone"). Spaces or underscores on the sign both match.
    pub fn sign_word(&self) -> &str {
        self.enchant.as_ref().map_or(self.item.as_str(), |(n, _)| n.as_str())
    }

    /// Does this stack count as this want: the item, and for a book the enchantment at its level?
    pub fn matches(&self, bot: &Client, st: &ItemStack) -> bool {
        inv::stack_id(st) == self.item && self.enchant.as_ref().is_none_or(|(name, min)| inv::stored_enchant_level(bot, st, name) >= *min)
    }

    /// Bought one trade per click, never by shift-click. A shift-click repeats while the result is
    /// the same ITEM, and every enchanted book is the same item: once the wanted offer locks, the
    /// server can carry the click on into another book offer the cost slots still cover.
    fn one_at_a_time(&self) -> bool {
        self.enchant.is_some()
    }
}

impl std::fmt::Display for Want {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.enchant {
            None => write!(f, "{}", self.item.replace('_', " ")),
            Some((name, 1)) => write!(f, "{} book", name.replace('_', " ")),
            Some((name, min)) => write!(f, "{} {min}+ book", name.replace('_', " ")),
        }
    }
}

/// "5" or "v" -> 5.
fn level_number(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return (n >= 1).then_some(n);
    }
    ["i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"].iter().position(|r| *r == s).map(|i| i as i32 + 1)
}

/// What an offer costs in emeralds right now: the base price moved by demand and any discount,
/// worked out as the server does (1 to 64).
pub fn price(o: &MerchantOffer) -> i32 {
    let base = o.base_cost_a.count;
    let demand = ((base * o.demand) as f32 * o.price_multiplier).floor().max(0.0) as i32;
    (base + demand + o.special_price_diff).clamp(1, 64)
}

/// The offer to buy `want` from: emeralds as the first cost and, as the second, nothing or - for an
/// enchanted book - a book; in stock; and payable now with `emeralds` and `books` at its real
/// price. The cheapest, if the villager has several.
pub fn buy_offer(bot: &Client, list: &[MerchantOffer], want: &Want, emeralds: i32, books: i32) -> Option<usize> {
    list.iter()
        .enumerate()
        .filter(|(_, o)| {
            o.base_cost_a.item == ItemKind::Emerald
                && match &o.cost_b {
                    None => true,
                    Some(b) => want.enchant.is_some() && b.item == ItemKind::Book && b.count <= books,
                }
                && want.matches(bot, &o.result)
                && !o.out_of_stock
                && o.uses < o.max_uses
                && price(o) <= emeralds
        })
        .min_by_key(|(_, o)| price(o))
        .map(|(i, _)| i)
}

/// Buy what this villager sells off `wants`, in one open sitting: each wanted offer is traded until
/// it locks, the emeralds or books run out, the bags fill, or `max` of it is bought. Opens with a
/// safe hand like a sale. Ok(how many items were bought, counted); Err if a trade handed over
/// something that is not on the list - then nothing more is bought.
pub async fn buy_from(bot: &Client, villager: Entity, pos: Vec3, wants: &[Want], max: i32) -> Result<i32, String> {
    if inv::count_id(bot, "emerald") == 0 || wants.is_empty() {
        return Ok(0);
    }
    let Some(handle) = open_villager(bot, villager, pos).await else { return Ok(0) };
    let mut total = Ok(0);
    match fresh_offers(bot, &handle).await {
        None => debug!(target: "trade", "no offers from the villager at {:.0} {:.0} {:.0}", pos.x, pos.y, pos.z),
        Some(list) => {
            for want in wants {
                let Some(idx) = buy_offer(bot, &list, want, have(bot, &handle, "emerald"), have(bot, &handle, "book")) else { continue };
                match trade_for(bot, &handle, &list[idx], idx, want, max).await {
                    Ok(got) => {
                        if got > 0 {
                            info!(target: "trade", "bought {got} {want} from the villager at {:.0} {:.0} {:.0} ({} emerald(s) a trade)", pos.x, pos.y, pos.z, price(&list[idx]));
                        }
                        total = total.map(|t| t + got);
                    }
                    Err(e) => {
                        warn!(target: "trade", "{e}");
                        total = Err(e);
                        break;
                    }
                }
            }
        }
    }
    handle.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    total
}

/// Trade one offer over and over for `want` (see `buy_from`): select it, wait for the result slot
/// to show exactly the offer's result, then take it - a shift-click for blocks, one trade per click
/// for an enchanted book, each book checked again where it lands.
async fn trade_for(bot: &Client, handle: &ContainerHandle, offer: &MerchantOffer, idx: usize, want: &Want, max: i32) -> Result<i32, String> {
    let cost = price(offer);
    let books = offer.cost_b.as_ref().map_or(0, |b| b.count);
    let held = || inv::count(bot, |s| want.matches(bot, s));
    let before = held();
    let mut trades = 0;
    let mut dry_bursts = 0;
    loop {
        if held() - before >= max || trades >= offer.max_uses - offer.uses || have(bot, handle, "emerald") < cost || have(bot, handle, "book") < books {
            break;
        }
        let Some(dest) = empty_bag_slot(handle) else { break };
        // Select the trade (the server moves the emeralds - and the book - into the cost slots).
        bot.write_packet(ServerboundSelectTrade { item: idx as u32 });
        if !result_is(bot, handle, &offer.result).await {
            break;
        }
        let pre = held();
        if want.one_at_a_time() {
            // One trade: pick the result up and put it down in an empty bag slot.
            handle.left_click(RESULT_SLOT);
            tokio::time::sleep(Duration::from_millis(200)).await;
            handle.left_click(dest);
            tokio::time::sleep(Duration::from_millis(300)).await;
            trades += 1;
            let landed = handle.menu().ok().flatten().and_then(|m| m.slot(dest).cloned()).unwrap_or_default();
            if !landed.is_empty() && !want.matches(bot, &landed) {
                return Err(format!("a trade for {want} handed over {} instead - stopped buying", describe(bot, &landed)));
            }
        } else {
            handle.shift_click(RESULT_SLOT);
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        if held() <= pre {
            dry_bursts += 1;
            if dry_bursts >= 2 {
                break;
            }
        } else {
            dry_bursts = 0;
        }
    }
    Ok((held() - before).max(0))
}

/// This villager's own offers. The menu opening and its offers are separate packets, and the list
/// kept is the last one sent: for a moment after opening it can still be the villager before's, and
/// a trade number read off it points at something else here. So wait (up to ~2 s) for the list that
/// carries this menu's container id.
async fn fresh_offers(bot: &Client, handle: &ContainerHandle) -> Option<Vec<MerchantOffer>> {
    for _ in 0..40 {
        if let Some((id, list)) = offers(bot)
            && id == handle.id()
        {
            return Some(list);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Wait (up to ~1.5 s) for the server to put `expect` in the result slot. True only once the slot
/// holds the same item with the same stored enchantments; empty (the trade is locked or unpaid) or
/// anything else is never clicked.
async fn result_is(bot: &Client, handle: &ContainerHandle, expect: &ItemStack) -> bool {
    let mut seen = ItemStack::default();
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen = handle.menu().ok().flatten().and_then(|m| m.slot(RESULT_SLOT).cloned()).unwrap_or_default();
        if same_result(bot, &seen, expect) {
            return true;
        }
    }
    if !seen.is_empty() {
        warn!(target: "trade", "the trade's result is {} where {} was expected - not taking it", describe(bot, &seen), describe(bot, expect));
    }
    false
}

/// The same trade result: the same item with the same stored enchantments (an enchanted book's
/// enchantment is all that tells one from another).
fn same_result(bot: &Client, a: &ItemStack, b: &ItemStack) -> bool {
    !a.is_empty() && !b.is_empty() && a.kind() == b.kind() && inv::stored_enchants(bot, a) == inv::stored_enchants(bot, b)
}

/// How many of `item` there is to pay with: the bags plus what a selected trade already moved into
/// the two cost slots.
fn have(bot: &Client, handle: &ContainerHandle, item: &str) -> i32 {
    let paid: i32 = handle
        .menu()
        .ok()
        .flatten()
        .map_or(0, |m| m.slots().iter().take(RESULT_SLOT).filter(|s| inv::stack_id(s) == item).map(|s| s.count()).sum());
    inv::count_id(bot, item) + paid
}

/// An empty bag slot, as a slot of the open menu (the main inventory before the hotbar).
fn empty_bag_slot(handle: &ContainerHandle) -> Option<usize> {
    let m = handle.menu().ok().flatten()?;
    let slots = m.slots();
    m.player_slots_range().find(|i| slots[*i].is_empty())
}

/// "enchanted_book (mending 1)", "glass".
fn describe(bot: &Client, st: &ItemStack) -> String {
    let e = inv::stored_enchants(bot, st);
    if e.is_empty() {
        return inv::stack_id(st).to_string();
    }
    let e: Vec<String> = e.iter().map(|(n, l)| format!("{n} {l}")).collect();
    format!("{} ({})", inv::stack_id(st), e.join(", "))
}

/// Turn to face `target`, the way a player would before interacting.
fn face(bot: &Client, target: Vec3) {
    let Ok(eye) = bot.position().map(|p| p + Vec3::new(0.0, 1.62, 0.0)) else { return };
    let d = target - eye;
    let yaw = (-d.x.atan2(d.z)).to_degrees() as f32;
    let pitch = (-d.y.atan2((d.x * d.x + d.z * d.z).sqrt())).to_degrees() as f32;
    movement::look_now(bot, yaw, pitch);
}

/// Put something that is NOT a stick in the hand before opening a villager: this server disables a
/// villager you interact with while a stick is in the main hand. Food is the natural thing to hold
/// (the bot carries it for auto-eat, and a held food item has no villager interaction, so the trade
/// still opens) - this is the hand that should have food in it. Sticks stay in the bags; the trade
/// pulls them from there, the held slot is irrelevant to the sale.
///
/// The reliable move is to SELECT a hotbar slot that already holds the wanted item - a bare
/// `SetCarriedItem`, no inventory-click. The old code emptied the hand with a container-click while
/// no container was open, which this server rejects intermittently, so the bot sometimes still held
/// a stick and disabled the villager. Here a swap is only the last resort (whole hotbar is sticks),
/// and the caller verifies the hand before the right-click regardless.
fn safe_hand(bot: &Client) {
    let slots = inv::bags(bot); // menu 9..=44 -> index 0..=35; hotbar slot h at index 27 + h
    let hot = |h: usize| slots.get(27 + h).map(|(_, s)| s.clone());
    let is_stick = |s: &ItemStack| inv::stack_id(s) == "stick";
    let is_food = |s: &ItemStack| !s.is_empty() && s.get_component::<Food>().is_some();

    // Already holding something that is not a stick (food, an emerald, an empty hand): keep it.
    if let Ok(h) = bot.selected_hotbar_slot()
        && let Some(s) = hot(h as usize)
        && !is_stick(&s)
    {
        return;
    }
    // Prefer a hotbar slot that already holds food.
    if let Some(h) = (0..9usize).find(|&h| hot(h).map(|s| is_food(&s)).unwrap_or(false)) {
        bot.set_selected_hotbar_slot(h as u8);
        return;
    }
    // Else any hotbar slot that is not a stick - an empty slot counts, an empty hand is safe too.
    if let Some(h) = (0..9usize).find(|&h| hot(h).map(|s| !is_stick(&s)).unwrap_or(true)) {
        bot.set_selected_hotbar_slot(h as u8);
        return;
    }
    // Whole hotbar is sticks: bring food up from the backpack (a swap - the caller still verifies).
    if let Some((m, _)) = slots.iter().take(27).find(|(_, s)| is_food(s)) {
        inv::hold(bot, *m, &[]);
    }
}

/// Face a villager and open its trade menu. `pos` is the villager's position (a block above it is
/// roughly its eye). None if the menu did not open - including a deliberate skip when the hand
/// still holds a stick, so this never disables a villager.
pub async fn open_villager(bot: &Client, villager: Entity, pos: Vec3) -> Option<ContainerHandle> {
    // Hold food (never a stick) before interacting; the slot change needs a few ticks to reach the
    // server, so settle after it and before the right-click.
    safe_hand(bot);
    face(bot, pos + Vec3::new(0.0, 1.3, 0.0));
    tokio::time::sleep(Duration::from_millis(250)).await;
    // Last line of defence: if the hand still holds a stick, DO NOT interact - skip this villager
    // rather than disable it. The next cycle tries again once the hotbar has a non-stick slot.
    if inv::held_id(bot) == "stick" {
        warn!(target: "trade", "hand still holds a stick - skipping the villager at {:.0} {:.0} {:.0} so it is not disabled", pos.x, pos.y, pos.z);
        return None;
    }
    bot.ecs.write().entity_mut(bot.entity).insert(WaitingForInventoryOpen);
    bot.ecs.write().trigger(EntityInteractEvent { client: bot.entity, target: villager, location: None });
    match bot.wait_for_container_open(Some(OPEN_TIMEOUT_TICKS)).await {
        Ok(h) => h,
        Err(e) => {
            warn!(target: "trade", "opening the villager at {:.0} {:.0} {:.0}: {e}", pos.x, pos.y, pos.z);
            None
        }
    }
}

/// Sell as much of `good` as this villager will buy, in one open sitting. Returns the emeralds
/// gained (counted). Stops when the trade locks (out of stock), the good runs out, or a burst moves
/// nothing. The offer list is read once, when the villager opens; a lock shows as an empty result
/// slot, and the result slot is checked before every click.
pub async fn sell(bot: &Client, villager: Entity, pos: Vec3, good: &Good) -> i32 {
    if inv::count_id(bot, good.id()) == 0 {
        return 0;
    }
    let Some(handle) = open_villager(bot, villager, pos).await else { return 0 };
    let before = inv::count_id(bot, "emerald");
    match fresh_offers(bot, &handle).await.and_then(|list| sell_offer(&list, good.item).map(|i| (i, list[i].result.clone()))) {
        None => debug!(target: "trade", "no {good} trade at the villager at {:.0} {:.0} {:.0}", pos.x, pos.y, pos.z),
        Some((idx, emeralds)) => {
            let mut dry_bursts = 0;
            while have(bot, &handle, good.id()) > 0 {
                let emeralds_pre = inv::count_id(bot, "emerald");
                // Select the trade (the server moves the goods into the cost slots); once the
                // result shows emeralds, shift-click it to take every trade it will give in one go.
                bot.write_packet(ServerboundSelectTrade { item: idx as u32 });
                if !result_is(bot, &handle, &emeralds).await {
                    break;
                }
                handle.shift_click(RESULT_SLOT);
                tokio::time::sleep(Duration::from_millis(400)).await;
                let gained = inv::count_id(bot, "emerald") - emeralds_pre;
                if gained <= 0 {
                    dry_bursts += 1;
                    if dry_bursts >= 2 {
                        debug!(target: "trade", "two bursts moved no emeralds - done with this villager");
                        break;
                    }
                } else {
                    dry_bursts = 0;
                }
            }
        }
    }
    handle.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let gained = (inv::count_id(bot, "emerald") - before).max(0);
    if gained > 0 {
        info!(target: "trade", "sold {good} to the villager at {:.0} {:.0} {:.0} for {gained} emerald(s)", pos.x, pos.y, pos.z);
    }
    gained
}

#[cfg(test)]
mod tests {
    use super::*;
    use azalea::protocol::packets::game::c_merchant_offers::{DataComponentExactPredicate, ItemCost};

    fn book(name: &str, min: i32) -> Want {
        Want { item: "enchanted_book".into(), enchant: Some((name.into(), min)) }
    }

    #[test]
    fn reads_the_buy_list() {
        assert_eq!(Want::parse("glowstone"), Ok(Want { item: "glowstone".into(), enchant: None }));
        assert_eq!(Want::parse(" minecraft:Sea Lantern "), Ok(Want { item: "sea_lantern".into(), enchant: None }));
        assert_eq!(Want::parse("enchanted_book:mending"), Ok(book("mending", 1)));
        assert_eq!(Want::parse("enchanted_book:Silk Touch"), Ok(book("silk_touch", 1)));
        assert_eq!(Want::parse("enchanted_book:sharpness:5"), Ok(book("sharpness", 5)));
        assert_eq!(Want::parse("enchanted_book:efficiency:IV"), Ok(book("efficiency", 4)));
        // Every enchanted book has the same id: a bare one would buy any book.
        assert!(Want::parse("enchanted_book").is_err());
        assert!(Want::parse("enchanted_book:").is_err());
        assert!(Want::parse("enchanted_book:sharpness:five").is_err());
        assert!(Want::parse("glass:mending").is_err());
        assert!(Want::parse("").is_err());
        assert_eq!(book("silk_touch", 1).sign_word(), "silk_touch");
        assert_eq!(book("sharpness", 5).to_string(), "sharpness 5+ book");
        assert_eq!(book("silk_touch", 1).to_string(), "silk touch book");
        // What the panel saves is read back the same.
        for entry in ["glowstone", "sea lantern", "enchanted_book:Silk Touch", "enchanted_book:sharpness:V"] {
            let w = Want::parse(entry).unwrap();
            assert_eq!(Want::parse(&w.entry()), Ok(w));
        }
        assert_eq!(Want::parse("enchanted_book:sharpness:V").unwrap().entry(), "enchanted_book:sharpness:5");
    }

    #[test]
    fn prices_an_offer_as_the_server_does() {
        let offer = |base: i32, demand: i32, mult: f32, special: i32| MerchantOffer {
            base_cost_a: ItemCost { item: ItemKind::Emerald, count: base, components: DataComponentExactPredicate { expected: Vec::new() } },
            result: ItemStack::default(),
            cost_b: None,
            out_of_stock: false,
            uses: 0,
            max_uses: 12,
            xp: 1,
            special_price_diff: special,
            price_multiplier: mult,
            demand,
        };
        assert_eq!(price(&offer(10, 0, 0.2, 0)), 10);
        assert_eq!(price(&offer(10, 5, 0.05, 0)), 12); // floor(10 * 5 * 0.05) = 2 more
        assert_eq!(price(&offer(10, -4, 0.2, 0)), 10); // negative demand never lowers it
        assert_eq!(price(&offer(10, 0, 0.2, -3)), 7); // a discount
        assert_eq!(price(&offer(60, 10, 0.2, 0)), 64); // capped at a stack
        assert_eq!(price(&offer(2, 0, 0.2, -5)), 1); // never free
    }

    #[test]
    fn reads_the_sell_list() {
        let good = |e: &str| Good::parse(e).map(|g| g.id());
        assert_eq!(good("stick"), Ok("stick"));
        assert_eq!(good(" minecraft:Iron_Ingot "), Ok("iron_ingot"));
        assert_eq!(good("iron"), Ok("iron_ingot"), "iron is the ingot, what its chest is signed with");
        assert_eq!(good("rotten flesh"), Ok("rotten_flesh"));
        assert!(Good::parse("iron ingto").is_err(), "not an item");
        assert!(Good::parse("").is_err());
        assert!(Good::parse("emerald").is_err(), "it is paid in emeralds");
        assert_eq!(Good::parse("iron_ingot").unwrap().sign_word(), "iron");
        assert_eq!(Good::parse("stick").unwrap().sign_word(), "stick");
        assert_eq!(Good::parse("iron_ingot").unwrap().to_string(), "iron ingot");
    }

    #[test]
    fn a_good_is_sold_to_the_jobs_that_buy_it() {
        use VillagerProfession as J;
        let iron = Good::parse("iron_ingot").unwrap();
        assert_eq!(iron.jobs(), vec![J::Armorer, J::Toolsmith, J::Weaponsmith]);
        assert!(iron.bought_by(Some(J::Armorer)));
        assert!(!iron.bought_by(Some(J::Fletcher)));
        assert!(iron.bought_by(None), "a villager whose job is not seen yet is tried");
        assert!(!iron.bought_by(Some(J::Nitwit)));
        let sticks = Good::parse("stick").unwrap();
        assert!(sticks.bought_by(Some(J::Fletcher)) && !sticks.bought_by(Some(J::Armorer)));
        // Nothing vanilla buys it: every villager with a job is tried.
        let odd = Good::parse("cobblestone").unwrap();
        assert!(odd.jobs().is_empty() && odd.bought_by(Some(J::Mason)) && odd.bought_by(Some(J::Fletcher)));
    }

    #[test]
    fn finds_the_offer_that_buys_a_good() {
        use azalea::inventory::ItemStack;
        let offer = |cost: ItemKind, result: ItemKind, uses: i32| MerchantOffer {
            base_cost_a: ItemCost { item: cost, count: 4, components: DataComponentExactPredicate { expected: Vec::new() } },
            result: ItemStack::new(result, 1),
            cost_b: None,
            out_of_stock: uses >= 12,
            uses,
            max_uses: 12,
            xp: 1,
            special_price_diff: 0,
            price_multiplier: 0.05,
            demand: 0,
        };
        let list = [offer(ItemKind::Coal, ItemKind::Emerald, 0), offer(ItemKind::Emerald, ItemKind::IronHelmet, 0), offer(ItemKind::IronIngot, ItemKind::Emerald, 0)];
        assert_eq!(sell_offer(&list, ItemKind::IronIngot), Some(2));
        assert_eq!(sell_offer(&list, ItemKind::Stick), None);
        let locked = [offer(ItemKind::IronIngot, ItemKind::Emerald, 12)];
        assert_eq!(sell_offer(&locked, ItemKind::IronIngot), None, "restocking");
    }
}
