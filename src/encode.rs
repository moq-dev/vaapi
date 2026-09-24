// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// Adapted from discord/cros-codecs (encoder/stateless/h264/vaapi.rs), itself
// BSD-3-Clause / Copyright The ChromiumOS Authors. See LICENSE.cros-codecs.

//! Thin VA-API H.264 encoder built on the vendored libva binding.
//!
//! Reuses the backend-agnostic bitstream layer ([`crate::codec::h264`]) from
//! discord/cros-codecs (BSD-3-Clause) for SPS/PPS/slice synthesis, and drives
//! libva directly for surface upload + slice submission, rather than vendoring
//! cros-codecs's generic multi-backend encoder framework. The per-frame VA
//! buffer population is ported from cros-codecs's `encoder/stateless/h264/vaapi.rs`.
//!
//! Low-latency only: IPPP (no B-frames), one reference frame, matching the
//! VideoToolbox / Media Foundation / NVENC backends in moq-video.
//!
//! Three ways in, cheapest first:
//! - [`Encoder::encode_dmabuf`] with an NV12 DMA-BUF at the encoder's size
//!   encodes the producer's memory in place.
//! - [`Encoder::encode_dmabuf`] with any other importable DMA-BUF (packed RGB
//!   from a screen capture, YUYV from a camera, NV12 at another size) converts
//!   and scales it into the encoder's own surface with the video processor, on
//!   the GPU.
//! - [`Encoder::encode_nv12`] uploads CPU pixels.
//!
//! Checked on Intel Meteor Lake with the iHD driver by decoding with ffmpeg's
//! software decoder: CPU and DMA-BUF input come back above 35 dB luma PSNR,
//! and BGRX converts to BT.601 and BT.709 with every channel within 6 code
//! values of the reference. The low-power entrypoint is untested, since that
//! device exposes the full one.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::codec::h264::parser::SpsBuilder;
use crate::codec::h264::parser::{Level, Pps, PpsBuilder, Profile, SliceHeader, SliceHeaderBuilder, SliceType, Sps};
use crate::codec::h264::synthesizer::Synthesizer;
use crate::dmabuf::DmaBuf;
use crate::vpp::Processor;
use crate::{
	BufferType, Color, Config as VaConfig, Context, Display, EncCodedBuffer, EncMiscParameter,
	EncMiscParameterFrameRate, EncMiscParameterRateControl, EncPackedHeaderParameter, EncPackedHeaderType,
	EncPictureParameter, EncPictureParameterBufferH264, EncSequenceParameter, EncSequenceParameterBufferH264,
	EncSliceParameter, EncSliceParameterBufferH264, H264EncFrameCropOffsets, H264EncPicFields, H264EncSeqFields,
	H264VuiFields, Image, MappedCodedBuffer, Picture, PictureH264, RcFlags, Surface, SurfaceMemoryDescriptor,
	UsageHint, VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAProfile, VA_ATTRIB_NOT_SUPPORTED,
	VA_ENC_PACKED_HEADER_PICTURE, VA_ENC_PACKED_HEADER_SEQUENCE, VA_FOURCC_ARGB, VA_FOURCC_BGRA, VA_FOURCC_BGRX,
	VA_FOURCC_NV12, VA_FOURCC_RGBA, VA_FOURCC_RGBX, VA_FOURCC_XRGB, VA_INVALID_ID, VA_PICTURE_H264_INVALID,
	VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RC_CBR, VA_RT_FORMAT_YUV420,
};

/// Whether a frame is used as a reference, and for how long.
/// (Vendored from discord/cros-codecs, BSD-3-Clause; used by the slice synthesizer.)
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IsReference {
	No,
	ShortTerm,
	LongTerm,
}

/// Quantizer bounds carried over from the cros-codecs encoder.
///
/// The full H.264 range lets Intel's CBR controller spend too much of a GOP on
/// its IDR and then raise the P-frame quantizer enough to produce a visible
/// quality pulse. These bounds trade strict CBR at the extremes for stable
/// picture quality.
const MIN_QP: u8 = 18;
const MAX_QP: u8 = 36;
/// Window over which libva's rate controller targets the configured bitrate.
///
/// This matches cros-codecs. The old `framerate * 1000` value confused frames
/// per second with seconds and gave a 30 fps encoder a 30-second window.
pub const RATE_CONTROL_WINDOW_MS: u32 = 1_500;
/// `max_frame_num` upper bound for the low-delay GOP; matches cros-codecs.
const LIMIT: u32 = 2048;

/// H.264 encoder configuration. Dimensions must be even (4:2:0 chroma).
#[derive(Clone, Debug)]
pub struct Config {
	pub width: u32,
	pub height: u32,
	pub framerate: u32,
	/// Target bitrate in bits per second (CBR).
	pub bitrate: u32,
	/// Keyframe interval in frames.
	pub gop: u32,
	/// DRM render node to open (e.g. `/dev/dri/renderD128`).
	///
	/// `None`, the default, opens the first node whose driver can encode H.264,
	/// and [`Encoder::config`] then names the node it picked.
	pub device: Option<PathBuf>,
	/// Color space written into the SPS, and the target of a conversion from RGB.
	///
	/// Defaults to [`Color::infer`] for the height, which is what a decoder
	/// assumes for an unlabelled stream.
	pub color: Color,
}

impl Config {
	pub fn new(width: u32, height: u32, framerate: u32, bitrate: u32, gop: u32) -> Self {
		Self {
			width,
			height,
			framerate,
			bitrate,
			gop,
			device: None,
			color: Color::infer(height),
		}
	}
}

/// Per-frame reference metadata. (Mirrors cros-codecs `DpbEntryMeta`.)
#[derive(Copy, Clone)]
struct FrameMeta {
	poc: u32,
	frame_num: u32,
}

/// A VA-API H.264 encoder. Built once, fed NV12 frames, emits Annex-B H.264.
pub struct Encoder {
	config: Config,
	sps: Rc<Sps>,
	pps: Rc<Pps>,
	width_mbs: u16,
	height_mbs: u16,

	// Source surface (reused; NV12 re-uploaded per frame) and a 2-deep
	// reconstructed surface ring (current + previous reference for IPPP).
	input: Option<Surface<()>>,
	recon: Vec<Surface<()>>,
	/// Reference from the previous encoded frame: (recon index, metadata).
	reference: Option<(usize, FrameMeta)>,

	counter: u32,

	/// The video processor converting and scaling DMA-BUF input, opened on the
	/// first frame that needs it.
	processor: Option<Processor>,
	/// Set by a bitrate change, and cleared by the next frame, which asks the
	/// driver to restart its rate controller at the new target.
	reset_rate_control: bool,

	// Drop order: encode resources before the display they belong to.
	coded: EncCodedBuffer,
	coded_size: usize,
	context: Rc<Context>,
	_va_config: VaConfig,
	display: Arc<Display>,
}

