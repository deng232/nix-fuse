{
  description = "Read-only FUSE view for Nix closure paths";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f {
            inherit system;
            pkgs = import nixpkgs { inherit system; };
          }
        );
    in
    {
      packages = forAllSystems (
        { pkgs, ... }:
        rec {
          nix-closure-fuser = pkgs.rustPlatform.buildRustPackage {
            pname = "nix-closure-fuser";
            version = "0.1.0";

            src = self;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [
              pkgs.pkg-config
            ];

            buildInputs = [
              pkgs.fuse3
            ];

            meta = {
              description = "Read-only FUSE filesystem exposing a filtered view of selected closure paths";
              mainProgram = "nix-closure-fuser";
              platforms = pkgs.lib.platforms.linux;
            };
          };

          default = nix-closure-fuser;
        }
      );

      apps = forAllSystems (
        { system, pkgs, ... }:
        let
          package = self.packages.${system}.nix-closure-fuser;
        in
        {
          nix-closure-fuser = {
            type = "app";
            program = "${package}/bin/nix-closure-fuser";
          };

          nix-closure-fuser-setcap = {
            type = "app";
            program = toString (
              pkgs.writeShellScript "nix-closure-fuser-setcap" ''
                set -euo pipefail
                sudo setcap cap_sys_admin+ep ${package}/bin/nix-closure-fuser
                exec ${package}/bin/nix-closure-fuser "$@"
              ''
            );
          };

          default = self.apps.${system}.nix-closure-fuser;
        }
      );

    };
}
