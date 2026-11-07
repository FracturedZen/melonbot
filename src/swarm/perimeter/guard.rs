//! Keeping a perimeter bot alive (5 Nov: a bot was shot dead by a pillager at the dig - 20 to 0
//! health in 3 s - and its tools were lost where it died, far from its respawn).
//! - **Auto-log**: hurt by anything but its own falls and clipping, down to LOG_HEALTH or LOG_BURST
//!   lost within BURST_SECS: it logs out and stays out, as the panel's Stop does - the reason on
//!   its row, in red in the feed, and on Discord.
//! - **The totem** is in the offhand from the start and is never moved (the user: "dont swap it"):
//!   it is there in case the log-out is too slow. Fill is placed from the hotbar while it is.
//! - **Armor** from the bags onto the empty armor slots.
//! - **The weapon** (a mace, else a sword) kept in the hotbar for kill-aura, never thrown out.

use std::time::Instant;

use azalea::{Client, inventory::operations::QuickMoveClick};
use tracing::{info, warn};

use crate::{
    bot::{Hurt, Shared},
    inv,
    modules::killaura,
    swarm::{BotLink, SwarmCtx},
};

/// Hurt down to this health (or under) by something else: log out.
pub const LOG_HEALTH: f32 = 12.0;
/// Or this much lost within BURST_SECS.
pub const LOG_BURST: f32 = 6.0;
const BURST_SECS: f64 = 5.0;
/// The low-health rule counts a hurt this recent.
const LOW_SECS: f64 = 10.0;
/// Its own doing: the mover clips through floors and bumps walls (the server bills a little).
const OWN_DOING: [&str; 3] = ["fall", "in_wall", "fly_into_wall"];
pub const TOTEM: &str = "totem_of_undying";
/// A second log-out this soon after one is the same one.
const LOGGED_FOR_SECS: u64 = 60;

/// The player menu's armor slots and what goes in each.
const ARMOR: [(usize, &str); 4] = [(5, "_helmet"), (6, "_chestplate"), (7, "_leggings"), (8, "_boots")];

/// Hurt by something other than its own moves (a hurt with no known cause counts).
fn from_outside(h: &Hurt) -> bool {
    h.from.is_empty() || h.from.iter().any(|f| !OWN_DOING.contains(&f.as_str()))
}

fn causes(hurts: &[&Hurt]) -> String {
    let mut c: Vec<&str> = hurts.iter().flat_map(|h| h.from.iter().map(String::as_str)).filter(|f| !OWN_DOING.contains(f)).collect();
    c.sort();
    c.dedup();
    if c.is_empty() { "something unknown".into() } else { c.join(", ") }
}

/// Why it should log out now, if it should.
pub fn log_out_why(hurts: &[Hurt], health: f32, now: Instant) -> Option<String> {
    let since = |h: &Hurt| now.saturating_duration_since(h.at).as_secs_f64();
    let burst: Vec<&Hurt> = hurts.iter().filter(|h| from_outside(h) && since(h) <= BURST_SECS).collect();
    let lost: f32 = burst.iter().map(|h| h.lost).sum();
    if lost >= LOG_BURST {
        return Some(format!("lost {lost:.0} health in {BURST_SECS:.0} s to {} - {health:.0} left", causes(&burst)));
    }
    let recent: Vec<&Hurt> = hurts.iter().filter(|h| from_outside(h) && since(h) <= LOW_SECS).collect();
    if health <= LOG_HEALTH && !recent.is_empty() {
        return Some(format!("down to {health:.0} health from {}", causes(&recent)));
    }
    None
}

/// Every tick: the log-out first. True when it is logging out (nothing else runs).
pub fn tick(bot: &Client, s: &Shared, ctx: &SwarmCtx, link: &BotLink, ticks: u64) -> bool {
    let Ok(health) = bot.health() else { return false };
    if health <= 0.0 {
        return false;
    }
    // Logged out a moment ago: still on its way out - unless it has been started again since.
    let joined = *link.joined_at.lock();
    {
        let mut d = link.perimeter.lock();
        if let Some(t) = d.logged_out_at {
            if joined.is_some_and(|j| j > t) || t.elapsed().as_secs() >= LOGGED_FOR_SECS {
                d.logged_out_at = None;
            } else {
                return true;
            }
        }
    }
    let hurts = s.hurts.lock().clone();
    if let Some(why) = log_out_why(&hurts, health, Instant::now()) {
        log_out(bot, s, ctx, link, &why);
        return true;
    }
    if ticks.is_multiple_of(40) && inv::no_container_open(bot) {
        wear_armor(bot);
        weapon_in_hotbar(bot);
    }
    false
}

