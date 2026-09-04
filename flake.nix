{
  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.git-hooks.flakeModule
        inputs.treefmt-nix.flakeModule
      ];

      flake = { };

      perSystem =
        {
          config,
          pkgs,
          lib,
          system,
          ...
        }:
        let
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;
          src = pkgs.lib.cleanSourceWith { src = ./.; };

          webSrc = pkgs.lib.cleanSourceWith { src = ./echonet-radar/web; };
          webDeps = pkgs.fetchPnpmDeps {
            pname = "echonet-radar-web";
            version = "0.0.0";
            src = webSrc;
            inherit (pkgs) pnpm;
            fetcherVersion = 4;
            hash = "sha256-ZIyJkfwT2wRF6tz0MLjwvYQFSgu4ebCH2kNfWwJLRmM=";
          };
          webAssets = pkgs.stdenv.mkDerivation {
            pname = "echonet-radar-web";
            version = "0.0.0";
            src = webSrc;
            pnpmDeps = webDeps;
            nativeBuildInputs = [
              pkgs.nodejs_24
              pkgs.pnpm
              pkgs.pnpmConfigHook
            ];
            buildPhase = ''
              runHook preBuild
              pnpm build
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out
              cp -r dist/. $out/
              runHook postInstall
            '';
          };

          # GPUI and wry need these to link and run on Linux. macOS uses the
          # system frameworks (Metal, WebKit) and needs nothing extra.
          guiNativeBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.pkg-config
          ];
          guiBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux (
            with pkgs;
            [
              wayland
              vulkan-headers
              vulkan-loader
              libxcb
              libxkbcommon
              fontconfig
              glib
              gtk3
              webkitgtk_4_1
            ]
          );

          commonArgs = {
            inherit src;
            strictDeps = true;
            nativeBuildInputs = guiNativeBuildInputs;
            buildInputs = guiBuildInputs;
            # rust-embed requires `web/dist` at compile time; stage the built
            # UI before cargo touches the crate.
            postPatch = ''
              mkdir -p echonet-radar/web/dist
              cp -r ${webAssets}/. echonet-radar/web/dist/
            '';
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        {
          _module.args.pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          checks = {
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit
                  src
                  cargoArtifacts
                  ;
                cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
              }
            );
            hakari = craneLib.mkCargoDerivation {
              inherit src;
              pname = "labs-hakari";
              cargoArtifacts = null;
              doInstallCargoArtifacts = false;
              buildPhaseCargoCommand = ''
                cargo hakari generate --diff
                cargo hakari manage-deps --dry-run
                cargo hakari verify
              '';
              nativeBuildInputs = [
                pkgs.cargo-hakari
              ];
            };
            test = craneLib.cargoTest (
              commonArgs
              // {
                inherit
                  src
                  cargoArtifacts
                  ;
              }
            );
          };

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ config.pre-commit.devShell ];

            packages = with pkgs; [
              cargo-hakari
              nodejs_24
              pnpm
              rustToolchain
              sccache
            ];

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
            '';
          };

          packages = import ./echonet-radar/package.nix {
            inherit
              cargoArtifacts
              commonArgs
              craneLib
              lib
              pkgs
              webAssets
              ;
          };

          pre-commit.settings = {
            hooks = {
              actionlint.enable = true;
              deadnix.enable = true;
              statix.enable = true;
              statix.excludes = [
                ".direnv"
              ];
            };
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
              rustfmt.enable = true;
              rustfmt.package = rustToolchain;
              taplo.enable = true;
              yamlfmt.enable = true;
            };
          };
        };

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    };
}
