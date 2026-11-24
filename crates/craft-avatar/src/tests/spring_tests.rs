use super::close;
use crate::*;

fn at(x: f32, y: f32, z: f32) -> Transform {
    Transform { translation: Vec3::new(x, y, z), ..Transform::IDENTITY }
}

/// A root with a two-joint chain sticking out along +X: joint `a` at the origin, its tail `b`
/// 0.2 to the side, and the chain's end `c` 0.2 further.
fn chain(stiffness: f32, gravity: f32, drag: f32) -> Model {
    let mut m = Model::default();
    m.nodes.push(Node::new("root", None, Transform::IDENTITY));
    m.nodes.push(Node::new("a", Some(0), Transform::IDENTITY));
    m.nodes.push(Node::new("b", Some(1), at(0.2, 0.0, 0.0)));
    m.nodes.push(Node::new("c", Some(2), at(0.2, 0.0, 0.0)));
    let joints = (1..=3).map(|n| SpringJoint { stiffness, gravity_power: gravity, drag, ..SpringJoint::new(n) }).collect();
    m.springs.springs.push(Spring { name: "tail".into(), joints, collider_groups: Vec::new() });
    m
}

fn run(m: &Model, seconds: f64, fps: f64) -> (SpringSim, Vec<Transform>) {
    let mut sim = SpringSim::new(m);
    let mut local = m.rest_local();
    let frames = (seconds * fps).round() as i64;
    for _ in 0..frames {
        sim.step(m, &mut local, 1.0 / fps);
    }
    (sim, local)
}

fn tip(m: &Model, local: &[Transform]) -> Vec3 {
    let mut world = vec![Transform::IDENTITY; m.nodes.len()];
    m.world_into(local, &mut world);
    world[3].translation
}

#[test]
fn a_chain_with_no_stiffness_hangs_straight_down() {
    let m = chain(0.0, 1.0, 0.4);
    m.validate(&Limits::default()).unwrap();
    let (sim, local) = run(&m, 10.0, 60.0);
    let end = tip(&m, &local);
    assert!(close(end.x, 0.0, 0.01) && close(end.y, -0.4, 0.01), "the chain ends at {end:?}");
    // Every tail stays at its rest length from its joint.
    let tails: Vec<Vec3> = sim.tails().collect();
    assert!(close(tails[0].length(), 0.2, 1e-4));
    assert!(close((tails[1] - tails[0]).length(), 0.2, 1e-3));
}

#[test]
fn stiffness_holds_the_chain_up_against_gravity() {
    let (_, soft) = run(&chain(0.2, 1.0, 0.4), 10.0, 60.0);
    let (_, stiff) = run(&chain(4.0, 1.0, 0.4), 10.0, 60.0);
    let m = chain(0.0, 0.0, 0.0);
    let (s, h) = (tip(&m, &soft), tip(&m, &stiff));
    assert!(s.y < h.y && h.y < 0.0, "soft {s:?} stiff {h:?}");
    assert!(h.x > 0.2, "the stiff chain still points sideways: {h:?}");
}

#[test]
fn a_pushed_chain_returns_to_rest() {
    let m = chain(1.0, 0.0, 0.4);
    let mut sim = SpringSim::new(&m);
    let mut local = m.rest_local();
    // Swing the root a quarter turn and back: the chain is left moving.
    local[0].rotation = Quat::from_axis_angle(Vec3::Z, 1.0);
    sim.step(&m, &mut local, 0.1);
    local[0].rotation = Quat::IDENTITY;
    for _ in 0..600 {
        sim.step(&m, &mut local, 1.0 / 60.0);
    }
    let end = tip(&m, &local);
    assert!(close(end.x, 0.4, 0.01) && close(end.y, 0.0, 0.01), "{end:?}");
}

