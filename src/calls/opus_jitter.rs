//! The peer's standard Opus, decoded here at playout time instead of in the
//! backend, the way WhatsApp's NetEQ does it: packets are kept in RTP
//! timestamp order and decoded into the playout queue as soon as they are
//! next in line, and when the playout runs dry the decoder itself fills in:
//!
//! - a packet that is only late is covered by libopus's concealment (Deep PLC,
//!   the neural one, see audio/libopus.rs) without giving up on it: when it
//!   arrives it is played after what was made up, and accelerate trims the
//!   delay that leaves;
//! - a packet that is lost (later ones are already here) is rebuilt from the
//!   in-band FEC the next packet carries, and what that can't cover is
//!   concealed, less whatever was already made up while waiting for it;
//! - a packet whose time has been fully made up already is dropped;
//! - a jump in timestamps with no gap in the sequence numbers is the peer
//!   not sending (DTX while it is silent, or while it is busy with video),
//!   not loss, so nothing is rebuilt or made up for it. WhatsApp's own jitter
//!   buffer tells the two apart the same way: it has a DTX state of its own,
//!   with comfort noise counted apart from concealment (`jb_plc_cng`).
//!
//! Decoded at 48 kHz with libopus's blind bandwidth extension, which makes up
//! the band above 8 kHz that a 16 kHz call leaves out (WhatsApp's engine does
//! the same for MLow calls with its "bwenet"), and LACE/NoLACE speech
//! enhancement. Time is still counted on the RTP clock, 16 kHz.
//!
//! Stereo packets (another whatSnap sending stereo, an experimental setting)
//! are decoded a second time by a plain stereo decoder for their side
//! ((left - right) / 2); the mid stays the enhanced mono decoder's.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::audio::libopus::{Channels, Decoder};

/// WhatsApp's RTP clock for audio; every count below is in its samples.
const RTP_RATE: u32 = 16000;
/// The most one Opus packet holds: 120 ms.
const MAX_PACKET_SAMPLES: usize = RTP_RATE as usize * 120 / 1000;
/// Concealment is made 10 ms at a time while waiting for a late packet.
const CONCEAL_STEP: usize = 160;
/// Opus frame sizes are multiples of 2.5 ms.
const OPUS_GRAIN: usize = 40;
/// After this much concealment with nothing arriving, play silence instead:
/// the peer has stopped sending (muted, DTX), it isn't a late packet.
const MAX_CONCEALMENT: usize = 1600;
/// A jump in timestamps bigger than this is a discontinuity (the peer paused
/// sending, or restarted), not loss: nothing is made up for it.
const MAX_LOSS: u64 = 6400;
/// How many of the latest packets' sequence numbers are remembered to tell
/// reordering and silence from loss.
const RECENT_SEQUENCES: usize = 64;
/// The stereo decoder is let go after this many mono packets in a row (30 s).
const STEREO_IDLE_PACKETS: u32 = 500;

pub(crate) struct OpusJitter {
	decoder: Decoder,
	/// Deep PLC, NoLACE and the bandwidth extension are on.
	enhanced: bool,
	/// Output samples per RTP sample: 3 at 48 kHz.
	scale: usize,
	/// Waiting packets by extended (unwrapped) RTP timestamp.
	packets: BTreeMap<u64, Vec<u8>>,
	/// The latest packets' sequence numbers with their extended timestamps.
	recent: VecDeque<(u16, u64)>,
	/// Timestamps of waiting packets whose sequence number follows its
	/// neighbour's: whatever the timestamps jump by before them, the peer
	/// sent nothing in between.
	unbroken: BTreeSet<u64>,
	/// The timestamp of the next sample to decode, once a packet has come.
	next: Option<u64>,
	/// Unwrapping 32-bit RTP timestamps.
	last_raw: Option<u32>,
	wraps: u64,
	/// Samples per packet, as last seen (what FEC can rebuild at most).
	packet_samples: usize,
	/// Samples made up since the last real audio: their time is covered.
	covered: usize,
	pcm: Vec<f32>,
	/// Decodes again for the side once a stereo packet came.
	stereo: Option<Decoder>,
	stereo_pcm: Vec<f32>,
	mono_packets: u32,
	/// The side of every sample written to the playout queue, in step with
	/// it: zero but for stereo packets. The playout takes it after each call.
	pub(crate) side: VecDeque<f32>,
	pub(crate) concealed: u64,
	pub(crate) recovered: u64,
	pub(crate) dropped_late: u64,
}

