# moq-vaapi

> **⚠️ AI GENERATED.** This crate was written by an AI agent (Claude), using the
> repositories below as references. How closely individual files track upstream
> varies, so treat the whole thing as derived work and review before relying on
> it. The encoder and the decoder have both been checked against ffmpeg's
> software codecs on Intel hardware (see below).

A small, self-contained **VA-API H.264 / H.265 hardware codec** for Linux (Intel / AMD),
derived from
[discord/cros-libva @ discord-0.0.13](https://github.com/discord/cros-libva/tree/discord-0.0.13)
and
[discord/cros-codecs @ discord-0.0.5](https://github.com/discord/cros-codecs/tree/discord-0.0.5)
— both BSD-3-Clause, themselves forks of Intel's
[cros-libva](https://github.com/intel/cros-libva) (ChromiumOS).

It exists to give [moq](https://github.com/moq-dev/moq)'s `moq-video` a thin,
crates.io-publishable VA-API codec brick instead of a multi-backend framework
pulled in as a git dependency.

## What it does

- **H.264 and H.265 encode** over VA-API: an Annex-B elementary stream out
  (packed parameter sets, low-latency IPPP, CBR rate control whose bitrate can
  change between frames, the color space labelled in the VUI). H.265 is Main
  profile with its VPS, SPS, PPS, and slice headers written here, its coding
  tools following what the driver reports, and its coded size following the
  driver's surface alignment, so the crop holds on radeonsi, which rewrites the
  SPS at that alignment. Three inputs:
  tightly-packed NV12 from the CPU, an NV12 DMA-BUF at the encoder's size
  encoded in place, and any other importable DMA-BUF (packed RGB, YUYV, NV12
  at another size or in another color space) converted and scaled into the
  encoder's surface on the GPU. NV12 and BGRX input are tested; the other
  formats go through the same video processor call untested.
  Checked on Intel Meteor Lake (iHD 26.1.5) by decoding with ffmpeg: luma
  above 35 dB PSNR against the source, and RGB converted to BT.601 and BT.709
  to within a few code values. These run as tests, for both codecs, wherever
  ffmpeg and a VA-API device are both present. H.265 passes them on Intel
  Arrow Lake (iHD 26.1.2). On AMD Radeon RX 9070 (radeonsi, Mesa 26.0.8) it
  passes all but the color conversion and bitrate checks: chroma converted by
  the video processor lands about 7 code values off, for either codec, and
  radeonsi's rate control runs about 4x over 250 kbps on per-frame noise. H.264 encode on that card does not yet
  produce a decodable stream.
- **DMA-BUF import and video post-processing.** `dmabuf` wraps a DRM PRIME
  buffer as a surface without copying it; `vpp` blits one surface into another
  through `VAEntrypointVideoProc`, scaling, converting, and re-tiling on the
  way.
- **H.264 decode** over VA-API: one Annex-B access unit in, tightly-packed NV12
  out, with in-stream parameter sets, a conformant DPB (reference marking,
  reordering, frame_num gaps), and mid-stream resolution changes. Progressive
  8-bit 4:2:0 only, with right/bottom cropping supported and left/top cropping
  rejected. Verified bit-exact against ffmpeg's software decoder on Intel
  Meteor Lake (iHD 26.1.5) for constrained baseline, main with B-frames, high at
  720p, and a cropped non-macroblock-aligned size. The main-with-B-frames case
  runs as a test wherever ffmpeg and a VA-API device are both present.
- **Pinned, vendored headers.** libva's headers are vendored into
  [`libva/`](./libva) (see `just vendor`) and fed to bindgen, so generating the
  bindings needs **no system libva-dev** — only `libclang` — and the pinned
  version means a system libva bump can't drift the bindings.
- **Loads system libva at runtime on Linux.** The generated `Va` function table
  loads `libva` / `libva-drm` with `libloading`. Building needs no system libva,
  and a binary can start without it; opening a display reports a missing driver
  or library. The crate also compiles on macOS for development.

## Upstream

Last checked **2026-10-31**, starting from the Discord vendor baselines:

| Source | Vendored baseline | ChromeOS default branch checked |
| --- | --- | --- |
| cros-libva | [`discord-0.0.13`, `896a248e`](https://github.com/discord/cros-libva/commit/896a248e003caae7a7120812f7eaa423c93a920d) | [`59384456ac2ae78c0c3e5515f41ef1efd9b802cf`](https://github.com/chromeos/cros-libva/commit/59384456ac2ae78c0c3e5515f41ef1efd9b802cf) |
| cros-codecs | [`discord-0.0.5`, `9cdecb19`](https://github.com/discord/cros-codecs/commit/9cdecb192ee3682950d02bd80a7f106163531b76) | [`a050c6cde530e563197ee9e3e72ab01f48a0c41f`](https://github.com/chromeos/cros-codecs/commit/a050c6cde530e563197ee9e3e72ab01f48a0c41f) |

The forks diverge, so compare commits reachable from the checked head but not
the baseline, rather than replacing Discord's additions with the upstream tree.
This run checked all five cros-libva commits and the one cros-codecs commit:

| Commit | Disposition |
| --- | --- |
| cros-libva [`838808ce`](https://github.com/chromeos/cros-libva/commit/838808cebfeff64f5c3543c873dbc604857b3702), protected-content headers | Not needed: no protected-content consumer or Android build in this crate. |
| cros-libva [`89454d66`](https://github.com/chromeos/cros-libva/commit/89454d660411627a4a463116fdea00ac1ac888bc), explicit image lifetimes | Already present in `Image::create_from` and `Picture::{derive_image,create_image}`. |
| cros-libva [`9f3c0d92`](https://github.com/chromeos/cros-libva/commit/9f3c0d9264ec04b4fa792764178b879341ce33b1), packed-header bindings | Already present in `bindgen_gen.rs`, `buffer.rs`, and the H.264 encoder. |
| cros-libva [`ed9c63b7`](https://github.com/chromeos/cros-libva/commit/ed9c63b743ce9490e1079a8b0c1a24b8e879324f), borrowed input buffers | Optional copy avoidance, not a correctness fix. Keep the existing owned-buffer API; no new public API or optimization without measurement. |
| cros-libva [`59384456`](https://github.com/chromeos/cros-libva/commit/59384456ac2ae78c0c3e5515f41ef1efd9b802cf), libva 2.23 VP9 fields | Build compatibility already covered by `..Default::default()` in the retained VP9 wrapper against pinned 2.23 headers. The codec drivers support H.264 only, so no VP9 segmentation control is needed. |
| cros-codecs [`a050c6cd`](https://github.com/chromeos/cros-codecs/commit/a050c6cde530e563197ee9e3e72ab01f48a0c41f), release log filtering | Ported: remove `log/release_max_level_debug`, which conflicts with a consumer selecting another release level. `just ci` checks feature unification with `log/release_max_level_trace`. |

GitHub's published security-advisory endpoints for
[chromeos/cros-libva](https://github.com/chromeos/cros-libva/security/advisories),
[chromeos/cros-codecs](https://github.com/chromeos/cros-codecs/security/advisories),
[discord/cros-libva](https://github.com/discord/cros-libva/security/advisories), and
[discord/cros-codecs](https://github.com/discord/cros-codecs/security/advisories)
all returned empty lists on this date. GitHub's global Rust advisory queries for
`cros-libva` and `cros-codecs` also returned none. This records the published
advisories checked, not a guarantee that no undisclosed vulnerabilities exist.

## Layout

- `src/` — libva bindings (`bindings`, `display`, `surface`, `buffer`, ...), the
  H.264 bitstream layer (`bitstream_utils`, `codec::h264`), the H.265 header
  writer (`encode/h265.rs`), the thin encode
  and decode drivers (`encode`, `decode`), DMA-BUF import (`dmabuf`), video
  post-processing (`vpp`), and the color space both share (`color`).
- `libva/` — vendored libva headers (checked in; refreshed by `just vendor`).
- `build.rs` + `bindgen_gen.rs` + `libva-wrapper.h` — bindgen setup.

## Development

Recipes run inside the nix devShell:

```sh
nix develop --command just check     # clippy + fmt
nix develop --command just ci        # check + cargo-deny
nix develop --command just vendor    # refresh libva/ headers from upstream
```

The crate compiles on macOS for development, with no link-time dependency on
libva.

## Licensing

BSD-3-Clause (see [`LICENSE`](./LICENSE)). Vendored upstreams keep their notices:
[`LICENSE.cros-codecs`](./LICENSE.cros-codecs) and the vendored libva headers
under [`LICENSE.libva`](./LICENSE.libva) (MIT).
