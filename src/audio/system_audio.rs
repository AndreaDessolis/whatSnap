use std::{
	collections::VecDeque,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
		mpsc,
	},
	time::Duration,
};

use windows::{
	Win32::{
		Foundation::WAIT_OBJECT_0,
		Media::Audio::{
			AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
			AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
			AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
			AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
			ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
			IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
			IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
			VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
		},
		System::{
			Com::{
				BLOB, COINIT_MULTITHREADED, CoInitializeEx,
				StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0},
			},
			Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject},
			Variant::VT_BLOB,
		},
	},
	core::{HRESULT, IUnknown, Interface, PCWSTR, Ref},
};

use crate::{
	UiCommand,
	calls::audio::{ComGuard, Downsampler, EventGuard, log_line},
	ui::wake::UiCommandSender,
};

const CAPTURE_RATE: u32 = 48_000;
const CAPTURE_CHANNELS: u16 = 2;
/// Frames (left and right at 16 kHz) that must be waiting before the shared
/// sound is mixed in, and the most kept waiting.
const MIX_PRIME: usize = 960;
const MIX_MAX: usize = 4000;
const MIX_GAIN: f32 = 0.7;
/// While you talk the shared sound is turned down to this share of its level,
/// like WhatsApp's ducking mixer for screen sharing sound
/// (wa_audio_ducking_mixer.cc in the WA Web voip wasm), so you're heard over it.
const DUCKED: f32 = 0.4;
/// Voice louder than this (about -32 dBFS) counts as talking.
const DUCK_THRESHOLD: f32 = 0.025;
/// The voice level follower at 16 kHz: rises in about 5 ms, falls in 100 ms.
const LEVEL_ATTACK: f32 = 1.0 / 80.0;
const LEVEL_RELEASE: f32 = 1.0 / 1600.0;
/// The shared sound goes down in about 30 ms and back up over half a second.
const DUCK_DOWN: f32 = 1.0 / 480.0;
const DUCK_UP: f32 = 1.0 / 8000.0;

pub(crate) struct SystemMix {
	queue: VecDeque<[f32; 2]>,
	primed: bool,
	/// How loud the voice is, followed sample by sample.
	level: f32,
	/// The ducking gain now: 1 untouched, DUCKED while talking.
	duck: f32,
}

impl Default for SystemMix {
	fn default() -> Self {
		Self { queue: VecDeque::new(), primed: false, level: 0.0, duck: 1.0 }
	}
}

impl SystemMix {
	/// Adds interleaved left/right samples at 16 kHz.
	pub(crate) fn push(&mut self, samples: &[f32]) {
		self.queue.extend(samples.chunks_exact(2).map(|pair| [pair[0], pair[1]]));
		if self.queue.len() > MIX_MAX {
			let excess = self.queue.len() - MIX_PRIME;
			self.queue.drain(..excess);
		}
	}

	/// Whether shared sound is flowing (also while it's silent: Windows hands
	/// over silence while nothing plays).
	pub(crate) fn sharing(&self) -> bool {
		self.primed
	}

	/// The shared sound to add to this voice sample, left and right, already
	/// at its mixing level. `duck` turns it down while the voice talks.
	pub(crate) fn next(&mut self, voice: f32, duck: bool) -> [f32; 2] {
		let loud = voice.abs();
		self.level += (loud - self.level) * if loud > self.level { LEVEL_ATTACK } else { LEVEL_RELEASE };
		let target = if duck && self.level > DUCK_THRESHOLD { DUCKED } else { 1.0 };
		self.duck += (target - self.duck) * if target < self.duck { DUCK_DOWN } else { DUCK_UP };
		if !self.primed {
			if self.queue.len() < MIX_PRIME {
				return [0.0; 2];
			}
			self.primed = true;
		}
		match self.queue.pop_front() {
			Some([left, right]) => {
				let gain = MIX_GAIN * self.duck;
				[left * gain, right * gain]
			}
			None => {
				self.primed = false;
				[0.0; 2]
			}
		}
	}

	pub(crate) fn clear(&mut self) {
		self.queue.clear();
		self.primed = false;
	}
}

pub(crate) struct SystemAudioCapture {
	stop: Arc<AtomicBool>,
}

impl Drop for SystemAudioCapture {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
	}
}

