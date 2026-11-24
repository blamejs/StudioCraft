use studiocraft_engine::{EngineError, Move};
use studiocraft_model::format::{FormatError, write_collection};
use studiocraft_model::{ItemId, SceneId, SourceId};

use super::*;

/// The oldest scene collection format: scenes Main (Background, Title) and Break (empty), and a
/// microphone that no scene places.
const V1: &str = include_str!("../../../crates/model/fixtures/collection-v1.json");

fn opened() -> Editor {
    let mut e = Editor::new();
    let t = e.begin_open();
    assert_eq!(e.finish_open(t, V1.as_bytes()), Ok(true));
    e
}

fn scene_names(e: &Editor) -> Vec<String> {
    e.view().scenes.into_iter().map(|s| s.name).collect()
}

fn item_names(e: &Editor) -> Vec<String> {
    e.view().items.into_iter().map(|i| i.name).collect()
}

#[test]
fn a_file_of_the_first_version_opens_and_lists_its_scenes_and_sources() {
    let e = opened();
    let v = e.view();
    assert_eq!(v.collection, "Fixture");
    assert_eq!(scene_names(&e), ["Main", "Break"]);
    assert!(v.scenes[0].program && v.scenes[0].listed);
    // The top of the picture (the last item) is listed first.
    assert_eq!(item_names(&e), ["Title", "Background"]);
    assert_eq!(v.items[1].kind, "color");
    assert_eq!(v.sources.len(), 3);
    assert!(!v.changed && !v.opening);
    assert_eq!(v.undo, None);
}

#[test]
fn scenes_are_added_renamed_moved_and_removed() {
    let mut e = opened();
    e.apply(Action::AddScene { name: None }).unwrap();
    assert_eq!(scene_names(&e), ["Main", "Break", "Scene"]);
    assert_eq!(e.listed_scene(), e.view().scenes[2].id, "a new scene is listed");
    e.apply(Action::AddScene { name: None }).unwrap();
    e.apply(Action::AddScene { name: Some(" Outro ".into()) }).unwrap();
    assert_eq!(scene_names(&e), ["Main", "Break", "Scene", "Scene 2", "Outro"], "a missing name is numbered, a given one trimmed");

    let brk = e.collection().scene_by_name("Break").unwrap().id;
    e.apply(Action::RenameScene(brk, "Intermission".into())).unwrap();
    e.apply(Action::MoveScene(brk, Move::Up)).unwrap();
    assert_eq!(scene_names(&e), ["Intermission", "Main", "Scene", "Scene 2", "Outro"]);
    // At the top of the list it stays.
    e.apply(Action::MoveScene(brk, Move::Up)).unwrap();
    assert_eq!(scene_names(&e)[0], "Intermission");
    e.apply(Action::MoveScene(brk, Move::Bottom)).unwrap();
    assert_eq!(scene_names(&e), ["Main", "Scene", "Scene 2", "Outro", "Intermission"]);

    e.apply(Action::RemoveScene(brk)).unwrap();
    assert_eq!(scene_names(&e), ["Main", "Scene", "Scene 2", "Outro"]);
    assert!(e.view().changed);
}

#[test]
fn removing_the_listed_scene_lists_the_scene_above_it() {
    let mut e = opened();
    let main = e.listed_scene();
    e.apply(Action::AddScene { name: Some("Third".into()) }).unwrap();
    let brk = e.collection().scene_by_name("Break").unwrap().id;
    e.apply(Action::ListScene(brk)).unwrap();
    e.apply(Action::RemoveScene(brk)).unwrap();
    assert_eq!(e.listed_scene(), main);
    // The first scene has none above it: the new first scene is listed.
    e.apply(Action::RemoveScene(main)).unwrap();
    assert_eq!(e.listed_scene(), e.collection().scene_by_name("Third").unwrap().id);
    // Undo brings the scene back; the listed scene still exists, so it stays.
    e.apply(Action::Undo).unwrap();
    assert_eq!(scene_names(&e), ["Main", "Third"]);
    assert_eq!(e.listed_scene(), e.collection().scene_by_name("Third").unwrap().id);
}

