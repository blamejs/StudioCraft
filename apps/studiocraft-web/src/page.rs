//! The page: one click handler for every button (each names its action in `data-action` and the
//! scene, source or scene item in `data-id`), the file picker, the download, and the lists drawn
//! again from [`Editor::view`] after every change. Names are set as text, never as markup.

use std::cell::RefCell;
use std::rc::Rc;

use studiocraft_engine::Move;
use studiocraft_model::{ItemId, SceneId, SourceId};
use studiocraft_web::{Action, Editor, OpenTicket, View};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Blob, BlobPropertyBag, Document, Element, Event, File, HtmlAnchorElement, HtmlElement, HtmlInputElement, HtmlSelectElement, Url};

/// What the status line says.
enum Status {
    Clear,
    Info(String),
    Error(String),
}

struct Page {
    editor: Editor,
    doc: Document,
    status: Status,
    /// The address of the last download, released when the next one is made.
    download: Option<String>,
}

type Shared = Rc<RefCell<Page>>;

pub fn start() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
    let page = Rc::new(RefCell::new(Page { editor: Editor::new(), doc: doc.clone(), status: Status::Clear, download: None }));

    let p = Rc::clone(&page);
    listen(&doc, "click", move |e| on_click(&p, &e));
    if let Some(input) = doc.get_element_by_id("file") {
        let p = Rc::clone(&page);
        listen(&input, "change", move |_| on_file(&p));
    }
    if let Some(w) = web_sys::window() {
        let p = Rc::clone(&page);
        // Leaving the page with unsaved changes asks first.
        listen(&w, "beforeunload", move |e| {
            if p.try_borrow().is_ok_and(|p| p.editor.changed()) {
                e.prevent_default();
            }
        });
    }
    render(&page.borrow());
}

/// Call `f` for each `event` on `target` for as long as the page is open.
fn listen(target: &web_sys::EventTarget, event: &str, f: impl FnMut(Event) + 'static) {
    let closure = Closure::<dyn FnMut(Event)>::new(f);
    if target.add_event_listener_with_callback(event, closure.as_ref().unchecked_ref()).is_ok() {
        // The handler lives as long as the page.
        closure.forget();
    }
}

fn by_id<T: JsCast>(doc: &Document, id: &str) -> Option<T> {
    doc.get_element_by_id(id).and_then(|e| e.dyn_into::<T>().ok())
}

fn on_click(page: &Shared, e: &Event) {
    let Some(button) = e.target().and_then(|t| t.dyn_into::<Element>().ok()).and_then(|t| t.closest("[data-action]").ok().flatten()) else {
        return;
    };
    if button.has_attribute("disabled") {
        return;
    }
    let action = button.get_attribute("data-action").unwrap_or_default();
    let id = button.get_attribute("data-id").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let Ok(mut p) = page.try_borrow_mut() else { return };
    let edit = match action.as_str() {
        "new" => {
            if p.editor.changed() && !confirm("Discard the unsaved changes and start a new collection?") {
                return;
            }
            p.editor.new_collection();
            p.status = Status::Clear;
            None
        }
        "open" => {
            if let Some(input) = by_id::<HtmlElement>(&p.doc, "file") {
                input.click();
            }
            return;
        }
        "save" => {
            save(&mut p);
            None
        }
        "undo" => Some(Action::Undo),
        "redo" => Some(Action::Redo),
        "add-scene" => {
            let field = by_id::<HtmlInputElement>(&p.doc, "scene-name");
            let typed = field.as_ref().map(HtmlInputElement::value).unwrap_or_default();
            // An empty field leaves the name to the editor. A refused name stays in the field.
            let added = p.editor.apply(Action::AddScene { name: (!typed.trim().is_empty()).then_some(typed) });
            p.status = match added {
                Ok(()) => {
                    if let Some(f) = &field {
                        f.set_value("");
                    }
                    Status::Clear
                }
                Err(e) => Status::Error(e.to_string()),
            };
            None
        }
        "list-scene" => Some(Action::ListScene(SceneId(id))),
        "scene-up" => Some(Action::MoveScene(SceneId(id), Move::Up)),
        "scene-down" => Some(Action::MoveScene(SceneId(id), Move::Down)),
        "scene-remove" => Some(Action::RemoveScene(SceneId(id))),
        "scene-rename" => {
            let current = p.editor.collection().scene(SceneId(id)).map(|s| s.name.clone()).unwrap_or_default();
            ask("Scene name", &current).map(|name| Action::RenameScene(SceneId(id), name))
        }
        "place-source" => {
            let picked = by_id::<HtmlSelectElement>(&p.doc, "place").map(|s| s.value()).and_then(|v| v.parse::<u64>().ok());
            picked.map(|s| Action::PlaceSource(SourceId(s)))
        }
        "source-rename" => {
            let current = p.editor.collection().source(SourceId(id)).map(|s| s.name.clone()).unwrap_or_default();
            ask("Source name", &current).map(|name| Action::RenameSource(SourceId(id), name))
        }
        "item-up" => Some(Action::MoveItem(ItemId(id), Move::Up)),
        "item-down" => Some(Action::MoveItem(ItemId(id), Move::Down)),
        "item-remove" => Some(Action::RemoveItem(ItemId(id))),
        _ => return,
    };
    if let Some(edit) = edit {
        p.status = match p.editor.apply(edit) {
            Ok(()) => Status::Clear,
            Err(e) => Status::Error(e.to_string()),
        };
    }
    render(&p);
}