#[test]
fn the_motion_is_the_same_at_any_frame_rate() {
    let m = chain(0.5, 1.0, 0.3);
    let (_, a) = run(&m, 0.5, 30.0);
    let (_, b) = run(&m, 0.5, 60.0);
    let (_, c) = run(&m, 0.5, 120.0);
    let (ta, tb, tc) = (tip(&m, &a), tip(&m, &b), tip(&m, &c));
    assert!((ta - tb).length() < 1e-5 && (tb - tc).length() < 1e-5, "{ta:?} {tb:?} {tc:?}");
    // One long gap runs at most MAX_STEPS steps.
    let mut sim = SpringSim::new(&m);
    let mut local = m.rest_local();
    sim.step(&m, &mut local, 1_000.0);
    let (_, eight) = run(&m, f64::from(MAX_STEPS) / 60.0, 60.0);
    assert!((tip(&m, &local) - tip(&m, &eight)).length() < 1e-5);
    // A step that is negative or not a number does nothing.
    let mut still = m.rest_local();
    for dt in [-1.0, f64::NAN, f64::INFINITY, 0.0] {
        sim.step(&m, &mut still, dt);
    }
    assert_eq!(still, m.rest_local());
}

#[test]
fn colliders_keep_the_chain_outside() {
    let mut m = chain(0.0, 1.0, 0.4);
    // A ball beside the point the chain would hang from.
    m.springs.colliders.push(Collider { node: 0, shape: ColliderShape::Sphere { offset: Vec3::new(0.08, -0.2, 0.0), radius: 0.1 } });
    m.springs.groups.push(ColliderGroup { name: "ball".into(), colliders: vec![0] });
    m.springs.springs[0].collider_groups.push(0);
    for j in &mut m.springs.springs[0].joints {
        j.hit_radius = 0.02;
    }
    m.validate(&Limits::default()).unwrap();
    let (sim, _) = run(&m, 10.0, 60.0);
    let center = Vec3::new(0.08, -0.2, 0.0);
    for t in sim.tails() {
        assert!((t - center).length() >= 0.1 + 0.02 - 2e-3, "a tail at {t:?} is inside the ball");
    }
    let (free, _) = run(&chain(0.0, 1.0, 0.4), 10.0, 60.0);
    let first_free = free.tails().next().unwrap();
    let first = sim.tails().next().unwrap();
    assert!((first - first_free).length() > 0.01, "the ball changed nothing");

    // A capsule lying across the chain works the same way.
    m.springs.colliders[0].shape = ColliderShape::Capsule { offset: Vec3::new(0.08, -0.2, -0.5), tail: Vec3::new(0.08, -0.2, 0.5), radius: 0.1 };
    let (sim, _) = run(&m, 10.0, 60.0);
    for t in sim.tails() {
        let d = Vec3::new(t.x - 0.08, t.y + 0.2, 0.0).length();
        assert!(d >= 0.12 - 2e-3, "a tail at {t:?} is inside the capsule");
    }
}

#[test]
fn a_tail_at_a_colliders_center_is_pushed_out() {
    let mut m = chain(0.0, 0.0, 1.0);
    // A ball centered on the first tail: with no force the tail would never leave it.
    m.springs.colliders.push(Collider { node: 0, shape: ColliderShape::Sphere { offset: Vec3::new(0.2, 0.0, 0.0), radius: 0.05 } });
    m.springs.groups.push(ColliderGroup { name: "ball".into(), colliders: vec![0] });
    m.springs.springs[0].collider_groups.push(0);
    let (sim, _) = run(&m, 1.0, 60.0);
    let first = sim.tails().next().unwrap();
    assert!((first - Vec3::new(0.2, 0.0, 0.0)).length() >= 0.05 - 2e-3, "the tail stayed in the ball at {first:?}");
    assert!(sim.tails().all(Vec3::is_finite));
}

#[test]
fn a_collider_along_the_part_turns_the_tail_out() {
    let mut m = chain(1.0, 0.0, 0.5);
    // The ball covers the first tail and lies on the part's own line.
    m.springs.colliders.push(Collider { node: 0, shape: ColliderShape::Sphere { offset: Vec3::new(0.1, 0.0, 0.0), radius: 0.15 } });
    m.springs.groups.push(ColliderGroup { name: "ball".into(), colliders: vec![0] });
    m.springs.springs[0].collider_groups.push(0);
    let (sim, _) = run(&m, 1.0, 60.0);
    let first = sim.tails().next().unwrap();
    assert!((first - Vec3::new(0.1, 0.0, 0.0)).length() >= 0.15 - 2e-3, "the tail stayed in the ball at {first:?}");
    assert!(close(first.length(), 0.2, 1e-4));
}

