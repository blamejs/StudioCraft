//! StudioCraft in the browser: an editor for scene collection files.
//!
//! [`Editor`] opens a scene collection file of any version the app reads, lists its scenes and the
//! sources each scene places, and adds, renames, removes and reorders them. Every edit runs the
//! app's own command through a [`studiocraft_engine::Engine`] (`scene.add`, `scene.rename`,
//! `scene.remove`, `scene.move`, `source.add_existing`, `source.rename`, `item.remove`,
//! `item.move`, `edit.undo` and `edit.redo`), so an edit has the same rules and the same undo
//! step as in the app. [`Editor::saved`] writes the collection back as a file's text in the
//! current format.
//!
//! The editor does no I/O and never waits. The page reads a chosen file on its own and hands the
//! bytes over with [`Editor::finish_open`]; the collection stays editable until they arrive, and
//! the bytes of an open that a newer one replaced are dropped. On `wasm32` the binary in
//! `src/main.rs` draws the page in `index.html` and offers the saved text as a download. The page
//! shows its text in the browser's language through [`i18n`].
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod i18n;

use std::sync::Arc;

use studiocraft_engine::{Command, Engine, EngineError, ItemRef, Move, SceneRef, SourceRef};
use studiocraft_model::format::{FormatError, read_collection, write_collection};
use studiocraft_model::{ItemId, SceneCollection, SceneId, SourceId};
use studiocraft_plugin::Registry;
use thiserror::Error;

/// The largest scene collection file the editor reads or writes, in bytes: the app's import
/// limit.
pub const MAX_FILE_BYTES: u64 = 16 << 20;
/// The name of a new collection, and the download name of a collection whose name leaves no
/// usable file name.
pub const DEFAULT_COLLECTION: &str = "Untitled";
/// The name given to a scene when a file's collection has none.
pub const DEFAULT_SCENE: &str = "Scene";
/// The longest download file name made from a collection's name, in characters, before `.json`.
pub const MAX_FILE_STEM: usize = 100;

/// Why a file or an edit was refused. The collection is unchanged after every error.
#[derive(Debug, Error, PartialEq)]
pub enum EditorError {
    #[error("the file is {bytes} bytes; a scene collection file can be at most {max} bytes")]
    TooLarge { bytes: u64, max: u64 },
    #[error("the file is not UTF-8 text")]
    NotText,
    #[error("{0}")]
    File(#[from] FormatError),
    #[error("{0}")]
    Edit(#[from] EngineError),
}

impl EditorError {
    /// The error in the page's language. Errors from a file's format and from a command show
    /// their English text.
    pub fn shown(&self) -> String {
        match self {
            EditorError::TooLarge { bytes, max } => i18n::tmsg(
                "the file is {bytes} bytes; a scene collection file can be at most {max} bytes",
                &[("bytes", bytes.to_string().as_str()), ("max", max.to_string().as_str())],
            ),
            EditorError::NotText => i18n::tmsg("the file is not UTF-8 text", &[]),
            other => other.to_string(),
        }
    }
}

/// One edit, as the page's buttons ask for it. Scene items act in the listed scene.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// List the sources of this scene. The file does not change.
    ListScene(SceneId),
    /// Add a scene at the end of the list and list it. Without a name it is called `Scene`, or
    /// `Scene 2` and so on when that is taken. A name another scene has is an error.
    AddScene {
        name: Option<String>,
    },
    RenameScene(SceneId, String),
    /// Remove a scene, unless it is the last one. Sources that no other scene places go with it.
    RemoveScene(SceneId),
    MoveScene(SceneId, Move),
    /// Place a source of the collection on top of the listed scene again.
    PlaceSource(SourceId),
    /// Rename a source; every scene that places it shows the new name.
    RenameSource(SourceId, String),
    /// Remove a scene item from the listed scene. Its source goes too when no other scene places
    /// it.
    RemoveItem(ItemId),
    /// Move a scene item in the listed scene. Up is toward the top of the picture, which is the
    /// top of the list.
    MoveItem(ItemId, Move),
    Undo,
    Redo,
}

/// A scene in the Scenes list.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneRow {
    pub id: SceneId,
    pub name: String,
    /// The scene the app shows on the program when it opens the file.
    pub program: bool,
    /// The scene whose sources are listed.
    pub listed: bool,
}

/// A scene item in the listed scene's Sources list.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemRow {
    pub item: ItemId,
    pub source: SourceId,
    pub name: String,
    /// The source plug-in's id (`color`, `image`, `camera`, ...).
    pub kind: String,
    pub visible: bool,
}

/// A source the listed scene can place.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceRow {
    pub id: SourceId,
    pub name: String,
    pub kind: String,
}

