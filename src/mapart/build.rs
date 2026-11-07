//! The in-game side: each map in the queue built on the site (site.rs), then mapped, locked, named
//! and stored, and the platform washed for the next.
//!
//! - **Laying**: a colour at a time - one already carried first, then the most-used (plan.rs) -
//!   from the nearest chest of it, what is not needed thrown out off the platform first. The
//!   bot hovers over the art a little above the froglight lip and lays every carpet in reach, four
//!   a tick, sneaking so a dispenser under one is placed on and never opened. A carpet the world
//!   does not show after a few tries is skipped and tried once more at the end of its colour, and
//!   whatever is still left is reported - never retried for ever. Leftovers go back after.
//! - **The map**: an empty map used over the middle of the art (a map takes in everything within
//!   128 blocks of its holder, so the middle sees all of it), held a few seconds, locked at the
//!   cartography table with a glass pane, named at the anvil (XP bottles thrown for the level it
//!   costs), and put in "finished".
//! - **Washing**: the button by the "clear" sign pressed, the carpets watched until they are gone,
//!   pressed again, and the water watched until it has drained.
//!
//! A restart picks up where the world says: the carpets already down stay, and a map already
//! stored (building.json) is not made again.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use azalea::{
    BlockPos, Client, Vec3,
    block::BlockState,
    container::ContainerHandle,
    entity::{ActiveEffects, Attributes, FluidOnEyes, LookDirection, Physics, mining::get_mine_progress},
    inventory::{ItemStack, components::CustomName},
    local_player::Experience,
    protocol::packets::{Packet, game::ServerboundRenameItem},
    registry::builtin::BlockKind,
};
use serde_json::json;
use tracing::{debug, info, warn};

use super::{
    art::{self, Map, SIDE},
    plan::{self, STRIP},
    queue::{Job, JobState},
    site::{self, Site},
};
use crate::{
    bot::Shared,
    chests, inv,
    modules::harvest,
    movement,
    print::{act, place::{self, Dir}},
    seller::{dead, in_world, interrupted, wait},
};

/// Carpets sent a tick, at most: 8 clicks and their sneak, hand and move packets come to ~240 a
/// second, under Paper's limit of 1250 in 4 s.
const PER_TICK: usize = 8;
/// From the eye to the block a carpet goes on (the printer's reach).
const REACH: f64 = 4.4;
const EYE: f64 = 1.62;
/// Feet over the carpet layer flying in and out of the art: clear of the froglight lip (its top is
/// 1 over).
const HOVER: f64 = 1.25;
/// ...and while laying inside it: just over the carpets (1/16 high), for the most reach (6 Nov: at
/// HOVER the reach took in little more than the strip, and the bot stopped and started).
const LAY_HOVER: f64 = 0.25;
/// While laying, the bot aims this far past the first carpet still to lay, along its strip, so it
/// keeps moving instead of stopping at each.
const LEAD: f64 = 2.0;
/// A carpet the world does not show this long after its click is clicked again...
const RETRY_TICKS: u64 = 15;
/// ...this many times, then skipped (and tried once more at the end of its colour).
const GIVE_UP: u32 = 5;
/// The route is looked along this far past the first carpet still to lay.
const SCAN_AHEAD: usize = 256;
/// No carpet laid for this long: stuck - said, waited on, tried again.
const STALL: Duration = Duration::from_secs(90);
const TICK: Duration = Duration::from_millis(50);
/// Trip speeds: between chests, and while laying - 8.5 blocks a tick, the most a hop goes without
/// charge packets, so a move along the art is one hop (6 Nov, the user: "update player position in short distances at a time instantly").
/// A hop under 8.5 blocks needs no charge packets.
const MOVE_BPS: f64 = 60.0;
const LAY_BPS: f64 = 170.0;
/// How long the map is held over the art for it to fill in (a map draws a sixteenth of its
/// columns a tick; this is many rounds of that).
const MAP_HOLD: Duration = Duration::from_secs(6);
/// XP levels banked when bottles are thrown (a name costs one).
const XP_BANK: u32 = 3;
/// Washing: the water runs at least this long before the button is pressed again to take it
/// back (the user, 6 Nov: "wait like 20-30 seconds before pressing again"), longer while carpets
/// are still coming off, up to the most; the water then has this long to drain.
const WASH_MIN: Duration = Duration::from_secs(25);
const WASH_MAX: Duration = Duration::from_secs(30);
const DRAIN_MAX: Duration = Duration::from_secs(120);
/// Food is topped up from the "food" chest below this many hunger points carried.
const FOOD_LOW: i32 = 16;
/// Clicks are taken as not yet seen by this client's view of the bags for this many ticks...
const HAND_SETTLE: u64 = 6;
/// ...and a stack swapped into the hotbar is clicked from this many ticks later.
const SWAP_SETTLE: u64 = 4;
/// Why a carpet was skipped when the server simply never put it down.
const NO_TAKE: &str = "would not take a carpet";
/// A fetch with fewer bag slots free than this, and more still to lay than they take, makes room:
/// junk thrown out, and other colours still to lay that are carried laid first.
const ROOM: usize = 8;
/// Junk is thrown out where it comes down at least this far from every chest and block the bot
/// goes to (farther is no better; of those, the nearest)...
const TOSS_CLEAR: f64 = 12.0;
/// ...from this high over the carpet layer (over the chests and their signs), this far out past
/// the lip...
const TOSS_UP: f64 = 6.0;
const TOSS_OUT: f64 = 4.0;
/// ...coming down about this far past the lip.
const TOSS_REACH: f64 = 9.0;

enum Stop {
    /// Stopped, dead or out of the world: the map stays the one being built, carried on after.
    Interrupted,
    /// Cannot go on until someone sees to it (a chest empty, a block missing): said, waited on,
    /// tried again.
    Wait(String),
    /// This map cannot be built: given up, and the next one begun.
    Failed(String),
}

/// How a colour's laying ended.
#[derive(PartialEq)]
enum Laid {
    /// Every carpet of it down, or given up on.
    Done,
    /// The bags too crowded with colours still to come for a load of it: those go down first, and
    /// this one is carried on with after.
    Crowded,
}

fn wait_for(why: impl Into<String>) -> Stop {
    Stop::Wait(why.into())
}

fn phase(ctx: &Shared, p: &str) {
    let mut r = ctx.routine.lock();
    if r.phase != p {
        info!(target: "mapart", "phase: {p}");
        r.phase = p.to_string();
        drop(r);
        ctx.feed("say", p);
    }
}

/// The panel's line, without a log line each time (laying progress).
fn progress(ctx: &Shared, p: String) {
    ctx.routine.lock().phase = p;
}


// ------------------------------------------------------------------ the loop