#[test]
fn an_unevenly_scaled_collider_uses_its_largest_axis() {
    let mut m = chain(0.0, 1.0, 0.4);
    m.nodes[0].rest.scale = Vec3::new(1.0, 2.0, 1.0);
    // Under the root's scale the ball's center is (0.08, -0.4, 0) and its radius at least 0.2.
    m.springs.colliders.push(Collider { node: 0, shape: ColliderShape::Sphere { offset: Vec3::new(0.08, -0.2, 0.0), radius: 0.1 } });
    m.springs.groups.push(ColliderGroup { name: "ball".into(), colliders: vec![0] });
    m.springs.springs[0].collider_groups.push(0);
    let center = Vec3::new(0.08, -0.4, 0.0);
    let mut sim = SpringSim::new(&m);
    let mut local = m.rest_local();
    for frame in 0..600 {
        sim.step(&m, &mut local, 1.0 / 60.0);
        for t in sim.tails() {
            assert!((t - center).length() >= 0.2 - 2e-3, "a tail at {t:?} is inside the ball at frame {frame}");
        }
    }
}

#[test]
fn the_drawn_tail_follows_the_simulated_tail_under_an_uneven_scale() {
    // Stiffness along +X and gravity down leave the tail on a slant, where the scale matters.
    let mut m = chain(1.0, 1.0, 0.4);
    m.nodes[0].rest.scale = Vec3::new(2.0, 1.0, 1.0);
    m.springs.springs[0].joints.truncate(2);
    let (sim, local) = run(&m, 10.0, 60.0);
    let first = sim.tails().next().unwrap();
    assert!(first.x.abs() > 0.05 && first.y < -0.05, "the tail is not on a slant: {first:?}");
    let mut world = vec![Transform::IDENTITY; m.nodes.len()];
    m.world_into(&local, &mut world);
    let tail = sim.tails().next().unwrap();
    assert!((world[2].translation - tail).length() < 1e-3, "drawn {:?} simulated {tail:?}", world[2].translation);
}

#[test]
fn too_many_spring_chains_are_refused() {
    let mut m = chain(1.0, 1.0, 0.5);
    for i in 0..3 {
        m.springs.springs.push(Spring { name: format!("empty{i}"), joints: Vec::new(), collider_groups: Vec::new() });
    }
    assert!(m.validate(&Limits::default()).is_ok());
    assert!(matches!(m.validate(&Limits { springs: 3, ..Limits::default() }), Err(AvatarError::TooLarge { what: "spring chains", .. })));
}

#[test]
fn the_hair_of_the_test_humanoid_falls_behind_the_head() {
    let m = super::humanoid();
    let (sim, _) = run(&m, 5.0, 60.0);
    let head = m.rest_world()[super::node(&m, "head")].translation;
    for t in sim.tails() {
        assert!(t.is_finite());
        assert!(t.z < head.z, "hair at {t:?} swung in front of the head");
    }
}

#[test]
fn bad_spring_settings_are_refused() {
    let ok = chain(1.0, 1.0, 0.5);
    ok.validate(&Limits::default()).unwrap();
    let bad: [fn(&mut Model); 8] = [
        |m| m.springs.springs[0].joints[1].node = 0,
        |m| m.springs.springs[0].joints[0].drag = 1.5,
        |m| m.springs.springs[0].joints[0].stiffness = -1.0,
        |m| m.springs.springs[0].joints[0].gravity_dir = Vec3::new(f32::NAN, 0.0, 0.0),
        |m| m.springs.springs[0].joints[2].node = 9,
        |m| m.springs.springs[0].collider_groups.push(3),
        |m| m.springs.colliders.push(Collider { node: 0, shape: ColliderShape::Sphere { offset: Vec3::ZERO, radius: -0.1 } }),
        |m| {
            let again = m.springs.springs[0].clone();
            m.springs.springs.push(again);
        },
    ];
    for (i, change) in bad.iter().enumerate() {
        let mut m = chain(1.0, 1.0, 0.5);
        change(&mut m);
        assert!(m.validate(&Limits::default()).is_err(), "case {i} was accepted");
    }
    assert!(matches!(ok.validate(&Limits { spring_joints: 2, ..Limits::default() }), Err(AvatarError::TooLarge { .. })));
}

#[test]
fn a_joint_with_no_length_stays_still() {
    let mut m = chain(0.0, 1.0, 0.0);
    m.nodes[2].rest = Transform::IDENTITY;
    let (_, local) = run(&m, 1.0, 60.0);
    assert_eq!(local[1], m.nodes[1].rest);
    assert!(local.iter().all(Transform::is_finite));
}
