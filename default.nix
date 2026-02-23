{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "zeroclaw";
  version = "0.1.6";

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  # Tests require runtime configuration and network access
  doCheck = false;

  meta = {
    description = "Fast, small, and fully autonomous AI assistant infrastructure";
    homepage = "https://github.com/zeroclaw-labs/zeroclaw";
    license = lib.licenses.mit;
    sourceProvenance = with lib.sourceTypes; [ fromSource ];
    mainProgram = "zeroclaw";
    platforms = lib.platforms.unix;
  };
}
