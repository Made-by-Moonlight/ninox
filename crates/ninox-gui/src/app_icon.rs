//! The Ninox owl as the app's icon. A bundled `Ninox.app` gets it from
//! `Ninox.icns`; a bare `ninox-gui` binary would otherwise show macOS's
//! generic executable icon in the Dock and app switcher.

const ICON_PNG: &[u8] = include_bytes!("../assets/icon-1024.png");
/// Window icons are small; 256px keeps the decoded RGBA under 300 KB.
const WINDOW_ICON_PX: u32 = 256;

/// The window icon (title bar / taskbar on Linux and Windows).
pub fn window_icon() -> Option<iced::window::Icon> {
    let image = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?;
    let rgba = image.resize(WINDOW_ICON_PX, WINDOW_ICON_PX, image::imageops::FilterType::Lanczos3).to_rgba8();
    let (w, h) = rgba.dimensions();
    iced::window::icon::from_rgba(rgba.into_raw(), w, h).ok()
}

/// macOS ignores window icons for the Dock: set the application's icon
/// image directly. Must run on the main thread, before the event loop.
#[cfg(target_os = "macos")]
pub fn set_dock_icon() {
    use objc2::rc::Retained;
    use objc2::ClassType;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};

    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(ICON_PNG);
    let image: Option<Retained<NSImage>> = NSImage::initWithData(NSImage::alloc(), &data);
    if let Some(image) = image {
        let app = NSApplication::sharedApplication(mtm);
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_icon() {}

#[cfg(test)]
mod tests {
    #[test]
    fn the_embedded_owl_decodes_into_a_window_icon() {
        assert!(super::window_icon().is_some());
    }
}
