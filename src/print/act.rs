//! Sending a chosen click: the look first when the placement depends on it, a sneak around it when
//! the placement needs one (pairing a chest, hanging a sign under another), and the click itself
//! from the OFFHAND - the server drops main-hand clicks on empty air, the offhand's go through.
//!
//! Every packet goes straight onto the connection, in order. The look is written that way
//! (movement::look_now), and a click sent through azalea's event queue went out later in the
//! frame: with several clicks in a tick the server got every look first and every click after -
//! all judged with the last look (16 Oct e2e test: three stairs all faced north).

use azalea::{
    BlockPos, Client, Vec3,
    connection::RawConnection,
    entity::LookDirection,
    interact::BlockStatePredictionHandler,
    protocol::packets::game::{
        ServerboundGamePacket, ServerboundPlayerInput,
        s_interact::InteractionHand,
        s_use_item::ServerboundUseItem,
        s_use_item_on::{BlockHit, ServerboundUseItemOn},
    },
};
use azalea::protocol::packets::Packet;

use super::place::{Choice, Dir};
use crate::movement;

/// A packet straight onto the connection, after whatever was written before it.
fn write(bot: &Client, p: ServerboundGamePacket) {
    let _ = bot.query_self::<&mut RawConnection, _>(|mut c| {
        let _ = c.write(p);
    });
}

fn sneak(bot: &Client, shift: bool) {
    write(bot, ServerboundPlayerInput { forward: false, backward: false, left: false, right: false, jump: false, shift, sprint: false }.into_variant());
}

fn use_item_on(bot: &Client, hand: InteractionHand, pos: BlockPos, face: Dir, hit: Vec3) {
    let seq = bot.query_self::<&mut BlockStatePredictionHandler, _>(|mut h| h.start_predicting()).unwrap_or(0);
    write(
        bot,
        ServerboundUseItemOn { hand, block_hit: BlockHit { block_pos: pos, direction: face.to_azalea(), location: hit, inside: false, world_border: false }, seq }
            .into_variant(),
    );
}

/// Send the click (the item must already be in the offhand).
pub fn click(bot: &Client, c: &Choice) {
    if c.needs_look {
        movement::look_now(bot, c.click.yaw, c.click.pitch);
    }
    if c.click.sneak {
        sneak(bot, true);
    }
    use_item_on(bot, InteractionHand::OffHand, c.click.clicked, c.click.face, c.click.hit);
    if c.click.sneak {
        sneak(bot, false);
    }
}

/// A main-hand right click on a block's face (its own use: a state fix, a hoe or shovel).
pub fn use_main(bot: &Client, pos: BlockPos, face: Dir, hit: Vec3) {
    use_item_on(bot, InteractionHand::MainHand, pos, face, hit);
}

/// A main-hand right click at no block - the held item's own use (armor in the hand goes on).
pub fn use_item_main(bot: &Client) {
    let seq = bot.query_self::<&mut BlockStatePredictionHandler, _>(|mut h| h.start_predicting()).unwrap_or(0);
    let (y_rot, x_rot) = bot.query_self::<&LookDirection, _>(|l| (l.y_rot(), l.x_rot())).unwrap_or((0.0, 0.0));
    write(bot, ServerboundUseItem { hand: InteractionHand::MainHand, seq, y_rot, x_rot }.into_variant());
}
