//! Swarm combat. With Defend on, fight hostile mobs and attackers - kill-aura's
//! mob list, and any player who hits a swarm bot or one of your own accounts; friends and your
//! own accounts never. An attack target (the controller's ATTACK_TARGET or the panel) is
//! chased and hit wherever a swarm bot can see it.
//!
//! Attackers come from the server's damage packets (every client in view gets them, with the cause
//! entity), caught by a system for the whole swarm. Kill-aura's mob list follows the controller
//! instance's own (its melonhack/standalone.json), read every few seconds.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use azalea::{
    Client, Vec3,
    core::entity_id::MinecraftEntityId,
    ecs::prelude::*,
    entity::{EntityUuid, LocalEntity, indexing::EntityIdIndex, metadata::Player},
    packet::game::ReceiveGamePacketEvent,
    protocol::packets::game::ClientboundGamePacket,
};
use serde_json::Value;
use tracing::{debug, info};

use super::{BotLink, SwarmCtx};
use crate::{inv, movement};

/// An attacker stays one for this long after its last hit.
const ATTACKER_FOR: Duration = Duration::from_secs(60);
/// How far a defending bot goes after a mob or an attacker.
const DEFEND_RADIUS: f64 = 16.0;
/// Melee reach, eye to the target's middle.
const HIT_REACH: f64 = 3.2;
/// A bot fights back at what hit it for this long after the hit.
const RETALIATE_FOR: Duration = Duration::from_secs(15);

/// Who hit one of ours: uuid -> (the hit, who was hit).
#[derive(Default)]
pub struct Attackers(pub HashMap<uuid::Uuid, (Instant, String)>);

/// The swarm's combat orders.
#[derive(Clone, Debug, Default)]
pub struct Orders {
    pub defend: bool,
    pub defend_from: &'static str,
    pub target: Option<String>,
    pub target_from: &'static str,
}

/// Damage packets: a player that hits a swarm bot or one of your own accounts is an attacker.
pub fn record_attackers(
    mut events: MessageReader<ReceiveGamePacketEvent>,
    indexes: Query<&EntityIdIndex, With<LocalEntity>>,
    entities: Query<(Option<&EntityUuid>, Has<Player>, Has<LocalEntity>)>,
) {
    let Some(ctx) = super::ctx() else { return };
    for ev in events.read() {
        let ClientboundGamePacket::DamageEvent(p) = ev.packet.as_ref() else { continue };
        let Some(cause) = p.source_cause_id.0 else { continue };
        let Ok(index) = indexes.get(ev.entity) else { continue };
        let resolve = |id: MinecraftEntityId| index.get_by_minecraft_entity(id);
        let (Some(victim), Some(attacker)) = (resolve(p.entity_id), resolve(cause)) else { continue };
        let Ok((a_uuid, a_player, a_local)) = entities.get(attacker) else { continue };
        if a_local {
            continue;
        }
        // A swarm bot hit by anything (mob or player): it fights back (fight_back).
        if entities.get(victim).is_ok_and(|(_, _, local)| local) {
            ctx.hit_by.lock().insert(victim, (attacker, Instant::now()));
        }
        let (true, Some(a_uuid)) = (a_player, a_uuid) else { continue };
        let Ok((v_uuid, _, v_local)) = entities.get(victim) else { continue };
        // One of ours: a swarm bot, or a player whose name is one of your own accounts (checked
        // by uuid against the tab names in the bot tick - here only the uuid is known).
        let victim_name = if v_local { "a swarm bot".to_string() } else { v_uuid.map(|u| u.to_string()).unwrap_or_default() };
        let mut a = ctx.attackers.lock();
        a.0.insert(**a_uuid, (Instant::now(), victim_name));
    }
}

/// Your own accounts, friends and swarm bots, lower case: never attacked.
pub fn protected(ctx: &SwarmCtx) -> std::collections::HashSet<String> {
    let mut set = crate::config::own_accounts();
    set.extend(ctx.cfg.read().friends.iter().map(|f| f.to_lowercase()));
    set.extend(ctx.bots.read().keys().map(|k| k.to_lowercase()));
    set
}