impl Encoder {
	/// Opens [`Config::device`], or the first render node that encodes H.264, and sets up an encode session for `config`.
	///
	/// # Errors
	///
	/// Fails when libva cannot be loaded, when the named node cannot be opened
	/// or does not encode H.264 (a named node is never swapped for another), when
	/// no node encodes H.264, and when the driver refuses the session.
	pub fn new(mut config: Config) -> anyhow::Result<Self> {
		let (device, display) = match config.device.take() {
			Some(device) => {
				let display = Display::open_drm_display(&device)
					.map_err(|e| anyhow::anyhow!("open DRM display {device:?}: {e:?}"))?;
				(device, display)
			}
			None => {
				crate::display::open_first(probe).map_err(|e| e.context("find a render node that encodes H.264"))?
			}
		};

		let entrypoint = encode_entrypoint(&display)?;
		let mut attrs = vec![
			VAConfigAttrib {
				type_: VAConfigAttribType::VAConfigAttribRTFormat,
				value: VA_RT_FORMAT_YUV420,
			},
			VAConfigAttrib {
				type_: VAConfigAttribType::VAConfigAttribRateControl,
				value: VA_RC_CBR,
			},
		];
		// The SPS and PPS go out as packed headers, so the VUI (color, frame
		// buffering) and the level are ours. iHD takes them without being told;
		// a driver that follows the spec writes its own unless the config asks.
		let packed = packed_headers(&display, entrypoint);
		if packed != 0 {
			attrs.push(VAConfigAttrib {
				type_: VAConfigAttribType::VAConfigAttribEncPackedHeaders,
				value: packed,
			});
		} else {
			log::warn!("the driver takes no packed SPS/PPS; the stream carries the driver's own headers");
		}
		let va_config = display
			.create_config(attrs, VAProfile::VAProfileH264Main, entrypoint)
			.map_err(|e| anyhow::anyhow!("create VA config: {e:?}"))?;

		let context = display
			.create_context::<()>(&va_config, config.width, config.height, None, true)
			.map_err(|e| anyhow::anyhow!("create VA context: {e:?}"))?;

		let coded_size = coded_size(config.bitrate);
		let coded = context
			.create_enc_coded(coded_size)
			.map_err(|e| anyhow::anyhow!("create coded buffer: {e:?}"))?;

		let input = make_surface(&display, &config)?;
		let recon = vec![make_surface(&display, &config)?, make_surface(&display, &config)?];

		let (sps, pps) = build_sps_pps(&config);
		let width_mbs = sps.pic_width_in_mbs_minus1 + 1;
		let height_mbs = sps.pic_height_in_map_units_minus1 + 1;

		log::info!(
			"opened VA-API H.264 encoder on {:?}: {}x{} @ {}fps, {} bps, low power: {}",
			device,
			config.width,
			config.height,
			config.framerate,
			config.bitrate,
			entrypoint == VAEntrypoint::VAEntrypointEncSliceLP,
		);
		config.device = Some(device);
		Ok(Self {
			config,
			sps,
			pps,
			width_mbs,
			height_mbs,
			input: Some(input),
			recon,
			reference: None,
			counter: 0,
			processor: None,
			reset_rate_control: false,
			coded,
			coded_size,
			context,
			_va_config: va_config,
			display,
		})
	}

	/// Encodes one tightly-packed NV12 frame, returning Annex-B H.264 (with inline
	/// SPS/PPS on each IDR). `keyframe` forces an IDR.
	///
	/// # Errors
	///
	/// Fails when `nv12` is shorter than a frame at the encoder's size, and when
	/// the upload or the encode fails.
	pub fn encode_nv12(&mut self, nv12: &[u8], keyframe: bool) -> anyhow::Result<Vec<u8>> {
		let (width, height) = (self.config.width as usize, self.config.height as usize);
		let expected = width * height + width * height.div_ceil(2);
		if nv12.len() < expected {
			anyhow::bail!("NV12 frame is {} bytes, {width}x{height} needs {expected}", nv12.len());
		}
		let input = self.input.take().expect("input surface present");
		let result = upload_nv12(&self.display, &input, self.config.width, self.config.height, nv12)
			.and_then(|()| self.encode_surface(&input, keyframe));
		self.input = Some(input);
		result
	}

	/// Encodes one frame held in a DMA-BUF, returning Annex-B H.264 like
	/// [`encode_nv12`](Self::encode_nv12).
	///
	/// An NV12 buffer at the encoder's size, whose size is a whole number of
	/// macroblocks and whose [`color`](DmaBuf::color) is the stream's or
	/// unknown, is imported and encoded in place. Any other mapped format or
	/// size goes through the video processor into the encoder's own surface
	/// first: scaled to the encoder's size and converted to [`Config::color`].
	/// A YUV buffer with no color of its own is taken to be in that space
	/// already. Nothing reaches the CPU on either path.
	///
	/// The macroblock condition is not a formality: the encoder reads whole
	/// macroblocks, so it reads 1088 rows of a 1080-row picture, and a producer
	/// that places the chroma plane right after the last visible luma row
	/// (PipeWire does) has nothing there to read.
	///
	/// The producer must have finished writing the buffer; this does not wait
	/// on its fence.
	///
	/// # Errors
	///
	/// Fails when the driver refuses to import the buffer (an unsupported
	/// modifier or layout), when the device has no video processor and the
	/// buffer needs one, and when the encode itself fails. The encoder stays
	/// usable after a refused import, so a caller can fall back to
	/// [`encode_nv12`](Self::encode_nv12) for the same frame.
	pub fn encode_dmabuf(&mut self, buffer: DmaBuf, keyframe: bool) -> anyhow::Result<Vec<u8>> {
		let fourcc = buffer
			.fourcc()
			.ok_or_else(|| anyhow::anyhow!("no VA-API format for DRM format {:#010x}", buffer.drm_format))?;
		let color = self.config.color;
		let in_place = fourcc == VA_FOURCC_NV12
			&& (buffer.width, buffer.height) == (self.config.width, self.config.height)
			&& buffer.width % 16 == 0
			&& buffer.height % 16 == 0
			&& buffer.color.is_none_or(|own| own == color);
		if in_place {
			let source = buffer.import(&self.display, Some(UsageHint::USAGE_HINT_ENCODER))?;
			return self.encode_surface(&source, keyframe);
		}

		let input_color = match is_rgb(fourcc) {
			true => None,
			false => Some(buffer.color.unwrap_or(color)),
		};
		let source = buffer.import(&self.display, Some(UsageHint::USAGE_HINT_VPP_READ))?;
		let input = self.input.take().expect("input surface present");
		let result = self
			.processor()
			.and_then(|processor| processor.blit(&source, &input, input_color, Some(color)))
			.and_then(|()| self.encode_surface(&input, keyframe));
		self.input = Some(input);
		result
	}

	/// Changes the target bitrate, in bits per second, from the next frame on.
	///
	/// The rate control parameters go out with every frame, so this neither
	/// forces an IDR nor rebuilds the session; the next frame also asks the
	/// driver to reset its rate controller. It then converges over its window,
	/// [`RATE_CONTROL_WINDOW_MS`].
	///
	/// The level in the SPS is not revisited, so a rate far above the one the
	/// stream opened with can exceed what that level allows.
	///
	/// # Errors
	///
	/// Fails for a bitrate of zero, and when a larger coded buffer for the new
	/// rate cannot be allocated, in which case the old rate stays.
	pub fn set_bitrate(&mut self, bitrate: u32) -> anyhow::Result<()> {
		if bitrate == 0 {
			anyhow::bail!("a bitrate of zero");
		}
		let size = coded_size(bitrate);
		if size > self.coded_size {
			// A higher rate can produce a picture larger than the buffer sized for
			// the old one, and a truncated picture is corrupt rather than short.
			self.coded = self
				.context
				.create_enc_coded(size)
				.map_err(|e| anyhow::anyhow!("create coded buffer: {e:?}"))?;
			self.coded_size = size;
		}
		if bitrate != self.config.bitrate {
			self.reset_rate_control = true;
		}
		self.config.bitrate = bitrate;
		Ok(())
	}

	/// Returns the configuration in effect, including a bitrate changed since opening and the render node picked when none was named.
	pub fn config(&self) -> &Config {
		&self.config
	}

	/// Returns the video processor, opening it on first use.
	fn processor(&mut self) -> anyhow::Result<&Processor> {
		if self.processor.is_none() {
			self.processor = Some(Processor::with_display(Arc::clone(&self.display))?);
		}
		Ok(self.processor.as_ref().expect("opened above"))
	}

