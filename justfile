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

# Full CI: check, unit tests, bindings drift, and dependency hygiene.
ci:
    just check
    # The checked-in bindings must be what the vendored headers generate.
    just bindgen
    git diff --exit-code src/bindings/libva.rs
    cargo test --lib
    # A consumer must be able to choose its own release logging level.
    cargo check --lib --features log/release_max_level_trace
    cargo deny check --show-stats

# Auto-fix clippy + formatting.
fix:
    cargo clippy --fix --allow-staged --allow-dirty --all-targets
    cargo fmt --all

# Refresh the vendored libva headers in `libva/` from upstream at {{ LIBVA_TAG }},
# then regenerate the bindings from them. Only the files bindgen needs are
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
    just bindgen

# Regenerate src/bindings/libva.rs from the vendored headers in `libva/`.
# Builds need neither this nor libclang: the output is checked in, and the C
# types it uses are fixed-width or symbolic (`c_char`), so one file serves every
# target. The exception is libva's own: two VPP structs pad to a constant size
# with a reserved array whose length depends on the pointer width, so both
# layouts are generated and the lines that differ are kept under a `cfg`.
bindgen:
    #!/usr/bin/env bash
    set -euo pipefail
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    # libva generates va_version.h from a template at build time, and installs
    # va_drm.h beside va.h; stage both where `#include <va/...>` finds them.
    mkdir -p "$work/va"
    version() { sed -nE "s/^$1\s*=\s*([0-9]+).*/\1/p" libva/meson.build; }
    major=$(version va_api_major_version)
    minor=$(version va_api_minor_version)
    micro=$(version va_api_micro_version)
    sed -e "s/@VA_API_MAJOR_VERSION@/$major/" -e "s/@VA_API_MINOR_VERSION@/$minor/" \
        -e "s/@VA_API_MICRO_VERSION@/$micro/" -e "s/@VA_API_VERSION@/$major.$minor.$micro/" \
        libva/va/va_version.h.in > "$work/va/va_version.h"
    cp libva/va/drm/va_drm.h "$work/va/va_drm.h"
    # `--dynamic-loading` emits a `Va` struct of function pointers that
    # libloading resolves at runtime, rather than link-time externs, so libva is
    # dlopen'd. A symbol a system libva older than these headers lacks errors
    # only when called, not when the library loads. Freestanding, so the host's
    # C library cannot change the output.
    generate() {
        bindgen libva-wrapper.h \
            --rust-target 1.85 \
            --rust-edition 2021 \
            --no-layout-tests \
            --with-derive-default \
            --with-derive-eq \
            --constified-enum-module 'VA.*' \
            --allowlist-var 'VA.*' \
            --allowlist-function 'va.*' \
            --allowlist-type '.*ExternalBuffers.*|.*PRIME.*|.*MPEG2.*|.*VP8.*|.*VP9.*|.*H264.*|.*HEVC.*|.*JPEG.*|VACodedBufferSegment|.*AV1.*|VAEncMisc.*|VASurfaceDecodeMBErrors|VADecodeErrorType|.*VAProc.*|VAEncPacked.*' \
            --dynamic-loading Va \
            --output "$2" \
            -- -Ilibva -I"$work" -ffreestanding --target="$1"
    }
    generate x86_64-unknown-linux-gnu "$work/wide.rs"
    generate i686-unknown-linux-gnu "$work/narrow.rs"
    # Only the reserved padding may differ; anything else needs a look before
    # it is papered over with a cfg.
    unexpected=$(diff "$work/wide.rs" "$work/narrow.rs" | grep -E '^[<>]' | grep -vE '^[<>]\s*pub va_reserved: \[u(16|32); [0-9]+usize\],$' || true)
    if [ -n "$unexpected" ]; then
        printf 'the bindings differ by pointer width in more than libva'"'"'s reserved padding:\n%s\n' "$unexpected" >&2
        exit 1
    fi
    # The condition libva's headers pad on: x86_64, or LP64.
    wide='any(target_arch = "x86_64", all(target_pointer_width = "64", not(windows)))'
    diff --unchanged-line-format='%L' \
        --old-line-format=$'\t#[cfg('"$wide"$')]\n%L' \
        --new-line-format=$'\t#[cfg(not('"$wide"$'))]\n%L' \
        "$work/wide.rs" "$work/narrow.rs" > src/bindings/libva.rs || [ $? -eq 1 ]
