//! libopus 1.6.1, built from third_party/opus by build.rs with Deep PLC on:
//! the neural packet-loss concealment from the Opus developers (BSD, weights
//! in the source). A decoder set to complexity 5 or more uses it whenever it
//! is asked to fill in for a missing packet.
//!
//! The same small API the `opus` crate had, so the voice-message and call
//! encoders read as before.

use std::{ffi::c_int, fmt};

#[repr(C)]
struct OpusEncoder {
	_private: [u8; 0],
}
#[repr(C)]
struct OpusDecoder {
	_private: [u8; 0],
}

unsafe extern "C" {
	fn opus_encoder_create(fs: i32, channels: c_int, application: c_int, error: *mut c_int) -> *mut OpusEncoder;
	fn opus_encoder_destroy(st: *mut OpusEncoder);
	fn opus_encode(st: *mut OpusEncoder, pcm: *const i16, frame_size: c_int, data: *mut u8, max: i32) -> i32;
	fn opus_encode_float(st: *mut OpusEncoder, pcm: *const f32, frame_size: c_int, data: *mut u8, max: i32) -> i32;
	fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
	fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
	fn opus_decoder_destroy(st: *mut OpusDecoder);
	fn opus_decode(
		st: *mut OpusDecoder,
		data: *const u8,
		len: i32,
		pcm: *mut i16,
		frame_size: c_int,
		decode_fec: c_int,
	) -> c_int;
	fn opus_decode_float(
		st: *mut OpusDecoder,
		data: *const u8,
		len: i32,
		pcm: *mut f32,
		frame_size: c_int,
		decode_fec: c_int,
	) -> c_int;
	fn opus_decoder_ctl(st: *mut OpusDecoder, request: c_int, ...) -> c_int;
	fn opus_packet_get_nb_samples(packet: *const u8, len: i32, fs: i32) -> c_int;
	fn opus_strerror(error: c_int) -> *const std::ffi::c_char;
}

const OPUS_APPLICATION_VOIP: c_int = 2048;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;
const OPUS_SET_OSCE_BWE_REQUEST: c_int = 4054;
#[cfg(test)]
const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
const OPUS_SET_MAX_BANDWIDTH_REQUEST: c_int = 4004;
const OPUS_SET_DTX_REQUEST: c_int = 4016;
const OPUS_BANDWIDTH_SUPERWIDEBAND: i32 = 1104;
const OPUS_BANDWIDTH_FULLBAND: i32 = 1105;
#[cfg(test)]
const OPUS_SIGNAL_VOICE: i32 = 3001;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Error(c_int);

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let text = unsafe { std::ffi::CStr::from_ptr(opus_strerror(self.0)) };
		write!(f, "libopus: {}", text.to_string_lossy())
	}
}

impl std::error::Error for Error {}

fn check(code: c_int) -> Result<usize, Error> {
	if code < 0 { Err(Error(code)) } else { Ok(code as usize) }
}

#[derive(Clone, Copy)]
pub(crate) enum Channels {
	Mono = 1,
	Stereo = 2,
}

#[derive(Clone, Copy)]
pub(crate) enum Application {
	Voip,
}

pub(crate) enum Bitrate {
	Bits(i32),
}

pub(crate) struct Encoder(*mut OpusEncoder);

// The encoder is only ever used from one thread at a time (moved, not shared).
unsafe impl Send for Encoder {}

impl Encoder {
	pub(crate) fn new(sample_rate: u32, channels: Channels, _application: Application) -> Result<Self, Error> {
		let mut error = 0;
		let st =
			unsafe { opus_encoder_create(sample_rate as i32, channels as c_int, OPUS_APPLICATION_VOIP, &mut error) };
		if st.is_null() || error < 0 {
			return Err(Error(error.min(-1)));
		}
		Ok(Self(st))
	}

	fn set(&mut self, request: c_int, value: i32) -> Result<(), Error> {
		check(unsafe { opus_encoder_ctl(self.0, request, value) }).map(|_| ())
	}

	pub(crate) fn set_bitrate(&mut self, bitrate: Bitrate) -> Result<(), Error> {
		let Bitrate::Bits(bits) = bitrate;
		self.set(OPUS_SET_BITRATE_REQUEST, bits)
	}

	pub(crate) fn set_complexity(&mut self, complexity: i32) -> Result<(), Error> {
		self.set(OPUS_SET_COMPLEXITY_REQUEST, complexity)
	}

	pub(crate) fn set_inband_fec(&mut self, on: bool) -> Result<(), Error> {
		self.set(OPUS_SET_INBAND_FEC_REQUEST, on as i32)
	}

	pub(crate) fn set_packet_loss_perc(&mut self, percent: i32) -> Result<(), Error> {
		self.set(OPUS_SET_PACKET_LOSS_PERC_REQUEST, percent)
	}

