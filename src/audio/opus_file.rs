//! Plays and measures Ogg Opus files a page at a time, so a long recording
//! never sits in memory decoded as a whole (an hour of stereo is over a
//! gigabyte of samples). The bundled tones still use `decode_opus`.

use std::{
	fs::File,
	io::{BufReader, Read, Seek, SeekFrom},
	num::{NonZeroU16, NonZeroU32},
	time::Duration,
};

use rodio::Source;

use crate::audio::libopus::{Channels, Decoder};

const RATE: u32 = 48_000;
/// The largest Opus frame, 120 ms at 48 kHz, per channel.
const MAX_FRAME: usize = 5760;

struct OggPackets {
	reader: BufReader<File>,
	pending: Vec<u8>,
}

impl OggPackets {
	/// Reads the next page and hands over each packet it completes. `None` at
	/// the end of the file (or where it stops making sense).
	fn next_page(&mut self, mut packet: impl FnMut(&[u8])) -> Option<()> {
		let mut header = [0u8; 27];
		self.reader.read_exact(&mut header).ok()?;
		// Out of step: slide forward a byte at a time to the next page.
		while &header[0..4] != b"OggS" {
			header.copy_within(1.., 0);
			self.reader.read_exact(&mut header[26..]).ok()?;
		}
		let flags = header[5];
		let mut lacing = vec![0u8; header[26] as usize];
		self.reader.read_exact(&mut lacing).ok()?;
		let body_len: usize = lacing.iter().map(|&b| b as usize).sum();
		let mut body = vec![0u8; body_len];
		self.reader.read_exact(&mut body).ok()?;

		let starts_with_continued = flags & 0x01 != 0;
		if !starts_with_continued {
			self.pending.clear();
		}
		let (mut offset, mut run_start, mut first_of_page) = (0usize, 0usize, true);
		for (i, &seg_len) in lacing.iter().enumerate() {
			offset += seg_len as usize;
			if seg_len < 255 {
				let bytes = &body[run_start..offset];
				run_start = offset;
				let continuation = first_of_page && starts_with_continued;
				first_of_page = false;
				if continuation {
					if self.pending.is_empty() {
						continue;
					}
					self.pending.extend_from_slice(bytes);
					packet(&self.pending);
					self.pending.clear();
				} else {
					packet(bytes);
				}
			} else if i + 1 == lacing.len() {
				self.pending.extend_from_slice(&body[run_start..offset]);
			}
		}
		Some(())
	}
}

/// How much is decoded ahead of a seek target and thrown away, so the
/// decoder has settled by the time the target plays: 80 ms, as RFC 7845
/// asks for.
const PREROLL: u64 = 3840;

/// The granule position and body length of the page at the reader, which is
/// left at the start of the body. `None` at the end or on a broken page.
fn read_page_header(reader: &mut BufReader<File>) -> Option<(i64, usize)> {
	let mut header = [0u8; 27];
	reader.read_exact(&mut header).ok()?;
	if &header[0..4] != b"OggS" {
		return None;
	}
	let granule = i64::from_le_bytes(header[6..14].try_into().ok()?);
	let mut lacing = vec![0u8; header[26] as usize];
	reader.read_exact(&mut lacing).ok()?;
	Some((granule, lacing.iter().map(|&b| b as usize).sum()))
}

struct OpusState {
	decoder: Option<Decoder>,
	channels: usize,
	pre_skip: usize,
	skipped: usize,
	pcm: Vec<i16>,
}

impl OpusState {
	fn feed(&mut self, packet: &[u8], out: &mut Vec<f32>) {
		if packet.starts_with(b"OpusHead") {
			let header_channels = if packet.len() >= 10 { (packet[9] as usize).max(1) } else { 1 };
			// Decoded as mono or stereo only, so that's what the samples are.
			self.channels = header_channels.min(2);
			self.pre_skip = if packet.len() >= 12 { u16::from_le_bytes([packet[10], packet[11]]) as usize } else { 0 };
			let channels = if self.channels >= 2 { Channels::Stereo } else { Channels::Mono };
			self.decoder = Decoder::new(RATE, channels).ok();
			self.skipped = 0;
			return;
		}
		if packet.starts_with(b"OpusTags") {
			return;
		}
		let Some(decoder) = self.decoder.as_mut() else { return };
		let ch = self.channels.max(1);
		self.pcm.resize(MAX_FRAME * ch, 0);
		if let Ok(n) = decoder.decode(packet, &mut self.pcm, false) {
			let mut decoded = &self.pcm[..n * ch];
			if self.skipped < self.pre_skip {
				let to_skip = (self.pre_skip - self.skipped).min(n);
				decoded = &decoded[to_skip * ch..];
				self.skipped += to_skip;
			}
			out.extend(decoded.iter().map(|&s| s as f32 / 32768.0));
		}
	}
}

