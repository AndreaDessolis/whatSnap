use windows::{
	Win32::{
		Media::MediaFoundation::{
			IMFAttributes, IMFByteStream, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
			MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFCreateMediaType,
			MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Video, MFVideoFormat_H264,
			MFVideoFormat_RGB32, MFVideoInterlace_Progressive,
		},
		System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
	},
	core::HSTRING,
};

use crate::media::{
	foundation::{self, pack_u64},
	gif::GifDecoder,
};

const MAX_EDGE: u32 = 640;
const MAX_FRAMES: usize = 600;
const MIN_DELAY_MS: u32 = 20;
const DEFAULT_DELAY_MS: u32 = 100;
const NOMINAL_FPS: u32 = 25;

/// GIFs whose canvas is bigger than this aren't converted (they go as a
/// photo): the decoder keeps a full-size canvas, plus a copy per frame.
const MAX_SOURCE_PIXELS: u64 = 4096 * 4096;

/// Converts a GIF to an MP4 one frame at a time - decode, resize, encode -
/// so only the current frame is ever held, never all of them (600 frames at
/// 640x640 would be close to a gigabyte). Returns the first frame as a PNG.
pub(crate) fn gif_to_mp4(gif_path: &str, mp4_path: &str) -> crate::error::Result<Vec<u8>> {
	let data = std::fs::read(gif_path)?;
	if data.len() >= 10 {
		let w = u16::from_le_bytes([data[6], data[7]]) as u64;
		let h = u16::from_le_bytes([data[8], data[9]]) as u64;
		if w * h > MAX_SOURCE_PIXELS {
			crate::error::bail!("GIF is too large to convert ({w}x{h})");
		}
	}
	let decoder = GifDecoder::new(&data).ok_or_else(|| crate::error::err!("not a GIF"))?;
	let source_size = (decoder.width, decoder.height);
	let (width, height) = encoded_size(decoder.width, decoder.height);
	let mut encoder: Option<Encoder> = None;
	let mut first_png = None;
	for frame in decoder.take(MAX_FRAMES) {
		let ms = if frame.delay_ms < MIN_DELAY_MS { DEFAULT_DELAY_MS } else { frame.delay_ms };
		let rgba = resize(frame.rgba, source_size, (width, height));
		if first_png.is_none() {
			first_png = Some(crate::media::video_thumbnail::thumbnail_png(width, height, rgba.clone()));
		}
		let encoder = match encoder.as_mut() {
			Some(encoder) => encoder,
			None => encoder.insert(Encoder::new(width, height, mp4_path)?),
		};
		encoder.write(&rgba, ms)?;
	}
	let (Some(encoder), Some(first_png)) = (encoder, first_png) else {
		crate::error::bail!("GIF has no frames");
	};
	encoder.finish()?;
	Ok(first_png)
}

fn encoded_size(width: u32, height: u32) -> (u32, u32) {
	let (mut w, mut h) = (width.max(2), height.max(2));
	if w > MAX_EDGE || h > MAX_EDGE {
		if w >= h {
			h = (h * MAX_EDGE / w).max(2);
			w = MAX_EDGE;
		} else {
			w = (w * MAX_EDGE / h).max(2);
			h = MAX_EDGE;
		}
	}
	(w & !1, h & !1)
}

