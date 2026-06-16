#!/usr/bin/env just --justfile
#
# moq-vaapi: a VA-API H.264 encoder vendored from cros-libva + discord/cros-codecs.
# Recipes run inside the nix devShell: `nix develop --command just <recipe>`.

# libva release to vendor headers from. VA-API 1.23 (= the seg_id_block_size /
# va_reserved8 VP9 struct fields) is the floor the binding is written against.
LIBVA_TAG := "2.23.0"

default:
    just check

# Compile, lint, and format-check.
check:
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all --check

# Full CI: check + dependency hygiene.
ci:
    just check
    cargo deny check --show-stats

# Auto-fix clippy + formatting.
fix:
    cargo clippy --fix --allow-staged --allow-dirty --all-targets
    cargo fmt --all

# Refresh the vendored libva headers in `libva/` from upstream at {{ LIBVA_TAG }}.
# We check the headers in (rather than a submodule) so the crate is a
# self-contained, crates.io-publishable tarball. Only the files bindgen needs are
# copied: meson.build (version), va/*.h{,.in}, and va/drm/va_drm.h.
vendor:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    git clone --depth 1 --branch {{ LIBVA_TAG }} https://github.com/intel/libva.git "$tmp"
    rm -rf libva
    mkdir -p libva/va/drm
    cp "$tmp/meson.build" libva/meson.build
    cp "$tmp"/va/*.h "$tmp"/va/*.h.in libva/va/
    cp "$tmp/va/drm/va_drm.h" libva/va/drm/va_drm.h
    cp "$tmp/COPYING" LICENSE.libva
    echo "vendored libva {{ LIBVA_TAG }} into libva/ ($(find libva -type f | wc -l | tr -d ' ') files)"
