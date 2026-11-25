use super::{humanoid, node};
use crate::*;

fn check(m: &Model) -> Result<(), AvatarError> {
    m.validate(&Limits::default())
}

fn refused(change: impl FnOnce(&mut Model), why: &str) {
    let mut m = humanoid();
    change(&mut m);
    match check(&m) {
        Err(AvatarError::Invalid(s)) => assert!(s.contains(why), "{s:?} does not mention {why:?}"),
        other => panic!("expected a refusal mentioning {why:?}, got {other:?}"),
    }
}

#[test]
fn the_test_humanoid_is_valid() {
    let m = humanoid();
    check(&m).unwrap();
    assert_eq!(m.bone(HumanBone::Head), Some(node(&m, "head")));
    assert_eq!(m.preset(Preset::Aa), Some(1));
    let world = m.rest_world();
    let head = world[node(&m, "head")].translation;
    assert!((head.y - 1.6).abs() < 1e-6, "{head:?}");
    let mut into = vec![Transform::IDENTITY; m.nodes.len()];
    m.world_into(&m.rest_local(), &mut into);
    assert_eq!(into, world);
}

#[test]
fn bone_names_follow_the_humanoid_list() {
    assert_eq!(HumanBone::ALL.len(), 55);
    for b in HumanBone::ALL {
        assert_eq!(HumanBone::from_name(b.name()), Some(b));
    }
    assert_eq!(HumanBone::from_name("upperchest"), None);
    assert_eq!(HumanBone::UpperChest.name(), "upperChest");
}

#[test]
fn broken_trees_are_refused() {
    refused(|m| m.nodes[3].parent = Some(3), "does not come before it");
    refused(|m| m.nodes[1].parent = Some(40), "does not come before it");
    refused(|m| m.nodes[2].rest.translation.x = f32::NAN, "not finite");
    // Each scale fits in an f32, but the two together do not.
    refused(
        |m| {
            m.nodes[1].rest.scale = Vec3::new(1e30, 1.0, 1.0);
            m.nodes[2].rest.scale = Vec3::new(1e30, 1.0, 1.0);
        },
        "not finite in the model's space",
    );
    refused(|m| m.nodes[2].rest.rotation = Quat::new(2.0, 0.0, 0.0, 0.0), "not of unit length");
    refused(|m| m.nodes[2].rest.rotation = Quat::new(0.0, 0.0, 0.0, 0.0), "not of unit length");
    refused(|m| m.nodes[2].mesh = Some(5), "mesh that does not exist");
    refused(|m| m.nodes[2].skin = Some(0), "skin that does not exist");
    // A chain deeper than the depth limit.
    let mut m = humanoid();
    let mut parent = 0;
    for i in 0..64 {
        m.nodes.push(Node::new(&format!("deep{i}"), Some(parent), Transform::IDENTITY));
        parent = m.nodes.len() - 1;
    }
    assert!(matches!(check(&m), Err(AvatarError::TooLarge { what: "node depth", .. })));
}

#[test]
fn humanoid_problems_are_refused() {
    refused(|m| m.humanoid = Humanoid::default().with(HumanBone::Hips, 0), "has no spine bone");
    refused(|m| m.humanoid.set(HumanBone::Jaw, 99), "names a node that does not exist");
    refused(|m| m.humanoid.set(HumanBone::Jaw, 0), "shares its node");
    // A model with no humanoid at all is allowed (a prop or a 2D figure).
    let mut m = humanoid();
    m.humanoid = Humanoid::default();
    check(&m).unwrap();
}

