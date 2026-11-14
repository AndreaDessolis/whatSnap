//! The other side's video: H.264 access units from the backend's video
//! receive bridge (backend/video_receive_bridge.go), decoded with Windows' own
//! H.264 decoder (Media Foundation's CLSID_MSH264DecoderMFT) into pictures the
//! call window shows. Only the newest picture is kept: one the window hasn't
//! drawn yet is replaced, never queued, so the video never falls behind.

use std::{
	io::{Read, Write},
	net::{Shutdown, TcpStream},
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use windows::Win32::{
	Media::MediaFoundation::{
		CLSID_MSH264DecoderMFT, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
		MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE,
		MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MFCreateMediaType,
		MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFT_MESSAGE_COMMAND_FLUSH,
		MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
		MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
	},
	System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize},
};

use crate::{media::foundation, ui::wake::UiCommandSender};

/// The largest picture handed to the window; bigger video is scaled down
/// here, where it's cheap, and the window scales the rest of the way.
const MAX_SHOWN_WIDTH: u32 = 960;
const MAX_SHOWN_HEIGHT: u32 = 960;
/// How long without a picture before the video counts as stopped (the peer
/// turned the camera off, or the network dropped it).
const STOPPED_AFTER: Duration = Duration::from_secs(3);
/// What the backend reads as "my decoder failed, start from a keyframe".
const KEYFRAME_REQUEST: u8 = b'K';

/// One decoded picture, upright, as RGBA.
pub(crate) struct PeerPicture {
	pub(crate) width: u32,
	pub(crate) height: u32,
	pub(crate) rgba: Vec<u8>,
}

/// What the call window hears about the peer's video.
#[derive(Clone, Copy)]
pub enum PeerVideoEvent {
	/// A new picture is waiting in `take_picture`.
	Picture,
	/// No picture for a while: hide the video.
	Stopped,
	/// The call's video receiver is gone: hide the video for good.
	Ended(u64),
}

static LATEST: Mutex<Option<(u64, PeerPicture)>> = Mutex::new(None);
static PICTURE_POSTED: AtomicBool = AtomicBool::new(false);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// The newest picture of receiver `session`, if the window hasn't taken it.
pub(crate) fn take_picture(session: u64) -> Option<PeerPicture> {
	PICTURE_POSTED.store(false, Ordering::SeqCst);
	let mut latest = LATEST.lock().unwrap();
	match latest.take() {
		Some((s, picture)) if s == session => Some(picture),
		_ => None,
	}
}

pub(crate) fn log_line(msg: &str) {
	crate::calls::video_send::log_line(&format!("[receive] {msg}"));
}

/// The receiving side of a call's video, from `call_connected` until the call
/// ends. Dropping it stops the thread and hides the video.
pub(crate) struct PeerVideo {
	pub(crate) session: u64,
	stop: Arc<AtomicBool>,
	tcp: Arc<Mutex<Option<TcpStream>>>,
	ui_tx: UiCommandSender,
}

impl Drop for PeerVideo {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::SeqCst);
		if let Some(tcp) = self.tcp.lock().unwrap().take() {
			let _ = tcp.shutdown(Shutdown::Both);
		}
		let _ = self.ui_tx.send(crate::UiCommand::PeerVideo(PeerVideoEvent::Ended(self.session)));
	}
}

pub(crate) fn start_peer_video(port: u16, ui_tx: UiCommandSender) -> PeerVideo {
	let session = NEXT_SESSION.fetch_add(1, Ordering::SeqCst);
	let stop = Arc::new(AtomicBool::new(false));
	let tcp = Arc::new(Mutex::new(None));
	let (thread_stop, thread_tcp, thread_tx) = (Arc::clone(&stop), Arc::clone(&tcp), ui_tx.clone());
	let spawned = std::thread::Builder::new()
		.name("video receive".into())
		.spawn(move || run(port, session, thread_stop, thread_tcp, thread_tx));
	if let Err(e) = spawned {
		log_line(&format!("Couldn't start the video receive thread: {e}"));
	}
	PeerVideo { session, stop, tcp, ui_tx }
}

