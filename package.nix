{ lib
, rustPlatform
, version ? "0.1.12" # x-release-please-version
}:

rustPlatform.buildRustPackage {
  pname = "runner-up";
  inherit version;

  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  env.RUNNER_UP_VERSION = "v${version}";

  # The tests run runner-up as a program beside stand-in ssh, docker, tart
  # and runner commands, which the Nix sandbox does not provide. The
  # project's own CI runs `cargo test` on ubuntu, macOS and Windows.
  doCheck = false;

  meta = {
    description = "Run GitHub Actions jobs on your own machines, one fresh container or VM per job";
    homepage = "https://github.com/rokartur/runner-up";
    license = lib.licenses.mit;
    mainProgram = "runner-up";
    platforms = [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-linux"
    ];
  };
}
