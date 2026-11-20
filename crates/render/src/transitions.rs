//! Built-in scene transitions: the transitions of `craft-transitions` as plug-ins.

use craft_transitions::{Ease, Edge, Kind, Params};
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Easing, Plugin, Transition, easing_field};

/// The Easing setting's key for Fade and Fade through color.
const FADE_EASING: &str = "fade_easing";
/// The Easing setting's key for Swipe. Its key is its own, so a Fade's easing does not carry over
/// when the transition changes.
const SWIPE_EASING: &str = "swipe_easing";

/// The key of `kind`'s Easing setting.
fn easing_key(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Cut => None,
        Kind::Fade | Kind::FadeThroughColor => Some(FADE_EASING),
        Kind::Swipe => Some(SWIPE_EASING),
    }
}

/// `kind`'s settings form.
fn schema(kind: Kind) -> SettingsSchema {
    let mut fields = Vec::new();
    match kind {
        Kind::FadeThroughColor => fields.push(Field::new("color", "Color", FieldKind::Color, "#000000")),
        Kind::Swipe => {
            let options = Edge::ALL.iter().map(|e| ChoiceOption::new(e.id(), e.label())).collect();
            fields.push(Field::new("direction", "Direction", FieldKind::Choice { options }, Edge::default().id()));
        }
        Kind::Cut | Kind::Fade => {}
    }
    if let Some(key) = easing_key(kind) {
        fields.push(easing_field(key, kind.default_ease().id()));
    }
    if fields.is_empty() { SettingsSchema::default() } else { SettingsSchema::new(fields) }
}

/// `kind`'s parameters from a scene collection's settings. A missing or unknown value takes the
/// default, as settings carry over from other transitions and older files.
fn params(kind: Kind, settings: &Settings) -> Params {
    let mut p = Params::default();
    if let Some(color) = settings.color("color") {
        p.color = color;
    }
    if let Some(edge) = settings.str("direction").and_then(Edge::from_id) {
        p.edge = edge;
    }
    p.ease = easing_key(kind).and_then(|k| settings.str(k)).and_then(Ease::from_id);
    p
}

macro_rules! built_in {
    ($(#[$doc:meta])* $name:ident, $kind:expr) => {
        $(#[$doc])*
        pub struct $name;

        impl $name {
            const KIND: Kind = $kind;
        }

        impl Plugin for $name {
            fn id(&self) -> &'static str {
                Self::KIND.id()
            }
            fn name(&self) -> &'static str {
                Self::KIND.name()
            }
            fn schema(&self) -> SettingsSchema {
                schema(Self::KIND)
            }
        }

        impl Transition for $name {
            fn instant(&self) -> bool {
                Self::KIND.instant()
            }
            fn shader(&self) -> &'static str {
                Self::KIND.shader()
            }
            fn params(&self, settings: &Settings) -> [f32; 4] {
                Self::KIND.uniform(&params(Self::KIND, settings))
            }
            fn easing(&self, settings: &Settings) -> Easing {
                Self::KIND.ease(&params(Self::KIND, settings))
            }
            fn audio_gains(&self, t: f32) -> (f32, f32) {
                Self::KIND.audio_gains(t)
            }
        }
    };
}

built_in!(
    /// Switches at once.
    Cut,
    Kind::Cut
);
built_in!(
    /// Blends the old scene into the new one.
    Fade,
    Kind::Fade
);
built_in!(
    /// Fades the old scene to a color, then the color to the new scene.
    FadeThroughColor,
    Kind::FadeThroughColor
);
built_in!(
    /// The new scene slides in over the old one from an edge.
    Swipe,
    Kind::Swipe
);

#[cfg(test)]
pub(crate) mod tests;
