use windows::{
	Win32::{
		Media::MediaFoundation::{
			IMF2DBuffer, IMFSample, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
			MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
			MF_SOURCE_READERF_ENDOFSTREAM, MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL,
			MFMediaType_Video, MFVideoFormat_RGB32,
		},
		System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
	},
	core::{HSTRING, Interface},
};

use crate::media::foundation;

pub(crate) fn first_frame_png(path: &str) -> Option<Vec<u8>> {
	let (width, height, rgba) = first_frame_rgba(path).ok()?;
	Some(thumbnail_png(width, height, rgba))
}

// Only a preview travels in JSON. Even incompressible RGBA at this size
// stays comfortably below the backend's 1 MiB command limit after base64.
pub(super) fn thumbnail_png(width: u32, height: u32, rgba: Vec<u8>) -> Vec<u8> {
	let edge = width.max(height).max(1);
	let size = if edge > 256 {
		((width as u64 * 256 / edge as u64).max(1) as u32, (height as u64 * 256 / edge as u64).max(1) as u32)
	} else {
		(width, height)
	};
	let rgba = crate::media::gif_video::resize(rgba, (width, height), size);
	crate::media::png::encode_rgba(size.0, size.1, &rgba)
}

fn first_frame_rgba(path: &str) -> crate::error::Result<(u32, u32, Vec<u8>)> {
	unsafe {
		let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
	}
	foundation::ensure_started();
	unsafe {
		let mut attrs: Option<windows::Win32::Media::MediaFoundation::IMFAttributes> = None;
		MFCreateAttributes(&mut attrs, 1)?;
		let attrs = attrs.ok_or_else(|| crate::error::err!("MFCreateAttributes returned no attributes"))?;
		attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
		let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path), &attrs)?;

		let rgb = MFCreateMediaType()?;
		rgb.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
		rgb.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
		let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
		reader.SetCurrentMediaType(stream, None, &rgb)?;
		let current = reader.GetCurrentMediaType(stream)?;
		let size = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
		let (width, height) = ((size >> 32) as u32, size as u32);
		if width == 0 || height == 0 {
			crate::error::bail!("video reports no frame size");
		}

		for _ in 0..30 {
			let mut flags = 0u32;
			let mut sample: Option<IMFSample> = None;
			reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))?;
			if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
				break;
			}
			if let Some(sample) = sample {
				return Ok((width, height, sample_to_rgba(&sample, width, height)?));
			}
		}
		crate::error::bail!("no frame decoded")
	}
}

unsafe fn sample_to_rgba(sample: &IMFSample, width: u32, height: u32) -> crate::error::Result<Vec<u8>> {
	let (w, h) = (width as usize, height as usize);
	let mut rgba = vec![0u8; w * h * 4];
	let buffer = unsafe { sample.GetBufferByIndex(0)? };
	let copy_rows = |top: *const u8, pitch: isize, rgba: &mut [u8]| {
		for y in 0..h {
			let row = unsafe { std::slice::from_raw_parts(top.offset(pitch * y as isize), w * 4) };
			for (x, px) in row.as_chunks::<4>().0.iter().enumerate() {
				let o = (y * w + x) * 4;
				rgba[o] = px[2];
				rgba[o + 1] = px[1];
				rgba[o + 2] = px[0];
				rgba[o + 3] = 255;
			}
		}
	};
	if let Ok(buffer_2d) = buffer.cast::<IMF2DBuffer>() {
		let mut top: *mut u8 = std::ptr::null_mut();
		let mut pitch: i32 = 0;
		unsafe { buffer_2d.Lock2D(&mut top, &mut pitch)? };
		if !top.is_null() && pitch.unsigned_abs() as usize >= w * 4 {
			copy_rows(top, pitch as isize, &mut rgba);
		}
		let _ = unsafe { buffer_2d.Unlock2D() };
		return Ok(rgba);
	}
	let contiguous = unsafe { sample.ConvertToContiguousBuffer()? };
	let mut ptr: *mut u8 = std::ptr::null_mut();
	let mut len: u32 = 0;
	unsafe { contiguous.Lock(&mut ptr, None, Some(&mut len))? };
	if !ptr.is_null() && len as usize >= w * h * 4 {
		copy_rows(ptr, (w * 4) as isize, &mut rgba);
	}
	let _ = unsafe { contiguous.Unlock() };
	Ok(rgba)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn large_noisy_preview_fits_in_ipc_command() {
		let mut seed = 42u32;
		let rgba: Vec<u8> = (0..1024 * 768 * 4)
			.map(|_| {
				seed ^= seed << 13;
				seed ^= seed >> 17;
				seed ^= seed << 5;
				seed as u8
			})
			.collect();
		let png = thumbnail_png(1024, 768, rgba);
		assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 256);
		assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 192);
		assert!(crate::util::base64(&png).len() < 400_000);
	}
}
