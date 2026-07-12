# Nixpkgs-style package definition, kept upstreamable: for nixpkgs,
# replace `src` with a fetcher (fetchFromGitea with `domain` for the
# self-hosted Forgejo, or fetchFromGitHub) and `cargoLock` with
# `cargoHash`, and fill in meta.homepage/license.
{
  lib,
  rustPlatform,
  pkg-config,
  wrapGAppsHook4,
  copyDesktopItems,
  makeDesktopItem,
  gtk4,
  poppler,
  alsa-lib,
  adwaita-icon-theme,
  librsvg,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "frack";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../src
      ../examples
      ../assets/logo.svg
      ../Cargo.toml
      ../Cargo.lock
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  strictDeps = true;

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    copyDesktopItems
  ];

  buildInputs = [
    gtk4
    poppler # poppler-glib
    alsa-lib # audio input for the tuner (cpal)
    adwaita-icon-theme # symbolic icons for the header/overlay buttons
    librsvg # gdk-pixbuf loader for the symbolic SVG icons
  ];

  # wrapGAppsHook4 wires up schemas and pixbuf loaders but not the icon
  # theme; without this, minimal environments (kiosks, the VM test)
  # render every button as the image-missing fallback.
  preFixup = ''
    gappsWrapperArgs+=(
      --prefix XDG_DATA_DIRS : "${adwaita-icon-theme}/share"
    )
  '';

  # Desktop file and icon are named after the GTK application id so the
  # desktop environment associates the running window with them.
  desktopItems = [
    (makeDesktopItem {
      name = "app.frack.Frack";
      exec = finalAttrs.pname;
      icon = "app.frack.Frack";
      desktopName = "Frack";
      genericName = "Sheet music viewer";
      comment = "Sheet music viewer with half-page turns, stylus annotations and a tuner";
      categories = [
        "AudioVideo"
        "Music"
        "Viewer"
      ];
    })
  ];

  postInstall = ''
    install -Dm644 assets/logo.svg \
      "$out/share/icons/hicolor/scalable/apps/app.frack.Frack.svg"
  '';

  meta = {
    description = "Sheet music viewer with half-page turns, stylus annotations burned into the PDF, and a tuner with pitch history";
    homepage = "https://frack.app";
    license = lib.licenses.gpl3Plus;
    mainProgram = "frack";
    platforms = lib.platforms.linux;
  };
})
