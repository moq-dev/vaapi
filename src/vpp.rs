//! Video post-processing: the `VAEntrypointVideoProc` pipeline.
//!
//! The one operation is a blit from one surface into another, which the video
//! processor stretches to fit the destination and converts to the destination's
//! format on the way. That covers three jobs without the pixels leaving the GPU:
//!
//! - Scaling: blit into a smaller or larger surface.
//! - Color conversion: blit a packed RGB or YUYV surface into an NV12 one, the
//!   format an encoder takes. [`Processor::blit`] takes the YUV space to write.
//! - Re-tiling: blit into a surface of the same size and format. The destination
//!   is one the driver allocated for itself, so the blit rewrites the pixels
//!   into whatever tiling the driver prefers for a fresh allocation. That is how
//!   a DMA-BUF carrying a format modifier some other API refuses to import
//!   becomes one it accepts. Measured on Intel Meteor Lake with iHD 26.1.5 (the
//!   `retile_round_trip` test prints it), a decode target exports at
//!   `0x100000000000009` (`I915_FORMAT_MOD_4_TILED`), which Mesa's Vulkan driver
//!   lists, so that combination does not need it. Older iHD releases wrote
//!   `Y_TILED` (`0x100000000000002`), which Mesa does not list.
//!
//! ```no_run
//! # fn example(buffer: moq_vaapi::dmabuf::DmaBuf) -> anyhow::Result<()> {
//! use moq_vaapi::vpp::Processor;
//!
//! let processor = Processor::new("/dev/dri/renderD128")?;
//! let retiled = processor.retile(buffer)?;
//! let exported = retiled.export_prime().map_err(|e| anyhow::anyhow!("export: {e:?}"))?;
//! # let _ = exported;
//! # Ok(())
//! # }
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::dmabuf::{rt_format, DmaBuf};
use crate::{
	bindings, BufferType, Color, Config as VaConfig, Context, Display, Picture, ProcColorProperties,
	ProcPipelineParameterBuffer, Surface, SurfaceMemoryDescriptor, UsageHint, VAConfigAttrib, VAConfigAttribType,
	VAEntrypoint, VAProfile,
};

/// Checks that `display`'s driver offers video post-processing, which is what [`Processor::with_display`] needs of a render node.
///
/// # Errors
///
/// Fails when the driver has no `VAEntrypointVideoProc`, or cannot be asked.
pub fn probe(display: &Display) -> anyhow::Result<()> {
	let entrypoints = display
		.query_config_entrypoints(VAProfile::VAProfileNone)
		.map_err(|e| anyhow::anyhow!("query video processing entrypoints: {e:?}"))?;
	match entrypoints.contains(&VAEntrypoint::VAEntrypointVideoProc) {
		true => Ok(()),
		false => anyhow::bail!("the driver has no video post-processing entrypoint"),
	}
}

/// A video post-processor on one device.
///
/// Holds the display, the VPP configuration, and one processing context per
/// destination size, which are what cost something to build. A caller
/// converting every frame of a stream, or of a few renditions, pays for each
/// context once.
pub struct Processor {
	display: Arc<Display>,
	// Drop order: the contexts before the config they were created from.
	contexts: RefCell<HashMap<(u32, u32), Rc<Context>>>,
	config: VaConfig,
}

impl Processor {
	/// Opens `device` and configures it for video post-processing.
	///
	/// `device` is a DRM render node, e.g. `/dev/dri/renderD128`. Pick the one
	/// matching the device that will consume the result: the blit only stays on
	/// the GPU when both ends are the same device.
	pub fn new<P: AsRef<Path>>(device: P) -> anyhow::Result<Self> {
		let device = device.as_ref();
		let display =
			Display::open_drm_display(device).map_err(|e| anyhow::anyhow!("open DRM display {device:?}: {e}"))?;
		Self::with_display(display)
	}

	/// Opens the first device whose driver can post-process, if any.
	///
	/// Mirrors [`Display::open`], and is what a caller with no opinion about
	/// which GPU should use. A machine with more than one wants
	/// [`Processor::new`] or [`Processor::with_display`] instead.
	pub fn open() -> anyhow::Result<Self> {
		let (_, display) =
			crate::display::open_first(probe).map_err(|e| e.context("find a render node that post-processes video"))?;
		Self::with_display(display)
	}

