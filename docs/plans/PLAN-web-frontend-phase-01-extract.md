# Phase 1: Renderer extraction (`shakenfist-spice-renderer`)

## Prompt

Before responding to questions or making changes, explore the
ryll codebase thoroughly. Read the master plan at
`docs/plans/PLAN-web-frontend.md` and the prior crate-extraction
phases (`docs/plans/PLAN-crate-extraction-phase-04-protocol.md`
and `-phase-05-usbredir.md`) for precedent. Key files for this
phase:

- `Cargo.toml` (workspace) and `ryll/Cargo.toml`
- `ryll/src/display/surface.rs` (especially the egui-coupled
  `texture()` method around line 465)
- `ryll/src/channels/inputs.rs` (especially `key_to_scancode`
  around line 886, which takes `egui::Key`)
- `ryll/src/channels/mod.rs` (the public enums: `ChannelEvent`,
  `InputEvent`, `CursorImage`, `UsbCommand`, `WebdavCommand`)
- `ryll/src/app.rs` (especially `run_connection` ~line 3084 and
  `run_headless` ~line 3370 — both must move to the renderer
  crate; the rest of `RyllApp` stays in `ryll`)

Pattern-match against `shakenfist-spice-protocol/`,
`shakenfist-spice-compression/`, and `shakenfist-spice-usbredir/`
for crate skeleton and dependency style. The dual-spec
`{ path = "..", version = ".." }` Cargo pattern is the convention.

Flag any uncertainty rather than guessing.

## Goal

Extract a new `shakenfist-spice-renderer` library crate from the
`ryll` binary crate. After this phase:

- `shakenfist-spice-renderer` contains `DisplaySurface` (the pixel
  buffer + draw-op API), all SPICE channel handlers, the input /
  cursor / channel-event types, and the `run_connection` / 
  `run_headless` session orchestrator.
- `ryll` becomes a thin GUI / CLI binary that depends on the new
  renderer crate the same way it already depends on
  `shakenfist-spice-protocol` etc.
- The `--web` mode added in later phases will join as a third
  consumer of the renderer crate, alongside GUI and headless.
- All existing tests pass; GUI and headless modes continue to
  work unchanged on Linux.

No new functionality is added in this phase. No web-facing code
yet. This is pure refactor.

## Scope

In:

- Two in-place API refactors that decouple substrate from `egui`
  (steps 1a, 1b). These must land before file movement, because
  the renderer crate cannot depend on `egui`.
- New crate skeleton `shakenfist-spice-renderer/` with its own
  `Cargo.toml` and an empty `lib.rs` (step 1c).
- File moves: `display/`, `channels/`, and the
  `run_connection`/`run_headless` orchestrator from `app.rs`
  (steps 1d, 1e).
- Updating `ryll/Cargo.toml` and the workspace `Cargo.toml`.
- Updating `use` statements throughout `ryll/src/` to reference
  the new crate.

Out:

- Reserving the crate name on crates.io. That is a separate
  pre-step the operator handles before publishing (analogous to
  `PLAN-crate-extraction-phase-02-reserve-names.md`). For
  development the dual-spec dep with a `path` works without
  publication.
- Any feature gating (`--gui` / `--web` cargo features). Master
  plan defers feature gating to future work.
- Writing the parity audit (Phase 0). Phases 0 and 1 are
  independent; either may run first.
- Web-specific code. That starts in Phase 2.

## Approach

### Refactor 1: `DisplaySurface` pixel/texture split (step 1b)

`display/surface.rs:465` exposes
`pub fn texture(&mut self, ctx: &Context) -> &TextureHandle`,
which calls `ctx.load_texture()` and `tex.set()`. This couples
the pixel substrate to `egui::Context`. Two viable shapes:

- **(a) Trait abstraction.** Define a `TextureSink` trait in the
  renderer crate; the GUI provides an egui-backed impl. Renderer
  doesn't know about textures, just hands the dirty rectangle to
  whatever sink the consumer registers.
- **(b) Move the egui binding to `ryll`.** Renderer exposes only
  raw pixels + dirty flag (`pixels(&self) -> &[u8]`,
  `is_dirty(&self) -> bool`, `consume_dirty(&mut self)`). The
  GUI in `ryll` wraps a `DisplaySurface` and maintains its own
  egui `TextureHandle` cache.

