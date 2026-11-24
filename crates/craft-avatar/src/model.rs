//! The avatar model: a node tree with the humanoid bones mapped onto it, meshes with skins and
//! morph targets, materials, expressions, look-at, spring bones and the license terms.
//!
//! A model is plain data that a reader fills and the renderer and the pose step read. It holds
//! no GPU objects. [`Model::validate`] checks every index and count against [`Limits`] before
//! anything else uses it, so code that reads a validated model can index without checks failing.

use crate::AvatarError;
use crate::expression::{Expression, Preset};
use crate::look_at::LookAt;
use crate::math::Transform;
use crate::spring::SpringBones;

/// Size limits for a model. The defaults suit avatars on a live canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub nodes: usize,
    pub depth: usize,
    pub meshes: usize,
    /// Primitives in all meshes together.
    pub primitives: usize,
    pub skins: usize,
    /// Joints in one skin.
    pub joints_per_skin: usize,
    /// Vertices in all primitives together.
    pub vertices: usize,
    /// Triangles in all primitives together.
    pub triangles: usize,
    pub morph_targets_per_mesh: usize,
    /// Position and normal offsets in all morph targets together.
    pub morph_values: usize,
    pub materials: usize,
    pub textures: usize,
    /// Longest side of one texture, in pixels.
    pub texture_side: u32,
    /// Bytes of texture data in all textures together.
    pub texture_bytes: usize,
    pub expressions: usize,
    /// Binds of each kind in one expression.
    pub binds_per_expression: usize,
    /// Spring chains.
    pub springs: usize,
    /// Joints in all spring chains together.
    pub spring_joints: usize,
    pub colliders: usize,
    pub collider_groups: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            nodes: 1_024,
            depth: 64,
            meshes: 256,
            primitives: 4_096,
            skins: 256,
            joints_per_skin: 1_024,
            vertices: 2_000_000,
            triangles: 2_000_000,
            morph_targets_per_mesh: 256,
            morph_values: 32_000_000,
            materials: 256,
            textures: 256,
            texture_side: 4_096,
            texture_bytes: 512 << 20,
            expressions: 256,
            binds_per_expression: 1_024,
            springs: 256,
            spring_joints: 2_048,
            colliders: 256,
            collider_groups: 256,
        }
    }
}

/// One node of the model's tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub name: String,
    /// The parent node. A parent always comes before its children in [`Model::nodes`].
    pub parent: Option<usize>,
    /// The node's rest transform, relative to its parent.
    pub rest: Transform,
    pub mesh: Option<usize>,
    pub skin: Option<usize>,
}

impl Node {
    pub fn new(name: &str, parent: Option<usize>, rest: Transform) -> Node {
        Node { name: name.to_string(), parent, rest, mesh: None, skin: None }
    }
}

macro_rules! bones {
    ($($v:ident = $n:literal),* $(,)?) => {
        /// A humanoid bone, as the VRM 1.0 humanoid specification names them
        /// (https://github.com/vrm-c/vrm-specification/blob/master/specification/VRMC_vrm-1.0/humanoid.md).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum HumanBone { $($v),* }

        impl HumanBone {
            pub const ALL: [HumanBone; 55] = [$(HumanBone::$v),*];

            pub fn name(self) -> &'static str {
                match self { $(HumanBone::$v => $n),* }
            }
        }
    };
}

