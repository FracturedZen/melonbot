//! The swarm printer's engine: any schematic, every block state, placed the way the standalone's
//! Printer places it. Schematic files (schem), Litematica placements from the controller instance
//! (litematica), rotation and mirror (transform), each block's vanilla type (block_types), the
//! item that places it (items), and what a click places / the click that places it (place).

pub mod act;
pub mod block_types;
pub mod items;
pub mod litematica;
pub mod place;
pub mod schem;
pub mod transform;