fn run(
	port: u16,
	session: u64,
	stop: Arc<AtomicBool>,
	tcp_slot: Arc<Mutex<Option<TcpStream>>>,
	ui_tx: UiCommandSender,
) {
	unsafe {
		let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
	}
	foundation::ensure_started();
	let mut decoder = None;
	let mut showing = false;
	let mut last_picture = Instant::now();
	let mut frames = 0u64;

	'connect: while !stop.load(Ordering::SeqCst) {
		let mut tcp = match TcpStream::connect(("127.0.0.1", port)) {
			Ok(tcp) => tcp,
			Err(e) => {
				log_line(&format!("Couldn't connect to the video receive bridge: {e}"));
				std::thread::sleep(Duration::from_millis(500));
				continue;
			}
		};
		let _ = tcp.set_nodelay(true);
		let _ = tcp.set_read_timeout(Some(Duration::from_millis(500)));
		match tcp.try_clone() {
			Ok(clone) => *tcp_slot.lock().unwrap() = Some(clone),
			Err(_) => break,
		}
		if stop.load(Ordering::SeqCst) {
			break;
		}

		loop {
			if stop.load(Ordering::SeqCst) {
				break 'connect;
			}
			if showing && last_picture.elapsed() > STOPPED_AFTER {
				showing = false;
				let _ = ui_tx.send(crate::UiCommand::PeerVideo(PeerVideoEvent::Stopped));
			}
			let mut header = [0u8; 12];
			match read_full(&mut tcp, &mut header, &stop) {
				Ok(true) => {}
				Ok(false) => continue,
				Err(e) => {
					if !stop.load(Ordering::SeqCst) {
						log_line(&format!("Video receive connection lost: {e}"));
						std::thread::sleep(Duration::from_millis(200));
					}
					continue 'connect;
				}
			}
			let len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
			let orientation = header[8] & 3;
			let screen = header[9] & 1 != 0;
			if len > 8 << 20 {
				log_line(&format!("Video frame of {len} bytes is too big, reconnecting"));
				continue 'connect;
			}
			let mut unit = vec![0u8; len];
			if let Err(e) = read_payload(&mut tcp, &mut unit, &stop) {
				log_line(&format!("Video receive connection lost mid-frame: {e}"));
				continue 'connect;
			}

			if decoder.is_none() {
				match Decoder::new() {
					Ok(d) => decoder = Some(d),
					Err(e) => {
						log_line(&format!("Couldn't create the H.264 decoder: {e}"));
						break 'connect;
					}
				}
			}
			let d = decoder.as_mut().unwrap();
			match d.decode(&unit) {
				Ok(Some(picture)) => {
					frames += 1;
					if frames == 1 {
						log_line(&format!(
							"First peer video picture: {}x{} (screen: {screen})",
							picture.width, picture.height
						));
					}
					let shown = to_rgba(&picture, orientation);
					*LATEST.lock().unwrap() = Some((session, shown));
					last_picture = Instant::now();
					showing = true;
					if !PICTURE_POSTED.swap(true, Ordering::SeqCst) {
						let _ = ui_tx.send(crate::UiCommand::PeerVideo(PeerVideoEvent::Picture));
					}
				}
				Ok(None) => {}
				Err(e) => {
					log_line(&format!("Decoding a {len}-byte frame failed ({e}); asking for a keyframe"));
					d.flush();
					let _ = tcp.write_all(&[KEYFRAME_REQUEST]);
				}
			}
		}
	}
	*tcp_slot.lock().unwrap() = None;
	drop(decoder);
	unsafe { CoUninitialize() };
}

/// Fills `buf`, or returns Ok(false) when nothing at all arrived before the
/// read timeout (so the caller can check `stop` and the stopped-video clock).
fn read_full(tcp: &mut TcpStream, buf: &mut [u8], stop: &AtomicBool) -> std::io::Result<bool> {
	let mut filled = 0;
	while filled < buf.len() {
		match tcp.read(&mut buf[filled..]) {
			Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
			Ok(n) => filled += n,
			Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
				if filled == 0 {
					return Ok(false);
				}
				if stop.load(Ordering::SeqCst) {
					return Err(e);
				}
			}
			Err(e) => return Err(e),
		}
	}
	Ok(true)
}

fn read_payload(tcp: &mut TcpStream, buf: &mut [u8], stop: &AtomicBool) -> std::io::Result<()> {
	let mut filled = 0;
	while filled < buf.len() {
		match read_full(tcp, &mut buf[filled..], stop)? {
			true => filled = buf.len(),
			false if stop.load(Ordering::SeqCst) => return Err(std::io::ErrorKind::Interrupted.into()),
			false => {}
		}
	}
	Ok(())
}

