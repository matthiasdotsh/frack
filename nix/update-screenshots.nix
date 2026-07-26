# `nix run .#update-screenshots`: builds the screenshot VM test (see
# screenshots-test.nix) and copies the captured PNGs into
# assets/screenshots/, where the README embeds them. Run from the
# repository root and commit the result. Uses the nix on $PATH so it
# talks to the same daemon/config as the caller.
#
# Applies the same comparison rules as the screenshots-up-to-date
# check: a fuzzy image (see screenshot-fuzz.nix) is only replaced when
# it drifts past the check's pixel tolerance, so running this on an
# up-to-date tree leaves git clean instead of churning the jittery
# tuner image on every run.
{
  writeShellApplication,
  imagemagick,
}:

let
  fuzz = import ./screenshot-fuzz.nix;
in
writeShellApplication {
  name = "frack-update-screenshots";
  runtimeInputs = [ imagemagick ];
  text = ''
    system=$(nix eval --raw --impure --expr builtins.currentSystem)
    echo "Building screenshot VM test for $system (needs KVM to be quick)..."
    out=$(nix build --print-out-paths --no-link ".#checks.$system.screenshots")
    mkdir -p assets/screenshots

    for existing in assets/screenshots/*.png; do
      [ -e "$existing" ] || continue # unmatched glob
      if [ ! -e "$out/$(basename "$existing")" ]; then
        echo "Removing $existing (no longer produced by the test)"
        rm "$existing"
      fi
    done

    for generated in "$out"/*.png; do
      name=$(basename "$generated")
      dest="assets/screenshots/$name"
      if [ -e "$dest" ]; then
        if cmp -s "$generated" "$dest"; then
          continue
        fi
        if [[ " ${toString fuzz.fuzzyImages} " == *" $name "* ]]; then
          # `compare -metric AE`'s pixel count isn't stable across
          # ImageMagick versions; count via a difference composite instead.
          pixels=$(magick "$generated" "$dest" -compose difference -composite -threshold 5% -format '%[fx:int(mean*w*h)]' info: 2>/dev/null || true)
          if [[ "$pixels" =~ ^[0-9]+$ ]] && [ "$pixels" -le ${toString fuzz.fuzzyMaxPixels} ]; then
            echo "Keeping $dest ($pixels differing pixels, within tolerance)"
            continue
          fi
        fi
      fi
      echo "Updating $dest"
      install -m644 "$generated" "$dest"
    done

    echo "assets/screenshots/ now matches the test output:"
    ls -1 assets/screenshots/*.png
  '';
}
