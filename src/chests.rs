//! The signed chests at the base - "tools", "repair", "food" - and moving stacks in and out of them.
//! Port of AiChores.toolsChests/signedChest + the Storage trips the melon routine uses.
//!
//! azalea keeps no block entities, so sign text is caught here from the chunk and block-entity
//! packets. Clicks are paced at 4 a tick: a 200-click tick got the Java client kicked for packet
//! rate on 1 Oct. Every store/take is judged by counting the bags before and after.

use std::{collections::HashMap, time::Duration};

use azalea::{
    BlockPos, Client, Vec3,
    app::{App, Plugin, Update},
    block::properties::FacingCardinal,
    ecs::prelude::*,
    entity::LocalEntity,
    inventory::ItemStack,
    packet::game::ReceiveGamePacketEvent,
    protocol::packets::game::ClientboundGamePacket,
    registry::builtin::{BlockEntityKind, BlockKind},
};
use azalea::ecs as bevy_ecs; // for #[derive(Component)]
use tracing::{debug, info, warn};

use crate::{inv, movement};

/// How far from the base the signs are looked for (AiChores.TOOLS_REACH), and up/down.
const REACH: i32 = 24;
const REACH_Y: i32 = 12;
const CLICKS_PER_TICK: usize = 4;

pub struct ChestsPlugin;

impl Plugin for ChestsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, record_signs);
    }
}

/// Sign text by position, lower case (front and back together).
#[derive(Component, Default)]
pub struct Signs(pub HashMap<BlockPos, String>);

/// Each sign's lines as written, front then back, empty ones left out - for a sign that must read
/// exactly something (the mapart's colour chests: "GRAY" is not "LIGHT GRAY"), where Signs' NBT
/// dump would also match its keys and flags.
#[derive(Component, Default)]
pub struct SignLines(pub HashMap<BlockPos, Vec<String>>);

/// A sign's lines from its block entity NBT: front_text then back_text, each a list of 4
/// messages - a plain string, or a text component (its "text", the "" of a mixed list's wrapper,
/// and its "extra" after it). A JSON string from an older server is read the same way.
pub fn sign_lines(nbt: &simdnbt::owned::NbtCompound) -> Vec<String> {
    fn component(c: &simdnbt::owned::NbtCompound) -> String {
        let mut out = c.string("text").or_else(|| c.string("")).map(|s| s.to_str().into_owned()).unwrap_or_default();
        if let Some(extra) = c.list("extra") {
            out.extend(line_list(extra));
        }
        out
    }
    fn plain(s: &str) -> String {
        let t = s.trim();
        if let Some(inner) = t.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
            return inner.to_string();
        }
        if t.starts_with('{') {
            // {"text":"RED"} and its extras: every "text" value, in order.
            return t.split("\"text\":\"").skip(1).filter_map(|p| p.split('"').next()).collect();
        }
        s.to_string()
    }
    fn line_list(l: &simdnbt::owned::NbtList) -> Vec<String> {
        if let Some(strs) = l.strings() {
            strs.iter().map(|s| plain(&s.to_str())).collect()
        } else if let Some(cs) = l.compounds() {
            cs.iter().map(component).collect()
        } else {
            Vec::new()
        }
    }
    let mut out = Vec::new();
    for side in ["front_text", "back_text"] {
        if let Some(m) = nbt.compound(side).and_then(|t| t.list("messages")) {
            out.extend(line_list(m).into_iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()));
        }
    }
    out
}

fn is_sign(kind: BlockEntityKind) -> bool {
    matches!(kind, BlockEntityKind::Sign | BlockEntityKind::HangingSign)
}

/// Ender chests seen in this bot's chunks (the swarm's tools and weapons link to the nearest).
#[derive(Component, Default)]
pub struct EnderChests(pub std::collections::HashSet<BlockPos>);

