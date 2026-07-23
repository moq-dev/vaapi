// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// The allow list of VA functions, structures and enum values.
const ALLOW_LIST_TYPE : &str = ".*ExternalBuffers.*|.*PRIME.*|.*MPEG2.*|.*VP8.*|.*VP9.*|.*H264.*|.*HEVC.*|.*JPEG.*|VACodedBufferSegment|.*AV1.*|VAEncMisc.*|VASurfaceDecodeMBErrors|VADecodeErrorType|.*VAProc.*|VAEncPacked.*";

// The common bindgen builder for VA-API.
pub fn vaapi_gen_builder(builder: bindgen::Builder) -> bindgen::Builder {
	builder
		.derive_default(true)
		.derive_eq(true)
		.layout_tests(false)
		.constified_enum_module("VA.*")
		.allowlist_var("VA.*")
		.allowlist_function("va.*")
		.allowlist_type(ALLOW_LIST_TYPE)
		// Emit a `Va` struct of libloading-resolved function pointers instead of
		// link-time `extern "C"` decls, so libva is dlopen'd at runtime (no
		// build-time or load-time libva dependency). `require_all(false)` lets a
		// system libva older than the vendored headers still load: a missing
		// symbol only errors if actually called, not at `Va::new`.
		.dynamic_library_name("Va")
		.dynamic_link_require_all(false)
}
