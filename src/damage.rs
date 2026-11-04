//! What hurt us: the server's damage packets for our own entity, so every health drop in the log
//! names its type (fall, in_wall, mob_attack...) instead of being guessed at. The Java side's
//! death recorder ("last hits: starve at health 3...") is the same idea.

use azalea::{
    Client,
    app::{App, Plugin, Update},
    core::entity_id::MinecraftEntityId,
    ecs::prelude::*,
    entity::LocalEntity,
    packet::game::ReceiveGamePacketEvent,
    protocol::packets::game::ClientboundGamePacket,
    registry::{DataRegistry, data::DamageKind},
};
use azalea::ecs as bevy_ecs; // for #[derive(Component)]

pub struct DamagePlugin;

impl Plugin for DamagePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, record_damage);
    }
}

/// Damage type ids received since the handler last looked.
#[derive(Component, Default)]
pub struct Hits(pub Vec<u32>);

fn record_damage(
    mut events: MessageReader<ReceiveGamePacketEvent>,
    mut us: Query<(&MinecraftEntityId, Option<&mut Hits>), With<LocalEntity>>,
    mut commands: Commands,
) {
    for ev in events.read() {
        let ClientboundGamePacket::DamageEvent(p) = ev.packet.as_ref() else { continue };
        let Ok((id, hits)) = us.get_mut(ev.entity) else { continue };
        if p.entity_id != *id {
            continue;
        }
        match hits {
            Some(mut h) => h.0.push(p.source_type_id),
            None => {
                commands.entity(ev.entity).insert(Hits(vec![p.source_type_id]));
            }
        }
    }
}

/// The damage types since the last call, by name ("fall", "in_wall", ...).
pub fn take_hits(bot: &Client) -> Vec<String> {
    let ids = bot.query_self::<Option<&mut Hits>, _>(|h| h.map(|mut h| std::mem::take(&mut h.0))).ok().flatten().unwrap_or_default();
    ids.into_iter()
        .map(|id| {
            bot.resolve_registry_key(&DamageKind::new_raw(id))
                .ok()
                .flatten()
                .map(|k| azalea::Identifier::from(k).path().to_string())
                .unwrap_or_else(|| format!("type {id}"))
        })
        .collect()
}