#[test]
fn mesh_problems_are_refused() {
    refused(|m| m.meshes[0].primitives[0].indices.push(1), "multiple of 3");
    refused(|m| m.meshes[0].primitives[0].indices[2] = 3, "past the last vertex");
    refused(|m| m.meshes[0].primitives[0].normals = vec![[0.0, 0.0, 1.0]; 2], "normals");
    refused(|m| m.meshes[0].primitives[0].positions[0][1] = f32::INFINITY, "not finite");
    refused(
        |m| {
            m.meshes[0].primitives[0].targets[1].positions.pop();
        },
        "morph target's length",
    );
    refused(|m| m.meshes[0].primitives[0].material = Some(0), "material that does not exist");
    refused(
        |m| {
            m.meshes[0].target_names.pop();
        },
        "target names",
    );
    refused(|m| m.meshes[0].default_weights = vec![0.0], "default weights");
    refused(|m| m.meshes[0].primitives[0].joints = vec![[0; 4]; 3], "joint and weight counts");
    refused(
        |m| {
            m.meshes[0].primitives[0].joints = vec![[0; 4]; 3];
            m.meshes[0].primitives[0].weights = vec![[1.0, 0.0, 0.0, 0.0]; 3];
        },
        "no node draws it with a skin",
    );
    refused(
        |m| {
            let face = node(m, "face");
            m.skins.push(Skin { joints: vec![0, 1], inverse_bind: vec![[0.0; 16]; 2] });
            m.nodes[face].skin = Some(0);
            m.meshes[0].primitives[0].joints = vec![[0, 0, 2, 0]; 3];
            m.meshes[0].primitives[0].weights = vec![[0.5, 0.0, 0.5, 0.0]; 3];
        },
        "past the skin's joints",
    );
    refused(|m| m.skins.push(Skin { joints: vec![0], inverse_bind: Vec::new() }), "inverse bind matrices");
    refused(|m| m.skins.push(Skin { joints: vec![500], inverse_bind: vec![[0.0; 16]] }), "joint node that does not exist");
    // A weight of 0 on a slot past the joints is allowed: files pad unused slots with joint 0
    // or any value.
    let mut m = humanoid();
    let face = node(&m, "face");
    m.skins.push(Skin { joints: vec![0, 1], inverse_bind: vec![[0.0; 16]; 2] });
    m.nodes[face].skin = Some(0);
    m.meshes[0].primitives[0].joints = vec![[0, 1, 9, 9]; 3];
    m.meshes[0].primitives[0].weights = vec![[0.5, 0.5, 0.0, 0.0]; 3];
    check(&m).unwrap();
    // A weight of 1 is allowed; one above 1 is refused.
    m.meshes[0].primitives[0].weights = vec![[1.0, 0.0, 0.0, 0.0]; 3];
    check(&m).unwrap();
    m.meshes[0].primitives[0].weights[1][0] = 2.0;
    assert!(matches!(check(&m), Err(AvatarError::Invalid(s)) if s.contains("skin weight")));
}

#[test]
fn limits_one_over_are_refused() {
    let m = humanoid();
    let vertices: usize = m.meshes.iter().flat_map(|x| &x.primitives).map(|p| p.positions.len()).sum();
    let base = Limits::default();
    let cases: [(Limits, &str); 5] = [
        (Limits { nodes: m.nodes.len() - 1, ..base }, "nodes"),
        (Limits { vertices: vertices - 1, ..base }, "vertices"),
        (Limits { triangles: 0, ..base }, "triangles"),
        (Limits { morph_targets_per_mesh: 1, ..base }, "morph targets in a mesh"),
        (Limits { expressions: 2, ..base }, "expressions"),
    ];
    for (limits, what) in cases {
        match m.validate(&limits) {
            Err(AvatarError::TooLarge { what: w, .. }) => assert_eq!(w, what),
            other => panic!("{what}: {other:?}"),
        }
    }
    m.validate(&Limits { nodes: m.nodes.len(), vertices, triangles: 1, morph_targets_per_mesh: 2, expressions: 3, ..base }).unwrap();
    let mut t = humanoid();
    t.textures.push(Texture { width: 4_097, height: 1, rgba: vec![0; 4_097 * 4] });
    assert!(matches!(check(&t), Err(AvatarError::TooLarge { what: "texture side", .. })));
    t.textures[0] = Texture { width: 2, height: 2, rgba: vec![0; 15] };
    assert!(matches!(check(&t), Err(AvatarError::Invalid(_))));
    t.textures[0] = Texture { width: 2, height: 2, rgba: vec![0; 16] };
    check(&t).unwrap();
    assert!(matches!(t.validate(&Limits { texture_bytes: 15, ..base }), Err(AvatarError::TooLarge { what: "texture bytes", .. })));
    // Empty primitives count toward the primitive limit.
    let mut p = humanoid();
    let empty = Primitive { targets: vec![MorphTarget::default(); 2], ..Primitive::default() };
    p.meshes[0].primitives.push(empty);
    p.validate(&Limits { primitives: 2, ..base }).unwrap();
    assert!(matches!(p.validate(&Limits { primitives: 1, ..base }), Err(AvatarError::TooLarge { what: "primitives", .. })));
    // The face mesh has 2 targets of 3 positions each: 6 values.
    assert!(matches!(m.validate(&Limits { morph_values: 5, ..base }), Err(AvatarError::TooLarge { what: "morph target values", .. })));
    m.validate(&Limits { morph_values: 6, ..base }).unwrap();
    let mut e = humanoid();
    e.expressions[0].morphs = vec![e.expressions[0].morphs[0]; 1_025];
    assert!(matches!(check(&e), Err(AvatarError::TooLarge { what: "binds in an expression", .. })));
}

