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

## Native Codex request conformance

The ignored conformance test drives the installed CLI against a local mock
provider. It captures actual Responses and Responses Lite requests, including
a recall round trip. It checks replacement of built-in instructions, bound
identity, strict schema, routing and tools. Successful NPC history is reused
only by its owner; other NPCs and fresh listeners receive no foreign history.
Only the two expected NPC threads remain loaded; reset destroys the worker.
This test makes no paid model decisions. It requires a real sanitized catalog
from the game's metadata-only doctor:

```sh
cargo run --locked -- --data-dir /tmp/btc-request-check doctor
BTC_CONFORMANCE_CATALOG=/tmp/btc-request-check/worker/codex/npc-models.json \
  cargo test --locked --lib codex_request_conformance -- --ignored --nocapture
```

Use a private data directory. Do not publish its authentication files or catalog.
The separate offline lifecycle tests cover late acknowledgements, caller drops,
reset generations, recall revocation, stalled unloading and interrupted writes.

## Paid NPC episodes

Normal `cargo test` never runs live episodes. Explicit opt-in exercises cook,
guard and captain continuity in castle/siege/succession, in both EN and PL,
plus six other-role conversations. There are thirty initial decisions, with at
most one correction each. Each episode controls the clock and uses validated
physical stimuli. Three episodes can run together; there is no autonomous host
or background inference.

```sh
BTC_RUN_PAID_NPC_SCENARIOS=1 BTC_LIVE_REPORT=/tmp/btc-live-review/review.jsonl \
  cargo test --locked --test npc_live npc_believability_live_scenarios \
  -- --ignored --nocapture
```

The private JSONL report records observations, final decisions, evidence IDs,
validator results, physical before/after state, latency and token counts. It
does not record credentials or private reasoning. A sibling `review.budget.json`
reserves each call before starting it and caps the cumulative count at sixty,
including corrections and reruns using the same report path. Keep that path
for the whole review; do not reset the budget to extend a session's cap.

To rerun affected episodes only, set `BTC_LIVE_EPISODES` to a comma-separated
list such as `castle-en-106,castle-pl-106,siege-en-102,siege-pl-102`. These names
identify the existing fixtures; arbitrary characters or worlds are rejected.
Review dialogue and memory continuity by hand: validator success establishes
legal actions, not believable characterization. See [NPC minds](npc-minds.md).

## Pacing comparison (separate authorization)

The ignored `npc_pacing_live_scenarios` uses a separate ledger with a maximum
of 48 new NPC decisions, including corrections. It exercises a burst of
questions and pronouns, taking/drinking/movement to fulfill a promise, and
authority plus an actual bribe. It records actual execution after each decision,
not just acceptance of JSON. Production NPC threads stay continuous inside
each episode; a new episode starts a new private driver.

`BTC_LIVE_VARIANT` labels a comparison, `BTC_LIVE_SEED` selects a controlled
seed, and `BTC_LIVE_PERSONA_SMOKE=1` replaces the main episodes with six
one-decision persona checks across all worlds and both languages. Use a new
private report directory for this authorization, never the exhausted 60-call
ledger. Calls are charged before launch, including errors, and ledger limits
cannot be silently enlarged or reset.

```sh
BTC_RUN_PAID_NPC_SCENARIOS=1 BTC_LIVE_VARIANT=after \
  BTC_LIVE_REPORT=/tmp/btc-pacing-comparison/review.jsonl \
  cargo test --locked --test npc_live npc_pacing_live_scenarios -- --ignored --nocapture
```
