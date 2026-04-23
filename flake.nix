{
  description = "hxy - a hex editor";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    flake-utils,
    crane,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {inherit system overlays;};

      rustToolchainToml = fromTOML (builtins.readFile ./rust-toolchain.toml);
      inherit (rustToolchainToml.toolchain) channel components targets;

      rustToolchain = pkgs.rust-bin.stable.${channel}.default.override {
        extensions = components;
        inherit targets;
      };

      craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

      srcFilter = path: type:
        (craneLib.filterCargoSources path type)
        || (builtins.match ".*assets.*" path != null)
        || (builtins.match ".*translations.*" path != null);

      commonArgs = {
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = srcFilter;
        };
        strictDeps = true;

        nativeBuildInputs = with pkgs; [pkg-config];

        buildInputs = with pkgs;
          [openssl]
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            vulkan-loader
          ];
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
    in
      with pkgs; {
        packages = let
          guiRuntimeLibs = lib.optionals stdenv.hostPlatform.isLinux [
            libxkbcommon
            libGL
            fontconfig
            wayland
            vulkan-loader
            libxcursor
            libxrandr
            libxi
            libx11
          ];

          guiBuildInputs =
            commonArgs.buildInputs
            ++ lib.optionals stdenv.hostPlatform.isLinux [
              libxkbcommon
              wayland
              libxcursor
              libxrandr
              libxi
              libx11
              fontconfig
            ];

          unwrapped = craneLib.buildPackage (commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "-p hxy-app";
              buildInputs = guiBuildInputs;
              meta.mainProgram = "hxy";
            });
        in {
          hxy =
            if stdenv.hostPlatform.isLinux
            then
              (pkgs.symlinkJoin {
                name = "hxy-${unwrapped.version or "dev"}";
                paths = [unwrapped];
                nativeBuildInputs = [pkgs.makeWrapper];
                postBuild = ''
                  wrapProgram $out/bin/hxy \
                    --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath guiRuntimeLibs}
                '';
              }).overrideAttrs {meta.mainProgram = "hxy";}
            else unwrapped;

          default = self.packages.${system}.hxy;
        };

        devShells.default = mkShell rec {
          buildInputs =
            [
              rustToolchain

              openssl
              pkg-config

              trunk
              mise
              cargo-edit

              llvmPackages.clang-unwrapped
              llvmPackages.llvm
            ]
            ++ lib.optionals stdenv.hostPlatform.isLinux [
              libxkbcommon
              libGL
              fontconfig

              wayland

              libxcursor
              libxrandr
              libxi
              libx11
            ];

          CC_wasm32_unknown_unknown = "${llvmPackages.clang-unwrapped}/bin/clang";
          AR_wasm32_unknown_unknown = "${llvmPackages.llvm}/bin/llvm-ar";

          LD_LIBRARY_PATH =
            lib.optionalString stdenv.hostPlatform.isLinux
            "${lib.makeLibraryPath buildInputs}";
        };
      });
}
