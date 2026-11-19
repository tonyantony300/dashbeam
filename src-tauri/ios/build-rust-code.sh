#!/bin/sh
# Xcode's "Build Rust Code" phase (see project.yml). Xcode runs build phases
# with a minimal environment, so when nix is installed, run the command inside
# the flake's iOS dev shell to get the pinned toolchain, pnpm and the Xcode
# toolchain overrides. Without nix, run it as-is.
set -eu

nix=/nix/var/nix/profiles/default/bin/nix
if [ -x "$nix" ]; then
  exec "$nix" develop "${SRCROOT:?}/../../..#ios" --command "$@"
fi
exec "$@"
