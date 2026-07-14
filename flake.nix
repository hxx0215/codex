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
      rustyV8Version = (builtins.head (builtins.filter (package: package.name == "v8") cargoLock.package)).version;
      rustToolchain = builtins.fromTOML (builtins.readFile ./codex-rs/rust-toolchain.toml);
      rustVersion = rustToolchain.toolchain.channel;
      rustyV8ArchiveBySystem = {
        x86_64-linux = {
          url = "https://github.com/denoland/rusty_v8/releases/download/v${rustyV8Version}/librusty_v8_release_x86_64-unknown-linux-gnu.a.gz";
          hash = "sha256-iu2YY323533Iv7i7R1nsW95HLQv3lD9Y4OYqNQlFxVk=";
        };
        aarch64-linux = {
          url = "https://github.com/denoland/rusty_v8/releases/download/v${rustyV8Version}/librusty_v8_release_aarch64-unknown-linux-gnu.a.gz";
          hash = "sha256-+XdRJ8pk3MSjZi0BpSGizvuluY+DOUOog9hHc7Kv88U=";
        };
        x86_64-darwin = {
          url = "https://github.com/denoland/rusty_v8/releases/download/v${rustyV8Version}/librusty_v8_release_x86_64-apple-darwin.a.gz";
          hash = "sha256-eUlAo4o/ZrfvUqXwA8awlPdDrQQKZK+z082frUlADwc=";
        };
        aarch64-darwin = {
          url = "https://github.com/denoland/rusty_v8/releases/download/v${rustyV8Version}/librusty_v8_release_aarch64-apple-darwin.a.gz";
          hash = "sha256-+rsuyNO6Wm3qY9uaNalg3FypheujLzQrm6Sqocc0sv4=";
        };
      };

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
          sandbox-server = pkgs.callPackage ./codex-rs {
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
        in
        {
          codex-rs = codex-rs;
          sandbox-server = sandbox-server;
          default = codex-rs;
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
