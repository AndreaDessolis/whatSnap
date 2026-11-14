//! Windows Graphics Capture: what Chromium, OBS and the Snipping Tool share a
//! screen or window with. It hands over the picture Windows composes for the
//! window or monitor, so GPU-drawn and covered windows come out as they look,
//! and only when it changed. The frames arrive as Direct3D textures; each new
//! one is copied back once into `pixels` (BGRA, top-down) for the encoder.
//!
//! Still black: DRM-protected video, and everything while a screen reader's
//! screen curtain is up - those are kept from every capture by design.

use windows::{
	Graphics::{
		Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
		DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
		SizeInt32,
	},
	Win32::{
		Foundation::{HMODULE, HWND},
		Graphics::{
			Direct3D::D3D_DRIVER_TYPE_HARDWARE,
			Direct3D11::{
				D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
				D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
				ID3D11DeviceContext, ID3D11Texture2D,
			},
			Dxgi::IDXGIDevice,
			Gdi::HMONITOR,
		},
		System::WinRT::{
			Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
			Graphics::Capture::IGraphicsCaptureItemInterop,
		},
	},
	core::{Interface, factory},
};

pub(crate) enum WgcTarget {
	Window(HWND),
	Monitor(HMONITOR),
}

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

pub(crate) struct WgcCapture {
	context: ID3D11DeviceContext,
	device: ID3D11Device,
	winrt_device: IDirect3DDevice,
	pool: Direct3D11CaptureFramePool,
	session: GraphicsCaptureSession,
	size: SizeInt32,
	staging: Option<(ID3D11Texture2D, u32, u32)>,
	/// The newest picture, BGRA top-down, `width` x `height`; empty until
	/// the first frame.
	pub(crate) pixels: Vec<u8>,
	pub(crate) width: usize,
	pub(crate) height: usize,
}

impl WgcCapture {
	pub(crate) fn new(target: WgcTarget) -> crate::error::Result<Self> {
		if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
			crate::error::bail!("Windows Graphics Capture isn't available");
		}
		unsafe {
			let mut device = None;
			let mut context = None;
			D3D11CreateDevice(
				None,
				D3D_DRIVER_TYPE_HARDWARE,
				HMODULE::default(),
				D3D11_CREATE_DEVICE_BGRA_SUPPORT,
				None,
				D3D11_SDK_VERSION,
				Some(&mut device),
				None,
				Some(&mut context),
			)?;
			let device = device.ok_or_else(|| crate::error::err!("no Direct3D device"))?;
			let context = context.ok_or_else(|| crate::error::err!("no Direct3D context"))?;
			let dxgi: IDXGIDevice = device.cast()?;
			let winrt_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
			let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
			let item: GraphicsCaptureItem = match target {
				WgcTarget::Window(hwnd) => interop.CreateForWindow(hwnd)?,
				WgcTarget::Monitor(monitor) => interop.CreateForMonitor(monitor)?,
			};
			let size = item.Size()?;
			let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, FORMAT, 2, size)?;
			let session = pool.CreateCaptureSession(&item)?;
			// The pointer is part of what's shared, as in WhatsApp.
			let _ = session.SetIsCursorCaptureEnabled(true);
			session.StartCapture()?;
			Ok(Self {
				context,
				device,
				winrt_device,
				pool,
				session,
				size,
				staging: None,
				pixels: Vec::new(),
				width: 0,
				height: 0,
			})
		}
	}

	/// Takes the newest frame, if one came since the last call, into
	/// `pixels`. Ok(false): nothing new, the last picture still stands.
	pub(crate) fn update(&mut self) -> crate::error::Result<bool> {
		let mut newest = None;
		while let Ok(frame) = self.pool.TryGetNextFrame() {
			if let Some(older) = newest.replace(frame) {
				let _ = older.Close();
			}
		}
		let Some(frame) = newest else { return Ok(false) };
		let content = frame.ContentSize()?;
		let copied = self.copy_frame(&frame, content);
		let _ = frame.Close();
		if content.Width != self.size.Width || content.Height != self.size.Height {
			// The window was resized: frames from now on at its new size.
			self.size = content;
			self.pool.Recreate(&self.winrt_device, FORMAT, 2, content)?;
		}
		copied?;
		Ok(true)
	}

	fn copy_frame(
		&mut self,
		frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
		content: SizeInt32,
	) -> crate::error::Result<()> {
		unsafe {
			let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
			let texture: ID3D11Texture2D = access.GetInterface()?;
			let mut desc = D3D11_TEXTURE2D_DESC::default();
			texture.GetDesc(&mut desc);
			if self.staging.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (desc.Width, desc.Height)) {
				let staging_desc = D3D11_TEXTURE2D_DESC {
					Usage: D3D11_USAGE_STAGING,
					BindFlags: 0,
					CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
					MiscFlags: 0,
					..desc
				};
				let mut staging = None;
				self.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
				let staging = staging.ok_or_else(|| crate::error::err!("no staging texture"))?;
				self.staging = Some((staging, desc.Width, desc.Height));
			}
			let (staging, _, _) = self.staging.as_ref().expect("just made");
			self.context.CopyResource(staging, &texture);
			let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
			self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
			// The texture can be bigger than the window right after a resize.
			let width = (content.Width.max(0) as u32).min(desc.Width) as usize;
			let height = (content.Height.max(0) as u32).min(desc.Height) as usize;
			self.pixels.resize(width * height * 4, 0);
			let source = mapped.pData as *const u8;
			for row in 0..height {
				std::ptr::copy_nonoverlapping(
					source.add(row * mapped.RowPitch as usize),
					self.pixels.as_mut_ptr().add(row * width * 4),
					width * 4,
				);
			}
			self.context.Unmap(staging, 0);
			self.width = width;
			self.height = height;
		}
		Ok(())
	}
}