	/// Encodes the picture in `source`, which must be NV12 at the encoder's size.
	fn encode_surface<D: SurfaceMemoryDescriptor>(
		&mut self,
		source: &Surface<D>,
		keyframe: bool,
	) -> anyhow::Result<Vec<u8>> {
		let is_idr = keyframe || self.counter % self.config.gop == 0;
		if is_idr {
			// IDR resets the H.264 frame numbering and the reference list.
			self.counter = 0;
			self.reference = None;
		}

		// frame_num counts modulo MaxFrameNum, and the slice header carries only
		// the low bits of the picture order count; the full count goes to the
		// driver in the picture parameters.
		let meta = FrameMeta {
			poc: self.counter.wrapping_mul(2),
			frame_num: self.counter % LIMIT,
		};
		let recon_idx = (self.counter as usize) % self.recon.len();

		let slice_type = if is_idr { SliceType::I } else { SliceType::P };
		let header = SliceHeaderBuilder::new(&self.pps)
			.slice_type(slice_type)
			.first_mb_in_slice(0)
			.frame_num(meta.frame_num as u16)
			.pic_order_cnt_lsb((meta.poc % (LIMIT * 2)) as u16)
			.build();

		let num_macroblocks = self.width_mbs as u32 * self.height_mbs as u32;
		let bits_per_second = self.config.bitrate;

		// Reference (previous reconstructed frame) for P slices: surface id + meta.
		let reference = match (is_idr, self.reference) {
			(false, Some((ref_idx, ref_meta))) => Some((self.recon[ref_idx].id(), ref_meta)),
			_ => None,
		};

		let seq_param = build_enc_seq_param(&self.sps, bits_per_second, LIMIT, 0);
		let pic_param = build_enc_pic_param(
			&self.pps,
			&self.coded,
			self.recon[recon_idx].id(),
			meta,
			is_idr,
			reference,
		);
		let slice_param = build_enc_slice_param(&self.pps, &header, reference, num_macroblocks);

		let mut picture = Picture::new(meta.frame_num as u64, Rc::clone(&self.context), source);

		// VA-API spec buffer order: sequence, picture, slice, then packed headers,
		// then rate-control misc params.
		picture.add_buffer(self.create(seq_param)?);
		picture.add_buffer(self.create(pic_param)?);
		picture.add_buffer(self.create(slice_param)?);

		if is_idr {
			let (sps_param, sps_data) = packed_header(EncPackedHeaderType::Sequence, &self.packed_sps()?);
			let (pps_param, pps_data) = packed_header(EncPackedHeaderType::Picture, &self.packed_pps()?);
			picture.add_buffer(self.create(sps_param)?);
			picture.add_buffer(self.create(sps_data)?);
			picture.add_buffer(self.create(pps_param)?);
			picture.add_buffer(self.create(pps_data)?);
		}

		let rc = EncMiscParameterRateControl::new(
			bits_per_second,
			100,                    // target_percentage (CBR)
			RATE_CONTROL_WINDOW_MS, // window_size (ms)
			u32::from((MIN_QP + MAX_QP) / 2),
			MIN_QP as u32,
			0, // basic_unit_size
			// Do not let rate control drop a frame to meet the bitrate. A live
			// encoder has already admitted this frame into the media timeline.
			RcFlags::new(u32::from(self.reset_rate_control), 1, 0, 0, 0, 0, 0, 0, 0),
			0, // icq_quality_factor
			MAX_QP as u32,
			0, // quality_factor
			0, // target_frame_size
		);
		picture.add_buffer(self.create(BufferType::EncMiscParameter(EncMiscParameter::RateControl(rc)))?);

		let framerate = EncMiscParameterFrameRate::new(self.config.framerate, 0);
		picture.add_buffer(self.create(BufferType::EncMiscParameter(EncMiscParameter::FrameRate(framerate)))?);

		let picture = picture.begin().map_err(|e| anyhow::anyhow!("picture begin: {e:?}"))?;
		let picture = picture.render().map_err(|e| anyhow::anyhow!("picture render: {e:?}"))?;
		let picture = picture.end().map_err(|e| anyhow::anyhow!("picture end: {e:?}"))?;
		// Sync (PictureEnd -> PictureSync) so the source is no longer read and
		// the coded buffer is ready.
		picture
			.sync()
			.map_err(|(e, _)| anyhow::anyhow!("picture sync: {e:?}"))?;

		// The reconstructed surface syncs implicitly; read the coded bitstream.
		let bitstream = self.read_coded()?;

		self.reference = Some((recon_idx, meta));
		self.counter += 1;
		self.reset_rate_control = false;
		Ok(bitstream)
	}

	fn create(&self, buffer: BufferType) -> anyhow::Result<crate::Buffer> {
		self.context
			.create_buffer(buffer)
			.map_err(|e| anyhow::anyhow!("create VA buffer: {e:?}"))
	}

	fn packed_sps(&self) -> anyhow::Result<Vec<u8>> {
		let mut buf = Vec::new();
		Synthesizer::<'_, Sps, _>::synthesize(3, &self.sps, &mut buf, true)
			.map_err(|e| anyhow::anyhow!("synthesize SPS: {e:?}"))?;
		Ok(buf)
	}

	fn packed_pps(&self) -> anyhow::Result<Vec<u8>> {
		let mut buf = Vec::new();
		Synthesizer::<'_, Pps, _>::synthesize(3, &self.pps, &mut buf, true)
			.map_err(|e| anyhow::anyhow!("synthesize PPS: {e:?}"))?;
		Ok(buf)
	}

	fn read_coded(&self) -> anyhow::Result<Vec<u8>> {
		let mapped = MappedCodedBuffer::new(&self.coded).map_err(|e| anyhow::anyhow!("map coded buffer: {e:?}"))?;
		let mut out = Vec::new();
		for segment in mapped.iter() {
			out.extend_from_slice(segment.buf);
		}
		Ok(out)
	}
}

/// Checks that `display`'s driver can encode H.264, which is what [`Encoder::new`] needs of a render node.
///
/// # Errors
///
/// Fails when the driver offers neither the full nor the low-power H.264 Main
/// encode entrypoint, or cannot be asked.
pub fn probe(display: &Display) -> anyhow::Result<()> {
	encode_entrypoint(display).map(drop)
}

/// Returns the entrypoint to encode H.264 Main with: the full one, or the low-power one where it is all there is.
///
/// Some Intel parts expose H.264 encode only through `VAEntrypointEncSliceLP`.
fn encode_entrypoint(display: &Display) -> anyhow::Result<VAEntrypoint::Type> {
	let entrypoints = display
		.query_config_entrypoints(VAProfile::VAProfileH264Main)
		.map_err(|e| anyhow::anyhow!("query H.264 Main entrypoints: {e:?}"))?;
	pick_entrypoint(&entrypoints).ok_or_else(|| anyhow::anyhow!("the device has no H.264 Main encode entrypoint"))
}

/// Returns the encode entrypoint to use out of those a device offers: the full one when present.
fn pick_entrypoint(offered: &[VAEntrypoint::Type]) -> Option<VAEntrypoint::Type> {
	[VAEntrypoint::VAEntrypointEncSlice, VAEntrypoint::VAEntrypointEncSliceLP]
		.into_iter()
		.find(|entrypoint| offered.contains(entrypoint))
}

/// Returns the packed header kinds to ask for: sequence and picture, where the driver takes them.
///
/// Zero when the driver takes neither, or when asking fails.
fn packed_headers(display: &Display, entrypoint: VAEntrypoint::Type) -> u32 {
	let mut attrs = [VAConfigAttrib {
		type_: VAConfigAttribType::VAConfigAttribEncPackedHeaders,
		value: 0,
	}];
	if display
		.get_config_attributes(VAProfile::VAProfileH264Main, entrypoint, &mut attrs)
		.is_err()
		|| attrs[0].value & VA_ATTRIB_NOT_SUPPORTED != 0
	{
		return 0;
	}
	attrs[0].value & (VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE)
}

