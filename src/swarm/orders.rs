//! Orders: what each swarm bot is doing, and who said so. Bots listen by default to what the
//! controller says or what is set for the swarm on the control panel, with a per-bot difference
//! only when one is set.
//!
//! So three places an order comes from - the panel's swarm-wide order, the controller (each
//! bot's own link), and a per-bot order set in the panel - and the rule:
//!   a per-bot order wins; else the newer of the panel's and the controller's (the controller only
//!   counts for a bot that listens to it). A controller Stay Near keeps its first stamp while its
//!   updates stream in, so a panel order given afterwards is not undone by the next update.

use std::time::{SystemTime, UNIX_EPOCH};

use azalea::Vec3;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Order {
    /// Hold where it is.
    Idle,
    /// Fly to a point (spread round it when several bots share the order).
    Goto { x: f64, y: f64, z: f64 },
    /// The controller's Come Here: its position when it was pressed.
    ComeHere { x: f64, y: f64, z: f64 },
    /// Keep near someone at an offset relative to their facing (the controller's Stay Near).
    StayNear {
        follow: Follow,
        #[serde(default)]
        offsets: Offsets,
        #[serde(default = "default_hold")]
        hold: f64,
    },
    /// The swarm formation (its shape is the swarm's formation setting) round a centre.
    Formation { centre: Centre },
}