bones! {
    Hips = "hips", Spine = "spine", Chest = "chest", UpperChest = "upperChest", Neck = "neck", Head = "head",
    LeftEye = "leftEye", RightEye = "rightEye", Jaw = "jaw",
    LeftUpperLeg = "leftUpperLeg", LeftLowerLeg = "leftLowerLeg", LeftFoot = "leftFoot", LeftToes = "leftToes",
    RightUpperLeg = "rightUpperLeg", RightLowerLeg = "rightLowerLeg", RightFoot = "rightFoot", RightToes = "rightToes",
    LeftShoulder = "leftShoulder", LeftUpperArm = "leftUpperArm", LeftLowerArm = "leftLowerArm", LeftHand = "leftHand",
    RightShoulder = "rightShoulder", RightUpperArm = "rightUpperArm", RightLowerArm = "rightLowerArm", RightHand = "rightHand",
    LeftThumbMetacarpal = "leftThumbMetacarpal", LeftThumbProximal = "leftThumbProximal", LeftThumbDistal = "leftThumbDistal",
    LeftIndexProximal = "leftIndexProximal", LeftIndexIntermediate = "leftIndexIntermediate", LeftIndexDistal = "leftIndexDistal",
    LeftMiddleProximal = "leftMiddleProximal", LeftMiddleIntermediate = "leftMiddleIntermediate", LeftMiddleDistal = "leftMiddleDistal",
    LeftRingProximal = "leftRingProximal", LeftRingIntermediate = "leftRingIntermediate", LeftRingDistal = "leftRingDistal",
    LeftLittleProximal = "leftLittleProximal", LeftLittleIntermediate = "leftLittleIntermediate", LeftLittleDistal = "leftLittleDistal",
    RightThumbMetacarpal = "rightThumbMetacarpal", RightThumbProximal = "rightThumbProximal", RightThumbDistal = "rightThumbDistal",
    RightIndexProximal = "rightIndexProximal", RightIndexIntermediate = "rightIndexIntermediate", RightIndexDistal = "rightIndexDistal",
    RightMiddleProximal = "rightMiddleProximal", RightMiddleIntermediate = "rightMiddleIntermediate", RightMiddleDistal = "rightMiddleDistal",
    RightRingProximal = "rightRingProximal", RightRingIntermediate = "rightRingIntermediate", RightRingDistal = "rightRingDistal",
    RightLittleProximal = "rightLittleProximal", RightLittleIntermediate = "rightLittleIntermediate", RightLittleDistal = "rightLittleDistal",
}

impl HumanBone {
    /// The bones a humanoid must have.
    pub const REQUIRED: [HumanBone; 15] = [
        HumanBone::Hips,
        HumanBone::Spine,
        HumanBone::Head,
        HumanBone::LeftUpperLeg,
        HumanBone::LeftLowerLeg,
        HumanBone::LeftFoot,
        HumanBone::RightUpperLeg,
        HumanBone::RightLowerLeg,
        HumanBone::RightFoot,
        HumanBone::LeftUpperArm,
        HumanBone::LeftLowerArm,
        HumanBone::LeftHand,
        HumanBone::RightUpperArm,
        HumanBone::RightLowerArm,
        HumanBone::RightHand,
    ];

    pub fn from_name(name: &str) -> Option<HumanBone> {
        HumanBone::ALL.iter().copied().find(|b| b.name() == name)
    }
}

/// Which node each humanoid bone is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Humanoid {
    bones: Vec<(HumanBone, usize)>,
}

impl Humanoid {
    /// Maps `bone` to `node`, replacing an earlier mapping of the same bone.
    pub fn set(&mut self, bone: HumanBone, node: usize) {
        match self.bones.iter_mut().find(|(b, _)| *b == bone) {
            Some(slot) => slot.1 = node,
            None => self.bones.push((bone, node)),
        }
    }

    pub fn node(&self, bone: HumanBone) -> Option<usize> {
        self.bones.iter().find(|(b, _)| *b == bone).map(|(_, n)| *n)
    }

    pub fn bones(&self) -> &[(HumanBone, usize)] {
        &self.bones
    }
}

/// One morph target: offsets added to the base vertices, at full weight.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MorphTarget {
    pub positions: Vec<[f32; 3]>,
    /// Empty, or one normal offset per vertex.
    pub normals: Vec<[f32; 3]>,
}

