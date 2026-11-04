//! The melon farmer's loop: the standalone's AiMelonRoutine, rebuilt on the Rust core. A fixed routine in
//! code - no model:
//!
//! 1. food first (the Food chest, melon slices when it is empty) - it starved on 15 Oct;
//! 2. a usable Silk Touch axe (worn ones into Repair, the best from Tools; none: one Discord line
//!    and wait);
//! 3. melon BLOCKS from the farm until the bags are full;
//! 4. build (the printer - not ported yet: it holds and says so).
//!
//! Every wait wakes for hunger, every step is judged by what the bags hold after, and the farm
//! loop can never spin (the Java one flew 33 blocks and back every 3 s for 45 minutes on 15 Oct).
//! Progress (melons mined) goes into melon-routine.json, the Java side's own file.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    entity::Dead,
    inventory::{ItemStack, components::Food, operations::ThrowClick},
    registry::builtin::BlockKind,
};
use tracing::{info, warn};

use crate::{bot::Shared, chests, inv, movement, printer, progress, schematic, sites};

/// Food carried, in hunger points (a slice 2, bread 5): under LOW it tops up, to WANT.
const FOOD_LOW: i32 = 16;
const FOOD_WANT: i32 = 48;
/// Hunger (of 20) at or under which, with nothing to eat, everything stops for food.
const HUNGRY: u32 = 12;
const FOOD_RETRY: Duration = Duration::from_secs(180);
const FOOD_CHEST_RETRY: Duration = Duration::from_secs(600);

#[derive(Default)]
pub struct RoutineState {
    /// The last schematic loaded (core_planet is 479k cells; a build resumes after every farm trip).
    plan: Option<Arc<schematic::Plan>>,
    stuck_passes: u32,
    pub phase: String,
    food_retry_at: Option<Instant>,
    food_chest_empty_at: Option<Instant>,
    pub posted: std::collections::HashSet<String>,
    pub mined_hour: i64,
    pub placed_hour: i64,
    /// Floor spots found unusable (built near the floor height, already floored, all land) and when.
    pub floor_skipped: std::collections::HashMap<(i32, i32), Instant>,
}

fn base(ctx: &Shared) -> Option<BlockPos> {
    ctx.cfg.base.map(|b| BlockPos::new(b.x, b.y, b.z))
}

fn set_phase(ctx: &Shared, p: &str) {
    let mut r = ctx.routine.lock();
    if r.phase != p {
        info!(target: "routine", "phase: {p}");
        r.phase = p.to_string();
        drop(r);
        ctx.feed("say", p);
    }
}

/// Post once per episode (cleared by `unpost` when the condition ends).
fn post_once(ctx: &Shared, key: &'static str, text: &str) {
    if ctx.routine.lock().posted.insert(key.to_string()) {
        warn!(target: "routine", "{text}");
        ctx.discord.send(text);
        ctx.feed("event", text);
    }
}

fn unpost(ctx: &Shared, key: &'static str) {
    ctx.routine.lock().posted.remove(key);
}

fn in_world(bot: &Client) -> bool {
    bot.position().is_ok()
}

pub fn is_dead(bot: &Client) -> bool {
    dead(bot)
}

pub fn is_hungry(ctx: &Shared, bot: &Client) -> bool {
    hungry(ctx, bot)
}

fn dead(bot: &Client) -> bool {
    bot.query_self::<azalea::ecs::query::Has<Dead>, _>(|d| d).unwrap_or(false)
}

/// Hungry with nothing auto-eat would eat (and not inside a retry window).
fn hungry(ctx: &Shared, bot: &Client) -> bool {
    if ctx.routine.lock().food_retry_at.is_some_and(|t| Instant::now() < t) {
        return false;
    }
    let food = bot.hunger().map(|h| h.food).unwrap_or(20);
    let points = ctx.autoeat.as_ref().map_or(1, |a| a.food_points(bot));
    food <= HUNGRY && points == 0 && !dead(bot)
}

