//! The browser page of the scene collection editor (`index.html`).
//!
//! Build and serve it with `trunk serve` in `apps/studiocraft-web`. The page opens a file the user
//! picks, lists and edits its scenes and sources through [`studiocraft_web::Editor`], and saves
//! the edited collection as a download.

#[cfg(target_arch = "wasm32")]
mod page;

#[cfg(target_arch = "wasm32")]
fn main() {
    page::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("studiocraft-web runs in the browser: build it with `trunk build --release` in apps/studiocraft-web");
}
