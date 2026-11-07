//! The seller: a melon-farmer-shaped single goal, no model. It stays inside a sign-marked box (the
//! pen), takes what is on its sell list from the chests signed for it - sticks from "sticks", iron
//! ingots from "iron" - sells each to the villagers in the box whose job buys it (the sticks to the
//! fletchers; the iron to the armorers, toolsmiths and weaponsmiths), and banks the emeralds into
//! the chest signed "emeralds" once a stack has built up. Nothing to sell, or every villager that
//! buys it restocking -> it idles and rechecks every 5 minutes. It only moves inside the box; a
//! death drops it on its bed (set inside the box) and the next pass carries on.
//!
//! It buys nothing: that is the buyer (buyer.rs), a second account with its own emeralds from the
//! chest signed "spend", which the seller never banks into. The pen helpers here - the box, getting
//! to a villager, food - are the buyer's too.

use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    ecs::entity::Entity,
    entity::Dead,
    inventory::ItemStack,
    registry::builtin::{EntityKind, VillagerProfession},
};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::{
    bot::Shared,
    chests,
    config::{Area, Pos},
    entities, inv, movement,
    trade::{self, Good},
};

/// Strays are thrown from this far inside the box's side, facing out and a little up.
const THROW_INSET: f64 = 1.0;
const THROW_PITCH: f32 = -30.0;
/// Deposit emeralds once at least this many are carried (~a stack).
const DEPOSIT_AT: i32 = 64;
/// How long to idle before looking again when there is nothing to sell.
pub const IDLE_RECHECK: Duration = Duration::from_secs(300);
/// How long to wait before trying again when no villager could be reached at all.
const UNREACHED_RECHECK: Duration = Duration::from_secs(30);
/// Bag slots left free for emeralds when taking goods.
const KEEP_FREE: usize = 6;
/// A good is topped up from its chests when fewer than this are carried (a stack).
const TOP_UP_BELOW: i32 = 64;
/// Fewer free slots than this to top a good up into: the other goods carried go back first.
const MIN_ROOM: usize = 8;
/// Top up food from the `food` chest when carried food points drop below this.
const FOOD_LOW: i32 = 16;
/// Trip top speed in the pen: small hops that ease up to chests and villagers, no 30 b/s charges
/// across a contained box.
const MOVE_BPS: f64 = 10.0;

/// The pen's trade lists while the bot runs: read from config.json at the start, changed from the
/// panel (the seller's Sell tab, the buyer's Shop tab) and saved back, read afresh by every round.
#[derive(Clone, Debug, Default)]
pub struct Shop {
    pub sell: Vec<String>,
    pub spend_emeralds: bool,
    pub buy: Vec<String>,
    /// The tab's last answer ("added iron ingot", or why an entry was refused).
    pub note: String,
}

impl Shop {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self { sell: cfg.sell.clone(), spend_emeralds: cfg.spend_emeralds, buy: cfg.buy.clone(), note: String::new() }
    }
}

enum Deposited {
    Banked,
    Full,
    NoChest,
}

// --------------------------------------------------------------------- small helpers

pub(crate) fn in_world(bot: &Client) -> bool {
    bot.position().is_ok()
}

pub(crate) fn dead(bot: &Client) -> bool {
    bot.query_self::<azalea::ecs::query::Has<Dead>, _>(|d| d).unwrap_or(false)
}

/// Stopped, dead or out of the world: whatever is under way ends here.
pub(crate) fn interrupted(ctx: &Shared, bot: &Client) -> bool {
    ctx.quit.load(Ordering::SeqCst) || !in_world(bot) || dead(bot)
}

pub(crate) fn set_phase(ctx: &Shared, p: &str) {
    let mut r = ctx.routine.lock();
    if r.phase != p {
        if ctx.cfg.is_buyer() {
            info!(target: "buyer", "phase: {p}");
        } else {
            info!(target: "seller", "phase: {p}");
        }
        r.phase = p.to_string();
        drop(r);
        ctx.feed("say", p);
    }
}

/// Post a line to Discord once per episode (cleared by `unpost`).
pub(crate) fn post_once(ctx: &Shared, key: &str, text: &str) {
    if ctx.routine.lock().posted.insert(key.to_string()) {
        if ctx.cfg.is_buyer() {
            warn!(target: "buyer", "{text}");
        } else {
            warn!(target: "seller", "{text}");
        }
        ctx.discord.send(text);
        ctx.feed("event", text);
    }
}