/// A player's name by uuid, from this bot's tab list.
fn name_of(bot: &Client, uuid: &uuid::Uuid) -> Option<String> {
    bot.tab_list().ok()?.get(uuid).map(|i| i.profile.name.to_string())
}

/// Health at or under this while something is hitting it: fly straight up out of reach.
const RETREAT_AT: f32 = 8.0;
/// ...and stay up there until healed to this.
const RETREAT_UNTIL: f32 = 16.0;
/// How high above where it was.
const RETREAT_UP: f64 = 24.0;

/// Low on health and under attack: up out of reach until healed (bots fly; zombies and slimes
/// cannot follow, skeletons mostly out of range). True while retreating - it has the movement,
/// over errands and work (16 Oct test: a bot alone at night died twice, the second time going
/// straight back for its items).
pub fn retreat(bot: &Client, ctx: &SwarmCtx, link: &BotLink) -> bool {
    let Ok(health) = bot.health() else { return false };
    let Ok(pos) = bot.position() else { return false };
    let mut r = link.retreat.lock();
    if r.is_none() && health > 0.0 && health <= RETREAT_AT {
        let hit = ctx.hit_by.lock().get(&bot.entity).is_some_and(|(_, at)| at.elapsed() < RETALIATE_FOR);
        if hit {
            let spot = pos + Vec3::new(0.0, RETREAT_UP, 0.0);
            info!(target: "combat", "health {health:.0} under attack - retreating up to y {:.0} to heal", spot.y);
            *r = Some(spot);
        }
    }
    let Some(spot) = *r else { return false };
    if health >= RETREAT_UNTIL || health <= 0.0 {
        info!(target: "combat", "healed to {health:.0} - back to work");
        *r = None;
        return false;
    }
    drop(r);
    movement::steer(bot, spot, "retreat", 0.5);
    link.set_combat_status(format!("retreating to heal (health {health:.0})"));
    true
}

