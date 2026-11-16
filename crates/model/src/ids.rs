//! Identifiers. Every id in a scene collection comes from one counter
//! ([`crate::SceneCollection::next_id`]), so an id is never reused within a collection.

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(
    /// A scene.
    SceneId
);
id_type!(
    /// A source (shared by every scene item that shows it).
    SourceId
);
id_type!(
    /// One placement of a source in a scene.
    ItemId
);
id_type!(
    /// A filter on a source.
    FilterId
);