pub(crate) fn start(mix: Arc<Mutex<SystemMix>>, ui_tx: UiCommandSender) -> SystemAudioCapture {
	let stop = Arc::new(AtomicBool::new(false));
	let thread_stop = Arc::clone(&stop);
	let spawned = std::thread::Builder::new().name("system audio".into()).spawn(move || {
		let mut first = true;
		while !thread_stop.load(Ordering::Relaxed) {
			if let Err(e) = run_session(&thread_stop, &mix, &mut first) {
				log_line(&format!("system audio: session ended with error: {:?}", e));
				if first {
					let _ = ui_tx.send(UiCommand::SystemAudioFailed(e.to_string()));
					break;
				}
				std::thread::sleep(Duration::from_millis(500));
			}
		}
		mix.lock().unwrap().clear();
		log_line("system audio: thread exiting");
	});
	if let Err(e) = spawned {
		log_line(&format!("system audio: couldn't start its thread: {e}"));
	}
	SystemAudioCapture { stop }
}

#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated {
	done: mpsc::SyncSender<()>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
	fn ActivateCompleted(&self, _operation: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
		let _ = self.done.try_send(());
		Ok(())
	}
}

unsafe fn activate_loopback() -> crate::error::Result<IAudioClient> {
	unsafe {
		let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
			ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
			Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
				ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
					TargetProcessId: GetCurrentProcessId(),
					ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
				},
			},
		};
		// A VT_BLOB pointing at `params`, which outlives the call. It must
		// never be dropped: the windows crate's PROPVARIANT runs
		// PropVariantClear on drop, which hands the blob pointer to
		// CoTaskMemFree, and freeing `params` (on the stack) crashed the app.
		let variant = std::mem::ManuallyDrop::new(PROPVARIANT {
			Anonymous: PROPVARIANT_0 {
				Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
					vt: VT_BLOB,
					wReserved1: 0,
					wReserved2: 0,
					wReserved3: 0,
					Anonymous: PROPVARIANT_0_0_0 {
						blob: BLOB {
							cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
							pBlobData: &mut params as *mut _ as *mut u8,
						},
					},
				}),
			},
		});
		let (done_tx, done_rx) = mpsc::sync_channel(1);
		let handler: IActivateAudioInterfaceCompletionHandler = Activated { done: done_tx }.into();
		let operation = ActivateAudioInterfaceAsync(
			VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
			&IAudioClient::IID,
			Some(&*variant),
			&handler,
		)?;
		log_line("system audio: activation requested");
		if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
			crate::error::bail!("Windows didn't open the system audio in time");
		}
		let mut result = HRESULT(0);
		let mut activated: Option<IUnknown> = None;
		log_line("system audio: activation completed");
		operation.GetActivateResult(&mut result, &mut activated)?;
		log_line(&format!("system audio: activation result {:#010x}", result.0 as u32));
		result.ok()?;
		let Some(activated) = activated else {
			crate::error::bail!("Windows returned no system audio stream");
		};
		Ok(activated.cast()?)
	}
}