pub(super) fn resize(rgba: Vec<u8>, (sw, sh): (u32, u32), (dw, dh): (u32, u32)) -> Vec<u8> {
	if (sw, sh) == (dw, dh) {
		return rgba;
	}
	fn weights(src: usize, dst: usize) -> Vec<Vec<(usize, f32)>> {
		let scale = src as f32 / dst as f32;
		let support = scale.max(1.0);
		(0..dst)
			.map(|d| {
				let center = (d as f32 + 0.5) * scale;
				let lo = (center - support).floor().max(0.0) as usize;
				let hi = ((center + support).ceil() as usize).min(src);
				let mut taps: Vec<(usize, f32)> = (lo..hi)
					.map(|s| (s, (1.0 - ((s as f32 + 0.5 - center) / support).abs()).max(0.0)))
					.filter(|&(_, w)| w > 0.0)
					.collect();
				if taps.is_empty() {
					taps.push((((center as usize).min(src - 1)), 1.0));
				}
				let total: f32 = taps.iter().map(|&(_, w)| w).sum();
				taps.iter_mut().for_each(|(_, w)| *w /= total);
				taps
			})
			.collect()
	}
	let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
	let (across, down) = (weights(sw, dw), weights(sh, dh));
	let mut wide = vec![0f32; dw * sh * 4];
	for y in 0..sh {
		for (x, taps) in across.iter().enumerate() {
			for c in 0..4 {
				wide[(y * dw + x) * 4 + c] = taps.iter().map(|&(s, w)| rgba[(y * sw + s) * 4 + c] as f32 * w).sum();
			}
		}
	}
	let mut out = vec![0u8; dw * dh * 4];
	for (y, taps) in down.iter().enumerate() {
		for x in 0..dw {
			for c in 0..4 {
				let v: f32 = taps.iter().map(|&(s, w)| wide[(s * dw + x) * 4 + c] * w).sum();
				out[(y * dw + x) * 4 + c] = v.round().clamp(0.0, 255.0) as u8;
			}
		}
	}
	out
}

struct Encoder {
	writer: windows::Win32::Media::MediaFoundation::IMFSinkWriter,
	stream: u32,
	frame_bytes: u32,
	time_100ns: i64,
}

impl Encoder {
	fn new(width: u32, height: u32, mp4_path: &str) -> crate::error::Result<Encoder> {
		unsafe {
			let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
		}
		foundation::ensure_started();
		unsafe {
			let writer =
				MFCreateSinkWriterFromURL(&HSTRING::from(mp4_path), None::<&IMFByteStream>, None::<&IMFAttributes>)?;

			let out = MFCreateMediaType()?;
			out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
			out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
			out.SetUINT32(&MF_MT_AVG_BITRATE, (width * height * 4).clamp(200_000, 2_000_000))?;
			out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
			out.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
			out.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(NOMINAL_FPS, 1))?;
			out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
			let stream = writer.AddStream(&out)?;

			let input = MFCreateMediaType()?;
			input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
			input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
			input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
			input.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
			input.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(NOMINAL_FPS, 1))?;
			input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
			input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
			writer.SetInputMediaType(stream, &input, None::<&IMFAttributes>)?;

			writer.BeginWriting()?;
			Ok(Encoder { writer, stream, frame_bytes: width * height * 4, time_100ns: 0 })
		}
	}

	fn write(&mut self, img: &[u8], ms: u32) -> crate::error::Result<()> {
		unsafe {
			let buffer = MFCreateMemoryBuffer(self.frame_bytes)?;
			let mut ptr: *mut u8 = std::ptr::null_mut();
			buffer.Lock(&mut ptr, None, None)?;
			let dst = std::slice::from_raw_parts_mut(ptr, self.frame_bytes as usize);
			for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(img.as_chunks::<4>().0.iter()) {
				let a = s[3] as u16;
				d[0] = (s[2] as u16 * a / 255) as u8;
				d[1] = (s[1] as u16 * a / 255) as u8;
				d[2] = (s[0] as u16 * a / 255) as u8;
				d[3] = 255;
			}
			buffer.Unlock()?;
			buffer.SetCurrentLength(self.frame_bytes)?;

			let sample = MFCreateSample()?;
			sample.AddBuffer(&buffer)?;
			let duration = ms as i64 * 10_000;
			sample.SetSampleTime(self.time_100ns)?;
			sample.SetSampleDuration(duration)?;
			self.writer.WriteSample(self.stream, &sample)?;
			self.time_100ns += duration;
		}
		Ok(())
	}

	fn finish(self) -> crate::error::Result<()> {
		unsafe { self.writer.Finalize()? };
		Ok(())
	}
}
