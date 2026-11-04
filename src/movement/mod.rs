//! Movement: flight, no-fall, velocity and the charge/vclip mover - the standalone's Flight +
//! NoFall + Velocity + ChargeFlight + GotoPilot, rebuilt on azalea.
//!
//! azalea only walks (its physics has gravity and no creative flight), so this owns the bot's
//! position outright: every tick, after azalea's physics and before azalea sends its own position
//! packet, [`flight_tick`] puts the bot where it should be and sends the packets for it itself -
//! charge packets, then one position packet - and tells azalea that position was already sent.
//!
//! - **Flight**: idle = hold position (no gravity, no drift); a trip = one hop a tick.
//! - **No-fall**: every move packet claims on-ground, ours and azalea's (the server banks no fall
//!   distance, so landing never hurts). After a server teleport one grounded status packet goes
//!   out at once, as FlightMover.assertGrounded does.
//! - **Velocity**: knockback and explosions only change azalea's velocity, which is zeroed and
//!   overridden every tick while flying.
//! - **Charge**: see [`budget`]; vertical clips through roofs and floors in [`trip`].

pub mod budget;
pub mod geom;
pub mod trip;

use std::time::Duration;

use azalea::{
    Client, Vec3,
    app::{App, Plugin},
    connection::RawConnection,
    core::tick::GameTick,
    ecs::prelude::*,
    entity::{Dead, HasClientLoaded, LastSentPosition, LocalEntity, LookDirection, Physics, Position, metadata::Health},
    local_player::WorldHolder,
    movement::{LastSentLookDirection, send_position},
    packet::game::SendGamePacketEvent,
    physics::PhysicsSystems,
    protocol::{
        common::movements::MoveFlags,
        packets::{
            Packet,
            game::{ServerboundGamePacket, ServerboundMovePlayerPos, ServerboundMovePlayerRot, ServerboundMovePlayerStatusOnly},
        },
    },
};
use azalea::ecs as bevy_ecs; // for #[derive(Component)]
use tracing::{debug, info, warn};

use self::{
    budget::Budget,
    trip::{Step, Trip, fmt},
};

pub struct MovementPlugin;

impl Plugin for MovementPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            GameTick,
            (add_mover, flight_tick)
                .chain()
                .after(PhysicsSystems)
                // Our position first, then azalea fits the pose and bounding box to it and sends nothing.
                .before(azalea::movement::update_pose)
                .before(azalea::entity::EntityGeometryUpdateSystems)
                .before(send_position),
        )
            .add_observer(watch_outgoing);
    }
}

/// Settings, named as in the standalone's flight / no-fall / velocity / charge-flight modules.
#[derive(Clone, Debug)]
pub struct MoveSettings {
    /// Hold position and fly trips (flight). Off = azalea's own walking physics.
    pub flight: bool,
    /// Every move packet claims on-ground (no-fall).
    pub no_fall: bool,
    /// Trip top speed, blocks per second (GotoPilot max).
    pub max_bps: f64,
    /// A flat box under a ceiling (the seller's pen): never climb to the cruise height, never clip
    /// or back off UPWARD to escape - go around at this height, or give the trip up. Everything is
    /// reachable in small hops, so the sky route (and the stranded-above-a-floor clip-down it leads
    /// to) is only a way to hurt itself. Normal descents still sink gently through open air with
    /// no-fall on; the clip-DOWN-through-solid fall-damage guard is untouched.
    pub contained: bool,
}

impl Default for MoveSettings {
    fn default() -> Self {
        Self { flight: true, no_fall: true, max_bps: trip::DEFAULT_MAX_BPS, contained: false }
    }
}

