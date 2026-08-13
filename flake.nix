{
  description = "hxy - a hex editor";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
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
    flake-utils.lib.eachSystem [ "aarch64-darwin" "x86_64-linux" ] (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {inherit system overlays;};

      rustToolchainToml = fromTOML (builtins.readFile ./rust-toolchain.toml);
      inherit (rustToolchainToml.toolchain) channel components targets;

      rustToolchain = pkgs.rust-bin.stable.${channel}.default.override {
        extensions = components;
        inherit targets;
      };

      craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
      sourcePkgs = pkgs.buildPackages;

      srcFilter = path: type:
        (craneLib.filterCargoSources path type)
        || (builtins.match ".*/BUCK" path != null)
        || (builtins.match ".*/reindeer\.toml" path != null)
        || (builtins.match ".*assets.*" path != null)
        || (builtins.match ".*translations.*" path != null)
        || (builtins.match ".*img.*" path != null)
        || (builtins.match ".*\\.wit" path != null)
        || (builtins.match ".*/wit" path != null)
        || (builtins.match ".*/vendor/.*" path != null);

      filteredSource = pkgs.lib.cleanSourceWith {
        src = ./.;
        filter = srcFilter;
      };

      eguiPhosphorSource = sourcePkgs.fetchFromGitHub {
        owner = "amPerl";
        repo = "egui-phosphor";
        rev = "2e7ec7ad6155cb9a5a713c9ba15f402d34c83ace";
        hash = "sha256-JJBCKaaVlEbxKiwy5y/Bx//QruJhktVPyGdjbTtajig=";
      };

      eguiPhosphor = sourcePkgs.applyPatches {
        name = "egui-phosphor-0.14.0";
        src = eguiPhosphorSource;
        patches = [./nix/patches/egui-phosphor-egui-0.36.patch];
      };

      buck2FixupsSource = sourcePkgs.fetchFromGitHub {
        owner = "facebook";
        repo = "buck2";
        rev = "dd59be39291fea745565e6d93fa33e8d11025b56";
        hash = "sha256-l1X+2JhISLST4N8PMJlGJVfr9tdkqKiZJn5N/hvXTj0=";
      };

      buck2Fixups = sourcePkgs.runCommand "hxy-buck2-fixups" {} ''
        cp -R ${buck2FixupsSource}/shim/third-party/rust/fixups "$out"
        chmod -R u+w "$out"
        rm -rf "$out/aws-lc-rs" "$out/aws-lc-sys" "$out/ring" "$out/syn" "$out/winapi"
        cp -R ${./nix/reindeer-fixups}/. "$out"
      '';

      hxySource = sourcePkgs.runCommand "hxy-source" {} ''
        mkdir -p "$out"
        cp -R ${filteredSource}/. "$out"
        mkdir -p "$out/third-party/overrides"
        ln -s ${eguiPhosphor} "$out/third-party/overrides/egui-phosphor"
        ln -s "$out/vendor/egui_ltreeview" "$out/third-party/overrides/egui_ltreeview"
        mkdir -p "$out/fixups"
        cp -R ${buck2Fixups}/. "$out/fixups"
      '';

      commonArgs = {
        pname = "hxy";
        version = "0.5.0";
        src = hxySource;
        strictDeps = true;

        nativeBuildInputs = with pkgs; [pkg-config];

        buildInputs = with pkgs;
          [openssl]
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            vulkan-loader
          ];
      };

      cargoVendorDir = craneLib.vendorCargoDeps (commonArgs // {src = filteredSource;});
      cargoVendorCacheKey = builtins.baseNameOf (toString cargoVendorDir);

      hxyDummySource = sourcePkgs.runCommand "hxy-dummy-source" {} ''
        mkdir -p "$out"
        cp -R ${craneLib.mkDummySrc (commonArgs // {src = filteredSource;})}/. "$out"
        mkdir -p "$out/third-party/overrides"
        ln -s ${eguiPhosphor} "$out/third-party/overrides/egui-phosphor"
        ln -s "$out/vendor/egui_ltreeview" "$out/third-party/overrides/egui_ltreeview"
      '';

      cargoArtifacts = craneLib.buildDepsOnly ((builtins.removeAttrs commonArgs ["src"]) // {
        inherit cargoVendorDir;
        dummySrc = hxyDummySource;
      });

      reindeer = pkgs.reindeer.overrideAttrs (_: {
        # macOS clamps the file-descriptor limit below Reindeer's test value.
        doCheck = false;
      });
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
              inherit cargoArtifacts cargoVendorDir;
              cargoExtraArgs = "-p hxy";
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
          egui-phosphor-source = eguiPhosphor;
        };

        devShells.default = mkShell rec {
          buildInputs =
            [
              rustToolchain
              buck2
              reindeer

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
          RUSTC = "${rustToolchain}/bin/rustc";
          RUSTDOC = "${rustToolchain}/bin/rustdoc";
          CARGO_NET_OFFLINE = "true";

          LD_LIBRARY_PATH =
            lib.optionalString stdenv.hostPlatform.isLinux
            "${lib.makeLibraryPath buildInputs}";

          shellHook = lib.optionalString stdenv.hostPlatform.isDarwin ''
            export DEVELOPER_DIR="$(env -u PROMPT_MULTILINE_INDICATOR -u SPROMPT nu ${./nix/check-xcode.nu})"
          '' + ''
            hxy_workspace_root="$PWD"
            while ! grep -q '^\[workspace\]' "$hxy_workspace_root/Cargo.toml" 2>/dev/null && [ "$hxy_workspace_root" != / ]; do
              hxy_workspace_root="$(dirname "$hxy_workspace_root")"
            done
            if ! grep -q '^\[workspace\]' "$hxy_workspace_root/Cargo.toml" 2>/dev/null; then
              echo "could not find the hxy workspace root" >&2
              exit 1
            fi
            mkdir -p "$hxy_workspace_root/third-party/overrides"
            ln -sfn ${eguiPhosphor} "$hxy_workspace_root/third-party/overrides/egui-phosphor"
            ln -sfn "$hxy_workspace_root/vendor/egui_ltreeview" "$hxy_workspace_root/third-party/overrides/egui_ltreeview"
            if [ -e "$hxy_workspace_root/fixups" ] && [ ! -L "$hxy_workspace_root/fixups" ]; then
              rm -rf "$hxy_workspace_root/fixups"
            fi
            ln -sfn ${buck2Fixups} "$hxy_workspace_root/fixups"
            hxy_cargo_home="''${XDG_CACHE_HOME:-$HOME/.cache}/hxy/cargo/${cargoVendorCacheKey}"
            mkdir -p "$hxy_cargo_home"
            install -m 644 ${cargoVendorDir}/config.toml "$hxy_cargo_home/config.toml"
            export CARGO_HOME="$hxy_cargo_home"
          '';
        };

        checks.environment = runCommand "hxy-environment-check" {
          nativeBuildInputs = [nushell buck2 reindeer rustToolchain pkg-config];
          CARGO_NET_OFFLINE = "true";
        } ''
          cargo_home="$PWD/cargo-home"
          mkdir -p "$cargo_home"
          install -m 644 ${cargoVendorDir}/config.toml "$cargo_home/config.toml"
          cd ${commonArgs.src}
          export CARGO_HOME="$cargo_home"
          CARGO_HOME="$cargo_home" nu ${./nix/check-flake.nu}
          CARGO_HOME="$cargo_home" nu ${./scripts/check-cargo-offline.nu}
          CARGO_HOME="$cargo_home" nu ${./nix/check-reindeer.nu}
          touch $out
        '';

        apps.check-flake = {
          type = "app";
          program = "${writeShellApplication {
            name = "hxy-check-flake";
            runtimeInputs = [nushell buck2 reindeer rustToolchain pkg-config];
            text = ''
              hxy_workspace_root="$PWD"
              while ! grep -q '^\[workspace\]' "$hxy_workspace_root/Cargo.toml" 2>/dev/null && [ "$hxy_workspace_root" != / ]; do
                hxy_workspace_root="$(dirname "$hxy_workspace_root")"
              done
              if ! grep -q '^\[workspace\]' "$hxy_workspace_root/Cargo.toml" 2>/dev/null; then
                echo "could not find the hxy workspace root" >&2
                exit 1
              fi
              mkdir -p "$hxy_workspace_root/third-party/overrides"
              ln -sfn ${eguiPhosphor} "$hxy_workspace_root/third-party/overrides/egui-phosphor"
              ln -sfn "$hxy_workspace_root/vendor/egui_ltreeview" "$hxy_workspace_root/third-party/overrides/egui_ltreeview"
              if [ -e "$hxy_workspace_root/fixups" ] && [ ! -L "$hxy_workspace_root/fixups" ]; then
                rm -rf "$hxy_workspace_root/fixups"
              fi
              ln -sfn ${buck2Fixups} "$hxy_workspace_root/fixups"
              hxy_cargo_home="''${XDG_CACHE_HOME:-$HOME/.cache}/hxy/cargo/${cargoVendorCacheKey}"
              mkdir -p "$hxy_cargo_home"
              install -m 644 ${cargoVendorDir}/config.toml "$hxy_cargo_home/config.toml"
              export CARGO_HOME="$hxy_cargo_home"
              export CARGO_NET_OFFLINE=true
              hxy_nu() {
                env -u PROMPT_MULTILINE_INDICATOR -u SPROMPT nu "$@"
              }
              hxy_nu ${./nix/check-flake.nu}
              hxy_nu ${./nix/check-reindeer.nu}
              exec env -u PROMPT_MULTILINE_INDICATOR -u SPROMPT nu ${./scripts/check-cargo-offline.nu}
            '';
          }}/bin/hxy-check-flake";
        };
      });
}