/// One tick of combat for this bot: true while it is chasing something (it has the movement).
pub fn tick(bot: &Client, ctx: &SwarmCtx, link: &BotLink, ticks: u64) -> bool {
    let orders = link.combat_orders(ctx);
    // Whatever hit this bot lately: fought back, Defend or not (16 Oct test: three bots nuking
    // with Defend off were killed by mobs - one three times - and dropped their pickaxes).
    let hit_by = if ctx.cfg.read().fight_back {
        let mut h = ctx.hit_by.lock();
        h.retain(|_, (_, at)| at.elapsed() < RETALIATE_FOR);
        h.get(&bot.entity).map(|(e, _)| *e)
    } else {
        None
    };
    if !orders.defend && orders.target.is_none() && hit_by.is_none() {
        return false;
    }
    let Ok(pos) = bot.position() else { return false };
    let eye = pos + Vec3::new(0.0, 1.62, 0.0);
    let safe = protected(ctx);
    let tab = bot.tab_list().unwrap_or_default();
    let attackers: Vec<uuid::Uuid> = {
        let mut a = ctx.attackers.lock();
        a.0.retain(|_, (at, _)| at.elapsed() < ATTACKER_FOR);
        // A hit on a non-swarm player counts only if that player is one of your own accounts.
        a.0.iter()
            .filter(|(_, (_, victim))| {
                victim == "a swarm bot"
                    || uuid::Uuid::parse_str(victim).ok().and_then(|u| tab.get(&u).map(|i| i.profile.name.to_lowercase())).is_some_and(|n| safe.contains(&n))
            })
            .map(|(u, _)| *u)
            .collect()
    };
    let target_name = orders.target.as_ref().map(|t| t.to_lowercase()).filter(|t| !safe.contains(t));
    let mob_kinds: std::collections::HashSet<String> = if orders.defend { link.killaura_targets(ctx) } else { Default::default() };

    // The best thing to go for: the attack target, then attackers, then hostile mobs.
    let mut best: Option<(u8, f64, crate::entities::Near, String)> = None;
    for (d, e) in crate::entities::near(bot) {
        if e.dead {
            continue;
        }
        let (rank, what) = if e.player {
            let Some(u) = e.uuid else { continue };
            let Some(name) = name_of(bot, &u) else { continue };
            let lower = name.to_lowercase();
            if safe.contains(&lower) {
                continue;
            }
            if target_name.as_deref() == Some(lower.as_str()) {
                (0, name)
            } else if hit_by == Some(e.entity) && d <= DEFEND_RADIUS {
                (1, format!("{name} (hit this bot)"))
            } else if orders.defend && attackers.contains(&u) && d <= DEFEND_RADIUS {
                (1, format!("{name} (attacked us)"))
            } else {
                continue;
            }
        } else if hit_by == Some(e.entity) && d <= DEFEND_RADIUS {
            (1, format!("{} (hit this bot)", e.kind))
        } else if orders.defend && d <= DEFEND_RADIUS && mob_kinds.contains(&e.kind.to_string()) {
            (2, e.kind.to_string())
        } else {
            continue;
        };
        if best.as_ref().is_none_or(|(r, bd, ..)| rank < *r || (rank == *r && d < *bd)) {
            best = Some((rank, d, e, what));
        }
    }
    // The target out of this bot's sight: go where another swarm bot sees it.
    if best.is_none()
        && let Some(t) = &target_name
        && let Some(s) = ctx.sightings.lock().get(t).filter(|s| s.at.elapsed() < Duration::from_secs(5)).cloned()
    {
        movement::steer(bot, s.pos + Vec3::new(0.0, 0.5, 0.0), "combat", 2.0);
        link.set_combat_status(format!("going to {} (seen by {})", s.name, s.by));
        return true;
    }
    let Some((_, _, e, what)) = best else {
        link.set_combat_status(String::new());
        return false;
    };
    let middle = e.pos + Vec3::new(0.0, 0.9, 0.0);
    let d = eye.distance_to(middle);
    // A weapon in hand: the best sword, else the best axe.
    if ticks.is_multiple_of(10) {
        hold_weapon(bot, ctx, link, ticks);
    }
    super::eclink::touch_held(bot, link, ticks);
    if d <= HIT_REACH {
        if !bot.has_attack_cooldown() {
            debug!(target: "combat", "hit {what} ({d:.1} away)");
            bot.attack(e.entity);
        }
        link.set_combat_status(format!("fighting {what}"));
    } else {
        // Close in to just inside reach, from where we are.
        let away = pos - e.pos;
        let dir = if away.length() > 0.01 { away.normalize() } else { Vec3::new(1.0, 0.0, 0.0) };
        let spot = e.pos + Vec3::new(dir.x * 2.0, 0.0, dir.z * 2.0);
        movement::steer(bot, spot, "combat", 0.8);
        link.set_combat_status(format!("chasing {what} ({d:.0} away)"));
    }
    true
}

/// A weapon's rank: the best sword, else the best axe (0: not a weapon).
fn weapon_rank(id: &str) -> i32 {
    let tier = if id.starts_with("netherite") {
        6
    } else if id.starts_with("diamond") {
        5
    } else if id.starts_with("iron") {
        4
    } else if id.starts_with("stone") {
        3
    } else if id.starts_with("golden") {
        2
    } else {
        1
    };
    if id.ends_with("_sword") {
        100 + tier
    } else if id.ends_with("_axe") {
        50 + tier
    } else {
        0
    }
}