fn record_signs(
    mut events: MessageReader<ReceiveGamePacketEvent>,
    mut us: Query<(Option<&mut Signs>, Option<&mut SignLines>, Option<&mut EnderChests>), With<LocalEntity>>,
    mut commands: Commands,
) {
    for ev in events.read() {
        let mut found: Vec<(BlockPos, String)> = Vec::new();
        let mut lines: Vec<(BlockPos, Vec<String>)> = Vec::new();
        let mut enders: Vec<BlockPos> = Vec::new();
        match ev.packet.as_ref() {
            ClientboundGamePacket::LevelChunkWithLight(p) => {
                for be in &p.chunk_data.block_entities {
                    let x = p.x * 16 + (be.packed_xz >> 4) as i32;
                    let z = p.z * 16 + (be.packed_xz & 15) as i32;
                    let at = BlockPos::new(x, be.y as i16 as i32, z);
                    if be.kind == BlockEntityKind::EnderChest {
                        enders.push(at);
                    } else if is_sign(be.kind) {
                        found.push((at, format!("{:?}", be.data).to_lowercase()));
                        lines.push((at, sign_lines(&be.data)));
                    }
                }
            }
            ClientboundGamePacket::BlockEntityData(p) if is_sign(p.block_entity_type) => {
                found.push((p.pos, format!("{:?}", p.tag).to_lowercase()));
                lines.push((p.pos, sign_lines(&p.tag)));
            }
            ClientboundGamePacket::BlockEntityData(p) if p.block_entity_type == BlockEntityKind::EnderChest => enders.push(p.pos),
            _ => continue,
        }
        if found.is_empty() && enders.is_empty() {
            continue;
        }
        let Ok((signs, sign_text, ec)) = us.get_mut(ev.entity) else { continue };
        if !lines.is_empty() {
            match sign_text {
                Some(mut s) => s.0.extend(lines),
                None => {
                    commands.entity(ev.entity).insert(SignLines(lines.into_iter().collect()));
                }
            }
        }
        if !found.is_empty() {
            match signs {
                Some(mut s) => s.0.extend(found),
                None => {
                    commands.entity(ev.entity).insert(Signs(found.into_iter().collect()));
                }
            }
        }
        if !enders.is_empty() {
            match ec {
                Some(mut e) => e.0.extend(enders),
                None => {
                    commands.entity(ev.entity).insert(EnderChests(enders.into_iter().collect()));
                }
            }
        }
    }
}

pub fn is_container(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Chest | BlockKind::TrappedChest | BlockKind::Barrel) || kind.to_str().ends_with("shulker_box")
}

/// The container a sign is on: behind a wall sign, else under, over or beside it.
pub fn container_for_sign(bot: &Client, sign: BlockPos) -> Option<BlockPos> {
    let world = bot.world().ok()?;
    let w = world.read();
    let state = w.get_block_state(sign)?;
    let mut tries = Vec::new();
    if let Some(f) = state.property::<FacingCardinal>() {
        // A wall sign faces away from the block it hangs on.
        let back = match f {
            FacingCardinal::North => BlockPos::new(0, 0, 1),
            FacingCardinal::South => BlockPos::new(0, 0, -1),
            FacingCardinal::East => BlockPos::new(-1, 0, 0),
            FacingCardinal::West => BlockPos::new(1, 0, 0),
        };
        tries.push(sign + back);
    }
    tries.extend([sign.down(1), sign.up(1), sign.north(1), sign.south(1), sign.east(1), sign.west(1)]);
    tries.into_iter().find(|p| w.get_block_state(*p).is_some_and(|s| is_container(BlockKind::from(s))))
}

/// The container signed with `word` ("tools", "repair", "food") nearest the base, if its chunk
/// has been seen.
pub fn signed(bot: &Client, base: BlockPos, word: &str) -> Option<BlockPos> {
    signed_all(bot, base, word).into_iter().next()
}

/// Every container in reach of `base` under a sign containing `word`, nearest first - a base may
/// have more than one tools or repair chest (6 Nov).
pub fn signed_all(bot: &Client, base: BlockPos, word: &str) -> Vec<BlockPos> {
    let signs: Vec<(BlockPos, String)> = bot
        .query_self::<Option<&Signs>, _>(|s| s.map(|s| s.0.iter().map(|(p, t)| (*p, t.clone())).collect()))
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut found: Vec<(i64, BlockPos)> = Vec::new();
    for (pos, text) in signs {
        if (pos.x - base.x).abs() > REACH || (pos.z - base.z).abs() > REACH || (pos.y - base.y).abs() > REACH_Y {
            continue;
        }
        if !text.contains(word) {
            continue;
        }
        // "repair" also contains no "tool"; "tools" must not match a "repair tools" sign first.
        if word == "tools" && text.contains("repair") {
            continue;
        }
        if let Some(c) = container_for_sign(bot, pos)
            && !found.iter().any(|(_, f)| *f == c)
        {
            let d = (c.x - base.x).pow(2) as i64 + (c.y - base.y).pow(2) as i64 + (c.z - base.z).pow(2) as i64;
            found.push((d, c));
        }
    }
    found.sort_by_key(|(d, c)| (*d, c.x, c.y, c.z));
    found.into_iter().map(|(_, c)| c).collect()
}

