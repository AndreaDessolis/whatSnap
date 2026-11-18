use windows::{
	Win32::Media::MediaFoundation::{
		IMFActivate, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
		MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
		MFCreateAttributes, MFEnumDeviceSources,
	},
	core::PWSTR,
};

use crate::media::foundation;

pub struct CameraDevice {
	pub name: String,
	pub symbolic_link: String,
}

pub(crate) fn read_allocated_string(activate: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
	unsafe {
		let mut ptr = PWSTR::null();
		let mut len: u32 = 0;
		activate.GetAllocatedString(key, &mut ptr, &mut len).ok()?;
		if ptr.is_null() {
			return None;
		}
		let s = ptr.to_string().ok();
		windows::Win32::System::Com::CoTaskMemFree(Some(ptr.0 as *const core::ffi::c_void));
		s
	}
}

/// Enumerates the video capture devices and takes ownership of every
/// `IMFActivate` the call returns. `MFEnumDeviceSources` hands back one
/// reference per entry plus the array itself; each entry is moved out of the
/// array (so dropping the returned `Vec` releases it) before the array is
/// freed, on every path.
pub(crate) fn enum_video_capture_activates() -> crate::error::Result<Vec<IMFActivate>> {
	foundation::ensure_started();
	unsafe {
		let mut attributes: Option<windows::Win32::Media::MediaFoundation::IMFAttributes> = None;
		MFCreateAttributes(&mut attributes, 1)?;
		let attributes = attributes.ok_or_else(|| crate::error::err!("MFCreateAttributes returned no attributes"))?;
		attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;

		let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
		let mut count: u32 = 0;
		MFEnumDeviceSources(&attributes, &mut activates, &mut count)?;
		if activates.is_null() {
			return Ok(Vec::new());
		}
		let mut owned = Vec::with_capacity(count as usize);
		for i in 0..count as usize {
			// Move the reference out; the array slot is never read again.
			if let Some(activate) = std::ptr::read(activates.add(i)) {
				owned.push(activate);
			}
		}
		windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const core::ffi::c_void));
		Ok(owned)
	}
}

pub fn list_cameras() -> Vec<CameraDevice> {
	let Ok(activates) = enum_video_capture_activates() else { return Vec::new() };
	let mut devices = Vec::with_capacity(activates.len());
	for activate in &activates {
		let Some(name) = read_allocated_string(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME) else { continue };
		let Some(symbolic_link) =
			read_allocated_string(activate, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
		else {
			continue;
		};
		devices.push(CameraDevice { name, symbolic_link });
	}
	devices
}

pub fn camera_names() -> Vec<String> {
	list_cameras().into_iter().map(|d| d.name).collect()
}

pub fn resolve_camera_symbolic_link(name: Option<&str>) -> Option<String> {
	let devices = list_cameras();
	if let Some(name) = name
		&& let Some(d) = devices.iter().find(|d| d.name == name)
	{
		return Some(d.symbolic_link.clone());
	}
	devices.into_iter().next().map(|d| d.symbolic_link)
}