	/// Tells the encoder the input is speech, so it stays on SILK.
	#[cfg(test)]
	pub(crate) fn set_signal_voice(&mut self) -> Result<(), Error> {
		self.set(OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE)
	}

	/// Caps the audio bandwidth: super-wideband (12 kHz) below a 48 kHz
	/// call rate, fullband (20 kHz) at it.
	pub(crate) fn set_max_bandwidth_for_rate(&mut self, rate: u32) -> Result<(), Error> {
		let bandwidth = if rate >= 48000 { OPUS_BANDWIDTH_FULLBAND } else { OPUS_BANDWIDTH_SUPERWIDEBAND };
		self.set(OPUS_SET_MAX_BANDWIDTH_REQUEST, bandwidth)
	}

	/// Discontinuous transmission: in silence the encoder emits a one or two
	/// byte packet every 400 ms and nothing between.
	pub(crate) fn set_dtx(&mut self, on: bool) -> Result<(), Error> {
		self.set(OPUS_SET_DTX_REQUEST, on as i32)
	}

	pub(crate) fn get_lookahead(&mut self) -> Result<i32, Error> {
		let mut value: i32 = 0;
		check(unsafe { opus_encoder_ctl(self.0, OPUS_GET_LOOKAHEAD_REQUEST, &mut value as *mut i32) })?;
		Ok(value)
	}

	pub(crate) fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, Error> {
		check(unsafe { opus_encode(self.0, pcm.as_ptr(), pcm.len() as c_int, out.as_mut_ptr(), out.len() as i32) })
	}

	pub(crate) fn encode_float(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, Error> {
		check(unsafe {
			opus_encode_float(self.0, pcm.as_ptr(), pcm.len() as c_int, out.as_mut_ptr(), out.len() as i32)
		})
	}
}

impl Drop for Encoder {
	fn drop(&mut self) {
		unsafe { opus_encoder_destroy(self.0) };
	}
}

pub(crate) struct Decoder {
	st: *mut OpusDecoder,
	channels: usize,
}

unsafe impl Send for Decoder {}

impl Decoder {
	pub(crate) fn new(sample_rate: u32, channels: Channels) -> Result<Self, Error> {
		let mut error = 0;
		let st = unsafe { opus_decoder_create(sample_rate as i32, channels as c_int, &mut error) };
		if st.is_null() || error < 0 {
			return Err(Error(error.min(-1)));
		}
		Ok(Self { st, channels: channels as usize })
	}

	/// Complexity 5 or more turns on Deep PLC for this decoder.
	pub(crate) fn set_complexity(&mut self, complexity: i32) -> Result<(), Error> {
		check(unsafe { opus_decoder_ctl(self.st, OPUS_SET_COMPLEXITY_REQUEST, complexity) }).map(|_| ())
	}

	/// The blind bandwidth extension: a 48 kHz decoder (complexity 4 or more)
	/// makes up the band above 8 kHz that wideband SILK leaves out.
	pub(crate) fn set_bandwidth_extension(&mut self, on: bool) -> Result<(), Error> {
		check(unsafe { opus_decoder_ctl(self.st, OPUS_SET_OSCE_BWE_REQUEST, on as i32) }).map(|_| ())
	}

	pub(crate) fn decode(&mut self, packet: &[u8], out: &mut [i16], fec: bool) -> Result<usize, Error> {
		let (data, len) =
			if packet.is_empty() { (std::ptr::null(), 0) } else { (packet.as_ptr(), packet.len() as i32) };
		check(unsafe {
			opus_decode(self.st, data, len, out.as_mut_ptr(), (out.len() / self.channels) as c_int, fec as c_int)
		})
	}

	/// Decodes `packet` into `out`; an empty packet asks for concealment of
	/// `out`'s length, and `fec` decodes the previous (lost) packet from the
	/// redundancy carried in this one.
	pub(crate) fn decode_float(&mut self, packet: &[u8], out: &mut [f32], fec: bool) -> Result<usize, Error> {
		let (data, len) =
			if packet.is_empty() { (std::ptr::null(), 0) } else { (packet.as_ptr(), packet.len() as i32) };
		check(unsafe {
			opus_decode_float(self.st, data, len, out.as_mut_ptr(), (out.len() / self.channels) as c_int, fec as c_int)
		})
	}
}

impl Drop for Decoder {
	fn drop(&mut self) {
		unsafe { opus_decoder_destroy(self.st) };
	}
}

pub(crate) mod packet {
	use super::*;

	/// How many samples (per channel) `packet` holds at `sample_rate`.
	pub(crate) fn get_nb_samples(packet: &[u8], sample_rate: u32) -> Result<usize, Error> {
		check(unsafe { opus_packet_get_nb_samples(packet.as_ptr(), packet.len() as i32, sample_rate as i32) })
	}
}
