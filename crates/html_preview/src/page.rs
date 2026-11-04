//! The page itself (Windows only): a WebView2 inside a small borderless window that floats exactly over the area
//! the tab occupies.
//!
//! Why a separate window and not a child of Zed's own: Zed draws its window through DirectComposition, which paints
//! over any child window, so a page placed as a child is created but never seen. A window owned by Zed's window is
//! composited on its own, stays above Zed, follows it when it is minimised, and has no taskbar button.

use std::path::Path;

use gpui::{Bounds, Pixels, Window};

/// An area in physical pixels: x, y, width, height.
type Area = (i32, i32, u32, u32);

fn area(bounds: Bounds<Pixels>, scale: f32) -> Area {
    let physical = |value: Pixels| (f32::from(value) * scale).round();
    (
        physical(bounds.origin.x) as i32,
        physical(bounds.origin.y) as i32,
        physical(bounds.size.width).max(0.0) as u32,
        physical(bounds.size.height).max(0.0) as u32,
    )
}

#[cfg(target_os = "windows")]
pub use native::Page;

#[cfg(not(target_os = "windows"))]
pub use other::Page;

#[cfg(target_os = "windows")]
mod native {
    use super::*;
    use anyhow::{Context as _, Result, anyhow};
    use std::ffi::c_void;
    use std::num::NonZeroIsize;
    use std::sync::Once;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
        SWP_NOZORDER, SetWindowPos, ShowWindow, WNDCLASSW, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};
    use wry::dpi::{PhysicalPosition, PhysicalSize};
    use wry::raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
    use wry::{NewWindowResponse, Rect, WebContext, WebView, WebViewBuilder};

    const CLASS: PCWSTR = w!("ZedHtmlPreview");

    unsafe extern "system" fn default_window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    /// The floating window the page lives in.
    struct Overlay {
        hwnd: HWND,
    }

    impl Overlay {
        fn new(owner: HWND, (x, y, width, height): Area) -> Result<Self> {
            static REGISTER: Once = Once::new();
            let instance = unsafe { GetModuleHandleW(None) }.context("module handle")?;
            REGISTER.call_once(|| {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(default_window_proc),
                    hInstance: instance.into(),
                    lpszClassName: CLASS,
                    ..Default::default()
                };
                unsafe { RegisterClassW(&class) };
            });
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    CLASS,
                    w!(""),
                    WS_POPUP | WS_CLIPCHILDREN,
                    x,
                    y,
                    width as i32,
                    height as i32,
                    Some(owner),
                    None,
                    Some(instance.into()),
                    None,
                )
            }
            .context("creating the page window")?;
            Ok(Self { hwnd })
        }
    }

    impl Drop for Overlay {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.hwnd).ok() };
        }
    }

    impl HasWindowHandle for Overlay {
        fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
            let hwnd = NonZeroIsize::new(self.hwnd.0 as isize).ok_or(HandleError::Unavailable)?;
            let raw = RawWindowHandle::Win32(Win32WindowHandle::new(hwnd));
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    pub struct Page {
        // Drop order matters: the page goes before the window it lives in.
        webview: WebView,
        _context: WebContext,
        overlay: Overlay,
        owner: HWND,
        screen: Area,
        visible: bool,
    }

    fn rect(width: u32, height: u32) -> Rect {
        Rect {
            position: PhysicalPosition::new(0, 0).into(),
            size: PhysicalSize::new(width, height).into(),
        }
    }

    /// Shows or hides the page window without taking the keyboard from whoever has it.
    fn show(hwnd: HWND, visible: bool) {
        unsafe { ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE }) }.as_bool();
    }

    /// Where an area of Zed's window is on the screen.
    fn on_screen(owner: HWND, (x, y, width, height): Area) -> Area {
        let mut point = POINT { x, y };
        unsafe { ClientToScreen(owner, &mut point) }.ok().ok();
        (point.x, point.y, width, height)
    }

    impl Page {
        pub fn open(window: &Window, path: &Path, bounds: Bounds<Pixels>, scale: f32) -> Result<Self> {
            let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window).context("Zed's window")?.as_raw() else {
                return Err(anyhow!("Zed's window is not a Windows window"));
            };
            let owner = HWND(handle.hwnd.get() as *mut c_void);
            let url = url::Url::from_file_path(path).map_err(|()| anyhow!("not an absolute file path"))?;
            let screen = on_screen(owner, area(bounds, scale));

            let overlay = Overlay::new(owner, screen)?;
            let mut context = WebContext::new(Some(paths::data_dir().join("html_preview")));
            let webview = WebViewBuilder::new_with_web_context(&mut context)
                .with_url(url.as_str())
                .with_bounds(rect(screen.2, screen.3))
                .with_incognito(true)
                .with_devtools(false)
                // The page may not leave local files, open windows or download anything.
                .with_navigation_handler(|url| url.starts_with("file:") || url.starts_with("about:"))
                .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
                .with_download_started_handler(|_, _| false)
                .build_as_child(&overlay)
                .context("starting the page viewer")?;
            show(overlay.hwnd, true);
            Ok(Self {
                webview,
                _context: context,
                overlay,
                owner,
                screen,
                visible: true,
            })
        }

        pub fn place(&mut self, bounds: Bounds<Pixels>, scale: f32) {
            let screen = on_screen(self.owner, area(bounds, scale));
            if screen.2 == 0 || screen.3 == 0 {
                self.hide();
                return;
            }
            if screen != self.screen {
                let size_changed = (screen.2, screen.3) != (self.screen.2, self.screen.3);
                self.screen = screen;
                unsafe {
                    SetWindowPos(
                        self.overlay.hwnd,
                        None,
                        screen.0,
                        screen.1,
                        screen.2 as i32,
                        screen.3 as i32,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    )
                    .ok();
                }
                if size_changed {
                    self.webview.set_bounds(rect(screen.2, screen.3)).ok();
                }
            }
            if !self.visible {
                self.visible = true;
                show(self.overlay.hwnd, true);
            }
        }

        pub fn hide(&mut self) {
            if self.visible {
                self.visible = false;
                show(self.overlay.hwnd, false);
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod other {
    use super::*;
    use anyhow::{Result, anyhow};

    pub struct Page;

    impl Page {
        pub fn open(_window: &Window, _path: &Path, _bounds: Bounds<Pixels>, _scale: f32) -> Result<Self> {
            Err(anyhow!("the page viewer is only built for Windows"))
        }

        pub fn place(&mut self, _bounds: Bounds<Pixels>, _scale: f32) {}

        pub fn hide(&mut self) {}
    }
}
