//! Formations: where each bot of the swarm stands in a shape round a centre, and how the shape
//! moves - set on the control panel: shapes, a sphere, a circle that keeps moving at a set speed.
//!
//! Outline shapes (circle, square, triangle, star, heart, wave) spread the bots evenly along the
//! outline and, with a speed, every bot travels along it at that many blocks a second - the whole
//! shape flows round. Solid shapes (sphere, helix, cube, line) turn about the vertical axis at the
//! same speed measured at the radius. Scatter is random points in a ball, re-drawn every few
//! seconds; Random picks a new shape (and plane) every few seconds.
//!
//! Positions are relative to the centre, in blocks; "upright" stands a flat shape on its edge,
//! facing the way the centre faces (the controller player's yaw).

use std::f64::consts::{PI, TAU};

use azalea::Vec3;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    Circle,
    Sphere,
    Square,
    Triangle,
    Star,
    Heart,
    Wave,
    Helix,
    Cube,
    Line,
    Scatter,
    Random,
}

impl Shape {
    pub const ALL: [Shape; 12] = [
        Shape::Circle,
        Shape::Sphere,
        Shape::Square,
        Shape::Triangle,
        Shape::Star,
        Shape::Heart,
        Shape::Wave,
        Shape::Helix,
        Shape::Cube,
        Shape::Line,
        Shape::Scatter,
        Shape::Random,
    ];

