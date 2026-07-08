<img src="assets/logo.svg" alt="frack logo" align="right" width="120"/>

# frack

A penguin in tails for your music stand: **frack** is a sheet music
viewer for Linux (GTK4/Rust). Half-page turns (the next page's top half
appears first), foot pedal support (Page Up/Down), freehand annotations
with a stylus – burned directly into the PDF file. No database: the
library is just a directory, setlists are folders of symlinks, and sync
and versioning are left to external tools (git-annex, Syncthing).

## Build & Run

With Nix:
```sh
nix build          # package
nix run            # build and start
```

For development, `direnv allow` activates the dev shell automatically
(or use `nix develop`), then `cargo build` / `cargo test` as usual.

```sh
cargo build --release
./target/release/frack
```
To use the package in another Nix configuration, add this repo as a
flake input and either take `packages.${system}.default` or apply
`overlays.default` (provides `pkgs.frack`).

`nix run .#sbom` writes `frack.sbom.cdx.json`, a CycloneDX SBOM
covering the full Nix runtime closure and all (transitive) Rust crates
including licenses.

## Configuration

`~/.config/frack/config.toml` (created on first start):

```toml
root_dir = "/home/ms/Noten"   # searched recursively for *.pdf
pen_width = 1.5
pen_color = "#cc0000"
a4 = 443.0                    # tuner reference pitch in Hz
note_names = "english"        # default is "german": H = english B, B = english Bb
accidentals = "sharp"         # D#/Dis instead of the default "flat" (Eb/Es)
start_fullscreen = false      # default true: open in fullscreen
```

## Keys & touch

Page Up/Down turns half pages, `a` = pen mode, Ctrl+Z = undo stroke,
`t` = tuner bar (pitch history over time),
F11 = fullscreen, Esc = back. Tapping the left/right screen edge turns
pages; tapping the middle opens an overlay with a page slider (fast
jumps in long PDFs) and touch buttons for back, pen, undo, tuner and
fullscreen – everything works without a keyboard. Pinch with two fingers to zoom in (e.g.
for precise annotations) and pan; pinch out to return to the fitted
view – page turns also reset the zoom. Annotations are saved on page
turn and on exit – after that, strokes are part of the PDF.