/// Whether the user accepts `message`; `false` when the browser shows no dialog.
fn confirm(message: &str) -> bool {
    web_sys::window().and_then(|w| w.confirm_with_message(message).ok()).unwrap_or(false)
}

/// The text the user types into a prompt showing `current`; `None` when the prompt is canceled.
fn ask(message: &str, current: &str) -> Option<String> {
    web_sys::window().and_then(|w| w.prompt_with_message_and_default(message, current).ok().flatten())
}

/// Read the picked file without waiting; the editor stays usable until its bytes arrive.
fn on_file(page: &Shared) {
    let Ok(mut p) = page.try_borrow_mut() else { return };
    let Some(input) = by_id::<HtmlInputElement>(&p.doc, "file") else { return };
    let file = input.files().and_then(|f| f.get(0));
    // Picking the same file again fires `change` again.
    input.set_value("");
    let Some(file) = file else { return };
    let name = file.name();
    // This file replaces any open still being read, even when it is refused.
    let ticket = p.editor.begin_open();
    // `size` is a whole number of bytes; a negative or too large value saturates.
    if let Err(e) = Editor::check_size(file.size() as u64) {
        p.editor.cancel_open(ticket);
        p.status = Status::Error(format!("{name} could not be opened: {e}"));
        render(&p);
        return;
    }
    p.status = Status::Info(format!("Opening {name}\u{2026}"));
    render(&p);
    drop(p);
    let page = Rc::clone(page);
    wasm_bindgen_futures::spawn_local(async move {
        let read = read_bytes(&file).await;
        finish(&page, ticket, &name, read);
    });
}

async fn read_bytes(file: &File) -> Result<Vec<u8>, String> {
    let buffer = wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await.map_err(|e| js_text(&e))?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

fn finish(page: &Shared, ticket: OpenTicket, name: &str, read: Result<Vec<u8>, String>) {
    let Ok(mut p) = page.try_borrow_mut() else { return };
    // A newer open or New replaced this one.
    if !p.editor.open_pending(ticket) {
        return;
    }
    let parsed = read
        .map_err(|e| format!("{name} could not be read: {e}"))
        .and_then(|bytes| studiocraft_web::parse(&bytes).map_err(|e| format!("{name} could not be opened: {e}")));
    p.status = match parsed {
        Err(e) => {
            p.editor.cancel_open(ticket);
            Status::Error(e)
        }
        // Unsaved changes, made before the open or while the file was read, are replaced only
        // when the user agrees.
        Ok(_) if p.editor.changed() && !confirm(&format!("Discard the unsaved changes and open {name}?")) => {
            p.editor.cancel_open(ticket);
            Status::Info(format!("{name} was not opened"))
        }
        Ok(collection) => {
            p.editor.finish_open_with(ticket, collection);
            Status::Info(format!("Opened {name}"))
        }
    };
    render(&p);
}

fn js_text(v: &JsValue) -> String {
    v.as_string().or_else(|| js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string())).unwrap_or_else(|| "the browser gave no reason".into())
}

/// Offer the collection as a download, and count it as saved once the download is offered.
fn save(p: &mut Page) {
    let saved = match p.editor.saved() {
        Ok(s) => s,
        Err(e) => {
            p.status = Status::Error(format!("The collection could not be saved: {e}"));
            return;
        }
    };
    match download(&p.doc, &saved.file_name, &saved.text) {
        Ok(url) => {
            if let Some(old) = p.download.replace(url) {
                let _ = Url::revoke_object_url(&old);
            }
            p.editor.mark_saved();
            p.status = Status::Info(format!("Saved {}", saved.file_name));
        }
        Err(e) => p.status = Status::Error(format!("The collection could not be saved: {}", js_text(&e))),
    }
}

/// Start a download of `text` as `file_name`; returns the address made for it.
fn download(doc: &Document, file_name: &str, text: &str) -> Result<String, JsValue> {
    let parts = js_sys::Array::of1(&JsValue::from_str(text));
    let options = BlobPropertyBag::new();
    options.set_type("application/json");
    let blob = Blob::new_with_str_sequence_and_options(&parts, &options)?;
    let url = Url::create_object_url_with_blob(&blob)?;
    let link: HtmlAnchorElement = doc.create_element("a")?.dyn_into().map_err(JsValue::from)?;
    link.set_href(&url);
    link.set_download(file_name);
    link.click();
    Ok(url)
}