impl Drop for WgcCapture {
	fn drop(&mut self) {
		let _ = self.session.Close();
		let _ = self.pool.Close();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A plain red window, captured: the picture that comes back is red.
	/// Needs a desktop; run by hand with `-- --ignored wgc`. (Black under a
	/// screen reader's screen curtain, which hides every capture.)
	#[test]
	#[ignore]
	fn captures_a_window_as_it_looks() {
		use windows::{
			Win32::{
				Graphics::Gdi::{CreateSolidBrush, FillRect, GetDC, ReleaseDC},
				System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize},
				UI::WindowsAndMessaging::{
					CreateWindowExW, DestroyWindow, DispatchMessageW, GetClientRect, MSG, PM_REMOVE, PeekMessageW,
					WS_OVERLAPPEDWINDOW, WS_VISIBLE,
				},
			},
			core::w,
		};
		unsafe {
			let _ = RoInitialize(RO_INIT_MULTITHREADED);
			let hwnd = CreateWindowExW(
				Default::default(),
				w!("STATIC"),
				w!("wgc test"),
				WS_OVERLAPPEDWINDOW | WS_VISIBLE,
				100,
				100,
				400,
				300,
				None,
				None,
				None,
				None,
			)
			.unwrap();
			let paint = || {
				let dc = GetDC(Some(hwnd));
				let mut rect = Default::default();
				let _ = GetClientRect(hwnd, &mut rect);
				FillRect(dc, &rect, CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x0000_00FF)));
				ReleaseDC(Some(hwnd), dc);
			};
			let mut capture = WgcCapture::new(WgcTarget::Window(hwnd)).unwrap();
			let mut got = false;
			for _ in 0..100 {
				let mut msg = MSG::default();
				while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
					DispatchMessageW(&msg);
				}
				paint();
				if capture.update().unwrap() && capture.width > 0 {
					got = true;
					break;
				}
				std::thread::sleep(std::time::Duration::from_millis(30));
			}
			let _ = DestroyWindow(hwnd);
			assert!(got, "no frame in 3 s");
			let (w, h) = (capture.width, capture.height);
			let at = ((h / 2) * w + w / 2) * 4;
			let (b, g, r) = (capture.pixels[at], capture.pixels[at + 1], capture.pixels[at + 2]);
			eprintln!("captured {w}x{h}, middle pixel r={r} g={g} b={b}");
			assert!(w >= 300 && h >= 200, "size {w}x{h}");
			assert!(r > 200 && g < 60 && b < 60, "the middle came out {r},{g},{b}, not red");
		}
	}
}