	/// Configures an already open display for video post-processing.
	///
	/// Sharing the display with a decoder or an encoder is what lets a surface
	/// move between them: a surface belongs to the display that created it.
	///
	/// # Errors
	///
	/// Fails when the device has no `VAEntrypointVideoProc`, which is how a
	/// caller finds out before the first frame rather than during it.
	pub fn with_display(display: Arc<Display>) -> anyhow::Result<Self> {
		// Asking for the attribute is the probe: a device without the VPP
		// entrypoint fails here instead of at vaCreateConfig, with an error that
		// says which entrypoint was missing.
		let mut attrs = [VAConfigAttrib {
			type_: VAConfigAttribType::VAConfigAttribRTFormat,
			value: 0,
		}];
		display
			.get_config_attributes(
				VAProfile::VAProfileNone,
				VAEntrypoint::VAEntrypointVideoProc,
				&mut attrs,
			)
			.map_err(|e| anyhow::anyhow!("query VPP config attributes: {e:?}"))?;

		let config = display
			.create_config(
				attrs.to_vec(),
				VAProfile::VAProfileNone,
				VAEntrypoint::VAEntrypointVideoProc,
			)
			.map_err(|e| anyhow::anyhow!("create VPP config: {e:?}"))?;

		Ok(Self {
			display,
			contexts: RefCell::new(HashMap::new()),
			config,
		})
	}

	/// Returns the display this processor runs on, to share with a decoder or an encoder.
	pub fn display(&self) -> &Arc<Display> {
		&self.display
	}

	/// Imports an exported DMA-BUF as a surface on this processor's display, without copying it.
	///
	/// The returned surface owns `buffer`, so the descriptor stays open exactly
	/// as long as the surface referring to it.
	///
	/// # Errors
	///
	/// See [`DmaBuf::import`].
	pub fn import(&self, buffer: DmaBuf) -> anyhow::Result<Surface<DmaBuf>> {
		buffer.import(&self.display, Some(UsageHint::USAGE_HINT_VPP_READ))
	}

	/// Blits `input` into `output`, scaling to the size of `output` and converting to its format.
	///
	/// `input_color` and `output_color` name the YUV space on each side. Leave
	/// the input side `None` for an RGB input, which has no matrix; set the
	/// output side to the space a YUV destination should be written in. A side
	/// left `None` is up to the driver, which is right for a blit that only
	/// re-tiles, and for a scale that names neither side. Returns once the
	/// destination holds the result.
	///
	/// # Errors
	///
	/// Fails when either surface belongs to another display, and when the
	/// driver refuses the blit (a format pair or scale it cannot do).
	pub fn blit<D, E>(
		&self,
		input: &Surface<D>,
		output: &Surface<E>,
		input_color: Option<Color>,
		output_color: Option<Color>,
	) -> anyhow::Result<()>
	where
		D: SurfaceMemoryDescriptor,
		E: SurfaceMemoryDescriptor,
	{
		// A surface id means something only on the display that made it, and
		// the driver would read another display's id as one of its own.
		if !Arc::ptr_eq(input.display(), &self.display) || !Arc::ptr_eq(output.display(), &self.display) {
			anyhow::bail!("a VPP blit between surfaces of another display");
		}
		let size = output.size();
		let context = self.context(size)?;
		let result = run_blit(&context, input, output, input_color, output_color);
		if result.is_err() {
			// A picture that failed half way can leave the context mid-picture,
			// so the next blit of this size starts on a fresh one.
			self.contexts.borrow_mut().remove(&size);
		}
		result
	}

	/// Blits `input` into a new driver-allocated surface of `fourcc`, `width` and `height`.
	///
	/// The destination is hinted as post-processing output that will be
	/// exported, so [`Surface::export_prime`] works on it. See
	/// [`blit`](Self::blit) for the colors.
	///
	/// # Errors
	///
	/// Fails for a format with no render target mapping, when the destination
	/// cannot be allocated, and as [`blit`](Self::blit) does.
	pub fn process<D: SurfaceMemoryDescriptor>(
		&self,
		input: &Surface<D>,
		fourcc: u32,
		(width, height): (u32, u32),
		input_color: Option<Color>,
		output_color: Option<Color>,
	) -> anyhow::Result<Surface<()>> {
		let rt_format = rt_format(fourcc).ok_or_else(|| anyhow::anyhow!("no RT format for fourcc {fourcc:#x}"))?;
		let output = self
			.display
			.create_surfaces(
				rt_format,
				Some(fourcc),
				width,
				height,
				Some(UsageHint::USAGE_HINT_VPP_WRITE | UsageHint::USAGE_HINT_EXPORT),
				vec![()],
			)
			.map_err(|e| anyhow::anyhow!("allocate a {width}x{height} VPP output surface: {e:?}"))?
			.pop()
			.ok_or_else(|| anyhow::anyhow!("vaCreateSurfaces returned no surface"))?;

		self.blit(input, &output, input_color, output_color)?;
		Ok(output)
	}