fn log_out(bot: &Client, s: &Shared, ctx: &SwarmCtx, link: &BotLink, why: &str) {
    link.perimeter.lock().logged_out_at = Some(Instant::now());
    // Its hurts are this time's: started again, it begins with none.
    s.hurts.lock().clear();
    let at = bot.position().map(|p| format!("{:.0} {:.0} {:.0}", p.x, p.y, p.z)).unwrap_or_else(|_| "?".into());
    let totem = inv::menu(bot).and_then(|m| m.slots().get(45).cloned()).is_some_and(|o| inv::stack_id(&o) == TOTEM);
    let msg = format!("AUTO-LOGGED OUT: {why}, at {at}{} - it stays out until started again", if totem { "" } else { " (no totem in the offhand)" });
    warn!(target: "perimeter", "{msg}");
    *link.last_error.lock() = msg.clone();
    ctx.feed("alert", &format!("{}: {msg}", link.name));
    s.discord.send(&msg);
    ctx.stop(&link.name);
}

/// The first armor piece in the bags for an empty armor slot, put on by a shift-click.
fn wear_armor(bot: &Client) {
    let Some(menu) = inv::menu(bot) else { return };
    let slots = menu.slots();
    for (slot, kind) in ARMOR {
        if !slots.get(slot).is_some_and(|s| s.is_empty()) {
            continue;
        }
        let Some((i, piece)) = inv::bags(bot).into_iter().find(|(_, s)| inv::stack_id(s).ends_with(kind)) else { continue };
        if let Ok(h) = bot.get_inventory() {
            info!(target: "perimeter", "putting on {}", inv::stack_id(&piece));
            h.click(QuickMoveClick::Left { slot: i as u16 });
        }
        return;
    }
}

/// The weapon kill-aura uses out of the backpack and into the hotbar - an empty slot, else one
/// with no tool in it - never the held slot (the dig is using it).
fn weapon_in_hotbar(bot: &Client) {
    let Some((slot, id)) = killaura::best_weapon(bot) else { return };
    if inv::hotbar_index(slot).is_some() {
        return;
    }
    let Some(menu) = inv::menu(bot) else { return };
    let slots = menu.slots();
    let held = bot.selected_hotbar_slot().ok();
    let pick = (0..9u8)
        .filter(|h| Some(*h) != held)
        .filter_map(|h| {
            let st = &slots[36 + h as usize];
            let rank = if st.is_empty() { 0 } else if !inv::is_tool(st) { 1 } else { return None };
            Some((rank, h))
        })
        .min();
    if let Some((_, h)) = pick
        && inv::swap_with(bot, slot, h)
    {
        info!(target: "perimeter", "{id} into hotbar {h}");
    }
}

/// The hotbar slot the weapon is in, if any - the dig never swaps a tool into it.
pub fn weapon_hotbar(bot: &Client) -> Vec<u8> {
    killaura::best_weapon(bot).and_then(|(slot, _)| inv::hotbar_index(slot)).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn hurt(ago: f64, lost: f32, from: &str, now: Instant) -> Hurt {
        Hurt { at: now - Duration::from_secs_f64(ago), lost, from: vec![from.to_string()] }
    }

    #[test]
    fn the_pillager_that_killed_a_bot_logs_it_out_at_the_first_arrow() {
        // 14:46:49 - 20 -> 14 from an arrow (then 8.6, 3.1, dead within 3 s).
        let now = Instant::now();
        let hurts = vec![hurt(0.0, 6.0, "arrow", now)];
        let why = log_out_why(&hurts, 14.0, now).expect("logs out");
        assert!(why.contains("arrow"), "{why}");
    }

    #[test]
    fn its_own_clipping_and_falls_never_log_it_out() {
        let now = Instant::now();
        let hurts = vec![hurt(1.0, 4.0, "fall", now), hurt(0.5, 3.0, "fall", now), hurt(0.0, 2.0, "in_wall", now)];
        assert_eq!(log_out_why(&hurts, 11.0, now), None);
    }

    #[test]
    fn low_health_after_a_small_hit_logs_out_an_old_hit_does_not() {
        let now = Instant::now();
        assert!(log_out_why(&[hurt(2.0, 2.0, "mob_attack", now)], 12.0, now).is_some());
        assert_eq!(log_out_why(&[hurt(20.0, 2.0, "mob_attack", now)], 12.0, now), None, "20 s ago, healing since");
        assert_eq!(log_out_why(&[hurt(1.0, 2.0, "mob_attack", now)], 18.0, now), None, "a scratch at full health");
    }
}