#[test]
fn undoing_the_new_listed_scene_lists_the_scene_above_it() {
    let mut e = opened();
    e.apply(Action::AddScene { name: None }).unwrap();
    e.apply(Action::Undo).unwrap();
    assert_eq!(scene_names(&e), ["Main", "Break"]);
    assert_eq!(e.listed_scene(), e.collection().scene_by_name("Break").unwrap().id);
    assert!(e.collection().scene(e.listed_scene()).is_some());
}

#[test]
fn sources_are_placed_renamed_reordered_and_removed_in_the_listed_scene() {
    let mut e = opened();
    let brk = e.collection().scene_by_name("Break").unwrap().id;
    let background = e.collection().source_by_name("Background").unwrap().id;
    let title = e.collection().source_by_name("Title").unwrap().id;
    e.apply(Action::ListScene(brk)).unwrap();
    assert!(item_names(&e).is_empty());
    e.apply(Action::PlaceSource(background)).unwrap();
    e.apply(Action::PlaceSource(title)).unwrap();
    assert_eq!(item_names(&e), ["Title", "Background"]);

    let bottom = e.view().items[1].item;
    e.apply(Action::MoveItem(bottom, Move::Up)).unwrap();
    assert_eq!(item_names(&e), ["Background", "Title"]);
    // At the top of the list it stays.
    e.apply(Action::MoveItem(bottom, Move::Up)).unwrap();
    assert_eq!(item_names(&e), ["Background", "Title"]);

    // A rename shows in every scene that places the source.
    e.apply(Action::RenameSource(title, "Headline".into())).unwrap();
    assert_eq!(item_names(&e), ["Background", "Headline"]);
    e.apply(Action::ListScene(e.collection().program_scene)).unwrap();
    assert_eq!(item_names(&e), ["Headline", "Background"]);

    // Removing an item from one scene keeps a source another scene places.
    let item = e.view().items[0].item;
    e.apply(Action::RemoveItem(item)).unwrap();
    assert_eq!(item_names(&e), ["Background"]);
    assert!(e.collection().source(title).is_some());
    // Removing the last placement removes the source.
    e.apply(Action::ListScene(brk)).unwrap();
    let headline = e.view().items.iter().find(|i| i.source == title).unwrap().item;
    e.apply(Action::RemoveItem(headline)).unwrap();
    assert!(e.collection().source(title).is_none());
}

#[test]
fn bad_edits_are_errors_and_change_nothing() {
    let mut e = opened();
    let before = e.collection().clone();
    let main = e.listed_scene();
    let brk = e.collection().scene_by_name("Break").unwrap().id;
    let cases = [
        Action::ListScene(SceneId(999)),
        Action::RenameScene(SceneId(999), "X".into()),
        Action::RenameScene(main, "Break".into()),
        Action::RenameScene(main, "   ".into()),
        Action::RenameScene(main, "a\nb".into()),
        Action::RenameScene(main, "x".repeat(257)),
        Action::RemoveScene(SceneId(999)),
        Action::MoveScene(SceneId(999), Move::Up),
        Action::PlaceSource(SourceId(999)),
        Action::RenameSource(SourceId(999), "X".into()),
        Action::RenameSource(e.collection().source_by_name("Title").unwrap().id, "Background".into()),
        Action::RemoveItem(ItemId(999)),
        Action::MoveItem(ItemId(999), Move::Down),
        Action::AddScene { name: Some(String::new()) },
        Action::AddScene { name: Some("Main".into()) },
    ];
    for a in cases {
        assert!(e.apply(a.clone()).is_err(), "{a:?}");
        assert_eq!(e.collection(), &before, "{a:?}");
        assert_eq!(e.listed_scene(), main, "{a:?}");
    }
    e.apply(Action::RemoveScene(brk)).unwrap();
    assert_eq!(e.apply(Action::RemoveScene(main)), Err(EditorError::Edit(EngineError::LastScene)));
    assert_eq!(scene_names(&e), ["Main"]);
}

