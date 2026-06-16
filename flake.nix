{
  description = "moq-vaapi - VA-API H.264 encoder (vendored libva)";

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
              # bindgen needs libclang to parse the vendored libva headers.
              clang
            ]
            ++ pkgs.lib.optionals (!pkgs.stdenv.isDarwin) [
              # libva is dlopen'd at runtime (not linked at build), so this is only
              # needed to actually run vaapi encode in the devShell. macOS has no VAAPI.
              libva
            ];

          # bindgen locates libclang via this.
          shellHook = ''
            export LIBCLANG_PATH="${pkgs.libclang.lib}/lib"
          '';
        };

        formatter = pkgs.nixfmt-tree;
      }
    );
}