impl OpusJitter {
	/// Turns the decoder's own processing (Deep PLC, LACE/NoLACE speech
	/// enhancement, bandwidth extension) on or off from the next packet. Off
	/// leaves libopus's plain decoding and its basic concealment.
	pub(crate) fn set_enhanced(&mut self, on: bool) {
		if on == self.enhanced {
			return;
		}
		self.enhanced = on;
		let _ = self.decoder.set_complexity(if on { 10 } else { 0 });
		if self.scale == 3 {
			let _ = self.decoder.set_bandwidth_extension(on);
		}
	}

	/// `rate`: what to decode to, 16000 or 48000 (48 kHz adds the bandwidth
	/// extension).
	pub(crate) fn new(rate: u32) -> crate::error::Result<Self> {
		let mut decoder = Decoder::new(rate, Channels::Mono)?;
		// 5 and up turn on Deep PLC, 7 NoLACE.
		decoder.set_complexity(10)?;
		if rate == 48000 {
			decoder.set_bandwidth_extension(true)?;
		}
		let scale = (rate / RTP_RATE).max(1) as usize;
		Ok(Self {
			enhanced: true,
			decoder,
			scale,
			packets: BTreeMap::new(),
			recent: VecDeque::with_capacity(RECENT_SEQUENCES),
			unbroken: BTreeSet::new(),
			next: None,
			last_raw: None,
			wraps: 0,
			packet_samples: 960,
			covered: 0,
			pcm: vec![0.0; MAX_PACKET_SAMPLES * scale],
			stereo: None,
			stereo_pcm: vec![0.0; MAX_PACKET_SAMPLES * scale * 2],
			mono_packets: 0,
			side: VecDeque::new(),
			concealed: 0,
			recovered: 0,
			dropped_late: 0,
		})
	}

	fn extend(&mut self, raw: u32) -> u64 {
		if let Some(last) = self.last_raw {
			// Forward across the 2^32 boundary.
			if raw < last && last - raw > u32::MAX / 2 {
				self.wraps += 1;
			} else if raw > last && raw - last > u32::MAX / 2 {
				// A reordered packet from just before a wrap.
				return (self.wraps.saturating_sub(1) << 32) | raw as u64;
			}
		}
		self.last_raw = Some(raw);
		(self.wraps << 32) | raw as u64
	}

	/// A packet from the network.
	pub(crate) fn push(&mut self, sequence: u16, timestamp: u32, packet: Vec<u8>) {
		let at = self.extend(timestamp);
		self.note_sequence(sequence, at);
		if let Some(next) = self.next
			&& at < next
		{
			self.dropped_late += 1;
			return;
		}
		self.packets.insert(at, packet);
	}

	/// Remembers the packet's sequence number, and marks it (or the packet
	/// that comes right after it, if that arrived first) as unbroken.
	fn note_sequence(&mut self, sequence: u16, at: u64) {
		let before = sequence.wrapping_sub(1);
		let after = sequence.wrapping_add(1);
		for &(other, other_at) in &self.recent {
			if other == before {
				self.unbroken.insert(at);
			} else if other == after {
				self.unbroken.insert(other_at);
			}
		}
		if self.recent.len() == RECENT_SEQUENCES {
			self.recent.pop_front();
		}
		self.recent.push_back((sequence, at));
	}

	/// Drops waiting packets that start before `next`: their time was played
	/// already by the packet before them, which turned out longer than the
	/// gap to them. Left waiting, the first of them would stand in front of
	/// every later packet, so no gap after it would ever be bridged and the
	/// call would go silent for good.
	fn drop_overlapped(&mut self) {
		let Some(next) = self.next else { return };
		while let Some((&first, _)) = self.packets.first_key_value() {
			if first >= next {
				break;
			}
			self.packets.remove(&first);
			self.dropped_late += 1;
		}
	}

	/// Decodes every packet that is next in line into `out`.
	pub(crate) fn drain_ready(&mut self, out: &mut VecDeque<f32>) {
		loop {
			self.drop_overlapped();
			let next = match self.next {
				Some(next) => next,
				None => match self.packets.keys().next() {
					Some(&first) => {
						self.next = Some(first);
						first
					}
					None => return,
				},
			};
			let Some(packet) = self.packets.remove(&next) else { return };
			self.unbroken.retain(|&at| at > next);
			self.decode_packet(&packet, next, out);
		}
	}