/// A decoded NV12 picture, cropped to what should be shown.
pub(crate) struct Nv12Picture {
	pub(crate) width: u32,
	pub(crate) height: u32,
	pub(crate) stride: usize,
	/// Rows of the luma plane in the buffer; the chroma plane follows them.
	pub(crate) plane_rows: usize,
	pub(crate) left: u32,
	pub(crate) top: u32,
	pub(crate) data: Vec<u8>,
}

struct OutputFormat {
	width: u32,
	height: u32,
	stride: usize,
	left: u32,
	top: u32,
	shown_width: u32,
	shown_height: u32,
}

pub(crate) struct Decoder {
	mft: IMFTransform,
	format: Option<OutputFormat>,
	time: i64,
}

impl Decoder {
	pub(crate) fn new() -> crate::error::Result<Self> {
		unsafe {
			let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_ALL)?;
			// Hand each picture out as soon as it's decoded instead of holding a
			// few back for reordering, which a call's video never uses.
			if let Ok(attributes) = mft.GetAttributes() {
				let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
			}
			let input = MFCreateMediaType()?;
			input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
			input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
			mft.SetInputType(0, &input, 0)?;
			let mut decoder = Decoder { mft, format: None, time: 0 };
			decoder.choose_nv12_output()?;
			decoder.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
			decoder.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
			Ok(decoder)
		}
	}

	/// Picks NV12 among the output types the decoder offers now, and learns
	/// the picture's size and layout from it (once the stream has said).
	fn choose_nv12_output(&mut self) -> crate::error::Result<()> {
		unsafe {
			let mut index = 0;
			loop {
				let available: IMFMediaType = self.mft.GetOutputAvailableType(0, index)?;
				if available.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
					self.mft.SetOutputType(0, &available, 0)?;
					self.format = output_format(&available);
					return Ok(());
				}
				index += 1;
			}
		}
	}

	/// Flushes what the decoder holds after a broken frame, so the next
	/// keyframe starts clean.
	pub(crate) fn flush(&mut self) {
		unsafe {
			let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
		}
	}

	/// Decodes one access unit; returns the newest picture it completed.
	pub(crate) fn decode(&mut self, unit: &[u8]) -> crate::error::Result<Option<Nv12Picture>> {
		let sample = unsafe { sample_from(unit, self.time)? };
		self.time += 333_333;
		let mut newest = None;
		match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
			Ok(()) => {}
			Err(e) if e.code() == MF_E_NOTACCEPTING => {
				// Output is waiting first: take it, then the input fits.
				newest = self.drain()?.or(newest);
				unsafe { self.mft.ProcessInput(0, &sample, 0)? };
			}
			Err(e) => return Err(e.into()),
		}
		Ok(self.drain()?.or(newest))
	}

	fn drain(&mut self) -> crate::error::Result<Option<Nv12Picture>> {
		let mut newest = None;
		loop {
			let info = unsafe { self.mft.GetOutputStreamInfo(0)? };
			let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
			let ours = if provides {
				None
			} else {
				let sample = unsafe { MFCreateSample()? };
				let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(1))? };
				unsafe { sample.AddBuffer(&buffer)? };
				Some(sample)
			};
			let mut data = [MFT_OUTPUT_DATA_BUFFER {
				dwStreamID: 0,
				pSample: core::mem::ManuallyDrop::new(ours),
				dwStatus: 0,
				pEvents: core::mem::ManuallyDrop::new(None),
			}];
			let mut status = 0u32;
			let result = unsafe { self.mft.ProcessOutput(0, &mut data, &mut status) };
			let produced = unsafe { core::mem::ManuallyDrop::take(&mut data[0].pSample) };
			let _events = unsafe { core::mem::ManuallyDrop::take(&mut data[0].pEvents) };
			match result {
				Ok(()) => {
					if let (Some(sample), Some(format)) = (produced, &self.format) {
						newest = Some(unsafe { picture_from(&sample, format)? });
					}
				}
				Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(newest),
				// The stream said its size (first keyframe) or changed it.
				Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.choose_nv12_output()?,
				Err(e) => return Err(e.into()),
			}
		}
	}
}

