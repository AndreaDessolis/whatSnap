//! The call's playout (at 48 kHz; the constants below are 16 kHz samples,
//! scaled to the rate), working the way WhatsApp's
//! engine does. Its NetEQ (WebRTC's jitter buffer, in the WA Web voip wasm as
//! NetEqImpl with expand/merge/accelerate stats) never plays silence just because
//! a packet is late:
//!
//! - Expand: with nothing to play, it continues the voice from what was just
//!   played: the last pitch cycle repeated, mixed with noise of the same
//!   loudness as far as the sound wasn't voiced, fading out over about 100 ms.
//! - Merge: when audio arrives again it is blended in from the expansion over
//!   a few milliseconds, so there is no click at the seam.
//! - Accelerate: with too much waiting, one pitch cycle is cut out of voiced or
//!   silent audio and its neighbours cross-faded, which shortens the delay
//!   without changing the pitch (playing the samples faster would).
//!
//! WhatsApp also runs a neural concealment model on top; this is the classic
//! NetEQ part only.

use std::collections::VecDeque;

/// Shortest and longest pitch period looked for: 2.5 ms (400 Hz) to 20 ms (50 Hz).
const MIN_PITCH: usize = 40;
const MAX_PITCH: usize = 320;
/// How much played audio is kept to build an expansion from.
const HISTORY: usize = 2 * MAX_PITCH + 160;
/// An expansion holds full level this long, then fades to nothing by FADE_END.
const FADE_START: usize = 320;
const FADE_END: usize = 1600;
/// The cross-fade from an expansion into arriving audio: 5 ms.
const MERGE: usize = 80;
/// Accelerate is tried at most this often, and cuts only this alike a cycle.
const ACCELERATE_EVERY: usize = 320;
const ACCELERATE_MIN_LIKENESS: f32 = 0.9;
/// Below this mean power a stretch is quiet enough to shorten whatever it is.
const QUIET_POWER: f32 = 1e-6;

/// The made-up continuation of the voice while no audio is there.
struct Expansion {
	cycle: Vec<f32>,
	noise: Vec<f32>,
	/// 1 for a clearly periodic (voiced) sound, 0 for noise.
	voiced: f32,
	at: usize,
	scale: usize,
}

impl Expansion {
	fn from_history(history: &VecDeque<f32>, scale: usize) -> Self {
		let recent: Vec<f32> = history.iter().copied().collect();
		if recent.len() < 2 * MIN_PITCH * scale {
			return Self { cycle: vec![0.0], noise: vec![0.0], voiced: 1.0, at: 0, scale };
		}
		let (period, likeness) = best_period(&recent[..], recent.len(), scale);
		let cycle = recent[recent.len() - period..].to_vec();
		let noise = recent[recent.len().saturating_sub(MAX_PITCH * scale)..].to_vec();
		Self { cycle, noise, voiced: likeness.clamp(0.0, 1.0), at: 0, scale }
	}

	fn next(&mut self, rng: &mut u32) -> f32 {
		let (start, end) = (FADE_START * self.scale, FADE_END * self.scale);
		let gain = if self.at < start {
			1.0
		} else if self.at < end {
			1.0 - (self.at - start) as f32 / (end - start) as f32
		} else {
			0.0
		};
		let periodic = self.cycle[self.at % self.cycle.len()];
		*rng ^= *rng << 13;
		*rng ^= *rng >> 17;
		*rng ^= *rng << 5;
		let pick = self.noise[(*rng as usize >> 1) % self.noise.len()];
		let noise = if *rng & 1 == 0 { pick } else { -pick };
		self.at += 1;
		gain * (self.voiced * periodic + (1.0 - self.voiced) * noise)
	}
}

