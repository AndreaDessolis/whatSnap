use std::{
	cell::RefCell,
	collections::{HashMap, VecDeque},
	sync::mpsc,
	time::{Duration, Instant},
};

use super::gdi::Bitmap;

const MAX_BITMAPS: usize = 256;
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Pictures waiting to be decoded at most; older requests are dropped.
const MAX_QUEUED_DECODES: usize = 48;

pub(crate) enum Avatar<'a> {
	Picture(&'a Bitmap),
	Nothing,
	Waiting,
}

#[derive(Default)]
struct Cache {
	paths: HashMap<String, Option<String>>,
	/// Decoded pictures by (jid, size): the chat list, header and people
	/// lists may draw the same person at different sizes.
	bitmaps: HashMap<(String, i32), Bitmap>,
	order: VecDeque<(String, i32)>,
	size: i32,
	asked: HashMap<String, Instant>,
	pending: Vec<String>,
	flush_scheduled: bool,
	tx: Option<mpsc::Sender<String>>,
	on_change: Vec<Box<dyn Fn()>>,
	/// Pictures being decoded off the UI thread: (jid, size) -> path.
	decoding: HashMap<(String, i32), String>,
	repaint_scheduled: bool,
}

thread_local! {
	static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

pub(crate) fn init(tx: mpsc::Sender<String>) {
	CACHE.with(|c| c.borrow_mut().tx = Some(tx));
}

pub(crate) fn size() -> Option<i32> {
	CACHE.with(|c| Some(c.borrow().size).filter(|&s| s > 0))
}

pub(crate) fn on_change(callback: impl Fn() + 'static) {
	CACHE.with(|c| c.borrow_mut().on_change.push(Box::new(callback)));
}

pub(crate) fn path_of(jid: &str) -> Option<String> {
	if jid.is_empty() {
		return None;
	}
	CACHE.with(|cache| {
		let mut c = cache.borrow_mut();
		match c.paths.get(jid) {
			Some(path) => path.clone(),
			None => {
				ask(&mut c, jid);
				None
			}
		}
	})
}

pub(crate) fn with_avatar<R>(jid: &str, size: i32, draw: impl FnOnce(Avatar) -> R) -> R {
	if jid.is_empty() {
		return draw(Avatar::Nothing);
	}
	CACHE.with(|cache| {
		let mut c = cache.borrow_mut();
		c.size = size;
		let path = match c.paths.get(jid) {
			Some(Some(path)) => path.clone(),
			Some(None) => return draw(Avatar::Nothing),
			None => {
				ask(&mut c, jid);
				return draw(Avatar::Waiting);
			}
		};
		let key = (jid.to_string(), size);
		if !c.bitmaps.contains_key(&key) {
			// Decoded off the UI thread, so scrolling never waits on it; the
			// list repaints once it's ready.
			if c.decoding.get(&key) != Some(&path) {
				c.decoding.insert(key, path.clone());
				let _ = decoder().send((jid.to_string(), path, size));
			}
			return draw(Avatar::Waiting);
		}
		// Keep recently drawn pictures, rather than evicting them in insertion order.
		if c.order.back() != Some(&key) {
			c.order.retain(|k| k != &key);
			c.order.push_back(key.clone());
		}
		draw(c.bitmaps.get(&key).map_or(Avatar::Nothing, Avatar::Picture))
	})
}

type DecodeRequest = (String, String, i32);

fn decoder() -> &'static mpsc::Sender<DecodeRequest> {
	static DECODER: std::sync::OnceLock<mpsc::Sender<DecodeRequest>> = std::sync::OnceLock::new();
	DECODER.get_or_init(|| {
		let (tx, rx) = mpsc::channel::<DecodeRequest>();
		crate::util::spawn_named("avatar decoder", move || {
			use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
			let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
			let mut queue: Vec<DecodeRequest> = Vec::new();
			loop {
				if queue.is_empty() {
					match rx.recv() {
						Ok(request) => queue.push(request),
						Err(_) => break,
					}
				}
				queue.extend(rx.try_iter());
				// Fast scrolling asks for every row passed; only the latest
				// can still be showing. The rest are asked again if repainted.
				if queue.len() > MAX_QUEUED_DECODES {
					let stale: Vec<DecodeRequest> = queue.drain(..queue.len() - MAX_QUEUED_DECODES).collect();
					wxdragon::call_after(Box::new(move || {
						CACHE.with(|cache| {
							let mut c = cache.borrow_mut();
							for (jid, path, size) in stale {
								let key = (jid, size);
								if c.decoding.get(&key) == Some(&path) {
									c.decoding.remove(&key);
								}
							}
						});
					}));
				}
				let Some((jid, path, size)) = queue.pop() else { continue };
				let pixels = crate::media::image_decode::decode_rgba(&path, (size * 2) as u32)
					.ok()
					.and_then(|(w, h, rgba)| super::gdi::circle_crop_rgba(w, h, &rgba, size));
				wxdragon::call_after(Box::new(move || decoded(jid, path, size, pixels)));
			}
			if initialized {
				unsafe { CoUninitialize() };
			}
		});
		tx
	})
}

