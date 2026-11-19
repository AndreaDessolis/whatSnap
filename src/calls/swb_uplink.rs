//! Our voice above 8 kHz, for calls that negotiated super-wideband (24 or
//! 48 kHz) Opus. The cleaning chain (echo canceller, noise suppressors, auto
//! gain, gate) runs at 16 kHz, so it never sees what lies above 8 kHz. The
//! mic's own high band is taken straight from the device, delayed to line up
//! with the chain's output, and scaled every 10 ms by what the chain did to
//! the band below: where it removed echo or noise, or the gate closed, the
//! high band goes down with it; where auto gain raised the voice, it comes up
//! with it. The two halves meet at 7.5 kHz.
//!
//! UNTESTED LIVE: the server has never turned super-wideband on for us.

use std::collections::VecDeque;

use crate::{audio::call_voice::Upsampler3, calls::audio::Downsampler};

/// The chain's delay from mic to cleaned 16 kHz audio (22 ms), measured by
/// cross-correlating a voiced test signal through CallVoice. A few ms off only
/// blurs the gain, which is smoothed over about as long.
const CHAIN_DELAY_16K: usize = 348;
const SLICE_16K: usize = 160;
const SLICE_48K: usize = SLICE_16K * 3;
const CROSSOVER_HZ: f32 = 7500.0;
const WIDE_RATE: f32 = 48_000.0;
/// The high band follows the chain's gain at most this far up.
const MAX_GAIN: f32 = 4.0;
/// Smoothing of that gain from one 10 ms slice to the next.
const GAIN_SMOOTHING: f32 = 0.5;

/// A second-order high-pass section (RBJ cookbook).
struct HighPass {
	b: [f32; 3],
	a: [f32; 2],
	x: [f32; 2],
	y: [f32; 2],
}

impl HighPass {
	fn new(hz: f32, q: f32) -> Self {
		let w = 2.0 * std::f32::consts::PI * hz / WIDE_RATE;
		let alpha = w.sin() / (2.0 * q);
		let cos = w.cos();
		let a0 = 1.0 + alpha;
		Self {
			b: [(1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0, (1.0 + cos) / 2.0 / a0],
			a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
			x: [0.0; 2],
			y: [0.0; 2],
		}
	}

	fn process(&mut self, x: f32) -> f32 {
		let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
			- self.a[0] * self.y[0]
			- self.a[1] * self.y[1];
		self.x = [x, self.x[0]];
		self.y = [y, self.y[0]];
		y
	}
}

pub(crate) struct HighBand {
	/// The device's audio at 48 kHz, mono, until the chain takes the same
	/// stretch at 16 kHz.
	wide: Downsampler,
	wide_buf: Vec<f32>,
	wide_waiting: VecDeque<f32>,
	/// Fourth-order Butterworth high-pass at the crossover.
	high_pass: [HighPass; 2],
	/// The high band, waiting for the chain's output to catch up.
	band: VecDeque<f32>,
	/// The raw 16 kHz mic the chain is given, delayed the same way, to tell
	/// what the chain did to it.
	raw: VecDeque<f32>,
	gain: f32,
	/// Gain-scaled high band, ready to join the cleaned audio frame by frame.
	ready: VecDeque<f32>,
	up: Upsampler3,
	/// Cleaned 16 kHz samples not yet a whole slice.
	pending_clean: Vec<f32>,
}

impl HighBand {
	pub(crate) fn new(device_rate: u32, channels: u16) -> Self {
		Self {
			wide: Downsampler::with_output(device_rate, channels, WIDE_RATE as u32, 20_000.0),
			wide_buf: Vec::new(),
			wide_waiting: VecDeque::new(),
			high_pass: [HighPass::new(CROSSOVER_HZ, 0.5412), HighPass::new(CROSSOVER_HZ, 1.3066)],
			band: std::iter::repeat_n(0.0, CHAIN_DELAY_16K * 3).collect(),
			raw: std::iter::repeat_n(0.0, CHAIN_DELAY_16K).collect(),
			gain: 0.0,
			ready: VecDeque::new(),
			up: Upsampler3::new(),
			pending_clean: Vec::new(),
		}
	}

	/// Takes the device's audio, as the chain's own resampler does.
	pub(crate) fn push_device(&mut self, device: &[f32]) {
		self.wide_buf.clear();
		self.wide.process(device, &mut self.wide_buf);
		self.wide_waiting.extend(&self.wide_buf);
	}

