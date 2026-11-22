# StudioCraft: rules for contributors

This file holds the rules for everyone who changes StudioCraft, people and coding agents alike.
The repository does not include a `CLAUDE.md`. When a local `CLAUDE.md` or other working
instructions disagree with this file, this file wins.

Rules are numbered within their section. Rule 0.3 is the third rule in section 0. Code comments
cite a section by its heading, for example "AGENTS.md › Never crash", and a rule by its number,
for example "AGENTS.md 0.3".

## 0. Never crash

A malformed file, a bad command parameter, a missing device or a full disk must give an error the
user can act on. It must never crash the app or lose a scene collection.

1. Return `Result<T, E>` and propagate with `?`. In the window, show the error as a notice or in a
   dialog and carry on.
2. Outside tests, do not use `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!` or
   `unimplemented!`. The workspace lints deny them; `clippy.toml` allows them in tests.
3. Do not write `unsafe` code. The workspace sets `unsafe_code = "forbid"`. Only crates that call
   native hardware APIs may use `unsafe`, and `ALLOWED` in `xtask/src/unsafe_code.rs` lists them.
   `craft-hwenc-native` (hardware video encoding) is the one such crate in the workspace. The list
   also holds the names `craft-gpu-interop` (sharing captured GPU textures with the compositor) and
   `craft-hwdec` (hardware video decoding) for planned crates. These crates follow FilmCraft's
   `crates/platform` rule
   (https://github.com/storytold/filmcraft/blob/main/docs/adr/0001-platform-ffi.md). An allowed
   crate meets each of these requirements:
   - The crate does not use the workspace lints. Its own `[lints]` tables copy them, except that
     `unsafe_code` is `deny`, and add `unsafe_op_in_unsafe_fn = "deny"` and
     `clippy::undocumented_unsafe_blocks = "deny"`.
   - Its `src/lib.rs` starts with `#![deny(unsafe_op_in_unsafe_fn)]`.
   - Its unsafe code is in private foreign function interface (FFI) modules named `ffi` or
     `<name>_ffi`, each in its own file and declared as `#[allow(unsafe_code)] mod ffi;` (or
     `pub(crate) mod`).
   - Every `unsafe` block has a `// SAFETY:` comment that says why the block is sound.
   - The files outside the FFI modules hold no `unsafe`, no raw pointer types and no `pub use` from
     an FFI module, so the public API is safe. Failures are `Result`s.
   - No panic crosses the FFI boundary.
   - The crate builds on every target and reports the backend as unavailable where the operating
     system API does not exist.

   `cargo xtask unsafe` checks these rules for the listed crates, and fails when any other crate
   uses `unsafe` or relaxes `unsafe_code`.
4. Treat every number from a file, a device, a plugin setting or a control request as hostile:
   index with `get()`, use checked or saturating arithmetic, never divide by a value that can be
   zero, and cap allocations sized by input.
5. Every crash fix comes with a test that failed before the fix.

## 1. Clean room

1. Learn how other streaming and recording software behaves only by using it and from its public
   documentation. Do not read its source code, its repository or its installed files.
2. Copy no code, text, icons, shaders or other assets from software outside the ArtCraft family.
   Code and assets from the ArtCraft Craft apps (https://github.com/storytold) and from craft-libs
   may be reused under their licenses. The crate that holds reused code names each source in its
   `NOTICE` file: the repository, the path, the commit, the license and the copyright line. Write
   every interface string in your own words. Draw icons from shapes in code
   (`crates/ui-egui/src/icons.rs`).
3. Do not name other products in the app or its documentation. `cargo xtask brands` checks this.
   `MECHANICS.md` and `mechanics.json` may name the product a value was compared with, and a
   crate's `NOTICE` file names the ArtCraft code it reuses.

## 2. Dependencies and assets

1. Every dependency must be licensed under the MIT License, Apache-2.0, BSD, Zlib, ISC, Unicode or
   a similar permissive license. No GPL, LGPL or AGPL code may be linked or copied.
   `cargo xtask licenses` checks this. ffmpeg runs as a separate program and is never linked or
   shipped.
2. The interface faces Inter Regular, Inter SemiBold and JetBrains Mono Regular are in
   `assets/fonts` with their OFL-1.1 license texts. Every other font comes from
   [craft-fonts](https://github.com/storytold/craft-fonts) through `CRAFT_FONTS_DIR`. Do not add
   other font files to this repository.
3. Every picture, sound, font or video file in the repository is listed in `ASSETS.md` with its
   author, source and license. `cargo xtask assets` checks this.

## 3. Structure

1. Respect the layers in `docs/architecture.md`. `cargo xtask layers` checks them.
2. Every change to a scene collection is a command with a stable id in `crates/engine`. Menus,
   shortcuts, the control channel and the Model Context Protocol (MCP) server run commands by id.
3. A new source, filter, transition, encoder or output implements a trait in
   `crates/plugin` and registers in the `Registry`. It must not need changes to the window code.
4. A change to a saved file's shape bumps its version and adds a migration in
   `crates/model/src/format.rs`, with a test that reads the old version.
5. The rules for the frame loop, the mixer loop, threads and queues are in
   [section 6](#6-performance).
6. Every string the window shows goes through `tl!`, `tlc!`, `tid!` or `tmsg!` and has a row in
   `crates/ui-egui/i18n/locales/en/studiocraft.tsv`. The i18n tests fail on a string without one.

## 4. Before every commit

Run `cargo xtask ci`. It runs these steps in this order and stops at the first failure:

1. `fmt` checks the formatting (`cargo fmt --all --check`).
2. `clippy` runs clippy with warnings as errors.
3. `test` runs the workspace tests.
4. `layers` checks the layers in `docs/architecture.md`.
5. `licenses` checks the dependency licenses.
6. `unsafe` checks the `unsafe` allowlist from rule 0.3.
7. `assets` checks that `ASSETS.md` lists every asset file.
8. `brands` checks that the app's text and docs name no other vendor's products.
9. `wasm` checks that the crates that build for the web compile for WebAssembly.

Each step also runs on its own, for example `cargo xtask assets`.

## 5. Repository text

The README, docs and commit messages describe what the app does, in plain American English.

## 6. Performance

1. The frame loop and the mixer loop do not allocate. Frames and audio blocks come from pools.
2. Frames stay on the GPU between the compositor and the window.
3. Threads share data through bounded lock-free queues or atomics.

`docs/performance.md` holds the budgets, the recorded baselines and how to measure them.
