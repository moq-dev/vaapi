// Vendored from discord/cros-codecs (BSD-3-Clause), trimmed to H.264.
pub mod h264;

/// A video coding format the hardware encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
	/// H.264 (AVC), Main profile.
	H264,
	/// H.265 (HEVC), Main profile.
	H265,
}
