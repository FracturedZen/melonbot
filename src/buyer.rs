//! The buyer: a second account in the seller's pen, no model. It takes emeralds from the chest
//! signed "spend" - loose, or a stack of emerald blocks it crafts into 9 stacks of emeralds; never
//! from the seller's "emeralds" chest, and the seller never banks into this one - and spends them
//! on the buy list (blocks, and enchanted books paid with a book from the
//! chest signed "books"), from the villagers in the box, banking each kind in the chest signed with
//! its name. Only what has a chest is bought. No emeralds, or nothing on the list for sale -> it
//! idles and rechecks every 5 minutes. The box, the food and the moving about are the seller's
//! (seller.rs).

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use azalea::{BlockPos, Client, entity::inventory::Inventory, inventory::ItemStack};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::{
    bot::Shared,
    chests,
    config::Area,
    inv,
    seller::{IDLE_RECHECK, approach, bounds, enter_pen, go_home, in_world, interrupted, post_once, set_phase, unpost, villagers, wait},
    trade::{self, Want},
};

/// The word on the buyer's emerald chest. The seller never banks into a chest signed with it.
pub const SPEND_SIGN: &str = "spend";
/// Take emeralds from the spend chest when fewer than this are carried.
const EMERALDS_LOW: i32 = 64;
/// Stacks of emeralds taken a visit - the bags keep room for what they buy.
const EMERALD_STACKS: usize = 4;
/// Free slots a stack of emerald blocks needs: the 9 stacks of emeralds it crafts into, and one
/// for what is bought.
const BLOCK_ROOM: usize = 10;
/// The player menu's crafting result, and the first slot of its 2x2 grid.
const CRAFT_RESULT: usize = 0;
const CRAFT_GRID: usize = 1;
/// Buy about this many of each listed item a visit (a block's shift-click can go a little past it;
/// the villager's trade stock caps it anyway).
const BUY_STACK: i32 = 64;
/// Take a stack of books from the "books" chest when fewer than this are carried (one book an
/// enchanted-book trade).
const BOOKS_LOW: i32 = 12;
/// The vanilla enchantment names that contain another one: the "protection" chest is not these.
const LONGER_NAMES: [&str; 3] = ["fire_protection", "blast_protection", "projectile_protection"];
/// How often it looks again while buying is switched off on the Shop tab.
const OFF_RECHECK: Duration = Duration::from_secs(10);

/// Buying is on and there is something to buy - and no trade has handed over something off the
/// list this run ("buy-wrong" stays posted until a restart).
fn spending(ctx: &Shared) -> bool {
    let on = {
        let shop = ctx.shop.lock();
        shop.spend_emeralds && !shop.buy.is_empty()
    };
    on && !ctx.routine.lock().posted.contains("buy-wrong")
}

// --------------------------------------------------------------------- the loop

pub async fn run(bot: Client, ctx: Arc<Shared>) {
    info!(target: "buyer", "buyer on");
    while !ctx.quit.load(Ordering::SeqCst) && in_world(&bot) {
        let Some((area, home)) = enter_pen(&ctx, &bot).await else { continue };
        if !spending(&ctx) {
            let why = if ctx.routine.lock().posted.contains("buy-wrong") {
                "buying stopped: a trade handed over something not on the list - restart the buyer"
            } else if ctx.shop.lock().buy.is_empty() {
                "nothing on the buy list - add something on the Shop tab"
            } else {
                "buying is switched off (Shop tab)"
            };
            set_phase(&ctx, why);
            go_home(&bot, home).await;
            wait(&ctx, &bot, OFF_RECHECK).await;
            continue;
        }
        if inv::count_id(&bot, "emerald") < EMERALDS_LOW {
            take_emeralds(&ctx, &bot, &area).await;
        }
        if inv::count_id(&bot, "emerald") == 0 {
            set_phase(&ctx, "no emeralds in the chest signed \"spend\" - idling, rechecking in 5 min");
            go_home(&bot, home).await;
            wait(&ctx, &bot, IDLE_RECHECK).await;
            continue;
        }
        if buy_round(&ctx, &bot, &area).await == 0 {
            set_phase(&ctx, "nothing on the buy list can be bought right now - idling, rechecking in 5 min");
            go_home(&bot, home).await;
            wait(&ctx, &bot, IDLE_RECHECK).await;
        }
    }
    info!(target: "buyer", "buyer stopped ({})", if in_world(&bot) { "quit" } else { "left the world" });
}

