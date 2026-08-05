# `libghostty-vt`: terminal state, rendering, input, and Cyclops integration evidence

This is a research record, not an adoption recommendation. It describes what
the published Rust binding and its pinned Ghostty core expose, what Cyclops
would retain or discard through its present workspace model, and which
comparisons still require measurement.

## Scope and pinned revisions

The name refers to two related but separately versioned projects:

- Ghostty's `libghostty-vt` is the native Zig/C terminal-emulation library in
  `ghostty-org/ghostty`.
- The Rust crates `libghostty-vt` and `libghostty-vt-sys` are the safe and raw
  bindings in the separate `Uzaaft/libghostty-rs` repository.

This report uses the following fixed revisions so that statements about the
released API do not drift with either `master` branch:

| Subject | Revision used | Why |
|---|---|---|
| Cyclops | `3b5c768eb6f2d03337d50fb0bae305f8f19eab35` | Current workspace baseline during this research |
| Rust binding release | `libghostty-vt` 0.2.1, repository commit [`46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0`](https://github.com/Uzaaft/libghostty-rs/commit/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0) | Exact published-release source |
| Ghostty core used by 0.2.1 | [`a887df42c56f6de86c0fe6da9c4eeca37931e083`](https://github.com/ghostty-org/ghostty/commit/a887df42c56f6de86c0fe6da9c4eeca37931e083) | Hard-coded in the release build script |
| Rust binding development snapshot | [`72ac98f292879bf9f788fcbb11238c562a1eebe6`](https://github.com/Uzaaft/libghostty-rs/commit/72ac98f292879bf9f788fcbb11238c562a1eebe6) | Used only to identify post-release churn |
| Ghostty development snapshot | [`168c7b94672d91cded4b506143cb0ebebc5d1ceb`](https://github.com/ghostty-org/ghostty/commit/168c7b94672d91cded4b506143cb0ebebc5d1ceb) | Used only for the upstream project's current status statement |

Claims labeled **measured** came from local source inspection or a command
recorded below. Claims labeled **inference** describe an integration consequence
of those sources. An absence means no public API was found in the pinned Rust
release; it is not a claim that no internal Ghostty implementation exists.

## Status and maturity

Ghostty describes the native terminal functionality as proven and stable, but
the library interface as unfinished: `libghostty-vt` is usable from Zig and C
on macOS, Linux, Windows, and WebAssembly, while its API signatures remain in
flux and the project has not tagged a standalone libghostty version
([upstream status](https://github.com/ghostty-org/ghostty/blob/168c7b94672d91cded4b506143cb0ebebc5d1ceb/README.md#L145-L170)). That distinction matters: terminal behavior can be mature while an embedding API is still changing.

The Rust layer is even more explicit: version 0.2.1 says it is in development,
not stable, and expected to make breaking changes
([crate warning](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/lib.rs#L11-L17)). Its workspace requires Rust 1.90 and edition 2024
([release manifest](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/Cargo.toml#L5-L10)). Cyclops currently declares edition 2021 and no workspace MSRV
([workspace manifest](../../Cargo.toml)); adding 0.2.1 would therefore make
Rust 1.90 the effective minimum unless the dependency were isolated another
way.

The release was committed on 2026-07-15. In the next two weeks, the inspected
Rust `master` snapshot changed 11 files, including 50 lines in `terminal.rs`
and 52 in Kitty graphics. It moved the Ghostty pin, changed the required Zig
line from 0.15 to 0.16, and added terminal pixel-size, viewport-active,
selection, and sticky VT-processing-error getters
([unpublished getters](https://github.com/Uzaaft/libghostty-rs/blob/72ac98f292879bf9f788fcbb11238c562a1eebe6/crates/libghostty-vt/src/terminal.rs#L564-L647),
[new selection getter](https://github.com/Uzaaft/libghostty-rs/blob/72ac98f292879bf9f788fcbb11238c562a1eebe6/crates/libghostty-vt/src/selection.rs#L199-L214)). These APIs are not in 0.2.1. This is concrete evidence of active, useful development and concrete evidence that an adapter must expect churn.

The Rust manifest declares `MIT OR Apache-2.0`
([manifest](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/Cargo.toml#L5-L10)), but the inspected release repository contains one `LICENSE` file with MIT text
([license](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/LICENSE)). Ghostty itself has an MIT license
([Ghostty license](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/LICENSE)). The missing Apache license text should be resolved in a dependency-license audit rather than assuming the metadata or repository file is the whole distribution story.

## Build, packaging, and platform behavior

### What a normal Cargo build does

The release is not a pure-Rust dependency. Its default path is:

1. `libghostty-vt` enables `kitty-graphics` by default, and
   `libghostty-vt-sys` enables `vendored` by default
   ([safe crate features](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/Cargo.toml#L10-L26),
   [sys crate features](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/Cargo.toml#L15-L29)).
2. The sys build script pins Ghostty commit `a887df42...`
   ([pin](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/build.rs#L5-L8)).
3. Without `GHOSTTY_SOURCE_DIR`, it runs `git clone --filter=blob:none`, then
   checks out that commit inside Cargo's `OUT_DIR`
   ([fetch path](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/build.rs#L319-L359)).
4. It runs `zig build -Demit-lib-vt=true -Dapp-runtime=none`, deriving Zig's
   optimization mode from the Cargo profile
   ([build invocation](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/build.rs#L102-L171),
   [optimization mapping](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/build.rs#L287-L317)).
5. Cargo links the static archive by default. `link-dynamic` opts into the
   shared library; an installed pre-1.0 library can be used through the
   optional `pkg-config` feature, with no promise that arbitrary C API
   revisions match the checked-in bindings
   ([link and pkg-config contract](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/README.md#L5-L20)).

The pinned Ghostty source requires Zig 0.15.2
([`build.zig.zon`](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/build.zig.zon#L1-L7)); the Rust release README accordingly says Zig 0.15.x
([build instructions](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/README.md#L56-L69)). The inspected post-release `master` instead requires Zig 0.16.x
([current build instructions](https://github.com/Uzaaft/libghostty-rs/blob/72ac98f292879bf9f788fcbb11238c562a1eebe6/README.md#L56-L69)). Pinning the Rust crate therefore also pins a Zig toolchain expectation.

### Network-free and package-manager paths

`GHOSTTY_SOURCE_DIR` selects a pre-fetched Ghostty checkout.
`GHOSTTY_ZIG_SYSTEM_DIR` supplies a pre-fetched Zig package store and adds
`zig build --system`, avoiding dependency downloads during the build script
([implementation](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/build.rs#L109-L162)). Ghostty's package manifest pins download hashes, including the non-lazy `uucode` package used for Unicode behavior
([package hashes](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/build.zig.zon#L7-L44)). The binding's Nix build exercises the preinstalled-library path and locks Rust 1.90, Zig 0.15.2, and the exact Ghostty input
([Nix inputs and toolchains](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/flake.nix#L9-L62),
[pkg-config build](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/flake.nix#L79-L118)).

This makes hermetic packaging possible, but it is not the default Cargo
experience. A default build executes two external programs, fetches a full
upstream source tree, and may let Zig resolve packages over the network.

### Platform matrix

The native Ghostty library advertises macOS, Linux, Windows, and WebAssembly.
The Rust 0.2.1 vendored build script has a narrower, explicit cross-target map:

- x86-64 and AArch64 Linux, GNU and musl;
- x86-64 and AArch64 macOS;
- x86-64 and AArch64 Windows, with the listed GNU/MSVC mappings;
- x86-64 and AArch64 Android.

Other cross targets panic as unsupported
([target mapping](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/build.rs#L380-L396)). In particular, the release build script has no Rust-to-Zig WASM mapping even though the native library supports WebAssembly. A Rust/WASM build therefore needs a different integration path or upstream work.

At the release commit, CI ran full Nix checks on macOS AArch64, Linux AArch64,
and Linux x86-64
([Unix matrix](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/.github/workflows/ci.yml#L13-L65)). Windows CI built, but did not test, x86-64 and AArch64 MSVC with Zig 0.15.2
([Windows matrix](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/.github/workflows/windows-ci.yml#L13-L48)). The source maps more targets than that release CI proves.

## Reproduction of Cyclops finding F34

**Measured on 2026-08-05.** This probe intentionally supplied the already
cloned, exact Ghostty source so that a Git or network failure could not be
mistaken for the Zig requirement.

Environment:

```text
host: aarch64-apple-darwin
rustc: 1.97.1 (8bab26f4f 2026-07-14)
cargo: 1.97.1 (c980f4866 2026-06-30)
zig: not present on PATH
dependency: libghostty-vt = "=0.2.1"
Ghostty source: a887df42c56f6de86c0fe6da9c4eeca37931e083
```

Probe:

```sh
env GHOSTTY_SOURCE_DIR=/tmp/libghostty-research.XFECr7/ghostty-a887df cargo build
```

Result: exit 101 from `libghostty-vt-sys`'s custom build command:

```text
failed to execute zig build: No such file or directory (os error 2)
```

The panic is the build script's failed `Command::status` at line 365
([source](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt-sys/build.rs#L362-L366)). This independently reproduces [F34](../../findings.md) without modifying Cyclops or the user's Cargo tree. It does not measure runtime correctness or performance; no native library was produced.

## Architectural boundary: emulator, not renderer

`libghostty-vt` parses VT bytes, maintains terminal state and history, and
encodes input. It does not paint glyphs, own a PTY, create windows, or provide
Ratatui widgets. Ghostty itself describes the library's scope as parsing
terminal sequences and maintaining terminal state
([scope](https://github.com/ghostty-org/ghostty/blob/168c7b94672d91cded4b506143cb0ebebc5d1ceb/README.md#L145-L160)). The Rust API begins with three persistent objects:

- `Terminal` owns parser state, primary/alternate screens, scrollback, modes,
  colors, cursor, title, working directory, and protocol state.
- `RenderState` is a renderer-oriented, stateful snapshot optimized for
  repeated updates from one terminal.
- Row and cell iterators expose the current visible viewport of a render
  snapshot.

The minimal frame flow is:

```text
tmux %output bytes
        |
        v
Terminal::vt_write
        |
        v
Terminal state + dirty flags
        |
        v
RenderState::update / begin_update + end
        |
        v
row iterator -> cell iterator -> Cyclops/Ratatui adapter
```

This is compatible with Cyclops's current conceptual split, but it is not a
drop-in renderer. Cyclops would still decide how Ghostty graphemes, colors,
cursor, selection, links, and images become cells or extra layers in its own
full-screen TUI.

### Ownership and threads

The safe Rust handles are deliberately `!Send + !Sync`: the binding will not
assume that the C API has no thread-local state or that concurrent access is
safe. Its documented pattern is to create the terminal on the thread where it
will live and communicate with that thread by channels
([thread-safety contract](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/lib.rs#L50-L72)). It is not valid to create a `Terminal` on one Rust thread and move it to another.

`RenderState` supports a two-phase update: `begin_update` copies the
terminal-dependent data; `end` performs deferred work after terminal access is
released
([two-phase API](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/render.rs#L342-L390)). This reduces exclusive terminal access, but it does not override the Rust wrapper's `!Send` constraint. A multi-threaded design must construct each non-send handle on the thread where it is used, or keep the whole terminal/render pipeline on one owner thread.

## Terminal processing and observable state

`Terminal::new` accepts columns, rows, and `max_scrollback`. The Rust/C surface
documents that value as a number of lines
([options](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L237-L255)), while the pinned native `Screen.Options` describes the value it receives as bytes
([native screen option](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/Screen.zig#L247-L268)). That documentation conflict needs a memory/row-count probe. `vt_write` keeps parser state across chunks and deliberately returns no `Result`: malformed or untrusted input is logged while the library tries to preserve consistent state
([write contract](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L302-L317)). Release 0.2.1 has no getter telling an embedder that a non-gracefully handled processing error occurred. The inspected unpublished `master` adds a sticky `vt_processing_error()` getter; a reset does not clear it.

Other fallible calls collapse native failures into `OutOfMemory`,
`InvalidValue`, or `OutOfSpace { required }`
([error surface](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/error.rs#L6-L35)). Internal logs are discarded unless a process-global logger is installed. The crate can send them to a custom thread-safe callback, stderr, `log`, or `tracing`; release builds compile out debug messages
([logging contract](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/log.rs#L9-L22),
[logger setup](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/log.rs#L122-L165)). Any Cyclops logger bridge would need a content audit before sending messages to product logs or the ledger.

Resize has more semantics than changing a matrix. The primary screen reflows
when wraparound is enabled, the alternate screen does not, pixel dimensions
are updated for image/size protocols, synchronized output is disabled, and an
in-band resize response may be generated
([resize contract](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L319-L345)).

The safe release exposes:

- cursor position, pending wrap, visibility, current pen style, and Kitty
  keyboard flags;
- active primary/alternate screen, total rows, scrollback rows, scrollbar,
  and mouse-tracking state;
- title, current working directory, default/effective foreground,
  background, cursor color, and all 256 palette entries;
- mode lookup and setting for ANSI/DEC modes including alternate screens,
  bracketed paste, synchronized output 2026, grapheme clustering 2027,
  color-scheme reporting 2031, and in-band resize 2048
  ([mode constants](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L882-L956));
- viewport scrolling by top, bottom, delta, or absolute row;
- optional incremental/full scrollback compression. Incremental compression
  is caller-scheduled idle work; no internal polling loop is required
  ([compression contract](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L464-L506)).

The terminal internally has primary and alternate screens and reports which is
active. The published renderer and formatter operate on the active screen.
No public 0.2.1 Rust API was found for independently rendering or serializing
the inactive screen.

## Rendering model

### Snapshot and dirty tracking

`RenderState` is explicitly stateful, attached in practice to one terminal,
and optimized for dirty-region updates
([design](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/render.rs#L16-L51)). After an update it reports:

- global `Clean`, `Partial`, or `Full` dirty state;
- an independent dirty bit for every row;
- viewport columns and rows;
- effective foreground/background, cursor color, and an RGB-resolved
  256-color palette;
- cursor position, wide-tail status, visible/blinking/password-input state,
  color, and bar/block/underline/hollow-block visual style;
- a row-local selection span.

The two dirty layers do not clear each other and `update` does not clear them.
The renderer must clear the global state and rendered row flags separately.
This is a correctness edge, not just an optimization: mishandling it can cause
permanent redraw work or skipped changes. The enum and cursor values are visible
in the release source
([dirty and cursor types](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/render.rs#L928-L977)).

A Cyclops adapter could keep a retained pane grid, update only dirty Ghostty
rows, then let Ratatui diff the composed frame. **Inference:** converting every
row into a fresh `CellGrid` on every render would preserve current behavior but
discard Ghostty's principal rendering optimization.

### Cell fidelity

For each visible cell, the renderer exposes:

- a base codepoint plus the full grapheme cluster as codepoints or UTF-8;
- width class `Narrow`, `Wide`, `SpacerTail`, or `SpacerHead`;
- raw and resolved foreground/background colors;
- foreground, background, and underline colors as default, palette index, or
  RGB;
- bold, italic, faint, blink, inverse, invisible, strikethrough, overline;
- underline style `None`, `Single`, `Double`, `Curly`, `Dotted`, or `Dashed`;
- selected state and whether explicit styling exists.

The style shape is defined in one value
([style](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/style.rs#L20-L44),
[underline variants](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/style.rs#L117-L129)). Graphemes have allocation-free buffer APIs as well as a convenient allocating `Vec<char>` call
([cell iteration](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/render.rs#L747-L840)). Raw row metadata includes wrapping and continuation, whether graphemes/styles/hyperlinks are present, OSC 133 prompt semantics, Kitty virtual placeholders, and dirty state
([rows and cells](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/screen.rs#L277-L398)).

Hyperlinks are only partly convenient for a render loop. A raw cell reports
that a hyperlink exists, but the URI comes from `GridRef::hyperlink_uri`
([URI API](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/screen.rs#L83-L127)). The same file warns that grid references are invalid after any terminal update and are not intended as the core of a high-frame-rate render loop
([lifetime/performance warning](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/screen.rs#L28-L43)). A hyperlink-aware adapter therefore needs a measured URI cache or a different upstream API, rather than blindly doing one grid lookup per linked cell per frame.

### What current Cyclops would preserve

Cyclops presently normalizes Alacritty into a deliberately small structure:
one Rust `char`, one wide-spacer boolean, default/indexed/RGB foreground and
background, and five boolean attributes
([`GridCell` and `CellAttrs`](../../crates/cyclops-workspace/src/runtime/grid.rs)). The Alacritty adapter performs a full `display_iter()` conversion only when its grid is dirty, then caches and clones that grid
([adapter](../../crates/cyclops-workspace/src/runtime/alacritty.rs)).

With no model changes, a libghostty adapter could retain:

- the first codepoint of a grapheme;
- tail/head spacer as one undifferentiated spacer flag;
- default/indexed/RGB colors if it uses raw styles, or RGB colors if it uses
  resolved render colors;
- bold, italic, one boolean underline, inverse, and faint/dim;
- block, underline, or bar cursor, dropping hollow-block and blinking detail.

It would discard:

- remaining grapheme codepoints, including combining marks and emoji ZWJ
  sequences;
- the distinction between wide spacer head and tail;
- underline color and underline shape;
- blink, invisible, strikethrough, and overline;
- hyperlink URI and OSC 133 semantic content;
- Kitty graphics and virtual-placeholder metadata;
- selected state unless selection remains a separate overlay;
- dynamic inner palette values if indexed colors are forwarded directly to the
  outer terminal rather than resolved.

Therefore the Ghostty parser and Ghostty visual capability are separate
questions. **Inference:** replacing `AlacrittyVt` while retaining the exact
`CellGrid` contract tests parser behavior but does not test most of the richer
visual surface that motivates libghostty.

### Palette choice

The render API can return both raw palette identities and resolved RGB values.
That matters in an embedded terminal: `Color::Indexed(196)` asks the user's
outer terminal to resolve index 196, whereas the program in the tmux pane may
have changed its own palette with OSC 4. Resolving through Ghostty's render
palette can preserve the inner pane's displayed color, at the cost of losing
the original indexed identity. This is a testable adapter policy, not a fixed
property of the library.

### Per-cell FFI cost

Rows and cells are opaque iterators. `style()`, `fg_color()`, `bg_color()`,
grapheme length/data, raw cell, and selected state are separate C calls
([getter implementation](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/render.rs#L707-L840)). The current Alacritty adapter reads Rust structs directly. Ghostty's dirty rows can reduce how many cells are converted, while rich conversion can require several FFI crossings per converted cell. No comparative benchmark was run because the native library could not be built. Both effects must be present in a fair benchmark.

## Unicode, graphemes, and a reset-sensitive default

Ghostty's core has explicit UAX-style grapheme handling and DEC private mode
2027. The Rust release exposes `Mode::GRAPHEME_CLUSTER` and full grapheme data.
However, the C construction path used by the Rust crate does not expose
Ghostty's `default_modes` option. It passes only dimensions and scrollback
([C construction](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/c/terminal.zig#L300-L352)), while native `Terminal.Options` uses the core `ModePacked` defaults, which leave grapheme mode off
([native defaults](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/Terminal.zig#L238-L267)).

Consequences at the pinned release:

- DEC 2027 is off when a Rust `Terminal` is created.
- A caller can enable it using `set_mode`.
- A RIS/full reset restores modes to the construction-time defaults
  ([reset implementation](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/Terminal.zig#L3880-L3914),
  [mode reset](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/modes.zig#L13-L33)).
- Because the Rust/C construction path cannot set `default_modes`, a child
  program's RIS can turn grapheme mode back off even if the embedder enabled it
  once.

The core itself tests that a native default mode survives reset
([upstream test](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/Terminal.zig#L13808-L13818)); the missing piece is exposing that option through the C/Rust constructor. This needs a dedicated probe with combining marks, emoji ZWJ sequences, variation selectors, and RIS in the middle of a stream. The existing Cyclops corpus only covers two CJK wide characters.

## Modern protocols and visual metadata

### Kitty graphics

Kitty graphics is a default Rust crate feature. The API exposes active-screen
image storage, image IDs and pixel data, placement iteration, source rectangles,
viewport coordinates, grid sizes, virtual placeholders, and three z ranges:
below the cell background, between background and text, and above text
([layer model](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/kitty/graphics.rs#L740-L800)). The embedder must clip and composite those pixels; Ratatui text cells do not do this automatically.

Raw RGB/RGBA and zlib-compressed images are represented directly. PNG input
requires a decoder callback; the optional Rust `png` feature supplies one
([PNG callback](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/kitty/graphics.rs#L813-L943)). The decoder is thread-local and must be installed on the same thread as the terminal.

The pinned native library defaults embedded-library image storage to 10,000,000
bytes rather than Ghostty application's 320,000,000 bytes, and allows only
direct payloads by default
([storage and media defaults](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/Terminal.zig#L248-L266)). The Rust API can change the storage cap and independently enable file, temporary-file, and shared-memory loading
([policy setters](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/kitty/graphics.rs#L239-L310)).

Ghostty also caps a single Kitty APC payload at 65 MiB by default; the Rust API
can lower the Kitty-specific or global APC bound
([core APC defaults](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/apc.zig#L234-L249),
[Rust setters](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L726-L741)). The payload cap and decoded-image storage cap are distinct.

### Hyperlinks, prompts, and selection

OSC 8 presence is stored per cell and the URI is retrievable. OSC 133 semantic
prompt state is stored per row and cell. These can support link activation,
prompt/output-aware selection, or visual decoration, but current `CellGrid`
has no fields for them.

Selection is substantially more than extracting text between two coordinates:

- all, line, word, nearest word between two points, and OSC 133 command-output
  selection;
- linear and rectangular selections;
- tracked grid references that follow scroll, prune, and reflow;
- terminal-aware adjustment and containment;
- formatting as plain text, VT, or HTML.

The core selection APIs are visible in
[`selection.rs`](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/selection.rs#L199-L423). There is also a high-level gesture state machine for press, drag, release, deep press, and autoscroll ticks
([gesture model](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/selection/gesture.rs#L1-L38)). Its release documentation notes that dropping a gesture without `reset` temporarily retains small tracked-reference allocations until the terminal is dropped; a long-lived adapter should exercise this lifecycle.

### Synchronized output

Mode 2026 is queryable. `RenderState` still reflects terminal state as bytes are
processed; the embedding application decides whether to suppress intermediate
frames while synchronized output is active. Resize deliberately disables the
mode. This can fit Cyclops's event-armed render scheduling without adding an
interval timer, but the suppression policy remains adapter work.

### Sixel and glyph protocol

No Sixel parser, image-storage API, or render API was found in the pinned
Ghostty terminal source or Rust surface. The Rust constant
`DeviceAttributeFeature::SIXEL` only lets an embedder advertise that feature in
a device-attributes response
([DA constants](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L1074-L1096)). Sixel support should not be inferred from that constant.

The release can enable or disable Ghostty's APC glyph protocol and clear its
glossary, but no Rust renderer-facing glyph-outline API was found. Its benefit
to a Ratatui cell renderer is therefore unproven in this release.

## Input and interaction APIs

The library includes protocol encoders as well as output parsing.

### Keyboard

The key encoder supports legacy terminal encoding and the Kitty keyboard
protocol. It models press, repeat, and release; physical key identity; logical
layout text; left/right modifiers and consumed modifiers; composition state;
and an unshifted Unicode codepoint. It can synchronize cursor-key application,
keypad application, alt-prefix, modifyOtherKeys, and Kitty flags from a
`Terminal`
([encoder and terminal synchronization](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/key.rs#L1-L14),
[option copy](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/key.rs#L126-L217)).

### Mouse, focus, and paste

The mouse encoder supports X10, UTF-8, SGR, urxvt, and SGR-pixel formats. It
copies tracking/format modes from the terminal but requires the renderer's
pixel and cell geometry and current-button state from the embedder
([mouse API](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/mouse.rs#L1-L14),
[geometry](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/mouse.rs#L321-L358)). Focus gained/lost encodes CSI I/CSI O; the caller must check DEC mode 1004 before sending it
([focus encoder](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/focus.rs#L1-L54)).

Paste helpers flag newline and bracketed-paste terminator injection, sanitize
control bytes, wrap bracketed paste, and translate line feeds to carriage
returns outside bracketed mode
([paste behavior](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/paste.rs#L40-L81)).

### Effect callbacks and PTY replies

Sequences with side effects or required replies are ignored until callbacks
are installed. Available effects cover PTY write-back, bell, ENQ, XTVERSION,
title and working-directory changes, size and color-scheme queries, device
attributes, and normalized clipboard writes
([effect handlers](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L1533-L1691)). Callbacks run synchronously inside `vt_write` and must not block
([callback contract](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/terminal.rs#L50-L81)). OSC 52 reads are always ignored by this Rust/C API; writes are decoded and forwarded atomically to the host policy callback.

Cyclops observes a tmux-managed pane rather than owning the child PTY. tmux is
already the application's terminal protocol peer. The current Alacritty
adapter similarly uses a null event listener. **Inference:** Ghostty's input
encoders and PTY response callbacks are capabilities to evaluate for routing,
not APIs that should automatically be wired into the pane; doing so without a
protocol-owner design could duplicate tmux behavior. They are still useful as
reference implementations and for a future architecture that directly owns a
PTY.

## Formatter and hydration limits

The formatter serializes the active screen as plain text, VT, or HTML. VT
formatting can include palette, changed modes, scrolling region, tab stops,
working directory, keyboard state, cursor, current style, OSC 8 hyperlink,
protection, Kitty keyboard state, and character sets
([formatter options](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/crates/libghostty-vt/src/fmt.rs#L23-L134)). It is valuable for copying and for serializing a terminal that libghostty already owns.

It is not a general checkpoint/import facility:

- it reads an existing `Terminal`; it does not convert a tmux capture into
  hidden parser state;
- it formats only the active screen;
- no 0.2.1 deserializer or native-state import API was found;
- its options do not claim to include the inactive screen, scrollback storage,
  saved cursors, title, Kitty image data, a partial parser sequence, pending
  synchronized-output transaction, callbacks, or all effect state.

Cyclops hydration currently throws away the old Alacritty terminal, creates a
fresh one, optionally enters alternate screen 1049, replays captured visible
rows with CRLF, and restores the cursor with CUP
([hydration implementation](../../crates/cyclops-workspace/src/runtime/alacritty.rs)). This intentionally restores pixels and a small amount of buffer identity, not exact VT state.

The same replay technique can feed libghostty, but the same information limit
remains. A tmux capture cannot reconstruct private modes, saved cursor stacks,
tab stops, scrolling margins, parser fragments, full history, both complete
buffers, dynamic protocol state, or images it did not encode. Libghostty's
richer live state makes the gap larger, not smaller. A fair hydration study
must separately score immediate visual equality and correctness after
subsequent output such as mode reset, alternate-screen exit, redraw, resize,
and wide/grapheme edits.

The grapheme default described earlier adds a libghostty-specific hydration
edge: enabling mode 2027 after constructing a terminal is insufficient if the
replayed or later live byte stream contains RIS.

## Direct evidence matrix against the current Alacritty path

This table compares only what is implemented or exposed at the pinned
revisions. It is not a ranking.

| Area | Cyclops + `alacritty_terminal` 0.26 now | `libghostty-vt` Rust 0.2.1 | Unmeasured question |
|---|---|---|---|
| Build | Builds in current Cargo/CI path without Zig | Rust 1.90, external Zig 0.15.x, native build; default source/package fetch | Hermetic CI time, cache size, release artifact size |
| Output parsing | Direct Rust `Processor::advance` | C FFI `Terminal::vt_write`; malformed input is best-effort and logged | Same-stream differential behavior |
| Existing corpus | 12/12 fixtures pass ([F35](../../findings.md)) | Not run because F34 reproduced | Actual 12-fixture result after installing Zig |
| Grid update | One local dirty bit; full visible grid conversion on demand | Global full/partial/clean plus per-row dirty flags | End-to-end frame time at realistic pane counts |
| Per-cell access | Direct Rust cell fields | Opaque iterator plus one or more FFI getters | Whether dirty-row savings outweigh richer FFI conversion |
| Text model | Alacritty has extra zero-width data, but Cyclops keeps one `char` | Full grapheme API and four-state width model; DEC 2027 default/reset caveat | Unicode/RIS corpus |
| Styles | Cyclops keeps fg/bg and five flags | Underline color/type plus blink, invisible, strike, overline, palette and RGB | Which fields Ratatui/Crossterm can faithfully emit |
| Cursor | Position, visible, block/underline/bar in model | Position, wide-tail, visible, blinking, password, color, hollow block too | Actual cursor painting policy |
| Hyperlinks/semantics | Not present in `CellGrid` | OSC 8 presence/URI and OSC 133 row/cell semantics | Cache design and interaction UX |
| Graphics | No image layer in current pane model | Kitty image storage and placement geometry; caller renders/composites | Whether a terminal-within-terminal can display it portably |
| Selection | Current adapter delegates simple range extraction to Alacritty; Cyclops owns pointer UX | Rich selection and gesture APIs, tracked anchors, plain/VT/HTML formatting | Fit with current chrome-first mouse ownership |
| Keyboard/mouse encoding | Cyclops routes input through tmux; pane engine is observational | Legacy/Kitty key, five mouse formats, focus, paste helpers | Whether any encoder belongs in a tmux-observer architecture |
| Primary/alternate | Engine stores state; hydration explicitly restores 1049 fact | Engine stores both; public render/format API exposes active screen | Inactive-screen inspection and recovery requirements |
| Threading | Current engine lives in existing workspace runtime | All safe handles are `!Send + !Sync` | One owner thread versus one thread per pane/group |
| Errors | Current parser call is infallible to adapter | `vt_write` is infallible; release lacks sticky processing-error getter | Operational observability under OOM/limits |
| Checkpoint/hydration | Visual tmux replay, known state loss | Formatter emits rich VT from a live owned terminal; no state import found | Post-hydration behavioral corpus |
| Sixel | Not integrated | No support evidenced; DA advertisement constant only | Explicit sixel fixture if it becomes a requirement |

The current 12-case corpus covers basic text, color, a few attributes, cursor
motion, wrapping, CJK width, alternate screen, bracketed-paste bytes, and two
synthetic agent fragments
([fixtures](../../crates/cyclops-workspace/tests/corpus.rs)). It does not cover most differentiators in this matrix.

## Performance and memory evidence

No local performance result is available. Zig was absent and the native build
stopped before a benchmark binary existed. It would be misleading to transfer
Ghostty application's benchmark claims directly to Cyclops: upstream says the
full Ghostty app is generally within a few percent of Alacritty, but that app
also has dedicated read/write/render threads, SIMD parser paths, and native
Metal/OpenGL renderers
([upstream performance discussion](https://github.com/ghostty-org/ghostty/blob/168c7b94672d91cded4b506143cb0ebebc5d1ceb/README.md#L99-L117)). Cyclops would use the parser/state core through FFI and render through Ratatui/Crossterm, a different pipeline.

Relevant mechanisms worth measuring are:

- Ghostty can avoid converting clean rows, while the present Cyclops adapter
  rebuilds all visible cells after any output batch.
- Ghostty's rich getters can cross FFI several times per dirty cell.
- `graphemes()` allocates a `Vec`; `graphemes_buf` and `graphemes_utf8` allow
  reusable storage.
- `RenderState` and row/cell iterator objects are intended for reuse.
- scrollback can be compressed through explicit bounded idle work.
- Kitty graphics has independent payload and decoded-storage bounds.
- static linking adds a native archive; no binary-size measurement was made.

Performance must be measured at the adapter boundary, not only as parser
bytes/second, because Cyclops's frame deadline, row conversion, Ratatui buffer
composition, and terminal writes determine visible latency.

## Security and trust boundaries

### Build-time surface

- Default Cargo builds execute `git` and `zig` and fetch pinned source. Zig may
  fetch content-hashed packages unless a system package directory is supplied.
- The Ghostty commit is pinned, and Zig dependencies have hashes, which makes a
  reviewed prefetch path possible.
- Checked-in FFI bindings avoid running bindgen by default.
- Static linking includes native Zig/C ABI code and unsafe Rust FFI. It expands
  the languages and toolchains in Cyclops's supply-chain audit.
- The `pkg-config` escape hatch shifts trust to the installed library and has
  an explicit pre-1.0 compatibility warning.

### Runtime child-output surface

- `vt_write` treats input as untrusted and attempts to remain consistent rather
  than return parse failures.
- APC size setters and Kitty image-storage limits bound two large allocation
  paths.
- File, temporary-file, and shared-memory Kitty media are off by default in the
  embedded library. If enabled, output from a pane can ask the host to read
  filesystem or shared-memory content. Ghostty performs path/type checks, but
  even its source calls the file safety check “really rough”
  ([file checks](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/src/terminal/kitty/graphics_image.zig#L250-L325)). These media require an explicit host threat model.
- PNG decoding is host-provided and runs synchronously on the terminal thread.
- Clipboard writes are decoded before a synchronous policy callback. OSC 52
  reads are ignored. Cyclops's existing rule that selected text is not logged or
  persisted still applies to any callback integration.
- Query replies and other effects are opt-in. Ignoring them matches an
  observational renderer; installing them makes the host responsible for
  avoiding blocking and re-entrancy mistakes.

No adversarial runtime probe was run. Upstream source contains many regression
tests annotated as originating in AFL++ or differential fuzzing, but this
report did not reproduce a fuzz campaign.

## Test evidence and gaps

**Measured static inventory at Ghostty `a887df42...`:** `rg '^test "'` finds
2,376 named Zig test blocks under `src/terminal` and 3,212 under all of `src`.
The terminal tests include explicit fuzz regressions and a differential
`printSlice` test. Counts are source declarations, not a claim that every test
was run in this environment.

**Measured static inventory at Rust release `46a9d2a...`:** the two crates
contain 11 `#[test]` functions plus documented examples. Release CI's Nix check
runs workspace tests, Clippy, docs, and formatting on its Unix matrix
([flake checks](https://github.com/Uzaaft/libghostty-rs/blob/46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0/flake.nix#L122-L164)); Windows release CI only builds. Local Rust and Ghostty tests did not run because the same missing-Zig prerequisite blocked compilation.

The current Cyclops evidence remains asymmetric:

- Alacritty has a direct 12/12 result in production's normalized grid.
- `vt100` has a 5/12 comparison result.
- libghostty has no corpus score. `vt100` is not a behavioral proxy for
  libghostty; it was only the buildable alternate in F34/F35.

## Decision experiments for a later synthesis phase

These are bounded experiments that would turn the open cells in the matrix
into evidence. They intentionally do not set an adoption threshold here.

### 1. Reproducible build and distribution probe

Pin Rust crate 0.2.1, Ghostty `a887df42...`, and Zig 0.15.2. Build with:

- default online vendoring;
- pre-fetched `GHOSTTY_SOURCE_DIR` plus `GHOSTTY_ZIG_SYSTEM_DIR` and network
  denied;
- static and dynamic linking;
- Cyclops's supported macOS/Linux targets and any release cross targets.

Record cold/warm build time, cache size, final binary size, dynamic
dependencies, licenses, and reproducibility. Repeat against the chosen newer
binding revision rather than silently mixing 0.2.1 with `master`.

### 2. Direct Cyclops corpus adapter

Implement a test-only libghostty adapter that maps into the exact current
`CellGrid`. Run the existing 12 fixtures with the same assertions. This answers
basic parser parity only; keep it separate from a richer model experiment.

### 3. Expanded fidelity corpus

Add fixtures and semantic assertions for:

- combining marks, emoji ZWJ, variation selectors, regional indicators,
  ambiguous width, spacer head/tail, and DEC 2027 before/after RIS;
- all underline shapes/color, strike, overline, invisible, blink, reverse,
  OSC palette mutation, and default color queries;
- OSC 8 links and OSC 133 prompt/input/output regions;
- alternate-screen variants 47/1047/1049, saved cursors, scrolling regions,
  origin/wrap modes, charsets, resize reflow, and history;
- synchronized-output suppression;
- Kitty direct RGB/RGBA, PNG, scrolling placements, virtual placeholders, z
  layers, deletes, and storage-limit rejection;
- malformed/truncated CSI, OSC, DCS, APC, and UTF-8 across arbitrary chunk
  boundaries.

Compare both engines' native state and the final Ratatui buffer. Otherwise a
parser may retain a feature that Cyclops drops before the user sees it.

### 4. Hydration differential

For each fixture, maintain a continuous reference engine and a second engine
that is destroyed and hydrated from the actual tmux snapshot fields. Compare:

1. the immediate visible frame;
2. state visible through public getters;
3. the frame after each subsequent operation: text append, line edit, RIS,
   alternate-screen exit, resize, scroll, palette change, and synchronized
   output end.

Run the same method for Alacritty and libghostty. Include a libghostty
formatter round trip from a still-live source terminal as a separate case; do
not confuse it with recovery from tmux capture.

### 5. Frame-path benchmark

Use realistic pane sizes and counts with three workloads:

- many tiny `%output` chunks coalesced into one frame;
- a spinner changing one row;
- a full-screen TUI redraw and high-volume scroll.

Measure parse time, `RenderState::update`, row/cell conversion, allocation
count, Ratatui composition, terminal flush bytes, p50/p95/p99 frame latency,
CPU, and resident memory. Include:

- full-grid conversion for both engines;
- retained-grid dirty-row conversion for Ghostty;
- allocating and reusable-buffer grapheme extraction;
- one-thread ownership and a channel-owned terminal thread.

### 6. Interaction protocol probe

Generate a matrix of Crossterm key/mouse/focus/paste events and compare the
bytes sent through current tmux routing with Ghostty's encoders under legacy,
application cursor/keypad, modifyOtherKeys, Kitty keyboard, five mouse formats,
bracketed paste, and focus mode. Determine protocol ownership before routing
any Ghostty-generated PTY replies.

### 7. Resource and adversarial probe

Feed randomized chunking and large OSC/DCS/APC inputs under low APC and Kitty
storage limits. Verify bounded memory, no crash, predictable callbacks, and
usable state after rejection. Explicitly test that file/shared-memory image
media stay disabled unless enabled and that clipboard contents never enter
logs or the ledger.

### 8. API-churn adapter audit

Build the same thin adapter against 0.2.1 and the selected later revision.
Record changed source lines and behavior, including Ghostty pin, Zig version,
getters, image policy, and FFI types. This estimates ongoing maintenance more
accurately than the pre-1.0 warning alone.

## Findings that can be used without extrapolation

- The published Rust 0.2.1 path is a safe wrapper over a pinned native
  Ghostty C ABI, not a Rust-native parser.
- F34 is reproducible: the normal build needs Zig, and exact 0.2.1 needs Zig
  0.15.x.
- A hermetic prefetch path exists, but it must be configured.
- `RenderState` exposes global and row dirty tracking plus substantially richer
  cells, cursors, semantics, selection, and Kitty graphics than Cyclops's
  current normalized grid.
- The library supplies emulator state and placement information, not a
  Ratatui/Crossterm renderer.
- The current `CellGrid` would erase most of the new visual fidelity unless it
  or the render pipeline changed.
- The Rust handles are `!Send + !Sync`; thread ownership is an architectural
  constraint.
- The current Rust/C constructor leaves DEC 2027 out of default modes, so RIS
  can disable an embedder's one-time grapheme-mode setting.
- The formatter is not a tmux-snapshot state importer.
- Kitty graphics is exposed with useful bounds and safe direct-only defaults,
  but drawing/compositing it in a terminal TUI remains host work.
- No direct libghostty score, runtime benchmark, binary-size result, or
  hydration-parity result exists yet. The current `vt100` result says nothing
  about libghostty correctness.

Those are the limits of this research lane. Selection between engines belongs
to the later synthesis phase after the direct experiments above.