#[derive(Component, Debug, Default)]
pub struct Mover {
    pub settings: MoveSettings,
    pub budget: Budget,
    /// Where it holds when no trip is running (set from the server's position on join/teleport).
    hold: Option<Vec3>,
    trip: Option<Trip>,
    next_id: u64,
    /// The last trip's end: (id, Ok or why it failed).
    last_result: Option<(u64, Result<(), String>)>,
    /// The server teleported us (rubber-band, join, respawn) since the last tick.
    teleported: bool,
    ticks: u64,
    /// Trip speed carried from one trip to the next (setbacks slow it, clean time speeds it up).
    bps: f64,
    clean_ticks: u32,
    /// The last hop sent (from, to) - named when the server refuses it.
    last_hop: Option<(Vec3, Vec3)>,
    /// Recent server setbacks (when, where, the hop refused), to notice being pulled back to one
    /// spot over and over - and which way out of it the server will not have.
    recent_setbacks: std::collections::VecDeque<(std::time::Instant, Vec3, Option<Vec3>)>,
    /// The tick of the last bought hop (charged, or longer than STEP_ALWAYS_LEGAL); 0 = none yet.
    last_bought_tick: u64,
    /// Trip destinations the server would not let us reach, and until when to believe it.
    refused: Vec<(Vec3, std::time::Instant)>,
    /// Columns whose clip up the server refused: trips step aside round them.
    refused_clips: trip::RefusedClips,
}

/// Pulled back to one spot this often within 5 s: give the trip up (16 Oct: at 5, with the farmer
/// re-sending the trip each tick, one drop cost 55 setbacks in 10 s).
const SAME_SPOT_REPEATS: usize = 3;
/// How long a refused destination is left alone.
const REFUSED_FOR: Duration = Duration::from_secs(60);
/// A pull-back this soon after a bought hop may be speed; any later one cannot be.
const BOUGHT_HOP_TICKS: u64 = 20;

impl Mover {
    fn start_trip(&mut self, from: Vec3, dest: Vec3, label: &str, quiet: bool, cruise: bool) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        if let Some(old) = self.trip.take() {
            self.last_result = Some((old.id, Err("replaced by a newer trip".into())));
        }
        if !quiet {
            info!(target: "goto", "trip '{label}' to {} ({:.0} blocks)", fmt(dest), from.distance_to(dest));
        }
        let start = if self.bps > 0.0 { self.bps } else { self.settings.max_bps };
        // Contained: never route via the sky, whatever the caller asked for.
        let cruise = cruise && !self.settings.contained;
        let mut t = Trip::new(id, from, dest, self.settings.max_bps, start, &self.budget, label, cruise, self.settings.contained);
        t.quiet = quiet;
        self.trip = Some(t);
        id
    }

    /// The server would not let us reach `dest`: callers that check (the farmer) leave it alone.
    fn refuse(&mut self, dest: Vec3) {
        let now = std::time::Instant::now();
        self.refused.retain(|(_, until)| *until > now);
        self.refused.push((dest, now + REFUSED_FOR));
        info!(target: "goto", "leaving {} alone for {} s - the server would not let us there", fmt(dest), REFUSED_FOR.as_secs());
    }

    fn is_refused(&self, at: Vec3, r: f64) -> bool {
        let now = std::time::Instant::now();
        self.refused.iter().any(|(p, until)| *until > now && p.distance_to(at) <= r)
    }

    fn result_for(&self, id: u64) -> Option<Result<(), String>> {
        self.last_result.as_ref().filter(|(i, _)| *i == id).map(|(_, r)| r.clone())
    }

    fn stop(&mut self) {
        if let Some(t) = self.trip.take() {
            self.last_result = Some((t.id, Err("stopped".into())));
        }
    }

    pub fn status(&self) -> String {
        match &self.trip {
            Some(t) => format!("{} to {} at {:.0} b/s ({})", t.label, fmt(t.dest), t.speed(), self.budget.describe()),
            None => format!("holding ({})", self.budget.describe()),
        }
    }
}

fn add_mover(mut commands: Commands, query: Query<Entity, (With<LocalEntity>, Without<Mover>)>) {
    for e in &query {
        commands.entity(e).insert(Mover::default());
    }
}

