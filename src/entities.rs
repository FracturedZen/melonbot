//! The entities around us, read under ONE lock of the ECS. azalea's nearest_entities_by holds a
//! read guard on our WorldName while it takes a second read for our Position; when the game loop
//! queues a write between the two, both wait forever (15 Oct: the bot froze 2 minutes into a print,
//! the deadlock watchdog's backtrace ended in kill-aura -> nearest_entities_by). Everything that
//! looks at entities goes through here instead.

use azalea::{
    Client, Vec3,
    core::entity_id::MinecraftEntityId,
    ecs::query::{Has, Without},
    entity::{Dead, EntityKindComponent, EntityUuid, LocalEntity, Position, metadata::{AbstractAgeableBaby, ItemItem, Player, VillagerVillagerData}},
    registry::builtin::{EntityKind, ItemKind, VillagerProfession},
    world::WorldName,
};
use azalea::ecs::entity::Entity;

#[derive(Clone, Debug)]
pub struct Near {
    pub entity: Entity,
    pub id: i32,
    pub kind: EntityKind,
    pub pos: Vec3,
    /// The item, for a dropped-item entity.
    pub item: Option<ItemKind>,
    pub player: bool,
    pub dead: bool,
    pub uuid: Option<uuid::Uuid>,
    /// A villager's job, once its data has been seen.
    pub profession: Option<VillagerProfession>,
    /// A baby mob (a baby villager has no trades).
    pub baby: bool,
}

/// Every other entity in our world, nearest first, with how far each is from us.
pub fn near(bot: &Client) -> Vec<(f64, Near)> {
    let mut ecs = bot.ecs.write();
    let (Some(world), Some(me)) = (ecs.get::<WorldName>(bot.entity).cloned(), ecs.get::<Position>(bot.entity).map(|p| **p)) else {
        return Vec::new();
    };
    let mut q = ecs.query_filtered::<(
        Entity,
        &WorldName,
        &Position,
        &EntityKindComponent,
        Option<&ItemItem>,
        Option<&MinecraftEntityId>,
        Has<Player>,
        Has<Dead>,
        Option<&EntityUuid>,
        Option<&VillagerVillagerData>,
        Option<&AbstractAgeableBaby>,
    ), Without<LocalEntity>>();
    let mut out: Vec<(f64, Near)> = q
        .iter(&ecs)
        .filter(|(_, w, ..)| **w == world)
        .map(|(e, _, pos, kind, item, id, player, dead, uuid, villager, baby)| {
            let pos = **pos;
            (
                pos.distance_to(me),
                Near {
                    entity: e,
                    id: id.map_or(-1, |i| i.0),
                    kind: **kind,
                    pos,
                    item: item.and_then(|i| (!i.0.is_empty()).then(|| i.0.kind())),
                    player,
                    dead,
                    uuid: uuid.map(|u| **u),
                    profession: villager.map(|v| v.0.profession),
                    baby: baby.is_some_and(|b| b.0),
                },
            )
        })
        .collect();
    drop(q);
    drop(ecs);
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}
