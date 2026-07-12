# Fails when the committed README screenshots in assets/screenshots/
# differ from what the VM test currently produces. Run by
# `nix flake check`; fixed by `nix run .#update-screenshots` (plus
# committing the result). This keeps the README images enforceably in
# sync with the UI.
#
# Most images must match byte-for-byte; the images listed in
# screenshot-fuzz.nix are compared perceptually with a small tolerance
# instead (parameters shared with `nix run .#update-screenshots`).
#
#   screenshots - the VM test derivation whose $out holds the fresh PNGs
{
  runCommand,
  imagemagick,
  screenshots,
}:

let
  fuzz = import ./screenshot-fuzz.nix;
in
runCommand "frack-screenshots-up-to-date"
  {
    nativeBuildInputs = [ imagemagick ];
    committed = ../assets/screenshots;
    fuzzyMaxPixels = toString fuzz.fuzzyMaxPixels;
    fuzzyImages = fuzz.fuzzyImages;
  }
  ''
    status=0
    for generated in ${screenshots}/*.png; do
      name=$(basename "$generated")
      if [ ! -e "$committed/$name" ]; then
        echo "MISSING: assets/screenshots/$name is not committed"
        status=1
      elif [[ " $fuzzyImages " == *" $name "* ]]; then
        # `compare` exits 1 on any difference; the pixel count decides.
        # Its output looks like "1234 (0.0188)" or "1.2e+06 (18.3)", so
        # let awk normalize the first field to a plain integer.
        raw=$(compare -metric AE "$generated" "$committed/$name" null: 2>&1 || true)
        pixels=$(printf '%s\n' "$raw" | awk 'NR == 1 { printf "%d", $1 }')
        if ! [[ "$pixels" =~ ^[0-9]+$ ]]; then
          echo "STALE: assets/screenshots/$name is not comparable ($raw)"
          status=1
        elif [ "$pixels" -gt "$fuzzyMaxPixels" ]; then
          echo "STALE: assets/screenshots/$name differs in $pixels pixels (allowed: $fuzzyMaxPixels)"
          status=1
        fi
      elif ! cmp -s "$generated" "$committed/$name"; then
        echo "STALE: assets/screenshots/$name differs from the test output"
        status=1
      fi
    done
    for existing in "$committed"/*.png; do
      name=$(basename "$existing")
      if [ ! -e "${screenshots}/$name" ]; then
        echo "ORPHAN: assets/screenshots/$name is no longer produced by the test"
        status=1
      fi
    done
    if [ "$status" -ne 0 ]; then
      echo
      echo "The README screenshots are out of sync with the UI."
      echo "Refresh them with: nix run .#update-screenshots"
      exit 1
    fi
    touch $out
  ''