/// Counts every move packet azalea itself sends (they spend the same per-tick index), and
/// notices server teleports by the accept-teleport azalea answers them with.
fn watch_outgoing(trigger: On<SendGamePacketEvent>, mut movers: Query<&mut Mover>) {
    let ev = trigger.event();
    let Ok(mut m) = movers.get_mut(ev.sent_by) else { return };
    match &ev.packet {
        ServerboundGamePacket::AcceptTeleportation(_) => m.teleported = true,
        ServerboundGamePacket::MovePlayerPos(_)
        | ServerboundGamePacket::MovePlayerPosRot(_)
        | ServerboundGamePacket::MovePlayerRot(_)
        | ServerboundGamePacket::MovePlayerStatusOnly(_) => m.budget.record(1, true),
        _ => m.budget.record(1, false),
    }
}

#[allow(clippy::type_complexity)]
fn flight_tick(
    mut query: Query<
        (
            &mut Mover,
            &mut Position,
            &mut LastSentPosition,
            &mut Physics,
            &WorldHolder,
            &mut RawConnection,
            Has<Dead>,
            Option<&crate::bot::State>,
            Option<&Health>,
        ),
        (With<LocalEntity>, With<HasClientLoaded>),
    >,
) {
    for (mut m, mut pos, mut last_sent, mut physics, holder, mut conn, dead, state, health) in &mut query {
        // A swarm bot logs into its own folder (debuglog routes by this span).
        let _bot = state.and_then(|s| s.swarm_name()).map(|n| tracing::info_span!("bot", name = %n).entered());
        m.ticks += 1;
        let here = **last_sent;

        if m.teleported {
            // The server put us somewhere (a setback, the join, a respawn): that is where we are now.
            m.teleported = false;
            let server_pos = **pos;
            if m.hold.is_some() || m.trip.is_some() {
                info!(target: "goto", "server moved us to {} ({})", fmt(server_pos), m.budget.describe());
            }
            m.budget.on_rubberband(server_pos);
            let refused_to = m.last_hop.take().map(|(from, to)| {
                info!(target: "goto", "  refused hop: {} -> {} ({:.2} blocks)", fmt(from), fmt(to), from.distance_to(to));
                to
            });
            // Pulled back to where a clip up started: that column is refused, and the trips step
            // aside round it rather than try it again.
            if let Some((from, to_y)) = m.trip.as_mut().and_then(|t| t.refused_clip(server_pos)) {
                let seen = geom::describe_column(&holder.shared.read(), from, to_y + 2.0);
                warn!(target: "goto", "the server refused the clip up {} -> y={to_y:.2}; avoiding that column for {} min. This client sees there: {seen}", fmt(from), trip::RefusedClips::minutes());
                m.refused_clips.add(from, to_y);
            }
            // Only a pull-back right after a bought hop can be speed. A hop under STEP_ALWAYS_LEGAL
            // is never too fast, so refusing one means something in the way that this client does
            // not see (16 Oct: ~1000 setbacks at the farm, all short hops down onto the floor, each
            // cutting the speed 40% - the bot crawled at 2 b/s for minutes after every burst).
            let speed = m.last_bought_tick > 0 && m.ticks.saturating_sub(m.last_bought_tick) <= BOUGHT_HOP_TICKS;
            if speed {
                m.budget.speed_setbacks += 1;
                // One speed for the next trips too.
                let base = if m.bps > 0.0 { m.bps } else { m.settings.max_bps };
                m.bps = (base * 0.6).max(2.0);
                m.clean_ticks = 0;
            }
            // Pulled back to the same spot again and again: stop pushing, and leave that
            // destination alone a while (the farm: 22 setbacks to one spot in 2 s on 15 Oct, 55 in
            // 10 s on 16 Oct).
            let now = std::time::Instant::now();
            m.recent_setbacks.push_back((now, server_pos, refused_to));
            while m.recent_setbacks.front().is_some_and(|(t, _, _)| now.duration_since(*t).as_secs() >= 5) {
                m.recent_setbacks.pop_front();
            }
            let same = m.recent_setbacks.iter().filter(|(_, p, _)| p.distance_to(server_pos) < 1.0).count();
            if same >= SAME_SPOT_REPEATS {
                if let Some(t) = m.trip.take() {
                    warn!(target: "goto", "the server pulled us back to {} {same} times in 5 s - giving up '{}' to {}", fmt(server_pos), t.label, fmt(t.dest));
                    // The way(s) out it refused: the next trip from here takes another (17 Oct: 16
                    // hours of the same climb from one spot, retried every few seconds).
                    let tos: Vec<Vec3> = m.recent_setbacks.iter().filter(|(_, p, _)| p.distance_to(server_pos) < 1.0).filter_map(|(_, _, to)| *to).collect();
                    for to in tos {
                        if m.refused_clips.add_way(server_pos, to) {
                            let column = Vec3::new(to.x, server_pos.y.min(to.y), to.z);
                            let seen = geom::describe_column(&holder.shared.read(), column, server_pos.y.max(to.y) + 2.0);
                            warn!(target: "goto", "  not going {} from {} again for {} min (toward {}). This client sees there: {seen}",
                                heading(server_pos, to), fmt(server_pos), trip::RefusedClips::way_minutes(), fmt(to));
                        }
                    }
                    m.last_result = Some((t.id, Err(format!("the server keeps pulling back to {}", fmt(server_pos)))));
                    m.refuse(t.dest);
                }
                m.recent_setbacks.clear();
            }
            m.hold = Some(server_pos);
            **last_sent = server_pos;
            if m.settings.no_fall {
                // azalea's answer to the teleport claimed off-ground: say grounded straight away.
                send(&mut conn, &mut m.budget, ServerboundMovePlayerStatusOnly { flags: grounded(true) }.into_variant(), true);
            }
            physics.velocity = Vec3::ZERO;
            physics.fall_distance = 0.0;
            continue;
        }
        m.budget.new_tick(here);
        if m.bps <= 0.0 {
            m.bps = m.settings.max_bps;
        }
        m.clean_ticks += 1;
        if m.clean_ticks >= 60 && m.bps < m.settings.max_bps {
            m.bps = (m.bps * 1.1).min(m.settings.max_bps);
            m.clean_ticks = 0;
        }
        if let Some(t) = &m.trip {
            // The trip's own speed after its setbacks is the one the next trip starts at.
            m.bps = m.bps.min(t.speed());
        }

        if dead || !m.settings.flight {
            if m.settings.no_fall {
                physics.set_on_ground(true);
            }
            m.hold = None;
            continue;
        }
        let hold = *m.hold.get_or_insert(here);

        // Where to be at the end of this tick.
        let target = match m.trip.as_mut() {
            None => hold,
            Some(_) => {
                let world = holder.shared.read();
                let Mover { trip, budget, refused_clips, .. } = &mut *m;
                let step = trip.as_mut().unwrap().tick(&world, here, budget, refused_clips, health.map_or(20.0, |h| **h));
                match step {
                    Step::Hop(next) => next,
                    Step::Wait => hold,
                    Step::Arrived => {
                        let t = m.trip.take().unwrap();
                        if !t.quiet {
                            info!(target: "goto", "ARRIVED at {} ({})", fmt(here), m.budget.describe());
                        }
                        m.last_result = Some((t.id, Ok(())));
                        here
                    }
                    Step::Failed(why) => {
                        let t = m.trip.take().unwrap();
                        warn!(target: "goto", "GAVE UP on '{}' to {}: {why}", t.label, fmt(t.dest));
                        if t.refused_out() {
                            m.refuse(t.dest);
                        }
                        m.last_result = Some((t.id, Err(why)));
                        here
                    }
                }
            }
        };

        if let Some(t) = &m.trip
            && m.ticks.is_multiple_of(20)
        {
            debug!(target: "goto", "'{}' at {} -> leg {} (dest {}), {:.0} b/s, {}", t.label, fmt(here), fmt(t.goal()), fmt(t.dest), t.speed(), m.budget.describe());
        }
        if target.distance_to(here) > 1.0e-4 {
            let charge = m.budget.charge_to(here, target);
            if !m.budget.can_afford(charge + 1) {
                // Paper's packet limiter would kick: hold this tick instead.
                warn!(target: "goto", "packet-rate limit near: holding this tick (charge {charge} wanted)");
                **pos = here;
            } else {
                if charge > 0 {
                    debug!(target: "goto", "charge {charge} for a {:.1}-block hop {} -> {}", here.distance_to(target), fmt(here), fmt(target));
                }
                if charge > 0 || here.distance_to(target) > budget::STEP_ALWAYS_LEGAL {
                    m.last_bought_tick = m.ticks;
                }
                let flags = grounded(m.settings.no_fall);
                for _ in 0..charge {
                    send(&mut conn, &mut m.budget, ServerboundMovePlayerStatusOnly { flags }.into_variant(), true);
                }
                let p = Vec3::new(geom::smooth(target.x), geom::smooth(target.y), geom::smooth(target.z));
                send(&mut conn, &mut m.budget, ServerboundMovePlayerPos { pos: p, flags }.into_variant(), true);
                **pos = p;
                **last_sent = p;
                m.last_hop = Some((here, p));
                m.hold = Some(p);
            }
        } else {
            **pos = here;
        }
        // Velocity: whatever knocked us this tick is gone. No-fall: nothing banks.
        physics.velocity = Vec3::ZERO;
        physics.fall_distance = 0.0;
        if m.settings.no_fall {
            physics.set_on_ground(true);
        }
    }
}

