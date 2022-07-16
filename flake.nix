{
  description = "stream-deck-controller flake";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
    let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
        devShell = pkgs.mkShell {
          nativeBuildInputs = [
            pkgs.cargo
            pkgs.rustc
            # pkgs.rustup
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.cargo-edit
            pkgs.go-task
            # needed for linker to work with tokio
            pkgs.darwin.apple_sdk.frameworks.Security
            # needed for linker to work with streamdeck
            pkgs.darwin.apple_sdk.frameworks.AppKit
            # enable atvremote integration
            pkgs.python310Packages.pyatv
          ];
          buildInputs = [ ];
          # Certain Rust tools won't work without this
          # This can also be fixed by using oxalica/rust-overlay and specifying the rust-src extension
          # See https://discourse.nixos.org/t/rust-src-not-found-and-other-misadventures-of-developing-rust-on-nixos/11570/3?u=samuela. for more details.
          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
          RUST_BACKTRACE = "1"; 
            # error from rust build: ld: library not found for -liconv
              # https://stackoverflow.com/questions/70313347/note-ld-library-not-found-for-lpq-when-build-rust-in-macos
          RUSTFLAGS = if pkgs.stdenv.isDarwin then "-L /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/lib" else "";
        };
    });
}