/// One drawn part of a mesh.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Primitive {
    pub positions: Vec<[f32; 3]>,
    /// Empty, or one per vertex.
    pub normals: Vec<[f32; 3]>,
    /// Empty, or one per vertex.
    pub uvs: Vec<[f32; 2]>,
    /// Empty, or one set of four skin joint slots per vertex.
    pub joints: Vec<[u16; 4]>,
    /// Empty, or one set of four weights per vertex, beside `joints`.
    pub weights: Vec<[f32; 4]>,
    /// Triangles, three vertex indices each.
    pub indices: Vec<u32>,
    pub material: Option<usize>,
    /// One per morph target of the mesh, in the mesh's order.
    pub targets: Vec<MorphTarget>,
}

/// A mesh: primitives that share morph targets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mesh {
    pub name: String,
    pub primitives: Vec<Primitive>,
    /// One name per morph target; may be empty when the file names none.
    pub target_names: Vec<String>,
    /// The morph weights when no expression is on, one per target or empty for all 0.
    pub default_weights: Vec<f32>,
}

impl Mesh {
    /// The number of morph targets, from the first primitive.
    pub fn target_count(&self) -> usize {
        self.primitives.first().map_or(0, |p| p.targets.len())
    }
}

/// A skin: the joints a mesh bends with and their inverse bind matrices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Skin {
    pub joints: Vec<usize>,
    /// One column-major 4 × 4 matrix per joint.
    pub inverse_bind: Vec<[f32; 16]>,
}

/// How a material's alpha is used.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum AlphaMode {
    #[default]
    Opaque,
    /// Pixels below the cutoff are not drawn.
    Mask(f32),
    Blend,
}

/// The toon shading values of a material.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Toon {
    pub shade_color: [f32; 3],
    /// Moves the light and shade border, −1 to 1.
    pub shading_shift: f32,
    /// How sharp the border is, 0 (soft) to 1 (hard).
    pub shading_toony: f32,
    pub rim_color: [f32; 3],
    pub rim_fresnel_power: f32,
    pub rim_lift: f32,
    /// Outline width as a fraction of the screen height; 0 draws none.
    pub outline_width: f32,
    pub outline_color: [f32; 3],
}

impl Default for Toon {
    fn default() -> Toon {
        Toon {
            shade_color: [0.0; 3],
            shading_shift: 0.0,
            shading_toony: 0.9,
            rim_color: [0.0; 3],
            rim_fresnel_power: 5.0,
            rim_lift: 0.0,
            outline_width: 0.0,
            outline_color: [0.0; 3],
        }
    }
}

/// A material.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub name: String,
    pub base_color: [f32; 4],
    pub base_texture: Option<usize>,
    pub emissive: [f32; 3],
    pub alpha: AlphaMode,
    pub double_sided: bool,
    /// Toon shading; `None` draws the material unlit.
    pub toon: Option<Toon>,
}

impl Default for Material {
    fn default() -> Material {
        Material {
            name: String::new(),
            base_color: [1.0; 4],
            base_texture: None,
            emissive: [0.0; 3],
            alpha: AlphaMode::Opaque,
            double_sided: false,
            toon: None,
        }
    }
}

/// A decoded texture: straight-alpha RGBA8 rows, `width * 4` bytes each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Who may use an avatar, as its license terms state it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AvatarPermission {
    #[default]
    OnlyAuthor,
    OnlySeparatelyLicensedPerson,
    Everyone,
}

/// Whether an avatar may be used commercially.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CommercialUsage {
    #[default]
    PersonalNonProfit,
    PersonalProfit,
    Corporation,
}

/// Whether an avatar may be changed, and whether a changed avatar may be passed on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Modification {
    #[default]
    Prohibited,
    Allowed,
    AllowedWithRedistribution,
}

/// The avatar's name, authors and license terms, as its file states them. The app shows them;
/// it does not enforce them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub name: String,
    pub version: String,
    pub authors: Vec<String>,
    pub copyright: String,
    pub contact: String,
    pub license_url: String,
    pub other_license_url: String,
    pub avatar_permission: AvatarPermission,
    pub commercial_usage: CommercialUsage,
    pub modification: Modification,
    pub allow_violent: bool,
    pub allow_sexual: bool,
    pub allow_political_or_religious: bool,
    pub allow_antisocial_or_hate: bool,
    pub allow_redistribution: bool,
    pub credit_required: bool,
}