/// The best weapon in hand: from the bags, or borrowed from the linked ender chest (eclink.rs)
/// when it has a better one.
fn hold_weapon(bot: &Client, ctx: &SwarmCtx, link: &BotLink, ticks: u64) {
    let held = weapon_rank(inv::stack_id(&bot.get_held_item().unwrap_or_default()));
    let carried = inv::bags(bot).into_iter().map(|(slot, st)| (weapon_rank(inv::stack_id(&st)), slot)).filter(|(r, _)| *r > 0).max();
    let (ec_on, panic) = {
        let c = ctx.cfg.read();
        (c.ec_link, c.ec_panic_health)
    };
    if ec_on
        && let Some((r, ec_slot)) = super::eclink::contents(bot, link).into_iter().map(|(slot, st)| (weapon_rank(inv::stack_id(&st)), slot)).filter(|(r, _)| *r > 0).max()
        && r > held
        && r > carried.map_or(0, |c| c.0)
        && super::eclink::borrow(bot, link, ec_slot, panic, ticks)
    {
        return;
    }
    let Some((r, slot)) = carried else { return };
    if held < r {
        inv::hold(bot, slot, &super::eclink::loan_hotbars(link));
    }
}

// ------------------------------------------------------------------ the controller instance

/// The Java instance running the controller mod as Controller: its folder and what the swarm copies from it.
#[derive(Clone, Debug, Default)]
pub struct ControllerInstance {
    pub name: String,
    pub file: PathBuf,
    pub modified: Option<SystemTime>,
    /// kill-aura's targets there.
    pub killaura_targets: Option<Vec<String>>,
    /// fast-nuker's settings there (for "use the controller's box").
    pub nuker: Option<Value>,
    /// printer's settings there (the swarm printer's defaults).
    pub printer: Option<Value>,
}

/// The folders holding Java instances (MultiMC and Prism).
fn instance_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
    vec![
        home.join("Desktop").join("MultiMC").join("MultiMC").join("instances"),
        home.join("AppData").join("Roaming").join("PrismLauncher").join("instances"),
    ]
}

/// Every few seconds: find the instance with the controller mod on as Controller (the most recently saved if
/// several) and read what the swarm copies from it.
pub async fn watch_controller_instance(ctx: Arc<SwarmCtx>) {
    loop {
        let found = tokio::task::spawn_blocking(find_controller_instance).await.ok().flatten();
        let changed = {
            let mut cur = ctx.controller_instance.lock();
            let same = matches!((&*cur, &found), (Some(a), Some(b)) if a.file == b.file && a.modified == b.modified)
                || (cur.is_none() && found.is_none());
            if !same {
                if let Some(b) = &found {
                    info!(target: "controller", "controller instance: {} ({} kill-aura targets)", b.name, b.killaura_targets.as_ref().map_or(0, |t| t.len()));
                } else {
                    info!(target: "controller", "no instance has the controller mod on as Controller any more");
                }
                *cur = found;
            }
            !same
        };
        if changed {
            ctx.modules_changed();
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn find_controller_instance() -> Option<ControllerInstance> {
    let mut best: Option<ControllerInstance> = None;
    for root in instance_roots() {
        let Ok(dir) = fs::read_dir(&root) else { continue };
        for inst in dir.flatten() {
            let file = inst.path().join(".minecraft").join("melonhack").join("standalone.json");
            let Ok(meta) = fs::metadata(&file) else { continue };
            let modified = meta.modified().ok();
            let Ok(text) = fs::read_to_string(&file) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            // The controller mod, by its id in the Java client's settings.
            let fa = &v["frac-ai"];
            if fa["enabled"].as_bool() != Some(true) || fa["settings"]["role"].as_str() != Some("Controller") {
                continue;
            }
            if best.as_ref().is_some_and(|b| b.modified >= modified) {
                continue;
            }
            let targets = v["kill-aura"]["settings"]["targets"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect());
            best = Some(ControllerInstance {
                name: inst.file_name().to_string_lossy().to_string(),
                file,
                modified,
                killaura_targets: targets,
                nuker: Some(v["fast-nuker"].clone()).filter(|n| n.is_object()),
                printer: Some(v["printer"].clone()).filter(|n| n.is_object()),
            });
        }
    }
    best
}
