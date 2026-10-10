#!/usr/bin/env bash
set -euo pipefail
binary=${1:?ELF path required}
architecture=${2:?x86_64 or aarch64 required}
if [[ ! -f $binary ]]; then printf 'Missing binary: %s\n' "$binary" >&2; exit 1; fi
headers=$(LC_ALL=C readelf -h "$binary")
programs=$(LC_ALL=C readelf -l "$binary")
dynamic=$(LC_ALL=C readelf -d "$binary")
case "$architecture" in
  x86_64) machine='Advanced Micro Devices X86-64' ;;
  aarch64) machine='AArch64' ;;
  *) printf 'Unsupported architecture\n' >&2; exit 2 ;;
esac
if ! [[ $headers == *"$machine"* && $headers == *ELF64* ]]; then
  printf 'Wrong ELF architecture: %s\n' "$binary" >&2; exit 1
fi
if [[ $programs == *INTERP* || $dynamic == *NEEDED* ]]; then
  printf 'ELF requires a runtime loader or dynamic library: %s\n' "$binary" >&2; exit 1
fi
printf 'Static %s ELF verified: %s\n' "$architecture" "$binary"