	/// The side of a packet the mono decoder just made `n` samples of.
	fn decode_side(&mut self, packet: &[u8], n: usize) {
		let stereo_packet = packet.first().is_some_and(|toc| toc & 0x04 != 0);
		if stereo_packet {
			self.mono_packets = 0;
			if self.stereo.is_none() {
				match Decoder::new(RTP_RATE * self.scale as u32, Channels::Stereo) {
					Ok(decoder) => {
						crate::calls::audio::log_line("playout: the peer sends stereo; playing it in stereo");
						self.stereo = Some(decoder);
					}
					Err(e) => crate::calls::audio::log_line(&format!("playout: no stereo decoder: {e}")),
				}
			}
		} else {
			self.mono_packets += 1;
			if self.mono_packets > STEREO_IDLE_PACKETS && self.stereo.take().is_some() {
				crate::calls::audio::log_line("playout: the peer sends mono again");
			}
		}
		// Mono packets go through the stereo decoder too, so its state follows
		// the stream; they come out with no side.
		let decoded = match self.stereo.as_mut() {
			Some(decoder) => decoder.decode_float(packet, &mut self.stereo_pcm, false).ok(),
			None => None,
		};
		match decoded {
			Some(m) if m == n => {
				self.side.extend(self.stereo_pcm[..2 * n].chunks_exact(2).map(|lr| (lr[0] - lr[1]) * 0.5));
			}
			_ => self.side.extend(std::iter::repeat_n(0.0, n)),
		}
	}

	fn decode_packet(&mut self, packet: &[u8], at: u64, out: &mut VecDeque<f32>) {
		let samples = match self.decoder.decode_float(packet, &mut self.pcm, false) {
			Ok(n) => {
				out.extend(&self.pcm[..n]);
				self.decode_side(packet, n);
				n / self.scale
			}
			Err(_) => {
				// Not decodable: conceal its length instead, so the timeline holds.
				let n = crate::audio::libopus::packet::get_nb_samples(packet, RTP_RATE).unwrap_or(self.packet_samples);
				self.conceal_into(n, out);
				n
			}
		};
		if samples > 0 {
			self.packet_samples = samples;
		}
		self.covered = 0;
		self.next = Some(at + samples.max(1) as u64);
	}

	/// Conceals `samples` (RTP samples) of audio into `out`.
	fn conceal_into(&mut self, samples: usize, out: &mut VecDeque<f32>) {
		let mut left = samples / OPUS_GRAIN * OPUS_GRAIN;
		while left > 0 {
			let step = left.min(MAX_PACKET_SAMPLES);
			let produced = step * self.scale;
			let n = match self.decoder.decode_float(&[], &mut self.pcm[..produced], false) {
				Ok(n) if n > 0 => {
					out.extend(&self.pcm[..n]);
					n
				}
				_ => {
					out.extend(std::iter::repeat_n(0.0, produced));
					produced
				}
			};
			self.side.extend(std::iter::repeat_n(0.0, n));
			self.concealed += step as u64;
			left -= step;
		}
	}

	/// Whether any packet has come yet: before that there is nothing to
	/// conceal from.
	pub(crate) fn started(&self) -> bool {
		self.next.is_some()
	}