#[test]
fn expression_problems_are_refused() {
    refused(|m| m.expressions[1].name = ExpressionName::Preset(Preset::Blink), "appears twice");
    refused(|m| m.expressions[0].morphs[0].target = 2, "morph target that does not exist");
    refused(|m| m.expressions[0].morphs[0].node = 0, "morph target that does not exist");
    refused(|m| m.expressions[0].morphs[0].weight = f32::NAN, "not finite");
    refused(|m| m.expressions[0].colors.push(ColorBind { material: 0, target: ColorTarget::Color, value: [1.0; 4] }), "material color bind");
    refused(|m| m.expressions[0].textures.push(TextureBind { material: 0, scale: [1.0; 2], offset: [0.0; 2] }), "texture bind");
    refused(|m| m.expressions[2].name = ExpressionName::Custom(String::new()), "0 bytes");
    refused(|m| m.expressions[2].name = ExpressionName::Custom("ih".into()), "preset");
    refused(|m| m.expressions[0].morphs[0].weight = -0.1, "outside 0 to 1");
    refused(|m| m.expressions[0].morphs[0].weight = 1.5, "outside 0 to 1");
    let mut ends = humanoid();
    ends.expressions[0].morphs[0].weight = 0.0;
    ends.expressions[1].morphs[0].weight = 1.0;
    ends.expressions[2].name = ExpressionName::Custom("Happy".into());
    check(&ends).unwrap();
    refused(|m| m.first_person.push((0, FirstPerson::ThirdPersonOnly)), "has no mesh");
    let mut m = humanoid();
    m.materials.push(Material { base_color: [1.0, f32::NAN, 1.0, 1.0], ..Material::default() });
    assert!(check(&m).is_err());
    m.materials[0].base_color = [1.0; 4];
    m.materials[0].toon = Some(Toon { outline_width: f32::INFINITY, ..Toon::default() });
    assert!(check(&m).is_err());
    m.materials[0].toon = Some(Toon::default());
    m.materials[0].base_texture = Some(0);
    assert!(check(&m).is_err());
}

#[test]
fn license_terms_default_to_the_most_restrictive() {
    let meta = Meta::default();
    assert_eq!(meta.avatar_permission, AvatarPermission::OnlyAuthor);
    assert_eq!(meta.commercial_usage, CommercialUsage::PersonalNonProfit);
    assert_eq!(meta.modification, Modification::Prohibited);
    assert!(!meta.allow_redistribution && !meta.allow_violent && !meta.allow_sexual);
}

trait With {
    fn with(self, b: HumanBone, n: usize) -> Self;
}

impl With for Humanoid {
    fn with(mut self, b: HumanBone, n: usize) -> Humanoid {
        self.set(b, n);
        self
    }
}