/// "up", "down", or a compass heading (north = -z), for the log.
fn heading(from: Vec3, to: Vec3) -> String {
    let d = to - from;
    let h = (d.x * d.x + d.z * d.z).sqrt();
    if d.y.abs() > h * 2.0 {
        return if d.y > 0.0 { "up" } else { "down" }.into();
    }
    const NAMES: [&str; 8] = ["east", "south-east", "south", "south-west", "west", "north-west", "north", "north-east"];
    let i = ((d.z.atan2(d.x) / std::f64::consts::FRAC_PI_4).round() as i32).rem_euclid(8) as usize;
    let tilt = if d.y > h * 0.3 { " and up" } else if d.y < -h * 0.3 { " and down" } else { "" };
    format!("{}{tilt}", NAMES[i])
}

fn grounded(on_ground: bool) -> MoveFlags {
    MoveFlags { on_ground, horizontal_collision: false }
}

/// Straight to the socket, counted - so the charge maths always knows the true index.
fn send(conn: &mut RawConnection, budget: &mut Budget, packet: ServerboundGamePacket, movement: bool) {
    if let Err(e) = conn.write(packet) {
        warn!("movement: could not send: {e}");
    }
    budget.record(1, movement);
}

// ------------------------------------------------------------------ the async side

/// Fly to `dest` (feet position); resolves when there, or with why not.
pub async fn fly_to(bot: &Client, dest: Vec3, label: &str) -> Result<(), String> {
    let from = bot.position().map_err(|e| e.to_string())?;
    let label = label.to_string();
    let id = bot.query_self::<&mut Mover, _>(|mut m| m.start_trip(from, dest, &label, false, true)).map_err(|e| e.to_string())?;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        match bot.query_self::<&Mover, _>(|m| m.result_for(id)) {
            Ok(Some(r)) => return r,
            Ok(None) => {}
            Err(e) => return Err(format!("left the world mid-trip ({e})")),
        }
    }
}