/// Every sign this bot has seen, by position, with its lines as written.
pub fn sign_texts(bot: &Client) -> Vec<(BlockPos, Vec<String>)> {
    bot.query_self::<Option<&SignLines>, _>(|s| s.map(|s| s.0.iter().map(|(p, t)| (*p, t.clone())).collect()))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Every sign text this bot has seen, by position (front and back joined, lower case).
pub fn signs(bot: &Client) -> Vec<(BlockPos, String)> {
    bot.query_self::<Option<&Signs>, _>(|s| s.map(|s| s.0.iter().map(|(p, t)| (*p, t.clone())).collect()))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Every container whose sign text contains `word`, inside the box, nearest the box centre first -
/// the stick-seller's source ("sticks") and deposit ("emeralds") chests.
pub fn signed_in(bot: &Client, area: &crate::config::Area, word: &str) -> Vec<BlockPos> {
    signed_where(bot, area, |text| text.contains(word))
}

/// Like `signed_in`, for a name that may be part of a longer one: `name` matches written with
/// spaces or underscores ("silk touch" / "silk_touch"), and a sign that reads one of the `longer`
/// names containing it is skipped - the "protection" chest is not the "fire protection" one.
pub fn signed_named(bot: &Client, area: &crate::config::Area, name: &str, longer: &[String]) -> Vec<BlockPos> {
    let forms = [name.to_string(), name.replace('_', " ")];
    let skip: Vec<String> = longer
        .iter()
        .filter(|l| l.as_str() != name && l.contains(name))
        .flat_map(|l| [l.clone(), l.replace('_', " ")])
        .collect();
    signed_where(bot, area, |text| forms.iter().any(|f| text.contains(f.as_str())) && !skip.iter().any(|s| text.contains(s.as_str())))
}

/// Every container whose sign text (lower case) passes `pick`, inside the box, nearest the box
/// centre first.
pub fn signed_where(bot: &Client, area: &crate::config::Area, pick: impl Fn(&str) -> bool) -> Vec<BlockPos> {
    let (cx, cz) = ((area.min.x + area.max.x) / 2, (area.min.z + area.max.z) / 2);
    let mut found: Vec<(i64, BlockPos)> = Vec::new();
    for (pos, text) in signs(bot) {
        if pos.x < area.min.x || pos.x > area.max.x || pos.z < area.min.z || pos.z > area.max.z {
            continue;
        }
        if !pick(&text) {
            continue;
        }
        if let Some(c) = container_for_sign(bot, pos)
            && !found.iter().any(|(_, p)| *p == c)
        {
            let d = (c.x - cx).pow(2) as i64 + (c.z - cz).pow(2) as i64;
            found.push((d, c));
        }
    }
    found.sort_by_key(|(d, _)| *d);
    found.into_iter().map(|(_, c)| c).collect()
}

/// Fly to a clear spot within reach of the container (above it first), and open it. A spot the
/// server will not let us reach (the refusals seen at the farm) is given up for the next best.
async fn open(bot: &Client, pos: BlockPos) -> Result<azalea::container::ContainerHandle, String> {
    fly_within_reach(bot, pos).await?;
    match bot.open_container_at(pos).await {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(format!("the chest at {pos} did not open")),
        Err(e) => Err(format!("opening the chest at {pos}: {e}")),
    }
}

/// The straight way to each spot is looked at from this near: from farther the trip goes by the
/// cruise height whatever the spot, and looking stopped the client (6 Nov: 2.5 s with the game tick
/// held, ~100 spots each sampled every 0.25 blocks over 900 - the perimeter bots picked up the junk
/// they had just thrown out meanwhile).
const WAY_CHECKED_WITHIN: f64 = 64.0;

/// Fly to a clear spot within reach of a container; a spot the server will not let us reach is
/// given up for the next best. Spots the mover can fly to from here without a detour come first
/// (it flies straight lines, and contained it gives up rather than go round: 30 Oct, the seller
/// tried the four nearest spots of a sticks chest, every one blocked from where it stood, on every
/// pass for two hours while the chest filled up), then the nearest the chest.
pub async fn fly_within_reach(bot: &Client, pos: BlockPos) -> Result<(), String> {
    let centre = Vec3::new(pos.x as f64 + 0.5, pos.y as f64 + 0.5, pos.z as f64 + 0.5);
    let from = bot.position().map_err(|e| e.to_string())?;
    // Already in reach (the reach the spots below are picked by): no trip. 3 Nov: a perimeter bot
    // logged in three blocks from its food chest and spent minutes flying to a spot beside it.
    if (from + Vec3::new(0.0, 1.62, 0.0)).distance_to(centre) <= 4.0 {
        return Ok(());
    }
    let near = from.distance_to(centre) <= WAY_CHECKED_WITHIN;
    let mut spots: Vec<(bool, f64, Vec3)> = {
        let world = bot.world().map_err(|e| e.to_string())?;
        let w = world.read();
        let mut all = Vec::new();
        for dy in [1, 2, 0, -1] {
            for dx in -2..=2 {
                for dz in -2..=2 {
                    let feet = Vec3::new(pos.x as f64 + 0.5 + dx as f64, pos.y as f64 + dy as f64, pos.z as f64 + 0.5 + dz as f64);
                    let eye = feet + Vec3::new(0.0, 1.62, 0.0);
                    let d = eye.distance_to(centre);
                    if d <= 4.0 && movement::geom::stand_clear(&w, feet) {
                        all.push((near && !movement::geom::way_clear(&w, from, feet), d, feet));
                    }
                }
            }
        }
        all
    };
    spots.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    if spots.is_empty() {
        return Err("no clear spot within reach of the chest".into());
    }
    let mut last_err = String::new();
    let mut reached = false;
    for (_, _, stand) in spots.iter().take(4) {
        match movement::fly_to(bot, *stand, "chest").await {
            Ok(()) => {
                reached = true;
                break;
            }
            Err(e) => {
                warn!(target: "chests", "could not reach {stand:?} by the chest at {pos}: {e} - trying another spot");
                last_err = e;
            }
        }
    }
    if !reached {
        return Err(last_err);
    }
    Ok(())
}

/// Shift-click the bag stacks `want` picks into the container, `max` stacks at most. Returns the
/// stacks that left the bags (counted, not assumed).
pub async fn store(bot: &Client, pos: BlockPos, want: impl Fn(&ItemStack) -> bool, max: usize) -> Result<usize, String> {
    store_picked(bot, pos, |bags| bags.iter().filter(|(_, s)| want(s)).take(max).map(|(i, _)| *i).collect()).await
}

/// Shift-click into the container the bag stacks `pick` chooses - given the bags as the chest
/// shows them, by their bag slot (9-35 main, 36-44 hotbar), it returns the bag slots - so one of
/// two alike stacks can be chosen (the perimeter's spare tools: 5 Nov). Returns the stacks that
/// left the bags (counted).
pub async fn store_picked(bot: &Client, pos: BlockPos, pick: impl FnOnce(&[(usize, ItemStack)]) -> Vec<usize>) -> Result<usize, String> {
    let c = open(bot, pos).await?;
    let before = filled(bot);
    let menu = c.menu().ok().flatten().ok_or("no menu")?;
    let slots = menu.slots();
    // The container's player part runs main then hotbar, as the bag slots 9-44 do.
    let start = *menu.player_slots_range().start();
    let bags: Vec<(usize, ItemStack)> = menu.player_slots_range().map(|i| (9 + i - start, slots[i].clone())).collect();
    let picks: Vec<usize> = pick(&bags).into_iter().filter(|b| inv::BAGS.contains(b)).map(|b| start + b - 9).collect();
    debug!(target: "chests", "store at {pos}: {} stack(s) picked", picks.len());
    click_paced(&c, &picks).await;
    answered(bot, &c, &picks).await;
    c.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let moved = before.saturating_sub(filled(bot));
    info!(target: "chests", "stored {moved} of {} stack(s) into {pos}", picks.len());
    if moved < picks.len() {
        warn!(target: "chests", "{} stack(s) did not go in - chest full?", picks.len() - moved);
    }
    Ok(moved)
}

/// Shift-click container stacks `want` picks into the bags, best first by `rank` (higher first),
/// `max` stacks at most. Returns the stacks that arrived (counted).
pub async fn take(
    bot: &Client,
    pos: BlockPos,
    want: impl Fn(&ItemStack) -> bool,
    rank: impl Fn(&ItemStack) -> i64,
    max: usize,
) -> Result<usize, String> {
    take_picked(bot, pos, |slots| {
        let mut picks: Vec<usize> = (0..slots.len()).filter(|i| want(&slots[*i])).collect();
        picks.sort_by_key(|i| -rank(&slots[*i]));
        picks.truncate(max);
        picks
    })
    .await
}

/// Shift-click into the bags the container slots `pick` chooses - given the container's own slots,
/// it returns their indexes - as many as there is room for. Returns the stacks that arrived
/// (counted).
pub async fn take_picked(bot: &Client, pos: BlockPos, pick: impl FnOnce(&[ItemStack]) -> Vec<usize>) -> Result<usize, String> {
    let c = open(bot, pos).await?;
    let before = filled(bot);
    let menu = c.menu().ok().flatten().ok_or("no menu")?;
    let slots = menu.slots();
    let first_player = *menu.player_slots_range().start();
    let mut picks: Vec<usize> = pick(&slots[..first_player]).into_iter().filter(|i| *i < first_player).collect();
    let wanted = picks.len();
    picks.truncate(inv::empty_slots(bot));
    debug!(target: "chests", "take at {pos}: {} stack(s) picked of {wanted} wanted", picks.len());
    if picks.len() < wanted {
        info!(target: "chests", "{} stack(s) wanted at {pos}, and no room in the bags for them", wanted - picks.len());
    }
    click_paced(&c, &picks).await;
    answered(bot, &c, &picks).await;
    c.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let got = filled(bot).saturating_sub(before);
    info!(target: "chests", "took {got} of {} stack(s) from {pos}", picks.len());
    Ok(got)
}

/// Take exactly `n` of item `id` - as near as the container holds and the bags have room for -
/// and nothing over (6 Nov, the mapart bot: "only pull out the carpet it needs from the chests not
/// extras so it doesnt clutter inventory"). Whole stacks are shift-clicked while a whole one is
/// still wanted; the last part is split off a stack: picked up, dropped one at a time into a bag
/// slot, the rest put back. Returns how many arrived (counted).
pub async fn take_exact(bot: &Client, pos: BlockPos, id: &str, n: i32) -> Result<i32, String> {
    use azalea::inventory::operations::PickupClick;
    let c = open(bot, pos).await?;
    let before = inv::count_id(bot, id);
    let menu = c.menu().ok().flatten().ok_or("no menu")?;
    let slots = menu.slots();
    let player: Vec<usize> = menu.player_slots_range().collect();
    let first_player = player.first().copied().unwrap_or(slots.len());
    let mut stacks: Vec<(usize, i32)> = (0..first_player).filter(|i| inv::stack_id(&slots[*i]) == id).map(|i| (i, slots[i].count())).collect();
    stacks.sort_by_key(|(_, k)| -k);
    let mut empty: Vec<usize> = player.iter().copied().filter(|i| slots[*i].is_empty()).collect();
    let (mut left, mut whole, mut rest) = (n, Vec::new(), Vec::new());
    for (i, k) in stacks {
        if k <= left && empty.len() > whole.len() + 1 {
            whole.push(i);
            left -= k;
        } else {
            rest.push((i, k));
        }
    }
    // The part: off the smallest stack that has it, into an empty bag slot.
    if left > 0
        && let Some((src, _)) = rest.iter().filter(|(_, k)| *k > left).min_by_key(|(_, k)| *k).copied()
        && let Some(to) = empty.pop()
    {
        debug!(target: "chests", "take at {pos}: {left} {id} split off a stack");
        c.click(PickupClick::Left { slot: Some(src as u16) });
        tokio::time::sleep(Duration::from_millis(50)).await;
        for k in 0..left {
            c.click(PickupClick::Right { slot: Some(to as u16) });
            if (k as usize + 1) % CLICKS_PER_TICK == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        c.click(PickupClick::Left { slot: Some(src as u16) });
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    click_paced(&c, &whole).await;
    answered(bot, &c, &whole).await;
    c.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let got = inv::count_id(bot, id) - before;
    info!(target: "chests", "took {got} of {n} {id} from {pos}");
    Ok(got.max(0))
}

/// Take stacks of each item until `needs` (item id -> count) is met or the bags are nearly full,
/// fullest stacks first; also what the container held when it was opened (item id -> count, less
/// what was taken) - the swarm printer remembers it so the next bot goes where the items are.
pub async fn take_amounts(bot: &Client, pos: BlockPos, needs: &HashMap<String, i32>) -> Result<(usize, HashMap<String, i32>), String> {
    let c = open(bot, pos).await?;
    let before = filled(bot);
    let menu = c.menu().ok().flatten().ok_or("no menu")?;
    let slots = menu.slots();
    let first_player = *menu.player_slots_range().start();
    let mut contents: HashMap<String, i32> = HashMap::new();
    for s in slots.iter().take(first_player).filter(|s| !s.is_empty()) {
        *contents.entry(inv::stack_id(s).to_string()).or_default() += s.count();
    }
    let mut order: Vec<usize> = (0..first_player).filter(|i| needs.contains_key(inv::stack_id(&slots[*i]))).collect();
    order.sort_by_key(|i| -slots[*i].count());
    let room = inv::empty_slots(bot).saturating_sub(1);
    let mut left = needs.clone();
    let mut picks: Vec<usize> = Vec::new();
    for i in order {
        if picks.len() >= room {
            break;
        }
        let id = inv::stack_id(&slots[i]);
        let Some(n) = left.get_mut(id) else { continue };
        if *n <= 0 {
            continue;
        }
        *n -= slots[i].count();
        picks.push(i);
        if let Some(e) = contents.get_mut(id) {
            *e -= slots[i].count();
        }
    }
    debug!(target: "chests", "take at {pos}: {} stack(s) picked; it held {} kinds", picks.len(), contents.len());
    click_paced(&c, &picks).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let got = filled(bot).saturating_sub(before);
    info!(target: "chests", "took {got} of {} stack(s) from {pos}", picks.len());
    Ok((got, contents))
}

/// Bag slots with something in them. Counted with the container open (its player part is the
/// server's word) and again after the close: counted before the trip there, the bags were what the
/// bot last thought - 6 Nov: "took 4 of 0 stack(s)" and "stored 0 of 1" for slots refilled on the
/// way that the bot still thought empty.
fn filled(bot: &Client) -> usize {
    inv::bags(bot).iter().filter(|(_, s)| !s.is_empty()).count()
}

/// The chest stays open after the clicks until the server has had its say: at least a round trip
/// (the server writes only when the client's guess at a click was wrong), then until every clicked
/// slot is empty, 2 s at most. azalea drops the server's word on a chest already closed, so a close
/// before it came left the bags showing what had gone (5 Nov: "1 stack(s) did not go in" for 44 of
/// the perimeter's worn tools, closed 300 ms after the click, on a lagging server - and the next
/// trip did not find them in the bags).
async fn answered(bot: &Client, c: &azalea::container::ContainerHandle, picks: &[usize]) {
    let ping = bot.tab_list().ok().and_then(|t| t.get(&bot.uuid()).map(|i| i.latency)).unwrap_or(150).clamp(0, 1000) as u64;
    tokio::time::sleep(Duration::from_millis((2 * ping + 150).max(300))).await;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        let done = c.menu().ok().flatten().is_some_and(|m| picks.iter().all(|i| m.slots().get(*i).is_none_or(|s| s.is_empty())));
        if done {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn click_paced(c: &azalea::container::ContainerHandle, slots: &[usize]) {
    for chunk in slots.chunks(CLICKS_PER_TICK) {
        for s in chunk {
            c.shift_click(*s);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