/// Sleep, but not through hunger, a stop, or leaving the world.
async fn wait(ctx: &Shared, bot: &Client, d: Duration) {
    let until = Instant::now() + d;
    let mut look = Instant::now();
    while Instant::now() < until && !ctx.quit.load(Ordering::SeqCst) && in_world(bot) {
        if Instant::now() >= look {
            look = Instant::now() + Duration::from_secs(5);
            if hungry(ctx, bot) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The loop. Returns when the bot leaves the world (the next join starts it again) or quits.
pub async fn run(bot: Client, ctx: Arc<Shared>) {
    info!(target: "routine", "melon routine on");
    if let Some(b) = base(&ctx) {
        for w in ["tools", "repair", "food"] {
            info!(target: "routine", "chest signed {w}: {:?}", chests::signed(&bot, b, w));
        }
    }
    let mut top_up = inv::empty_slots(&bot) > 0;
    while !ctx.quit.load(Ordering::SeqCst) && in_world(&bot) {
        if ctx.in_queue() {
            set_phase(&ctx, "in the queue - waiting");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        if dead(&bot) {
            set_phase(&ctx, "dead - waiting to respawn");
            let at = bot.position().ok();
            while dead(&bot) && in_world(&bot) {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if let Some(at) = at {
                recover(&ctx, &bot, at).await;
            }
            continue;
        }
        ensure_food(&ctx, &bot).await;
        if !ensure_axe(&ctx, &bot).await {
            set_phase(&ctx, "waiting for a Silk Touch axe in the Tools chest");
            wait(&ctx, &bot, Duration::from_secs(300)).await;
            continue;
        }
        let melons = inv::count_id(&bot, "melon");
        if melons == 0 && inv::empty_slots(&bot) == 0 {
            // Every slot holds something else: nothing here throws your things out.
            set_phase(&ctx, "bags full with no melons - waiting");
            post_once(&ctx, "bags-full", &format!("bags are full with no melons in them ({}) - cannot farm, waiting", bags_line(&bot)));
            wait(&ctx, &bot, Duration::from_secs(300)).await;
            continue;
        }
        unpost(&ctx, "bags-full");
        if melons == 0 || top_up {
            top_up = !farm(&ctx, &bot).await;
            continue;
        }
        place_next(&ctx, &bot).await;
    }
    info!(target: "routine", "melon routine stopped ({})", if in_world(&bot) { "quit" } else { "left the world" });
}

fn bags_line(bot: &Client) -> String {
    let mut stacks: std::collections::HashMap<&str, i32> = std::collections::HashMap::new();
    for (_, s) in inv::bags(bot) {
        if !s.is_empty() {
            *stacks.entry(inv::stack_id(&s)).or_default() += 1;
        }
    }
    let mut v: Vec<_> = stacks.into_iter().collect();
    v.sort_by_key(|(_, n)| -n);
    v.iter().take(6).map(|(id, n)| format!("{n} {id}")).collect::<Vec<_>>().join(", ")
}

// ------------------------------------------------------------------ farming

/// Harvest until the bags are full (true) or something cuts it short (false).
async fn farm(ctx: &Shared, bot: &Client) -> bool {
    let Some(h) = &ctx.harvest else {
        post_once(ctx, "no-farm", "no melon farm plot in the config - cannot farm");
        wait(ctx, bot, Duration::from_secs(300)).await;
        return false;
    };
    // To the farm first: the farmer only works a plot it can see, and from a build 400 blocks out
    // the plot is not even loaded (10 Oct on the Java side, and again on 15 Oct here).
    if let Some(centre) = farm_centre(ctx) {
        if bot.position().is_ok_and(|p| p.distance_to(centre) > 48.0) {
            set_phase(ctx, "flying back to the melon farm");
            if let Err(e) = movement::fly_to(bot, centre, "farm").await {
                warn!(target: "routine", "could not reach the farm: {e}");
            }
        }
    }
    set_phase(ctx, "farming melons until the bags are full");
    let before = inv::count_id(bot, "melon");
    let broken_before = h.broken();
    h.set_on(true);
    let mut last = before;
    let mut last_gain = Instant::now();
    let mut restarts = 0;
    let full = loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if ctx.quit.load(Ordering::SeqCst) || !in_world(bot) || dead(bot) {
            break false;
        }
        if hungry(ctx, bot) {
            info!(target: "routine", "hungry with nothing to eat - leaving the farm for food");
            break false;
        }
        let now = inv::count_id(bot, "melon");
        if now > last {
            last = now;
            last_gain = Instant::now();
            restarts = 0;
        }
        if !h.on() {
            let why = h.status();
            if why.starts_with("bags full") {
                break true;
            }
            if why.starts_with("no usable") || why.starts_with("melon slices") {
                info!(target: "routine", "farmer stopped: {why}");
                break false;
            }
            // Stopped for anything else with work left: on again, but never in a tight loop.
            restarts += 1;
            warn!(target: "routine", "farmer stopped itself ({why}) - switching it back on ({restarts})");
            if restarts > 3 {
                wait(ctx, bot, Duration::from_secs(60)).await;
            }
            h.set_on(true);
            continue;
        }
        if last_gain.elapsed() > Duration::from_secs(600) {
            info!(target: "routine", "farm quiet for 10 min with {now} melons - still waiting to fill up");
            last_gain = Instant::now();
        }
    };
    h.set_on(false);
    movement::stop(bot);
    let got = (inv::count_id(bot, "melon") - before).max(0);
    ctx.routine.lock().mined_hour += got as i64;
    crate::progress::add_mined(ctx, got as i64);
    info!(target: "routine", "farmed {got} melons ({} breaks, {} carried){}", h.broken() - broken_before, inv::count_id(bot, "melon"), if full { " - full" } else { "" });
    full
}

// ------------------------------------------------------------------ the axe

/// Worn: the routine's retire point or the never-break rule (Inv.nearlyBroken: at most max(10,
/// 3%) uses left - it set a 60/2031 axe aside on 15 Oct).
/// A few blocks over the middle of the farm plot.
fn farm_centre(ctx: &Shared) -> Option<Vec3> {
    let f = ctx.cfg.farm?;
    Some(Vec3::new((f.min.x + f.max.x) as f64 / 2.0 + 0.5, f.max.y as f64 + 3.0, (f.min.z + f.max.z) as f64 / 2.0 + 0.5))
}

pub fn worn(st: &ItemStack, retire_at: i32) -> bool {
    let Some(left) = inv::uses_left(st) else { return false };
    let max = st.get_component::<azalea::inventory::components::MaxDamage>().map_or(0, |m| m.amount);
    left <= retire_at || left <= 10.max(max * 3 / 100)
}

fn silk_axe(bot: &Client, st: &ItemStack) -> bool {
    inv::stack_id(st).ends_with("_axe") && inv::enchant_level(bot, st, "silk_touch") > 0
}

/// A usable Silk Touch axe in the bags, or false. At the base: worn ones go in Repair, the best
/// usable one comes out of Tools. None anywhere: one Discord line, and the caller waits.
async fn ensure_axe(ctx: &Shared, bot: &Client) -> bool {
    let Some(h) = &ctx.harvest else { return true };
    if h.silk_axe(bot).is_some() {
        unpost(ctx, "repairs");
        return true;
    }
    set_phase(ctx, "getting a fresh Silk Touch axe");
    let Some(b) = base(ctx) else {
        post_once(ctx, "no-base", "no base in the config - the Tools chest cannot be found");
        return false;
    };
    let retire = h.retire_at();
    if let Err(e) = movement::fly_to(bot, Vec3::new(b.x as f64 + 0.5, b.y as f64, b.z as f64 + 0.5), "base").await {
        warn!(target: "routine", "could not reach the base: {e}");
        return false;
    }
    if let Some(repair) = chests::signed(bot, b, "repair") {
        let n = inv::bags(bot).iter().filter(|(_, s)| silk_axe(bot, s) && worn(s, retire)).count();
        if n > 0 {
            let r = chests::store(bot, repair, |s| silk_axe(bot, s) && worn(s, retire), n).await;
            info!(target: "routine", "worn axes into Repair: {r:?}");
        }
    }
    match chests::signed(bot, b, "tools") {
        Some(tools) => {
            let r = chests::take(bot, tools, |s| silk_axe(bot, s) && !worn(s, retire), |s| inv::uses_left(s).unwrap_or(0) as i64, 1).await;
            info!(target: "routine", "from Tools: {r:?}");
        }
        None => warn!(target: "routine", "no chest signed Tools near the base"),
    }
    if h.silk_axe(bot).is_some() {
        unpost(ctx, "repairs");
        return true;
    }
    post_once(ctx, "repairs", "repairs needed: no usable Silk Touch axe is left in the Tools chest - waiting until there is one");
    false
}

// ------------------------------------------------------------------ food

/// Food, judged by what is carried after. Under FOOD_LOW: the Food chest first, melon slices broken
/// bare-handed when it is empty or missing. A top-up waits for a pass with room in the bags;
/// hungry with nothing to eat, a stack of melons goes to make room.
async fn ensure_food(ctx: &Shared, bot: &Client) {
    let Some(ae) = &ctx.autoeat else { return };
    let points = ae.food_points(bot);
    let food = bot.hunger().map(|h| h.food).unwrap_or(20);
    if points >= FOOD_LOW || ctx.routine.lock().food_retry_at.is_some_and(|t| Instant::now() < t) {
        return;
    }
    let urgent = food <= HUNGRY && points == 0;
    if !urgent && inv::empty_slots(bot) == 0 {
        return;
    }
    set_phase(ctx, if urgent { "hungry with nothing to eat - getting food" } else { "topping up food" });
    info!(target: "routine", "food {points} points carried, hunger {food}/20 - {}", if urgent { "hungry" } else { "topping up" });
    if urgent && inv::empty_slots(bot) == 0 {
        make_room(bot);
    }
    food_from_chest(ctx, bot).await;
    if ae.food_points(bot) < FOOD_LOW {
        get_slices(ctx, bot).await;
    }
    let after = ae.food_points(bot);
    let food_after = bot.hunger().map(|h| h.food).unwrap_or(20);
    if after > points || food_after > food {
        info!(target: "routine", "food {points} -> {after} points, hunger {food} -> {food_after}");
        unpost(ctx, "no-food");
        return;
    }
    ctx.routine.lock().food_retry_at = Some(Instant::now() + FOOD_RETRY);
    warn!(target: "routine", "got NO food - hunger {food_after}/20, trying again in 3 min");
    if urgent {
        post_once(ctx, "no-food", &format!("no food: the Food chest is empty or missing and no melon slices could be got - hunger {food_after}/20, trying again every 3 minutes"));
    }
}

/// Hungry with full bags: one stack of melon blocks out (the farm refills it), so food fits.
fn make_room(bot: &Client) {
    let slot = inv::bags(bot).into_iter().rev().find(|(_, s)| inv::stack_id(s) == "melon").map(|(i, _)| i);
    match slot {
        Some(i) => {
            if let Ok(h) = bot.get_inventory() {
                h.click(ThrowClick::All { slot: i as u16 });
                info!(target: "routine", "bags full and hungry - threw out a stack of melons to make room for food");
            }
        }
        None => warn!(target: "routine", "bags full and hungry, and no melon stack to throw out for room"),
    }
}

async fn food_from_chest(ctx: &Shared, bot: &Client) {
    let Some(ae) = &ctx.autoeat else { return };
    if ctx.routine.lock().food_chest_empty_at.is_some_and(|t| t.elapsed() < FOOD_CHEST_RETRY) {
        return;
    }
    let Some(b) = base(ctx) else { return };
    if movement::fly_to(bot, Vec3::new(b.x as f64 + 0.5, b.y as f64, b.z as f64 + 0.5), "base").await.is_err() {
        return;
    }
    let Some(chest) = chests::signed(bot, b, "food") else {
        info!(target: "routine", "no chest signed Food near the base - melon slices instead");
        ctx.routine.lock().food_chest_empty_at = Some(Instant::now());
        return;
    };
    for _ in 0..3 {
        let had = ae.food_points(bot);
        if had >= FOOD_WANT || inv::empty_slots(bot) == 0 {
            return;
        }
        let points = |s: &ItemStack| s.count() as i64 * s.get_component::<Food>().map_or(0, |f| f.nutrition) as i64;
        let r = chests::take(bot, chest, |s| ae.edible(s), points, 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let now = ae.food_points(bot);
        if now <= had {
            info!(target: "routine", "the Food chest gave no food ({r:?}) - melon slices instead");
            ctx.routine.lock().food_chest_empty_at = Some(Instant::now());
            post_once(ctx, "food-chest-empty", "got no food from the Food chest (empty?) - eating melon slices until it is restocked");
            return;
        }
        unpost(ctx, "food-chest-empty");
        info!(target: "routine", "took food from the Food chest: {had} -> {now} points");
    }
}

/// The fallback: slices from farm melons broken with no tool (START+STOP with an empty or
/// tool-free hand; the server parks the slow bare-hand break and finishes it).
async fn get_slices(ctx: &Shared, bot: &Client) {
    let Some(farm) = ctx.cfg.farm else { return };
    if inv::empty_slots(bot) == 0 && !inv::bags(bot).iter().any(|(_, s)| inv::stack_id(s) == "melon_slice" && s.count() < 64) {
        info!(target: "routine", "no room in the bags for melon slices");
        return;
    }
    set_phase(ctx, "getting melon slices to eat");
    let before = inv::count_id(bot, "melon_slice");
    let mut broken = 0;
    for _ in 0..5 {
        let Ok(pos) = bot.position() else { return };
        let melon = {
            let Ok(world) = bot.world() else { return };
            let w = world.read();
            let states = BlockKind::Melon.into();
            w.find_blocks(BlockPos::from(pos), &states)
                .find(|b| b.x >= farm.min.x && b.x <= farm.max.x && b.z >= farm.min.z && b.z <= farm.max.z)
        };
        let Some(m) = melon else { break };
        if movement::fly_to(bot, Vec3::new(m.x as f64 + 0.5, m.y as f64 + 2.0, m.z as f64 + 0.5), "slices").await.is_err() {
            continue;
        }
        // An empty or tool-free hotbar slot in hand.
        let slots = inv::bags(bot);
        let Some(bare) = (0..9u8).find(|h| {
            let s = &slots[(36 + *h as usize) - 9].1;
            s.is_empty() || inv::uses_left(s).is_none()
        }) else {
            break;
        };
        bot.set_selected_hotbar_slot(bare);
        tokio::time::sleep(Duration::from_millis(150)).await;
        crate::modules::harvest::send_break(bot, m);
        tokio::time::sleep(Duration::from_millis(1800)).await;
        broken += 1;
        // Pick the slices up where they fell.
        let _ = movement::fly_to(bot, Vec3::new(m.x as f64 + 0.5, m.y as f64, m.z as f64 + 0.5), "slices").await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if ctx.autoeat.as_ref().is_some_and(|a| a.food_points(bot) >= FOOD_WANT) {
            break;
        }
    }
    info!(target: "routine", "broke {broken} melon(s) bare-handed, {} slice(s) gained", inv::count_id(bot, "melon_slice") - before);
}

// ------------------------------------------------------------------ after a death

/// Back to where it died and pick up what lies there (items last 5 minutes).
async fn recover(ctx: &Shared, bot: &Client, at: Vec3) {
    set_phase(ctx, "respawned - going back for the items");
    info!(target: "routine", "recovering items at {:.0} {:.0} {:.0}", at.x, at.y, at.z);
    let _ = movement::fly_to(bot, at, "recover").await;
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until && in_world(bot) {
        let near: Vec<Vec3> = crate::entities::near(bot)
            .into_iter()
            .filter(|(_, e)| e.item.is_some() && e.pos.distance_to(at) < 12.0)
            .map(|(_, e)| e.pos)
            .collect();
        let Some(next) = near.first() else { break };
        let _ = movement::fly_to(bot, *next, "recover").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    info!(target: "routine", "recovery done");
}

// ------------------------------------------------------------------ building

/// The routine's build settings, from ai-pilot in modules.json (the Java names).
struct Builds {
    list: Vec<String>,
    dir: std::path::PathBuf,
}

fn builds(ctx: &Shared) -> Builds {
    Builds {
        list: ctx.modules.get("ai-pilot", "melon-builds", Vec::new()),
        dir: ctx.cfg.schematics.clone().unwrap_or_default(),
    }
}

fn stem(file: &str) -> String {
    file.rsplit_once('.').map_or(file, |(s, _)| s).to_string()
}

fn plan(ctx: &Shared, file: &str) -> Option<Arc<schematic::Plan>> {
    load_plan(ctx, file, false)
}

/// `any`: every block of the file is a melon cell (the ceiling art), not only its melons.
fn load_plan(ctx: &Shared, file: &str, any: bool) -> Option<Arc<schematic::Plan>> {
    if let Some(p) = ctx.routine.lock().plan.clone().filter(|p| p.name.eq_ignore_ascii_case(file)) {
        return Some(p);
    }
    let path = builds(ctx).dir.join(file);
    match if any { schematic::load_any(&path) } else { schematic::load(&path) } {
        Ok(p) => {
            info!(target: "routine", "loaded {}: {}x{}x{}, {} melons", p.name, p.w, p.h, p.d, p.total);
            let p = Arc::new(p);
            ctx.routine.lock().plan = Some(p.clone());
            Some(p)
        }
        Err(e) => {
            warn!(target: "routine", "could not read {}: {e:#}", path.display());
            None
        }
    }
}

/// What to build next (AiMelonRoutine.placeNext): builds marked done without a check go back on
/// the list; then the one in progress, one asked for, a started one, any open one.
async fn place_next(ctx: &Shared, bot: &Client) {
    let b = builds(ctx);
    let mut reopened = Vec::new();
    let mut p = progress::update(ctx, |p| {
        let unchecked: Vec<String> = p.done.iter().filter(|d| p.site(d).is_some_and(|s| !s.checked)).cloned().collect();
        for d in unchecked {
            p.done.retain(|x| *x != d);
            if let Some(s) = p.site_mut(&d) {
                s.layer = 0;
            }
            reopened.push(stem(&d));
        }
    });
    if !reopened.is_empty() {
        ctx.discord.send(&format!("rechecking {}: marked finished without a check - going back to build whatever is missing", reopened.join(", ")));
    }
    // A floor piece already begun is finished first, ceiling or not.
    if p.floor_current.is_some() {
        lay_floor(ctx, bot).await;
        return;
    }
    if p.ceiling_current.is_some() && ceiling_file(ctx).is_some() {
        fill(ctx, bot).await;
        return;
    }
    if p.current.is_none() && p.floor_requests > 0 {
        progress::update(ctx, |p| p.floor_requests -= 1);
        fill(ctx, bot).await;
        return;
    }
    let pick = match p.current.clone() {
        Some(c) => c,
        None => {
            let left: Vec<&String> = b.list.iter().filter(|x| !p.done.contains(x)).collect();
            if left.is_empty() {
                fill(ctx, bot).await;
                return;
            }
            // Started ones first (a half-built one is finished before a new one is begun), then a
            // random one: each built once, in random order.
            let started: Option<&String> = left.iter().copied().find(|x| p.site(x).is_some());
            p.next_build
                .clone()
                .filter(|n| left.iter().any(|x| *x == n))
                .or_else(|| started.cloned())
                .unwrap_or_else(|| left[rand_index(left.len())].clone())
        }
    };
    if p.site(&pick).is_none() {
        let Some(plan) = plan(ctx, &pick) else {
            hold(ctx, bot, "no-schematic", &format!("cannot read the schematic {pick} - holding")).await;
            return;
        };
        set_phase(ctx, &format!("finding a spot for {}", stem(&pick)));
        let Some(rules) = site_rules(ctx, bot) else {
            hold(ctx, bot, "no-base", "no base in the config - nowhere to measure build sites from").await;
            return;
        };
        match sites::find_build_site(ctx, bot, &rules, &p, plan.w, plan.h, plan.d).await {
            Some(o) => {
                info!(target: "routine", "{} goes at {} ({}x{}x{})", stem(&pick), o, plan.w, plan.h, plan.d);
                p = progress::update(ctx, |p| {
                    p.sites.push(progress::Site { file: pick.clone(), x: o.x, y: o.y, z: o.z, w: plan.w, h: plan.h, d: plan.d, ..Default::default() });
                    p.current = Some(pick.clone());
                    p.next_build = None;
                });
            }
            None => {
                // No room found nearby yet: floor (or ceiling) meanwhile (the Java side did the same).
                info!(target: "routine", "no open spot found for {} yet - {} meanwhile", stem(&pick),
                    if ceiling_file(ctx).is_some() { "building the ceiling" } else { "laying floor" });
                progress::update(ctx, |p| {
                    p.current = None;
                    p.floor_requests += 1;
                });
                return;
            }
        }
    }
    if p.current.as_deref() != Some(pick.as_str()) {
        p = progress::update(ctx, |p| {
            p.current = Some(pick.clone());
            p.next_build = None;
        });
    }
    build(ctx, bot, &pick, &p).await;
}

async fn hold(ctx: &Shared, bot: &Client, key: &'static str, why: &str) {
    set_phase(ctx, why);
    post_once(ctx, key, why);
    wait(ctx, bot, Duration::from_secs(300)).await;
}

async fn build(ctx: &Shared, bot: &Client, file: &str, p: &progress::Progress) {
    let name = stem(file);
    set_phase(ctx, &format!("building {name}"));
    let Some(plan) = plan(ctx, file) else {
        hold(ctx, bot, "no-schematic", &format!("cannot read the schematic {file} - holding")).await;
        return;
    };
    let site = p.site(file).cloned().unwrap();
    let origin = BlockPos::new(site.x, site.y, site.z);
    let f = file.to_string();
    let res = printer::print(ctx, bot, &plan, origin, None, site.layer, |layer| {
        progress::update(ctx, |p| {
            if let Some(s) = p.site_mut(&f) {
                s.layer = layer;
            }
        });
    })
    .await;
    ctx.routine.lock().placed_hour += res.placed;
    progress::update(ctx, |p| p.placed += res.placed);
    match res.end {
        printer::End::Stuck => stuck_pass(ctx, bot, &name, res.placed).await,
        printer::End::Done => {
            ctx.routine.lock().stuck_passes = 0;
            unpost(ctx, "stuck");
            set_phase(ctx, &format!("checking {name} is really finished"));
            let fly_y = (site.y + site.h + 4).min(318) as f64;
            let Some(c) = printer::check(ctx, bot, &plan, origin, None, fly_y).await else { return };
            let was = site.missing;
            if c.missing > 0 && was.is_none_or(|m| (c.missing as i32) < m) {
                progress::update(ctx, |p| {
                    if let Some(s) = p.site_mut(file) {
                        s.missing = Some(c.missing as i32);
                        s.layer = c.lowest;
                    }
                });
                info!(target: "routine", "{name} is NOT finished - {} of {} missing, lowest at y {} - building on from there", c.missing, c.total, c.lowest);
                return;
            }
            let p = progress::update(ctx, |p| {
                if let Some(s) = p.site_mut(file) {
                    s.missing = Some(c.missing as i32);
                    s.checked = true;
                }
                if !p.done.iter().any(|d| d == file) {
                    p.done.push(file.to_string());
                }
                p.current = None;
            });
            let left = if c.missing == 0 {
                format!("checked: all {} melons in place", c.total)
            } else {
                format!("checked: {} of {} melons in place, {} would not place", c.total - c.missing, c.total, c.missing)
            };
            let total = builds(ctx).list.len();
            info!(target: "routine", "{name} finished - {left}");
            ctx.discord.send(&format!("finished building {name} - {left} ({} of {total} builds done)", p.done.len()));
        }
        _ => {}
    }
}

/// A print pass that ended away from the build: count it, and unstick.
async fn stuck_pass(ctx: &Shared, bot: &Client, name: &str, placed: i64) {
    let passes = {
        let mut r = ctx.routine.lock();
        if placed > 0 { r.stuck_passes = 0 } else { r.stuck_passes += 1 }
        r.stuck_passes
    };
    if passes == 0 {
        unpost(ctx, "stuck");
    }
    unstick(ctx, bot, name, passes).await;
}

/// After a print pass that ended away from the build. Never the same thing over and over (17 Oct:
/// 16 hours of one climb into something only the server sees, retried every 5 minutes): every
/// trip already leaves out the ways the server refused, and from the third pass with nothing
/// placed the routine changes more than the route - the farm, then a reconnect, then longer waits.
async fn unstick(ctx: &Shared, bot: &Client, name: &str, passes: u32) {
    match passes {
        0..=2 => wait(ctx, bot, Duration::from_secs(5)).await,
        3 => {
            post_once(ctx, "stuck", &format!("stuck on {name}: the printer placed nothing in 3 passes - starting over from the farm, then a reconnect, then longer waits between tries"));
            set_phase(ctx, "stuck - back to the farm to set off from there");
            if let Some(c) = farm_centre(ctx)
                && let Err(e) = movement::fly_to(bot, c, "unstick").await
            {
                warn!(target: "routine", "could not reach the farm to start over: {e}");
            }
        }
        4 => {
            set_phase(ctx, "stuck - reconnecting for a fresh copy of the world");
            warn!(target: "routine", "stuck on {name} for 4 passes - reconnecting: the server refuses moves through what this client sees as open air");
            bot.disconnect();
            // The routine ends with the connection; the next join starts it again.
            wait(ctx, bot, Duration::from_secs(10)).await;
        }
        n => {
            let mins = (5u64 << (n - 5).min(3)).min(30);
            set_phase(ctx, &format!("stuck - trying again in {mins} min"));
            warn!(target: "routine", "stuck on {name} for {n} passes - trying again in {mins} min");
            wait(ctx, bot, Duration::from_secs(mins * 60)).await;
        }
    }
}

fn rand_index(n: usize) -> usize {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    nanos as usize % n.max(1)
}

/// The site rules from the config and modules.json (ai-pilot melon-gap / melon-home-clear /
/// melon-floor-y), and the world's build limit.
fn site_rules(ctx: &Shared, bot: &Client) -> Option<sites::SiteRules> {
    let world = bot.world().ok()?;
    let max_y = {
        let w = world.read();
        w.chunks.min_y() + w.chunks.height() as i32 - 1
    };
    Some(sites::SiteRules {
        base: base(ctx)?,
        farm: ctx.cfg.farm,
        gap: ctx.modules.get("ai-pilot", "melon-gap", 20),
        home_clear: ctx.modules.get("ai-pilot", "melon-home-clear", 200),
        floor_y: ctx.modules.get("ai-pilot", "melon-floor-y", 64),
        max_y,
    })
}

/// The floor: a flat melon slab at the floor height on open ocean, spiralling out from the base,
/// filling round land and islands (only water/air cells at that height) - never flattening.
async fn lay_floor(ctx: &Shared, bot: &Client) {
    let file: String = ctx.modules.get("ai-pilot", "melon-floor", "MELON FLOOR.litematic".to_string());
    let Some(plan) = plan(ctx, &file) else {
        hold(ctx, bot, "no-schematic", &format!("cannot read the floor schematic {file} - holding")).await;
        return;
    };
    let Some(rules) = site_rules(ctx, bot) else {
        hold(ctx, bot, "no-base", "no base in the config - nowhere to lay floor from").await;
        return;
    };
    let mut p = progress::load(ctx);
    if p.floor_current.is_none() {
        set_phase(ctx, "finding open ocean for the floor");
        match sites::find_floor_site(ctx, bot, &rules, &p, plan.w, plan.d).await {
            Some((x0, z0)) => p = progress::update(ctx, |p| p.floor_current = Some([x0, z0])),
            None => {
                hold(ctx, bot, "no-ocean", "no open ocean found for the floor nearby - waiting").await;
                return;
            }
        }
    }
    let fc = p.floor_current.unwrap();
    let origin = BlockPos::new(fc[0], rules.floor_y, fc[1]);
    set_phase(ctx, "laying floor");
    let centre = Vec3::new((origin.x + plan.w / 2) as f64, (rules.floor_y + 4) as f64, (origin.z + plan.d / 2) as f64);
    if let Err(e) = movement::fly_to(bot, centre, "floor").await {
        warn!(target: "routine", "could not reach the floor piece: {e}");
    }
    // Fill round land, never flatten: only the cells that are open (air or water, nothing over
    // them - sites::floor_cell_open) or already melon.
    let open: std::collections::HashSet<BlockPos> = {
        let Ok(world) = bot.world() else { return };
        let w = world.read();
        plan.layers
            .values()
            .flatten()
            .map(|(x, y, z)| BlockPos::new(origin.x + x, origin.y + y, origin.z + z))
            .filter(|c| {
                w.get_block_state(*c).is_some_and(|st| azalea::registry::builtin::BlockKind::from(st) == azalea::registry::builtin::BlockKind::Melon)
                    || sites::floor_cell_open(&w, *c, rules.gap)
            })
            .collect()
    };
    info!(target: "routine", "floor piece at {} {}: {} open cells", origin.x, origin.z, open.len());
    let res = printer::print(ctx, bot, &plan, origin, Some(&open), 0, |_| {}).await;
    ctx.routine.lock().placed_hour += res.placed;
    progress::update(ctx, |p| p.placed += res.placed);
    if res.end != printer::End::Done {
        return;
    }
    set_phase(ctx, "checking the floor piece is really laid");
    let Some(c) = printer::check(ctx, bot, &plan, origin, Some(&open), (rules.floor_y + 6) as f64).await else { return };
    let was = p.floor_missing;
    if c.missing > 0 && was.is_none_or(|m| (c.missing as i32) < m) {
        progress::update(ctx, |p| p.floor_missing = Some(c.missing as i32));
        info!(target: "routine", "floor piece at {} {} is NOT laid - {} of {} missing - going over it again", origin.x, origin.z, c.missing, c.total);
        return;
    }
    let p = progress::update(ctx, |p| {
        p.floors.push(fc);
        p.floor_current = None;
        p.floor_missing = None;
    });
    info!(target: "routine", "floor piece {} laid at {} {} - checked: {} of {} melons in place", p.floors.len(), origin.x, origin.z, c.total - c.missing, c.total);
}

// ------------------------------------------------------------------ the ceiling

/// The ceiling art (ai-pilot melon-ceiling), or None for the floor instead. Set, it takes the
/// floor's place everywhere: after the builds, and while no site is found for one.
fn ceiling_file(ctx: &Shared) -> Option<String> {
    let f: String = ctx.modules.get("ai-pilot", "melon-ceiling", String::new());
    (!f.trim().is_empty()).then_some(f)
}

/// What the routine fills its time with once the builds are done: the ceiling, or the floor.
async fn fill(ctx: &Shared, bot: &Client) {
    match ceiling_file(ctx) {
        Some(f) => build_ceiling(ctx, bot, &f).await,
        None => lay_floor(ctx, bot).await,
    }
}

/// The top layer of a build alone, as layer 0.
fn one_layer(p: &schematic::Plan) -> schematic::Plan {
    let cells: Vec<(i32, i32, i32)> = p.layers.values().last().map(|v| v.iter().map(|&(x, _, z)| (x, 0, z)).collect()).unwrap_or_default();
    schematic::Plan { name: p.name.clone(), w: p.w, h: 1, d: p.d, total: cells.len(), layers: std::collections::BTreeMap::from([(0, cells)]) }
}

/// How far out from the base the ceiling's tiles are laid out.
const CEILING_REACH: i32 = 3000;

/// Ceiling tile origins (x, z): the first centred over the base, the rest on a grid with a 1-block
/// gap between tiles - nearest first, and round each ring by angle, so it spirals out.
fn ceiling_tiles(base: BlockPos, w: i32, d: i32) -> Vec<(i32, i32)> {
    let (pw, pd) = (w + 1, d + 1);
    let (ri, rj) = (CEILING_REACH / pw + 1, CEILING_REACH / pd + 1);
    let mut out: Vec<(f64, f64, i32, i32)> = Vec::new();
    for i in -ri..=ri {
        for j in -rj..=rj {
            let (dx, dz) = ((i * pw) as f64, (j * pd) as f64);
            out.push((dx.hypot(dz), dz.atan2(dx), i, j));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    out.into_iter().map(|(_, _, i, j)| (base.x - w / 2 + i * pw, base.z - d / 2 + j * pd)).collect()
}

/// The ceiling: tiles of the ceiling art - every block of it a melon - with their top layer at the
/// build limit, the first over the base and the rest spiralling out (ceiling_tiles). A cell
/// something else already fills is left alone, as in the builds. Finished only by a check of the
/// world, like a build.
async fn build_ceiling(ctx: &Shared, bot: &Client, file: &str) {
    let Some(art) = load_plan(ctx, file, true) else {
        hold(ctx, bot, "no-schematic", &format!("cannot read the ceiling art {file} - holding")).await;
        return;
    };
    // One block thick: the art's top layer only - a flat piece at build height (a "flat"
    // schematic is often the same layer several times over).
    let plan = one_layer(&art);
    let Some(b) = base(ctx) else {
        hold(ctx, bot, "no-base", "no base in the config - nowhere to start the ceiling from").await;
        return;
    };
    let Ok(world) = bot.world() else { return };
    let max_y = {
        let w = world.read();
        w.chunks.min_y() + w.chunks.height() as i32 - 1
    };
    let y0 = max_y - plan.layers.keys().last().copied().unwrap_or(0);
    let mut p = progress::load(ctx);
    let tile = match p.ceiling_current {
        Some(t) => t,
        None => {
            let Some((x0, z0)) = ceiling_tiles(b, plan.w, plan.d).into_iter().find(|t| !p.ceilings.contains(&[t.0, t.1])) else {
                hold(ctx, bot, "no-ceiling", &format!("every ceiling tile within {CEILING_REACH} blocks of the base is built - holding")).await;
                return;
            };
            info!(target: "routine", "ceiling tile {} goes at {x0} {z0}, y {y0} ({}x{}, {} melons)", p.ceilings.len() + 1, plan.w, plan.d, plan.total);
            p = progress::update(ctx, |p| {
                p.ceiling_current = Some([x0, z0]);
                p.ceiling_layer = 0;
                p.ceiling_missing = None;
            });
            [x0, z0]
        }
    };
    let origin = BlockPos::new(tile[0], y0, tile[1]);
    let name = format!("ceiling tile {}", p.ceilings.len() + 1);
    set_phase(ctx, &format!("building {name}"));
    let res = printer::print(ctx, bot, &plan, origin, None, p.ceiling_layer, |layer| {
        progress::update(ctx, |p| p.ceiling_layer = layer);
    })
    .await;
    ctx.routine.lock().placed_hour += res.placed;
    progress::update(ctx, |p| p.placed += res.placed);
    match res.end {
        printer::End::Stuck => stuck_pass(ctx, bot, &name, res.placed).await,
        printer::End::Done => {
            ctx.routine.lock().stuck_passes = 0;
            unpost(ctx, "stuck");
            set_phase(ctx, &format!("checking {name} is really built"));
            let Some(c) = printer::check(ctx, bot, &plan, origin, None, (max_y + 4) as f64).await else { return };
            if c.missing > 0 && p.ceiling_missing.is_none_or(|m| (c.missing as i32) < m) {
                progress::update(ctx, |p| {
                    p.ceiling_missing = Some(c.missing as i32);
                    p.ceiling_layer = c.lowest;
                });
                info!(target: "routine", "{name} is NOT built - {} of {} missing, lowest at y {} - going over it again", c.missing, c.total, c.lowest);
                return;
            }
            let p = progress::update(ctx, |p| {
                p.ceilings.push(tile);
                p.ceiling_current = None;
                p.ceiling_missing = None;
                p.ceiling_layer = 0;
            });
            let left = if c.missing == 0 {
                format!("all {} melons in place", c.total)
            } else {
                format!("{} of {} melons in place, {} would not place", c.total - c.missing, c.total, c.missing)
            };
            info!(target: "routine", "{name} built at {} {} - checked: {left}", tile[0], tile[1]);
            ctx.discord.send(&format!("ceiling tile {} built - {left}", p.ceilings.len()));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_tiles_start_over_the_base_and_spiral_out_a_block_apart() {
        let base = BlockPos::new(1231, 71, -3460);
        let (w, d) = (219, 59);
        let t = ceiling_tiles(base, w, d);
        // Centred over the base.
        assert_eq!(t[0], (base.x - w / 2, base.z - d / 2));
        // Every tile once, on the grid, never overlapping or touching: a 1-block gap.
        let set: std::collections::HashSet<_> = t.iter().copied().collect();
        assert_eq!(set.len(), t.len());
        for (x, z) in &t {
            assert_eq!((x - t[0].0).rem_euclid(w + 1), 0);
            assert_eq!((z - t[0].1).rem_euclid(d + 1), 0);
        }
        // Nearest first: the next ones are the neighbours north and south (60 apart), then
        // further out; the distance never goes down.
        let dist = |(x, z): (i32, i32)| ((x - t[0].0) as f64).hypot((z - t[0].1) as f64);
        assert!(t[1..3].iter().all(|c| dist(*c) == 60.0), "{:?}", &t[1..3]);
        assert!(t.windows(2).all(|p| dist(p[0]) <= dist(p[1])));
    }
}