/// The pitch period that best matches the audio ending at `end` with the
/// cycle before it, and how alike the two cycles are (normalised correlation).
/// At `scale` times 16 kHz it looks at every scale-th period and sample, so it
/// costs the same at 48 kHz as at 16.
fn best_period(audio: &[f32], end: usize, scale: usize) -> (usize, f32) {
	let mut best = (MIN_PITCH * scale, -1.0f32);
	let longest = (MAX_PITCH * scale).min(end / 2);
	let mut period = MIN_PITCH * scale;
	while period <= longest {
		let a = &audio[end - 2 * period..end - period];
		let b = &audio[end - period..end];
		let likeness = likeness(a, b, scale);
		if likeness > best.1 {
			best = (period, likeness);
		}
		period += scale;
	}
	best
}

fn likeness(a: &[f32], b: &[f32], step: usize) -> f32 {
	let (mut ab, mut aa, mut bb) = (0.0f32, 0.0f32, 0.0f32);
	for (&x, &y) in a.iter().step_by(step).zip(b.iter().step_by(step)) {
		ab += x * y;
		aa += x * x;
		bb += y * y;
	}
	if aa <= 0.0 || bb <= 0.0 {
		return if aa == bb { 1.0 } else { 0.0 };
	}
	ab / (aa * bb).sqrt()
}

pub(crate) struct Playout {
	pub(crate) queue: VecDeque<f32>,
	/// The side ((left - right) / 2) of each sample in `queue`, for a peer
	/// sending stereo; zeros otherwise. Whatever is done to `queue` is done
	/// to it too.
	pub(crate) side: VecDeque<f32>,
	/// The side of the sample `next` returned last.
	pub(crate) last_side: f32,
	/// The peer's Opus packets, when it sends standard Opus: decoded into
	/// `queue` as they come, and asked to conceal (libopus Deep PLC) when the
	/// queue runs dry, in place of the waveform expansion below, which is for
	/// audio that arrives already decoded (MLow).
	pub(crate) opus: Option<crate::calls::opus_jitter::OpusJitter>,
	history: VecDeque<f32>,
	expansion: Option<Expansion>,
	/// Samples left of the merge from the expansion into arriving audio.
	merging: usize,
	refill_level: usize,
	pub(crate) refilling: bool,
	until_accelerate: usize,
	rng: u32,
	/// The rate over 16 kHz: 3 at 48 kHz.
	scale: usize,
	/// Samples made up while nothing was there, and cut by accelerate.
	pub(crate) concealed: u64,
	pub(crate) accelerated: u64,
}

impl Playout {
	/// `refill_level`: how much must be waiting before playback (re)starts,
	/// in samples at `rate` (16000 or 48000).
	pub(crate) fn new(refill_level: usize, rate: u32) -> Self {
		let scale = (rate / 16000).max(1) as usize;
		Self {
			queue: VecDeque::new(),
			side: VecDeque::new(),
			last_side: 0.0,
			opus: None,
			history: VecDeque::with_capacity(HISTORY * scale),
			expansion: None,
			merging: 0,
			refill_level,
			refilling: true,
			until_accelerate: 0,
			rng: 0x9e37_79b9,
			scale,
			concealed: 0,
			accelerated: 0,
		}
	}

	/// The next sample to play. `backlog`: the queue is above where it should
	/// sit, so a pitch cycle may be cut.
	pub(crate) fn next(&mut self, backlog: bool) -> f32 {
		if self.queue.is_empty()
			&& let Some(opus) = self.opus.as_mut()
			&& opus.started()
		{
			// The decoder fills in: decodes what's due, rebuilds a lost
			// packet, or conceals while a late one is on its way.
			opus.produce(&mut self.queue);
			self.side.extend(opus.side.drain(..));
			self.refilling = false;
		}
		self.line_up_side();
		if self.refilling {
			if self.queue.len() < self.refill_level {
				return self.conceal();
			}
			self.refilling = false;
		}
		if backlog {
			self.maybe_accelerate();
		} else {
			self.until_accelerate = self.until_accelerate.saturating_sub(1);
		}
		let Some(mut sample) = self.queue.pop_front() else {
			self.refilling = true;
			return self.conceal();
		};
		let mut side = self.side.pop_front().unwrap_or(0.0);
		if let Some(expansion) = self.expansion.as_mut() {
			let merge = MERGE * self.scale;
			if self.merging == 0 {
				self.merging = merge;
			}
			let made_up = expansion.next(&mut self.rng);
			let weight = 1.0 - self.merging as f32 / merge as f32;
			sample = sample * weight + made_up * (1.0 - weight);
			side *= weight;
			self.merging -= 1;
			if self.merging == 0 {
				self.expansion = None;
			}
		}
		self.last_side = side;
		self.remember(sample);
		sample
	}