pub(crate) fn unpost(ctx: &Shared, key: &str) {
    ctx.routine.lock().posted.remove(key);
}

/// Sleep, but wake on a stop, a death, or leaving the world.
pub(crate) async fn wait(ctx: &Shared, bot: &Client, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !interrupted(ctx, bot) {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// --------------------------------------------------------------------- the box (the leash)

/// A horizontal (x/z) test - the leash ignores height so the bot can fly above the pen to reach a
/// chest or a villager.
fn in_box(area: &Area, p: Vec3) -> bool {
    p.x >= area.min.x as f64 && p.x <= area.max.x as f64 + 1.0 && p.z >= area.min.z as f64 && p.z <= area.max.z as f64 + 1.0
}

/// The perimeter: the config `bounds` if set, else the box between the two corner signs "1" and "2".
pub(crate) fn bounds(ctx: &Shared, bot: &Client) -> Option<Area> {
    if let Some(b) = ctx.cfg.bounds {
        return Some(b);
    }
    corner_bounds(bot)
}

/// A corner sign's only text is a digit (1-9). Match the QUOTED digit (e.g. `"2"`): sign message
/// text is quoted in the block-entity NBT dump, but coordinates and boolean flag bytes in that same
/// dump are not, so a stray `2` in a coordinate or a `byte(1)` flag cannot be mistaken for a corner.
fn is_corner(text: &str) -> bool {
    ('1'..='9').any(|d| text.contains(&format!("\"{d}\"")))
}

/// The box enclosing EVERY corner sign ("1", "2", "3", ... - two or more). With three-plus corners
/// it is still the axis-aligned bounding box of all of them. Height is padded a little so the bot
/// can hover above the floor.
fn corner_bounds(bot: &Client) -> Option<Area> {
    let corners: Vec<BlockPos> = chests::signs(bot).into_iter().filter(|(_, t)| is_corner(t)).map(|(p, _)| p).collect();
    if corners.len() < 2 {
        return None;
    }
    Some(Area {
        min: Pos {
            x: corners.iter().map(|p| p.x).min().unwrap(),
            y: corners.iter().map(|p| p.y).min().unwrap() - 2,
            z: corners.iter().map(|p| p.z).min().unwrap(),
        },
        max: Pos {
            x: corners.iter().map(|p| p.x).max().unwrap(),
            y: corners.iter().map(|p| p.y).max().unwrap() + 6,
            z: corners.iter().map(|p| p.z).max().unwrap(),
        },
    })
}

/// Where to idle / return to: the config base, else the middle of the box, a little above its floor.
fn home_pos(ctx: &Shared, area: &Area) -> Vec3 {
    if let Some(b) = ctx.cfg.base {
        Vec3::new(b.x as f64 + 0.5, b.y as f64, b.z as f64 + 0.5)
    } else {
        Vec3::new((area.min.x + area.max.x) as f64 / 2.0 + 0.5, area.min.y as f64 + 2.0, (area.min.z + area.max.z) as f64 / 2.0 + 0.5)
    }
}

pub(crate) async fn go_home(bot: &Client, home: Vec3) {
    if bot.position().is_ok_and(|p| p.distance_to(home) > 3.0) {
        let _ = movement::fly_to(bot, home, "home").await;
    }
}

/// If we have drifted out of the box, come back to it before doing anything else.
async fn ensure_in_bounds(ctx: &Shared, bot: &Client, area: &Area, home: Vec3) {
    if bot.position().is_ok_and(|p| !in_box(area, p)) {
        set_phase(ctx, "outside the box - returning");
        let _ = movement::fly_to(bot, home, "box").await;
    }
}

/// The start of every pass in the pen, the seller's and the buyer's: movement kept small and
/// contained, out of the queue, alive, the box found and the bot inside it, food stocked. Some(the
/// box, home) to go on; None when not now (it has waited) - go round again.
pub(crate) async fn enter_pen(ctx: &Shared, bot: &Client) -> Option<(Area, Vec3)> {
    // The pen is a flat box under a ceiling and everything is a short hop away: keep movement
    // contained (no cruise to the sky, no clipping over the roof) and the hops small. Set each
    // pass so a reconnect's fresh mover (which defaults to not-contained) is put back in line.
    movement::set_contained(bot, true);
    movement::set_max_bps(bot, MOVE_BPS);
    if ctx.in_queue() {
        set_phase(ctx, "in the queue - waiting");
        tokio::time::sleep(Duration::from_secs(2)).await;
        return None;
    }
    if dead(bot) {
        set_phase(ctx, "dead - waiting to respawn");
        while dead(bot) && in_world(bot) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        return None; // respawns on the bed inside the box; the next pass resumes
    }
    let Some(area) = bounds(ctx, bot) else {
        set_phase(ctx, "no box yet - waiting for the corner signs");
        post_once(ctx, "no-box", "cannot find the box: place two or more signs reading 1, 2, 3, ... at the corners (or set \"bounds\" in config.json)");
        wait(ctx, bot, Duration::from_secs(60)).await;
        return None;
    };
    unpost(ctx, "no-box");
    let home = home_pos(ctx, &area);
    ensure_in_bounds(ctx, bot, &area, home).await;
    ensure_food(ctx, bot, &area).await;
    Some((area, home))
}

// --------------------------------------------------------------------- the loop

pub async fn run(bot: Client, ctx: Arc<Shared>) {
    info!(target: "seller", "seller on");
    // Goods every villager reached was restocking for, and when they are tried again.
    let mut resting: HashMap<Good, Instant> = HashMap::new();
    while !ctx.quit.load(Ordering::SeqCst) && in_world(&bot) {
        let Some((area, home)) = enter_pen(&ctx, &bot).await else { continue };
        throw_strays(&ctx, &bot, &area).await;

        // Bank emeralds once a stack has built up.
        if inv::count_id(&bot, "emerald") >= DEPOSIT_AT {
            match deposit(&ctx, &bot, &area).await {
                Deposited::Full => {
                    post_once(&ctx, "emerald-full",
                        "the emeralds chest is full - stopping until it has room. Empty it (some into the buyer's chest signed \"spend\"), or add another chest signed \"emeralds\".");
                    set_phase(&ctx, "emerald chest full - stopped");
                    go_home(&bot, home).await;
                    wait(&ctx, &bot, IDLE_RECHECK).await;
                    continue;
                }
                Deposited::Banked => unpost(&ctx, "emerald-full"),
                // Reported; it sells on, the emeralds staying in the bags.
                Deposited::NoChest => {}
            }
        }

        resting.retain(|_, until| *until > Instant::now());
        let pass = sell_pass(&ctx, &bot, &area, &mut resting).await;
        if pass.reached > 0 {
            unpost(&ctx, "no-reach");
        }
        if pass.sold > 0 {
            unpost(&ctx, "all-locked");
        } else if pass.offered && pass.reached == 0 {
            // Not one villager reached (or none in the box): that is not restocking - no 5-minute
            // idle on it. Go home, among them, and try again from there shortly.
            set_phase(&ctx, "could not reach a villager - trying again in 30 s");
            post_once(&ctx, "no-reach", "could not get within reach of any villager in the box - trying again every 30 s");
            go_home(&bot, home).await;
            wait(&ctx, &bot, UNREACHED_RECHECK).await;
        } else if let Some(until) = resting.values().min().copied() {
            let names: Vec<String> = resting.keys().map(|g| g.to_string()).collect();
            set_phase(&ctx, &format!("every villager that buys {} is restocking - idling", names.join(", ")));
            post_once(&ctx, "all-locked", &format!("every villager that buys {} is restocking - idling, will try again in 5 min", names.join(", ")));
            go_home(&bot, home).await;
            wait(&ctx, &bot, until.saturating_duration_since(Instant::now()).max(Duration::from_secs(5))).await;
        } else {
            set_phase(&ctx, "nothing to sell in the chests - idling, rechecking in 5 min");
            go_home(&bot, home).await;
            wait(&ctx, &bot, IDLE_RECHECK).await;
        }
    }
    info!(target: "seller", "seller stopped ({})", if in_world(&bot) { "quit" } else { "left the world" });
}

// --------------------------------------------------------------------- the sell list

/// The sell list, read. A bad entry is reported and left out - never guessed at.
fn goods(ctx: &Shared) -> Vec<Good> {
    let list = ctx.shop.lock().sell.clone();
    let mut out = Vec::new();
    let mut bad = Vec::new();
    for entry in &list {
        match Good::parse(entry) {
            Ok(g) if !out.contains(&g) => out.push(g),
            Ok(_) => {}
            Err(e) => bad.push(e),
        }
    }
    if bad.is_empty() {
        unpost(ctx, "bad-sell");
    } else {
        post_once(ctx, "bad-sell", &format!("sell list: {} - left out", bad.join("; ")));
    }
    out
}

/// The chests signed for a good: its sign word ("iron", "stick" - a "sticks" sign reads as it
/// too), not a longer word that contains it (another good's). Nearest the middle of the box first.
fn good_chests(bot: &Client, area: &Area, good: &Good, goods: &[Good]) -> Vec<BlockPos> {
    let longer: Vec<String> = goods.iter().map(|g| g.sign_word().to_string()).collect();
    chests::signed_named(bot, area, good.sign_word(), &longer)
}

/// "armorer, toolsmith, weaponsmith": the jobs that buy a good, for a message (empty: none known).
fn job_names(good: &Good) -> String {
    let jobs: Vec<&str> = good.jobs().iter().map(|j| j.to_str().strip_prefix("minecraft:").unwrap_or(j.to_str())).collect();
    jobs.join(", ")
}

/// What one pass over the sell list came to.
#[derive(Default)]
struct Pass {
    /// Emeralds gained.
    sold: i32,
    /// Villagers got within reach of.
    reached: usize,
    /// Something was carried to the villagers that buy it.
    offered: bool,
}

/// One pass over the sell list. A good is sold when a chest is signed for it in the box (or some
/// is carried) and a villager there has a job that buys it: the bags topped up with it from its
/// chests, then each of those villagers visited. A good that every villager reached was restocking
/// for rests a while (`resting`) - the rest are sold meanwhile.
async fn sell_pass(ctx: &Shared, bot: &Client, area: &Area, resting: &mut HashMap<Good, Instant>) -> Pass {
    let goods = goods(ctx);
    let mut pass = Pass::default();
    let mut signed = false;
    for good in &goods {
        if interrupted(ctx, bot) {
            break;
        }
        let chests = good_chests(bot, area, good, &goods);
        if chests.is_empty() && inv::count_id(bot, good.id()) == 0 {
            continue;
        }
        signed = true;
        let buyers: Vec<(Entity, Vec3)> = villagers(bot, area).into_iter().filter(|v| good.bought_by(v.2)).map(|(v, pos, _)| (v, pos)).collect();
        let key = format!("no-buyer-{}", good.id());
        if buyers.is_empty() {
            let jobs = job_names(good);
            post_once(ctx, &key, &format!("a chest is signed for {good} but no villager in the box buys it{} - not selling it",
                if jobs.is_empty() { String::new() } else { format!(" (the jobs that do: {jobs})") }));
            continue;
        }
        unpost(ctx, &key);
        if resting.contains_key(good) {
            continue;
        }
        if inv::count_id(bot, good.id()) < TOP_UP_BELOW && !chests.is_empty() {
            make_room(ctx, bot, area, good, &goods).await;
            take_good(ctx, bot, good, chests).await;
        }
        if inv::count_id(bot, good.id()) == 0 {
            continue;
        }
        pass.offered = true;
        let (sold, reached) = sell_to(ctx, bot, area, good, buyers).await;
        pass.sold += sold;
        pass.reached += reached;
        if sold == 0 && reached > 0 {
            resting.insert(*good, Instant::now() + IDLE_RECHECK);
        }
    }
    if signed {
        unpost(ctx, "no-sell-chest");
    } else if goods.is_empty() {
        post_once(ctx, "no-sell-chest", "the sell list is empty - nothing to sell (add something on the Sell tab)");
    } else {
        let signs: Vec<String> = goods.iter().map(|g| format!("\"{}\"", g.sign_word().replace('_', " "))).collect();
        post_once(ctx, "no-sell-chest", &format!("no chest signed {} in the box - nothing to sell", signs.join(" or ")));
    }
    pass
}

// --------------------------------------------------------------------- goods in

/// Fill the bags with `good` from its chests, keeping a few slots free for emeralds. Returns how
/// many were gained (counted).
///
/// The nearest chest next - a short hop along the wall, not the sign map's random order. That
/// order was fixed for a whole run, so the same chests were always approached from the same spot:
/// on 30 Oct six of them, one a farm-fed chest, were unreachable from there on every pass and the
/// farm chest filled up. A chest that fails is tried once more at the end, from wherever the bot is
/// by then; one that fails again is reported.
async fn take_good(ctx: &Shared, bot: &Client, good: &Good, mut left: Vec<BlockPos>) -> i32 {
    let before = inv::count_id(bot, good.id());
    let key = format!("chest-unreachable-{}", good.id());
    let mut failed: Vec<BlockPos> = Vec::new();
    let mut finished = false;
    'passes: for pass in 0..2 {
        while !left.is_empty() {
            if interrupted(ctx, bot) {
                break 'passes;
            }
            let room = inv::empty_slots(bot).saturating_sub(KEEP_FREE);
            if room == 0 {
                break 'passes;
            }
            let i = bot.position().map_or(0, |p| {
                let d = |c: &BlockPos| p.distance_to(Vec3::new(c.x as f64 + 0.5, c.y as f64 + 0.5, c.z as f64 + 0.5));
                (0..left.len()).min_by(|a, b| d(&left[*a]).total_cmp(&d(&left[*b]))).unwrap_or(0)
            });
            let c = left.swap_remove(i);
            set_phase(ctx, &format!("fetching {good}"));
            match chests::take(bot, c, |s| inv::stack_id(s) == good.id(), |s| s.count() as i64, room).await {
                Ok(n) => info!(target: "seller", "took {n} stack(s) of {good} from {c}"),
                Err(e) => {
                    warn!(target: "seller", "{good} chest at {c}: {e}");
                    failed.push(c);
                }
            }
        }
        if pass == 0 {
            left = std::mem::take(&mut failed);
        } else {
            finished = true;
        }
    }
    if !failed.is_empty() {
        let at: Vec<String> = failed.iter().map(|c| c.to_string()).collect();
        post_once(ctx, &key,
            &format!("cannot get to the {good} chest(s) at {} - it will pile up there until something in the way is moved", at.join(", ")));
    } else if finished {
        unpost(ctx, &key);
    }
    (inv::count_id(bot, good.id()) - before).max(0)
}

/// Too little room to top `good` up: the other goods carried go back to their chests first.
async fn make_room(ctx: &Shared, bot: &Client, area: &Area, good: &Good, goods: &[Good]) {
    if inv::empty_slots(bot).saturating_sub(KEEP_FREE) >= MIN_ROOM {
        return;
    }
    for other in goods.iter().filter(|g| *g != good) {
        for chest in good_chests(bot, area, other, goods) {
            if inv::count_id(bot, other.id()) == 0 || interrupted(ctx, bot) {
                break;
            }
            set_phase(ctx, &format!("putting the {other} back to make room"));
            match chests::store(bot, chest, |s| inv::stack_id(s) == other.id(), 36).await {
                Ok(n) => info!(target: "seller", "put {n} stack(s) of {other} back into {chest}"),
                Err(e) => warn!(target: "seller", "{other} chest at {chest}: {e}"),
            }
        }
    }
}

// --------------------------------------------------------------------- selling

/// The villagers in the box that can trade - grown, with a job (a baby, a nitwit or a jobless one
/// never opens a trade menu) - and each one's job, if its data has been seen.
pub(crate) fn villagers(bot: &Client, area: &Area) -> Vec<(Entity, Vec3, Option<VillagerProfession>)> {
    entities::near(bot)
        .into_iter()
        .filter(|(_, e)| {
            e.kind == EntityKind::Villager
                && !e.dead
                && !e.baby
                && in_box(area, e.pos)
                && !matches!(e.profession, Some(VillagerProfession::None | VillagerProfession::Nitwit))
        })
        .map(|(_, e)| (e.entity, e.pos, e.profession))
        .collect()
}

/// Visit each villager that buys `good` and sell it what it will take. Returns the emeralds gained
/// and how many villagers were reached - none reached is not "all restocking".
async fn sell_to(ctx: &Shared, bot: &Client, area: &Area, good: &Good, buyers: Vec<(Entity, Vec3)>) -> (i32, usize) {
    let mut total = 0;
    let mut reached = 0;
    for (v, pos) in buyers {
        if interrupted(ctx, bot) || inv::count_id(bot, good.id()) == 0 {
            break;
        }
        set_phase(ctx, &format!("selling {good}"));
        if let Err(e) = approach(bot, pos, area).await {
            warn!(target: "seller", "could not reach a villager at {:.0} {:.0} {:.0}: {e}", pos.x, pos.y, pos.z);
            continue;
        }
        reached += 1;
        total += trade::sell(bot, v, pos, good).await;
        if inv::count_id(bot, "emerald") >= DEPOSIT_AT {
            deposit(ctx, bot, area).await;
        }
    }
    (total, reached)
}

/// Get within interaction reach of a villager, never leaving the box.
///
/// It picks a clear spot beside the villager at its own level (or a block above or below) with a
/// DIRECT flight - no climb to cruise height; the pen is small and after a tall sticks chest the
/// bot would otherwise sit ~9 blocks up and never come down. The spots are close enough, and the
/// trip arrives tightly enough, that "arrived" always means within reach: a stand 1.8 out with the
/// default 1.25 arrival left the bot ~3 blocks off on 30 Oct, so it gave up on nearly every
/// villager in a fraction of a second and then idled 5 minutes as if they were all restocking.
/// Spots it can fly to without a detour come first (it does no routing in the pen). It polls the
/// real distance and stops the instant it is close enough to trade; up to four spots, then the
/// villager is skipped rather than hovered against forever.
pub(crate) async fn approach(bot: &Client, pos: Vec3, area: &Area) -> Result<(), String> {
    // Close enough for the server to accept the villager interaction (entity reach is ~3 blocks).
    const REACH: f64 = 2.8;
    // A spot at most STAND from the villager, arrived at within ARRIVE, is inside REACH.
    const STAND: f64 = 2.2;
    const ARRIVE: f64 = 0.4;
    let cur = bot.position().map_err(|e| e.to_string())?;
    if cur.distance_to(pos) <= REACH {
        movement::stop(bot);
        return Ok(());
    }
    let mut spots: Vec<(bool, f64, Vec3)> = {
        let world = bot.world().map_err(|e| e.to_string())?;
        let w = world.read();
        let mut all = Vec::new();
        for dy in [0.0, 1.0, -1.0] {
            for dx in -2..=2 {
                for dz in -2..=2 {
                    if dx == 0 && dz == 0 {
                        continue; // the villager's own cell
                    }
                    let feet = Vec3::new(pos.x.floor() + 0.5 + dx as f64, pos.y + dy, pos.z.floor() + 0.5 + dz as f64);
                    if feet.distance_to(pos) <= STAND && in_box(area, feet) && movement::geom::stand_clear(&w, feet) {
                        all.push((!movement::geom::way_clear(&w, cur, feet), cur.distance_to(feet), feet));
                    }
                }
            }
        }
        all
    };
    if spots.is_empty() {
        return Err("no clear spot beside it".into());
    }
    spots.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    for (_, _, stand) in spots.into_iter().take(4) {
        movement::go_direct_within(bot, stand, "villager", ARRIVE);
        // Up to ~6 s a spot: done the moment we are within reach; stop so the bot holds still to trade.
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if bot.position().map_err(|e| e.to_string())?.distance_to(pos) <= REACH {
                movement::stop(bot);
                return Ok(());
            }
            if !movement::busy(bot) {
                break; // trip ended short of reach (gave up on this spot)
            }
        }
        movement::stop(bot);
    }
    if bot.position().map_err(|e| e.to_string())?.distance_to(pos) <= REACH {
        Ok(())
    } else {
        Err("could not get within reach".into())
    }
}

