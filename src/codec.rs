// Vendored from discord/cros-codecs (BSD-3-Clause), trimmed to H.264.
pub mod h264;

/// A video coding format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
	/// H.264 (AVC).
	H264,
	/// H.265 (HEVC).
	H265,
}