	/// Imports `buffer` and blits it into a driver-allocated surface of the same format and size.
	///
	/// This is the whole re-tile. Export the result with
	/// [`Surface::export_prime`] and read the modifier off the object to see what
	/// the driver chose.
	/// Destroying the returned surface does not invalidate an export taken from
	/// it, since the exported descriptor holds its own reference on the
	/// underlying buffer.
	pub fn retile(&self, buffer: DmaBuf) -> anyhow::Result<Surface<()>> {
		let fourcc = buffer
			.fourcc()
			.ok_or_else(|| anyhow::anyhow!("no VA-API format for DRM format {:#010x}", buffer.drm_format))?;
		let input = self.import(buffer)?;
		self.process(&input, fourcc, input.size(), None, None)
	}

	/// Returns the processing context for a destination of `size`, building it on first use.
	fn context(&self, (width, height): (u32, u32)) -> anyhow::Result<Rc<Context>> {
		let mut contexts = self.contexts.borrow_mut();
		if let Some(context) = contexts.get(&(width, height)) {
			return Ok(Rc::clone(context));
		}
		// No render targets: a VPP context takes its destination per picture, and
		// binding it to one surface would make every other destination a rebuild.
		let context = self
			.display
			.create_context::<()>(&self.config, width, height, None, true)
			.map_err(|e| anyhow::anyhow!("create a {width}x{height} VPP context: {e:?}"))?;
		contexts.insert((width, height), Rc::clone(&context));
		Ok(context)
	}
}

/// Runs one blit on `context` and waits for it to finish.
fn run_blit<D, E>(
	context: &Rc<Context>,
	input: &Surface<D>,
	output: &Surface<E>,
	input_color: Option<Color>,
	output_color: Option<Color>,
) -> anyhow::Result<()>
where
	D: SurfaceMemoryDescriptor,
	E: SurfaceMemoryDescriptor,
{
	let parameters = pipeline(input.id(), input_color, output_color);
	let buffer = context
		.create_buffer(BufferType::ProcPipelineParameter(parameters))
		.map_err(|e| anyhow::anyhow!("create VPP pipeline buffer: {e:?}"))?;

	let mut picture = Picture::new(0, Rc::clone(context), output);
	picture.add_buffer(buffer);
	picture
		.begin()
		.map_err(|e| anyhow::anyhow!("vaBeginPicture: {e:?}"))?
		.render()
		.map_err(|e| anyhow::anyhow!("vaRenderPicture: {e:?}"))?
		.end()
		.map_err(|e| anyhow::anyhow!("vaEndPicture: {e:?}"))?
		.sync()
		.map_err(|(e, _)| anyhow::anyhow!("vaSyncSurface: {e:?}"))?;
	Ok(())
}