// --------------------------------------------------------------------- emeralds out

/// The chests the seller banks emeralds in: signed "emeralds" - never the buyer's, signed "spend".
fn emerald_chests(bot: &Client, area: &Area) -> Vec<BlockPos> {
    chests::signed_where(bot, area, |t| t.contains("emerald") && !t.contains(crate::buyer::SPEND_SIGN))
}

/// Bank the emeralds, filling the emerald chests in turn.
async fn deposit(ctx: &Shared, bot: &Client, area: &Area) -> Deposited {
    let chests = emerald_chests(bot, area);
    if chests.is_empty() {
        post_once(ctx, "no-emerald-chest", "no chest signed 'emeralds' in the box - cannot bank emeralds");
        return Deposited::NoChest;
    }
    unpost(ctx, "no-emerald-chest");
    set_phase(ctx, "depositing emeralds");
    for chest in chests {
        match chests::store(bot, chest, |s| inv::stack_id(s) == "emerald", 36).await {
            Ok(n) => {
                info!(target: "seller", "deposited {n} stack(s) of emeralds into {chest}");
                if n > 0 {
                    ctx.discord.send(&format!("deposited {n} stack(s) of emeralds"));
                }
            }
            Err(e) => warn!(target: "seller", "emerald chest at {chest}: {e}"),
        }
        if inv::count_id(bot, "emerald") == 0 {
            return Deposited::Banked;
        }
    }
    // Emeralds still carried after storing all we could = the chests are full.
    Deposited::Full
}

