use adw::prelude::*;
use gtk::{gio, glib};

use crate::config::AppConfig;
use crate::window::ManuscriptWindow;

pub struct ManuscriptApp {
    app: adw::Application,
}

impl ManuscriptApp {
    pub fn new() -> Self {
        let app = adw::Application::builder()
            .application_id("io.gitlab.ilshat_apps.manuscript")
            .flags(gio::ApplicationFlags::HANDLES_OPEN)
            .build();

        app.connect_startup(|_| {
            setup_icon_theme();
            register_global_css();
            std::thread::spawn(|| {
                crate::highlighter::prewarm();
            });
        });

        app.connect_activate(|app| {
            let window = ManuscriptWindow::new(app);
            window.try_restore_session();
            window.present();
        });

        app.connect_open(move |app, files, _| {
            // Re-read config on every open so the toggle takes effect without restart.
            let new_window = AppConfig::load().open_files_in_new_window;
            let paths: Vec<_> = files.iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                let window = app
                    .active_window()
                    .and_then(|w| w.downcast::<ManuscriptWindow>().ok())
                    .unwrap_or_else(|| ManuscriptWindow::new(app));
                window.present();
                return;
            }

            if new_window || app.active_window().is_none() {
                // One new window per file, so multi-file open from the file manager
                // (Open With → multiple selection) tiles side-by-side.
                for path in &paths {
                    let window = ManuscriptWindow::new(app);
                    window.open_file(path);
                    window.present();
                }
            } else {
                let window = app
                    .active_window()
                    .and_then(|w| w.downcast::<ManuscriptWindow>().ok())
                    .unwrap_or_else(|| ManuscriptWindow::new(app));
                if let Some(path) = paths.first() {
                    window.open_file(path);
                }
                window.present();
            }
        });

        // Keyboard shortcuts
        app.set_accels_for_action("win.new", &["<Control>n"]);
        app.set_accels_for_action("win.open", &["<Control>o"]);
        app.set_accels_for_action("win.save", &["<Control>s"]);
        app.set_accels_for_action("win.search", &["<Control>f"]);
        app.set_accels_for_action("win.find-replace", &["<Control>h"]);
        app.set_accels_for_action("win.print", &["<Control>p"]);
        app.set_accels_for_action("win.toggle-edit", &["<Control>e"]);
        app.set_accels_for_action("win.toggle-split", &["<Control><Shift>e"]);
        app.set_accels_for_action("win.preferences", &["<Control>comma"]);
        // F10 for win.toggle-vault-sidebar is handled in window.rs
        // setup_key_capture instead — the action-accelerator path is
        // unreliable while a sourceview5::View has focus (same reason
        // F9/F11 are capture-handled). Binding it here too would fire
        // both paths and toggle twice.
        app.set_accels_for_action("window.close", &["<Control>q"]);

        Self { app }
    }

    pub fn run(&self) -> glib::ExitCode {
        self.app.run()
    }
}

/// Register process-global CSS for static rules (image rounding, etc.).
/// Per-view font CSS continues to flow through each window's own providers.
fn register_global_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".manuscript-image { \
            border-radius: 8px; \
            background: transparent; \
            border: none; \
        } \
        .manuscript-table-wrap { \
            border: 1px solid alpha(@view_fg_color, 0.2); \
            border-radius: 6px; \
            margin: 8px 0; \
        } \
        .manuscript-table-cell { \
            background: transparent; \
            border-right: 1px solid alpha(@view_fg_color, 0.12); \
            border-bottom: 1px solid alpha(@view_fg_color, 0.12); \
            padding: 0; \
        } \
        .manuscript-table-cell.last-col { \
            border-right: none; \
        } \
        .manuscript-table-cell.last-row { \
            border-bottom: none; \
        } \
        .manuscript-table-header { \
            background: alpha(@accent_bg_color, 0.08); \
            font-weight: bold; \
        } \
        .manuscript-table-row-even { \
            background: alpha(@view_fg_color, 0.03); \
        } \
        .manuscript-footer { \
            padding: 2px 12px; \
            border-top: 1px solid alpha(@view_fg_color, 0.1); \
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn setup_icon_theme() {
    gtk::Window::set_default_icon_name("io.gitlab.ilshat_apps.manuscript");

    let display = gtk::gdk::Display::default().unwrap();
    let icon_theme = gtk::IconTheme::for_display(&display);

    // Development: icons in project data/ directory
    let cwd_icons = std::path::PathBuf::from("data/icons");
    if cwd_icons.exists() {
        icon_theme.add_search_path(cwd_icons);
    }

    // Installed to ~/.local
    if let Some(data_dir) = dirs::data_dir() {
        let local_icons = data_dir.join("icons");
        if local_icons.exists() {
            icon_theme.add_search_path(local_icons);
        }
    }
}