/// Whether a mesh is drawn in a first-person view, a third-person view, or both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FirstPerson {
    #[default]
    Auto,
    Both,
    ThirdPersonOnly,
    FirstPersonOnly,
}

/// A whole avatar.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    pub meta: Meta,
    pub nodes: Vec<Node>,
    pub humanoid: Humanoid,
    pub meshes: Vec<Mesh>,
    pub skins: Vec<Skin>,
    pub materials: Vec<Material>,
    pub textures: Vec<Texture>,
    pub expressions: Vec<Expression>,
    pub look_at: LookAt,
    pub springs: SpringBones,
    /// The first-person flag of a mesh node, by node index.
    pub first_person: Vec<(usize, FirstPerson)>,
    /// Things the reader passed over or changed, in English.
    pub warnings: Vec<String>,
}

fn bad(what: impl Into<String>) -> AvatarError {
    AvatarError::Invalid(what.into())
}

fn finite3(v: &[f32; 3]) -> bool {
    v.iter().all(|c| c.is_finite())
}

impl Model {
    /// The expression named by `preset`, if the model has it.
    pub fn preset(&self, preset: Preset) -> Option<usize> {
        self.expressions.iter().position(|e| e.name.preset() == Some(preset))
    }

    /// The node of a humanoid bone.
    pub fn bone(&self, bone: HumanBone) -> Option<usize> {
        self.humanoid.node(bone)
    }

    /// The rest transform of every node in the model's space, in node order.
    pub fn rest_world(&self) -> Vec<Transform> {
        let mut world: Vec<Transform> = Vec::with_capacity(self.nodes.len());
        for n in &self.nodes {
            let w = match n.parent.and_then(|p| world.get(p)) {
                Some(parent) => n.rest.then(parent),
                None => n.rest,
            };
            world.push(w);
        }
        world
    }

    /// Checks every index, count and value against `limits`. An error names the first problem.
    pub fn validate(&self, limits: &Limits) -> Result<(), AvatarError> {
        self.validate_nodes(limits)?;
        self.validate_meshes(limits)?;
        self.validate_materials(limits)?;
        self.validate_humanoid()?;
        self.validate_expressions(limits)?;
        self.look_at.validate()?;
        self.springs.validate(self, limits)?;
        for (node, _) in &self.first_person {
            if self.nodes.get(*node).and_then(|n| n.mesh).is_none() {
                return Err(bad(format!("a first-person flag names node {node}, which has no mesh")));
            }
        }
        Ok(())
    }

    fn validate_nodes(&self, limits: &Limits) -> Result<(), AvatarError> {
        if self.nodes.len() > limits.nodes {
            return Err(AvatarError::TooLarge { what: "nodes", limit: limits.nodes });
        }
        let mut depth = vec![0usize; self.nodes.len()];
        for (i, n) in self.nodes.iter().enumerate() {
            if let Some(p) = n.parent {
                if p >= i {
                    return Err(bad(format!("node {i} has parent {p}, which does not come before it")));
                }
                depth[i] = depth[p] + 1;
                if depth[i] >= limits.depth {
                    return Err(AvatarError::TooLarge { what: "node depth", limit: limits.depth });
                }
            }
            if !n.rest.is_finite() {
                return Err(bad(format!("node {i} has a transform that is not finite")));
            }
            if !n.rest.rotation.is_unit() {
                return Err(bad(format!("node {i} has a rotation that is not of unit length")));
            }
            if n.mesh.is_some_and(|m| m >= self.meshes.len()) {
                return Err(bad(format!("node {i} names a mesh that does not exist")));
            }
            if n.skin.is_some_and(|s| s >= self.skins.len()) {
                return Err(bad(format!("node {i} names a skin that does not exist")));
            }
            if n.skin.is_some() && n.mesh.is_none() {
                return Err(bad(format!("node {i} has a skin and no mesh")));
            }
        }
        Ok(())
    }

