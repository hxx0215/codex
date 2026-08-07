{
  cmake,
  llvmPackages,
  openssl,
  perl ? null,
  libcap ? null,
  rustPlatform,
  rustyV8Archive ? null,
  rustyV8SrcBindingPath ? null,
  pkg-config,
  lib,
  stdenv,
  cargoBuildFlags ? [ ],
  mainProgram ? "codex",
  pname ? "codex-rs",
  version ? "0.0.0",
  ...
}:
rustPlatform.buildRustPackage (_: {
  env.RUSTFLAGS = lib.optionalString stdenv.hostPlatform.isMusl "-C target-feature=+crt-static";
  env.PKG_CONFIG_PATH = lib.makeSearchPathOutput "dev" "lib/pkgconfig" (
    [ openssl ] ++ lib.optionals stdenv.isLinux [ libcap ]
  );
  inherit cargoBuildFlags pname;
  inherit version;
  cargoLock.lockFile = ./Cargo.lock;
  env.RUSTY_V8_ARCHIVE = lib.optionalString (rustyV8Archive != null) "${rustyV8Archive}";
  env.RUSTY_V8_SRC_BINDING_PATH = lib.optionalString (rustyV8SrcBindingPath != null) "${rustyV8SrcBindingPath}";
  doCheck = false;
  src = ./.;

  # Patch the workspace Cargo.toml so that cargo embeds the correct version in
  # CARGO_PKG_VERSION (which the binary reads via env!("CARGO_PKG_VERSION")).
  # On release commits the Cargo.toml already contains the real version and
  # this sed is a no-op.
  postPatch = ''
    sed -i 's/^version = "0\.0\.0"$/version = "${version}"/' Cargo.toml
  '';
  nativeBuildInputs = [
    cmake
    llvmPackages.clang
    llvmPackages.libclang.lib
    openssl
    pkg-config
  ] ++ lib.optionals (perl != null) [
    perl
  ] ++ lib.optionals stdenv.isLinux [
    libcap
  ];

  cargoLock.outputHashes = {
    "crossterm-0.29.0" = "sha256-ewiWWQPEU1lSUHzmZTiO5yes5luIaQ9TrvCNnTWhxpE=";
    "nucleo-0.5.0" = "sha256-Hm4SxtTSBrcWpXrtSqeO0TACbUxq3gizg1zD/6Yw/sI=";
    "nucleo-matcher-0.3.1" = "sha256-Hm4SxtTSBrcWpXrtSqeO0TACbUxq3gizg1zD/6Yw/sI=";
    "runfiles-0.1.0" = "sha256-uJpVLcQh8wWZA3GPv9D8Nt43EOirajfDJ7eq/FB+tek=";
    "tokio-tungstenite-0.28.0" = "sha256-V1xmnrfRWOcZZogelZEA4vvyMj2awCfHVA5/glQ6KAI=";
    "tungstenite-0.27.0" = "sha256-VVHhk7l9J/sEmG3q/UuV/sQ3f+fGsmq5vumSy8vbMvw=";
  };

  meta = with lib; {
    description = "OpenAI Codex command‑line interface rust implementation";
    license = licenses.asl20;
    homepage = "https://github.com/openai/codex";
    inherit mainProgram;
  };
})