pub async fn run(bot: Client, ctx: Arc<Shared>) {
    info!(target: "mapart", "mapart builder on");
    let Some(m) = super::get().cloned() else {
        warn!(target: "mapart", "the mapart side is not started - nothing to build");
        return;
    };
    let mut reported = String::new();
    let mut known: Option<Site> = None;
    let mut said = String::new();
    let mut started: HashSet<String> = HashSet::new();
    while !ctx.quit.load(Ordering::SeqCst) && in_world(&bot) {
        movement::set_contained(&bot, true);
        movement::set_speed(&bot, MOVE_BPS);
        if ctx.in_queue() {
            phase(&ctx, "in the server queue - waiting");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        if dead(&bot) {
            phase(&ctx, "dead - waiting to respawn");
            while dead(&bot) && in_world(&bot) {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            continue;
        }
        // Nothing to build: wait at the bed, the site not looked at again (6 Nov: looked at every
        // 5 s, "finding the site" and "waiting for a map" by turns in the log all night).
        let idle = m.queue.lock().waiting().is_empty();
        if idle {
            phase(&ctx, "waiting for a map");
            if let Some(s) = &known {
                go_home(&bot, s).await;
            }
            wait(&ctx, &bot, Duration::from_secs(5)).await;
            continue;
        }
        let s = match find_site(&ctx, &bot).await {
            Ok(s) => s,
            Err(e) => {
                let text = format!("Cannot find the mapart site: {e}");
                if said != text {
                    warn!(target: "mapart", "{text}");
                    said = text;
                }
                wait(&ctx, &bot, Duration::from_secs(60)).await;
                continue;
            }
        };
        known = Some(s.clone());
        let report = s.describe();
        if report != reported {
            info!(target: "mapart", "Site:\n{report}");
            reported = report;
        }
        if site::off_grid(s.art) != (0, 0) {
            let text = format!("Not building: {}.", site::grid_advice(s.art));
            if said != text {
                warn!(target: "mapart", "{text}");
                said = text;
            }
            wait(&ctx, &bot, Duration::from_secs(60)).await;
            continue;
        }
        ensure_food(&ctx, &bot, &s).await;
        let job = m.queue.lock().next();
        let Some(job) = job else { continue };
        if started.insert(job.id.clone()) {
            info!(target: "mapart", "Building {} - {} carpets, sent by {}.", job.title(), art::grouped(job.carpets), job.by);
        }
        match build(&ctx, &bot, &s, &job).await {
            Ok(note) => {
                m.queue.lock().finish(&job.id, JobState::Done, &note);
                clear_stage(&ctx);
                said.clear();
                // Discord hears two things: whether a map was taken (discord.rs) and this - the
                // rest goes to the log (the user, 6 Nov: "it doesnt need to print anything except
                // that it accepted the mapart or not and when it finishes the map").
                let text = format!("Done: {} is in the finished chest.{}", job.title(), if note.is_empty() { String::new() } else { format!(" {note}") });
                info!(target: "mapart", "{text}");
                super::post(text);
            }
            Err(Stop::Interrupted) => {}
            Err(Stop::Wait(why)) => {
                let text = format!("{}: waiting - {why}", job.title());
                if said != text {
                    warn!(target: "mapart", "{text}");
                    said = text;
                }
                movement::stop(&bot);
                wait(&ctx, &bot, Duration::from_secs(60)).await;
            }
            Err(Stop::Failed(why)) => {
                m.queue.lock().finish(&job.id, JobState::Failed, &why);
                clear_stage(&ctx);
                warn!(target: "mapart", "{} failed: {why}", job.title());
            }
        }
    }
}

/// The site, from where the bot is; when not all of it is in sight, from over the middle of what is.
async fn find_site(ctx: &Shared, bot: &Client) -> Result<Site, String> {
    phase(ctx, "finding the site");
    // From the bed the far side may be out of sight: hop toward the middle of what is seen until
    // all of it is (each hop shows more, and moves the middle on).
    let mut last = String::new();
    for k in 0..12 {
        match site::survey(bot) {
            // Short of something every map needs: looked at again a few times first, the far
            // chunks still coming in (6 Nov: straight after a restart the north side's chests,
            // table and button were all missing, and the map it made waited 60 s for a table).
            Ok(s) if k < 5 && !s.missing().is_empty() => last = format!("{} not in sight", s.missing().join(", ")),
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
        let (Some(spot), Ok(here)) = (site::lookout(bot), bot.position()) else { break };
        if here.distance_to(spot) < 6.0 {
            // Already over the middle: the far chunks are still coming in (6 Nov: every look
            // straight after a join missed part of the lip). Look again in a moment.
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        }
        // At most 48 blocks a hop, so the next look is from somewhere already in sight.
        let step = here.horizontal_distance_to(spot).max(1e-9);
        let k = (48.0 / step).min(1.0);
        let spot = Vec3::new(here.x + (spot.x - here.x) * k, spot.y, here.z + (spot.z - here.z) * k);
        info!(target: "mapart", "survey: {last} - looking again from over {}", movement::trip::fmt(spot));
        movement::fly_to(bot, spot, "lookout").await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(last)
}

async fn go_home(bot: &Client, s: &Site) {
    let Some(bed) = s.bed else { return };
    let home = Vec3::new(bed.x as f64 + 0.5, bed.y as f64 + 1.0, bed.z as f64 + 0.5);
    if bot.position().is_ok_and(|p| p.distance_to(home) > 3.0) {
        rise(bot, s).await;
    }
    if bot.position().is_ok_and(|p| p.distance_to(home) > 3.0)
        && let Err(e) = movement::fly_to(bot, home, "home").await
    {
        warn!(target: "mapart", "could not get back to the bed: {e}");
    }
}

async fn ensure_food(ctx: &Shared, bot: &Client, s: &Site) {
    let Some(ae) = &ctx.autoeat else { return };
    if ae.food_points(bot) >= FOOD_LOW {
        return;
    }
    let Some(chest) = plan::nearest_chest(s.chests(site::FOOD), &[], bot.position().unwrap_or_default()) else { return };
    phase(ctx, "getting food");
    rise(bot, s).await;
    let _ = approach(bot, s, chest).await;
    if let Err(e) = chests::take(bot, chest, |st| ae.edible(st), |st| st.count() as i64, 2).await {
        warn!(target: "mapart", "food chest at {chest}: {e}");
    }
}

// ------------------------------------------------------------------ one map

async fn build(ctx: &Shared, bot: &Client, s: &Site, job: &Job) -> Result<String, Stop> {
    let art = art::read(&ctx.dir.join(&job.file)).map_err(|e| Stop::Failed(format!("its file cannot be read now: {e}")))?;
    let map = art.maps.into_iter().find(|mp| (mp.col, mp.row) == job.map).ok_or_else(|| Stop::Failed("its map is not in its file".into()))?;
    let mut notes = Vec::new();
    if !stored(ctx, &job.id) {
        // A map carried with every carpet down is this one's, made before a restart (6 Nov:
        // restarted while getting XP for the anvil): it is carried on with, not made again.
        let needed = still_needed(bot, s, &map);
        if needed.is_empty() && filled_slots(bot).len() == 1 {
            info!(target: "mapart", "carrying on with the map made before the restart");
            toss(ctx, bot, s, &[]).await;
            finish_map(ctx, bot, s, job, true).await?;
            save_stored(ctx, &job.id);
            wash(ctx, bot, s).await?;
            return Ok(String::new());
        }
        // Carpet of a colour still to lay is kept if carried (a restart mid-colour).
        tidy(ctx, bot, s, &needed).await?;
        // Another map's carpets: washed off first. A few strays among this map's own, part laid,
        // are broken as the laying comes to them (6 Nov: 44 of them after a 60 s wait washed off
        // 1,446 carpets of the map being built).
        let (right, wrong) = platform_carpets(bot, s, &map);
        if wrong > 0 && wrong * 10 > right {
            info!(target: "mapart", "{wrong} carpet(s) on the platform that are not this map's - washing first");
            wash(ctx, bot, s).await?;
        } else if wrong > 0 {
            info!(target: "mapart", "{wrong} stray carpet(s) among the {right} of this map's already down - broken as the laying comes to them");
        }
        let mut skipped: HashMap<BlockPos, String> = HashMap::new();
        let mut done: Vec<BlockKind> = Vec::new();
        loop {
            let left: Vec<(BlockKind, usize)> = plan::colours(&map).into_iter().filter(|(k, _)| !done.contains(k)).collect();
            // A colour already carried goes down first, the most of it first (6 Nov: a map straight
            // after one with the same colours kept 20 stacks of them for later, and light blue came
            // 128 at a time); then the most-used.
            let carried = |k: BlockKind| inv::count_id(bot, art::block_id(k));
            let Some(&(kind, n)) = left.iter().filter(|(k, _)| carried(*k) > 0).max_by_key(|(k, _)| carried(*k)).or(left.first()) else { break };
            let keep: Vec<BlockKind> = still_needed(bot, s, &map).into_iter().filter(|k| !done.contains(k)).collect();
            if lay_colour(ctx, bot, s, &map, kind, n, &keep, &mut skipped).await? == Laid::Done {
                done.push(kind);
            }
            // Its leftovers out; carpet of a colour still to come is kept (6 Nov: thrown out after
            // a colour with nothing to lay, 8 stacks of the next colour went over the edge).
            let keep: Vec<BlockKind> = still_needed(bot, s, &map).into_iter().filter(|k| !done.contains(k)).collect();
            toss(ctx, bot, s, &keep).await;
        }
        if !skipped.is_empty() {
            for (p, why) in &skipped {
                warn!(target: "mapart", "not laid: {} ({why})", fmt_pos(*p));
            }
            let mut whys: Vec<&str> = skipped.values().map(|w| w.as_str()).collect();
            whys.sort();
            whys.dedup();
            notes.push(format!("{} carpet(s) could not be laid ({}) - where is in the bot's log.", skipped.len(), whys.join("; ")));
        }
        make_map(ctx, bot, s, job).await?;
        save_stored(ctx, &job.id);
    }
    wash(ctx, bot, s).await?;
    Ok(notes.join(" "))
}

// ------------------------------------------------------------------ building.json

fn stage_path(ctx: &Shared) -> std::path::PathBuf {
    ctx.dir.join("building.json")
}

/// This map is already in the finished chest (a restart between storing and washing).
fn stored(ctx: &Shared, id: &str) -> bool {
    fs::read_to_string(stage_path(ctx))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| v["job"] == id && v["stored"] == true)
}

fn save_stored(ctx: &Shared, id: &str) {
    if let Err(e) = fs::write(stage_path(ctx), json!({ "job": id, "stored": true }).to_string()) {
        warn!(target: "mapart", "could not write building.json: {e}");
    }
}

fn clear_stage(ctx: &Shared) {
    let _ = fs::remove_file(stage_path(ctx));
}

// ------------------------------------------------------------------ the bags

/// A bag slot holding `id` - one in the hotbar first, so taking it in hand is no swap (6 Nov: the
/// main bags came first, and every tick of laying swapped a stack in and out of the hotbar).
fn bag_slot(bot: &Client, id: &str) -> Option<(usize, ItemStack)> {
    let mut found: Vec<(usize, ItemStack)> = inv::bags(bot).into_iter().filter(|(_, st)| inv::stack_id(st) == id).collect();
    found.sort_by_key(|(i, _)| !inv::HOTBAR.contains(i));
    found.into_iter().next()
}

/// Before a map: carpets carried go back to their colour's chests (else "used carpet"), and a map
/// left in the bags (a restart mid-way) goes in "finished".
async fn tidy(ctx: &Shared, bot: &Client, s: &Site, keep: &[BlockKind]) -> Result<(), Stop> {
    toss(ctx, bot, s, keep).await;
    if inv::count_id(bot, "filled_map") > 0 {
        phase(ctx, "putting away a map from before");
        store_maps(bot, s).await?;
        info!(target: "mapart", "A map was still in the bags from before a restart - it is in the finished chest now.");
    }
    Ok(())
}

/// The colours this map still has carpets to lay of.
fn still_needed(bot: &Client, s: &Site, map: &Map) -> Vec<BlockKind> {
    plan::colours(map).into_iter().map(|(k, _)| k).filter(|k| left_to_lay(bot, s, map, *k) > 0).collect()
}

/// What the bot carries for the job: food, maps, glass panes, XP bottles, and carpet of the
/// colours in `keep` (still to lay).
fn keeps(ctx: &Shared, st: &ItemStack, keep: &[BlockKind]) -> bool {
    let id = inv::stack_id(st);
    matches!(id, "map" | "filled_map" | "glass_pane" | "experience_bottle" | "totem_of_undying" | "mace")
        || keep.iter().any(|k| art::block_id(*k) == id)
        || ctx.autoeat.as_ref().is_some_and(|ae| ae.edible(st))
}

/// Everything else carried - carpet left over from a colour, anything picked up - thrown out off
/// the platform (6 Nov, the user: "if it has extra carpets or junk items it tosses them outside
/// the platform"): up over the lip and out past it (the user: "if it flys above and off the
/// platform a bit it can dump there and it wont be in the way"), facing out and a little up.
/// Putting leftovers back had it trying chest after full chest, 3 s each.
async fn toss(ctx: &Shared, bot: &Client, s: &Site, keep: &[BlockKind]) {
    let junk = |bot: &Client| -> Vec<usize> { inv::bags(bot).into_iter().filter(|(_, st)| !st.is_empty() && !keeps(ctx, st, keep)).map(|(i, _)| i).collect() };
    if junk(bot).is_empty() {
        return;
    }
    let Ok(p) = bot.position() else { return };
    phase(ctx, "throwing out extra carpet and junk");
    let (lip, ox, oz) = toss_spot(s, p);
    rise(bot, s).await;
    if let Err(e) = movement::fly_to(bot, lip, "toss").await {
        warn!(target: "mapart", "could not get over the edge to throw junk out: {e}");
        return;
    }
    let off = Vec3::new(lip.x + ox * TOSS_OUT, lip.y, lip.z + oz * TOSS_OUT);
    if let Err(e) = movement::fly_to(bot, off, "toss").await {
        info!(target: "mapart", "could not get out past the edge ({e}) - throwing from over it");
    }
    movement::look_now(bot, (-ox).atan2(oz).to_degrees() as f32, -30.0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let slots = junk(bot);
    let mut what: BTreeMap<String, i32> = BTreeMap::new();
    for (i, st) in inv::bags(bot) {
        if slots.contains(&i) {
            *what.entry(inv::stack_id(&st).to_string()).or_default() += st.count();
        }
    }
    let what: Vec<String> = what.into_iter().map(|(id, n)| format!("{n} {id}")).collect();
    for (k, slot) in slots.iter().enumerate() {
        inv::throw(bot, *slot);
        if k % 4 == 3 {
            tokio::time::sleep(TICK).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    info!(target: "mapart", "threw out {} stack(s): {}", slots.len(), what.join(", "));
}

/// Where junk goes off the platform: over the point along the lip whose throw comes down farthest
/// from every chest and block the bot goes to, up to TOSS_CLEAR, and of those the nearest (6 Nov:
/// thrown over the nearest edge, by the chest it had just used, it was picked up again on the
/// next visit there - full bags, a 60 s wait and the same 3 stacks thrown every 2 minutes).
/// (over the lip, which way is out)
fn toss_spot(s: &Site, from: Vec3) -> (Vec3, f64, f64) {
    let used: Vec<BlockPos> = s.carpets.values().chain(s.chests.values()).flatten().copied().chain([s.table, s.anvil, s.button, s.bed].into_iter().flatten()).collect();
    let side = SIDE as f64;
    let (x0, z0) = (s.art.x as f64, s.art.z as f64);
    let y = s.art.y as f64 + TOSS_UP;
    // (clear, how far from here, spot, out x, out z)
    let mut best: Option<(f64, f64, Vec3, f64, f64)> = None;
    for i in 0..SIDE {
        let t = i as f64 + 0.5;
        for (x, z, ox, oz) in [(x0 + t, z0 - 0.5, 0.0, -1.0), (x0 + t, z0 + side + 0.5, 0.0, 1.0), (x0 - 0.5, z0 + t, -1.0, 0.0), (x0 + side + 0.5, z0 + t, 1.0, 0.0)] {
            let (lx, lz) = (x + ox * TOSS_REACH, z + oz * TOSS_REACH);
            let clear = used.iter().map(|p| (p.x as f64 + 0.5 - lx).hypot(p.z as f64 + 0.5 - lz)).fold(TOSS_CLEAR, f64::min).floor();
            let spot = Vec3::new(x, y, z);
            let far = from.horizontal_distance_to(spot);
            if best.is_none_or(|(c, f, ..)| clear > c || (clear == c && far < f)) {
                best = Some((clear, far, spot, ox, oz));
            }
        }
    }
    best.map_or((Vec3::new(x0 + side / 2.0, y, z0 + side + 0.5), 0.0, 1.0), |(_, _, spot, ox, oz)| (spot, ox, oz))
}

/// Exactly the carpets of the colour still to lay (as many as the bags take), from the chest on
/// the way back to `toward` (where the laying goes on); a chest found empty is passed over
/// (`empty`) until the map is done. Each fetch first throws out what is carried and not in
/// `keep` (the colours still to lay, this one among them) - the user, 6 Nov: "just make it
/// regularly dump carpets it doesnt need". Ok(false): the bags are still too crowded with other
/// colours still to lay for a load of this one - those go down first.
async fn fetch_carpet(ctx: &Shared, bot: &Client, s: &Site, kind: BlockKind, need: usize, toward: Vec3, empty: &mut Vec<BlockPos>, keep: &[BlockKind]) -> Result<bool, Stop> {
    let id = art::block_id(kind);
    let sign = art::colour(kind).to_uppercase();
    let Some(chests) = s.carpets.get(&kind) else {
        return Err(wait_for(format!("there is no chest signed {sign} for the {} carpet this map needs ({need} of it)", art::colour(kind))));
    };
    // Chests opened with no room for any of it (something picked up on the way there).
    let mut no_room = 0;
    loop {
        if interrupted(ctx, bot) {
            return Err(Stop::Interrupted);
        }
        let Some(c) = plan::chest_on_the_way(chests, empty, bot.position().unwrap_or_default(), toward) else {
            empty.clear();
            return Err(wait_for(format!("every {sign} chest is out of {} carpet - {} still to lay", art::colour(kind), art::grouped(need))));
        };
        toss(ctx, bot, s, keep).await;
        let room = inv::empty_slots(bot);
        // A chest is taken from with a slot left free.
        let fits = room.saturating_sub(1) * 64;
        if room < ROOM && need > fits && keep.iter().any(|k| *k != kind && inv::count_id(bot, art::block_id(*k)) > 0) {
            info!(target: "mapart", "room for {fits} {id} of the {need} still to lay - laying the other colours carried first");
            return Ok(false);
        }
        if room == 0 || (no_room > 0 && room <= 1) || no_room >= 3 {
            return Err(wait_for("the bags are full - nothing more fits".to_string()));
        }
        phase(ctx, &format!("fetching {} carpet", art::colour(kind)));
        let _ = approach(bot, s, c).await;
        let got = chests::take_exact(bot, c, id, need.min(i32::MAX as usize) as i32).await;
        match got {
            Ok(n) if n > 0 => return Ok(true),
            // The bags filled on the way: not the chest out of it (6 Nov: passed over as empty,
            // then 60 s waited on "the bags are full").
            Ok(_) if inv::empty_slots(bot) <= 1 => {
                info!(target: "mapart", "no room in the bags at the {sign} chest at {c} - something was picked up on the way");
                no_room += 1;
            }
            Ok(_) => {
                info!(target: "mapart", "the {sign} chest at {c} had no {id}");
                empty.push(c);
            }
            Err(e) => {
                warn!(target: "mapart", "the {sign} chest at {c}: {e}");
                empty.push(c);
            }
        }
    }
}

// ------------------------------------------------------------------ laying

/// The colour's carpets the world shows laid along its route.
fn laid(bot: &Client, s: &Site, route: &[(usize, usize)], kind: BlockKind) -> usize {
    let Ok(world) = bot.world() else { return 0 };
    let w = world.read();
    route.iter().filter(|(x, z)| w.get_block_state(s.at(*x, *z)).is_some_and(|st| BlockKind::from(st) == kind)).count()
}

/// The colour's carpets this map still needs laid.
fn left_to_lay(bot: &Client, s: &Site, map: &Map, kind: BlockKind) -> usize {
    let Ok(world) = bot.world() else { return 0 };
    let w = world.read();
    plan::route(map, kind).into_iter().filter(|(x, z)| w.get_block_state(s.at(*x, *z)).is_none_or(|st| BlockKind::from(st) != kind)).count()
}

/// Carpets on the platform that this map has there, and that it does not (another map's, left
/// from before, or strays).
fn platform_carpets(bot: &Client, s: &Site, map: &Map) -> (usize, usize) {
    let Ok(world) = bot.world() else { return (0, 0) };
    let w = world.read();
    let (mut right, mut wrong) = (0, 0);
    for z in 0..SIDE {
        for x in 0..SIDE {
            if let Some(st) = w.get_block_state(s.at(x, z)) {
                let k = BlockKind::from(st);
                if !art::is_carpet(k) {
                    continue;
                }
                if map.at(x, z) == Some(k) {
                    right += 1;
                } else {
                    wrong += 1;
                }
            }
        }
    }
    (right, wrong)
}

/// Every carpet of one colour, along its route - or, the bags too crowded with other colours still
/// to lay (`keep`) for a load of it, as many as were carried (Laid::Crowded).
async fn lay_colour(ctx: &Shared, bot: &Client, s: &Site, map: &Map, kind: BlockKind, total: usize, keep: &[BlockKind], skipped: &mut HashMap<BlockPos, String>) -> Result<Laid, Stop> {
    let id = art::block_id(kind);
    let name = art::colour(kind);
    let route = plan::route(map, kind);
    let mut empty: Vec<BlockPos> = Vec::new();
    // Strays broken where this colour goes.
    let mut broke = 0usize;
    for pass in 0..2 {
        if pass == 1 {
            // The ones the server never put down: once more, from fresh.
            let before = skipped.len();
            skipped.retain(|p, why| why != NO_TAKE || !route.iter().any(|(x, z)| s.at(*x, *z) == *p));
            if skipped.len() == before {
                break;
            }
        }
        let mut sent: HashMap<usize, (u64, u32)> = HashMap::new();
        let mut cursor = 0usize;
        let mut tick: u64 = 0;
        let mut last_dest: Option<Vec3> = None;
        let mut moved_on = Instant::now();
        // Clicks sent in the last HAND_SETTLE ticks: (tick, hotbar index, how many); and after a
        // swap into the hotbar, no clicks before this tick.
        let mut recent: VecDeque<(u64, u8, usize)> = VecDeque::new();
        let mut settle_until: u64 = 0;
        // The stray being broken: (where, given up on after this tick, what it is).
        let mut breaking: Option<(BlockPos, u64, String)> = None;
        // 20 a second (6 Nov: a sleep after each pass ran it 13.5 times a second).
        let mut pace = tokio::time::interval(TICK);
        pace.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut logged = Instant::now();
        // What the last 30 s took: clicks sent, ticks, ticks with nothing to send, and carpets the
        // world shows laid - sent far over laid is the server turning clicks down.
        let (mut clicks, mut ticks, mut idle) = (0usize, 0usize, 0usize);
        let mut laid_at_log = laid(bot, s, &route, kind);
        movement::set_speed(bot, LAY_BPS);
        loop {
            if interrupted(ctx, bot) {
                return Err(Stop::Interrupted);
            }
            tick += 1;
            let Ok(pos) = bot.position() else { return Err(Stop::Interrupted) };
            let eye = pos + Vec3::new(0.0, EYE, 0.0);
            let mut first: Option<usize> = None;
            let mut picks: Vec<(usize, BlockPos)> = Vec::new();
            let mut blocked: Vec<(BlockPos, String)> = Vec::new();
            // Something else where one of these goes, in reach: broken.
            let mut stray: Option<(BlockPos, BlockState)> = None;
            let standing;
            let mut breaking_gone = false;
            let start = cursor;
            {
                let Ok(world) = bot.world() else { return Err(Stop::Interrupted) };
                let w = world.read();
                standing = movement::geom::supported(&w, pos);
                if let Some((bp, ..)) = &breaking {
                    let bp = *bp;
                    breaking_gone = w.get_block_state(bp).is_none_or(|st| st.is_air() || BlockKind::from(st) == kind);
                }
                let mut i = cursor;
                while i < route.len() {
                    let (x, z) = route[i];
                    let p = s.at(x, z);
                    let here = w.get_block_state(p);
                    if skipped.contains_key(&p) || here.is_some_and(|st| BlockKind::from(st) == kind) {
                        if first.is_none() {
                            cursor = i + 1;
                        }
                        i += 1;
                        continue;
                    }
                    let f = *first.get_or_insert(i);
                    if i > f + SCAN_AHEAD || picks.len() >= PER_TICK {
                        break;
                    }
                    let below = p.down(1);
                    let due = sent.get(&i).is_none_or(|(t, _)| tick >= t + RETRY_TICKS);
                    if place::block_dist(eye, below) <= REACH {
                        match (here, w.get_block_state(below)) {
                            // Anything else on the carpet layer - another colour the wash missed,
                            // a stray - is broken and this one laid after (the user, 6 Nov: "there
                            // should never be carpet in the way. if flooding missed it then it
                            // breaks the carpet"). Water cannot be.
                            (Some(h), Some(_)) if !h.is_air() => {
                                if matches!(BlockKind::from(h), BlockKind::Water | BlockKind::Lava | BlockKind::BubbleColumn) {
                                    blocked.push((p, format!("{} in the way", in_the_way(h))));
                                } else if stray.is_none() && breaking.is_none() && place::block_dist(eye, p) <= REACH {
                                    stray = Some((p, h));
                                }
                            }
                            (Some(_), Some(b)) if due => {
                                if b.is_air() {
                                    blocked.push((p, "nothing under it".into()));
                                } else {
                                    picks.push((i, p));
                                }
                            }
                            _ => {}
                        }
                    }
                    i += 1;
                }
            }
            for (p, why) in blocked {
                warn!(target: "mapart", "{name} carpet at {p}: {why} - skipped");
                skipped.insert(p, why);
            }
            // One stray at a time: START and STOP together, the server finishing it in its
            // delayed slot (a fifth as fast off the ground - the perimeter bots, 6 Nov).
            if let Some((bp, until, what)) = breaking.clone() {
                if breaking_gone {
                    broke += 1;
                    breaking = None;
                } else if tick > until {
                    warn!(target: "mapart", "{name} carpet at {}: the {what} there would not break - skipped", fmt_pos(bp));
                    skipped.insert(bp, format!("{what} in the way"));
                    breaking = None;
                }
            }
            if breaking.is_none()
                && let Some((bp, st)) = stray
            {
                let what = in_the_way(st);
                let per = break_per_tick(bot, st, standing);
                if per <= 0.0 {
                    warn!(target: "mapart", "{name} carpet at {}: {what} in the way, and it cannot be broken - skipped", fmt_pos(bp));
                    skipped.insert(bp, format!("{what} in the way"));
                } else {
                    harvest::send_break(bot, bp);
                    let natural = (1.0 / per as f64).ceil() as u64;
                    debug!(target: "mapart", "breaking the {what} at {} ({natural} ticks)", fmt_pos(bp));
                    breaking = Some((bp, tick + natural * 2 + 20, what));
                }
            }
            let Some(first) = first else { break };
            if cursor != start {
                moved_on = Instant::now();
            }
            if moved_on.elapsed() > STALL {
                let (x, z) = route[first];
                movement::stop(bot);
                warn!(target: "mapart", "stuck laying {name} carpet at {}", fmt_pos(s.at(x, z)));
                return Err(wait_for(format!("stuck laying {name} carpet")));
            }
            if logged.elapsed() > Duration::from_secs(30) {
                let now = laid(bot, s, &route, kind);
                let secs = logged.elapsed().as_secs_f64();
                info!(
                    target: "mapart",
                    "laying {name}: {now} of {total} down - last {secs:.0} s: {} laid ({:.1}/s), {clicks} clicks sent, {ticks} ticks, {idle} with nothing in reach",
                    now.saturating_sub(laid_at_log),
                    now.saturating_sub(laid_at_log) as f64 / secs
                );
                laid_at_log = now;
                logged = Instant::now();
                (clicks, ticks, idle) = (0, 0, 0);
            }
            ticks += 1;
            if picks.is_empty() {
                idle += 1;
            }
            progress(ctx, format!("laying {name} carpet - {} of {}", art::grouped(cursor.min(total)), art::grouped(total)));

            // Carpets to lay with.
            let Some((slot, stack)) = bag_slot(bot, id) else {
                movement::stop(bot);
                rise(bot, s).await;
                // Exactly what is still to lay of it: not laid, not given up on.
                let need = route
                    .iter()
                    .filter(|(x, z)| !skipped.contains_key(&s.at(*x, *z)))
                    .count()
                    .saturating_sub(laid(bot, s, &route, kind));
                let (fx, fz) = route[first];
                let toward = Vec3::new(s.art.x as f64 + fx as f64 + 0.5, s.art.y as f64 + HOVER, s.art.z as f64 + fz as f64 + 0.5);
                if !fetch_carpet(ctx, bot, s, kind, need, toward, &mut empty, keep).await? {
                    movement::stop(bot);
                    movement::set_speed(bot, MOVE_BPS);
                    return Ok(Laid::Crowded);
                }
                movement::set_speed(bot, LAY_BPS);
                last_dest = None;
                moved_on = Instant::now();
                continue;
            };
            // What this client sees of the bags trails the server: the hand is clicked from only
            // with what it is sure of (6 Nov: carpets washed off the last map lie on the platform,
            // and one picked up into a hand the server had already emptied went down in the next
            // clicks - black, gray and brown in a cyan colour; and a stack swapped into the slot
            // holding glass panes and clicked from in the same tick put 8 glass panes on the art).
            recent.retain(|(t, ..)| tick < t + HAND_SETTLE);
            match inv::HOTBAR.contains(&slot).then(|| (slot - *inv::HOTBAR.start()) as u8) {
                // Into the hotbar first, and clicked from once the server has had time to say
                // otherwise.
                None => {
                    if !picks.is_empty() && inv::hold(bot, slot, &[]).is_some() {
                        settle_until = tick + SWAP_SETTLE;
                    }
                    picks.clear();
                }
                Some(_) if tick < settle_until => picks.clear(),
                // No more than it holds, less the clicks this client has not yet seen taken off it.
                Some(h) => {
                    let unseen: usize = recent.iter().filter(|(_, rh, _)| *rh == h).map(|(.., n)| n).sum();
                    picks.truncate((stack.count().max(0) as usize).saturating_sub(unseen));
                }
            }
            let mut tops = Vec::new();
            for (i, p) in &picks {
                let tries = sent.get(i).map_or(0, |(_, n)| *n);
                if tries >= GIVE_UP {
                    warn!(target: "mapart", "{name} carpet at {}: clicked {tries} times and not there - skipped for now", fmt_pos(*p));
                    skipped.insert(*p, NO_TAKE.to_string());
                    continue;
                }
                sent.insert(*i, (tick, tries + 1));
                tops.push((p.down(1), Vec3::new(p.x as f64 + 0.5, p.y as f64, p.z as f64 + 0.5)));
            }
            if !tops.is_empty()
                && let Some(h) = inv::hold(bot, slot, &[])
            {
                act::place_on_tops_sneaking(bot, h, &tops);
                clicks += tops.len();
                recent.push_back((tick, h, tops.len()));
            }

            // On along the strip: to a little past the first one still to lay while it is in
            // reach, to over it when not. Strips run west to east, then back (plan::route).
            let (fx, fz) = route[first];
            let ahead = if (fz / STRIP) % 2 == 0 { 1.0 } else { -1.0 };
            let in_reach = place::block_dist(eye, s.at(fx, fz).down(1)) <= REACH;
            let x = s.art.x as f64 + fx as f64 + 0.5 + if in_reach { LEAD * ahead } else { 0.0 };
            let x = x.clamp(s.art.x as f64 + 0.5, s.art.x as f64 + SIDE as f64 - 0.5);
            let low = inside_art(s, pos, 0.0);
            let mut dest = Vec3::new(x, s.art.y as f64 + if low { LAY_HOVER } else { HOVER }, s.art.z as f64 + ((fz / STRIP) * STRIP + STRIP / 2) as f64);
            // Over the strip's middle and still out of reach (6 Nov: an edge row, 3.4 off with the
            // arrival's slack, stuck 90 s): right over it instead.
            if !in_reach && pos.horizontal_distance_to(dest) <= 0.5 {
                dest = Vec3::new(s.art.x as f64 + fx as f64 + 0.5, dest.y, s.art.z as f64 + fz as f64 + 0.5);
            }
            if pos.distance_to(dest) > 0.3 && (!movement::busy(bot) || last_dest.is_none_or(|d| d.distance_to(dest) > 0.25)) {
                movement::go_direct_within(bot, dest, "mapart", 0.25);
                last_dest = Some(dest);
            }
            pace.tick().await;
        }
    }
    movement::stop(bot);
    movement::set_speed(bot, MOVE_BPS);
    rise(bot, s).await;
    if broke > 0 {
        info!(target: "mapart", "{name} done - {broke} stray(s) where it goes broken first");
    } else {
        info!(target: "mapart", "{name} done");
    }
    Ok(Laid::Done)
}

fn fmt_pos(p: BlockPos) -> String {
    format!("{} {} {}", p.x, p.y, p.z)
}

/// "light blue carpet", "glass pane".
fn in_the_way(st: BlockState) -> String {
    art::block_id(BlockKind::from(st)).replace('_', " ")
}

/// What a tick of breaking `st` does, as the server works it out: the held item, and a fifth of
/// it off the ground whatever is claimed (6 Nov, the perimeter bots). The held item is read before
/// the query - asking the client for anything inside it deadlocks (harvest.rs).
fn break_per_tick(bot: &Client, st: BlockState, standing: bool) -> f32 {
    let held = bot.get_held_item().unwrap_or_default();
    let per = bot
        .query_self::<(&FluidOnEyes, &Physics, &Attributes, &ActiveEffects), _>(|(f, p, a, e)| {
            let mut ground = p.clone();
            ground.set_on_ground(true);
            get_mine_progress(st.to_trait(), &held, f, &ground, a, e)
        })
        .unwrap_or(0.0);
    if standing { per } else { per * 0.2 }
}

/// Over the art (x and z), within margin blocks of it.
fn inside_art(s: &Site, p: Vec3, margin: f64) -> bool {
    let (x0, z0) = (s.art.x as f64 - margin, s.art.z as f64 - margin);
    let (x1, z1) = (s.art.x as f64 + SIDE as f64 + margin, s.art.z as f64 + SIDE as f64 + margin);
    p.x >= x0 && p.x <= x1 && p.z >= z0 && p.z <= z1
}

/// Down among the carpets, up to over the lip's height first - a trip out of the art from there
/// runs into the lip, and a contained trip gives up rather than climb.
async fn rise(bot: &Client, s: &Site) {
    let Ok(p) = bot.position() else { return };
    let top = s.art.y as f64 + HOVER;
    if p.y < top - 0.1 && inside_art(s, p, 1.0) {
        movement::stop(bot);
        if let Err(e) = movement::fly_to(bot, Vec3::new(p.x, top, p.z), "rise").await {
            warn!(target: "mapart", "could not rise over the lip: {e}");
        }
    }
}

// ------------------------------------------------------------------ the map

async fn make_map(ctx: &Shared, bot: &Client, s: &Site, job: &Job) -> Result<(), Stop> {
    // What it takes, before it flies out over the art.
    take_one_kind(ctx, bot, s, site::MAPS, "map", "empty maps").await?;
    take_one_kind(ctx, bot, s, site::GLASS, "glass_pane", "glass panes").await?;

    phase(ctx, "mapping the art");
    let over = s.middle(3.0);
    movement::fly_to(bot, over, "map").await.map_err(|e| wait_for(format!("could not get over the art to map it: {e}")))?;
    let Some((slot, _)) = bag_slot(bot, "map") else { return Err(wait_for("the empty map is gone from the bags")) };
    let had: Vec<usize> = filled_slots(bot);
    let Some(_) = inv::hold(bot, slot, &[]) else { return Err(wait_for("could not take the empty map in hand")) };
    tokio::time::sleep(Duration::from_millis(300)).await;
    act::use_item_main(bot);
    let new = {
        let t0 = Instant::now();
        loop {
            if let Some(n) = filled_slots(bot).into_iter().find(|i| !had.contains(i)) {
                break Some(n);
            }
            if t0.elapsed() > Duration::from_secs(4) {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let Some(new) = new else { return Err(wait_for("used an empty map and got no map back")) };
    inv::hold(bot, new, &[]);
    wait(ctx, bot, MAP_HOLD).await;
    if interrupted(ctx, bot) {
        return Err(Stop::Interrupted);
    }

    finish_map(ctx, bot, s, job, false).await
}

/// The map made: locked, named, stored. `resumed`: made before a restart - it may be locked or
/// named already, and a step it has had is passed over.
async fn finish_map(ctx: &Shared, bot: &Client, s: &Site, job: &Job, resumed: bool) -> Result<(), Stop> {
    let name_wanted = job.anvil_name();
    let named = inv::bags(bot)
        .iter()
        .any(|(_, st)| inv::stack_id(st) == "filled_map" && st.get_component::<CustomName>().is_some_and(|n| n.name.to_string() == name_wanted));
    if !named {
        if resumed {
            take_one_kind(ctx, bot, s, site::GLASS, "glass_pane", "glass panes").await?;
        }
        lock(ctx, bot, s, resumed).await?;
        get_levels(ctx, bot, s).await?;
        name(ctx, bot, s, &name_wanted).await?;
    }
    phase(ctx, "storing the map");
    store_maps(bot, s).await
}

fn filled_slots(bot: &Client) -> Vec<usize> {
    inv::bags(bot).into_iter().filter(|(_, st)| inv::stack_id(st) == "filled_map").map(|(i, _)| i).collect()
}

/// A stack of `id` from the chest signed `word`, unless some is carried.
async fn take_one_kind(ctx: &Shared, bot: &Client, s: &Site, word: &str, id: &str, what: &str) -> Result<(), Stop> {
    if inv::count_id(bot, id) > 0 {
        return Ok(());
    }
    rise(bot, s).await;
    let mut tried = Vec::new();
    loop {
        let Some(c) = plan::nearest_chest(s.chests(word), &tried, bot.position().unwrap_or_default()) else {
            return Err(wait_for(if s.chests(word).is_empty() {
                format!("there is no chest signed {word} for the {what}")
            } else {
                format!("the {word} chest has no {what}")
            }));
        };
        phase(ctx, &format!("fetching {what}"));
        let _ = approach(bot, s, c).await;
        match chests::take(bot, c, |st| inv::stack_id(st) == id, |st| st.count() as i64, 1).await {
            Ok(n) if n > 0 => return Ok(()),
            Ok(_) => tried.push(c),
            Err(e) => {
                warn!(target: "mapart", "the {word} chest at {c}: {e}");
                tried.push(c);
            }
        }
    }
}

/// Within reach of a chest or block of the base, the way round when the straight way is blocked:
/// up three blocks and again, then from straight over it (6 Nov: from the cartography table,
/// every spot by the XP chest a few blocks off was "no clear way through", and the contained
/// trip gives up rather than go round).
async fn approach(bot: &Client, s: &Site, pos: BlockPos) -> Result<(), String> {
    rise(bot, s).await;
    let mut last = match chests::fly_within_reach(bot, pos).await {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    // By way of the art, the open middle of the site: out to the nearest of it (the way in to
    // where the bot is), along it to the nearest the target, and in (6 Nov: the XP chest, walled
    // off from the cartography table, was reached from the art side every time).
    let over_art = |x: f64, z: f64| {
        let (x0, z0, side) = (s.art.x as f64, s.art.z as f64, SIDE as f64);
        Vec3::new(x.clamp(x0 + 0.5, x0 + side - 0.5), s.art.y as f64 + HOVER, z.clamp(z0 + 0.5, z0 + side - 0.5))
    };
    if let Ok(p) = bot.position() {
        let _ = movement::fly_to(bot, over_art(p.x, p.z), "round").await;
        let _ = movement::fly_to(bot, over_art(pos.x as f64 + 0.5, pos.z as f64 + 0.5), "round").await;
        match chests::fly_within_reach(bot, pos).await {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    if let Ok(p) = bot.position() {
        let _ = movement::fly_to(bot, p + Vec3::new(0.0, 3.0, 0.0), "round").await;
        match chests::fly_within_reach(bot, pos).await {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    let over = Vec3::new(pos.x as f64 + 0.5, (pos.y.max(s.art.y) + 4) as f64, pos.z as f64 + 0.5);
    let _ = movement::fly_to(bot, over, "round").await;
    chests::fly_within_reach(bot, pos).await.map_err(|_| last)
}

/// Open a block's screen from within reach.
async fn open_at(bot: &Client, s: &Site, pos: BlockPos, what: &str) -> Result<ContainerHandle, Stop> {
    approach(bot, s, pos).await.map_err(|e| wait_for(format!("could not reach the {what}: {e}")))?;
    match bot.open_container_at(pos).await {
        Ok(Some(c)) => Ok(c),
        _ => Err(wait_for(format!("the {what} did not open"))),
    }
}

/// The open screen's slot (by its index) holding `id` in the player's part.
fn menu_slot_of(c: &ContainerHandle, id: &str) -> Option<usize> {
    let m = c.menu().ok().flatten()?;
    let slots = m.slots();
    m.player_slots_range().find(|i| inv::stack_id(&slots[*i]) == id)
}

fn menu_slot_empty(c: &ContainerHandle, i: usize) -> bool {
    c.menu().ok().flatten().is_none_or(|m| m.slots().get(i).is_none_or(|s| s.is_empty()))
}

/// Wait up to `ms` for `f`.
async fn until(ms: u64, f: impl Fn() -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    f()
}

/// A round trip to the server and a little more.
async fn settle(bot: &Client) {
    let ping = bot.tab_list().ok().and_then(|t| t.get(&bot.uuid()).map(|i| i.latency)).unwrap_or(150).clamp(0, 1000) as u64;
    tokio::time::sleep(Duration::from_millis(2 * ping + 150)).await;
}

/// The map and a glass pane in the cartography table, the locked map taken out.
async fn lock(ctx: &Shared, bot: &Client, s: &Site, resumed: bool) -> Result<(), Stop> {
    let Some(table) = s.table else { return Err(wait_for("there is no cartography table in the site")) };
    phase(ctx, "locking the map");
    let c = open_at(bot, s, table, "cartography table").await?;
    let result = 2usize;
    let (Some(map), Some(glass)) = (menu_slot_of(&c, "filled_map"), menu_slot_of(&c, "glass_pane")) else {
        c.close();
        return Err(wait_for("the map or the glass pane is not in the bags"));
    };
    c.shift_click(map);
    tokio::time::sleep(TICK).await;
    c.shift_click(glass);
    let ready = until(3000, || !menu_slot_empty(&c, result)).await;
    if !ready {
        c.close();
        tokio::time::sleep(Duration::from_millis(300)).await;
        // A map locked already gets nothing from the table: one made before a restart may be.
        if resumed {
            info!(target: "mapart", "the table gave nothing for the map from before - taken as locked already");
            return Ok(());
        }
        return Err(wait_for("the cartography table gave no locked map"));
    }
    c.shift_click(result);
    until(3000, || menu_slot_empty(&c, result) && menu_slot_empty(&c, 0)).await;
    settle(bot).await;
    c.close();
    tokio::time::sleep(Duration::from_millis(300)).await;
    if inv::count_id(bot, "filled_map") == 0 {
        return Err(wait_for("the locked map did not come back to the bags"));
    }
    info!(target: "mapart", "map locked");
    Ok(())
}

fn level(bot: &Client) -> u32 {
    bot.query_self::<Option<&Experience>, _>(|e| e.map(|e| e.level)).ok().flatten().unwrap_or(0)
}

/// At least one XP level for the anvil: bottles o' enchanting thrown at the feet until XP_BANK.
async fn get_levels(ctx: &Shared, bot: &Client, s: &Site) -> Result<(), Stop> {
    if level(bot) >= 1 {
        return Ok(());
    }
    if let Some(anvil) = s.anvil {
        let _ = approach(bot, s, anvil).await;
    }
    for _ in 0..16 {
        if interrupted(ctx, bot) {
            return Err(Stop::Interrupted);
        }
        if level(bot) >= XP_BANK {
            break;
        }
        take_one_kind(ctx, bot, s, site::XP, "experience_bottle", "bottles o' enchanting").await?;
        let Some((slot, _)) = bag_slot(bot, "experience_bottle") else { continue };
        phase(ctx, "throwing XP bottles for the anvil");
        if inv::hold(bot, slot, &[]).is_none() {
            continue;
        }
        let yaw = bot.query_self::<&LookDirection, _>(|l| l.y_rot()).unwrap_or(0.0);
        movement::look_now(bot, yaw, 90.0);
        for _ in 0..3 {
            act::use_item_main(bot);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    info!(target: "mapart", "XP level {}", level(bot));
    if level(bot) == 0 {
        return Err(wait_for("threw XP bottles and got no level for the anvil"));
    }
    Ok(())
}

/// The locked map named at the anvil.
async fn name(ctx: &Shared, bot: &Client, s: &Site, name: &str) -> Result<(), Stop> {
    let Some(anvil) = s.anvil else { return Err(wait_for("there is no anvil in the site")) };
    phase(ctx, &format!("naming the map {name}"));
    let c = open_at(bot, s, anvil, "anvil").await?;
    let Some(map) = menu_slot_of(&c, "filled_map") else {
        c.close();
        return Err(wait_for("the map is not in the bags"));
    };
    c.shift_click(map);
    settle(bot).await;
    act::write(bot, ServerboundRenameItem { name: name.to_string() }.into_variant());
    let result = 2usize;
    if !until(3000, || !menu_slot_empty(&c, result)).await {
        c.close();
        tokio::time::sleep(Duration::from_millis(300)).await;
        return Err(wait_for("the anvil gave no named map"));
    }
    c.shift_click(result);
    until(3000, || menu_slot_empty(&c, result) && menu_slot_empty(&c, 0)).await;
    settle(bot).await;
    c.close();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let named = inv::bags(bot)
        .iter()
        .any(|(_, st)| inv::stack_id(st) == "filled_map" && st.get_component::<CustomName>().is_some_and(|n| n.name.to_string() == name));
    if !named {
        return Err(wait_for(format!("the map did not come back from the anvil named {name}")));
    }
    info!(target: "mapart", "map named {name}");
    Ok(())
}

/// Every map carried into the nearest finished chest that takes it.
async fn store_maps(bot: &Client, s: &Site) -> Result<(), Stop> {
    rise(bot, s).await;
    let mut full = Vec::new();
    while inv::count_id(bot, "filled_map") > 0 {
        let Some(c) = plan::nearest_chest(s.chests(site::FINISHED), &full, bot.position().unwrap_or_default()) else {
            return Err(wait_for(if s.chests(site::FINISHED).is_empty() { "there is no chest signed finished" } else { "the finished chest is full" }));
        };
        let _ = approach(bot, s, c).await;
        match chests::store(bot, c, |st| inv::stack_id(st) == "filled_map", 9).await {
            Ok(n) if n > 0 => {}
            Ok(_) => full.push(c),
            Err(e) => {
                warn!(target: "mapart", "the finished chest at {c}: {e}");
                full.push(c);
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ washing

/// What is on the carpet layer of the art: (carpets, water); None while part of it is not loaded.
fn on_platform(bot: &Client, s: &Site) -> Option<(usize, usize)> {
    let world = bot.world().ok()?;
    let w = world.read();
    let (mut carpets, mut water) = (0, 0);
    for z in 0..SIDE {
        for x in 0..SIDE {
            let k = BlockKind::from(w.get_block_state(s.at(x, z))?);
            if art::is_carpet(k) {
                carpets += 1;
            } else if k == BlockKind::Water {
                water += 1;
            }
        }
    }
    Some((carpets, water))
}

async fn press(bot: &Client, s: &Site, button: BlockPos) -> Result<(), Stop> {
    approach(bot, s, button).await.map_err(|e| wait_for(format!("could not reach the water button: {e}")))?;
    act::use_main(bot, button, Dir::Up, Vec3::new(button.x as f64 + 0.5, button.y as f64 + 0.5, button.z as f64 + 0.5));
    info!(target: "mapart", "pressed the water button at {}", fmt_pos(button));
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// Press, watch the carpets go, press again, watch the water go.
async fn wash(ctx: &Shared, bot: &Client, s: &Site) -> Result<(), Stop> {
    let Some(button) = s.button else { return Err(wait_for("there is no water button - put a sign reading CLEAR next to it")) };
    phase(ctx, "washing the carpets off");
    rise(bot, s).await;
    press(bot, s, button).await?;
    // Timed from the press. Watched from over the middle, high enough that nothing floating is
    // picked up; back at the button for the second press as WASH_MIN runs out.
    let t0 = Instant::now();
    let over = s.middle(4.0);
    let _ = movement::fly_to(bot, over, "wash").await;
    wait(ctx, bot, WASH_MIN.saturating_sub(t0.elapsed())).await;
    while t0.elapsed() < WASH_MAX && !interrupted(ctx, bot) {
        if on_platform(bot, s).is_some_and(|(c, _)| c == 0) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if interrupted(ctx, bot) {
        return Err(Stop::Interrupted);
    }
    let left = on_platform(bot, s).map_or(0, |(c, _)| c);
    phase(ctx, "turning the water off");
    press(bot, s, button).await?;
    let _ = movement::fly_to(bot, over, "wash").await;
    let t1 = Instant::now();
    while t1.elapsed() < DRAIN_MAX && !interrupted(ctx, bot) {
        if on_platform(bot, s).is_some_and(|(_, w)| w == 0) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if interrupted(ctx, bot) {
        return Err(Stop::Interrupted);
    }
    let water = on_platform(bot, s).map_or(0, |(_, w)| w);
    info!(target: "mapart", "washed: {left} carpet(s) left, {water} water on the carpet layer");
    if left > 0 {
        warn!(target: "mapart", "The wash left {left} carpet(s) on the platform.");
    }
    if water > 0 {
        return Err(wait_for(format!("the water has not drained from the platform ({water} blocks of it after {} s)", DRAIN_MAX.as_secs())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The site as on 6 Nov: chests down the west and east sides, the bed and button by the north
    /// edge's middle. Junk comes down well clear of all of them, never by a chest.
    #[test]
    fn junk_goes_where_nothing_is_used() {
        let art = BlockPos::new(64, 123, -64);
        let mut s = Site { art, ..Default::default() };
        let rows: Vec<BlockPos> = (-48..=61).flat_map(|z| [BlockPos::new(61, 126, z), BlockPos::new(194, 126, z)]).collect();
        s.carpets.insert(BlockKind::LightBlueCarpet, rows.clone());
        s.bed = Some(BlockPos::new(130, 124, -70));
        s.button = Some(BlockPos::new(126, 124, -70));
        // From by the east chest it used to throw it out at.
        let (lip, ox, oz) = toss_spot(&s, Vec3::new(193.5, 125.0, -37.5));
        assert_eq!(ox.abs() + oz.abs(), 1.0, "straight out of a side");
        let down = Vec3::new(lip.x + ox * TOSS_REACH, 0.0, lip.z + oz * TOSS_REACH);
        let nearest = rows.iter().chain([s.bed.unwrap(), s.button.unwrap()].iter()).map(|p| (p.x as f64 + 0.5 - down.x).hypot(p.z as f64 + 0.5 - down.z)).fold(f64::MAX, f64::min);
        assert!(nearest >= TOSS_CLEAR, "comes down {nearest:.1} from a chest");
        assert_eq!(lip.y, art.y as f64 + TOSS_UP);
    }
}
