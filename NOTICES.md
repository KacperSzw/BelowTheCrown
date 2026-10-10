# Third-party notices

Our Rust code, authored worlds, baked portraits and original assets are under the
MIT license in `LICENSE`. Dependencies retain their own licenses; MIT does not
relicense them. Release source archives preserve all vendored Cargo package
sources and their license files.

## Worker helper

The binary contains an independently executable **bubblewrap 0.13.0** payload,
extracted to a private cache before use. Copyright belongs to its upstream
contributors. Bubblewrap is licensed under GNU LGPL version 2.1 or later, except
files carrying their own compatible notices. The full license is supplied in
`licenses/bubblewrap-COPYING.LIB`.

- Source: https://github.com/containers/bubblewrap/tree/v0.13.0
- Exact source archive and hash: `flake.nix`
- Corresponding source/build instructions: release source archives and
  `docs/sandbox.md`
- No patches are applied to bubblewrap by this project.

The helper statically uses **libcap** (BSD 3-clause) and **musl libc** (MIT).
Their notices are preserved in `licenses/`. The pinned source/build recipe is
included with helper corresponding source; libcap sources are included too.

## Rust dependencies

The statically linked Rust 1.99 standard library retains the Rust Project's
MIT/Apache-2.0 licensing. Its exact notices are preserved in
`licenses/rust-LICENSE-MIT`, `licenses/rust-LICENSE-APACHE` and
`licenses/rust-COPYRIGHT`, from
[Rust 1.99](https://github.com/rust-lang/rust/tree/1.99.0).

`Cargo.lock` identifies the exact dependency versions. Each binary's release
source archive includes `cargo-vendor/`, containing the complete source and
license notices for those versions, and the Cargo vendor configuration. This
includes MPL-2.0 licensed WezTerm/Termwiz components where applicable; those
covered files remain available in their original source form. SQLite's bundled
implementation is public domain. Other libraries remain under their declared
MIT, Apache, BSD and compatible upstream licenses.

## Artwork and music

The three portrait PNGs were generated specifically for this project using the
available OpenAI image generator on 10 October 2026. They contain no imported
photos or third-party artwork. Source manifests record normalized face crops;
Braille is baked offline by Chafa. The original developer created `dungeon.wav`
from synthesized tones for this game. These project assets are offered under
MIT to the extent we hold rights in them.

Chafa and ImageMagick are authoring tools and are not bundled or invoked during
play. Codex CLI is a separately installed dependency, not redistributed here.