    fn validate_meshes(&self, limits: &Limits) -> Result<(), AvatarError> {
        if self.meshes.len() > limits.meshes {
            return Err(AvatarError::TooLarge { what: "meshes", limit: limits.meshes });
        }
        if self.skins.len() > limits.skins {
            return Err(AvatarError::TooLarge { what: "skins", limit: limits.skins });
        }
        for (i, s) in self.skins.iter().enumerate() {
            if s.joints.len() > limits.joints_per_skin {
                return Err(AvatarError::TooLarge { what: "joints in a skin", limit: limits.joints_per_skin });
            }
            if s.inverse_bind.len() != s.joints.len() {
                return Err(bad(format!("skin {i} has {} joints and {} inverse bind matrices", s.joints.len(), s.inverse_bind.len())));
            }
            if s.joints.iter().any(|&j| j >= self.nodes.len()) {
                return Err(bad(format!("skin {i} names a joint node that does not exist")));
            }
            if s.inverse_bind.iter().any(|m| m.iter().any(|v| !v.is_finite())) {
                return Err(bad(format!("skin {i} has an inverse bind matrix that is not finite")));
            }
        }
        // The smallest joint count among the skins each mesh is drawn with.
        let mut mesh_joints = vec![None::<usize>; self.meshes.len()];
        for n in &self.nodes {
            if let (Some(m), Some(s)) = (n.mesh, n.skin) {
                let j = self.skins.get(s).map_or(0, |s| s.joints.len());
                let slot = &mut mesh_joints[m];
                *slot = Some(slot.map_or(j, |old: usize| old.min(j)));
            }
        }
        let (mut vertices, mut triangles, mut morph_values, mut primitives) = (0usize, 0usize, 0usize, 0usize);
        for (mi, m) in self.meshes.iter().enumerate() {
            let targets = m.target_count();
            if targets > limits.morph_targets_per_mesh {
                return Err(AvatarError::TooLarge { what: "morph targets in a mesh", limit: limits.morph_targets_per_mesh });
            }
            if !m.target_names.is_empty() && m.target_names.len() != targets {
                return Err(bad(format!("mesh {mi} has {targets} morph targets and {} target names", m.target_names.len())));
            }
            if !m.default_weights.is_empty() && m.default_weights.len() != targets {
                return Err(bad(format!("mesh {mi} has {targets} morph targets and {} default weights", m.default_weights.len())));
            }
            if m.default_weights.iter().any(|w| !w.is_finite()) {
                return Err(bad(format!("mesh {mi} has a default weight that is not finite")));
            }
            primitives = primitives.saturating_add(m.primitives.len());
            if primitives > limits.primitives {
                return Err(AvatarError::TooLarge { what: "primitives", limit: limits.primitives });
            }
            for (pi, p) in m.primitives.iter().enumerate() {
                let at = |what: &str| bad(format!("mesh {mi} primitive {pi}: {what}"));
                let v = p.positions.len();
                vertices = vertices.saturating_add(v);
                if vertices > limits.vertices {
                    return Err(AvatarError::TooLarge { what: "vertices", limit: limits.vertices });
                }
                if p.indices.len() % 3 != 0 {
                    return Err(at("the index count is not a multiple of 3"));
                }
                triangles = triangles.saturating_add(p.indices.len() / 3);
                if triangles > limits.triangles {
                    return Err(AvatarError::TooLarge { what: "triangles", limit: limits.triangles });
                }
                for (len, what) in [(p.normals.len(), "normals"), (p.uvs.len(), "texture coordinates"), (p.joints.len(), "joints")] {
                    if len != 0 && len != v {
                        return Err(at(&format!("it has {v} vertices and {len} {what}")));
                    }
                }
                if p.weights.len() != p.joints.len() {
                    return Err(at("the joint and weight counts differ"));
                }
                if p.indices.iter().any(|&i| i as usize >= v) {
                    return Err(at("an index is past the last vertex"));
                }
                if !p.positions.iter().chain(&p.normals).all(finite3) || !p.uvs.iter().all(|t| t[0].is_finite() && t[1].is_finite()) {
                    return Err(at("a vertex value is not finite"));
                }
                if p.weights.iter().any(|w| w.iter().any(|x| !x.is_finite() || *x < 0.0)) {
                    return Err(at("a skin weight is negative or not finite"));
                }
                if !p.joints.is_empty() {
                    let Some(Some(count)) = mesh_joints.get(mi) else {
                        return Err(at("it has joints and no node draws it with a skin"));
                    };
                    let used = p.joints.iter().zip(&p.weights).any(|(j, w)| j.iter().zip(w).any(|(&j, &w)| w > 0.0 && j as usize >= *count));
                    if used {
                        return Err(at("a weighted joint slot is past the skin's joints"));
                    }
                }
                if p.material.is_some_and(|m| m >= self.materials.len()) {
                    return Err(at("it names a material that does not exist"));
                }
                if p.targets.len() != targets {
                    return Err(at("its morph target count differs from the mesh's first primitive"));
                }
                for t in &p.targets {
                    morph_values = morph_values.saturating_add(t.positions.len()).saturating_add(t.normals.len());
                    if morph_values > limits.morph_values {
                        return Err(AvatarError::TooLarge { what: "morph target values", limit: limits.morph_values });
                    }
                    if t.positions.len() != v || (!t.normals.is_empty() && t.normals.len() != v) {
                        return Err(at("a morph target's length differs from the vertex count"));
                    }
                    if !t.positions.iter().chain(&t.normals).all(finite3) {
                        return Err(at("a morph target value is not finite"));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_materials(&self, limits: &Limits) -> Result<(), AvatarError> {
        if self.materials.len() > limits.materials {
            return Err(AvatarError::TooLarge { what: "materials", limit: limits.materials });
        }
        if self.textures.len() > limits.textures {
            return Err(AvatarError::TooLarge { what: "textures", limit: limits.textures });
        }
        let mut bytes = 0usize;
        for (i, t) in self.textures.iter().enumerate() {
            if t.width > limits.texture_side || t.height > limits.texture_side {
                return Err(AvatarError::TooLarge { what: "texture side", limit: limits.texture_side as usize });
            }
            let want = (t.width as usize).saturating_mul(t.height as usize).saturating_mul(4);
            if t.rgba.len() != want {
                return Err(bad(format!("texture {i} is {} × {} and holds {} bytes", t.width, t.height, t.rgba.len())));
            }
            bytes = bytes.saturating_add(want);
            if bytes > limits.texture_bytes {
                return Err(AvatarError::TooLarge { what: "texture bytes", limit: limits.texture_bytes });
            }
        }
        for (i, m) in self.materials.iter().enumerate() {
            if m.base_texture.is_some_and(|t| t >= self.textures.len()) {
                return Err(bad(format!("material {i} names a texture that does not exist")));
            }
            let mut values = m.base_color.to_vec();
            values.extend_from_slice(&m.emissive);
            if let AlphaMode::Mask(c) = m.alpha {
                values.push(c);
            }
            if let Some(t) = &m.toon {
                values.extend_from_slice(&t.shade_color);
                values.extend_from_slice(&t.rim_color);
                values.extend_from_slice(&t.outline_color);
                values.extend_from_slice(&[t.shading_shift, t.shading_toony, t.rim_fresnel_power, t.rim_lift, t.outline_width]);
            }
            if values.iter().any(|v| !v.is_finite()) {
                return Err(bad(format!("material {i} has a value that is not finite")));
            }
        }
        Ok(())
    }

    fn validate_humanoid(&self) -> Result<(), AvatarError> {
        let bones = self.humanoid.bones();
        if bones.is_empty() {
            return Ok(());
        }
        for (b, n) in bones {
            if *n >= self.nodes.len() {
                return Err(bad(format!("the humanoid bone {} names a node that does not exist", b.name())));
            }
        }
        for (i, (b, n)) in bones.iter().enumerate() {
            if bones[..i].iter().any(|(_, m)| m == n) {
                return Err(bad(format!("the humanoid bone {} shares its node with another bone", b.name())));
            }
        }
        for b in HumanBone::REQUIRED {
            if self.humanoid.node(b).is_none() {
                return Err(bad(format!("the humanoid has no {} bone", b.name())));
            }
        }
        Ok(())
    }

    fn validate_expressions(&self, limits: &Limits) -> Result<(), AvatarError> {
        if self.expressions.len() > limits.expressions {
            return Err(AvatarError::TooLarge { what: "expressions", limit: limits.expressions });
        }
        for (i, e) in self.expressions.iter().enumerate() {
            let name = e.name.as_str();
            if name.is_empty() || name.len() > crate::expression::MAX_NAME_BYTES {
                return Err(bad(format!("expression {i} has a name of {} bytes", name.len())));
            }
            if self.expressions[..i].iter().any(|o| o.name == e.name) {
                return Err(bad(format!("the expression {name} appears twice")));
            }
            let max = limits.binds_per_expression;
            if e.morphs.len() > max || e.colors.len() > max || e.textures.len() > max {
                return Err(AvatarError::TooLarge { what: "binds in an expression", limit: max });
            }
            for b in &e.morphs {
                let mesh = self.nodes.get(b.node).and_then(|n| n.mesh).and_then(|m| self.meshes.get(m));
                match mesh {
                    Some(m) if b.target < m.target_count() => {}
                    _ => return Err(bad(format!("the expression {name} binds a morph target that does not exist"))),
                }
                if !b.weight.is_finite() {
                    return Err(bad(format!("the expression {name} has a weight that is not finite")));
                }
            }
            for b in &e.colors {
                if b.material >= self.materials.len() || b.value.iter().any(|v| !v.is_finite()) {
                    return Err(bad(format!("the expression {name} has a material color bind that is not valid")));
                }
            }
            for b in &e.textures {
                if b.material >= self.materials.len() || b.scale.iter().chain(&b.offset).any(|v| !v.is_finite()) {
                    return Err(bad(format!("the expression {name} has a texture bind that is not valid")));
                }
            }
        }
        Ok(())
    }
}

/// The world transform of node `i` from local transforms `local`, by walking up its parents.
/// `None` when an index is out of range.
pub(crate) fn world_of(model: &Model, local: &[Transform], i: usize) -> Option<Transform> {
    let mut t = *local.get(i)?;
    let mut at = model.nodes.get(i)?.parent;
    let mut steps = 0usize;
    while let Some(p) = at {
        steps += 1;
        if steps > model.nodes.len() {
            return None;
        }
        t = t.then(local.get(p)?);
        at = model.nodes.get(p)?.parent;
    }
    Some(t)
}

impl Model {
    /// The local rest transform of every node, in node order: the starting pose.
    pub fn rest_local(&self) -> Vec<Transform> {
        self.nodes.iter().map(|n| n.rest).collect()
    }

    /// Fills `world` with each node's transform in the model's space from the local transforms
    /// `local`, in one pass in node order. Nodes past the end of either slice are left out. It
    /// does not allocate.
    pub fn world_into(&self, local: &[Transform], world: &mut [Transform]) {
        let n = self.nodes.len().min(local.len()).min(world.len());
        for i in 0..n {
            let parent = self.nodes[i].parent.filter(|&p| p < i).map(|p| world[p]);
            world[i] = match parent {
                Some(p) => local[i].then(&p),
                None => local[i],
            };
        }
    }
}