fn output_format(media_type: &IMFMediaType) -> Option<OutputFormat> {
	unsafe {
		let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
		let (width, height) = ((size >> 32) as u32, size as u32);
		if width == 0 || height == 0 {
			return None;
		}
		let stride = media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(width as i32);
		let stride = (stride.unsigned_abs() as usize).max(width as usize);
		// A 1080-line picture is coded as 1088 lines; the aperture says which
		// part is the picture.
		let mut area = MFVideoArea::default();
		let area_bytes = std::slice::from_raw_parts_mut(
			&mut area as *mut MFVideoArea as *mut u8,
			std::mem::size_of::<MFVideoArea>(),
		);
		let (left, top, shown_width, shown_height) =
			if media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, area_bytes, None).is_ok() {
				let left = area.OffsetX.value.max(0) as u32;
				let top = area.OffsetY.value.max(0) as u32;
				let w = (area.Area.cx.max(0) as u32).min(width.saturating_sub(left));
				let h = (area.Area.cy.max(0) as u32).min(height.saturating_sub(top));
				if w > 0 && h > 0 { (left, top, w, h) } else { (0, 0, width, height) }
			} else {
				(0, 0, width, height)
			};
		Some(OutputFormat { width, height, stride, left, top, shown_width, shown_height })
	}
}

unsafe fn sample_from(unit: &[u8], time: i64) -> crate::error::Result<IMFSample> {
	unsafe {
		let buffer = MFCreateMemoryBuffer(unit.len().max(1) as u32)?;
		let mut ptr: *mut u8 = std::ptr::null_mut();
		buffer.Lock(&mut ptr, None, None)?;
		std::ptr::copy_nonoverlapping(unit.as_ptr(), ptr, unit.len());
		buffer.Unlock()?;
		buffer.SetCurrentLength(unit.len() as u32)?;
		let sample = MFCreateSample()?;
		sample.AddBuffer(&buffer)?;
		sample.SetSampleTime(time)?;
		sample.SetSampleDuration(333_333)?;
		Ok(sample)
	}
}

unsafe fn picture_from(sample: &IMFSample, format: &OutputFormat) -> crate::error::Result<Nv12Picture> {
	unsafe {
		let buffer = sample.ConvertToContiguousBuffer()?;
		let mut ptr: *mut u8 = std::ptr::null_mut();
		let mut len: u32 = 0;
		buffer.Lock(&mut ptr, None, Some(&mut len))?;
		let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
		let _ = buffer.Unlock();
		// The planes may be padded below the coded height too.
		let plane_rows = (data.len() / format.stride * 2 / 3).max(format.height as usize);
		if data.len() < format.stride * plane_rows + format.stride * (format.height as usize).div_ceil(2) {
			crate::error::bail!(
				"decoded picture is {} bytes, too small for {}x{}",
				data.len(),
				format.width,
				format.height
			);
		}
		Ok(Nv12Picture {
			width: format.shown_width,
			height: format.shown_height,
			stride: format.stride,
			plane_rows,
			left: format.left,
			top: format.top,
			data,
		})
	}
}

/// NV12 to upright RGBA (BT.601, video range), turned `orientation` quarter
/// turns clockwise, and scaled down to fit MAX_SHOWN_*.
pub(crate) fn to_rgba(picture: &Nv12Picture, orientation: u8) -> PeerPicture {
	let (src_w, src_h) = (picture.width, picture.height);
	let turned = orientation % 2 == 1;
	let (up_w, up_h) = if turned { (src_h, src_w) } else { (src_w, src_h) };
	let step = (up_w.div_ceil(MAX_SHOWN_WIDTH)).max(up_h.div_ceil(MAX_SHOWN_HEIGHT)).max(1);
	let (out_w, out_h) = ((up_w / step).max(1), (up_h / step).max(1));
	let mut rgba = vec![255u8; (out_w * out_h * 4) as usize];
	let stride = picture.stride;
	let chroma_start = stride * picture.plane_rows;
	for oy in 0..out_h {
		for ox in 0..out_w {
			// The upright pixel (ux, uy) comes from source pixel (sx, sy).
			let (ux, uy) = (ox * step, oy * step);
			let (sx, sy) = match orientation % 4 {
				0 => (ux, uy),
				1 => (uy, src_h - 1 - ux),
				2 => (src_w - 1 - ux, src_h - 1 - uy),
				_ => (src_w - 1 - uy, ux),
			};
			let (x, y) = ((sx + picture.left) as usize, (sy + picture.top) as usize);
			let luma = picture.data.get(y * stride + x).copied().unwrap_or(16) as i32;
			let chroma = chroma_start + (y / 2) * stride + (x & !1);
			let u = picture.data.get(chroma).copied().unwrap_or(128) as i32;
			let v = picture.data.get(chroma + 1).copied().unwrap_or(128) as i32;
			let (c, d, e) = (luma - 16, u - 128, v - 128);
			let i = ((oy * out_w + ox) * 4) as usize;
			rgba[i] = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
			rgba[i + 1] = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
			rgba[i + 2] = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
		}
	}
	PeerPicture { width: out_w, height: out_h, rgba }
}

