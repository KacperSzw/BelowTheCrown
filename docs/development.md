# Development

The application is Rust. Content, portraits and original audio are embedded;
release binaries do not read a repository checkout or invoke an asset converter.

```sh
nix develop
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo run -- host
```

The pinned shell supplies Rust 1.99, C compilers, an isolated worker helper and
portrait-authoring tools. Codex is an external dependency and is never installed
or reconfigured by this shell. Public tests use fixture decisions and private
temporary storage, not paid model calls.

A conventional Rust toolchain also works for development. Provide a compatible
C compiler for bundled SQLite/ring. Debug builds can use an explicit sandbox
helper override; production decisions fail closed if isolation is unavailable.

## Release

Build natively on Linux x64 or ARM64:

```sh
./tools/build-release.sh x86_64-unknown-linux-musl
# On an ARM64 machine:
./tools/build-release.sh aarch64-unknown-linux-musl
```

The script builds the pinned static bubblewrap helper, embeds it, compiles the
musl target, rejects dynamic ELF dependencies and prepares executable, license,
checksum and corresponding-source artifacts in `dist/`. Nix is a build tool;
it is not required by downloaded binaries.

Every push and pull request checks formatting, Clippy, tests and both native
architecture builds. Main publishes a rolling `preview`. A `v<package-version>`
tag publishes a stable release once; existing stable releases are not replaced.
Only publishing jobs receive repository-write permission. No workflow has Codex
credentials or calls a real model.

## Content

[World packages](../content/worlds/README.md) describe authored fiction and
localization. [Portrait authoring](../content/portraits/README.md) covers source
crops and the offline Braille bake. [Architecture](architecture.md) documents
state ownership, scheduling, privacy and terminal transport.

Tests protect rules and lifecycle transitions. Use controlled clocks/randomness
and test-owned tuning. Avoid assertions that freeze incidental balance values.
Do not add a scripted production fallback when a model fails.