/// Start a trip without waiting for it (replacing any running one); quiet = no trip/arrival lines
/// (the farmer starts one a second).
pub fn go(bot: &Client, dest: Vec3, label: &str) -> Option<u64> {
    let from = bot.position().ok()?;
    bot.query_self::<&mut Mover, _>(|mut m| m.start_trip(from, dest, label, true, true)).ok()
}

/// As go, but straight there: never via the cruise height (hops inside a build).
pub fn go_direct(bot: &Client, dest: Vec3, label: &str) -> Option<u64> {
    let from = bot.position().ok()?;
    bot.query_self::<&mut Mover, _>(|mut m| m.start_trip(from, dest, label, true, false)).ok()
}

/// As go_direct, but it only counts as there within `arrive` blocks (not ARRIVE_DIST's 1.25): a
/// spot that must be reached closely - a villager's trading reach.
pub fn go_direct_within(bot: &Client, dest: Vec3, label: &str, arrive: f64) -> Option<u64> {
    let from = bot.position().ok()?;
    bot.query_self::<&mut Mover, _>(|mut m| {
        let id = m.start_trip(from, dest, label, true, false);
        if let Some(t) = m.trip.as_mut() {
            t.set_arrive(arrive);
        }
        id
    })
    .ok()
}

/// Fly to a destination that moves (a formation slot, a followed player): the running trip with
/// this label carries on while the spot has not jumped far - pointed at the new spot once it is
/// on its last leg; otherwise a fresh trip (via the cruise height when far). Arrives within
/// `arrive` blocks. Quiet.
pub fn steer(bot: &Client, dest: Vec3, label: &str, arrive: f64) {
    let Ok(from) = bot.position() else { return };
    let _ = bot.query_self::<&mut Mover, _>(|mut m| {
        // Never restarted for a trip still on its way (cruise legs, a back-off, a shaft): 3 Nov,
        // over a perimeter cell at the cruise height a fresh trip a tick had its first two legs
        // where the bot already was, and never got to the leg down.
        if let Some(t) = m.trip.as_mut()
            && t.label == label
            && t.dest.distance_to(dest) < 16.0
        {
            if t.on_last_leg() {
                t.retarget(dest);
            }
            return;
        }
        if from.distance_to(dest) <= arrive {
            return;
        }
        m.start_trip(from, dest, label, true, true);
        if let Some(t) = m.trip.as_mut() {
            t.set_arrive(arrive);
        }
    });
}