/// Returns the lowest H.264 level, from 4 up, whose frame size and macroblock rate cover the stream.
///
/// Only those two limits: the level's maximum bitrate is not checked.
///
/// Level 4 is the floor because it was the fixed level before, and decoders
/// take the DPB size from `max_dec_frame_buffering` rather than from the level,
/// so a lower one would buy nothing. Above 1080p (a 1440p or 4K screen share) a
/// fixed level 4 would declare a stream its decoder may refuse.
fn level(width: u32, height: u32, framerate: u32) -> Level {
	// H.264 Table A-1: (level, MaxMBPS, MaxFS).
	const LEVELS: [(Level, u64, u64); 8] = [
		(Level::L4, 245_760, 8_192),
		(Level::L4_2, 522_240, 8_704),
		(Level::L5, 589_824, 22_080),
		(Level::L5_1, 983_040, 36_864),
		(Level::L5_2, 2_073_600, 36_864),
		(Level::L6, 4_177_920, 139_264),
		(Level::L6_1, 8_355_840, 139_264),
		(Level::L6_2, 16_711_680, 139_264),
	];
	let frame = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
	let rate = frame * u64::from(framerate.max(1));
	LEVELS
		.into_iter()
		.find(|&(_, max_rate, max_frame)| frame <= max_frame && rate <= max_rate)
		.map(|(level, _, _)| level)
		.unwrap_or(Level::L6_2)
}

/// Returns the coded buffer size in bytes for `bitrate`: two seconds of it, and at least 1.5 MB.
fn coded_size(bitrate: u32) -> usize {
	(bitrate as usize / 4).max(1_500_000)
}

/// Returns whether the VA fourcc `fourcc` is packed RGB, which has no YUV matrix of its own.
fn is_rgb(fourcc: u32) -> bool {
	matches!(
		fourcc,
		VA_FOURCC_BGRX | VA_FOURCC_BGRA | VA_FOURCC_RGBX | VA_FOURCC_RGBA | VA_FOURCC_XRGB | VA_FOURCC_ARGB
	)
}

/// Allocate one NV12 encode surface.
fn make_surface(display: &Arc<Display>, config: &Config) -> anyhow::Result<Surface<()>> {
	let mut surfaces = display
		.create_surfaces::<()>(
			VA_RT_FORMAT_YUV420,
			Some(VA_FOURCC_NV12),
			config.width,
			config.height,
			Some(UsageHint::USAGE_HINT_ENCODER),
			vec![()],
		)
		.map_err(|e| anyhow::anyhow!("create surface: {e:?}"))?;
	surfaces.pop().ok_or_else(|| anyhow::anyhow!("no surface created"))
}

/// Upload tightly-packed NV12 into a surface. Ported from cros-codecs
/// `upload_nv12_img` (honors the image's per-plane offsets + pitches).
fn upload_nv12(
	display: &Arc<Display>,
	surface: &Surface<()>,
	width: u32,
	height: u32,
	data: &[u8],
) -> anyhow::Result<()> {
	let formats = display
		.query_image_formats()
		.map_err(|e| anyhow::anyhow!("query image formats: {e:?}"))?;
	let format = formats
		.into_iter()
		.find(|f| f.fourcc == VA_FOURCC_NV12)
		.ok_or_else(|| anyhow::anyhow!("driver has no NV12 image format"))?;

	let mut image = Image::create_from(surface, format, surface.size(), surface.size())
		.map_err(|e| anyhow::anyhow!("create image: {e:?}"))?;
	let va_image = *image.image();
	let dest: &mut [u8] = image.as_mut();
	let (w, h) = (width as usize, height as usize);

	// Luma plane.
	let mut src = data;
	let mut dst = &mut dest[va_image.offsets[0] as usize..];
	for _ in 0..h {
		dst[..w].copy_from_slice(&src[..w]);
		dst = &mut dst[va_image.pitches[0] as usize..];
		src = &src[w..];
	}
	// Interleaved chroma plane (h/2 rows of w bytes).
	let mut src = &data[w * h..];
	let mut dst = &mut dest[va_image.offsets[1] as usize..];
	for _ in 0..h / 2 {
		dst[..w].copy_from_slice(&src[..w]);
		dst = &mut dst[va_image.pitches[1] as usize..];
		src = &src[w..];
	}

	drop(image);
	surface.sync().map_err(|e| anyhow::anyhow!("surface sync: {e:?}"))?;
	Ok(())
}

/// Build the SPS/PPS for a low-latency IPPP stream. Ported from
/// discord/cros-codecs `LowDelayH264Delegate::new_sequence` (BSD-3-Clause).
fn build_sps_pps(config: &Config) -> (Rc<Sps>, Rc<Pps>) {
	let level = level(config.width, config.height, config.framerate);
	let (primaries, transfer, matrix) = config.color.vui();
	let sps = SpsBuilder::new()
		.seq_parameter_set_id(0)
		.profile_idc(Profile::Main)
		.chroma_format_idc(1)
		.level_idc(level)
		.max_frame_num(LIMIT)
		.pic_order_cnt_type(0)
		.max_pic_order_cnt_lsb(LIMIT * 2)
		.max_num_ref_frames(1)
		.frame_mbs_only_flag(true)
		.direct_8x8_inference_flag(level >= Level::L3)
		.resolution(config.width, config.height)
		.bit_depth_luma(8)
		.bit_depth_chroma(8)
		.aspect_ratio(1, 1)
		.timing_info(1, config.framerate * 2, false)
		.video_signal_type(config.color.full_range, primaries, transfer, matrix)
		.max_num_reorder_frames(0)
		.max_dec_frame_buffering(1)
		.build();

	let pps = PpsBuilder::new(Rc::clone(&sps))
		.pic_parameter_set_id(0)
		.pic_init_qp(26)
		.entropy_coding_mode_flag(true)
		.transform_8x8_mode_flag(false)
		.deblocking_filter_control_present_flag(true)
		.num_ref_idx_l0_default_active(1)
		.num_ref_idx_l1_default_active_minus1(0)
		.build();

	(sps, pps)
}

fn build_invalid_pic() -> PictureH264 {
	PictureH264::new(VA_INVALID_ID, 0, VA_PICTURE_H264_INVALID, 0, 0)
}

fn build_h264_pic(surface_id: u32, meta: FrameMeta) -> PictureH264 {
	PictureH264::new(
		surface_id,
		meta.frame_num,
		VA_PICTURE_H264_SHORT_TERM_REFERENCE,
		meta.poc as i32,
		meta.poc as i32,
	)
}