    /// The shapes Random chooses between (everything but itself).
    const PICKS: [Shape; 11] = [
        Shape::Circle,
        Shape::Sphere,
        Shape::Square,
        Shape::Triangle,
        Shape::Star,
        Shape::Heart,
        Shape::Wave,
        Shape::Helix,
        Shape::Cube,
        Shape::Line,
        Shape::Scatter,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Shape::Circle => "circle",
            Shape::Sphere => "sphere",
            Shape::Square => "square",
            Shape::Triangle => "triangle",
            Shape::Star => "star",
            Shape::Heart => "heart",
            Shape::Wave => "wave",
            Shape::Helix => "helix",
            Shape::Cube => "cube",
            Shape::Line => "line",
            Shape::Scatter => "scatter",
            Shape::Random => "random",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Plane {
    /// Lying flat, round the centre.
    Flat,
    /// Standing on its edge, facing the way the centre faces.
    Upright,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Formation {
    pub shape: Shape,
    /// Blocks from the centre to the outline.
    pub radius: f64,
    /// Blocks a second each bot travels along the shape (0 = still).
    pub speed: f64,
    /// Blocks above the centre.
    pub height: f64,
    pub plane: Plane,
    /// Scatter re-draws and Random changes shape this often (seconds).
    pub shuffle_secs: f64,
}

impl Default for Formation {
    fn default() -> Self {
        Self { shape: Shape::Circle, radius: 6.0, speed: 4.0, height: 3.0, plane: Plane::Flat, shuffle_secs: 10.0 }
    }
}

/// Where bot `i` of `n` stands at `t` seconds since the formation started, relative to the centre
/// (x east, y up, z south), with the centre facing `yaw` (degrees, Minecraft: 0 = south).
pub fn slot(f: &Formation, i: usize, n: usize, t: f64, yaw: f32) -> Vec3 {
    let n = n.max(1);
    let r = f.radius.max(0.5);
    let shuffle = f.shuffle_secs.max(1.0);
    let (shape, plane) = match f.shape {
        Shape::Random => {
            let round = (t / shuffle).floor() as u64;
            let pick = Shape::PICKS[(hash(round) % Shape::PICKS.len() as u64) as usize];
            let plane = if hash(round ^ 0x5eed) % 3 == 0 { Plane::Upright } else { f.plane };
            (pick, plane)
        }
        s => (s, f.plane),
    };
    // Along an outline: everyone's share of it plus how far the flow has carried them.
    let u = |len: f64| (i as f64 / n as f64 + f.speed * t / len.max(1e-6)).rem_euclid(1.0);
    // Round the vertical axis, by the speed at the radius.
    let spin = f.speed * t / r;
    let local = match shape {
        Shape::Circle => {
            let a = TAU * u(TAU * r);
            Vec3::new(r * a.cos(), 0.0, r * a.sin())
        }
        Shape::Square => outline(&square(r), u(perimeter(&square(r)))),
        Shape::Triangle => outline(&polygon(3, r), u(perimeter(&polygon(3, r)))),
        Shape::Star => outline(&star(r), u(perimeter(&star(r)))),
        Shape::Heart => {
            let pts = heart(r);
            outline(&pts, u(perimeter(&pts)))
        }
        Shape::Wave => {
            let a = TAU * u(TAU * r);
            Vec3::new(r * a.cos(), (r / 4.0) * (3.0 * a).sin(), r * a.sin())
        }
        Shape::Sphere => rotate_y(fibonacci_sphere(i, n, r), spin),
        Shape::Helix => {
            // Two turns, bottom to top over 2r.
            let k = if n == 1 { 0.5 } else { i as f64 / (n - 1) as f64 };
            let a = TAU * 2.0 * k + spin;
            Vec3::new(r * a.cos(), -r + 2.0 * r * k, r * a.sin())
        }
        Shape::Cube => rotate_y(cube_point(i, n, r), spin),
        Shape::Line => {
            let k = if n == 1 { 0.0 } else { i as f64 / (n - 1) as f64 * 2.0 - 1.0 };
            rotate_y(Vec3::new(k * r, 0.0, 0.0), spin)
        }
        Shape::Scatter => {
            let round = (t / shuffle).floor() as u64;
            random_in_ball(hash(round.wrapping_mul(1_000_003).wrapping_add(i as u64)), r)
        }
        Shape::Random => unreachable!(),
    };
    let placed = match plane {
        // Flat shapes lie in x/z; upright ones turn that plane on its edge (z becomes up), then
        // face the centre's yaw. A sphere/helix/cube is already 3D: upright only faces it.
        Plane::Flat => local,
        Plane::Upright => {
            let up = if matches!(shape, Shape::Sphere | Shape::Helix | Shape::Cube | Shape::Scatter) {
                local
            } else {
                Vec3::new(local.x, -local.z, local.y)
            };
            rotate_y(up, -(yaw as f64).to_radians())
        }
    };
    Vec3::new(placed.x, placed.y + f.height, placed.z)
}

/// A point a fraction `u` (0..1) of the way round a closed outline of corner points (x/z).
fn outline(pts: &[(f64, f64)], u: f64) -> Vec3 {
    let total = perimeter(pts);
    let mut want = u.rem_euclid(1.0) * total;
    for k in 0..pts.len() {
        let (a, b) = (pts[k], pts[(k + 1) % pts.len()]);
        let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        if want <= len || k == pts.len() - 1 {
            let s = if len > 0.0 { (want / len).clamp(0.0, 1.0) } else { 0.0 };
            return Vec3::new(a.0 + (b.0 - a.0) * s, 0.0, a.1 + (b.1 - a.1) * s);
        }
        want -= len;
    }
    Vec3::ZERO
}

fn perimeter(pts: &[(f64, f64)]) -> f64 {
    (0..pts.len())
        .map(|k| {
            let (a, b) = (pts[k], pts[(k + 1) % pts.len()]);
            ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()
        })
        .sum()
}

fn square(r: f64) -> Vec<(f64, f64)> {
    vec![(r, r), (-r, r), (-r, -r), (r, -r)]
}

fn polygon(k: usize, r: f64) -> Vec<(f64, f64)> {
    (0..k).map(|j| {
        let a = TAU * j as f64 / k as f64 - PI / 2.0;
        (r * a.cos(), r * a.sin())
    }).collect()
}

fn star(r: f64) -> Vec<(f64, f64)> {
    (0..10)
        .map(|j| {
            let a = TAU * j as f64 / 10.0 - PI / 2.0;
            let rr = if j % 2 == 0 { r } else { r * 0.42 };
            (rr * a.cos(), rr * a.sin())
        })
        .collect()
}

/// The classic heart curve, scaled to `r`, as 48 points (pointing -z when flat, down when upright).
fn heart(r: f64) -> Vec<(f64, f64)> {
    (0..48)
        .map(|j| {
            let a = TAU * j as f64 / 48.0;
            let x = 16.0 * a.sin().powi(3);
            let y = 13.0 * a.cos() - 5.0 * (2.0 * a).cos() - 2.0 * (3.0 * a).cos() - (4.0 * a).cos();
            (x * r / 17.0, -y * r / 17.0)
        })
        .collect()
}

fn fibonacci_sphere(i: usize, n: usize, r: f64) -> Vec3 {
    if n == 1 {
        return Vec3::new(0.0, r, 0.0);
    }
    let golden = PI * (3.0 - 5f64.sqrt());
    let y = 1.0 - 2.0 * (i as f64 + 0.5) / n as f64;
    let ring = (1.0 - y * y).max(0.0).sqrt();
    let a = golden * i as f64;
    Vec3::new(r * ring * a.cos(), r * y, r * ring * a.sin())
}

/// The 8 corners first, then the 12 edge middles, then round again further in.
fn cube_point(i: usize, n: usize, r: f64) -> Vec3 {
    let _ = n;
    let corners = [(1, 1, 1), (-1, 1, 1), (-1, 1, -1), (1, 1, -1), (1, -1, 1), (-1, -1, 1), (-1, -1, -1), (1, -1, -1)];
    let edges = [
        (0, 1, 1), (-1, 1, 0), (0, 1, -1), (1, 1, 0),
        (0, -1, 1), (-1, -1, 0), (0, -1, -1), (1, -1, 0),
        (1, 0, 1), (-1, 0, 1), (-1, 0, -1), (1, 0, -1),
    ];
    let all: Vec<(i32, i32, i32)> = corners.iter().chain(edges.iter()).copied().collect();
    let (x, y, z) = all[i % all.len()];
    let scale = r / (1.0 + (i / all.len()) as f64);
    Vec3::new(x as f64 * scale, y as f64 * scale, z as f64 * scale)
}

fn random_in_ball(seed: u64, r: f64) -> Vec3 {
    let a = TAU * unit(hash(seed));
    let y = unit(hash(seed ^ 0xa5a5)) * 2.0 - 1.0;
    let d = r * unit(hash(seed ^ 0x5a5a)).cbrt();
    let ring = (1.0 - y * y).max(0.0).sqrt();
    Vec3::new(d * ring * a.cos(), d * y, d * ring * a.sin())
}

fn rotate_y(v: Vec3, a: f64) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x * c - v.z * s, v.y, v.x * s + v.z * c)
}

/// SplitMix64: the same seed gives the same point on every bot, so a scatter agrees swarm-wide.
fn hash(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(shape: Shape) -> Formation {
        Formation { shape, radius: 8.0, speed: 4.0, height: 0.0, plane: Plane::Flat, shuffle_secs: 10.0 }
    }

    #[test]
    fn circle_spreads_bots_evenly_and_flows_at_the_set_speed() {
        let c = f(Shape::Circle);
        for i in 0..4 {
            let p = slot(&c, i, 4, 0.0, 0.0);
            assert!(((p.x * p.x + p.z * p.z).sqrt() - 8.0).abs() < 1e-9);
        }
        let (a, b) = (slot(&c, 0, 4, 0.0, 0.0), slot(&c, 1, 4, 0.0, 0.0));
        // Four on a radius-8 ring: neighbours a quarter turn apart, 8 * sqrt(2) apart.
        let chord = a.distance_to(b);
        assert!((chord - 8.0 * 2f64.sqrt()).abs() < 1e-6, "chord {chord}");
        // One second later each bot is 4 blocks further round (arc), so ~4 blocks away (chord).
        let later = slot(&c, 0, 4, 1.0, 0.0);
        let moved = a.distance_to(later);
        assert!((moved - 4.0).abs() < 0.1, "moved {moved}");
    }

    #[test]
    fn still_when_speed_is_zero() {
        let mut c = f(Shape::Star);
        c.speed = 0.0;
        assert_eq!(slot(&c, 2, 5, 0.0, 0.0), slot(&c, 2, 5, 37.0, 0.0));
    }

    #[test]
    fn outline_shapes_stay_on_their_outline_while_flowing() {
        let s = f(Shape::Square);
        for k in 0..50 {
            let p = slot(&s, 1, 3, k as f64 * 0.37, 0.0);
            let on_edge = (p.x.abs() - 8.0).abs() < 1e-6 || (p.z.abs() - 8.0).abs() < 1e-6;
            assert!(on_edge && p.x.abs() <= 8.0 + 1e-6 && p.z.abs() <= 8.0 + 1e-6, "{p:?}");
        }
    }

    #[test]
    fn sphere_points_are_on_the_sphere_and_distinct() {
        let s = f(Shape::Sphere);
        let pts: Vec<Vec3> = (0..10).map(|i| slot(&s, i, 10, 3.0, 0.0)).collect();
        for p in &pts {
            assert!((p.length() - 8.0).abs() < 1e-6);
        }
        for a in 0..pts.len() {
            for b in a + 1..pts.len() {
                assert!(pts[a].distance_to(pts[b]) > 1.0);
            }
        }
    }

    #[test]
    fn height_lifts_and_upright_stands_a_circle_up() {
        let mut c = f(Shape::Circle);
        c.height = 5.0;
        c.plane = Plane::Upright;
        let ys: Vec<f64> = (0..8).map(|i| slot(&c, i, 8, 0.0, 0.0).y).collect();
        let (lo, hi) = (ys.iter().cloned().fold(f64::MAX, f64::min), ys.iter().cloned().fold(f64::MIN, f64::max));
        assert!(hi - lo > 15.0, "an upright circle spans its diameter vertically ({lo}..{hi})");
        assert!(((hi + lo) / 2.0 - 5.0).abs() < 1.0);
    }

    #[test]
    fn random_and_scatter_agree_between_bots_and_change_with_the_shuffle() {
        let r = f(Shape::Random);
        assert_eq!(slot(&r, 3, 6, 12.0, 0.0), slot(&r, 3, 6, 12.0, 0.0));
        let s = f(Shape::Scatter);
        assert_eq!(slot(&s, 1, 4, 2.0, 0.0), slot(&s, 1, 4, 9.0, 0.0));
        assert_ne!(slot(&s, 1, 4, 2.0, 0.0), slot(&s, 1, 4, 12.0, 0.0));
        for i in 0..4 {
            assert!(slot(&s, i, 4, 5.0, 0.0).length() <= 8.0 + 1e-9);
        }
    }

    #[test]
    fn every_shape_gives_finite_points_for_one_to_twenty_bots() {
        for shape in Shape::ALL {
            let c = f(shape);
            for n in 1..=20 {
                for i in 0..n {
                    let p = slot(&c, i, n, 7.3, 123.0);
                    assert!(p.x.is_finite() && p.y.is_finite() && p.z.is_finite(), "{shape:?} {i}/{n}");
                }
            }
        }
    }
}
