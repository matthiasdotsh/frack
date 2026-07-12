# Perceptual-comparison parameters shared by the screenshots-up-to-date
# check and `nix run .#update-screenshots`. Most screenshots must match
# byte-for-byte; the images listed here are compared with a pixel
# tolerance instead because their content jitters between runs — and
# the updater keeps the committed file while it would still pass the
# check, so a fresh run on an up-to-date tree leaves git clean.
{
  # The tuner's pitch-history graph shifts by a few pixels between
  # runs: audio capture and analysis ticks are not phase-locked.
  fuzzyImages = [ "tuner-half-page.png" ];

  # Maximum number of differing pixels for a fuzzy image (~3% of an
  # 800x1280 frame; layout changes differ by far more, jitter by far
  # less — the tuner graph was measured at ~1600 pixels between runs).
  fuzzyMaxPixels = 30000;
}
