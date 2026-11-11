//! The tray icon: a left click opens the window; its menu opens it or quits. Its events come
//! on the main thread's message loop, which GPUI runs.

use backseat_core::{self as core, App as Core};
use std::sync::Arc;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

const ICON: &[u8] = include_bytes!("../../app/icons/icon.png");

thread_local! {
    /// The menu's items, to put them in the UI language when it changes (UI thread only).
    static ITEMS: std::cell::RefCell<Option<(MenuItem, MenuItem)>> = const { std::cell::RefCell::new(None) };
}

/// The menu in the current UI language.
pub fn relabel() {
    let (s, q) = core::tray_text();
    ITEMS.with(|i| {
        if let Some((show, quit)) = i.borrow().as_ref() {
            show.set_text(s);
            quit.set_text(q);
        }
    });
}

pub fn start(core: Arc<Core>) -> Result<(), String> {
    let (show_text, quit_text) = core::tray_text();
    let menu = Menu::new();
    let show = MenuItem::with_id("show", show_text, true, None);
    let quit = MenuItem::with_id("quit", quit_text, true, None);
    menu.append_items(&[&show, &quit]).map_err(|e| e.to_string())?;
    ITEMS.with(|i| *i.borrow_mut() = Some((show.clone(), quit.clone())));
    let picture = image::load_from_memory(ICON).map_err(|e| e.to_string())?.resize(32, 32, image::imageops::FilterType::Lanczos3).into_rgba8();
    let (w, h) = picture.dimensions();
    let icon = Icon::from_rgba(picture.into_raw(), w, h).map_err(|e| e.to_string())?;
    let tray = TrayIconBuilder::new().with_tooltip("Backseat").with_menu(Box::new(menu)).with_menu_on_left_click(false).with_icon(icon).build().map_err(|e| e.to_string())?;
    // The icon lives as long as the app.
    std::mem::forget(tray);
    TrayIconEvent::set_event_handler(Some(|e| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            crate::show_main_window();
        }
    }));
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| match e.id.0.as_str() {
        "show" => crate::show_main_window(),
        "quit" => {
            // Tell the server first, then leave.
            core::leave_for_good(&core, std::time::Duration::from_millis(500));
            std::process::exit(0);
        }
        _ => {}
    }));
    Ok(())
}
