# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/matthiasdotsh/frack/commits/main