/// What the page shows.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub collection: String,
    pub scenes: Vec<SceneRow>,
    /// The listed scene's items, the top of the picture first.
    pub items: Vec<ItemRow>,
    /// Every source of the collection, in the collection's order.
    pub sources: Vec<SourceRow>,
    /// The name of the step undo takes back, and of the one redo puts back.
    pub undo: Option<&'static str>,
    pub redo: Option<&'static str>,
    /// Whether the collection changed since it was opened or saved.
    pub changed: bool,
    /// Whether a file is being read.
    pub opening: bool,
}

/// The text of a saved collection and the name to download it under.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub file_name: String,
    pub text: String,
}

/// One file open, from [`Editor::begin_open`] to [`Editor::finish_open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenTicket(u64);

/// An open scene collection with its undo history, the scene whose sources are listed, and the
/// file open in progress.
pub struct Editor {
    engine: Engine,
    listed: SceneId,
    /// The collection as it was last opened or saved.
    saved: SceneCollection,
    last_ticket: u64,
    pending: Option<u64>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    /// A new collection with one empty scene.
    pub fn new() -> Self {
        let collection = new_collection();
        let listed = collection.program_scene;
        Self { saved: collection.clone(), engine: Engine::new(collection, Arc::new(Registry::new())), listed, last_ticket: 0, pending: None }
    }

    /// Replace the collection with a new one. A file being read is dropped when it arrives.
    pub fn new_collection(&mut self) {
        self.pending = None;
        self.load(new_collection());
    }

    /// Refuse a file of `bytes` bytes before it is read.
    pub fn check_size(bytes: u64) -> Result<(), EditorError> {
        if bytes > MAX_FILE_BYTES { Err(EditorError::TooLarge { bytes, max: MAX_FILE_BYTES }) } else { Ok(()) }
    }

    /// Start opening a file. The page reads it without waiting and passes the bytes to
    /// [`Editor::finish_open`] with the ticket returned here; the collection stays editable
    /// meanwhile. Starting another open replaces this one.
    pub fn begin_open(&mut self) -> OpenTicket {
        self.last_ticket = self.last_ticket.wrapping_add(1);
        self.pending = Some(self.last_ticket);
        OpenTicket(self.last_ticket)
    }

    /// End the open of `ticket` without a file (the read failed or was canceled). An older ticket
    /// changes nothing.
    pub fn cancel_open(&mut self, ticket: OpenTicket) {
        if self.pending == Some(ticket.0) {
            self.pending = None;
        }
    }