#[cfg(test)]
mod tests {
	use super::*;

	fn nv12(width: u32, height: u32, pixel: impl Fn(u32, u32) -> (u8, u8, u8)) -> Nv12Picture {
		let stride = width as usize;
		let mut data = vec![0u8; stride * height as usize * 3 / 2];
		for y in 0..height {
			for x in 0..width {
				let (l, u, v) = pixel(x, y);
				data[y as usize * stride + x as usize] = l;
				let c = stride * height as usize + (y as usize / 2) * stride + (x as usize & !1);
				data[c] = u;
				data[c + 1] = v;
			}
		}
		Nv12Picture { width, height, stride, plane_rows: height as usize, left: 0, top: 0, data }
	}

	#[test]
	fn colours_convert_from_video_range() {
		// BT.601 video-range red, green, blue, white and black.
		for ((l, u, v), want) in [
			((81, 90, 240), [255, 0, 0]),
			((145, 54, 34), [0, 255, 0]),
			((41, 240, 110), [0, 0, 255]),
			((235, 128, 128), [255, 255, 255]),
			((16, 128, 128), [0, 0, 0]),
		] {
			let shown = to_rgba(&nv12(2, 2, |_, _| (l, u, v)), 0);
			for (got, want) in shown.rgba[..3].iter().zip(want) {
				assert!((*got as i32 - want).abs() <= 3, "({l},{u},{v}) -> {:?}, want {want:?}", &shown.rgba[..3]);
			}
		}
	}

	#[test]
	fn a_quarter_turn_puts_the_picture_upright() {
		// A 4x2 picture, bright on its left half. A phone held upright sends
		// it lying on its side with orientation 1: turned a quarter clockwise
		// it's 2x4, bright on top.
		let picture = nv12(4, 2, |x, _| (if x < 2 { 235 } else { 16 }, 128, 128));
		let shown = to_rgba(&picture, 1);
		assert_eq!((shown.width, shown.height), (2, 4));
		let bright = |x: u32, y: u32| shown.rgba[((y * 2 + x) * 4) as usize] > 128;
		assert!(bright(0, 0) && bright(1, 0) && bright(0, 1) && bright(1, 1), "top half should be bright");
		assert!(!bright(0, 2) && !bright(1, 3), "bottom half should be dark");
		let upside_down = to_rgba(&picture, 2);
		assert!(upside_down.rgba[0] < 128, "half a turn puts the bright half on the right");
	}

	#[test]
	fn big_pictures_are_scaled_down() {
		let shown = to_rgba(&nv12(1920, 1080, |_, _| (128, 128, 128)), 0);
		assert!(shown.width <= MAX_SHOWN_WIDTH && shown.height <= MAX_SHOWN_HEIGHT);
		assert_eq!(shown.width * 1080 / 1920, shown.height, "aspect kept");
	}

	/// Our own encoder's H.264 through the decoder: the size and colour that
	/// went in come out.
	#[test]
	fn decodes_what_the_encoder_sends() {
		unsafe {
			let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
		}
		foundation::ensure_started();
		let units = crate::calls::video_send::encode_solid_frames_for_test((81, 90, 240), 6);
		assert!(!units.is_empty(), "the encoder produced nothing");
		let mut decoder = Decoder::new().unwrap();
		let mut last = None;
		for unit in &units {
			if let Some(picture) = decoder.decode(unit).unwrap() {
				last = Some(picture);
			}
		}
		let picture = last.expect("no picture decoded");
		assert_eq!((picture.width, picture.height), (1280, 720));
		let shown = to_rgba(&picture, 0);
		let middle = (((shown.height / 2) * shown.width + shown.width / 2) * 4) as usize;
		let [r, g, b] = [shown.rgba[middle], shown.rgba[middle + 1], shown.rgba[middle + 2]];
		assert!(r > 200 && g < 60 && b < 60, "decoded red came out as {r},{g},{b}");
	}
}