pub(crate) struct OpusFileSource {
	packets: OggPackets,
	state: OpusState,
	buf: Vec<f32>,
	at: usize,
	duration: Option<Duration>,
	ended: bool,
}

impl OpusFileSource {
	pub(crate) fn open(path: &str) -> Option<OpusFileSource> {
		let file = File::open(path).ok()?;
		let mut source = OpusFileSource {
			packets: OggPackets { reader: BufReader::with_capacity(64 * 1024, file), pending: Vec::new() },
			state: OpusState { decoder: None, channels: 1, pre_skip: 0, skipped: 0, pcm: Vec::new() },
			buf: Vec::new(),
			at: 0,
			duration: None,
			ended: false,
		};
		// The identification header comes first; with it the format is known.
		while source.state.decoder.is_none() {
			let OpusFileSource { packets, state, buf, .. } = &mut source;
			packets.next_page(|p| state.feed(p, buf))?;
		}
		source.duration = granule_duration(path, source.state.pre_skip);
		Some(source)
	}

	/// Opens the file at `start`: skips whole pages by their granule
	/// positions without decoding them, then decodes from a little before
	/// `start` (so the decoder settles) and drops the samples up to it. A
	/// seek in a long voice message used to decode everything before it, on
	/// the audio thread, which froze the sound for a moment on every press.
	pub(crate) fn open_at(path: &str, start: Duration) -> Option<OpusFileSource> {
		let mut source = Self::open(path)?;
		let target = (start.as_nanos() * RATE as u128 / 1_000_000_000) as u64;
		if target == 0 {
			return Some(source);
		}
		let pre_skip = source.state.pre_skip as u64;
		let want = target + pre_skip;
		let resume_at = want.saturating_sub(PREROLL);
		// The last page that ends at or before `resume_at`: decoding starts
		// with the page after it, at its granule.
		let mut resume = None;
		let reader = &mut source.packets.reader;
		let headers_end = reader.stream_position().ok()?;
		while let Some((granule, body_len)) = read_page_header(reader) {
			if granule >= 0 && granule as u64 > resume_at {
				break;
			}
			if reader.seek_relative(body_len as i64).is_err() {
				break;
			}
			if granule >= 0 {
				resume = Some((reader.stream_position().ok()?, granule as u64));
			}
		}
		let (offset, decoded_from) = resume.unwrap_or((headers_end, 0));
		reader.seek(SeekFrom::Start(offset)).ok()?;
		source.packets.pending.clear();
		// The pre-skip samples before `decoded_from` were never decoded, so
		// only the rest of them is still to drop.
		source.state.skipped = (decoded_from as usize).min(source.state.pre_skip);
		let first = decoded_from.max(pre_skip);
		let channels = source.state.channels.max(1);
		let mut drop = (want.saturating_sub(first) as usize) * channels;
		while drop > 0 {
			if source.at == source.buf.len() && !source.refill() {
				break;
			}
			let take = drop.min(source.buf.len() - source.at);
			source.at += take;
			drop -= take;
		}
		Some(source)
	}

	fn refill(&mut self) -> bool {
		self.buf.clear();
		self.at = 0;
		while self.buf.is_empty() && !self.ended {
			let OpusFileSource { packets, state, buf, .. } = self;
			if packets.next_page(|p| state.feed(p, buf)).is_none() {
				self.ended = true;
			}
		}
		!self.buf.is_empty()
	}
}

impl Iterator for OpusFileSource {
	type Item = f32;

	fn next(&mut self) -> Option<f32> {
		if self.at == self.buf.len() && !self.refill() {
			return None;
		}
		self.at += 1;
		Some(self.buf[self.at - 1])
	}
}

impl Source for OpusFileSource {
	fn current_span_len(&self) -> Option<usize> {
		None
	}

