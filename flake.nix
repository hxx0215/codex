{
  description = "Development Nix flake for OpenAI Codex CLI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;

      # Read the version from the workspace Cargo.toml (the single source of
      # truth used by the release workflow).
      cargoToml = builtins.fromTOML (builtins.readFile ./codex-rs/Cargo.toml);
      cargoVersion = cargoToml.workspace.package.version;
      cargoLock = builtins.fromTOML (builtins.readFile ./codex-rs/Cargo.lock);
      cargoV8Version = (builtins.head (builtins.filter (package: package.name == "v8") cargoLock.package)).version;
      rustToolchain = builtins.fromTOML (builtins.readFile ./codex-rs/rust-toolchain.toml);
      rustVersion = rustToolchain.toolchain.channel;

      # The prebuilt rusty_v8 archives are fetched from URLs templated on the v8
      # crate version, but their hashes are pinned literals.  When Cargo.lock
      # bumps v8 the URLs follow automatically and the hashes do not, so keep the
      # two in lockstep here: otherwise the mismatch only surfaces as an opaque
      # fixed-output hash error tens of minutes into a release build.
      v8Pins = builtins.fromJSON (builtins.readFile ./nix/v8-pins.json);
      rustyV8Version =
        if v8Pins.version == cargoV8Version
        then cargoV8Version
        else throw ''
          rusty_v8 pin drift: codex-rs/Cargo.lock wants v8 ${cargoV8Version},
          but nix/v8-pins.json pins ${v8Pins.version}.
          Run ./nix/update-v8-pins.sh to refresh the pinned hashes.
        '';

      rustyV8ArchiveBySystem = nixpkgs.lib.mapAttrs (_: pin: {
        url = "https://github.com/denoland/rusty_v8/releases/download/v${rustyV8Version}/librusty_v8_release_${pin.target}.a.gz";
        inherit (pin) hash;
      }) v8Pins.gnu;

      rustyV8MuslBySystem = nixpkgs.lib.mapAttrs (_: pin: {
        inherit (pin) target;
        archive = {
          url = "https://github.com/openai/codex/releases/download/rusty-v8-v${rustyV8Version}/librusty_v8_release_${pin.target}.a.gz";
          hash = pin.archiveHash;
        };
        bindings = {
          url = "https://github.com/openai/codex/releases/download/rusty-v8-v${rustyV8Version}/src_binding_release_${pin.target}.rs";
          hash = pin.bindingsHash;
        };
      }) v8Pins.musl;

      # When building from a release commit the Cargo.toml already carries the
      # real version (e.g. "0.101.0").  On the main branch it is the placeholder
      # "0.0.0", so we fall back to a dev version derived from the flake source.
      version =
        if cargoVersion != "0.0.0"
        then cargoVersion
        else "0.0.0-dev+${self.shortRev or "dirty"}";
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          codex-rs = pkgs.callPackage ./codex-rs {
            inherit version;
            rustyV8Archive = pkgs.fetchurl rustyV8ArchiveBySystem.${system};
            rustPlatform = pkgs.makeRustPlatform {
              cargo = pkgs.rust-bin.stable.${rustVersion}.minimal;
              rustc = pkgs.rust-bin.stable.${rustVersion}.minimal;
            };
          };
          sandbox-server-dynamic = pkgs.callPackage ./codex-rs {
            pname = "codex-sandbox-server";
            inherit version;
            cargoBuildFlags = [
              "-p"
              "codex-sandbox-server"
              "-p"
              "codex-linux-sandbox"
            ];
            mainProgram = "codex-sandbox-server";
            rustyV8Archive = pkgs.fetchurl rustyV8ArchiveBySystem.${system};
            rustPlatform = pkgs.makeRustPlatform {
              cargo = pkgs.rust-bin.stable.${rustVersion}.minimal;
              rustc = pkgs.rust-bin.stable.${rustVersion}.minimal;
            };
          };
          musl = rustyV8MuslBySystem.${system} or null;
          muslRust = if musl == null then null else pkgs.rust-bin.stable.${rustVersion}.minimal.override {
            targets = [ musl.target ];
          };
          muslPkgs =
            if system == "x86_64-linux" then
              pkgs.pkgsCross.musl64
            else if system == "aarch64-linux" then
              pkgs.pkgsCross.aarch64-multiplatform-musl
            else
              null;
          sandbox-server-static = if musl == null then null else muslPkgs.callPackage ./codex-rs {
            pname = "codex-sandbox-server-static";
            inherit version;
            perl = pkgs.perl;
            cargoBuildFlags = [
              "-p"
              "codex-sandbox-server"
              "-p"
              "codex-linux-sandbox"
            ];
            mainProgram = "codex-sandbox-server";
            rustyV8Archive = pkgs.fetchurl musl.archive;
            rustyV8SrcBindingPath = pkgs.fetchurl musl.bindings;
            rustPlatform = muslPkgs.makeRustPlatform {
              cargo = muslRust;
              rustc = muslRust;
            };
          };
        in
        {
          codex-rs = codex-rs;
          sandbox-server =
            if sandbox-server-static != null then sandbox-server-static else sandbox-server-dynamic;
          sandbox-server-dynamic = sandbox-server-dynamic;
          default = codex-rs;
        } // pkgs.lib.optionalAttrs (sandbox-server-static != null) {
          sandbox-server-static = sandbox-server-static;
        }
      );

      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rust = pkgs.rust-bin.stable.${rustVersion}.default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
          };
        in
        {
          default = pkgs.mkShell {
            buildInputs = [
              rust
              pkgs.pkg-config
              pkgs.openssl
              pkgs.cmake
              pkgs.llvmPackages.clang
              pkgs.llvmPackages.libclang.lib
            ];
            PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            # Use clang for BoringSSL compilation (avoids GCC 15 warnings-as-errors)
            shellHook = ''
              export CC=clang
              export CXX=clang++
            '';
          };
        }
      );
    };
}
