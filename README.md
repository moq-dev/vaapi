# moq-vaapi

A small, self-contained **VA-API H.264 hardware encoder** for Linux (Intel / AMD),
vendored and trimmed from [cros-libva](https://github.com/intel/cros-libva) and
[discord/cros-codecs](https://github.com/discord/cros-codecs) (BSD-3-Clause).

It exists to give [moq](https://github.com/moq-dev/moq)'s `moq-video` a thin,
crates.io-publishable VA-API encoder brick instead of a multi-backend framework
pulled in as a git dependency.

## What it does

- **H.264 encode** over VA-API: tightly-packed NV12 in, an Annex-B elementary
  stream out (packed SPS/PPS + slice headers, low-latency IPPP, rate control).
- **Hermetic build.** libva's headers are vendored into [`libva/`](./libva) (see
  `just vendor`) and fed to bindgen, so the build needs **no system libva-dev** —
  only `libclang`. The header version is pinned, so a system libva bump can't
  drift the generated bindings.
- **Runtime `dlopen`.** libva is loaded at runtime rather than linked, so a built
  binary links on a libva-less builder and starts on machines without libva
  (callers fall back to a software encoder).

## Layout

- `src/` — libva bindings (`bindings`, `display`, `surface`, `buffer`, ...), the
  H.264 bitstream layer (`bitstream_utils`, `codec::h264`), and the thin encode
  driver (`encode`).
- `libva/` — vendored libva headers (checked in; refreshed by `just vendor`).
- `build.rs` + `bindgen_gen.rs` + `libva-wrapper.h` — bindgen setup.

## Development

Recipes run inside the nix devShell:

```sh
nix develop --command just check     # clippy + fmt
nix develop --command just ci        # check + cargo-deny
nix develop --command just vendor    # refresh libva/ headers from upstream
```

The crate compiles on any OS (macOS included) because the build is header-only +
dlopen; runtime use is Linux-only.

## Licensing

BSD-3-Clause (see [`LICENSE`](./LICENSE)). Vendored upstreams keep their notices:
[`LICENSE.cros-codecs`](./LICENSE.cros-codecs) and the vendored libva headers
under [`LICENSE.libva`](./LICENSE.libva) (MIT).
