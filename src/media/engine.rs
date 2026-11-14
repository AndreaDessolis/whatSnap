use std::sync::{
	Arc,
	atomic::{AtomicBool, AtomicU32, Ordering},
};

use windows::{
	Win32::{
		Foundation::{HWND, RECT},
		Media::MediaFoundation::{
			CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory,
			IMFMediaEngineEx, IMFMediaEngineNotify, IMFMediaEngineNotify_Impl, MF_MEDIA_ENGINE_CALLBACK,
			MF_MEDIA_ENGINE_EVENT_ENDED, MF_MEDIA_ENGINE_EVENT_ERROR, MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA,
			MF_MEDIA_ENGINE_PLAYBACK_HWND, MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, MFCreateAttributes,
		},
		System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
		UI::WindowsAndMessaging::GetClientRect,
	},
	core::{BSTR, Interface, Result as WinResult},
};

use crate::media::foundation;

const OUTPUT_FORMAT_BGRA: u32 = 87;

#[derive(Default)]
struct Reports {
	loaded: AtomicBool,
	ended: AtomicBool,
	error: AtomicU32,
}

#[windows::core::implement(IMFMediaEngineNotify)]
struct Notify {
	reports: Arc<Reports>,
}

impl IMFMediaEngineNotify_Impl for Notify_Impl {
	fn EventNotify(&self, event: u32, param1: usize, _param2: u32) -> WinResult<()> {
		let event = event as i32;
		if event == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA.0 {
			self.reports.loaded.store(true, Ordering::Release);
		} else if event == MF_MEDIA_ENGINE_EVENT_ENDED.0 {
			self.reports.ended.store(true, Ordering::Release);
		} else if event == MF_MEDIA_ENGINE_EVENT_ERROR.0 {
			self.reports.error.store((param1 as u32).max(1), Ordering::Release);
		}
		Ok(())
	}
}

pub(crate) struct Changes {
	pub(crate) loaded: bool,
	pub(crate) ended: bool,
	pub(crate) error: Option<&'static str>,
}

pub(crate) struct MediaEngine {
	engine: IMFMediaEngine,
	hwnd: HWND,
	reports: Arc<Reports>,
}

impl MediaEngine {
	/// Starts loading `path` (playback waits for [`Self::play`]). Must be
	/// called on the UI thread, which wx has already set up for COM.
	pub(crate) fn open(hwnd: *mut std::ffi::c_void, path: &str) -> crate::error::Result<Self> {
		foundation::ensure_started();
		let reports = Arc::new(Reports::default());
		unsafe {
			let factory: IMFMediaEngineClassFactory =
				CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
			let mut attrs: Option<IMFAttributes> = None;
			MFCreateAttributes(&mut attrs, 3)?;
			let attrs = attrs.ok_or_else(|| crate::error::err!("MFCreateAttributes returned no attributes"))?;
			let notify: IMFMediaEngineNotify = Notify { reports: reports.clone() }.into();
			attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
			attrs.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND, hwnd as u64)?;
			attrs.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, OUTPUT_FORMAT_BGRA)?;
			let engine = factory.CreateInstance(0, &attrs)?;
			engine.SetSource(&BSTR::from(path))?;
			Ok(Self { engine, hwnd: HWND(hwnd), reports })
		}
	}

	pub(crate) fn take_reports(&self) -> Changes {
		let error = match self.reports.error.swap(0, Ordering::AcqRel) {
			0 => None,
			4 => Some(crate::i18n::tr("This video's format can't be played here.")),
			3 => Some(crate::i18n::tr("The video couldn't be decoded. The file may be damaged.")),
			_ => Some(crate::i18n::tr("The video couldn't be played.")),
		};
		Changes {
			loaded: self.reports.loaded.swap(false, Ordering::AcqRel),
			ended: self.reports.ended.swap(false, Ordering::AcqRel),
			error,
		}
	}

	pub(crate) fn play(&self) {
		unsafe {
			let _ = self.engine.Play();
		}
	}

	pub(crate) fn pause(&self) {
		unsafe {
			let _ = self.engine.Pause();
		}
	}

	pub(crate) fn is_paused(&self) -> bool {
		unsafe { self.engine.IsPaused().as_bool() }
	}

	pub(crate) fn is_ended(&self) -> bool {
		unsafe { self.engine.IsEnded().as_bool() }
	}

	pub(crate) fn position(&self) -> f64 {
		let at = unsafe { self.engine.GetCurrentTime() };
		if at.is_finite() { at.max(0.0) } else { 0.0 }
	}

	pub(crate) fn duration(&self) -> f64 {
		let length = unsafe { self.engine.GetDuration() };
		if length.is_finite() { length.max(0.0) } else { 0.0 }
	}

	pub(crate) fn seek(&self, seconds: f64) {
		let length = self.duration();
		let target = if length > 0.0 { seconds.clamp(0.0, length) } else { seconds.max(0.0) };
		unsafe {
			let _ = self.engine.SetCurrentTime(target);
		}
	}

	pub(crate) fn set_volume(&self, volume: f64) {
		unsafe {
			let _ = self.engine.SetVolume(volume.clamp(0.0, 1.0));
		}
	}

	pub(crate) fn is_muted(&self) -> bool {
		unsafe { self.engine.GetMuted().as_bool() }
	}

	pub(crate) fn set_muted(&self, muted: bool) {
		unsafe {
			let _ = self.engine.SetMuted(muted);
		}
	}

	pub(crate) fn set_rate(&self, rate: f64) {
		unsafe {
			let _ = self.engine.SetPlaybackRate(rate);
		}
	}

	pub(crate) fn fit_to_window(&self) {
		unsafe {
			let mut rect = RECT::default();
			if GetClientRect(self.hwnd, &mut rect).is_err() {
				return;
			}
			if let Ok(ex) = self.engine.cast::<IMFMediaEngineEx>() {
				let _ = ex.UpdateVideoStream(None, Some(&rect), None);
			}
		}
	}
}

