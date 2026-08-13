#!/usr/bin/env bash
# Refresh nix/v8-pins.json for the v8 crate version currently in
# codex-rs/Cargo.lock.
#
# The prebuilt rusty_v8 archive URLs are templated on that version, so bumping
# the crate silently repoints every fetch at new bytes.  Run this whenever
# Cargo.lock changes v8; flake.nix refuses to evaluate while the two disagree.
set -euo pipefail

repo_root="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
pins_file="${repo_root}/nix/v8-pins.json"

version="$(awk '/^name = "v8"$/ { getline; gsub(/"/, ""); print $3; exit }' \
  "${repo_root}/codex-rs/Cargo.lock")"
if [[ -z "${version}" ]]; then
  echo "Could not read the v8 crate version from codex-rs/Cargo.lock." >&2
  exit 1
fi
echo "Pinning rusty_v8 ${version}..." >&2

gnu_base="https://github.com/denoland/rusty_v8/releases/download/v${version}"
musl_base="https://github.com/openai/codex/releases/download/rusty-v8-v${version}"

# nix-prefetch-url emits base32; the flake wants SRI.
prefetch() {
  local url="$1" raw
  echo "  fetching ${url##*/}" >&2
  if ! raw="$(nix-prefetch-url "${url}" 2>/dev/null)" || [[ -z "${raw}" ]]; then
    echo "Failed to fetch ${url}" >&2
    exit 1
  fi
  nix hash convert --hash-algo sha256 --to sri "${raw}"
}

gnu_x86_64_linux="$(prefetch "${gnu_base}/librusty_v8_release_x86_64-unknown-linux-gnu.a.gz")"
gnu_aarch64_linux="$(prefetch "${gnu_base}/librusty_v8_release_aarch64-unknown-linux-gnu.a.gz")"
gnu_x86_64_darwin="$(prefetch "${gnu_base}/librusty_v8_release_x86_64-apple-darwin.a.gz")"
gnu_aarch64_darwin="$(prefetch "${gnu_base}/librusty_v8_release_aarch64-apple-darwin.a.gz")"

musl_x86_64_archive="$(prefetch "${musl_base}/librusty_v8_release_x86_64-unknown-linux-musl.a.gz")"
musl_x86_64_bindings="$(prefetch "${musl_base}/src_binding_release_x86_64-unknown-linux-musl.rs")"
musl_aarch64_archive="$(prefetch "${musl_base}/librusty_v8_release_aarch64-unknown-linux-musl.a.gz")"
musl_aarch64_bindings="$(prefetch "${musl_base}/src_binding_release_aarch64-unknown-linux-musl.rs")"

cat > "${pins_file}" <<EOF
{
  "_comment": "Pinned hashes for prebuilt rusty_v8 archives. Regenerate with ./nix/update-v8-pins.sh whenever codex-rs/Cargo.lock bumps the v8 crate.",
  "version": "${version}",
  "gnu": {
    "x86_64-linux": {
      "target": "x86_64-unknown-linux-gnu",
      "hash": "${gnu_x86_64_linux}"
    },
    "aarch64-linux": {
      "target": "aarch64-unknown-linux-gnu",
      "hash": "${gnu_aarch64_linux}"
    },
    "x86_64-darwin": {
      "target": "x86_64-apple-darwin",
      "hash": "${gnu_x86_64_darwin}"
    },
    "aarch64-darwin": {
      "target": "aarch64-apple-darwin",
      "hash": "${gnu_aarch64_darwin}"
    }
  },
  "musl": {
    "x86_64-linux": {
      "target": "x86_64-unknown-linux-musl",
      "archiveHash": "${musl_x86_64_archive}",
      "bindingsHash": "${musl_x86_64_bindings}"
    },
    "aarch64-linux": {
      "target": "aarch64-unknown-linux-musl",
      "archiveHash": "${musl_aarch64_archive}",
      "bindingsHash": "${musl_aarch64_bindings}"
    }
  }
}
EOF

echo "Wrote ${pins_file#"${repo_root}/"} for v8 ${version}." >&2