#[test]
fn an_item_of_another_scene_is_not_in_the_listed_scene() {
    let mut e = opened();
    let item = e.view().items[0].item;
    let brk = e.collection().scene_by_name("Break").unwrap().id;
    e.apply(Action::ListScene(brk)).unwrap();
    assert!(e.apply(Action::RemoveItem(item)).is_err());
    assert!(e.apply(Action::MoveItem(item, Move::Up)).is_err());
}

#[test]
fn undo_and_redo_name_their_step() {
    let mut e = opened();
    let main = e.listed_scene();
    e.apply(Action::RenameScene(main, "Live".into())).unwrap();
    assert_eq!(e.view().undo, Some("Rename Scene"));
    assert!(e.changed());
    e.apply(Action::Undo).unwrap();
    assert_eq!(scene_names(&e), ["Main", "Break"]);
    assert_eq!(e.view().redo, Some("Rename Scene"));
    assert!(!e.changed(), "undone back to the opened file");
    e.apply(Action::Redo).unwrap();
    assert_eq!(scene_names(&e), ["Live", "Break"]);
    assert!(e.changed());
    // After a save, undoing past it is a change again.
    e.mark_saved();
    e.apply(Action::Undo).unwrap();
    assert!(e.changed());
    e.apply(Action::Redo).unwrap();
    assert!(!e.changed(), "back to the saved collection");
}

#[test]
fn a_saved_collection_opens_again_with_its_edits() {
    let mut e = opened();
    let main = e.listed_scene();
    e.apply(Action::RenameScene(main, "Live".into())).unwrap();
    e.apply(Action::AddScene { name: Some("Outro".into()) }).unwrap();
    let saved = e.saved().unwrap();
    assert_eq!(saved.file_name, "Fixture.json");
    assert!(saved.text.contains("\"version\""));
    assert!(e.view().changed, "the collection counts as changed until the download is offered");
    e.mark_saved();
    assert!(!e.view().changed);

    let back = parse(saved.text.as_bytes()).unwrap();
    assert_eq!(&back, e.collection());
    assert!(back.problems().is_empty(), "{:?}", back.problems());
    // The current format: saving the opened file again gives the same text.
    assert_eq!(write_collection(&back).unwrap(), saved.text);
}

#[test]
fn edits_go_on_while_a_file_is_read_and_its_bytes_replace_them_when_they_arrive() {
    let mut e = Editor::new();
    let t = e.begin_open();
    assert!(e.view().opening);
    // The read is late: the editor keeps working on the current collection meanwhile.
    e.apply(Action::AddScene { name: Some("Draft".into()) }).unwrap();
    assert_eq!(scene_names(&e), ["Scene", "Draft"]);
    assert!(e.saved().is_ok());
    assert_eq!(e.finish_open(t, V1.as_bytes()), Ok(true));
    assert_eq!(scene_names(&e), ["Main", "Break"]);
    assert!(!e.view().opening && !e.view().changed);
    assert_eq!(e.view().undo, None, "an opened file starts a new history");
    // The same bytes again, for a finished open, change nothing.
    e.apply(Action::AddScene { name: None }).unwrap();
    assert_eq!(e.finish_open(t, V1.as_bytes()), Ok(false));
    assert_eq!(scene_names(&e), ["Main", "Break", "Scene"]);
}

#[test]
fn a_replaced_or_canceled_open_drops_its_bytes() {
    let mut e = Editor::new();
    let first = e.begin_open();
    let second = e.begin_open();
    assert!(!e.open_pending(first) && e.open_pending(second));
    assert_eq!(e.finish_open(first, V1.as_bytes()), Ok(false), "a newer open replaced it");
    assert_eq!(scene_names(&e), ["Scene"]);
    assert!(e.opening());
    e.cancel_open(first);
    assert!(e.opening(), "an older ticket does not cancel the newer open");
    e.cancel_open(second);
    assert!(!e.opening());
    assert_eq!(e.finish_open(second, V1.as_bytes()), Ok(false));
    assert_eq!(scene_names(&e), ["Scene"]);

    let third = e.begin_open();
    e.new_collection();
    assert!(!e.opening());
    assert_eq!(e.finish_open(third, V1.as_bytes()), Ok(false), "New replaced the open");
}

