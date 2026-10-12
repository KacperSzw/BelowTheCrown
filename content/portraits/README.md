# Baked Braille portraits

`baked.json` embeds 33 monochrome faces in three terminal-cell sizes: 11 × 5,
15 × 7 and 21 × 9. Every row contains only spaces or U+2800–U+28FF Braille.
There are no ANSI colors, animation, image protocols or runtime conversion.

Faces share the same library for humans and NPCs. Permanent actors retain curated
keys from `sources.json`; newly joined players receive unused faces while any
remain. Saved `portraitKey` is cosmetic and stable across language, movement,
equipment and connection changes. Unknown retired keys display names without art.

A terminal may support UTF-8 without a suitable font. Automatic mode therefore
uses conservative hints; `/portraits on` and `/portraits off` override them for
the session. Cramped layouts omit artwork to preserve names, chat and input.

## Sources and provenance

The nine PNG sheets were generated for this project with the available image
generator on 12 October 2026.
There are no imported photographs or external character illustrations.

- `sources/studies-00.png`: three initial studies for the guard, captain and servant.
- `sources/studies-01.png` through `studies-07.png`: four distinct adults per sheet.
- `sources/studies-08.png`: two further faces, completing the 33-face library.

The direction is rough naturalism: uneven faces, crooked noses, receding chins,
heavy jowls, worn skin and imperfect posture. Ages, builds, hair and headwear vary;
coarse grayscale light keeps the features readable in monochrome Braille.

The manifest records the original PNG, normalized `[x,y,width,height]` crop and
optional gamma/contrast. All previous portrait keys and curated actor mappings
are preserved, so existing saves use the new artwork for the same identities.

## Bake

From the repository root:

```sh
nix develop --command cargo run --example bake_portraits
# One edited source/crop:
nix develop --command cargo run --example bake_portraits -- --only captain
```

The Rust example uses ImageMagick for crop/grayscale and aspect-preserving fitting
to a black canvas for each terminal size, then Chafa's symbol renderer
with `--colors none --symbols braille`, fixed dimensions and no terminal probe.
It validates bounds, pads rows and prints a review preview before writing the
runtime library. Review and commit source, manifest and baked text together.
