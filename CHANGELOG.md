# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- An eraser: the stylus' eraser end erases, and a new eraser button (or
  `e`) turns on finger/mouse erasing (mutually exclusive with the pen). It
  rubs out the parts of a stroke it passes over — splitting the stroke and
  keeping the rest as ordinary ink annotations — shows a circle the size of
  the erase area, and is fully undoable/redoable. Which end of the stylus is
  touching is read as the tool comes into range, so flipping the pen over
  switches between drawing and erasing.
- The stylus now draws without switching on a mode first. The pen button
  changed meaning to "finger drawing on/off"; two fingers still zoom in
  either case. Resting-palm touches are ignored while the stylus draws,
  and starting to draw on the half-page view switches to the full page
  under the pen.
- Unlimited undo (Ctrl+Z) and redo (Ctrl+Shift+Z / Ctrl+Y), with a redo
  button next to undo. Because strokes live in the PDF, undo reaches
  strokes saved in an earlier session too; redo lasts for the current
  session. Their buttons enable/disable with the current page's history.
- Autosave: strokes are written back a few seconds after the pen is
  lifted (in addition to on page turns and on exit), so a single-page
  score or an unexpected shutdown no longer loses annotations.

### Changed

- Freehand strokes are now saved as standard PDF ink annotations
  (ISO 32000) instead of being drawn into the page content stream. The
  page content stays untouched, so individual strokes can be deleted or
  edited afterwards with any standard PDF tool (e.g. Okular), or all of
  them stripped with Ghostscript (see README). Each stroke keeps its own
  colour and width, so annotations made in other editors are shown
  faithfully.

## [0.2.0] - 2026-07-18

### Added

- GitHub Actions workflow mirroring the Forgejo CI: it installs Nix and
  grants access to /dev/kvm, then runs `nix flake check` (including the
  NixOS VM screenshot test) on every push and pull request.

### Fixed

- The library scan now skips hidden files and directories, so a `root_dir`
  that also holds a git-annex repository or Syncthing state lists the real
  scores (via their working-tree symlinks) instead of the `.git` or
  `.stversions` internals (e.g. content-addressed annex blobs).
- Half-page turns no longer shift the page: the split view now uses the
  same centered layout as the full page, so its divider falls on the
  page's vertical midline and completing a turn keeps the page in place.
- Portrait pages no longer show black letterbox bars; the area around a
  fitted page is filled with paper white to match the page.

## [0.1.0] - 2026-07-12

### Added

- PDF viewer with a library that recursively scans a configured root
  directory; setlists work as folders of symlinks. Substring search
  across the (relative) file paths.
- Half-page turns: the top half of the next page appears first while the
  bottom half of the current page stays visible.
- Page turning via foot pedal (Page Up/Down), arrow keys, space, or by
  tapping the left/right screen edge.
- Freehand annotations with a pressure-sensitive stylus (palm rejection)
  or the mouse, burned directly into the PDF as an added content stream;
  written atomically. Per-stroke undo before saving.
- Two-finger pinch zoom and pan in the full-page view.
- Page slider with page-number and thumbnail previews for fast
  navigation in long PDFs; thumbnails render in background worker threads.
- Tuner bar with the nearest note, cents deviation and frequency, plus a
  scrolling history graph of the pitch deviation over the last seconds.
- Fully touch-operable, including a middle-tap overlay with the page
  slider and action buttons; starts in fullscreen by default.
- Configuration via `~/.config/frack/config.toml`: root directory, pen
  width and color, tuner reference pitch (A4), note naming
  (german/english), accidental style (flat/sharp) and fullscreen start.
- Packaging as a Nix flake: package, overlay, development shell, and an
  SBOM generator (`nix run .#sbom`).
- Bundled public domain sample scores (Brahms, Symphony No. 1, trombone
  parts) in `sample-scores/`, and an optional command line argument
  naming a library directory that overrides `root_dir` for the run —
  together they allow trying frack without any setup:
  `nix run . -- ./sample-scores`.
- NixOS VM integration test (`checks.<system>.screenshots`) that boots
  frack in a kiosk and captures the README screenshots by driving the
  real UI: searching, opening a part, half-page turns, the touch
  overlay, freehand annotations drawn with pointer strokes, and the
  tuner fed by a generated sine wave through an ALSA loopback
  microphone. `nix run .#update-screenshots` refreshes
  `assets/screenshots/`; a companion check (`screenshots-up-to-date`)
  makes `nix flake check` fail when the committed images differ from
  what the UI renders.
- Forgejo Actions workflow running `nix flake check` on every push and
  pull request, so the package build, the VM screenshot test and the
  screenshot freshness check gate all changes.

[Unreleased]: https://github.com/matthiasdotsh/frack/commits/main
