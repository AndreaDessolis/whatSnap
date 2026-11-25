//! Getting keyboard focus back to whatSnap after another window took it, such
//! as the Windows Hello prompt. When that prompt closes, Windows can leave
//! whatSnap disabled and not in front, so keys go nowhere until Alt+Tab.

use windows::Win32::{
	Foundation::HWND,
	System::Threading::{AttachThreadInput, GetCurrentThreadId},
	UI::{
		Input::KeyboardAndMouse::{EnableWindow, GetFocus, IsWindowEnabled, SetFocus},
		WindowsAndMessaging::{
			GW_OWNER, GetForegroundWindow, GetWindow, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
			SetForegroundWindow,
		},
	},
};

/// Shows a modal dialog, then gives the keyboard back.
///
/// `owner` is the window to return to: the main window, or the dialog the
/// error came from. With `take_focus`, whatSnap is brought forward before the
/// dialog opens, for dialogs that arrive on their own (an error from the
/// service while the user is in another app). Focus then goes back to the
/// control that had it before the dialog, when that's still there.
pub fn show_modal_and_refocus(owner: HWND, take_focus: bool, show: impl FnOnce() -> i32) -> i32 {
	let previous = unsafe { GetFocus() };
	if take_focus {
		bring_to_front(owner);
	}
	let result = show();
	bring_to_front(owner);
	unsafe {
		if !previous.is_invalid()
			&& IsWindow(Some(previous)).as_bool()
			&& IsWindowVisible(previous).as_bool()
			&& IsWindowEnabled(previous).as_bool()
		{
			let _ = SetFocus(Some(previous));
		}
	}
	result
}

/// The window that owns `hwnd` (a dialog's main window), or null.
pub fn owner_of(hwnd: HWND) -> HWND {
	unsafe { GetWindow(hwnd, GW_OWNER) }.unwrap_or_default()
}

/// Makes `hwnd` enabled and in front, so it gets the keyboard again. Call it
/// on the UI thread, once the other window has closed.
pub fn bring_to_front(hwnd: HWND) {
	unsafe {
		if !IsWindow(Some(hwnd)).as_bool() {
			return;
		}
		if !IsWindowEnabled(hwnd).as_bool() {
			let _ = EnableWindow(hwnd, true);
		}
		if GetForegroundWindow() == hwnd {
			return;
		}
		if SetForegroundWindow(hwnd).as_bool() {
			return;
		}
		// Windows only lets the process that has the foreground window (or
		// was last given input) move focus. Sharing input with that window's
		// thread for a moment makes the request count; it's undone at once.
		let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
		let me = GetCurrentThreadId();
		let attached = foreground_thread != 0
			&& foreground_thread != me
			&& AttachThreadInput(me, foreground_thread, true).as_bool();
		let _ = SetForegroundWindow(hwnd);
		if attached {
			let _ = AttachThreadInput(me, foreground_thread, false);
		}
	}
}

/// The window the user was last in that belongs to another program, kept so a
/// notification button can hand the keyboard back to it. Pressing a button in
/// a notification moves the foreground to the notification, and when it
/// closes Windows leaves nothing focused (a screen reader says "unknown").
static PREVIOUS: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

fn class_name(hwnd: HWND) -> String {
	let mut name = [0u16; 128];
	let len = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut name) };
	usize::try_from(len).map_or_else(|_| String::new(), |len| String::from_utf16_lossy(&name[..len.min(name.len())]))
}

fn is_notification_host(hwnd: HWND) -> bool {
	class_name(hwnd) == "Windows.UI.Core.CoreWindow"
}

/// Shell surfaces that aren't somewhere the user works: the taskbar, flyouts
/// and the notification host. The desktop itself (Progman, WorkerW) counts as
/// a place to go back to.
fn is_shell_surface(hwnd: HWND) -> bool {
	matches!(
		class_name(hwnd).as_str(),
		"Shell_TrayWnd"
			| "Shell_SecondaryTrayWnd"
			| "Windows.UI.Core.CoreWindow"
			| "XamlExplorerHostIslandWindow"
			| "TopLevelWindowForOverflowXamlIsland"
	)
}

fn is_desktop(hwnd: HWND) -> bool {
	matches!(class_name(hwnd).as_str(), "Progman" | "WorkerW")
}

fn process_id(hwnd: HWND) -> u32 {
	let mut pid = 0u32;
	unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
	pid
}

unsafe extern "system" fn on_foreground(
	_hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
	_event: u32,
	hwnd: HWND,
	_object: i32,
	_child: i32,
	_thread: u32,
	_time: u32,
) {
	if hwnd.is_invalid() || is_shell_surface(hwnd) {
		return;
	}
	if process_id(hwnd) != std::process::id() {
		PREVIOUS.store(hwnd.0 as isize, std::sync::atomic::Ordering::SeqCst);
	}
}

/// Starts remembering which other program's window was in front.
pub fn track_foreground() {
	use windows::Win32::UI::{
		Accessibility::SetWinEventHook,
		WindowsAndMessaging::{
			DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GetMessageW, MSG, TranslateMessage, WINEVENT_OUTOFCONTEXT,
		},
	};
	crate::util::spawn_named("focus watch", || unsafe {
		let hook = SetWinEventHook(
			EVENT_SYSTEM_FOREGROUND,
			EVENT_SYSTEM_FOREGROUND,
			None,
			Some(on_foreground),
			0,
			0,
			WINEVENT_OUTOFCONTEXT,
		);
		if hook.is_invalid() {
			return;
		}
		let mut msg = MSG::default();
		while GetMessageW(&mut msg, None, 0, 0).as_bool() {
			let _ = TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
	});
}

/// Visible and not cloaked (Windows cloaks closed shell surfaces and windows
/// on other virtual desktops).
fn shown(hwnd: HWND) -> bool {
	use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
	if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
		return false;
	}
	let mut cloaked = 0u32;
	let read =
		unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32) };
	read.is_err() || cloaked == 0
}

