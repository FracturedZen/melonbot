//! The world border, from the server's packets: azalea reads them and keeps nothing (0.16), so no
//! bot knew where it was - 6 Nov: from the perimeter box's corner at the border, kill-aura struck at
//! pillagers beyond it 26,000 times in a day, the server putting the bot back each time.

use azalea::{
    Client, Vec3,
    app::{App, Plugin, Update},
    ecs::prelude::*,
    entity::LocalEntity,
    packet::game::ReceiveGamePacketEvent,
    protocol::packets::game::ClientboundGamePacket,
};
use azalea::ecs as bevy_ecs; // for #[derive(Component)]

/// Vanilla's border when a server sets none.
const DEFAULT_SIZE: f64 = 59_999_968.0;

#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct WorldBorder {
    pub x: f64,
    pub z: f64,
    /// Its full width; while it moves, the smaller of where it was and where it is going.
    pub size: f64,
}

impl WorldBorder {
    /// `p` is inside it, `margin` blocks clear of its edge.
    pub fn holds(&self, p: Vec3, margin: f64) -> bool {
        let half = self.size / 2.0 - margin;
        (p.x - self.x).abs() <= half && (p.z - self.z).abs() <= half
    }
}

pub struct BorderPlugin;

impl Plugin for BorderPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, record_border);
    }
}

fn record_border(mut events: MessageReader<ReceiveGamePacketEvent>, mut us: Query<Option<&mut WorldBorder>, With<LocalEntity>>, mut commands: Commands) {
    for ev in events.read() {
        let (centre, size) = match ev.packet.as_ref() {
            ClientboundGamePacket::InitializeBorder(p) => (Some((p.new_center_x, p.new_center_z)), Some(p.old_size.min(p.new_size))),
            ClientboundGamePacket::SetBorderCenter(p) => (Some((p.new_center_x, p.new_center_z)), None),
            ClientboundGamePacket::SetBorderSize(p) => (None, Some(p.size)),
            ClientboundGamePacket::SetBorderLerpSize(p) => (None, Some(p.old_size.min(p.new_size))),
            _ => continue,
        };
        let Ok(border) = us.get_mut(ev.entity) else { continue };
        match border {
            Some(mut b) => {
                if let Some((x, z)) = centre {
                    (b.x, b.z) = (x, z);
                }
                if let Some(s) = size {
                    b.size = s;
                }
            }
            None => {
                let (x, z) = centre.unwrap_or((0.0, 0.0));
                commands.entity(ev.entity).insert(WorldBorder { x, z, size: size.unwrap_or(DEFAULT_SIZE) });
            }
        }
    }
}

/// The border, once the server has said where it is.
pub fn get(bot: &Client) -> Option<WorldBorder> {
    bot.query_self::<Option<&WorldBorder>, _>(|b| b.copied()).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_and_outside_a_border() {
        // A box's corner at the border, pillagers past it.
        let b = WorldBorder { x: 0.0, z: 0.0, size: 2_000.0 };
        assert!(b.holds(Vec3::new(-837.4, 70.0, 997.8), 1.0));
        assert!(!b.holds(Vec3::new(-850.0, 68.0, 1010.0), 1.0), "the pillager past it");
        assert!(!b.holds(Vec3::new(-837.4, 70.0, 999.5), 1.0), "half a block in: not a block clear of it");
    }
}
