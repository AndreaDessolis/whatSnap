//! Giving memory back to Windows once something big is closed.

use windows::Win32::System::{
	Memory::{GetProcessHeaps, HEAP_FLAGS, HeapCompact, SETPROCESSWORKINGSETSIZEEX_FLAGS, SetProcessWorkingSetSizeEx},
	Threading::GetCurrentProcess,
};

/// After a video player (or anything else holding large buffers) closes: the
/// heaps hand their freed, now-empty pages back to Windows, and pages the app
/// isn't touching leave its working set, which is the memory Task Manager
/// shows. Nothing in use is lost; a page needed again is simply read back.
pub(crate) fn give_back_memory() {
	unsafe {
		let mut heaps = [Default::default(); 64];
		let count = GetProcessHeaps(&mut heaps) as usize;
		for heap in &heaps[..count.min(heaps.len())] {
			HeapCompact(*heap, HEAP_FLAGS(0));
		}
		let _ = SetProcessWorkingSetSizeEx(
			GetCurrentProcess(),
			usize::MAX,
			usize::MAX,
			SETPROCESSWORKINGSETSIZEEX_FLAGS(0),
		);
	}
}

/// `give_back_memory` a couple of seconds from now, once a closed player's
/// decoding threads have wound down.
pub(crate) fn give_back_memory_soon() {
	crate::util::spawn_named("memory give back", || {
		std::thread::sleep(std::time::Duration::from_secs(2));
		give_back_memory();
	});
}
