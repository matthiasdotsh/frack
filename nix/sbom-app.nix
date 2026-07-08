# `nix run .#sbom`: writes one merged CycloneDX SBOM covering the Nix
# runtime closure (licenses from nixpkgs meta, via sbomnix) and all
# Rust crates (licenses from the crate manifests, via sbom.nix).
#
# Non-pkgs arguments, passed explicitly by the flake:
#   flake     - the flake itself (self); sbomnix needs a flakeref, not a
#               store path, or license enrichment silently yields nothing
#   sbomnix   - the sbomnix package (from its upstream flake; the
#               nixpkgs version cannot parse current Nix derivation JSON)
#   sbom-rust - the Rust dependency SBOM derivation (./sbom.nix)
{
  writeShellApplication,
  jq,
  flake,
  sbomnix,
  sbom-rust,
}:

writeShellApplication {
  name = "frack-sbom";
  runtimeInputs = [
    sbomnix
    jq
  ];
  text = ''
    out="''${1:-frack.sbom.cdx.json}"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    sbomnix "path:${flake}#frack" \
      --cdx "$tmp/nix.cdx.json" \
      --spdx "$tmp/nix.spdx.json" \
      --csv "$tmp/sbom.csv"
    jq --slurpfile rust ${sbom-rust} \
      '.components = ((.components + $rust[0].components)
        | unique_by(.purl // (.name + "@" + (.version // ""))))' \
      "$tmp/nix.cdx.json" > "$out"
    echo "SBOM written to: $out" >&2
  '';
}