	/// The playout has nothing to play: fill in now. Adds at least one
	/// sample to `out` once started.
	pub(crate) fn produce(&mut self, out: &mut VecDeque<f32>) {
		self.drop_overlapped();
		let Some(next) = self.next else { return };
		match self.packets.keys().next().copied() {
			Some(first) if first > next => {
				let missing = first - next;
				if missing > MAX_LOSS || self.unbroken.contains(&first) {
					// A pause in sending, not loss (the sequence numbers run on
					// unbroken, or the jump is too big to be one): carry on from
					// the new packet.
					self.next = Some(first);
				} else {
					// Lost. Whatever was made up while waiting already took
					// part of its time; FEC in the next packet rebuilds the end.
					let missing = (missing as usize).saturating_sub(self.covered);
					let fec = missing.min(self.packet_samples) / OPUS_GRAIN * OPUS_GRAIN;
					self.conceal_into(missing - fec, out);
					if fec > 0 {
						let packet = self.packets[&first].clone();
						match self.decoder.decode_float(&packet, &mut self.pcm[..fec * self.scale], true) {
							Ok(n) if n > 0 => {
								out.extend(&self.pcm[..n]);
								self.side.extend(std::iter::repeat_n(0.0, n));
								self.recovered += (n / self.scale) as u64;
							}
							_ => self.conceal_into(fec, out),
						}
					}
					self.next = Some(first);
				}
				self.covered = 0;
				self.drain_ready(out);
			}
			Some(_) => self.drain_ready(out),
			None => {
				// Late: make up 10 ms and keep waiting for it.
				if self.covered < MAX_CONCEALMENT {
					self.conceal_into(CONCEAL_STEP, out);
				} else {
					out.extend(std::iter::repeat_n(0.0, CONCEAL_STEP * self.scale));
					self.side.extend(std::iter::repeat_n(0.0, CONCEAL_STEP * self.scale));
				}
				self.covered += CONCEAL_STEP;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::audio::libopus::{Application, Bitrate, Encoder};

	fn voice(at: usize) -> f32 {
		let t = at as f32 / RTP_RATE as f32;
		0.3 * (2.0 * std::f32::consts::PI * 210.0 * t).sin() + 0.1 * (2.0 * std::f32::consts::PI * 630.0 * t).sin()
	}

	/// 60 ms packets the way the call encoder makes them, with FEC on.
	fn packets(count: usize) -> Vec<(u32, Vec<u8>)> {
		let mut encoder = Encoder::new(RTP_RATE, Channels::Mono, Application::Voip).unwrap();
		encoder.set_bitrate(Bitrate::Bits(24000)).unwrap();
		encoder.set_inband_fec(true).unwrap();
		encoder.set_packet_loss_perc(20).unwrap();
		(0..count)
			.map(|n| {
				let pcm: Vec<f32> = (n * 960..(n + 1) * 960).map(voice).collect();
				let mut out = vec![0u8; 1275];
				let len = encoder.encode_float(&pcm, &mut out).unwrap();
				out.truncate(len);
				(1000 + (n * 960) as u32, out)
			})
			.collect()
	}

	fn rms(x: &[f32]) -> f32 {
		(x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
	}

	#[test]
	fn packets_in_order_decode_straight_through() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		for (n, (ts, p)) in packets(10).into_iter().enumerate() {
			j.push(n as u16, ts, p);
			j.drain_ready(&mut out);
		}
		assert_eq!(out.len(), 9600);
		assert_eq!(j.concealed, 0);
	}

	#[test]
	fn reordered_packets_are_put_back_in_order() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(4);
		for i in [0, 2, 1, 3] {
			j.push(i as u16, p[i].0, p[i].1.clone());
			j.drain_ready(&mut out);
		}
		assert_eq!(out.len(), 4 * 960);
		assert_eq!(j.concealed + j.recovered, 0);
	}

	#[test]
	fn a_lost_packet_is_rebuilt_from_fec_and_the_voice_goes_on() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(12);
		for (i, (ts, packet)) in p.iter().enumerate() {
			if i == 6 {
				continue; // lost
			}
			j.push(i as u16, *ts, packet.clone());
			j.drain_ready(&mut out);
			if i == 7 {
				// The playout reaches the hole with packet 7 already here.
				assert_eq!(out.len(), 6 * 960);
				j.produce(&mut out);
			}
		}
		assert_eq!(out.len(), 12 * 960, "the timeline lost or gained time");
		assert!(j.recovered > 0, "the next packet's FEC wasn't used");
		let out: Vec<f32> = out.into_iter().collect();
		let hole = &out[6 * 960..7 * 960];
		assert!(rms(hole) > 0.5 * rms(&out[4 * 960..6 * 960]), "the lost packet went quiet: {}", rms(hole));
	}

	#[test]
	fn a_packet_inside_the_one_before_it_does_not_stall_later_gaps() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(8);
		// Packet 2 arrives first, and one more that starts 30 ms into it.
		j.push(0, p[0].0, p[0].1.clone());
		j.drain_ready(&mut out);
		j.push(2, p[2].0, p[2].1.clone());
		j.push(3, p[2].0 + 480, p[3].1.clone());
		j.drain_ready(&mut out);
		j.produce(&mut out);
		assert_eq!(out.len(), 3 * 960);
		// Then the peer pauses (a timestamp jump with no sequence gap).
		j.push(4, p[6].0, p[6].1.clone());
		j.drain_ready(&mut out);
		j.produce(&mut out);
		j.push(5, p[7].0, p[7].1.clone());
		j.drain_ready(&mut out);
		assert_eq!(out.len(), 5 * 960, "the packets after the pause weren't played");
		assert_eq!(j.dropped_late, 1);
	}

