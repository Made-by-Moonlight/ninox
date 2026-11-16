//! The window draws under a transparent title bar (`main.rs`'s
//! `window_settings`), so on macOS the traffic lights sit on top of the nav
//! bar. [`place_traffic_lights`] centres them on the bar and reports where
//! they end, so the brand clears them by measurement rather than a guess.

use iced::window::raw_window_handle::WindowHandle;

/// Moves the close/minimise/zoom buttons so their centre is `bar_height / 2`
/// from the window's top, and returns how much of the bar's left edge they
/// take, in window points: where the zoom button ends, or 0 in native full
/// screen, where AppKit moves them into its own reveal toolbar and they're
/// left alone. AppKit re-tiles them whenever it lays the title bar out again
/// (resize, regaining focus, leaving full screen), so callers re-run this
/// then. `None` off macOS, or when AppKit doesn't hand back the views.
#[cfg(target_os = "macos")]
pub fn place_traffic_lights(handle: WindowHandle<'_>, bar_height: f64) -> Option<f32> {
    use iced::window::raw_window_handle::RawWindowHandle;
    use objc2_app_kit::{NSView, NSWindowButton, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect};

    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return None };
    // SAFETY: winit's AppKit handle is a live `NSView` for the duration of
    // this callback, which iced runs on the main thread.
    let view: &NSView = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    let window = view.window()?;
    if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return Some(0.0);
    }
    let buttons = [NSWindowButton::NSWindowCloseButton, NSWindowButton::NSWindowMiniaturizeButton, NSWindowButton::NSWindowZoomButton]
        .map(|kind| window.standardWindowButton(kind));
    let [Some(close), Some(mini), Some(zoom)] = buttons else { return None };

    // The buttons live in NSTitlebarView inside NSTitlebarContainerView.
    // Both are private, so both get explicit frames: the container pinned
    // to the top of its own superview at the bar's height, the title-bar
    // view filling it, and the buttons centred in that.
    // SAFETY: plain AppKit view-geometry calls on main-thread views.
    unsafe {
        let titlebar = close.superview()?;
        let container = titlebar.superview()?;
        let host_height = container.superview()?.frame().size.height;
        let mut frame: NSRect = container.frame();
        frame.size.height = bar_height;
        frame.origin.y = host_height - bar_height;
        container.setFrame(frame);
        titlebar.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        for button in [&close, &mini, &zoom] {
            let rect = button.frame();
            button.setFrameOrigin(NSPoint::new(rect.origin.x, (bar_height - rect.size.height) / 2.0));
        }
    }
    let zoom = zoom.frame();
    Some((zoom.origin.x + zoom.size.width) as f32)
}

#[cfg(not(target_os = "macos"))]
pub fn place_traffic_lights(_handle: WindowHandle<'_>, _bar_height: f64) -> Option<f32> {
    None
}