	/// Audio put in `queue` without a side (decoded by the backend, or by
	/// a test) gets a zero one, so the two stay in step.
	fn line_up_side(&mut self) {
		if self.side.len() != self.queue.len() {
			self.side.resize(self.queue.len(), 0.0);
		}
	}

	fn conceal(&mut self) -> f32 {
		self.merging = 0;
		self.last_side = 0.0;
		if self.expansion.is_none() {
			self.expansion = Some(Expansion::from_history(&self.history, self.scale));
		}
		self.concealed += 1;
		let sample = self.expansion.as_mut().expect("just made").next(&mut self.rng);
		self.remember(sample);
		sample
	}

	fn remember(&mut self, sample: f32) {
		if self.history.len() == HISTORY * self.scale {
			self.history.pop_front();
		}
		self.history.push_back(sample);
	}

	/// NetEQ's accelerate: where the next two cycles are alike (or quiet),
	/// cross-fade them into one, so a cycle's worth less is waiting.
	fn maybe_accelerate(&mut self) {
		if self.until_accelerate > 0 {
			self.until_accelerate -= 1;
			return;
		}
		// Only the audio looked at has to be here: how much to cut is the
		// caller's call (calls/audio.rs keeps a cushion the network needed).
		// Requiring the refill level on top kept at least 160 ms waiting.
		let window = 2 * MAX_PITCH * self.scale;
		if self.expansion.is_some() || self.queue.len() < window {
			return;
		}
		let ahead: Vec<f32> = self.queue.iter().take(window).copied().collect();
		let (period, likeness) = best_period(&ahead, window, self.scale);
		let (first, second) = (&ahead[window - 2 * period..], window - period);
		let power = first.iter().map(|s| s * s).sum::<f32>() / first.len() as f32;
		if likeness < ACCELERATE_MIN_LIKENESS && power > QUIET_POWER {
			self.until_accelerate = ACCELERATE_EVERY * self.scale / 2;
			return;
		}
		// Cycles [start, start+period) and [start+period, start+2*period)
		// become one cycle that fades from the first into the second.
		let start = window - 2 * period;
		for i in 0..period {
			let fade = i as f32 / period as f32;
			self.queue[second + i] = ahead[start + i] * (1.0 - fade) + ahead[second + i] * fade;
			self.side[second + i] = self.side[start + i] * (1.0 - fade) + self.side[second + i] * fade;
		}
		self.queue.drain(start..start + period);
		self.side.drain(start..start + period);
		self.accelerated += period as u64;
		self.until_accelerate = ACCELERATE_EVERY * self.scale;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn voice(at: usize) -> f32 {
		// A 200 Hz vowel-like tone with a harmonic.
		let t = at as f32 / 16000.0;
		0.4 * (2.0 * std::f32::consts::PI * 200.0 * t).sin() + 0.15 * (2.0 * std::f32::consts::PI * 400.0 * t).sin()
	}

	fn rms(samples: &[f32]) -> f32 {
		(samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
	}

	#[test]
	fn nothing_plays_before_the_first_audio() {
		let mut p = Playout::new(960, 16000);
		let out: Vec<f32> = (0..500).map(|_| p.next(false)).collect();
		assert!(out.iter().all(|&s| s == 0.0));
	}

	#[test]
	fn a_late_packet_is_bridged_by_the_voice_not_silence() {
		let mut p = Playout::new(960, 16000);
		p.queue.extend((0..1920).map(voice));
		let played: Vec<f32> = (0..1920).map(|_| p.next(false)).collect();
		// The next packet is 20 ms late.
		let gap: Vec<f32> = (0..320).map(|_| p.next(false)).collect();
		assert!(p.concealed >= 320);
		assert!(rms(&gap) > 0.8 * rms(&played), "the gap went quiet: {} against {}", rms(&gap), rms(&played));
		// And it carries on as the same tone: each sample near where the
		// voice would have been a whole number of cycles later.
		let jumps = played.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
		let worst = std::iter::once(played[1919]).chain(gap.iter().copied()).collect::<Vec<_>>();
		let gap_jumps = worst.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
		assert!(gap_jumps < 1.5 * jumps, "a click into the expansion: {gap_jumps} against {jumps}");
	}

	#[test]
	fn arriving_audio_is_merged_in_without_a_click() {
		let mut p = Playout::new(960, 16000);
		p.queue.extend((0..1920).map(voice));
		let mut out: Vec<f32> = (0..1920).map(|_| p.next(false)).collect();
		out.extend((0..320).map(|_| p.next(false)));
		// The late audio arrives (refill level and more), out of step with
		// what the expansion guessed.
		p.queue.extend((5000..5000 + 1920).map(voice));
		out.extend((0..960).map(|_| p.next(false)));
		let normal = (1..1920).map(|i| (voice(i) - voice(i - 1)).abs()).fold(0.0, f32::max);
		let worst = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
		assert!(worst < 2.0 * normal, "a click at the seam: {worst} against {normal}");
		assert!(p.expansion.is_none(), "still expanding with audio waiting");
	}

	#[test]
	fn a_long_gap_fades_to_silence() {
		let mut p = Playout::new(960, 16000);
		p.queue.extend((0..1920).map(voice));
		for _ in 0..1920 {
			p.next(false);
		}
		let gap: Vec<f32> = (0..3200).map(|_| p.next(false)).collect();
		assert!(gap[FADE_END..].iter().all(|&s| s == 0.0), "a buzz past the fade");
		assert!(rms(&gap[..FADE_START]) > 0.2);
	}

	#[test]
	fn accelerate_cuts_whole_cycles_and_keeps_the_pitch() {
		let mut p = Playout::new(960, 16000);
		p.queue.extend((0..16000).map(voice));
		let out: Vec<f32> = (0..8000).map(|_| p.next(true)).collect();
		assert!(p.accelerated > 1000, "only {} samples cut", p.accelerated);
		assert_eq!(p.concealed, 0);
		let normal = (1..1920).map(|i| (voice(i) - voice(i - 1)).abs()).fold(0.0, f32::max);
		let worst = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
		assert!(worst < 1.5 * normal, "a click where a cycle was cut: {worst} against {normal}");
		// Still 200 Hz: zero crossings per second unchanged (within a cycle).
		let crossings = out.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
		assert!((98..=102).contains(&crossings), "{crossings} upward crossings in 0.5 s, want ~100");
	}

	#[test]
	fn speech_that_doesnt_repeat_is_left_alone() {
		let mut p = Playout::new(960, 16000);
		let mut rng = 1u32;
		p.queue.extend((0..16000).map(|_| {
			rng ^= rng << 13;
			rng ^= rng >> 17;
			rng ^= rng << 5;
			(rng as f32 / u32::MAX as f32 - 0.5) * 0.5
		}));
		for _ in 0..4000 {
			p.next(true);
		}
		assert_eq!(p.accelerated, 0, "cut a cycle out of noise");
	}

	#[test]
	fn the_side_stays_in_step_through_accelerate() {
		let mut p = Playout::new(960, 16000);
		p.queue.extend((0..16000).map(voice));
		// Right only: side = -mid, so left stays silent whatever is cut.
		p.side.extend((0..16000).map(|i| -voice(i)));
		for _ in 0..8000 {
			let mid = p.next(true);
			assert!((mid + p.last_side).abs() < 1e-6, "left not silent: mid {mid} side {}", p.last_side);
		}
		assert!(p.accelerated > 1000);
	}

	#[test]
	fn audio_without_a_side_plays_as_mono() {
		let mut p = Playout::new(960, 16000);
		p.queue.extend((0..1920).map(voice));
		for _ in 0..1920 {
			p.next(false);
			assert_eq!(p.last_side, 0.0);
		}
	}
}