	#[test]
	fn a_late_packet_is_concealed_then_played_not_dropped() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(6);
		for (n, (ts, packet)) in p[..3].iter().enumerate() {
			j.push(n as u16, *ts, packet.clone());
			j.drain_ready(&mut out);
		}
		out.clear();
		// Packet 3 is 30 ms late: the playout asks three times.
		for _ in 0..3 {
			j.produce(&mut out);
		}
		assert_eq!(out.len(), 480);
		assert!(rms(&out.iter().copied().collect::<Vec<_>>()) > 0.05, "concealment was silent");
		j.push(3, p[3].0, p[3].1.clone());
		j.drain_ready(&mut out);
		assert_eq!(out.len(), 480 + 960, "the late packet wasn't played");
		assert_eq!(j.dropped_late, 0);
	}

	#[test]
	fn a_pause_in_sending_is_not_filled_with_made_up_sound() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(2);
		j.push(0, p[0].0, p[0].1.clone());
		j.drain_ready(&mut out);
		// The next packet comes two seconds on (DTX).
		j.push(1, p[0].0 + 32000, p[1].1.clone());
		out.clear();
		j.produce(&mut out);
		assert_eq!(out.len(), 960, "filled a pause");
	}

	/// Plays three packets, then lets the playout wait `wait_ms` for the next
	/// one, which carries timestamp `gap` samples after the last packet's end
	/// and sequence number `sequence`. Returns what the jitter made up for the
	/// gap once that packet arrived, in output samples, and the total of samples
	/// concealed or rebuilt.
	fn made_up_after_gap(gap: u32, sequence: u16, wait_ms: usize) -> (usize, u64) {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(4);
		for (n, (ts, packet)) in p[..3].iter().enumerate() {
			j.push(n as u16, *ts, packet.clone());
			j.drain_ready(&mut out);
		}
		out.clear();
		for _ in 0..wait_ms / 10 {
			j.produce(&mut out);
		}
		let waited = out.len();
		j.push(sequence, p[2].0 + 960 + gap, p[3].1.clone());
		j.produce(&mut out);
		(out.len() - waited, j.recovered + j.concealed)
	}

	#[test]
	fn silence_with_unbroken_sequence_numbers_is_not_loss() {
		// DTX: the peer sends one packet 300 ms of silence later and counts on from the last
		// sequence number. Whether the playout is already dry when it comes or has waited, only
		// the packet itself is added.
		for wait_ms in [0, 360] {
			let (extra_samples, _) = made_up_after_gap(4800, 3, wait_ms);
			assert_eq!(extra_samples, 960, "the packet after the pause came with more than itself ({wait_ms} ms wait)");
		}
	}

	#[test]
	fn the_same_jump_with_a_sequence_gap_is_still_loss() {
		let (extra_samples, repaired) = made_up_after_gap(4800, 4, 0);
		assert!(extra_samples > 960, "a lost stretch was not filled in");
		assert!(repaired > 0);
	}

	#[test]
	fn a_packet_that_arrives_before_its_predecessor_still_counts_as_unbroken() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(5);
		for (n, (ts, packet)) in p[..2].iter().enumerate() {
			j.push(n as u16, *ts, packet.clone());
			j.drain_ready(&mut out);
		}
		out.clear();
		// Sequence 3 is here before 2; 2 follows 1 across a pause.
		j.push(3, p[2].0 + 960 + 4800 + 960, p[3].1.clone());
		j.push(2, p[2].0 + 960 + 4800, p[2].1.clone());
		j.produce(&mut out);
		assert_eq!(out.len(), 2 * 960, "made up sound for a pause");
		assert_eq!(j.recovered + j.concealed, 0);
	}

	#[test]
	fn long_silence_from_the_peer_fades_to_nothing() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(3);
		for (n, (ts, packet)) in p.iter().enumerate() {
			j.push(n as u16, *ts, packet.clone());
			j.drain_ready(&mut out);
		}
		out.clear();
		for _ in 0..40 {
			j.produce(&mut out);
		}
		let tail: Vec<f32> = out.iter().skip(MAX_CONCEALMENT).copied().collect();
		assert!(tail.iter().all(|&s| s == 0.0), "kept making up sound for {} ms", out.len() / 16);
	}

	/// Share of a 48 kHz signal's energy above 9 kHz, from 1024-point FFTs.
	fn share_above_9k(x: &[f32]) -> f32 {
		let fft = crate::audio::fft::Fft::new(1024);
		let (mut high, mut all) = (0.0f32, 0.0f32);
		for block in x.chunks_exact(1024) {
			let mut re = block.to_vec();
			let mut im = vec![0.0; 1024];
			fft.transform(&mut re, &mut im, false);
			for bin in 1..512 {
				let power = re[bin] * re[bin] + im[bin] * im[bin];
				all += power;
				if bin as f32 * 48000.0 / 1024.0 > 9000.0 {
					high += power;
				}
			}
		}
		high / all.max(1e-12)
	}

	/// A buzzy, voice-like sound: a 140 Hz pulse train, harmonics all the way up.
	fn buzzy(at: usize) -> f32 {
		let phase = (at as f32 * 140.0 / RTP_RATE as f32).fract();
		0.3 * (1.0 - 2.0 * phase)
	}

	#[test]
	fn at_48khz_the_band_above_8khz_is_filled_in() {
		let mut encoder = Encoder::new(RTP_RATE, Channels::Mono, Application::Voip).unwrap();
		encoder.set_bitrate(Bitrate::Bits(24000)).unwrap();
		encoder.set_signal_voice().unwrap();
		let packets: Vec<(u32, Vec<u8>)> = (0..30)
			.map(|n| {
				let pcm: Vec<f32> = (n * 960..(n + 1) * 960).map(buzzy).collect();
				let mut out = vec![0u8; 1275];
				let len = encoder.encode_float(&pcm, &mut out).unwrap();
				out.truncate(len);
				((n * 960) as u32, out)
			})
			.collect();
		let tocs: Vec<u8> = packets.iter().map(|(_, p)| p[0]).collect();
		assert!(tocs.iter().all(|t| t >> 3 == 11), "not 60 ms SILK wideband, as WhatsApp sends: {tocs:x?}");

		let mut extended = OpusJitter::new(48000).unwrap();
		let mut out = VecDeque::new();
		let started = std::time::Instant::now();
		for (n, (ts, p)) in packets.iter().enumerate() {
			extended.push(n as u16, *ts, p.clone());
			extended.drain_ready(&mut out);
		}
		eprintln!(
			"decoding 1.8 s with NoLACE and the extension took {:.1} ms ({:.1}% of one core)",
			started.elapsed().as_secs_f64() * 1000.0,
			started.elapsed().as_secs_f64() / 1.8 * 100.0
		);
		assert_eq!(out.len(), 30 * 960 * 3, "48 kHz output, timeline in 16 kHz");
		let with: Vec<f32> = out.into_iter().skip(5 * 2880).collect();

		let mut plain = Decoder::new(48000, Channels::Mono).unwrap();
		plain.set_complexity(10).unwrap();
		let mut without = Vec::new();
		let mut pcm = vec![0.0; 2880];
		for (_, p) in &packets {
			let n = plain.decode_float(p, &mut pcm, false).unwrap();
			without.extend_from_slice(&pcm[..n]);
		}
		let without = &without[5 * 2880..];
		let (with_share, without_share) = (share_above_9k(&with), share_above_9k(without));
		eprintln!("energy above 9 kHz: {:.4}% extended, {:.4}% plain", with_share * 100.0, without_share * 100.0);
		assert!(without_share < 0.001, "plain wideband already had a high band: {without_share}");
		assert!(with_share > 20.0 * without_share.max(1e-5), "the extension added nothing above 9 kHz");
	}

	#[test]
	fn timestamps_wrap_around() {
		let mut j = OpusJitter::new(16000).unwrap();
		let mut out = VecDeque::new();
		let p = packets(4);
		let start = u32::MAX - 1500;
		for (n, (_, packet)) in p.iter().enumerate() {
			j.push(n as u16, start.wrapping_add((n * 960) as u32), packet.clone());
			j.drain_ready(&mut out);
		}
		assert_eq!(out.len(), 4 * 960);
	}
}
