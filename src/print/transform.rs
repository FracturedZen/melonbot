//! Rotation and mirror, for positions (Litematica's PositionUtils.getTransformedBlockPos, read from
//! the 0.28.3 bytecode) and block states (vanilla Block.rotate / Block.mirror, by property name:
//! facing, axis, the 16-step rotation, orientation, the four side connections, door hinges).

use azalea::block::BlockState;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Ccw90,
}

impl Rotation {
    /// Litematica's / vanilla's enum names.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "CLOCKWISE_90" => Self::Cw90,
            "CLOCKWISE_180" => Self::Cw180,
            "COUNTERCLOCKWISE_90" => Self::Ccw90,
            _ => Self::None,
        }
    }

    /// Quarter turns clockwise.
    pub fn steps(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Cw90 => 1,
            Self::Cw180 => 2,
            Self::Ccw90 => 3,
        }
    }

    fn from_steps(n: i32) -> Self {
        match n.rem_euclid(4) {
            1 => Self::Cw90,
            2 => Self::Cw180,
            3 => Self::Ccw90,
            _ => Self::None,
        }
    }

    /// Rotation.getRotated: this, then `other`.
    pub fn then(self, other: Rotation) -> Rotation {
        Self::from_steps(self.steps() + other.steps())
    }

    pub fn is_quarter(self) -> bool {
        matches!(self, Self::Cw90 | Self::Ccw90)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mirror {
    #[default]
    None,
    /// Flips z (north <-> south).
    LeftRight,
    /// Flips x (east <-> west).
    FrontBack,
}

impl Mirror {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "LEFT_RIGHT" => Self::LeftRight,
            "FRONT_BACK" => Self::FrontBack,
            _ => Self::None,
        }
    }

    /// The other axis' mirror (Litematica swaps a sub-region's mirror under a quarter turn).
    pub fn swapped(self) -> Mirror {
        match self {
            Self::LeftRight => Self::FrontBack,
            Self::FrontBack => Self::LeftRight,
            Self::None => Self::None,
        }
    }
}

/// PositionUtils.getTransformedBlockPos: mirror, then rotate about the origin.
pub fn pos(p: (i32, i32, i32), m: Mirror, r: Rotation) -> (i32, i32, i32) {
    let (mut x, y, mut z) = p;
    match m {
        Mirror::LeftRight => z = -z,
        Mirror::FrontBack => x = -x,
        Mirror::None => {}
    }
    match r {
        Rotation::Cw90 => (-z, y, x),
        Rotation::Ccw90 => (z, y, -x),
        Rotation::Cw180 => (-x, y, -z),
        Rotation::None => (x, y, z),
    }
}

// ------------------------------------------------------------------ directions by name

const HORIZONTAL: [&str; 4] = ["north", "east", "south", "west"];

fn rotate_dir(d: &str, r: Rotation) -> String {
    match HORIZONTAL.iter().position(|h| *h == d) {
        Some(i) => HORIZONTAL[(i as i32 + r.steps()).rem_euclid(4) as usize].to_string(),
        None => d.to_string(),
    }
}

fn mirror_dir(d: &str, m: Mirror) -> String {
    match (m, d) {
        (Mirror::LeftRight, "north") => "south".into(),
        (Mirror::LeftRight, "south") => "north".into(),
        (Mirror::FrontBack, "east") => "west".into(),
        (Mirror::FrontBack, "west") => "east".into(),
        _ => d.to_string(),
    }
}

/// A FrontAndTop value ("north_up", "up_east"): both halves turned.
fn map_front_top(v: &str, f: impl Fn(&str) -> String) -> String {
    match v.split_once('_') {
        Some((a, b)) => format!("{}_{}", f(a), f(b)),
        None => v.to_string(),
    }
}

/// Set properties on a state by name (unknown ones are ignored, as the schematic reader does).
fn with(state: BlockState, changes: &[(&str, String)]) -> BlockState {
    if changes.is_empty() {
        return state;
    }
    let mut b = state.to_trait().boxed();
    for (k, v) in changes {
        let _ = b.set_property(k, v);
    }
    b.as_block_state()
}

/// Block.rotate, for every property a rotation moves.
pub fn rotate_state(state: BlockState, r: Rotation) -> BlockState {
    if r == Rotation::None {
        return state;
    }
    let t = state.to_trait();
    let props = t.property_map();
    let mut changes: Vec<(&str, String)> = Vec::new();
    for (k, v) in &props {
        match *k {
            "facing" => changes.push(("facing", rotate_dir(v, r))),
            "axis" if r.is_quarter() => changes.push(("axis", match *v {
                "x" => "z".into(),
                "z" => "x".into(),
                other => other.to_string(),
            })),
            "rotation" => {
                if let Ok(n) = v.parse::<i32>() {
                    changes.push(("rotation", ((n + r.steps() * 4).rem_euclid(16)).to_string()));
                }
            }
            "orientation" => changes.push(("orientation", map_front_top(v, |d| rotate_dir(d, r)))),
            _ => {}
        }
    }
    // The four side connections move round with the block: the new north is the old west after
    // a clockwise quarter turn.
    if HORIZONTAL.iter().all(|h| props.contains_key(h)) {
        for (i, h) in HORIZONTAL.iter().enumerate() {
            let from = HORIZONTAL[(i as i32 - r.steps()).rem_euclid(4) as usize];
            changes.push((h, props[from].to_string()));
        }
    }
    with(state, &changes)
}

