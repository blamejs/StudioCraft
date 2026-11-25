//! The pose step and the spring step neither allocate nor free memory once they run. This file
//! holds one test, so no other test in the same process allocates while it counts.

use std::alloc::System;

use craft_avatar::*;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn model() -> Model {
    let mut m = Model::default();
    let at = |y: f32, z: f32| Transform { translation: Vec3::new(0.0, y, z), ..Transform::IDENTITY };
    m.nodes.push(Node::new("head", None, at(1.5, 0.0)));
    m.springs.colliders.push(Collider { node: 0, shape: ColliderShape::Sphere { offset: Vec3::ZERO, radius: 0.1 } });
    m.springs.groups.push(ColliderGroup { name: "head".into(), colliders: vec![0] });
    // Two strands of 50 joints: 100 spring joints.
    for strand in 0..2 {
        let first = m.nodes.len();
        let mut parent = 0;
        for i in 0..50 {
            m.nodes.push(Node::new(&format!("hair{strand}.{i}"), Some(parent), at(0.0, -0.01)));
            parent = m.nodes.len() - 1;
        }
        let joints = (first..m.nodes.len()).map(|n| SpringJoint { gravity_power: 1.0, hit_radius: 0.01, ..SpringJoint::new(n) }).collect();
        m.springs.springs.push(Spring { name: format!("hair{strand}"), joints, collider_groups: vec![0] });
    }
    for p in Preset::ALL {
        m.expressions.push(Expression::new(ExpressionName::Preset(p)));
    }
    for i in 0..42 {
        m.expressions.push(Expression::new(ExpressionName::parse(&format!("custom{i}"))));
    }
    m.expressions[0].override_mouth = Override::Blend;
    m
}

#[test]
fn the_steps_do_not_allocate() {
    let m = model();
    m.validate(&Limits::default()).unwrap();
    let mut animator = Animator::new(&m, AnimatorSettings::default(), 5).unwrap();
    let mut sim = SpringSim::new(&m);
    let mut local = m.rest_local();
    let mut world = vec![Transform::IDENTITY; m.nodes.len()];
    let mut frame = TrackFrame { present: true, ..TrackFrame::default() };
    frame.weights.set(Blendshape::JawOpen, 0.5);
    frame.weights.set(Blendshape::MouthSmileLeft, 0.7);
    let mut step = |i: u32| {
        let t = f64::from(i) / 60.0;
        frame.time = t;
        frame.present = (i / 90).is_multiple_of(2);
        frame.rotation = Quat::from_yaw_pitch_roll((t.sin() * 20.0) as f32, 0.0, 0.0);
        let pose = animator.step(if i.is_multiple_of(2) { Some(&frame) } else { None }, t);
        local[0].rotation = pose.head;
        sim.step(&m, &mut local, 1.0 / 60.0);
        m.world_into(&local, &mut world);
    };
    // Counted from the first step: ten seconds, with the face lost and found every 1.5 seconds.
    let region = Region::new(GLOBAL);
    for i in 0..600 {
        step(i);
    }
    let stats = region.change();
    assert_eq!((stats.allocations, stats.reallocations, stats.deallocations), (0, 0, 0), "{stats:?}");
}
