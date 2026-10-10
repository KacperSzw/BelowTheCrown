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

The three PNGs were generated for this project with the available image generator
on 10 October 2026, after the user approved that alternative to Prophet Draw.
There are no imported photographs or external character illustrations.

- `sources/captain.png`: male captain, dark hair, short beard, broken nose and
  eyebrow scar, black background and broad grayscale charcoal lighting.
- `sources/atlas-a.png`: sixteen distinct medieval adults, including the initial
  guard, king, steward, smith, courtier, porter, cook, servant and healer.
- `sources/atlas-b.png`: sixteen further faces with varied ages, features,
  hair and headwear in the same style.

The manifest records the original PNG, normalized `[x,y,width,height]` crop and
optional gamma/contrast. Existing identity crops are preserved.

## Bake

From the repository root:

```sh
nix develop --command cargo run --example bake_portraits
# One edited source/crop:
nix develop --command cargo run --example bake_portraits -- --only captain
```

The Rust example uses ImageMagick for crop/grayscale and Chafa's symbol renderer
with `--colors none --symbols braille`, fixed dimensions and no terminal probe.
It validates bounds, pads rows and prints a review preview before writing the
runtime library. Review and commit source, manifest and baked text together.