**Choose (b).** It is mechanically simpler, removes the trait
indirection, and matches what every consumer actually does (each
frontend is going to build its own representation — egui texture
for GUI, openh264 input for the encoder, datachannel cursor for
web). Trait abstraction is over-engineering for three concrete
consumers.

The change in step 1b:

1. Delete `texture()` from `DisplaySurface`.
2. Add a `consume_dirty(&mut self) -> bool` that returns the
   dirty bit and clears it (the existing `is_dirty()` stays read-only).
3. In `app.rs`, introduce a small `GuiSurface` wrapper (probably
   in a new `ryll/src/display_gui.rs`) that owns an
   `Option<TextureHandle>` and a reference (or owned wrapper)
   around `DisplaySurface`, and exposes
   `texture(&mut self, ctx: &Context)` that builds/refreshes the
   texture from `pixels()` when `consume_dirty()` returns true.
4. Update all GUI call sites (the egui paint loop) to use
   `GuiSurface::texture()` instead of
   `DisplaySurface::texture()`.

After this commit, `DisplaySurface` has no `egui` references and
`display/surface.rs` has no `eframe::egui` import.

### Refactor 2: scancode mapping (step 1a)

`channels/inputs.rs:886` exposes
`pub fn key_to_scancode(key: egui::Key) -> Option<(u32, bool)>`.
The function body is a giant `match` on `egui::Key` variants
mapping each to an AT scancode.

Replace with:

1. A neutral lookup function in the renderer-bound code:
   `pub fn scancode_for_logical_key(key: LogicalKey) -> Option<(u32, bool)>`,
   where `LogicalKey` is a small enum the renderer owns
   (`Letter('A'..'Z')`, `Digit(0..9)`, `Function(F1..F12)`,
   `Arrow(Up/Down/Left/Right)`, `Modifier(...)`, `Special(...)`).
2. An adapter function in `ryll` (probably in a new
   `ryll/src/input_egui.rs`) that takes `egui::Key` and returns
   `Option<LogicalKey>`. The adapter is then composed with
   `scancode_for_logical_key()` at the GUI call sites.
3. The web frontend (Phase 5) will provide a *different* adapter
   that takes a JS `KeyboardEvent.code` string (over the
   datachannel) and returns `Option<LogicalKey>` — same neutral
   pivot, two adapters.

This is more value than just stripping the `egui::Key` parameter
because it gives both GUI and web a typed neutral midpoint
instead of stringly-typed scancode plumbing.

After this commit, `channels/inputs.rs` has no `eframe::egui`
import.

### Crate skeleton (step 1c)

Create `shakenfist-spice-renderer/` next to the other extracted
crates. Files:

- `Cargo.toml` — version `version.workspace = true`, edition
  matching the workspace, dependencies copied from the
  "definitely move" set in the codebase research:
  `tokio` (full), `tokio-rustls`, `rustls-pemfile`,
  `webpki-roots`, `bytes`, `byteorder`, `rsa`, `sha1`, `rand`,
  `cpal`, `opus-decoder`, `rtrb`, `socket2`, `nusb`,
  `lz4_flex`, `tracing`, `anyhow`, `thiserror`, `async-channel`,
  `serde`, `serde_json`, plus path-deps on the existing
  `shakenfist-spice-{protocol,compression,usbredir}` crates.
- `src/lib.rs` — initially empty (just `// shakenfist-spice-renderer`).

Add the new member to the workspace `Cargo.toml` member list.
Add a `path + version` dep entry in `ryll/Cargo.toml`. Confirm
`cargo build --workspace` succeeds. No code moves yet.

### File moves (steps 1d, 1e)

**Step 1d** — move `ryll/src/display/` and `ryll/src/channels/`
to `shakenfist-spice-renderer/src/display/` and `.../channels/`.
Re-export the public API from `lib.rs`:

```rust
pub mod channels;
pub mod display;

pub use channels::{
    ChannelEvent, CursorImage, InputEvent, LogicalKey,
    UsbCommand, WebdavCommand,
};
pub use display::DisplaySurface;
```

Update `use` statements throughout `ryll/src/`:

- `use crate::channels::*` → `use shakenfist_spice_renderer::*`
- `use crate::display::*` → `use shakenfist_spice_renderer::*`
- Internal references inside the moved modules
  (`use crate::*`) need to be rewritten to either
  `use crate::*` (referring to the new crate root) or absolute
  paths into other extracted crates.

