mod animator_tests;
mod calibration_tests;
mod expression_tests;
mod idle_tests;
mod look_at_tests;
mod mapping_tests;
mod model_tests;
mod presence_tests;
mod spring_tests;
mod vocab_tests;

use crate::*;

pub(crate) fn close(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

fn at(x: f32, y: f32, z: f32) -> Transform {
    Transform { translation: Vec3::new(x, y, z), ..Transform::IDENTITY }
}

/// A small humanoid: every required bone, a neck, two eyes, a three-joint hair chain hanging
/// back from the head, one face mesh with two morph targets on node `face`, and the expressions
/// `blink`, `aa` and `happy`.
pub(crate) fn humanoid() -> Model {
    let mut m = Model::default();
    let mut add = |name: &str, parent: Option<usize>, t: Transform| {
        m.nodes.push(Node::new(name, parent, t));
        m.nodes.len() - 1
    };
    let hips = add("hips", None, at(0.0, 1.0, 0.0));
    let spine = add("spine", Some(hips), at(0.0, 0.1, 0.0));
    let neck = add("neck", Some(spine), at(0.0, 0.4, 0.0));
    let head = add("head", Some(neck), at(0.0, 0.1, 0.0));
    let left_eye = add("leftEye", Some(head), at(0.03, 0.06, 0.08));
    let right_eye = add("rightEye", Some(head), at(-0.03, 0.06, 0.08));
    let mut limbs = Vec::new();
    for (side, x) in [("left", 1.0f32), ("right", -1.0)] {
        let ul = add(&format!("{side}UpperLeg"), Some(hips), at(0.1 * x, -0.05, 0.0));
        let ll = add(&format!("{side}LowerLeg"), Some(ul), at(0.0, -0.45, 0.0));
        let ft = add(&format!("{side}Foot"), Some(ll), at(0.0, -0.45, 0.0));
        let ua = add(&format!("{side}UpperArm"), Some(spine), at(0.2 * x, 0.35, 0.0));
        let la = add(&format!("{side}LowerArm"), Some(ua), at(0.25 * x, 0.0, 0.0));
        let hd = add(&format!("{side}Hand"), Some(la), at(0.25 * x, 0.0, 0.0));
        limbs.push((side, [ul, ll, ft, ua, la, hd]));
    }
    let hair0 = add("hair0", Some(head), at(0.0, 0.1, -0.1));
    let hair1 = add("hair1", Some(hair0), at(0.0, 0.0, -0.1));
    let hair2 = add("hair2", Some(hair1), at(0.0, 0.0, -0.1));
    let face = add("face", Some(head), Transform::IDENTITY);
    for (b, n) in [
        (HumanBone::Hips, hips),
        (HumanBone::Spine, spine),
        (HumanBone::Neck, neck),
        (HumanBone::Head, head),
        (HumanBone::LeftEye, left_eye),
        (HumanBone::RightEye, right_eye),
    ] {
        m.humanoid.set(b, n);
    }
    for (side, [ul, ll, ft, ua, la, hd]) in limbs {
        let names = ["UpperLeg", "LowerLeg", "Foot", "UpperArm", "LowerArm", "Hand"];
        for (name, n) in names.iter().zip([ul, ll, ft, ua, la, hd]) {
            let bone = HumanBone::from_name(&format!("{side}{name}")).unwrap();
            m.humanoid.set(bone, n);
        }
    }
    let target = |dy: f32| MorphTarget { positions: vec![[0.0, dy, 0.0]; 3], normals: Vec::new() };
    m.meshes.push(Mesh {
        name: "face".into(),
        primitives: vec![Primitive {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            indices: vec![0, 1, 2],
            targets: vec![target(0.1), target(-0.1)],
            ..Primitive::default()
        }],
        target_names: vec!["blink".into(), "open".into()],
        default_weights: Vec::new(),
    });
    m.nodes[face].mesh = Some(0);
    let mut blink = Expression::new(ExpressionName::Preset(Preset::Blink));
    blink.morphs.push(MorphBind { node: face, target: 0, weight: 1.0 });
    let mut aa = Expression::new(ExpressionName::Preset(Preset::Aa));
    aa.morphs.push(MorphBind { node: face, target: 1, weight: 1.0 });
    let happy = Expression::new(ExpressionName::Preset(Preset::Happy));
    m.expressions = vec![blink, aa, happy];
    let mut joints: Vec<SpringJoint> = [hair0, hair1, hair2].iter().map(|&n| SpringJoint::new(n)).collect();
    for j in &mut joints {
        j.stiffness = 0.5;
        j.gravity_power = 1.0;
    }
    m.springs.springs.push(Spring { name: "hair".into(), joints, collider_groups: Vec::new() });
    m
}

pub(crate) fn node(m: &Model, name: &str) -> usize {
    m.nodes.iter().position(|n| n.name == name).unwrap()
}