/// The server refused to let us reach somewhere within `r` of `at` in the last minute.
pub fn refused_near(bot: &Client, at: Vec3, r: f64) -> bool {
    bot.query_self::<&Mover, _>(|m| m.is_refused(at, r)).unwrap_or(false)
}

/// Face this way now: a rotation packet goes straight out (a click sent after it is judged with
/// this look - a stair's facing, a piston's...), counted like any move packet, and azalea is told
/// it was sent so it does not send its own.
pub fn look_now(bot: &Client, yaw: f32, pitch: f32) {
    let _ = bot.query_self::<(&mut Mover, &mut RawConnection, &mut LookDirection, &mut LastSentLookDirection), _>(
        |(mut m, mut conn, mut look, mut last)| {
            let dir = LookDirection::new(yaw, pitch);
            *look = dir;
            last.y_rot = dir.y_rot();
            last.x_rot = dir.x_rot();
            let flags = grounded(m.settings.no_fall);
            send(&mut conn, &mut m.budget, ServerboundMovePlayerRot { look_direction: dir, flags }.into_variant(), true);
        },
    );
}

/// Top trip speed (the swarm panel sets it for every swarm bot).
pub fn set_max_bps(bot: &Client, v: f64) {
    let v = v.clamp(1.0, 100.0);
    let _ = bot.query_self::<&mut Mover, _>(|mut m| {
        if (m.settings.max_bps - v).abs() > 1e-9 {
            m.settings.max_bps = v;
            if m.bps > v {
                m.bps = v;
            }
        }
    });
}

/// Keep every trip inside a flat, roofed box: no cruise climb to the sky, no clipping or backing
/// off UPWARD to escape an obstacle - go around at this height instead, or give the trip up. The
/// seller sets this once; it is what stops a short hop to a nearby chest turning into a launch over
/// the roof and a self-harming clip back down through the floor.
pub fn set_contained(bot: &Client, on: bool) {
    let _ = bot.query_self::<&mut Mover, _>(|mut m| m.settings.contained = on);
}

/// A trip is running.
pub fn busy(bot: &Client) -> bool {
    bot.query_self::<&Mover, _>(|m| m.trip.is_some()).unwrap_or(false)
}

pub fn stop(bot: &Client) {
    let _ = bot.query_self::<&mut Mover, _>(|mut m| m.stop());
}

pub fn status(bot: &Client) -> String {
    bot.query_self::<&Mover, _>(|m| m.status()).unwrap_or_else(|_| "no mover".into())
}