**Step 1e** — extract `run_connection()` and `run_headless()`
from `app.rs` into `shakenfist-spice-renderer/src/session.rs`.
The signatures are already clean (they consume channel
senders/receivers and `Arc<AtomicBool>` cancel flags, none of
which are GUI types). Re-export them:

```rust
pub mod session;
pub use session::{run_connection, run_headless};
```

`ryll/src/main.rs` updates: `app::run_headless` →
`shakenfist_spice_renderer::run_headless`. `ryll/src/app.rs`
updates: `RyllApp::reconnect()` calls
`shakenfist_spice_renderer::run_connection` instead of the
local function. The `RyllApp` struct itself, the `eframe::App`
impl, and the GUI event loop stay in `ryll`.

## Prerequisites

- Confirm the master plan (`PLAN-web-frontend.md`) is committed
  on `thought-bubble`. (It is, as of commit `4d8f9443`.)
- No crates.io publication required for development. If/when
  the renderer crate is published, follow the precedent in
  `PLAN-crate-extraction-phase-02-reserve-names.md` to reserve
  the name; treat that as a separate one-line task outside this
  phase.

## Steps

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 1a   | medium | sonnet | none | Refactor `key_to_scancode(egui::Key)` in `ryll/src/channels/inputs.rs:886`. Introduce a `LogicalKey` enum in `inputs.rs` (or a new sibling `keys.rs`) covering Letters, Digits, Function keys F1–F12, Arrows, modifiers, and the navigation/control keys currently in the match. Add `scancode_for_logical_key(LogicalKey) -> Option<(u32, bool)>` containing the existing scancode table. Add an adapter `egui_key_to_logical(egui::Key) -> Option<LogicalKey>` in a new `ryll/src/input_egui.rs`. Update all callers in `app.rs` to compose the two. Delete the old `key_to_scancode` from `inputs.rs`. Verify `inputs.rs` no longer imports `eframe::egui`. Run `cargo test --workspace` and `pre-commit run --all-files`. Single commit. |
| 1b   | high   | opus  | worktree | Refactor `DisplaySurface::texture(ctx: &egui::Context)` in `ryll/src/display/surface.rs:465`. Delete the method. Add `consume_dirty(&mut self) -> bool` (returns the dirty bit and clears it; existing `is_dirty()` stays). Create `ryll/src/display_gui.rs` with a `GuiSurface` wrapper that owns the `DisplaySurface` plus an `Option<TextureHandle>`, and exposes `texture(&mut self, ctx: &egui::Context) -> &TextureHandle` reading `pixels()` and `consume_dirty()` from the inner surface. Update `app.rs` paint loop call sites — search for `.texture(ctx` to find them — to use `GuiSurface` instead. The `RyllApp` `surfaces` HashMap likely needs to change from `HashMap<(u8,u32), DisplaySurface>` to `HashMap<(u8,u32), GuiSurface>`. Verify `surface.rs` no longer imports `eframe::egui`. Run cargo tests + pre-commit. Worktree because this restructures the GUI's main paint hot path; if the wrapper turns out wrong we want to throw it away cleanly. Single commit. |
| 1c   | low    | sonnet | none | Create the `shakenfist-spice-renderer` crate skeleton. Add `shakenfist-spice-renderer/Cargo.toml` (mirror the structure of `shakenfist-spice-protocol/Cargo.toml`, deps listed in the master plan's Phase 1 Approach: `tokio`, `tokio-rustls`, `rustls-pemfile`, `webpki-roots`, `bytes`, `byteorder`, `rsa`, `sha1`, `rand`, `cpal`, `opus-decoder`, `rtrb`, `socket2`, `nusb`, `lz4_flex`, `tracing`, `anyhow`, `thiserror`, `async-channel`, `serde`, `serde_json`, plus `path + version` deps on the three existing extracted crates). Add `shakenfist-spice-renderer/src/lib.rs` with just a doc comment. Add the crate to the workspace `Cargo.toml` `members` list. Add a `path + version` dep entry in `ryll/Cargo.toml`. Run `cargo build --workspace` and `pre-commit run --all-files`. No code moves yet. Single commit. |
| 1d   | high   | opus  | worktree | Move `ryll/src/display/` and `ryll/src/channels/` directory trees into `shakenfist-spice-renderer/src/`. Update `shakenfist-spice-renderer/src/lib.rs` with `pub mod channels; pub mod display;` and re-exports for `ChannelEvent`, `CursorImage`, `InputEvent`, `LogicalKey`, `UsbCommand`, `WebdavCommand`, `DisplaySurface`. Inside the moved modules, rewrite internal `use crate::channels::...` / `use crate::display::...` to `use crate::...` (now relative to the renderer crate root) and rewrite cross-crate refs (`use crate::settings::...` etc.) to either absolute paths or move the dependency along too if it's substrate. The `notifications` module and `metrics` module stay in `ryll` — channel code that emits notifications should communicate via `ChannelEvent` enum variants, *not* via direct calls into a notification store. If you find direct calls into ryll-side modules from channel code, flag them and either add `ChannelEvent` variants to indirect them, or pull the called module into the renderer too. In `ryll/src/`, replace every `use crate::channels::...` with `use shakenfist_spice_renderer::...`, same for display. Update `ryll/src/lib.rs` (or `main.rs` mod tree) to delete the `mod channels;` and `mod display;` lines. Run cargo tests + pre-commit. Worktree because this is the largest mechanical move (~15 files, hundreds of `use` line edits); if anything goes sideways we want to throw the worktree away. Single commit. |
| 1e   | high   | opus  | worktree | Extract `run_connection()` (around `app.rs:3084`) and `run_headless()` (around `app.rs:3370`) and their helper functions from `ryll/src/app.rs` into a new `shakenfist-spice-renderer/src/session.rs`. Re-export from the renderer's `lib.rs`. Update `ryll/src/main.rs` (around line 164) to call `shakenfist_spice_renderer::run_headless` instead of `app::run_headless`. Update `ryll/src/app.rs::reconnect()` (around line 711) to call `shakenfist_spice_renderer::run_connection` instead of the local function. The `RyllApp` struct, `eframe::App` impl, GUI event loop, and side-panel handling stay in `app.rs`. **Be careful**: any helper functions called by `run_connection`/`run_headless` need to move with them; any helpers called by both `run_connection` and the GUI event loop need to stay in `app.rs` and be referenced from the renderer (likely via callback or by being moved entirely to the renderer if they're substrate). The signatures of `run_connection`/`run_headless` should stay identical so call sites only change the path. Run cargo tests + pre-commit. Both `cargo run -p ryll -- --headless ...` and `cargo run -p ryll` (GUI) should still work end-to-end. Worktree. Single commit. |