fn run_session(stop: &AtomicBool, mix: &Mutex<SystemMix>, first: &mut bool) -> crate::error::Result<()> {
	unsafe {
		CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
		let _com_guard = ComGuard;

		// Each step is logged: a crash in here leaves nothing else behind.
		log_line("system audio: opening process loopback");
		let client = activate_loopback()?;
		log_line("system audio: loopback opened");
		let block_align = CAPTURE_CHANNELS * 2;
		let format = WAVEFORMATEX {
			wFormatTag: WAVE_FORMAT_PCM as u16,
			nChannels: CAPTURE_CHANNELS,
			nSamplesPerSec: CAPTURE_RATE,
			nAvgBytesPerSec: CAPTURE_RATE * block_align as u32,
			nBlockAlign: block_align,
			wBitsPerSample: 16,
			cbSize: 0,
		};
		client.Initialize(
			AUDCLNT_SHAREMODE_SHARED,
			AUDCLNT_STREAMFLAGS_LOOPBACK
				| AUDCLNT_STREAMFLAGS_EVENTCALLBACK
				| AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
				| AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
			200_000,
			0,
			&format,
			None,
		)?;
		log_line("system audio: stream initialised");
		let event = CreateEventW(None, false, false, PCWSTR::null())?;
		let _event_guard = EventGuard(event);
		client.SetEventHandle(event)?;
		let capture_client: IAudioCaptureClient = client.GetService()?;
		client.Start()?;
		*first = false;
		log_line("system audio: capture started");

		// Left and right each to 16 kHz, so a stereo call can send them apart.
		let mut downsamplers = [Downsampler::new(CAPTURE_RATE, 1), Downsampler::new(CAPTURE_RATE, 1)];
		let mut scratch: Vec<f32> = Vec::new();
		let mut channel: Vec<f32> = Vec::new();
		let mut resampled: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
		let mut interleaved: Vec<f32> = Vec::new();
		while !stop.load(Ordering::Relaxed) {
			if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
				continue;
			}
			loop {
				if capture_client.GetNextPacketSize()? == 0 {
					break;
				}
				let mut data_ptr: *mut u8 = std::ptr::null_mut();
				let mut num_frames: u32 = 0;
				let mut flags: u32 = 0;
				capture_client.GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)?;
				let total_samples = num_frames as usize * CAPTURE_CHANNELS as usize;
				scratch.clear();
				if data_ptr.is_null() || (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
					scratch.resize(total_samples, 0.0);
				} else {
					let src = std::slice::from_raw_parts(data_ptr as *const i16, total_samples);
					scratch.extend(src.iter().map(|&s| s as f32 / 32768.0));
				}
				capture_client.ReleaseBuffer(num_frames)?;

				for (side, downsampler) in downsamplers.iter_mut().enumerate() {
					channel.clear();
					channel.extend(scratch.iter().skip(side).step_by(CAPTURE_CHANNELS as usize));
					resampled[side].clear();
					downsampler.process(&channel, &mut resampled[side]);
				}
				interleaved.clear();
				for (&left, &right) in resampled[0].iter().zip(&resampled[1]) {
					interleaved.extend([left, right]);
				}
				mix.lock().unwrap().push(&interleaved);
			}
		}
		client.Stop()?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn stereo(left: f32, right: f32, frames: usize) -> Vec<f32> {
		std::iter::repeat_n([left, right], frames).flatten().collect()
	}

	#[test]
	fn mixing_waits_for_a_primed_queue_then_adds_the_sound() {
		let mut mix = SystemMix::default();
		mix.push(&stereo(0.5, 0.25, MIX_PRIME - 1));
		assert_eq!(mix.next(0.0, true), [0.0; 2], "mixed before the queue was primed");
		mix.push(&stereo(0.5, 0.25, 1));
		let [left, right] = mix.next(0.0, true);
		assert!((left - 0.5 * MIX_GAIN).abs() < 1e-6);
		assert!((right - 0.25 * MIX_GAIN).abs() < 1e-6);
		assert!(mix.sharing());
	}

	#[test]
	fn running_dry_waits_to_prime_again() {
		let mut mix = SystemMix::default();
		mix.push(&stereo(0.5, 0.5, MIX_PRIME));
		for _ in 0..MIX_PRIME {
			mix.next(0.0, true);
		}
		assert_eq!(mix.next(0.2, true), [0.0; 2]);
		assert!(!mix.sharing());
		mix.push(&stereo(0.5, 0.5, 10));
		assert_eq!(mix.next(0.2, true), [0.0; 2], "mixed a few late samples before priming again");
	}

	#[test]
	fn a_queue_running_ahead_drops_its_oldest_sound() {
		let mut mix = SystemMix::default();
		mix.push(&stereo(0.1, 0.1, MIX_MAX));
		mix.push(&stereo(0.9, 0.8, 1));
		assert_eq!(mix.queue.len(), MIX_PRIME);
		assert_eq!(mix.queue.back(), Some(&[0.9, 0.8]));
	}

	#[test]
	fn talking_ducks_the_shared_sound_and_it_comes_back_after() {
		let mut mix = SystemMix::default();
		mix.push(&stereo(0.5, 0.5, MIX_MAX));
		let full = mix.next(0.0, true)[0];
		// 100 ms of a 200 Hz voice at about -12 dBFS.
		let mut ducked = full;
		for i in 0..1600 {
			mix.push(&stereo(0.5, 0.5, 1));
			let voice = 0.25 * (2.0 * std::f32::consts::PI * 200.0 * i as f32 / 16000.0).sin();
			ducked = mix.next(voice, true)[0];
		}
		assert!(ducked < full * (DUCKED + 0.05), "not ducked while talking: {ducked} against {full}");
		// A second of quiet brings it back.
		let mut after = ducked;
		for _ in 0..16000 {
			mix.push(&stereo(0.5, 0.5, 1));
			after = mix.next(0.0, true)[0];
		}
		assert!(after > full * 0.8, "still ducked a second after talking: {after} against {full}");
	}

	#[test]
	fn without_ducking_talking_leaves_the_sound_alone() {
		let mut mix = SystemMix::default();
		mix.push(&stereo(0.5, 0.5, MIX_MAX));
		let full = mix.next(0.0, false)[0];
		let mut level = full;
		for _ in 0..1600 {
			mix.push(&stereo(0.5, 0.5, 1));
			level = mix.next(0.3, false)[0];
		}
		assert_eq!(level, full);
	}
}
