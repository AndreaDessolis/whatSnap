use std::{
	io::Write,
	sync::atomic::{AtomicU32, Ordering},
};

use windows::{
	Win32::{
		Foundation::HMODULE,
		System::{
			Diagnostics::Debug::{
				AddVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, RtlCaptureStackBackTrace,
				SetUnhandledExceptionFilter,
			},
			LibraryLoader::{
				GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
				GetModuleFileNameW, GetModuleHandleExW,
			},
		},
	},
	core::PCWSTR,
};

use crate::platform::paths;

pub(crate) fn install() {
	std::panic::set_hook(Box::new(|info| append("panic.log", &format!("{info}"))));
	unsafe {
		SetUnhandledExceptionFilter(Some(on_native_crash));
		AddVectoredExceptionHandler(1, Some(on_fault_raised));
	}
}

const FATAL_CODES: [u32; 5] = [0xC000_0005, 0xC000_001D, 0xC000_00FD, 0xC000_0374, 0xC000_0409];
static RAISED: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn on_fault_raised(info: *mut EXCEPTION_POINTERS) -> i32 {
	if let Some(record) = unsafe { info.as_ref().and_then(|info| info.ExceptionRecord.as_ref()) } {
		let code = record.ExceptionCode.0 as u32;
		if let Some(i) = FATAL_CODES.iter().position(|&c| c == code)
			&& RAISED.fetch_or(1 << i, Ordering::Relaxed) & (1 << i) == 0
		{
			let address = record.ExceptionAddress as usize;
			let info = &record.ExceptionInformation[..(record.NumberParameters as usize).min(2)];
			append(
				"crash.log",
				&format!(
					"fault {code:#010x} raised at {} ({info:x?}) in build {}, stack:{}
named stack:
{}",
					describe(address),
					env!("WHATSNAP_SHORT_HASH"),
					stack(),
					named_stack()
				),
			);
		}
	}
	EXCEPTION_CONTINUE_SEARCH
}

fn stack() -> String {
	let mut frames = [std::ptr::null_mut(); 32];
	let count = unsafe { RtlCaptureStackBackTrace(0, &mut frames, None) } as usize;
	frames[..count].iter().map(|&frame| format!("\n    {}", describe(frame as usize))).collect()
}

/// The same stack with function names and source lines, read from the .pdb
/// beside the exe: a build of your own has one, so its crashes name the code.
/// A copy without it gets addresses only, which the raw stack above already has.
fn named_stack() -> String {
	let trace = std::backtrace::Backtrace::force_capture().to_string();
	// The frames of the handler and the exception dispatch come first and say
	// nothing about the crash; 40 frames is plenty for what's below them.
	trace.lines().take(80).collect::<Vec<_>>().join(
		"
",
	)
}

fn describe(address: usize) -> String {
	match module_of(address) {
		Some((name, base)) => format!("{name}+{:#x}", address - base),
		None => format!("{address:#x}"),
	}
}

fn append(file: &str, what: &str) {
	let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
	let thread = std::thread::current();
	let line = format!(
		"[{}.{:03}] thread {}: {what}\n",
		now.as_secs(),
		now.subsec_millis(),
		thread.name().unwrap_or("unnamed")
	);
	if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(paths::log_dir().join(file)) {
		let _ = f.write_all(line.as_bytes());
	}
}

unsafe extern "system" fn on_native_crash(info: *const EXCEPTION_POINTERS) -> i32 {
	let record = unsafe { info.as_ref().and_then(|info| info.ExceptionRecord.as_ref()) };
	if let Some(record) = record {
		let place = describe(record.ExceptionAddress as usize);
		append("crash.log", &format!("unhandled exception {:#010x} at {place}", record.ExceptionCode.0 as u32));
	}
	// Windows still reports the crash and ends the process as before.
	EXCEPTION_CONTINUE_SEARCH
}

fn module_of(address: usize) -> Option<(String, usize)> {
	unsafe {
		let mut module = HMODULE::default();
		GetModuleHandleExW(
			GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
			PCWSTR(address as *const u16),
			&mut module,
		)
		.ok()?;
		let mut path = [0u16; 260];
		let len = GetModuleFileNameW(Some(module), &mut path) as usize;
		let path = String::from_utf16_lossy(&path[..len.min(path.len())]);
		let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
		Some((name, module.0 as usize))
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn the_named_stack_names_the_code() {
		let stack = super::named_stack();
		assert!(stack.contains("the_named_stack_names_the_code"), "no function names in:\n{stack}");
		assert!(stack.contains("crash_log.rs"), "no source lines in:\n{stack}");
	}
}
