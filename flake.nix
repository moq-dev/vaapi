{
  description = "moq-vaapi - VA-API hardware video codecs (vendored libva)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rust-toolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "rustfmt"
            "clippy"
          ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages =
            with pkgs;
            [
              rust-toolchain
              just
              git
              cargo-deny
              # `just bindgen` regenerates the checked-in bindings; builds need neither it nor libclang.
              rust-bindgen
            ]
            ++ pkgs.lib.optionals (!pkgs.stdenv.isDarwin) [
              # libva is dlopen'd at runtime (not linked at build), so this is only
              # needed to actually run vaapi encode in the devShell. macOS has no VAAPI.
              libva
            ];
        };

        formatter = pkgs.nixfmt-tree;
      }
    );
}