impl Drop for MediaEngine {
	fn drop(&mut self) {
		unsafe {
			let _ = self.engine.Shutdown();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	pub(crate) fn private_mb() -> f64 {
		use windows::Win32::System::{
			ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
			Threading::GetCurrentProcess,
		};
		let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
		unsafe {
			let _ = GetProcessMemoryInfo(
				GetCurrentProcess(),
				&mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
				size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
			);
		}
		counters.PrivateUsage as f64 / 1048576.0
	}

	fn working_set_mb() -> f64 {
		use windows::Win32::System::{
			ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
			Threading::GetCurrentProcess,
		};
		let mut counters = PROCESS_MEMORY_COUNTERS::default();
		unsafe {
			let _ =
				GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size_of::<PROCESS_MEMORY_COUNTERS>() as u32);
		}
		counters.WorkingSetSize as f64 / 1048576.0
	}

	fn loaded_modules() -> String {
		use windows::Win32::System::{
			ProcessStatus::{EnumProcessModules, GetModuleBaseNameW},
			Threading::GetCurrentProcess,
		};
		let mut modules = [windows::Win32::Foundation::HMODULE::default(); 512];
		let mut needed = 0u32;
		let mut names = Vec::new();
		unsafe {
			let process = GetCurrentProcess();
			if EnumProcessModules(process, modules.as_mut_ptr(), size_of_val(&modules) as u32, &mut needed).is_ok() {
				for module in &modules[..(needed as usize / size_of::<windows::Win32::Foundation::HMODULE>()).min(512)]
				{
					let mut name = [0u16; 128];
					let len = GetModuleBaseNameW(process, Some(*module), &mut name) as usize;
					let name = String::from_utf16_lossy(&name[..len]).to_lowercase();
					if name.contains("d3d")
						|| name.contains("nv")
						|| name.contains("ig")
						|| name.contains("amd")
						|| name.contains("msmpeg")
						|| name.contains("mfh264")
						|| name.contains("dxva")
						|| name.contains("mfplat")
					{
						names.push(name);
					}
				}
			}
		}
		names.join(", ")
	}

	fn pump_for(ms: u64) {
		use windows::Win32::UI::WindowsAndMessaging::{
			DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
		};
		let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
		while std::time::Instant::now() < end {
			let mut msg = MSG::default();
			unsafe {
				while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
					let _ = TranslateMessage(&msg);
					DispatchMessageW(&msg);
				}
			}
			std::thread::sleep(std::time::Duration::from_millis(10));
		}
	}

	/// Plays a real video in a window, closes it, and reports what the process
	/// still holds. Run by hand: `WHATSNAP_TEST_VIDEO=<file.mp4> cargo test
	/// --bin whatSnap -- --ignored --nocapture video_memory`.
	#[test]
	#[ignore]
	fn video_memory_is_given_back_after_closing() {
		use windows::{
			Win32::{
				System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
				UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_OVERLAPPEDWINDOW, WS_VISIBLE},
			},
			core::w,
		};
		let path = std::env::var("WHATSNAP_TEST_VIDEO").expect("set WHATSNAP_TEST_VIDEO");
		unsafe {
			let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
		}
		let start = private_mb();
		foundation::ensure_started();
		eprintln!("after MFStartup: {:.0} MB private, {:.0} MB working set", private_mb(), working_set_mb());
		for round in 1..=3 {
			let hwnd = unsafe {
				CreateWindowExW(
					Default::default(),
					w!("STATIC"),
					w!("video"),
					WS_OVERLAPPEDWINDOW | WS_VISIBLE,
					0,
					0,
					1280,
					720,
					None,
					None,
					None,
					None,
				)
			}
			.unwrap();
			let engine = MediaEngine::open(hwnd.0, &path).unwrap();
			engine.set_muted(true);
			engine.play();
			pump_for(6000);
			let playing = private_mb();
			drop(engine);
			unsafe {
				let _ = DestroyWindow(hwnd);
			}
			pump_for(1500);
			let closed = private_mb();
			let closed_ws = working_set_mb();
			crate::platform::memory::give_back_memory();
			eprintln!(
				"round {round}: start {start:.0} MB, playing {playing:.0} MB, closed {closed:.0} MB private / {closed_ws:.0} MB working set, after give_back {:.0} MB private / {:.0} MB working set",
				private_mb(),
				working_set_mb()
			);
			let modules = loaded_modules();
			eprintln!("modules: {modules}");
		}
	}
}