After 1e, the `ryll` crate's `src/` should contain roughly:
`main.rs`, `app.rs` (GUI only), `config.rs`, `settings.rs`,
`notifications.rs`, `bugreport.rs`, `capture.rs`, `metrics.rs`,
`display_gui.rs`, `input_egui.rs`, `usb/`, `webdav/` — i.e. the
GUI binary, with all SPICE substrate extracted.

## Step details

### Step 1a expanded brief

The current `key_to_scancode` is a flat match on `egui::Key`
variants returning `Option<(u32, bool)>` where the bool is the
"shift required" flag. The new structure keeps the same return
type but pivots through `LogicalKey`.

Suggested `LogicalKey` variants (verify against the existing
match arms — only emit variants for keys that *appear* in the
existing match; do not invent new ones):

```rust
pub enum LogicalKey {
    Letter(char),         // 'A'..='Z'
    Digit(u8),            // 0..=9
    Function(u8),         // 1..=12 (F1..F12)
    Arrow(Direction),     // Up/Down/Left/Right
    Modifier(Modifier),   // Shift/Ctrl/Alt/Meta + L/R variants if present
    Navigation(NavKey),   // Home/End/PageUp/PageDown/Insert/Delete
    Whitespace(WSKey),    // Space/Tab/Enter/Backspace
    Escape,
}
```

Keep the existing scancode values verbatim; this is a pivot, not
a rewrite of the table.

### Step 1b expanded brief

The texture-binding refactor is the most disruptive of the three
in-place changes because it touches the GUI paint hot path. Read
the existing `texture()` body (`surface.rs:465`–~489) carefully
before writing the wrapper — it caches a `TextureHandle` and only
calls `tex.set()` when dirty. The wrapper must preserve this
caching behaviour or the GUI will allocate a new texture every
frame.

Suggested `GuiSurface` shape:

```rust
pub struct GuiSurface {
    inner: DisplaySurface,
    texture: Option<TextureHandle>,
}

impl GuiSurface {
    pub fn new(id: u32, width: u32, height: u32) -> Self { ... }

    pub fn surface(&self) -> &DisplaySurface { &self.inner }
    pub fn surface_mut(&mut self) -> &mut DisplaySurface { &mut self.inner }

    pub fn texture(&mut self, ctx: &egui::Context) -> &TextureHandle {
        let dirty = self.inner.consume_dirty();
        match (self.texture.as_mut(), dirty) {
            (None, _) => {
                // initial allocation
                let img = ColorImage::from_rgba_unmultiplied(
                    [self.inner.size().0 as usize, self.inner.size().1 as usize],
                    self.inner.pixels(),
                );
                self.texture = Some(ctx.load_texture(
                    format!("surface-{}", self.inner.id()),
                    img,
                    TextureOptions { magnification: TextureFilter::Nearest, ..Default::default() },
                ));
            }
            (Some(_tex), true) => {
                let img = ColorImage::from_rgba_unmultiplied(...);
                self.texture.as_mut().unwrap().set(img, TextureOptions { ... });
            }
            (Some(_), false) => { /* reuse existing texture */ }
        }
        self.texture.as_ref().unwrap()
    }
}
```

The `RyllApp::surfaces` HashMap value type needs to change from
`DisplaySurface` to `GuiSurface`. Search for every place that
calls a method on a surface from `surfaces.get_mut(...)` and
verify it routes through `surface_mut()` or moves to a
`GuiSurface` method.

### Step 1d expanded brief

This is the largest single commit by line count — possibly
1500–2000 lines of `use` rewrites and file relocations. Do it
in a worktree so a botched move can be discarded.

Order of operations:

1. `git mv ryll/src/display shakenfist-spice-renderer/src/display`
2. `git mv ryll/src/channels shakenfist-spice-renderer/src/channels`
3. Edit `shakenfist-spice-renderer/src/lib.rs` to declare the
   new modules and re-export the public types.
4. Inside the moved files, `use crate::display::...` /
   `use crate::channels::...` paths still resolve (they're now
   inside the renderer crate). The dangerous edits are
   `use crate::settings::...`, `use crate::notifications::...`,
   `use crate::bugreport::...` etc. — those refer to ryll-side
   modules that *don't* move. Each one needs a decision:
   - If the import is for logging configuration that's read-only:
     pass the value in via `run_connection` parameters or via a
     trait/callback rather than direct module access.
   - If the import is for emitting notifications: replace with a
     `ChannelEvent` variant the renderer emits and the GUI
     observes, then route into the notification store on the
     ryll side.
   - If the import is for bug-report ring-buffer recording:
     same — the renderer should emit raw protocol bytes via an
     observer/callback, with ryll registering its bug-report
     ring buffer as the observer.
5. In every remaining `ryll/src/*.rs` file, update
   `use crate::channels::...` → `use shakenfist_spice_renderer::...`,
   same for display.