/// Returns a pipeline parameter buffer describing a plain blit of `surface`.
///
/// No regions (so the whole input stretched over the whole output), no
/// filters, no references, no rotation or mirroring, and each side's color
/// standard and range as given, or the driver's default where not.
fn pipeline(
	surface: bindings::VASurfaceID,
	input_color: Option<Color>,
	output_color: Option<Color>,
) -> ProcPipelineParameterBuffer {
	let side = |color: Option<Color>| match color {
		Some(color) => (
			color.va_standard(),
			ProcColorProperties::new(0, color.va_range(), 0, 0, 0),
		),
		None => (0, ProcColorProperties::default()),
	};
	let (input_standard, input_properties) = side(input_color);
	let (output_standard, output_properties) = side(output_color);
	ProcPipelineParameterBuffer::new(
		surface,
		None,
		input_standard,
		None,
		0,
		output_standard,
		0,
		0,
		None,
		None,
		None,
		0,
		None,
		0,
		None,
		0,
		0,
		input_properties,
		output_properties,
		0,
		None,
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{VA_FOURCC_NV12, VA_RT_FORMAT_YUV420};

	/// A driver-allocated NV12 surface survives a round trip through the
	/// post-processor, and the modifier the driver picked is printed.
	///
	/// Skips without a VA-API device, so it is a no-op on a builder and real
	/// coverage on a machine with a GPU. What it asserts is that the whole
	/// sequence runs (import an exported DMA-BUF, blit, export the result); the
	/// modifier the driver picks is hardware policy, so it is printed rather
	/// than asserted.
	#[test]
	fn retile_round_trip() {
		let Some(display) = Display::open() else {
			eprintln!("skipping: no VA-API display");
			return;
		};
		let Ok(processor) = Processor::with_display(display) else {
			eprintln!("skipping: device has no video post-processing entrypoint");
			return;
		};

		// A decode-hinted surface, which is the allocation whose tiling is the
		// problem a re-tile exists for.
		let mut surfaces = processor
			.display()
			.create_surfaces(
				VA_RT_FORMAT_YUV420,
				Some(VA_FOURCC_NV12),
				640,
				480,
				Some(UsageHint::USAGE_HINT_DECODER | UsageHint::USAGE_HINT_EXPORT),
				vec![()],
			)
			.expect("allocate a decode surface");
		let decoded = surfaces.pop().expect("one surface");
		let exported = decoded.export_prime().expect("export the decode surface");
		let source_modifier = exported.objects[0].drm_format_modifier;
		let (fourcc, size, planes) = (
			exported.fourcc,
			(exported.width, exported.height),
			exported.layers[0].num_planes,
		);
		let buffer = DmaBuf::from_prime(exported).expect("a composed export is one object");

		let retiled = processor.retile(buffer).expect("re-tile the decode surface");
		let output = retiled.export_prime().expect("export the re-tiled surface");
		let target_modifier = output.objects[0].drm_format_modifier;

		eprintln!("modifier {source_modifier:#x} -> {target_modifier:#x}");
		assert_eq!(output.fourcc, fourcc, "the blit preserves the pixel format");
		assert_eq!((output.width, output.height), size);
		assert_eq!(output.objects.len(), 1, "a composed export is one object");
		assert_eq!(
			output.layers[0].num_planes, planes,
			"the blit preserves the plane count"
		);
	}
	/// A surface scaled by the processor exports and reads back at its new
	/// size, with the picture scaled rather than cropped.
	#[test]
	fn a_scaled_surface_reads_back_at_its_new_size() {
		let Some(display) = Display::open() else {
			eprintln!("skipping: no VA-API display");
			return;
		};
		let Ok(processor) = Processor::with_display(display) else {
			eprintln!("skipping: device has no video post-processing entrypoint");
			return;
		};

		// Left half dark, right half bright, so a crop and a scale differ.
		let (width, height) = (640u32, 480u32);
		let source = processor
			.display()
			.create_surfaces(
				VA_RT_FORMAT_YUV420,
				Some(VA_FOURCC_NV12),
				width,
				height,
				Some(UsageHint::USAGE_HINT_EXPORT),
				vec![()],
			)
			.expect("allocate a surface")
			.pop()
			.expect("one surface");
		let format = processor
			.display()
			.query_image_formats()
			.expect("query image formats")
			.into_iter()
			.find(|f| f.fourcc == VA_FOURCC_NV12)
			.expect("an NV12 image format");
		{
			let mut image =
				crate::Image::create_from(&source, format, (width, height), (width, height)).expect("create an image");
			let va_image = *image.image();
			let data = image.as_mut();
			for y in 0..height as usize {
				let row = va_image.offsets[0] as usize + y * va_image.pitches[0] as usize;
				for x in 0..width as usize {
					data[row + x] = if x < width as usize / 2 { 40 } else { 200 };
				}
			}
			for y in 0..height as usize / 2 {
				let row = va_image.offsets[1] as usize + y * va_image.pitches[1] as usize;
				data[row..row + width as usize].fill(128);
			}
		}

		let scaled = processor
			.process(&source, VA_FOURCC_NV12, (width / 2, height / 2), None, None)
			.expect("scale the surface");
		let exported = crate::decode::ExportedFrame::from_surface(scaled, 7).expect("export the scaled surface");
		assert_eq!((exported.width, exported.height), (width / 2, height / 2));
		assert_eq!(exported.timestamp, 7);
		let frame = exported.download().expect("read the scaled surface back");
		let (w, h) = ((width / 2) as usize, (height / 2) as usize);
		assert_eq!(frame.data.len(), w * h * 3 / 2);
		let (left, right) = (frame.data[h / 2 * w + w / 8], frame.data[h / 2 * w + w * 7 / 8]);
		assert!(
			left.abs_diff(40) < 4 && right.abs_diff(200) < 4,
			"read back {left} and {right}"
		);
	}
}