	/// Takes the 16 kHz mic samples the chain was just given (it can hold
	/// some back while the played sound catches up), and the matching
	/// stretch of the 48 kHz audio.
	pub(crate) fn chain_took(&mut self, raw16: &[f32]) {
		self.raw.extend(raw16);
		let n = (raw16.len() * 3).min(self.wide_waiting.len());
		let [first, second] = &mut self.high_pass;
		for sample in self.wide_waiting.drain(..n) {
			self.band.push_back(second.process(first.process(sample)));
		}
	}

	/// Takes the chain's cleaned output (before system audio and the limiter)
	/// and scales the matching stretch of high band by what the chain did.
	pub(crate) fn push_cleaned(&mut self, cleaned: &[f32]) {
		self.pending_clean.extend_from_slice(cleaned);
		let mut used = 0;
		while self.pending_clean.len() - used >= SLICE_16K
			&& self.raw.len() >= SLICE_16K
			&& self.band.len() >= SLICE_48K
		{
			let clean = &self.pending_clean[used..used + SLICE_16K];
			let clean_energy: f32 = clean.iter().map(|s| s * s).sum();
			let raw_energy: f32 = self.raw.drain(..SLICE_16K).map(|s| s * s).sum();
			let target = if raw_energy > 1e-9 { (clean_energy / raw_energy).sqrt().min(MAX_GAIN) } else { 0.0 };
			let start = self.gain;
			self.gain = GAIN_SMOOTHING * self.gain + (1.0 - GAIN_SMOOTHING) * target;
			for (i, sample) in self.band.drain(..SLICE_48K).enumerate() {
				let g = start + (self.gain - start) * i as f32 / SLICE_48K as f32;
				self.ready.push_back(sample * g);
			}
			used += SLICE_16K;
		}
		self.pending_clean.drain(..used);
	}

	/// One 48 kHz frame: the finished 16 kHz frame brought up to 48 kHz with
	/// the high band added. The high band is left out while it lags behind.
	pub(crate) fn wide_frame(&mut self, frame16: &[f32], out: &mut Vec<f32>) {
		out.clear();
		self.up.process(frame16, out);
		if self.ready.len() < out.len() {
			return;
		}
		let n = out.len();
		for (o, h) in out.iter_mut().zip(self.ready.drain(..n)) {
			*o = (*o + h).clamp(-1.0, 1.0);
		}
		// Never let the high band fall far behind: drop what piles up.
		let keep = out.len() * 2;
		if self.ready.len() > keep {
			let extra = self.ready.len() - keep;
			self.ready.drain(..extra);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tone(hz: f32, rate: f32, n: usize, amp: f32) -> Vec<f32> {
		(0..n).map(|i| amp * (2.0 * std::f32::consts::PI * hz * i as f32 / rate).sin()).collect()
	}

	fn rms(x: &[f32]) -> f32 {
		(x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
	}

	#[test]
	fn the_high_band_follows_what_the_chain_did() {
		// Device at 48 kHz with a 10 kHz tone; the chain passes the band below
		// untouched (gain 1), then mutes it (gain 0).
		let mut hb = HighBand::new(48_000, 1);
		let device = tone(10_000.0, 48_000.0, 2880, 0.1);
		let low = tone(1_000.0, 16_000.0, 960, 0.1);
		let mut out = Vec::new();
		let mut levels = Vec::new();
		for frame in 0..20 {
			hb.push_device(&device);
			hb.chain_took(&low);
			let cleaned = if frame < 10 { low.clone() } else { vec![0.0; 960] };
			hb.push_cleaned(&cleaned);
			hb.wide_frame(&vec![0.0; 960], &mut out);
			assert_eq!(out.len(), 2880);
			levels.push(rms(&out));
		}
		assert!(levels[8] > 0.05, "high band passed at gain 1: {levels:?}");
		assert!(levels[19] < 0.005, "high band muted with the chain: {levels:?}");
	}

	#[test]
	fn the_band_below_the_crossover_is_left_to_the_chain() {
		let mut hb = HighBand::new(48_000, 1);
		let device = tone(2_000.0, 48_000.0, 2880, 0.1);
		let low = tone(2_000.0, 16_000.0, 960, 0.1);
		let mut out = Vec::new();
		for _ in 0..10 {
			hb.push_device(&device);
			hb.chain_took(&low);
			hb.push_cleaned(&low);
			hb.wide_frame(&vec![0.0; 960], &mut out);
		}
		assert!(rms(&out) < 0.003, "2 kHz leaked into the high band: {}", rms(&out));
	}
}