6. Delete `mod channels;` and `mod display;` from
   `ryll/src/main.rs` (or wherever they're declared).
7. `cargo build --workspace`. Iterate on errors until clean.
8. `cargo test --workspace`. All existing tests should pass
   (they moved with their modules).

If step 4 surfaces tight coupling between channel code and
ryll-side modules (`notifications`, `bugreport`, `metrics`,
etc.), pause and report — that may turn one commit into two
(introduce the indirection in commit A, do the move in commit B).

### Step 1e expanded brief

`run_connection` is the function `RyllApp::reconnect()` spawns
on a fresh tokio runtime in a new thread (around `app.rs:734`).
Its signature roughly takes:

- A `Config` (or its loaded equivalent)
- An `Arc<AtomicBool>` cancel flag
- `mpsc` senders for `ChannelEvent`s (one per channel type)
- `mpsc` receivers for `InputEvent`, `UsbCommand`,
  `WebdavCommand`, monitor-resize events
- An `Arc<Notify>` for repaint
- Audio output handles (cpal stream)

All of these are either renderer-owned types or generic
sync primitives. None are `eframe`/`egui` types. Verify before
moving — if a parameter type is GUI-side, that's a refactor to
do first.

`run_headless` is similar but constructs null receivers/senders
internally. Both should be `pub async fn` in
`shakenfist-spice-renderer/src/session.rs` and re-exported from
`lib.rs`.

The audio side is worth attention: `cpal::Stream` ownership in
the GUI today might live on the `RyllApp`. If so, decide whether
the renderer constructs the stream and hands a handle back, or
whether the consumer constructs the stream and hands it in. The
latter is cleaner (renderer doesn't depend on cpal output device
management) — pass in an `rtrb::Producer<i16>` and let the
consumer wire that up to its own audio sink. If today's code
already does it that way, great; if not, this is a small
additional refactor inside step 1e.

## Acceptance criteria

- `pre-commit run --all-files` passes after each of 1a, 1b, 1c,
  1d, 1e.
- `cargo test --workspace` passes after each step.
- `cargo build --workspace` produces both the `ryll` binary and
  a `libshakenfist_spice_renderer.rlib`.
- `ryll --headless <vv-file>` connects, runs cadence, and exits
  cleanly — equivalent to the current behaviour.
- `ryll <vv-file>` (GUI) connects, displays the desktop, accepts
  keyboard/mouse input, and reconnects on disconnect —
  equivalent to current behaviour.
- `shakenfist-spice-renderer/src/` contains zero `egui` /
  `eframe` references (`grep -r "egui\|eframe" shakenfist-spice-renderer/src/`
  returns nothing).
- Each of 1a–1e is a single commit on `thought-bubble` with a
  message that follows project conventions (operator's
  `~/.claude/CLAUDE.md`: 50-char first line ending in a period,
  75-char wrap, Prompt paragraph, Signed-off-by, Co-Authored-By
  with model+context+effort).

## Risks

- **Hidden coupling between channel code and ryll-side
  modules** (notifications, bug-report ring buffer, metrics).
  Step 1d may discover that channels emit notifications by
  direct module call rather than via `ChannelEvent`. If so,
  introduce a `ChannelEvent` variant first (commit), then do
  the move (commit). Worktree isolation makes the abort cheap.
- **`cpal::Stream` ownership location.** If the audio output
  stream is owned on the GUI side and the renderer expects to
  hand pre-decoded PCM upward, this is fine. If it's owned
  inside the channel code today, step 1e will need to extract
  the stream construction into the consumer. Read
  `playback.rs` early to know which world we're in.
- **`thought-bubble` branch divergence from `main`.** The
  master plan was committed on `thought-bubble`; if
  `main` advances during this phase, rebase before each step
  rather than at the end. Five commits will rebase cleanly;
  one giant commit will not.
- **`include_bytes!` of GUI assets in moved files.** Codebase
  research found none, but a fresh grep before step 1d is
  cheap insurance.
- **eframe shutdown semantics.** The GUI uses
  `SHUTDOWN_REQUESTED` `AtomicBool` to coordinate eframe exit
  and channel cancellation. After 1e the renderer-side
  `run_connection` keeps the cancel flag; the GUI-side eframe
  shutdown writes to that flag. The contract is unchanged but
  worth verifying after the move.

## Documentation updates

After step 1e, update:

- `ARCHITECTURE.md` — note the new crate, its responsibilities,
  and the relationship between the renderer and the three
  frontends (GUI, headless, planned web). Likely a new section
  or a refactored "Crate organisation" subsection.
- `AGENTS.md` — add the renderer to the crate list / build
  notes if a list exists.
- `README.md` — only if it mentions the crate structure. The
  user-facing description is unchanged.
- `docs/plans/PLAN-web-frontend.md` — flip the Phase 1 row in
  the Execution table from *Not written* to *In progress* when
  starting, *Complete* when done.
- `docs/plans/index.md` — flip the Web frontend status from
  *Proposed* to *In progress* on first phase commit.

These doc updates can be batched into the step 1e commit, or
done as a follow-up commit on top of 1e — operator's
preference.

## Back brief

Before executing 1a, the implementing agent should back-brief:
which files will change in 1a, what `LogicalKey` variants will
exist, and what the call-site update pattern will look like. Do
not start editing without the back-brief.

Subsequent steps (1b–1e) follow the same pattern: back-brief
first, edit second.

## Estimated total scope

Roughly 2,500–3,500 lines of churn across five commits, the
bulk of which is mechanical (`use` rewrites in 1d, function
relocation in 1e). The genuinely new code is small (~150 lines
across 1a + 1b: `LogicalKey` enum + `egui_key_to_logical` +
`GuiSurface`).
