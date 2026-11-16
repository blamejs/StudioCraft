# StudioCraft: rules for contributors

These rules apply to every contributor. `CLAUDE.md` holds the working instructions; this file holds
the rules that must not be broken. When the two disagree, this file wins.

## 0. Never crash

A malformed file, a bad command parameter, a missing device or a full disk must give an error the
user can act on. It must never crash the app or lose a scene collection.

1. Return `Result<T, E>` and propagate with `?`. In the window, show the error in the status bar or
   a dialog and carry on.
2. Outside tests, do not use `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!` or
   `unimplemented!`. The workspace lints deny them; `clippy.toml` allows them in tests.
3. Do not write `unsafe` code. The workspace sets `unsafe_code = "forbid"`.
4. Treat every number from a file, a device, a plugin setting or a control request as hostile:
   index with `get()`, use checked or saturating arithmetic, never divide by a value that can be
   zero, and cap allocations sized by input.
5. Every crash fix comes with a test that failed before the fix.

## 1. Clean room

1. Learn how other streaming and recording software behaves only by using it and from its public
   documentation. Do not read its source code, its repository or its installed files.
2. Copy no code, text, icons, shaders or other assets from other software. Write every interface
   string in your own words. Draw icons from shapes in code (`crates/ui-egui/src/icons.rs`).
3. Do not name other products in the app or its documentation. `cargo xtask brands` checks this.
   `MECHANICS.md` and `mechanics.json` may name the product a value was compared with.

## 2. Dependencies and assets

1. Every dependency must be licensed MIT, Apache-2.0, BSD, Zlib, ISC, Unicode or a similar
   permissive license. No GPL, LGPL or AGPL code, linked or copied. `cargo xtask licenses` checks
   this. ffmpeg runs as a separate program and is never linked or shipped.
2. The interface faces Inter Regular, Inter SemiBold and JetBrains Mono Regular are in
   `assets/fonts` with their OFL-1.1 license texts. Every other font comes from
   [craft-fonts](https://github.com/storytold/craft-fonts) through `CRAFT_FONTS_DIR`. Do not add
   other font files to this repository.
3. Every picture, sound, font or video file in the repository is listed in `ASSETS.md` with its
   author, source and license. `cargo xtask assets` checks this.

## 3. Structure

1. Respect the layers in `docs/architecture.md`. `cargo xtask layers` checks them.
2. Every change to a scene collection is a command with a stable id in `crates/engine`. Menus,
   shortcuts, the control channel and the MCP server run commands by id.
3. A new source, filter, transition, encoder or output implements a trait in
   `crates/plugin` and registers in the `Registry`. It must not need changes to the window code.
4. A change to a saved file's shape bumps its version and adds a migration in
   `crates/model/src/format.rs`, with a test that reads the old version.
5. The frame loop and the mixer loop do not allocate. Frames stay on the GPU between the compositor
   and the window. Threads share data through bounded lock-free queues or atomics.
6. Every string the window shows goes through `tl!`, `tlc!`, `tid!` or `tmsg!` and has a row in
   `crates/ui-egui/i18n/locales/en/studiocraft.tsv`. The i18n tests fail on a string without one.

## 4. Before every commit

Run `cargo xtask ci`. It runs the formatter check, clippy with warnings as errors, the tests, and
the layer, license, asset, product name and WebAssembly checks.

## 5. Repository text

The README, docs and commit messages describe what the app does, in plain American English.