/// Ported from cros-codecs `build_enc_seq_param` (BSD-3-Clause).
fn build_enc_seq_param(sps: &Sps, bits_per_second: u32, intra_period: u32, ip_period: u32) -> BufferType {
	let seq_fields = H264EncSeqFields::new(
		sps.chroma_format_idc as u32,
		sps.frame_mbs_only_flag as u32,
		sps.mb_adaptive_frame_field_flag as u32,
		sps.seq_scaling_matrix_present_flag as u32,
		sps.direct_8x8_inference_flag as u32,
		sps.log2_max_frame_num_minus4 as u32,
		sps.pic_order_cnt_type as u32,
		sps.log2_max_pic_order_cnt_lsb_minus4 as u32,
		sps.delta_pic_order_always_zero_flag as u32,
	);

	let frame_crop = if sps.frame_cropping_flag {
		Some(H264EncFrameCropOffsets::new(
			sps.frame_crop_left_offset,
			sps.frame_crop_right_offset,
			sps.frame_crop_top_offset,
			sps.frame_crop_bottom_offset,
		))
	} else {
		None
	};

	let vui_fields = if sps.vui_parameters_present_flag {
		Some(H264VuiFields::new(
			sps.vui_parameters.aspect_ratio_idc as u32,
			sps.vui_parameters.timing_info_present_flag as u32,
			sps.vui_parameters.bitstream_restriction_flag as u32,
			sps.vui_parameters.log2_max_mv_length_horizontal,
			sps.vui_parameters.log2_max_mv_length_vertical,
			sps.vui_parameters.fixed_frame_rate_flag as u32,
			sps.vui_parameters.low_delay_hrd_flag as u32,
			sps.vui_parameters.motion_vectors_over_pic_boundaries_flag as u32,
		))
	} else {
		None
	};

	let mut offset_for_ref_frame = [0i32; 256];
	offset_for_ref_frame[..255].copy_from_slice(&sps.offset_for_ref_frame[..]);

	BufferType::EncSequenceParameter(EncSequenceParameter::H264(EncSequenceParameterBufferH264::new(
		sps.seq_parameter_set_id,
		sps.level_idc as u8,
		intra_period,
		intra_period,
		ip_period,
		bits_per_second,
		sps.max_num_ref_frames as u32,
		sps.pic_width_in_mbs_minus1 + 1,
		sps.pic_height_in_map_units_minus1 + 1,
		&seq_fields,
		sps.bit_depth_luma_minus8,
		sps.bit_depth_chroma_minus8,
		sps.num_ref_frames_in_pic_order_cnt_cycle,
		sps.offset_for_non_ref_pic,
		sps.offset_for_top_to_bottom_field,
		offset_for_ref_frame,
		frame_crop,
		vui_fields,
		sps.vui_parameters.aspect_ratio_idc,
		sps.vui_parameters.sar_width as u32,
		sps.vui_parameters.sar_height as u32,
		sps.vui_parameters.num_units_in_tick,
		sps.vui_parameters.time_scale,
	)))
}

/// Ported from cros-codecs `build_enc_pic_param` (BSD-3-Clause).
fn build_enc_pic_param(
	pps: &Pps,
	coded: &EncCodedBuffer,
	recon_id: u32,
	meta: FrameMeta,
	is_idr: bool,
	reference: Option<(u32, FrameMeta)>,
) -> BufferType {
	let pic_fields = H264EncPicFields::new(
		is_idr as u32,
		1, // reference_pic_flag: this frame is a short-term reference
		pps.entropy_coding_mode_flag as u32,
		pps.weighted_pred_flag as u32,
		pps.weighted_bipred_idc as u32,
		pps.constrained_intra_pred_flag as u32,
		pps.transform_8x8_mode_flag as u32,
		pps.deblocking_filter_control_present_flag as u32,
		pps.redundant_pic_cnt_present_flag as u32,
		0,
		pps.pic_scaling_matrix_present_flag as u32,
	);

	let curr_pic = build_h264_pic(recon_id, meta);
	let mut reference_frames: [PictureH264; 16] = std::array::from_fn(|_| build_invalid_pic());
	if let Some((id, m)) = reference {
		reference_frames[0] = build_h264_pic(id, m);
	}

	BufferType::EncPictureParameter(EncPictureParameter::H264(EncPictureParameterBufferH264::new(
		curr_pic,
		reference_frames,
		coded.id(),
		pps.pic_parameter_set_id,
		pps.seq_parameter_set_id,
		0,
		meta.frame_num as u16,
		(pps.pic_init_qp_minus26 + 26) as u8,
		pps.num_ref_idx_l0_default_active_minus1,
		pps.num_ref_idx_l1_default_active_minus1,
		pps.chroma_qp_index_offset,
		pps.second_chroma_qp_index_offset,
		&pic_fields,
	)))
}

/// Ported from cros-codecs `build_enc_slice_param` (BSD-3-Clause), simplified for
/// IPPP (no weighted prediction, single reference in list 0).
fn build_enc_slice_param(
	pps: &Pps,
	header: &SliceHeader,
	reference: Option<(u32, FrameMeta)>,
	num_macroblocks: u32,
) -> BufferType {
	let mut ref_pic_list_0: [PictureH264; 32] = std::array::from_fn(|_| build_invalid_pic());
	if let Some((id, m)) = reference {
		ref_pic_list_0[0] = build_h264_pic(id, m);
	}
	let ref_pic_list_1: [PictureH264; 32] = std::array::from_fn(|_| build_invalid_pic());

	let (num_ref_idx_l0_active_minus1, num_ref_idx_l1_active_minus1) = if header.num_ref_idx_active_override_flag {
		(header.num_ref_idx_l0_active_minus1, header.num_ref_idx_l1_active_minus1)
	} else {
		(
			pps.num_ref_idx_l0_default_active_minus1,
			pps.num_ref_idx_l1_default_active_minus1,
		)
	};

	BufferType::EncSliceParameter(EncSliceParameter::H264(EncSliceParameterBufferH264::new(
		header.first_mb_in_slice,
		num_macroblocks,
		VA_INVALID_ID,
		header.slice_type as u8,
		pps.pic_parameter_set_id,
		header.idr_pic_id,
		header.pic_order_cnt_lsb,
		header.delta_pic_order_cnt_bottom,
		header.delta_pic_order_cnt,
		header.direct_spatial_mv_pred_flag as u8,
		header.num_ref_idx_active_override_flag as u8,
		num_ref_idx_l0_active_minus1,
		num_ref_idx_l1_active_minus1,
		ref_pic_list_0,
		ref_pic_list_1,
		header.pred_weight_table.luma_log2_weight_denom,
		header.pred_weight_table.chroma_log2_weight_denom,
		0,
		header.pred_weight_table.luma_weight_l0,
		[0i16; 32],
		0,
		header.pred_weight_table.chroma_weight_l0,
		[[0i16; 2]; 32],
		0,
		header.pred_weight_table.luma_weight_l1,
		[0i16; 32],
		0,
		header.pred_weight_table.chroma_weight_l1,
		[[0i16; 2]; 32],
		header.cabac_init_idc,
		header.slice_qp_delta,
		header.disable_deblocking_filter_idc,
		header.slice_alpha_c0_offset_div2,
		header.slice_beta_offset_div2,
	)))
}

/// A packed-header parameter + data buffer pair for a synthesized NAL.
fn packed_header(kind: EncPackedHeaderType, data: &[u8]) -> (BufferType, BufferType) {
	let param =
		BufferType::EncPackedHeaderParameter(EncPackedHeaderParameter::new(kind, (data.len() * 8) as u32, true));
	let payload = BufferType::EncPackedHeaderData(data.to_vec());
	(param, payload)
}

#[cfg(test)]
mod tests {
	use std::path::PathBuf;
	use std::process::Command;
	use std::sync::atomic::{AtomicU32, Ordering};

	use super::*;
	use crate::dmabuf::DmaBuf;
	use crate::{VA_RT_FORMAT_RGB32, VA_RT_FORMAT_YUV420};

	const WIDTH: u32 = 320;
	const HEIGHT: u32 = 240;

	fn config() -> Config {
		Config::new(WIDTH, HEIGHT, 30, 2_000_000, 30)
	}

	/// Opens an encoder, or `None` to skip on a machine without VA-API H.264 encode.
	fn encoder(config: Config) -> Option<Encoder> {
		match Encoder::new(config) {
			Ok(encoder) => Some(encoder),
			Err(err) => {
				eprintln!("skipping: no VA-API H.264 encoder: {err:#}");
				None
			}
		}
	}