/// Whether the window is one the user works in: shown, not minimized, not a
/// tool, shell or desktop window, and not whatSnap's.
fn is_app_window(hwnd: HWND) -> bool {
	use windows::Win32::UI::WindowsAndMessaging::{
		GWL_EXSTYLE, GetWindowLongPtrW, GetWindowTextLengthW, IsIconic, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
	};
	if hwnd.is_invalid() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() || !shown(hwnd) {
		return false;
	}
	if unsafe { IsIconic(hwnd) }.as_bool() {
		return false;
	}
	let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
	if ex_style & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) as isize != 0 {
		return false;
	}
	if !owner_of(hwnd).is_invalid() || unsafe { GetWindowTextLengthW(hwnd) } == 0 {
		return false;
	}
	process_id(hwnd) != std::process::id() && !is_shell_surface(hwnd) && !is_desktop(hwnd)
}

/// The window highest in the z-order that `matches` accepts, if any.
fn topmost(matches: fn(HWND) -> bool) -> Option<HWND> {
	use windows::Win32::{Foundation::LPARAM, UI::WindowsAndMessaging::EnumWindows};
	struct Search {
		matches: fn(HWND) -> bool,
		found: HWND,
	}
	unsafe extern "system" fn visit(hwnd: HWND, search: LPARAM) -> windows::core::BOOL {
		let search = unsafe { &mut *(search.0 as *mut Search) };
		if (search.matches)(hwnd) {
			search.found = hwnd;
			return false.into();
		}
		true.into()
	}
	let mut search = Search { matches, found: HWND::default() };
	unsafe {
		let _ = EnumWindows(Some(visit), LPARAM((&raw mut search) as isize));
	}
	(!search.found.is_invalid()).then_some(search.found)
}

/// The notification center, if it's open: the shell's notification host still
/// showing a window once the pressed notification has gone.
fn notification_center() -> Option<HWND> {
	fn is_center(hwnd: HWND) -> bool {
		is_notification_host(hwnd) && shown(hwnd) && process_name(hwnd).eq_ignore_ascii_case("ShellExperienceHost.exe")
	}
	topmost(is_center)
}

/// The file name of the window's program, such as `ShellExperienceHost.exe`.
fn process_name(hwnd: HWND) -> String {
	use windows::{
		Win32::{
			Foundation::CloseHandle,
			System::Threading::{
				OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
			},
		},
		core::PWSTR,
	};
	unsafe {
		let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id(hwnd)) else {
			return String::new();
		};
		let mut buffer = [0u16; 1024];
		let mut len = buffer.len() as u32;
		let read = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len);
		let _ = CloseHandle(process);
		if read.is_err() {
			return String::new();
		}
		let path = String::from_utf16_lossy(&buffer[..len as usize]);
		path.rsplit('\\').next().unwrap_or_default().to_owned()
	}
}

/// Where the user was: the window tracked as last in front while it's still
/// there to go back to, else the topmost app window, else the desktop.
fn last_place() -> Option<HWND> {
	use windows::{Win32::UI::WindowsAndMessaging::FindWindowW, core::w};
	let previous = HWND(PREVIOUS.load(std::sync::atomic::Ordering::SeqCst) as *mut _);
	let previous_usable = !previous.is_invalid()
		&& (is_app_window(previous) || (unsafe { IsWindow(Some(previous)) }.as_bool() && is_desktop(previous)));
	if previous_usable {
		return Some(previous);
	}
	topmost(is_app_window)
		.or_else(|| unsafe { FindWindowW(w!("Progman"), None) }.ok().filter(|hwnd| !hwnd.is_invalid()))
}

/// Moves the foreground to `hwnd` from the background: directly if Windows
/// allows it, else by sharing input with the foreground thread for a moment,
/// else the way Alt+Tab switches.
fn switch_to(hwnd: HWND) {
	use windows::Win32::UI::WindowsAndMessaging::SwitchToThisWindow;
	unsafe {
		if SetForegroundWindow(hwnd).as_bool() {
			return;
		}
		let current = GetForegroundWindow();
		let foreground_thread = if current.is_invalid() { 0 } else { GetWindowThreadProcessId(current, None) };
		let me = GetCurrentThreadId();
		let attached = foreground_thread != 0
			&& foreground_thread != me
			&& AttachThreadInput(me, foreground_thread, true).as_bool();
		let moved = SetForegroundWindow(hwnd).as_bool();
		if attached {
			let _ = AttachThreadInput(me, foreground_thread, false);
		}
		if !moved {
			SwitchToThisWindow(hwnd, true);
		}
	}
}

/// After a notification button that works without whatSnap's window: waits for
/// the notification to close and, if that left focus nowhere, puts it back on
/// the notification center when that's still open, otherwise where the user
/// was (their last app, or the desktop). Never on whatSnap, and nothing at all
/// once the user is in an app.
pub fn restore_previous_after_toast() {
	crate::util::spawn_named("focus restore", || {
		for _ in 0..12 {
			std::thread::sleep(std::time::Duration::from_millis(125));
			if is_app_window(unsafe { GetForegroundWindow() }) {
				return;
			}
			if let Some(target) = notification_center().or_else(last_place) {
				switch_to(target);
			}
		}
	});
}
