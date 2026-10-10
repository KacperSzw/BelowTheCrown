#!/usr/bin/env bash
set -euo pipefail
: "${GH_REPO:?Repository required}"
: "${RELEASE_REF:?Git ref required}"
: "${RELEASE_TAG:?Ref name required}"
: "${RELEASE_SHA:?Commit SHA required}"
[[ -n ${GH_TOKEN:-} ]] || { printf 'Publishing token missing\n' >&2; exit 1; }

if [[ $RELEASE_REF == refs/heads/main ]]; then
  # A slow older workflow must not replace a newer main preview.
  current_sha=$(gh api "repos/$GH_REPO/git/ref/heads/main" --jq '.object.sha')
  if [[ $current_sha != "$RELEASE_SHA" ]]; then printf 'Skipping superseded preview\n'; exit 0; fi
  release_tag=preview
  if gh api "repos/$GH_REPO/git/ref/tags/$release_tag" > /dev/null 2>&1; then
    gh api --method PATCH "repos/$GH_REPO/git/refs/tags/$release_tag" -f sha="$RELEASE_SHA" -F force=true > /dev/null
  else
    gh api --method POST "repos/$GH_REPO/git/refs" -f ref="refs/tags/$release_tag" -f sha="$RELEASE_SHA" > /dev/null
  fi
  if ! gh release view "$release_tag" > /dev/null 2>&1; then
    gh release create "$release_tag" --target "$RELEASE_SHA" --title 'Below the Crown preview' --prerelease --notes 'Latest verified main build. Linux LAN/localhost SSH. Host requires a compatible Codex CLI login.'
  fi
  gh release edit "$release_tag" --target "$RELEASE_SHA" --title 'Below the Crown preview' --prerelease --notes "Verified main commit $RELEASE_SHA. Linux LAN/localhost SSH."
  gh release upload "$release_tag" dist/* --clobber
else
  [[ $RELEASE_REF == refs/tags/v* ]] || { printf 'Unsupported publication ref\n' >&2; exit 1; }
  package_version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -n 1)
  [[ $RELEASE_TAG == "v$package_version" ]] || { printf 'Tag differs from Cargo package version\n' >&2; exit 1; }
  if gh release view "$RELEASE_TAG" > /dev/null 2>&1; then
    printf 'Stable release already exists; refusing replacement\n' >&2
    exit 1
  fi
  gh release create "$RELEASE_TAG" dist/* --target "$RELEASE_SHA" --title "Below the Crown $package_version" --generate-notes
fi
