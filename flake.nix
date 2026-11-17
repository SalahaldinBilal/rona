{
  description = "rona";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachSystem ["x86_64-linux" "aarch64-linux"] (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;
        craneLib = crane.mkLib pkgs;

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        buildInputs = with pkgs; [
          fontconfig
          libxkbcommon
          wayland
        ];

        runtimeLibraryPath = lib.makeLibraryPath (with pkgs; [
          mpv-unwrapped
          fontconfig
          libGL
          libxkbcommon
          wayland
          libx11
          libxcursor
          libxi
          libxrandr
        ]);

        commonArgs = {
          src = ./.;
          strictDeps = true;
          inherit nativeBuildInputs buildInputs;
          cargoExtraArgs = "--locked -p rona --no-default-features";
        };

        rona = craneLib.buildPackage (
          commonArgs
          // {
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;
            doCheck = false;
            postInstall = ''
              cp -r target/release/share $out/share
            '';
            postFixup = ''
              patchelf --add-rpath ${runtimeLibraryPath} $out/bin/rona
            '';
            meta = {
              description = "A video player with mpv as the playback backend";
              license = lib.licenses.gpl3Plus;
              mainProgram = "rona";
              platforms = lib.platforms.linux;
            };
          }
        );
      in {
        packages.default = rona;

        devShells.default = craneLib.devShell {
          inputsFrom = [rona];
          packages = with pkgs; [
            rust-analyzer
            rustfmt
            clippy
          ];
          LD_LIBRARY_PATH = runtimeLibraryPath;
        };
      }
    )
    // {
      nixosModules.default = {
        config,
        lib,
        pkgs,
        ...
      }: let
        cfg = config.programs.rona;
      in {
        options.programs.rona = {
          enable = lib.mkEnableOption "Rona";
          package = lib.mkOption {
            type = lib.types.package;
            default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
            description = "The Rona package to install.";
          };
        };
        config = lib.mkIf cfg.enable {environment.systemPackages = [cfg.package];};
      };
    };
}