	/// A diagonal gradient with a block moving across it, so a stale or
	/// misplaced reference shows up as the block in the wrong place.
	fn nv12_frame(width: u32, height: u32, step: u32) -> Vec<u8> {
		let (w, h) = (width as usize, height as usize);
		let mut data = vec![128u8; w * h + w * h.div_ceil(2)];
		for y in 0..h {
			for x in 0..w {
				data[y * w + x] = (32 + (x + y) * 160 / (w + h)) as u8;
			}
		}
		let x0 = (step as usize * 8) % (w / 2);
		for y in h / 4..h / 2 {
			for x in x0..x0 + w / 4 {
				data[y * w + x] = 220;
			}
		}
		data
	}

	/// Returns a temporary file path unique to this call.
	///
	/// The tests run as threads of one process, so the process id alone would
	/// let two of them share a file.
	fn temp_path(label: &str) -> PathBuf {
		static NEXT: AtomicU32 = AtomicU32::new(0);
		let index = NEXT.fetch_add(1, Ordering::Relaxed);
		std::env::temp_dir().join(format!("moq-vaapi-{label}-{}-{index}.264", std::process::id()))
	}

	/// Decodes an Annex-B stream with ffmpeg's software decoder into NV12
	/// pictures, or `None` to skip without ffmpeg.
	fn ffmpeg_decode(stream: &[u8], name: &str) -> Option<Vec<u8>> {
		// ffmpeg reads the elementary stream from a file rather than a pipe, so
		// the test never has to drain its output while still writing its input.
		let path = temp_path(name);
		std::fs::write(&path, stream).expect("write the elementary stream");
		let output = Command::new("ffmpeg")
			.args(["-v", "error", "-i"])
			.arg(&path)
			.args(["-pix_fmt", "nv12", "-f", "rawvideo", "-"])
			.output();
		let _ = std::fs::remove_file(&path);
		match output {
			Ok(output) if output.status.success() => Some(output.stdout),
			Ok(output) => panic!("ffmpeg refused the stream: {}", String::from_utf8_lossy(&output.stderr)),
			Err(_) => {
				eprintln!("skipping: no ffmpeg");
				None
			}
		}
	}

	/// Asks ffprobe for the stream's `color_range,color_space`.
	fn ffprobe_color(stream: &[u8]) -> Option<String> {
		let path = temp_path("probe");
		std::fs::write(&path, stream).expect("write the elementary stream");
		let output = Command::new("ffprobe")
			.args([
				"-v",
				"error",
				"-show_entries",
				"stream=color_range,color_space",
				"-of",
				"csv=p=0",
			])
			.arg(&path)
			.output()
			.ok();
		let _ = std::fs::remove_file(&path);
		Some(String::from_utf8(output?.stdout).ok()?.trim().to_string())
	}

	/// Returns the peak signal-to-noise ratio of the luma planes, in dB.
	fn luma_psnr(a: &[u8], b: &[u8], width: u32, height: u32) -> f64 {
		let len = (width * height) as usize;
		let mse = a[..len]
			.iter()
			.zip(&b[..len])
			.map(|(&x, &y)| (x as f64 - y as f64).powi(2))
			.sum::<f64>()
			/ len as f64;
		10.0 * (255.0f64.powi(2) / mse.max(1e-9)).log10()
	}

	/// Returns the mean of the samples in a rectangle of one plane.
	fn mean(
		plane: &[u8],
		stride: usize,
		(x0, y0, x1, y1): (usize, usize, usize, usize),
		step: usize,
		at: usize,
	) -> f64 {
		let mut sum = 0.0;
		let mut count = 0.0;
		for y in y0..y1 {
			for x in x0..x1 {
				sum += plane[y * stride + x * step + at] as f64;
				count += 1.0;
			}
		}
		sum / count
	}

	/// Returns the NAL unit types in an Annex-B access unit.
	fn nal_types(unit: &[u8]) -> Vec<u8> {
		let mut types = Vec::new();
		let mut i = 0;
		while i + 3 < unit.len() {
			if unit[i..i + 3] == [0, 0, 1] {
				types.push(unit[i + 3] & 0x1f);
				i += 3;
			}
			i += 1;
		}
		types
	}

	/// Returns a driver surface of `fourcc` on `display`, filled through an image.
	fn surface_with(
		display: &Arc<Display>,
		fourcc: u32,
		rt_format: u32,
		(width, height): (u32, u32),
		fill: impl FnOnce(&mut [u8], &crate::bindings::VAImage),
	) -> Surface<()> {
		let surface = display
			.create_surfaces(
				rt_format,
				Some(fourcc),
				width,
				height,
				Some(UsageHint::USAGE_HINT_EXPORT),
				vec![()],
			)
			.expect("allocate a surface")
			.pop()
			.expect("one surface");
		let format = display
			.query_image_formats()
			.expect("query image formats")
			.into_iter()
			.find(|f| f.fourcc == fourcc)
			.expect("the driver has an image format for the surface format");
		let mut image =
			Image::create_from(&surface, format, (width, height), (width, height)).expect("create an image");
		let va_image = *image.image();
		fill(image.as_mut(), &va_image);
		drop(image);
		surface.sync().expect("sync the filled surface");
		surface
	}

	/// Returns an NV12 surface holding `nv12_frame`, exported as a DMA-BUF.
	fn nv12_dmabuf(display: &Arc<Display>, (width, height): (u32, u32), step: u32) -> DmaBuf {
		let surface = surface_with(display, VA_FOURCC_NV12, VA_RT_FORMAT_YUV420, (width, height), |_, _| {});
		upload_nv12(display, &surface, width, height, &nv12_frame(width, height, step)).expect("upload");
		DmaBuf::from_prime(surface.export_prime().expect("export the surface")).expect("one object")
	}

	/// With no node named, the encoder opens one that encodes H.264 and records
	/// which. A named node is used or refused, never swapped for another.
	#[test]
	fn the_render_node_is_found_or_named() {
		let Some(encoder) = encoder(config()) else { return };
		let device = encoder.config().device.clone().expect("the picked node is recorded");
		let display = Display::open_drm_display(&device).expect("reopen the picked node");
		probe(&display).expect("the picked node encodes H.264");

		let named = Config {
			device: Some(PathBuf::from("/dev/null")),
			..config()
		};
		assert!(
			Encoder::new(named).is_err(),
			"a named node that is not a render node was swapped for one that is"
		);
	}

	/// CPU NV12 in, H.264 out that an independent decoder reads back as the same picture.
	#[test]
	fn nv12_round_trips_through_a_software_decoder() {
		let Some(mut encoder) = encoder(config()) else { return };
		let mut stream = Vec::new();
		let mut frames = Vec::new();
		for step in 0..10 {
			let frame = nv12_frame(WIDTH, HEIGHT, step);
			stream.extend(encoder.encode_nv12(&frame, false).expect("encode"));
			frames.push(frame);
		}
		let Some(decoded) = ffmpeg_decode(&stream, "nv12") else {
			return;
		};
		let len = frames[0].len();
		assert_eq!(decoded.len(), len * frames.len(), "ffmpeg decoded a different count");
		for (index, frame) in frames.iter().enumerate() {
			let psnr = luma_psnr(frame, &decoded[index * len..], WIDTH, HEIGHT);
			assert!(psnr > 35.0, "picture {index} decoded at {psnr:.1} dB");
		}
	}

	/// The SPS names the color space the encoder was configured with.
	#[test]
	fn the_stream_is_labelled_with_its_color_space() {
		for (config, expected) in [
			(config(), "tv,smpte170m"),
			(
				Config {
					color: Color::BT709,
					..config()
				},
				"tv,bt709",
			),
		] {
			let Some(mut encoder) = encoder(config) else { return };
			let unit = encoder
				.encode_nv12(&nv12_frame(WIDTH, HEIGHT, 0), true)
				.expect("encode");
			let Some(color) = ffprobe_color(&unit) else {
				eprintln!("skipping: no ffprobe");
				return;
			};
			assert_eq!(color, expected);
		}
	}