fn decoded(jid: String, path: String, size: i32, pixels: Option<Vec<u8>>) {
	let repaint = CACHE.with(|cache| {
		let mut c = cache.borrow_mut();
		let key = (jid, size);
		if c.decoding.get(&key) != Some(&path) {
			return false;
		}
		c.decoding.remove(&key);
		let jid = key.0.clone();
		if c.paths.get(&jid) != Some(&Some(path)) {
			return false;
		}
		match pixels.and_then(|rgba| Bitmap::from_rgba(size, size, &rgba)) {
			Some(bitmap) => {
				c.order.retain(|k| k != &key);
				while c.bitmaps.len() >= MAX_BITMAPS
					&& let Some(old) = c.order.pop_front()
				{
					c.bitmaps.remove(&old);
				}
				c.order.push_back(key.clone());
				c.bitmaps.insert(key, bitmap);
			}
			// An unreadable file: draw the silhouette, don't retry each paint.
			None => {
				c.paths.insert(jid, None);
			}
		}
		!std::mem::replace(&mut c.repaint_scheduled, true)
	});
	// One repaint for every picture decoded in a row, after all of them.
	if repaint {
		wxdragon::call_after(Box::new(|| {
			CACHE.with(|c| c.borrow_mut().repaint_scheduled = false);
			changed();
		}));
	}
}

fn ask(c: &mut Cache, jid: &str) {
	if jid.is_empty() || c.asked.get(jid).is_some_and(|t| t.elapsed() < RETRY_AFTER) {
		return;
	}
	c.asked.insert(jid.to_string(), Instant::now());
	c.pending.push(jid.to_string());
	if c.flush_scheduled {
		return;
	}
	c.flush_scheduled = true;
	wxdragon::call_after(Box::new(|| {
		CACHE.with(|cache| {
			let mut c = cache.borrow_mut();
			c.flush_scheduled = false;
			let jids = std::mem::take(&mut c.pending);
			if let Some(tx) = &c.tx
				&& !jids.is_empty()
			{
				let payload = serde_json::json!({ "action": "get_avatar_previews", "data": { "jids": jids } });
				let _ = tx.send(payload.to_string());
			}
		});
	}));
}

/// Drops every size of `jid`'s picture, decoded or on its way.
fn forget_pictures(c: &mut Cache, jid: &str) {
	c.bitmaps.retain(|(j, _), _| j != jid);
	c.order.retain(|(j, _)| j != jid);
	c.decoding.retain(|(j, _), _| j != jid);
}

fn changed() {
	let callbacks = CACHE.with(|c| std::mem::take(&mut c.borrow_mut().on_change));
	for callback in &callbacks {
		callback();
	}
	CACHE.with(|c| {
		let mut c = c.borrow_mut();
		let added = std::mem::replace(&mut c.on_change, callbacks);
		c.on_change.extend(added);
	});
}

pub(crate) fn on_previews(data: &serde_json::Value) {
	let Some(items) = data.get("items").and_then(|i| i.as_array()) else { return };
	CACHE.with(|cache| {
		let mut c = cache.borrow_mut();
		for item in items {
			let Some(jid) = item.get("jid").and_then(|j| j.as_str()) else { continue };
			let path = item.get("path").and_then(|p| p.as_str()).filter(|p| !p.is_empty()).map(str::to_string);
			c.asked.remove(jid);
			forget_pictures(&mut c, jid);
			c.paths.insert(jid.to_string(), path);
		}
	});
	changed();
}

pub(crate) fn on_updated(jid: &str) {
	CACHE.with(|cache| {
		let mut c = cache.borrow_mut();
		c.paths.remove(jid);
		forget_pictures(&mut c, jid);
		c.asked.remove(jid);
	});
	changed();
}
