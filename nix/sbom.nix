# CycloneDX SBOM of all Rust dependencies (including transitive ones)
# with their licenses, taken from the vendored crates' manifests. The
# flake merges this with the sbomnix output for the Nix closure.
{
  lib,
  stdenv,
  rustPlatform,
  cargo,
  cargo-cyclonedx,
}:

stdenv.mkDerivation {
  pname = "frack-sbom-rust";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../src
      ../examples
      ../Cargo.toml
      ../Cargo.lock
    ];
  };

  cargoDeps = rustPlatform.importCargoLock { lockFile = ../Cargo.lock; };

  strictDeps = true;

  nativeBuildInputs = [
    cargo
    cargo-cyclonedx
    rustPlatform.cargoSetupHook
  ];

  buildPhase = ''
    runHook preBuild
    cargo cyclonedx --format json --override-filename sbom
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    cp sbom.json $out
    runHook postInstall
  '';
}