#[test]
fn a_file_read_first_is_loaded_only_for_the_open_in_progress() {
    let mut e = Editor::new();
    let first = e.begin_open();
    let parsed = parse(V1.as_bytes()).unwrap();
    let second = e.begin_open();
    assert!(!e.finish_open_with(first, parsed.clone()), "a newer open replaced it");
    assert_eq!(scene_names(&e), ["Scene"]);
    assert!(e.finish_open_with(second, parsed.clone()));
    assert_eq!(scene_names(&e), ["Main", "Break"]);
    assert!(!e.opening() && !e.changed());
    assert!(!e.finish_open_with(second, parsed), "the open already finished");
}

#[test]
fn a_file_that_cannot_be_read_ends_the_open_and_keeps_the_collection() {
    let mut e = opened();
    e.apply(Action::AddScene { name: Some("Kept".into()) }).unwrap();
    let before = e.collection().clone();
    let newer = V1.replace("\"version\": 1", "\"version\": 4000000");
    let profile = V1.replace("studiocraft.scene-collection", "studiocraft.profile");
    let dangling = V1.replace("\"id\": 2,", "\"id\": 1,");
    let bad: [&[u8]; 6] = [b"", b"{", &[0xff, 0xfe, 0x00], newer.as_bytes(), profile.as_bytes(), dangling.as_bytes()];
    for bytes in bad {
        let t = e.begin_open();
        assert!(e.finish_open(t, bytes).is_err(), "{:?}", String::from_utf8_lossy(bytes));
        assert!(!e.opening());
        assert_eq!(e.collection(), &before);
    }
    let t = e.begin_open();
    assert_eq!(e.finish_open(t, &[0xff, 0xfe]), Err(EditorError::NotText));
    let t = e.begin_open();
    assert!(matches!(e.finish_open(t, newer.as_bytes()), Err(EditorError::File(FormatError::TooNew { .. }))));
}

#[test]
fn a_file_larger_than_the_limit_is_refused_before_it_is_read() {
    assert_eq!(Editor::check_size(MAX_FILE_BYTES), Ok(()));
    assert_eq!(Editor::check_size(MAX_FILE_BYTES + 1), Err(EditorError::TooLarge { bytes: MAX_FILE_BYTES + 1, max: MAX_FILE_BYTES }));
    assert!(Editor::check_size(u64::MAX).is_err());
    let mut e = opened();
    let t = e.begin_open();
    let huge = vec![b' '; usize::try_from(MAX_FILE_BYTES).unwrap() + 1];
    assert!(matches!(e.finish_open(t, &huge), Err(EditorError::TooLarge { .. })));
    assert_eq!(scene_names(&e), ["Main", "Break"]);
}

#[test]
fn download_names_keep_only_characters_file_systems_take() {
    assert_eq!(file_name("Show"), "Show.json");
    assert_eq!(file_name("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j.json");
    assert_eq!(file_name("../../etc/passwd"), "_.._etc_passwd.json");
    assert_eq!(file_name("tab\there"), "tab_here.json");
    assert_eq!(file_name("  . "), "Untitled.json");
    assert_eq!(file_name(""), "Untitled.json");
    assert_eq!(file_name("Late show."), "Late show.json");
    assert_eq!(file_name("Stream 日本語"), "Stream 日本語.json");
    let long = "x".repeat(300);
    assert_eq!(file_name(&long), format!("{}.json", "x".repeat(MAX_FILE_STEM)));
}

#[test]
fn a_new_collection_has_one_scene_and_no_history() {
    let mut e = opened();
    e.apply(Action::AddScene { name: None }).unwrap();
    e.new_collection();
    let v = e.view();
    assert_eq!(v.collection, DEFAULT_COLLECTION);
    assert_eq!(scene_names(&e), [DEFAULT_SCENE]);
    assert!(v.items.is_empty() && v.sources.is_empty() && v.undo.is_none() && !v.changed);
    assert_eq!(e.saved().unwrap().file_name, "Untitled.json");
}