// --------------------------------------------------------------------- emeralds in

/// The buyer's emerald chests: signed "spend".
fn spend_chests(bot: &Client, area: &Area) -> Vec<BlockPos> {
    chests::signed_in(bot, area, SPEND_SIGN)
}

/// Take emeralds from the chests signed "spend", in turn until enough are carried: a few stacks of
/// loose ones, else a stack of emerald blocks crafted into emeralds - 64 blocks, 9 stacks. Blocks
/// that did not fit go back.
async fn take_emeralds(ctx: &Shared, bot: &Client, area: &Area) {
    // Blocks still carried from before: crafted first.
    craft_emeralds(ctx, bot).await;
    let chests = spend_chests(bot, area);
    if chests.is_empty() {
        post_once(ctx, "no-spend-chest", "no chest signed \"spend\" in the box - nothing to buy with (the seller's \"emeralds\" chest is never touched)");
        return;
    }
    unpost(ctx, "no-spend-chest");
    let is_block = |s: &ItemStack| inv::stack_id(s) == "emerald_block";
    for chest in chests {
        if interrupted(ctx, bot) || inv::count_id(bot, "emerald") >= EMERALDS_LOW {
            break;
        }
        set_phase(ctx, "fetching emeralds");
        match chests::take(bot, chest, |s| inv::stack_id(s) == "emerald", |s| s.count() as i64, EMERALD_STACKS).await {
            Ok(n) => info!(target: "buyer", "took {n} stack(s) of emeralds from {chest}"),
            Err(e) => {
                warn!(target: "buyer", "spend chest at {chest}: {e}");
                continue;
            }
        }
        if inv::count_id(bot, "emerald") >= EMERALDS_LOW {
            break;
        }
        if inv::empty_slots(bot) < BLOCK_ROOM {
            post_once(ctx, "spend-no-room", &format!("no room in the bags to craft a stack of emerald blocks ({BLOCK_ROOM} free slots needed)"));
            continue;
        }
        unpost(ctx, "spend-no-room");
        match chests::take(bot, chest, is_block, |s| s.count() as i64, 1).await {
            Ok(0) => {}
            Ok(n) => {
                info!(target: "buyer", "took {n} stack(s) of emerald blocks from {chest}");
                craft_emeralds(ctx, bot).await;
                if inv::count(bot, is_block) > 0 {
                    match chests::store(bot, chest, is_block, 36).await {
                        Ok(n) => info!(target: "buyer", "put {n} stack(s) of emerald blocks back into {chest} (no room to craft them)"),
                        Err(e) => warn!(target: "buyer", "spend chest at {chest}: {e}"),
                    }
                }
            }
            Err(e) => warn!(target: "buyer", "spend chest at {chest}: {e}"),
        }
    }
    if inv::count_id(bot, "emerald") == 0 {
        post_once(ctx, "spend-empty", "the chest signed \"spend\" has no emeralds or emerald blocks - nothing to buy with until it has some");
    } else {
        unpost(ctx, "spend-empty");
    }
}

/// The player menu's crafting result and its first grid slot, as the client last heard of them.
fn craft_slots(bot: &Client) -> Option<(ItemStack, ItemStack)> {
    bot.query_self::<&Inventory, _>(|inv| {
        let s = inv.inventory_menu.slots();
        (s.get(CRAFT_RESULT).cloned().unwrap_or_default(), s.get(CRAFT_GRID).cloned().unwrap_or_default())
    })
    .ok()
}

/// Up to 1.5 s for the crafting slots to read as `done` says.
async fn craft_settles(bot: &Client, done: impl Fn(&ItemStack, &ItemStack) -> bool) -> bool {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if craft_slots(bot).is_some_and(|(result, grid)| done(&result, &grid)) {
            return true;
        }
    }
    false
}