    /// Whether a file is being read.
    pub fn opening(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether `ticket` is the open in progress, whose bytes [`Editor::finish_open`] would load.
    pub fn open_pending(&self, ticket: OpenTicket) -> bool {
        self.pending == Some(ticket.0)
    }

    /// Replace the collection with the one in `bytes`, read for `ticket`. Returns `Ok(false)` and
    /// changes nothing when the open was replaced, canceled or already finished. A file that
    /// can't be read ends the open with an error, and the current collection stays.
    pub fn finish_open(&mut self, ticket: OpenTicket, bytes: &[u8]) -> Result<bool, EditorError> {
        if !self.open_pending(ticket) {
            return Ok(false);
        }
        self.pending = None;
        let collection = parse(bytes)?;
        self.load(collection);
        Ok(true)
    }

    /// Replace the collection with `collection`, read with [`parse`] for `ticket`. Returns
    /// `false` and changes nothing when the open was replaced, canceled or already finished.
    pub fn finish_open_with(&mut self, ticket: OpenTicket, collection: SceneCollection) -> bool {
        if !self.open_pending(ticket) {
            return false;
        }
        self.pending = None;
        self.load(collection);
        true
    }

    fn load(&mut self, collection: SceneCollection) {
        self.listed = collection.program_scene;
        self.saved = collection.clone();
        self.engine.load(collection);
    }

    pub fn collection(&self) -> &SceneCollection {
        self.engine.collection()
    }

    /// Whether the collection differs from the one last opened or saved. Undoing every edit back
    /// to it counts as unchanged.
    pub fn changed(&self) -> bool {
        self.engine.is_dirty() && self.engine.collection() != &self.saved
    }

    /// The scene whose sources are listed.
    pub fn listed_scene(&self) -> SceneId {
        self.listed
    }

    /// Run `action`.
    pub fn apply(&mut self, action: Action) -> Result<(), EditorError> {
        let scene = Some(SceneRef::Id(self.listed));
        let cmd = match action {
            Action::ListScene(id) => {
                if self.collection().scene(id).is_none() {
                    return Err(EngineError::NoScene(id.to_string()).into());
                }
                self.listed = id;
                return Ok(());
            }
            Action::AddScene { name } => Command::SceneAdd { name },
            Action::RenameScene(id, name) => Command::SceneRename { scene: SceneRef::Id(id), name },
            Action::RemoveScene(id) => Command::SceneRemove { scene: SceneRef::Id(id) },
            Action::MoveScene(id, to) => Command::SceneMove { scene: SceneRef::Id(id), to },
            Action::PlaceSource(id) => Command::SourceAddExisting { scene, source: SourceRef::Id(id) },
            Action::RenameSource(id, name) => Command::SourceRename { source: SourceRef::Id(id), name },
            Action::RemoveItem(id) => Command::ItemRemove { scene, item: ItemRef::Id(id) },
            Action::MoveItem(id, to) => Command::ItemMove { scene, item: ItemRef::Id(id), to },
            Action::Undo => Command::Undo {},
            Action::Redo => Command::Redo {},
        };
        let added = matches!(cmd, Command::SceneAdd { .. });
        let before = self.collection().scene_index(self.listed).unwrap_or(0);
        let outcome = self.engine.execute(cmd)?;
        if added && let Some(id) = outcome.result.get("scene").and_then(serde_json::Value::as_u64) {
            self.listed = SceneId(id);
        }
        // A removed scene, or one that undo took back, lists the scene that was above it.
        let c = self.engine.collection();
        if c.scene(self.listed).is_none()
            && let Some(s) = c.scenes.get(before.saturating_sub(1)).or(c.scenes.first())
        {
            self.listed = s.id;
        }
        Ok(())
    }

    /// What the page shows.
    pub fn view(&self) -> View {
        let c = self.collection();
        let scenes = c
            .scenes
            .iter()
            .map(|s| SceneRow { id: s.id, name: s.name.clone(), program: s.id == c.program_scene, listed: s.id == self.listed })
            .collect();
        let items = c
            .scene(self.listed)
            .map(|s| {
                s.items
                    .iter()
                    .rev()
                    .map(|i| {
                        let source = c.source(i.source);
                        ItemRow {
                            item: i.id,
                            source: i.source,
                            name: source.map(|s| s.name.clone()).unwrap_or_default(),
                            kind: source.map(|s| s.kind.clone()).unwrap_or_default(),
                            visible: i.visible,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sources = c.sources.iter().map(|s| SourceRow { id: s.id, name: s.name.clone(), kind: s.kind.clone() }).collect();
        View {
            collection: c.name.clone(),
            scenes,
            items,
            sources,
            undo: self.engine.undo_label(),
            redo: self.engine.redo_label(),
            changed: self.changed(),
            opening: self.opening(),
        }
    }

    /// The collection as a scene collection file's text in the current format, and the name to
    /// download it under. Call [`Editor::mark_saved`] once the download is offered.
    pub fn saved(&self) -> Result<Saved, EditorError> {
        let text = write_collection(self.collection())?;
        Self::check_size(u64::try_from(text.len()).unwrap_or(u64::MAX))?;
        Ok(Saved { file_name: file_name(&self.collection().name), text })
    }

    /// The collection counts as unchanged from here on.
    pub fn mark_saved(&mut self) {
        self.saved = self.engine.collection().clone();
        self.engine.mark_saved();
    }
}

fn new_collection() -> SceneCollection {
    SceneCollection::new(DEFAULT_COLLECTION, DEFAULT_SCENE)
}

/// The collection in a scene collection file's bytes, migrated and repaired as the app does when
/// it imports the file.
pub fn parse(bytes: &[u8]) -> Result<SceneCollection, EditorError> {
    Editor::check_size(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
    let text = std::str::from_utf8(bytes).map_err(|_| EditorError::NotText)?;
    Ok(read_collection(text, DEFAULT_SCENE)?)
}

/// The row of a redrawn list that takes the keyboard focus a control in row `row`, at `index`,
/// had: that row when the list still has it, else the row now at `index`, or the last row.
/// `None` when the list is empty.
pub fn refocus_row(row: u64, index: usize, rows: &[u64]) -> Option<usize> {
    rows.iter().position(|r| *r == row).or_else(|| rows.len().checked_sub(1).map(|last| index.min(last)))
}

/// The source Add to Scene places after the list of sources is drawn again: `selected` when the
/// collection still has it, else the first source.
pub fn kept_source(selected: Option<SourceId>, sources: &[SourceRow]) -> Option<SourceId> {
    selected.filter(|s| sources.iter().any(|r| r.id == *s)).or_else(|| sources.first().map(|r| r.id))
}

/// A download file name for a collection called `name`: the name with the characters file
/// systems refuse replaced by `_`, without leading or trailing spaces and dots, at most
/// [`MAX_FILE_STEM`] characters, with `.json`. A name that leaves nothing gives `Untitled.json`.
pub fn file_name(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .take(MAX_FILE_STEM)
        .collect();
    let stem = stem.trim_matches(|c: char| c == '.' || c.is_whitespace());
    format!("{}.json", if stem.is_empty() { DEFAULT_COLLECTION } else { stem })
}

#[cfg(test)]
mod tests;
