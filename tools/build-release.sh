#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"
release_target=${1:-}
case "$release_target" in
  x86_64-unknown-linux-musl) release_arch=x86_64 ;;
  aarch64-unknown-linux-musl) release_arch=aarch64 ;;
  *) printf 'Usage: %s x86_64-unknown-linux-musl|aarch64-unknown-linux-musl\n' "$0" >&2; exit 2 ;;
esac
if [[ $(uname -m) == "$release_arch" ]]; then
  release_shell=default
  release_helper=sandbox-helper
elif [[ $(uname -m) == x86_64 && $release_arch == aarch64 ]]; then
  release_shell=cross-aarch64
  release_helper=sandbox-helper-aarch64
else
  printf 'Build this target natively, or ARM64 from an x64 host.\n' >&2
  exit 2
fi

# Git filtering excludes volatile/large build output, even before the first commit.
if [[ -d .git ]]; then release_flake="$repo_dir"; else release_flake="path:$repo_dir"; fi
release_helper_dir=$(nix build --no-link --print-out-paths "$release_flake#$release_helper")
release_source_dir=$(nix build --no-link --print-out-paths "$release_flake#helper-sources")
release_target_dir=${CARGO_TARGET_DIR:-target}
BTC_SANDBOX_HELPER="$release_helper_dir/bin/bwrap" BTC_REQUIRE_BUNDLED_HELPER=1 \
  nix develop "$release_flake#$release_shell" --command cargo build --locked --release --target "$release_target"

mkdir -p dist
release_name="below-the-crown-linux-$release_arch"
cp "$release_target_dir/$release_target/release/below-the-crown" "dist/$release_name"
chmod 755 "dist/$release_name"
./tools/verify-elf.sh "dist/$release_name" "$release_arch"
./tools/verify-elf.sh "$release_helper_dir/bin/bwrap" "$release_arch"

# Preserve every locked crate's license and covered source, including MPL files.
# This stays under ignored build output and never vendors legacy implementation.
release_stage=$(mktemp -d "${TMPDIR:-/tmp}/below-the-crown-release.XXXXXXXX")
trap 'chmod -R u+w "$release_stage"; rm -r -- "$release_stage"' EXIT
mkdir -p "$release_stage/helper-source" "$release_stage/project" "$release_stage/licenses"
cp -a "$release_source_dir/." "$release_stage/helper-source/"
chmod -R u+w "$release_stage/helper-source"
cp LICENSE NOTICES.md Cargo.toml Cargo.lock rust-toolchain.toml README.md AGENTS.md flake.nix flake.lock build.rs .gitignore "$release_stage/project/"
cp -a src content docs examples tools licenses tests .github "$release_stage/project/"
cp licenses/* "$release_stage/licenses/"
if ! nix develop "$release_flake#default" --command cargo vendor --locked "$release_stage/cargo-vendor" \
    > "$release_stage/cargo-vendor-config.toml" 2> "$release_stage/vendor.log"; then
  cat "$release_stage/vendor.log" >&2
  exit 1
fi
rm -- "$release_stage/vendor.log"
mkdir -p "$release_stage/project/.cargo"
sed 's|^directory = ".*"$|directory = "../cargo-vendor"|' "$release_stage/cargo-vendor-config.toml" > "$release_stage/project/.cargo/config.toml"
rm -- "$release_stage/cargo-vendor-config.toml"
# Exercise the archived configuration with no global registry or cached crates.
mkdir "$release_stage/cargo-home"
(cd "$release_stage/project" && CARGO_HOME="$release_stage/cargo-home" \
  nix develop "$release_flake#default" --command cargo metadata --offline --locked --format-version 1 > /dev/null)
rm -r -- "$release_stage/cargo-home"
# GNU tar normalizes ownership/time for reproducible source bundles.
tar --sort=name --mtime='UTC 2026-01-01' --owner=0 --group=0 --numeric-owner -cJf \
  "dist/$release_name-sources.tar.xz" -C "$release_stage" .
cp LICENSE "dist/$release_name-LICENSE"
cp NOTICES.md "dist/$release_name-NOTICES.md"
(cd dist && sha256sum "$release_name" "$release_name-sources.tar.xz" "$release_name-LICENSE" "$release_name-NOTICES.md" > "$release_name-SHA256SUMS")
printf 'Built %s\n' "$repo_dir/dist/$release_name"