	/// An IDR opens every group, and only there.
	#[test]
	fn keyframes_follow_the_group_length() {
		let Some(mut encoder) = encoder(Config { gop: 10, ..config() }) else {
			return;
		};
		for step in 0..25 {
			let unit = encoder
				.encode_nv12(&nv12_frame(WIDTH, HEIGHT, step), false)
				.expect("encode");
			let types = nal_types(&unit);
			let idr = types.contains(&5);
			assert_eq!(idr, step % 10 == 0, "frame {step} has NAL types {types:?}");
			if idr {
				assert!(
					types.contains(&7) && types.contains(&8),
					"an IDR carries its SPS and PPS"
				);
			}
		}
		// A forced keyframe restarts the count: the next IDR is a group later.
		for step in 0..=10 {
			let unit = encoder
				.encode_nv12(&nv12_frame(WIDTH, HEIGHT, step), step == 0)
				.expect("encode");
			assert_eq!(
				nal_types(&unit).contains(&5),
				step % 10 == 0,
				"frame {step} after the forced keyframe"
			);
		}
	}

	#[test]
	fn the_full_entrypoint_wins_and_low_power_is_the_fallback() {
		use VAEntrypoint::{
			VAEntrypointEncSlice as Full, VAEntrypointEncSliceLP as LowPower, VAEntrypointVLD as Decode,
		};
		assert_eq!(pick_entrypoint(&[Decode, LowPower, Full]), Some(Full));
		assert_eq!(pick_entrypoint(&[Decode, LowPower]), Some(LowPower));
		assert_eq!(pick_entrypoint(&[Decode]), None);
	}

	/// A buffer the driver refuses leaves the encoder usable, so the caller's
	/// CPU fallback can encode the same frame.
	#[test]
	fn a_refused_import_leaves_the_encoder_usable() {
		let Some(mut encoder) = encoder(config()) else { return };
		let Some(display) = Display::open() else {
			eprintln!("skipping: no VA-API display");
			return;
		};
		// A format with no mapping, and a real buffer under a modifier no driver has.
		let unmapped = DmaBuf {
			drm_format: u32::from_le_bytes(*b"ZZZZ"),
			..nv12_dmabuf(&display, (WIDTH, HEIGHT), 0)
		};
		assert!(encoder.encode_dmabuf(unmapped, true).is_err());
		let unknown_modifier = DmaBuf {
			modifier: 0x00ff_ffff_ffff_fffe,
			..nv12_dmabuf(&display, (WIDTH, HEIGHT), 0)
		};
		assert!(encoder.encode_dmabuf(unknown_modifier, true).is_err());

		let unit = encoder
			.encode_nv12(&nv12_frame(WIDTH, HEIGHT, 0), true)
			.expect("the CPU path still encodes");
		let Some(decoded) = ffmpeg_decode(&unit, "refused") else {
			return;
		};
		let psnr = luma_psnr(&nv12_frame(WIDTH, HEIGHT, 0), &decoded, WIDTH, HEIGHT);
		assert!(psnr > 35.0, "decoded at {psnr:.1} dB");
	}

	/// A 1080-row buffer is not encoded in place: the encoder reads 1088 rows,
	/// so it goes through the video processor into the padded input surface.
	#[test]
	fn a_buffer_short_of_whole_macroblocks_is_blitted() {
		let (width, height) = (1920, 1080);
		let Some(mut encoder) = encoder(Config::new(width, height, 30, 4_000_000, 30)) else {
			return;
		};
		let Some(display) = Display::open() else {
			eprintln!("skipping: no VA-API display");
			return;
		};
		let unit = encoder
			.encode_dmabuf(nv12_dmabuf(&display, (width, height), 0), true)
			.expect("encode the DMA-BUF");
		let Some(decoded) = ffmpeg_decode(&unit, "1080") else {
			return;
		};
		let psnr = luma_psnr(&nv12_frame(width, height, 0), &decoded, width, height);
		assert!(psnr > 35.0, "decoded at {psnr:.1} dB");
	}

	/// A YUV buffer labelled with another color space is converted into the
	/// stream's, not relabelled.
	#[test]
	fn a_yuv_dmabuf_in_another_space_is_converted() {
		let Some(mut encoder) = encoder(Config {
			color: Color::BT601,
			..config()
		}) else {
			return;
		};
		let Some(display) = Display::open() else {
			eprintln!("skipping: no VA-API display");
			return;
		};
		// Flat red, written in BT.709.
		let [y, u, v] = limited([255, 0, 0], Color::BT709).map(|value| value.round() as u8);
		let surface = surface_with(
			&display,
			VA_FOURCC_NV12,
			VA_RT_FORMAT_YUV420,
			(WIDTH, HEIGHT),
			|data, image| {
				for row in 0..HEIGHT as usize {
					let at = image.offsets[0] as usize + row * image.pitches[0] as usize;
					data[at..at + WIDTH as usize].fill(y);
				}
				for row in 0..HEIGHT as usize / 2 {
					let at = image.offsets[1] as usize + row * image.pitches[1] as usize;
					for pair in data[at..at + WIDTH as usize].chunks_exact_mut(2) {
						pair.copy_from_slice(&[u, v]);
					}
				}
			},
		);
		let buffer = DmaBuf {
			color: Some(Color::BT709),
			..DmaBuf::from_prime(surface.export_prime().expect("export")).expect("one object")
		};
		let unit = encoder.encode_dmabuf(buffer, true).expect("encode");
		let Some(decoded) = ffmpeg_decode(&unit, "yuv-color") else {
			return;
		};

		let (w, h) = (WIDTH as usize, HEIGHT as usize);
		let (luma, chroma) = decoded.split_at(w * h);
		let actual = [
			mean(luma, w, (w / 4, h / 4, w * 3 / 4, h * 3 / 4), 1, 0),
			mean(chroma, w, (w / 8, h / 8, w * 3 / 8, h * 3 / 8), 2, 0),
			mean(chroma, w, (w / 8, h / 8, w * 3 / 8, h * 3 / 8), 2, 1),
		];
		let expected = limited([255, 0, 0], Color::BT601);
		for (channel, (a, e)) in actual.iter().zip(expected).enumerate() {
			assert!(
				(a - e).abs() < 6.0,
				"channel {channel}: decoded {a:.1}, expected BT.601 {e:.1}"
			);
		}
	}

	/// An NV12 DMA-BUF at the encoder's size is encoded from the producer's memory.
	#[test]
	fn an_nv12_dmabuf_encodes_in_place() {
		let Some(mut encoder) = encoder(config()) else { return };
		let Some(display) = Display::open() else {
			eprintln!("skipping: no VA-API display");
			return;
		};
		let mut stream = Vec::new();
		for step in 0..5 {
			let buffer = nv12_dmabuf(&display, (WIDTH, HEIGHT), step);
			stream.extend(encoder.encode_dmabuf(buffer, false).expect("encode the DMA-BUF"));
		}
		let Some(decoded) = ffmpeg_decode(&stream, "dmabuf") else {
			return;
		};
		let len = nv12_frame(WIDTH, HEIGHT, 0).len();
		assert_eq!(decoded.len(), 5 * len);
		for step in 0..5 {
			let psnr = luma_psnr(
				&nv12_frame(WIDTH, HEIGHT, step),
				&decoded[step as usize * len..],
				WIDTH,
				HEIGHT,
			);
			assert!(psnr > 35.0, "picture {step} decoded at {psnr:.1} dB");
		}
	}