/// Block.mirror, for every property a mirror moves (doors also swap their hinge).
pub fn mirror_state(state: BlockState, m: Mirror) -> BlockState {
    if m == Mirror::None {
        return state;
    }
    let t = state.to_trait();
    let props = t.property_map();
    let mut changes: Vec<(&str, String)> = Vec::new();
    for (k, v) in &props {
        match *k {
            "facing" => changes.push(("facing", mirror_dir(v, m))),
            "rotation" => {
                // Mirror.mirror(rotation, 16) (26.2 bytecode): LEFT_RIGHT (8 - r), FRONT_BACK (16 - r).
                // 0 faces south, 4 west: flipping z sends south to north (8), west stays.
                if let Ok(n) = v.parse::<i32>() {
                    let out = match m {
                        Mirror::LeftRight => (8 - n).rem_euclid(16),
                        _ => (16 - n).rem_euclid(16),
                    };
                    changes.push(("rotation", out.to_string()));
                }
            }
            "orientation" => changes.push(("orientation", map_front_top(v, |d| mirror_dir(d, m)))),
            // DoorBlock.mirror cycles the hinge whatever the axis.
            "hinge" => changes.push(("hinge", if *v == "left" { "right".into() } else { "left".into() })),
            _ => {}
        }
    }
    let (a, b) = match m {
        Mirror::LeftRight => ("north", "south"),
        _ => ("east", "west"),
    };
    if let (Some(va), Some(vb)) = (props.get(a), props.get(b))
        && HORIZONTAL.iter().all(|h| props.contains_key(h))
    {
        changes.push((a, vb.to_string()));
        changes.push((b, va.to_string()));
    }
    with(state, &changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::schem::parse_state;

    #[test]
    fn positions_turn_like_litematica() {
        assert_eq!(pos((1, 2, 3), Mirror::None, Rotation::Cw90), (-3, 2, 1));
        assert_eq!(pos((1, 2, 3), Mirror::None, Rotation::Ccw90), (3, 2, -1));
        assert_eq!(pos((1, 2, 3), Mirror::None, Rotation::Cw180), (-1, 2, -3));
        assert_eq!(pos((1, 2, 3), Mirror::LeftRight, Rotation::None), (1, 2, -3));
        assert_eq!(pos((1, 2, 3), Mirror::FrontBack, Rotation::Cw90), (-3, 2, -1));
        assert_eq!(Rotation::Cw90.then(Rotation::Cw180), Rotation::Ccw90);
    }

    #[test]
    fn states_turn_with_their_blocks() {
        let s = parse_state("oak_stairs[facing=north,half=top]").unwrap();
        assert_eq!(rotate_state(s, Rotation::Cw90).get_property("facing"), Some("east"));
        assert_eq!(rotate_state(s, Rotation::Cw90).get_property("half"), Some("top"));
        let log = parse_state("oak_log[axis=x]").unwrap();
        assert_eq!(rotate_state(log, Rotation::Ccw90).get_property("axis"), Some("z"));
        assert_eq!(rotate_state(log, Rotation::Cw180).get_property("axis"), Some("x"));
        let sign = parse_state("oak_sign[rotation=3]").unwrap();
        assert_eq!(rotate_state(sign, Rotation::Cw90).get_property("rotation"), Some("7"));
        assert_eq!(mirror_state(sign, Mirror::LeftRight).get_property("rotation"), Some("5"));
        assert_eq!(mirror_state(sign, Mirror::FrontBack).get_property("rotation"), Some("13"));
        let fence = parse_state("oak_fence[north=true,east=false,south=false,west=false]").unwrap();
        let turned = rotate_state(fence, Rotation::Cw90);
        assert_eq!((turned.get_property("north"), turned.get_property("east")), (Some("false"), Some("true")));
        let door = parse_state("oak_door[facing=east,hinge=left]").unwrap();
        let m = mirror_state(door, Mirror::FrontBack);
        assert_eq!((m.get_property("facing"), m.get_property("hinge")), (Some("west"), Some("right")));
        let jig = parse_state("jigsaw[orientation=north_up]").unwrap();
        assert_eq!(rotate_state(jig, Rotation::Cw90).get_property("orientation"), Some("east_up"));
        assert_eq!(rotate_state(parse_state("hopper[facing=down]").unwrap(), Rotation::Cw90).get_property("facing"), Some("down"));
    }
}