fn render(p: &Page) {
    let v = p.editor.view();
    let doc = &p.doc;
    set_text(doc, "collection-name", &v.collection);
    set_text(doc, "changed", if v.changed { "Unsaved changes" } else { "" });
    let (text, class) = match &p.status {
        Status::Clear => ("", "status"),
        Status::Info(t) => (t.as_str(), "status"),
        Status::Error(t) => (t.as_str(), "status error"),
    };
    set_text(doc, "status", text);
    if let Some(s) = doc.get_element_by_id("status") {
        s.set_class_name(class);
    }
    step_button(doc, "undo", "Undo", v.undo);
    step_button(doc, "redo", "Redo", v.redo);
    if let Some(list) = doc.get_element_by_id("scenes") {
        let _ = scene_rows(doc, &list, &v);
    }
    let listed = v.scenes.iter().find(|s| s.listed).map(|s| s.name.as_str()).unwrap_or_default();
    set_text(doc, "listed-scene", listed);
    if let Some(list) = doc.get_element_by_id("items") {
        let _ = item_rows(doc, &list, &v);
    }
    if let Some(select) = doc.get_element_by_id("place") {
        let _ = source_options(doc, &select, &v);
    }
}

fn set_text(doc: &Document, id: &str, text: &str) {
    if let Some(e) = doc.get_element_by_id(id) {
        e.set_text_content(Some(text));
    }
}

fn step_button(doc: &Document, id: &str, verb: &str, label: Option<&str>) {
    let Some(b) = doc.get_element_by_id(id) else { return };
    match label {
        Some(l) => {
            let _ = b.remove_attribute("disabled");
            let _ = b.set_attribute("title", &format!("{verb} {l}"));
        }
        None => {
            let _ = b.set_attribute("disabled", "");
            let _ = b.remove_attribute("title");
        }
    }
}

fn button(doc: &Document, text: &str, action: &str, id: u64) -> Result<Element, JsValue> {
    let b = doc.create_element("button")?;
    b.set_attribute("type", "button")?;
    b.set_attribute("data-action", action)?;
    b.set_attribute("data-id", &id.to_string())?;
    b.set_text_content(Some(text));
    Ok(b)
}

fn scene_rows(doc: &Document, list: &Element, v: &View) -> Result<(), JsValue> {
    list.set_inner_html("");
    for s in &v.scenes {
        let row = doc.create_element("li")?;
        row.set_class_name(if s.listed { "listed" } else { "" });
        let name = button(doc, &s.name, "list-scene", s.id.0)?;
        name.set_class_name("name");
        if s.listed {
            name.set_attribute("aria-current", "true")?;
        }
        row.append_child(&name)?;
        if s.program {
            let badge = doc.create_element("span")?;
            badge.set_class_name("badge");
            badge.set_text_content(Some("Program"));
            row.append_child(&badge)?;
        }
        for (text, action) in [("Up", "scene-up"), ("Down", "scene-down"), ("Rename", "scene-rename"), ("Remove", "scene-remove")] {
            let b = button(doc, text, action, s.id.0)?;
            row.append_child(&b)?;
        }
        list.append_child(&row)?;
    }
    Ok(())
}

fn item_rows(doc: &Document, list: &Element, v: &View) -> Result<(), JsValue> {
    list.set_inner_html("");
    for i in &v.items {
        let row = doc.create_element("li")?;
        let name = doc.create_element("span")?;
        name.set_class_name(if i.visible { "name" } else { "name hidden" });
        name.set_text_content(Some(&i.name));
        row.append_child(&name)?;
        let kind = doc.create_element("span")?;
        kind.set_class_name("kind");
        kind.set_text_content(Some(&i.kind));
        row.append_child(&kind)?;
        for (text, action) in [("Up", "item-up"), ("Down", "item-down"), ("Remove", "item-remove")] {
            let b = button(doc, text, action, i.item.0)?;
            row.append_child(&b)?;
        }
        let rename = button(doc, "Rename", "source-rename", i.source.0)?;
        row.append_child(&rename)?;
        list.append_child(&row)?;
    }
    Ok(())
}

fn source_options(doc: &Document, select: &Element, v: &View) -> Result<(), JsValue> {
    select.set_inner_html("");
    for s in &v.sources {
        let option = doc.create_element("option")?;
        option.set_attribute("value", &s.id.0.to_string())?;
        option.set_text_content(Some(&s.name));
        select.append_child(&option)?;
    }
    if let Some(b) = doc.get_element_by_id("place-source") {
        if v.sources.is_empty() { b.set_attribute("disabled", "")? } else { b.remove_attribute("disabled")? }
    }
    Ok(())
}