	/// An NV12 DMA-BUF at another size is scaled to the encoder's on the GPU.
	#[test]
	fn a_dmabuf_at_another_size_is_scaled() {
		let Some(mut encoder) = encoder(config()) else { return };
		let Some(display) = Display::open() else {
			eprintln!("skipping: no VA-API display");
			return;
		};
		let mut stream = Vec::new();
		for step in 0..3 {
			let buffer = nv12_dmabuf(&display, (WIDTH * 2, HEIGHT * 2), step);
			stream.extend(encoder.encode_dmabuf(buffer, false).expect("encode the DMA-BUF"));
		}
		let Some(decoded) = ffmpeg_decode(&stream, "scaled") else {
			return;
		};
		let len = nv12_frame(WIDTH, HEIGHT, 0).len();
		assert_eq!(decoded.len(), 3 * len, "the output is at the encoder's size");
		// Compare against the source scaled on the CPU by point sampling, which
		// differs from the GPU filter only at edges. 25 dB separates a scale
		// from a crop, which scores about 16 dB on this gradient.
		for step in 0..3u32 {
			let source = nv12_frame(WIDTH * 2, HEIGHT * 2, step);
			let mut expected = vec![0u8; (WIDTH * HEIGHT) as usize];
			for y in 0..HEIGHT as usize {
				for x in 0..WIDTH as usize {
					expected[y * WIDTH as usize + x] = source[(y * 2) * (WIDTH as usize * 2) + x * 2];
				}
			}
			let psnr = luma_psnr(&expected, &decoded[step as usize * len..], WIDTH, HEIGHT);
			assert!(psnr > 25.0, "picture {step} decoded at {psnr:.1} dB");
		}
	}

	/// A packed RGB DMA-BUF is converted on the GPU into the configured color space.
	#[test]
	fn an_rgb_dmabuf_is_converted_to_the_configured_space() {
		// Four flat quadrants, so the check reads colors rather than edges.
		const QUADRANTS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];
		for color in [Color::BT601, Color::BT709] {
			let Some(mut encoder) = encoder(Config { color, ..config() }) else {
				return;
			};
			let Some(display) = Display::open() else {
				eprintln!("skipping: no VA-API display");
				return;
			};
			let surface = surface_with(
				&display,
				VA_FOURCC_BGRX,
				VA_RT_FORMAT_RGB32,
				(WIDTH, HEIGHT),
				|data, image| {
					let pitch = image.pitches[0] as usize;
					let offset = image.offsets[0] as usize;
					for y in 0..HEIGHT as usize {
						for x in 0..WIDTH as usize {
							let quadrant = (y >= HEIGHT as usize / 2) as usize * 2 + (x >= WIDTH as usize / 2) as usize;
							let [r, g, b] = QUADRANTS[quadrant];
							let at = offset + y * pitch + x * 4;
							data[at..at + 4].copy_from_slice(&[b, g, r, 255]);
						}
					}
				},
			);
			let exported = surface.export_prime().expect("export");
			let buffer = DmaBuf::from_prime(exported).expect("one object");
			assert_eq!(buffer.drm_format, crate::dmabuf::drm::XRGB8888);
			let unit = encoder.encode_dmabuf(buffer, false).expect("encode the RGB DMA-BUF");
			let Some(decoded) = ffmpeg_decode(&unit, "rgb") else {
				return;
			};

			let (w, h) = (WIDTH as usize, HEIGHT as usize);
			let (luma, chroma) = decoded.split_at(w * h);
			for (quadrant, rgb) in QUADRANTS.iter().enumerate() {
				let (qx, qy) = (quadrant % 2, quadrant / 2);
				// The middle half of each quadrant, clear of the chroma bleed at its edges.
				let area = |scale: usize| {
					let (qw, qh) = (w / 2 / scale, h / 2 / scale);
					(
						qx * qw + qw / 4,
						qy * qh + qh / 4,
						qx * qw + qw * 3 / 4,
						qy * qh + qh * 3 / 4,
					)
				};
				let actual = [
					mean(luma, w, area(1), 1, 0),
					mean(chroma, w, area(2), 2, 0),
					mean(chroma, w, area(2), 2, 1),
				];
				let expected = limited(*rgb, color);
				for (channel, (a, e)) in actual.iter().zip(expected).enumerate() {
					assert!(
						(a - e).abs() < 6.0,
						"{color:?} quadrant {quadrant} channel {channel}: decoded {a:.1}, expected {e:.1}"
					);
				}
			}
		}
	}

	#[test]
	fn the_level_covers_the_frame_size_and_rate() {
		assert_eq!(level(320, 240, 30), Level::L4);
		assert_eq!(level(1920, 1080, 30), Level::L4);
		assert_eq!(level(1920, 1080, 60), Level::L4_2);
		assert_eq!(level(2560, 1440, 30), Level::L5);
		assert_eq!(level(3840, 2160, 30), Level::L5_1);
		assert_eq!(level(3840, 2160, 60), Level::L5_2);
	}

	/// Returns the limited-range Y'CbCr of an RGB color, per BT.601 or BT.709.
	fn limited([r, g, b]: [u8; 3], color: Color) -> [f64; 3] {
		let (kr, kb) = match color.matrix {
			crate::Matrix::Bt601 => (0.299, 0.114),
			crate::Matrix::Bt709 => (0.2126, 0.0722),
		};
		let (r, g, b) = (r as f64, g as f64, b as f64);
		let y = kr * r + (1.0 - kr - kb) * g + kb * b;
		[
			16.0 + y * 219.0 / 255.0,
			128.0 + (b - y) / (2.0 * (1.0 - kb)) * 224.0 / 255.0,
			128.0 + (r - y) / (2.0 * (1.0 - kr)) * 224.0 / 255.0,
		]
	}

	/// Lowering the bitrate mid-stream shrinks the pictures without an IDR.
	#[test]
	fn a_bitrate_change_takes_effect_without_a_keyframe() {
		let Some(mut encoder) = encoder(Config::new(WIDTH, HEIGHT, 30, 2_000_000, 1_000)) else {
			return;
		};
		// The moving block over light noise: enough detail that 2 Mbps is
		// spent, not so much that the quantizer bound decides the size instead
		// of the rate controller.
		let mut state = 0x2545_f491_u32;
		let mut frame = |step: u32| {
			let mut frame = nv12_frame(WIDTH, HEIGHT, step);
			for sample in &mut frame[..(WIDTH * HEIGHT) as usize] {
				state ^= state << 13;
				state ^= state >> 17;
				state ^= state << 5;
				*sample = sample.saturating_add((state >> 28) as u8);
			}
			frame
		};
		// The mean picture size over the second half of 60 frames, once the
		// rate controller has settled.
		// Also asserts that no picture after the first is an IDR.
		let mut average = |encoder: &mut Encoder, from: u32| {
			let sizes: Vec<usize> = (from..from + 60)
				.map(|step| {
					let unit = encoder.encode_nv12(&frame(step), false).expect("encode");
					if step > 0 {
						assert!(!nal_types(&unit).contains(&5), "picture {step} is an IDR");
					}
					unit.len()
				})
				.collect();
			sizes[30..].iter().sum::<usize>() / 30
		};

		let high = average(&mut encoder, 0);
		encoder.set_bitrate(250_000).expect("set the bitrate");
		assert_eq!(encoder.config().bitrate, 250_000);
		assert!(encoder.set_bitrate(0).is_err(), "a zero bitrate is refused");
		let low = average(&mut encoder, 60);
		eprintln!("mean picture at 2 Mbps: {high} bytes, at 250 kbps: {low} bytes");
		assert!(
			low * 3 < high,
			"the rate did not drop: {high} -> {low} bytes per picture"
		);
	}
}