/// Craft the emerald blocks carried into emeralds in the bags' own 2x2 grid - no crafting table:
/// a stack into the grid, then a shift-click on the result crafts all that fit (the server does
/// the crafting; azalea's guess at it moves one result and is corrected). What did not fit goes
/// back into the bags. Returns the emeralds gained.
async fn craft_emeralds(ctx: &Shared, bot: &Client) -> i32 {
    let before = inv::count_id(bot, "emerald");
    for _ in 0..4 {
        let Some((slot, stack)) = inv::bags(bot).into_iter().find(|(_, s)| inv::stack_id(s) == "emerald_block") else { break };
        // Room for every emerald it makes (its own slot frees as it goes into the grid): blocks
        // left in the grid would be dropped at the next logout if the bags were full.
        if interrupted(ctx, bot) || inv::empty_slots(bot) + 1 < (stack.count() as usize * 9).div_ceil(64) {
            break;
        }
        // The bags' own menu: only with no container open.
        let Ok(menu) = bot.get_inventory() else { break };
        if menu.id() != 0 {
            warn!(target: "buyer", "a container is still open - not crafting the emerald blocks now");
            break;
        }
        set_phase(ctx, "crafting emerald blocks into emeralds");
        let blocks = inv::count_id(bot, "emerald_block");
        menu.left_click(slot);
        menu.left_click(CRAFT_GRID);
        if !craft_settles(bot, |result, _| inv::stack_id(result) == "emerald").await {
            warn!(target: "buyer", "no emeralds came up in the crafting grid for the blocks - taking them back out");
            menu.shift_click(CRAFT_GRID);
            craft_settles(bot, |_, grid| grid.is_empty()).await;
            break;
        }
        menu.shift_click(CRAFT_RESULT);
        // Done when the grid is empty, or the bags are full with blocks left in it.
        craft_settles(bot, |_, grid| grid.is_empty()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        if craft_slots(bot).is_some_and(|(_, grid)| !grid.is_empty()) {
            menu.shift_click(CRAFT_GRID);
            craft_settles(bot, |_, grid| grid.is_empty()).await;
        }
        let crafted = blocks - inv::count_id(bot, "emerald_block");
        info!(target: "buyer", "crafted {crafted} emerald block(s) into emeralds ({} carried now)", inv::count_id(bot, "emerald"));
        if crafted <= 0 {
            break;
        }
    }
    (inv::count_id(bot, "emerald") - before).max(0)
}

// --------------------------------------------------------------------- buying

/// The buy list, read. A bad entry is reported and left out - never guessed at.
fn wants(ctx: &Shared) -> Vec<Want> {
    let list = ctx.shop.lock().buy.clone();
    let mut out = Vec::new();
    let mut bad = Vec::new();
    for entry in &list {
        match Want::parse(entry) {
            Ok(w) => out.push(w),
            Err(e) => bad.push(e),
        }
    }
    if bad.is_empty() {
        unpost(ctx, "bad-buy");
    } else {
        post_once(ctx, "bad-buy", &format!("buy list: {} - left out", bad.join("; ")));
    }
    out
}

/// The chest signed for a want: the enchantment's name for a book ("mending", "silk touch"), the
/// item's for anything else. A sign reading a longer name that contains it (another want's, or
/// "fire protection" for "protection") is not a match.
fn want_chest(bot: &Client, area: &Area, want: &Want, all: &[Want]) -> Option<BlockPos> {
    let longer: Vec<String> = LONGER_NAMES.iter().map(|n| n.to_string()).chain(all.iter().map(|w| w.sign_word().to_string())).collect();
    chests::signed_named(bot, area, want.sign_word(), &longer).into_iter().next()
}

/// The chest signed "books": the plain books an enchanted-book trade takes. Not a sign that also
/// reads "enchanted", "bookshelf" or a wanted book's enchantment - that is where bought books go.
fn books_chest(bot: &Client, area: &Area, wants: &[Want]) -> Option<BlockPos> {
    let names: Vec<String> =
        wants.iter().filter(|w| w.enchant.is_some()).flat_map(|w| [w.sign_word().to_string(), w.sign_word().replace('_', " ")]).collect();
    chests::signed_where(bot, area, |t| {
        t.contains("book") && !t.contains("enchant") && !t.contains("bookshel") && !names.iter().any(|n| t.contains(n.as_str()))
    })
    .into_iter()
    .next()
}

/// Carry a stack of books for the enchanted-book trades when few are left.
async fn pull_books(ctx: &Shared, bot: &Client, area: &Area, wants: &[Want]) {
    if !wants.iter().any(|w| w.enchant.is_some()) || inv::count_id(bot, "book") >= BOOKS_LOW {
        return;
    }
    let Some(chest) = books_chest(bot, area, wants) else {
        post_once(ctx, "no-books-chest", "no chest signed 'books' in the box - enchanted books cannot be bought without books");
        return;
    };
    unpost(ctx, "no-books-chest");
    set_phase(ctx, "fetching books");
    match chests::take(bot, chest, |s| inv::stack_id(s) == "book", |s| s.count() as i64, 1).await {
        Ok(n) => info!(target: "buyer", "took {n} stack(s) of books from {chest}"),
        Err(e) => warn!(target: "buyer", "books chest at {chest}: {e}"),
    }
    if inv::count_id(bot, "book") == 0 {
        post_once(ctx, "no-books", "the books chest is empty - enchanted books cannot be bought until it has books");
    } else {
        unpost(ctx, "no-books");
    }
}

/// Spend emeralds on the buy list: visit each villager in the box that can trade, buy whatever on
/// the list it sells, and bank each kind in its chest after every villager. Only what has a chest
/// signed for it, with room, is bought - nothing piles up in the bags. Repeats while a pass bought
/// something. Returns how many items were bought.
async fn buy_round(ctx: &Shared, bot: &Client, area: &Area) -> i32 {
    let all = wants(ctx);
    let mut wants = Vec::new();
    let mut no_chest = Vec::new();
    for w in &all {
        if want_chest(bot, area, w, &all).is_some() {
            wants.push(w.clone());
        } else {
            no_chest.push(w.to_string());
        }
    }
    if no_chest.is_empty() {
        unpost(ctx, "buy-no-chest");
    } else {
        post_once(ctx, "buy-no-chest", &format!("no chest signed for {} in the box - not buying it", no_chest.join(", ")));
    }
    // Anything left over from before goes in first; what will not go in (chest full) is not bought.
    let full = deposit_bought(ctx, bot, area, &wants).await;
    wants.retain(|w| !full.contains(w));
    if wants.is_empty() {
        return 0;
    }
    let mut total = 0;
    loop {
        pull_books(ctx, bot, area, &wants).await;
        set_phase(ctx, "buying with emeralds");
        let mut bought = 0;
        for (v, pos, _) in villagers(bot, area) {
            if interrupted(ctx, bot) || inv::count_id(bot, "emerald") == 0 || wants.is_empty() {
                break;
            }
            if let Err(e) = approach(bot, pos, area).await {
                warn!(target: "buyer", "could not reach a villager at {:.0} {:.0} {:.0}: {e}", pos.x, pos.y, pos.z);
                continue;
            }
            match trade::buy_from(bot, v, pos, &wants, BUY_STACK).await {
                Ok(0) => {}
                Ok(n) => {
                    bought += n;
                    let full = deposit_bought(ctx, bot, area, &wants).await;
                    wants.retain(|w| !full.contains(w));
                }
                Err(e) => {
                    // Something off the list landed in the bags: stop buying for this run.
                    post_once(ctx, "buy-wrong", &format!("{e}. Buying is off until the buyer is restarted."));
                    deposit_bought(ctx, bot, area, &wants).await;
                    return total + bought;
                }
            }
        }
        total += bought;
        if bought == 0 || wants.is_empty() || inv::count_id(bot, "emerald") == 0 || interrupted(ctx, bot) {
            break;
        }
    }
    total
}

/// Bank what was bought into the chests signed for it. Returns the wants still in the bags after -
/// their chest is full (reported), so they are not bought again until it has room.
async fn deposit_bought(ctx: &Shared, bot: &Client, area: &Area, wants: &[Want]) -> Vec<Want> {
    let mut full = Vec::new();
    for w in wants {
        if inv::count(bot, |s| w.matches(bot, s)) == 0 {
            continue;
        }
        let Some(chest) = want_chest(bot, area, w, wants) else { continue };
        set_phase(ctx, "depositing what was bought");
        match chests::store(bot, chest, |s| w.matches(bot, s), 36).await {
            Ok(n) => info!(target: "buyer", "deposited {n} stack(s) of {w} into {chest}"),
            Err(e) => warn!(target: "buyer", "the chest for {w} at {chest}: {e}"),
        }
        if inv::count(bot, |s| w.matches(bot, s)) > 0 {
            full.push(w.clone());
        }
    }
    if full.is_empty() {
        unpost(ctx, "buy-chest-full");
    } else {
        let names: Vec<String> = full.iter().map(|w| w.to_string()).collect();
        post_once(ctx, "buy-chest-full", &format!("the chest for {} is full (or out of reach) - not buying more of it until it has room", names.join(", ")));
    }
    full
}

// --------------------------------------------------------------------- the panel's Shop tab

/// The Shop tab: buying on or off, the buy list as the buyer reads it - each entry, what it means,
/// and the chest it goes in or why it will not be bought - the books chest and the spend chest.
pub fn panel_state(bot: &Client, ctx: &Shared) -> Value {
    let shop = ctx.shop.lock().clone();
    let area = bounds(ctx, bot);
    let parsed: Vec<Result<Want, String>> = shop.buy.iter().map(|e| Want::parse(e)).collect();
    let all: Vec<Want> = parsed.iter().filter_map(|w| w.as_ref().ok().cloned()).collect();
    let at = |p: BlockPos| format!("{} {} {}", p.x, p.y, p.z);
    let buy: Vec<Value> = shop
        .buy
        .iter()
        .zip(&parsed)
        .map(|(entry, w)| match w {
            Ok(w) => json!({
                "entry": entry,
                "label": w.to_string(),
                "sign": w.sign_word().replace('_', " "),
                "chest": area.as_ref().and_then(|a| want_chest(bot, a, w, &all)).map(at),
            }),
            Err(e) => json!({"entry": entry, "error": e}),
        })
        .collect();
    let needs_books = all.iter().any(|w| w.enchant.is_some());
    json!({
        "spend": shop.spend_emeralds,
        "buy": buy,
        "box": area.is_some(),
        "needsBooks": needs_books,
        "booksChest": if needs_books { area.as_ref().and_then(|a| books_chest(bot, a, &all)).map(at) } else { None },
        "spendChest": area.as_ref().and_then(|a| spend_chests(bot, a).first().copied()).map(at),
        "emeralds": inv::count_id(bot, "emerald"),
        "books": inv::count_id(bot, "book"),
        "stopped": ctx.routine.lock().posted.contains("buy-wrong"),
        "note": shop.note,
    })
}

/// A Shop-tab order: "shop-spend" {on} switches buying; "shop-buy" {add: entry} or {remove: entry}
/// edits the list. A new entry is read first and refused, with the reason, if it cannot be. Saved
/// to config.json at once; the next buying round uses it. Returns the answer.
pub fn panel_cmd(ctx: &Shared, cmd: &Value) -> String {
    let mut shop = ctx.shop.lock();
    let answer = match (cmd["type"].as_str(), cmd["add"].as_str(), cmd["remove"].as_str()) {
        (Some("shop-spend"), ..) => {
            shop.spend_emeralds = cmd["on"].as_bool().unwrap_or(!shop.spend_emeralds);
            Ok(format!("buying {}", if shop.spend_emeralds { "on" } else { "off" }))
        }
        (Some("shop-buy"), Some(add), _) => match Want::parse(add) {
            Err(e) => Err(e),
            Ok(w) if shop.buy.iter().any(|e| Want::parse(e).as_ref() == Ok(&w)) => Err(format!("{w} is already on the list")),
            Ok(w) => {
                shop.buy.push(w.entry());
                Ok(format!("added {w} - it goes in the chest signed \"{}\"", w.sign_word().replace('_', " ")))
            }
        },
        (Some("shop-buy"), _, Some(remove)) => match shop.buy.iter().position(|e| e == remove) {
            Some(i) => {
                shop.buy.remove(i);
                Ok(format!("removed {}", Want::parse(remove).map_or(remove.to_string(), |w| w.to_string())))
            }
            None => Err(format!("{remove} is not on the list")),
        },
        _ => Err("unknown Shop order".to_string()),
    };
    let saved = answer.is_ok().then(|| {
        crate::config::Config::update_file(&ctx.dir, &[("spend_emeralds", json!(shop.spend_emeralds)), ("buy", json!(shop.buy))])
    });
    let note = match (answer, saved) {
        (Ok(a), Some(Err(e))) => format!("{a} (for now - saving config.json failed: {e})"),
        (Ok(a), _) | (Err(a), _) => a,
    };
    shop.note = note.clone();
    drop(shop);
    // A changed list is reported afresh: these were about the old one.
    for key in ["bad-buy", "buy-no-chest", "buy-chest-full"] {
        unpost(ctx, key);
    }
    info!(target: "buyer", "shop: {note}");
    note
}