	fn channels(&self) -> rodio::ChannelCount {
		NonZeroU16::new(self.state.channels.max(1) as u16).unwrap_or(NonZeroU16::MIN)
	}

	fn sample_rate(&self) -> rodio::SampleRate {
		NonZeroU32::new(RATE).unwrap_or(NonZeroU32::MIN)
	}

	fn total_duration(&self) -> Option<Duration> {
		self.duration
	}
}

/// The length from the last page's granule position, read off the end of
/// the file, as players do, instead of decoding the whole thing.
fn granule_duration(path: &str, pre_skip: usize) -> Option<Duration> {
	let mut file = File::open(path).ok()?;
	let len = file.metadata().ok()?.len();
	let start = len.saturating_sub(64 * 1024);
	file.seek(SeekFrom::Start(start)).ok()?;
	let mut tail = Vec::new();
	file.read_to_end(&mut tail).ok()?;
	let mut end = tail.len();
	while let Some(at) = tail[..end].windows(4).rposition(|w| w == b"OggS") {
		if let Some(bytes) = tail.get(at + 6..at + 14)
			&& tail.get(at + 4) == Some(&0)
		{
			let granule = i64::from_le_bytes(bytes.try_into().ok()?);
			if granule >= 0 {
				let samples = (granule as u64).saturating_sub(pre_skip as u64);
				return Some(Duration::from_nanos(samples * 1_000_000_000 / RATE as u64));
			}
		}
		end = at;
	}
	None
}

/// The length of an Ogg Opus file: from its last granule position, or by
/// decoding (without keeping the samples) when that's missing.
pub(crate) fn duration(path: &str) -> Option<Duration> {
	let source = OpusFileSource::open(path)?;
	if let Some(d) = source.duration.filter(|d| !d.is_zero()) {
		return Some(d);
	}
	let channels = source.state.channels.max(1) as u64;
	let samples = source.count() as u64 / channels;
	(samples > 0).then(|| Duration::from_nanos(samples * 1_000_000_000 / RATE as u64))
}

#[cfg(test)]
mod tests {
	#[test]
	fn a_file_streams_the_same_samples_as_decoding_it_whole() {
		let dir = std::env::temp_dir().join(format!("opus_file_test_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("tone.ogg");
		let data = crate::types::NotificationSound::MessageCurrent.ogg();
		std::fs::write(&path, data).unwrap();
		let path = path.to_string_lossy().to_string();

		let whole: Vec<f32> = super::super::decode_opus(data).unwrap().collect();
		let streamed: Vec<f32> = super::OpusFileSource::open(&path).unwrap().collect();
		assert_eq!(whole.len(), streamed.len());
		assert!(whole.iter().zip(&streamed).all(|(a, b)| a == b));

		let expected = whole.len() as f64 / 48_000.0 / {
			use rodio::Source;
			super::super::decode_opus(data).unwrap().channels().get() as f64
		};
		let measured = super::duration(&path).unwrap().as_secs_f64();
		assert!((measured - expected).abs() < 0.05, "{measured} vs {expected}");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn opening_at_a_time_leaves_the_rest_of_the_file() {
		let dir = std::env::temp_dir().join(format!("opus_seek_test_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("tone.ogg");
		let data = crate::types::NotificationSound::MessageCurrent.ogg();
		std::fs::write(&path, data).unwrap();
		let path = path.to_string_lossy().to_string();

		let source = super::OpusFileSource::open(&path).unwrap();
		let channels = source.state.channels.max(1);
		let whole: Vec<f32> = source.collect();
		let frames = whole.len() / channels;
		for at_frame in [0, frames / 4, frames / 2, frames - 1] {
			let start = std::time::Duration::from_nanos(at_frame as u64 * 1_000_000_000 / 48_000);
			let rest = super::OpusFileSource::open_at(&path, start).unwrap().count() / channels;
			let expected = frames - at_frame;
			assert!(rest.abs_diff(expected) <= 1, "at {at_frame}: {rest} left, expected {expected}");
		}
		let past_end = std::time::Duration::from_secs(3600);
		assert_eq!(super::OpusFileSource::open_at(&path, past_end).unwrap().count(), 0);
		let _ = std::fs::remove_dir_all(&dir);
	}
}