// --------------------------------------------------------------------- food

/// Keep the bags stocked from the `food` chest; auto-eat (ticking in the handler) does the eating.
async fn ensure_food(ctx: &Shared, bot: &Client, area: &Area) {
    let Some(ae) = &ctx.autoeat else { return };
    if ae.food_points(bot) >= FOOD_LOW {
        return;
    }
    let Some(chest) = chests::signed_in(bot, area, "food").into_iter().next() else {
        post_once(ctx, "no-food-chest", "no chest signed 'food' in the box - nothing to restock food from");
        return;
    };
    unpost(ctx, "no-food-chest");
    set_phase(ctx, "getting food");
    match chests::take(bot, chest, |s| ae.edible(s), |s| s.count() as i64, 2).await {
        Ok(n) => info!(target: "seller", "took {n} stack(s) of food from {chest}"),
        Err(e) => warn!(target: "seller", "food chest at {chest}: {e}"),
    }
}

// --------------------------------------------------------------------- strays

/// What the seller keeps: its goods, emeralds, and the food it eats (thrown out, that would only be
/// fetched again from the food chest).
fn keeps(ctx: &Shared, goods: &[Good], s: &ItemStack) -> bool {
    let id = inv::stack_id(s);
    id == "emerald" || goods.iter().any(|g| g.id() == id) || ctx.autoeat.as_ref().is_some_and(|ae| ae.edible(s))
}

