//! Opening web links through Explorer instead of from whatSnap itself.
//!
//! When whatSnap starts the browser directly (wxLaunchDefaultBrowser is a
//! plain ShellExecute), a browser that wasn't already running becomes
//! whatSnap's child process, and Task Manager adds all of its memory to
//! whatSnap's total, which made whatSnap look like it was using 700 MB. Asking
//! the desktop's Explorer to run the link (the same trick the share target
//! uses to start whatSnap) makes the browser Explorer's child instead.

use wxdragon::utils::{BrowserLaunchFlags, launch_default_browser};

/// Opens `url` in the default browser. http(s) links go through Explorer;
/// anything else, or a failure, falls back to wxWidgets as before.
pub fn open_url(url: &str) {
	#[cfg(target_os = "windows")]
	{
		let lower = url.trim_start().to_ascii_lowercase();
		if lower.starts_with("http://") || lower.starts_with("https://") {
			match through_explorer(url.trim()) {
				Ok(()) => return,
				Err(e) => eprintln!("[LINK] opening through Explorer failed ({e}); opening it directly"),
			}
		}
	}
	launch_default_browser(url, BrowserLaunchFlags::Default);
}

#[cfg(target_os = "windows")]
fn through_explorer(url: &str) -> windows::core::Result<()> {
	use windows::{
		Win32::{
			System::{
				Com::{CLSCTX_LOCAL_SERVER, CoCreateInstance, IDispatch, IServiceProvider},
				Variant::VARIANT,
			},
			UI::{
				Shell::{
					IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellView, IShellWindows,
					SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
				},
				WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow, SW_SHOWNORMAL},
			},
		},
		core::{BSTR, Interface},
	};
	unsafe {
		let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
		let mut hwnd = 0i32;
		let desktop = windows.FindWindowSW(
			&VARIANT::from(0i32),
			&VARIANT::default(),
			SWC_DESKTOP,
			&mut hwnd,
			SWFO_NEEDDISPATCH,
		)?;
		let provider: IServiceProvider = desktop.cast()?;
		let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
		let view: IShellView = browser.QueryActiveShellView()?;
		let background: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
		let folder_view: IShellFolderViewDual = background.cast()?;
		let shell: IShellDispatch2 = folder_view.Application()?.cast()?;
		// whatSnap is in the foreground; pass that on so the browser window
		// takes focus (and the screen reader follows) instead of only
		// flashing on the taskbar, since Explorer is the one starting it.
		let _ = AllowSetForegroundWindow(ASFW_ANY);
		shell.ShellExecute(
			&BSTR::from(url),
			&VARIANT::default(),
			&VARIANT::default(),
			&VARIANT::from(BSTR::from("open")),
			&VARIANT::from(SW_SHOWNORMAL.0),
		)
	}
}