fn default_hold() -> f64 {
    3.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Follow {
    /// The player at the controller (its live position stream, else its Stay Near updates).
    Controller,
    /// A player by name, wherever a swarm bot can see them.
    Player { name: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Centre {
    Controller,
    Player { name: String },
    Point { x: f64, y: f64, z: f64 },
}

/// Stay Near offsets, blocks, relative to the followed player's facing (BotFormation).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Offsets {
    pub front: f64,
    pub back: f64,
    pub left: f64,
    pub right: f64,
    pub above: f64,
    pub below: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    Panel,
    Controller,
    /// Set in the panel for this one bot.
    Bot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stamped {
    pub order: Order,
    pub source: Source,
    /// Milliseconds since the epoch: newer wins.
    pub at: u64,
}

impl Stamped {
    pub fn new(order: Order, source: Source) -> Self {
        Self { order, source, at: now_ms() }
    }

    pub fn idle() -> Self {
        Self { order: Order::Idle, source: Source::Panel, at: 0 }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// One bot's orders.
#[derive(Clone, Debug, Default)]
pub struct BotOrders {
    /// The latest from the controller link.
    pub controller: Option<Stamped>,
    /// Where the controller's last Stay Near put the anchor (its player), and its yaw if known.
    pub anchor: Option<(Vec3, f32)>,
    /// Set in the panel for this bot alone.
    pub own: Option<Stamped>,
}

impl BotOrders {
    /// The controller said something. A Stay Near that continues one already running keeps the
    /// running one's stamp (only the anchor moves).
    pub fn from_controller(&mut self, order: Order) {
        let keep = match (&self.controller, &order) {
            (Some(Stamped { order: Order::StayNear { .. }, at, .. }), Order::StayNear { .. }) => Some(*at),
            _ => None,
        };
        let mut s = Stamped::new(order, Source::Controller);
        if let Some(at) = keep {
            s.at = at;
        }
        self.controller = Some(s);
    }

    /// The controller released Stay Near: back to whatever the panel says.
    pub fn controller_release(&mut self) {
        if matches!(self.controller, Some(Stamped { order: Order::StayNear { .. }, .. })) {
            self.controller = None;
            self.anchor = None;
        }
    }
}

/// The order this bot follows now.
pub fn effective(panel: &Stamped, bot: &BotOrders, listens_to_controller: bool) -> Stamped {
    if let Some(own) = &bot.own {
        return own.clone();
    }
    match &bot.controller {
        Some(c) if listens_to_controller && c.at >= panel.at => c.clone(),
        _ => panel.clone(),
    }
}

/// Where a Stay Near keeps the bot: the followed player's spot moved by the offsets, turned to
/// their facing (the controller's computeStayNearHoldPoint: yaw 0 = south, clockwise).
pub fn hold_point(base: Vec3, yaw: f32, o: &Offsets) -> Vec3 {
    let a = (yaw as f64).to_radians();
    let (fx, fz) = (-a.sin(), a.cos());
    let (rx, rz) = (-fz, fx);
    let forward = o.front - o.back;
    let strafe = o.right - o.left;
    Vec3::new(base.x + fx * forward + rx * strafe, base.y + o.above - o.below, base.z + fz * forward + rz * strafe)
}

/// Bot `i` of `n` sent to one point: spread on a ring `spacing` apart, so they do not all stand in
/// one block (one bot: the point itself).
pub fn fan_out(point: Vec3, i: usize, n: usize, spacing: f64) -> Vec3 {
    if n <= 1 {
        return point;
    }
    let r = (spacing * n as f64 / std::f64::consts::TAU).max(spacing);
    let a = std::f64::consts::TAU * i as f64 / n as f64;
    Vec3::new(point.x + r * a.cos(), point.y, point.z + r * a.sin())
}

impl Order {
    /// A short line for the panel.
    pub fn describe(&self) -> String {
        match self {
            Order::Idle => "idle".into(),
            Order::Goto { x, y, z } => format!("go to {x:.0} {y:.0} {z:.0}"),
            Order::ComeHere { x, y, z } => format!("come here ({x:.0} {y:.0} {z:.0})"),
            Order::StayNear { follow: Follow::Controller, .. } => "stay near the controller".into(),
            Order::StayNear { follow: Follow::Player { name }, .. } => format!("stay near {name}"),
            Order::Formation { centre } => format!(
                "formation round {}",
                match centre {
                    Centre::Controller => "the controller".to_string(),
                    Centre::Player { name } => name.clone(),
                    Centre::Point { x, y, z } => format!("{x:.0} {y:.0} {z:.0}"),
                }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_bot_wins_then_the_newer_of_panel_and_controller() {
        let panel = Stamped { order: Order::Idle, source: Source::Panel, at: 100 };
        let mut b = BotOrders::default();
        assert_eq!(effective(&panel, &b, true).source, Source::Panel);
        b.controller = Some(Stamped { order: Order::Goto { x: 1.0, y: 2.0, z: 3.0 }, source: Source::Controller, at: 200 });
        assert_eq!(effective(&panel, &b, true).source, Source::Controller);
        // Not listening: the controller is ignored.
        assert_eq!(effective(&panel, &b, false).source, Source::Panel);
        // A newer panel order wins.
        let newer = Stamped { order: Order::Idle, source: Source::Panel, at: 300 };
        assert_eq!(effective(&newer, &b, true).source, Source::Panel);
        // A per-bot order wins over both.
        b.own = Some(Stamped { order: Order::Idle, source: Source::Bot, at: 1 });
        assert_eq!(effective(&newer, &b, true).source, Source::Bot);
    }

    #[test]
    fn stay_near_updates_keep_their_first_stamp() {
        let mut b = BotOrders::default();
        let sn = || Order::StayNear { follow: Follow::Controller, offsets: Offsets::default(), hold: 3.0 };
        b.from_controller(sn());
        let first = b.controller.as_ref().unwrap().at;
        std::thread::sleep(std::time::Duration::from_millis(5));
        b.from_controller(sn());
        assert_eq!(b.controller.as_ref().unwrap().at, first);
        // A different order is new.
        b.from_controller(Order::ComeHere { x: 0.0, y: 0.0, z: 0.0 });
        assert!(b.controller.as_ref().unwrap().at >= first);
        b.controller_release();
        assert!(b.controller.is_some(), "release only drops a Stay Near");
    }

    #[test]
    fn hold_point_turns_with_the_facing() {
        let o = Offsets { front: 2.0, ..Default::default() };
        // Facing south (yaw 0): in front is +z.
        let p = hold_point(Vec3::ZERO, 0.0, &o);
        assert!((p.z - 2.0).abs() < 1e-9 && p.x.abs() < 1e-9);
        // Facing west (yaw 90): in front is -x.
        let p = hold_point(Vec3::ZERO, 90.0, &o);
        assert!((p.x + 2.0).abs() < 1e-9 && p.z.abs() < 1e-9);
    }

    #[test]
    fn fan_out_spreads_a_shared_point() {
        let p = Vec3::new(10.0, 64.0, 10.0);
        assert_eq!(fan_out(p, 0, 1, 2.0), p);
        let a = fan_out(p, 0, 4, 2.0);
        let b = fan_out(p, 1, 4, 2.0);
        assert!(a.distance_to(b) >= 2.0);
    }

    #[test]
    fn orders_round_trip_through_json() {
        let o = Order::Formation { centre: Centre::Player { name: "SomePlayer".into() } };
        let s = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<Order>(&s).unwrap(), o);
        let s = r#"{"kind":"stay-near","follow":{"kind":"controller"}}"#;
        assert!(matches!(serde_json::from_str::<Order>(s).unwrap(), Order::StayNear { hold, .. } if hold == 3.0));
    }
}
