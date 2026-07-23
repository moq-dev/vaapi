// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module implements the bindgen C FFI bindings for use within this crate

#![allow(missing_docs)]
#![allow(clippy::useless_transmute)]
#![allow(clippy::too_many_arguments)]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
#![allow(dead_code)]
// The bindings are generated from vendored C headers; their doc comments carry
// bare URLs and other rustdoc-unfriendly markup. Don't lint generated docs.
#![allow(rustdoc::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use std::sync::OnceLock;

/// The dlopen'd libva symbol table, resolved once on first use. The result is
/// cached (success or failure) so a missing libva is probed at most once.
static VA: OnceLock<Result<Va, String>> = OnceLock::new();

/// Loads libva (via `libva-drm.so.2`) on first call and returns the symbol table.
///
/// `libva-drm.so.2` is linked against `libva.so.2`, so a single dlopen exposes
/// both `vaGetDisplayDRM` and the core `va*` symbols (dlsym on the handle searches
/// its dependencies). Returns `Err` if the library is absent, so a caller on a
/// libva-less host can fall back to another encoder instead of aborting.
pub(crate) fn load() -> Result<&'static Va, &'static str> {
	VA.get_or_init(|| unsafe { Va::new("libva-drm.so.2") }.map_err(|e| e.to_string()))
		.as_ref()
		.map_err(String::as_str)
}

/// Accessor for the loaded libva symbols.
///
/// Panics if libva has not loaded. Every entry point that reaches libva goes
/// through a [`crate::Display`], whose constructor calls [`load`] and bails out on
/// failure, so by the time any other `va*` call runs the library is present.
pub(crate) fn va() -> &'static Va {
	load().expect("libva not loaded; open a Display first")
}