/// Anything else carried - picked up off the floor (4 Nov: 18 stacks of arrows had filled the bags,
/// leaving room for 3 of sticks) - is thrown out of the pen over its nearest side, to land a few
/// blocks past it. The bot never leaves the box, so it never walks over them again.
async fn throw_strays(ctx: &Shared, bot: &Client, area: &Area) {
    let goods = goods(ctx);
    let strays: Vec<(usize, ItemStack)> = inv::bags(bot).into_iter().filter(|(_, s)| !s.is_empty() && !keeps(ctx, &goods, s)).collect();
    if strays.is_empty() {
        return;
    }
    let Ok(p) = bot.position() else { return };
    set_phase(ctx, "throwing out what it does not sell");
    let (spot, yaw) = throw_spot(area, p);
    if p.distance_to(spot) > 0.5
        && let Err(e) = movement::fly_to(bot, spot, "throw-out").await
    {
        warn!(target: "seller", "could not get to the side of the box to throw out ({e}) - throwing from here");
    }
    movement::stop(bot);
    let _ = bot.set_direction(yaw, THROW_PITCH);
    // The turn reaches the server with the next tick's movement packet: a throw goes where it faces.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut thrown = Vec::new();
    for (slot, s) in strays {
        if interrupted(ctx, bot) {
            break;
        }
        let _ = bot.set_direction(yaw, THROW_PITCH);
        if inv::throw(bot, slot) {
            thrown.push(format!("{} {}", s.count(), inv::stack_id(&s).replace('_', " ")));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !thrown.is_empty() {
        info!(target: "seller", "threw out of the pen: {}", thrown.join(", "));
    }
}

/// Where to throw from, and the way to face (yaw): just inside the side of the box nearest `p`,
/// facing out. Minecraft's yaw: 0 faces +z (south), 90 faces -x (west).
fn throw_spot(area: &Area, p: Vec3) -> (Vec3, f32) {
    let (x0, x1, z0, z1) = (area.min.x as f64, area.max.x as f64 + 1.0, area.min.z as f64, area.max.z as f64 + 1.0);
    // (how far, which way is out)
    let sides: [(f64, f64, f64); 4] = [(p.x - x0, -1.0, 0.0), (x1 - p.x, 1.0, 0.0), (p.z - z0, 0.0, -1.0), (z1 - p.z, 0.0, 1.0)];
    let (_, ox, oz) = sides.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).unwrap_or((0.0, 0.0, 1.0));
    let spot = Vec3::new(
        if ox < 0.0 { x0 + THROW_INSET } else if ox > 0.0 { x1 - THROW_INSET } else { p.x },
        p.y,
        if oz < 0.0 { z0 + THROW_INSET } else if oz > 0.0 { z1 - THROW_INSET } else { p.z },
    );
    (spot, (-ox).atan2(oz).to_degrees() as f32)
}

// --------------------------------------------------------------------- the panel's Sell tab

/// The Sell tab: the sell list as the seller reads it - each entry, the chest it comes from and how
/// many villagers in the box buy it, or why it is not sold - and the emerald chest.
pub fn panel_state(bot: &Client, ctx: &Shared) -> Value {
    let shop = ctx.shop.lock().clone();
    let area = bounds(ctx, bot);
    let parsed: Vec<Result<Good, String>> = shop.sell.iter().map(|e| Good::parse(e)).collect();
    let goods: Vec<Good> = parsed.iter().filter_map(|g| g.as_ref().ok().copied()).collect();
    let near = area.as_ref().map(|a| villagers(bot, a)).unwrap_or_default();
    let at = |p: BlockPos| format!("{} {} {}", p.x, p.y, p.z);
    let sell: Vec<Value> = shop
        .sell
        .iter()
        .zip(&parsed)
        .map(|(entry, g)| match g {
            Ok(g) => {
                let chests = area.as_ref().map(|a| good_chests(bot, a, g, &goods)).unwrap_or_default();
                json!({
                    "entry": entry,
                    "label": g.to_string(),
                    "sign": g.sign_word().replace('_', " "),
                    "chest": chests.first().copied().map(at),
                    "chests": chests.len(),
                    "buyers": near.iter().filter(|v| g.bought_by(v.2)).count(),
                    "jobs": job_names(g),
                    "carried": inv::count_id(bot, g.id()),
                })
            }
            Err(e) => json!({"entry": entry, "error": e}),
        })
        .collect();
    json!({
        "sell": sell,
        "box": area.is_some(),
        "emeralds": inv::count_id(bot, "emerald"),
        "emeraldChest": area.as_ref().and_then(|a| emerald_chests(bot, a).first().copied()).map(at),
        "note": shop.note,
    })
}

/// A Sell-tab order: "sell-list" {add: entry} or {remove: entry} edits the list. A new entry is read
/// first and refused, with the reason, if it cannot be. Saved to config.json at once; the next pass
/// uses it. Returns the answer.
pub fn panel_cmd(ctx: &Shared, cmd: &Value) -> String {
    let mut shop = ctx.shop.lock();
    let answer = match (cmd["type"].as_str(), cmd["add"].as_str(), cmd["remove"].as_str()) {
        (Some("sell-list"), Some(add), _) => match Good::parse(add) {
            Err(e) => Err(e),
            Ok(g) if shop.sell.iter().any(|e| Good::parse(e) == Ok(g)) => Err(format!("{g} is already on the list")),
            Ok(g) => {
                shop.sell.push(g.id().to_string());
                let jobs = job_names(&g);
                Ok(format!("added {g} - it comes from the chest signed \"{}\"{}", g.sign_word().replace('_', " "),
                    if jobs.is_empty() { " and is offered to every villager".to_string() } else { format!(" and is sold to: {jobs}") }))
            }
        },
        (Some("sell-list"), _, Some(remove)) => match shop.sell.iter().position(|e| e == remove) {
            Some(i) => {
                shop.sell.remove(i);
                Ok(format!("removed {}", Good::parse(remove).map_or(remove.to_string(), |g| g.to_string())))
            }
            None => Err(format!("{remove} is not on the list")),
        },
        _ => Err("unknown Sell order".to_string()),
    };
    let saved = answer.is_ok().then(|| crate::config::Config::update_file(&ctx.dir, &[("sell", json!(shop.sell))]));
    let note = match (answer, saved) {
        (Ok(a), Some(Err(e))) => format!("{a} (for now - saving config.json failed: {e})"),
        (Ok(a), _) | (Err(a), _) => a,
    };
    shop.note = note.clone();
    drop(shop);
    // A changed list is reported afresh: these were about the old one.
    for key in ["bad-sell", "no-sell-chest"] {
        unpost(ctx, key);
    }
    info!(target: "seller", "sell list: {note}");
    note
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strays_go_out_over_the_nearest_side() {
        let area = Area { min: Pos { x: 0, y: 60, z: 0 }, max: Pos { x: 9, y: 70, z: 9 } };
        let at = |x, z| throw_spot(&area, Vec3::new(x, 64.0, z));
        let near = |a: f32, b: f32| (a - b).abs() < 0.01 || (a - b).abs() > 359.99;
        let (s, yaw) = at(1.5, 5.0);
        assert_eq!((s.x, s.z), (1.0, 5.0));
        assert!(near(yaw, 90.0), "west: {yaw}");
        let (s, yaw) = at(9.5, 5.0);
        assert_eq!((s.x, s.z), (9.0, 5.0));
        assert!(near(yaw, -90.0), "east: {yaw}");
        let (s, yaw) = at(5.0, 0.4);
        assert_eq!((s.x, s.z), (5.0, 1.0));
        assert!(near(yaw, 180.0), "north: {yaw}");
        let (s, yaw) = at(5.0, 9.2);
        assert_eq!((s.x, s.z), (5.0, 9.0));
        assert!(near(yaw, 0.0), "south: {yaw}");
    }
}
