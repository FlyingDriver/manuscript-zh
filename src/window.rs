use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use libspelling as spelling;
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::AppConfig;
use crate::history::{History, Location, Pos};
use crate::preferences;
use crate::renderer;
use crate::search_order::{merge_matches, HitRef, TableMatches};

/// The ☰ menu. Single builder so the initial build and `rebuild_recent_menu`
/// can never drift apart.
fn build_main_menu(recent_files: &[String]) -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("新建文件"), Some("win.new"));
    if !recent_files.is_empty() {
        let recent_submenu = gio::Menu::new();
        for path_str in recent_files {
            let path = std::path::Path::new(path_str);
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path_str.clone());
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(
                Some("win.open-recent"),
                Some(&path_str.to_variant()),
            );
            recent_submenu.append_item(&item);
        }
        menu.append_submenu(Some("最近文件"), &recent_submenu);
    }
    menu.append(Some("快速打开…"), Some("win.quick-open"));
    // View-mode toggles — also live in the header on wide windows, but the
    // toggle group is hidden on narrow screens, so keep them reachable here.
    menu.append(Some("编辑模式"), Some("win.toggle-edit"));
    menu.append(Some("分栏视图"), Some("win.toggle-split"));
    menu.append(Some("打印"), Some("win.print"));
    menu.append(Some("导出为 PDF…"), Some("win.export-pdf"));
    menu.append(Some("首选项"), Some("win.preferences"));
    menu.append(Some("写作统计"), Some("win.writing-stats"));
    menu.append(Some("键盘快捷键"), Some("win.shortcuts"));
    menu.append(Some("关于 Manuscript"), Some("win.about"));
    menu
}

/// Content-width clamp value meaning "unlimited" (content_width == 0). Wide
/// beyond any real display so the view fills the window, but NOT near
/// `i32::MAX` — AdwClamp reports its natural width as `maximum_size`, and
/// `i32::MAX + padding` overflows to a huge negative width, producing
/// `gtk_widget_size_allocate` warnings and broken layout on narrow windows.
const UNLIMITED_CONTENT_WIDTH: i32 = 100_000;

// -- View mode enum --

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewMode {
    View,
    Edit,
    Split,
}

// -- Subclass implementation --

/// One entry of the merged search hit list (main buffer or a table cell).
#[derive(Clone)]
pub enum SearchHit {
    Main(gtk::TextMark),
    Cell {
        view: glib::WeakRef<gtk::TextView>,
        mark: gtk::TextMark,
    },
}

mod imp {
    use super::*;

    pub struct ManuscriptWindow {
        pub current_file: RefCell<Option<PathBuf>>,
        pub file_content: RefCell<String>,
        pub modified: Cell<bool>,
        pub(super) view_mode: Cell<ViewMode>,
        pub config: RefCell<AppConfig>,

        // Widgets
        pub content_stack: RefCell<Option<gtk::Stack>>,
        pub text_view: RefCell<Option<gtk::TextView>>,
        pub edit_view: RefCell<Option<sourceview5::View>>,
        pub split_edit_view: RefCell<Option<sourceview5::View>>,
        pub split_render_view: RefCell<Option<gtk::TextView>>,
        pub split_render_scroll: RefCell<Option<gtk::ScrolledWindow>>,
        pub edit_scroll: RefCell<Option<gtk::ScrolledWindow>>,
        pub split_edit_scroll: RefCell<Option<gtk::ScrolledWindow>>,
        pub header_bar: RefCell<Option<adw::HeaderBar>>,
        pub mobile_action_bar: RefCell<Option<gtk::ActionBar>>,
        /// Holds the parent ToolbarView so we can add/remove the mobile
        /// action bar dynamically. Keeping it permanently attached caused
        /// GtkToolbarView to measure the empty Revealer at unrealistic
        /// widths during layout (the source of the GtkRevealer / Scrolled
        /// Window "needs at least N" warnings).
        pub toolbar_view: RefCell<Option<adw::ToolbarView>>,
        pub mobile_bar_attached: Cell<bool>,
        pub narrow_active: Cell<bool>,
        /// True while focus-dim is currently applied to at least one buffer.
        /// Used to skip the per-keystroke buffer-wide tag scan when the toggle
        /// is off and no stale tags remain.
        pub focus_dim_active: Cell<bool>,
        /// Pending typewriter scroll idle source id (per source view kind:
        /// edit / split_edit). We coalesce so that fast typing does not
        /// accumulate a queue of idle callbacks.
        pub typewriter_idle: RefCell<Option<glib::SourceId>>,
        /// Last hash (content + theme) the View pane was rendered for. Used
        /// by refresh_view to skip re-rendering when nothing actually changed.
        pub last_view_render_hash: Cell<u64>,
        pub last_split_render_hash: Cell<u64>,
        /// Coalesces sync_typewriter_margins calls triggered by a burst of
        /// page-size-notify events on a single resize gesture.
        pub margins_idle: RefCell<Option<glib::SourceId>>,
        /// Last paragraph bounds applied by focus mode: (ps_line, pe_line,
        /// total_lines). On the next cursor notify we can skip the full
        /// remove+apply pass when the cursor is still inside [ps_line, pe_line]
        /// and the buffer's line count is unchanged.
        pub focus_para_cache: Cell<(i32, i32, i32)>,
        /// Last seen tag_table size when we set the focus-dim priority. We
        /// only re-promote when sourceview has added new syntax tags since
        /// the last apply.
        pub focus_tag_table_size: Cell<i32>,
        /// Last CSS string loaded per CssProvider, keyed by provider role.
        /// `apply_font_css` skips `load_from_data` when the new CSS is
        /// byte-identical to the previous one. CSS reloads were repeatedly
        /// invalidating the GTK style cache, surfacing as `gsk_reload_font`
        /// in the flamegraph.
        pub last_view_css: RefCell<String>,
        pub last_split_css: RefCell<String>,
        pub last_edit_css: RefCell<String>,
        pub last_split_edit_css: RefCell<String>,
        pub split_paned: RefCell<Option<gtk::Paned>>,
        /// Source-line → rendered-offset map from the last split render.
        pub split_line_map: RefCell<Vec<renderer::LineAnchor>>,
        pub view_clamp: RefCell<Option<adw::ClampScrollable>>,
        pub view_scroll: RefCell<Option<gtk::ScrolledWindow>>,
        pub title_label: RefCell<Option<adw::WindowTitle>>,
        pub view_toggle: RefCell<Option<gtk::ToggleButton>>,
        pub edit_toggle: RefCell<Option<gtk::ToggleButton>>,
        pub split_toggle: RefCell<Option<gtk::ToggleButton>>,
        pub save_btn: RefCell<Option<gtk::Button>>,
        /// Back button in the content header; visible only while the back
        /// stack is non-empty.
        pub back_btn: RefCell<Option<gtk::Button>>,
        pub forward_btn: RefCell<Option<gtk::Button>>,
        /// Browser-style back/forward stacks (in-memory only).
        pub nav_history: RefCell<History>,
        /// Last known scroll position per file, remembered across switches.
        pub scroll_positions: RefCell<HashMap<PathBuf, Pos>>,
        /// True while a back/forward move is in flight, so it isn't recorded.
        pub nav_guard: Cell<bool>,
        /// True while `set_editor_text` replaces a buffer programmatically.
        pub programmatic_edit: Cell<bool>,
        /// Settling re-apply of a restored scroll position: the adjustment,
        /// its `upper` handler and the timer that ends the window.
        pub scroll_restore: RefCell<Option<super::ScrollRestore>>,
        pub stats_label: RefCell<Option<gtk::Label>>,
        pub stats_debounce: RefCell<Option<glib::SourceId>>,
        /// Footer ActionBar parked into AdwToolbarView's bottom slot when
        /// `config.show_footer` is on. Attached/detached the same way the
        /// mobile action bar is — keeping it permanently parented while
        /// invisible reproduces the GtkRevealer measurement warnings.
        pub footer_bar: RefCell<Option<gtk::ActionBar>>,
        /// Three separate labels — words / read time / today's goal — each
        /// paired with a symbolic icon. The goal segment is hidden when
        /// writing-stats is off.
        pub footer_words_label: RefCell<Option<gtk::Label>>,
        pub footer_time_label: RefCell<Option<gtk::Label>>,
        pub footer_goal_label: RefCell<Option<gtk::Label>>,
        pub footer_goal_box: RefCell<Option<gtk::Box>>,
        pub footer_goal_icon: RefCell<Option<gtk::Image>>,
        pub toast_overlay: adw::ToastOverlay,
        pub split_render_debounce: RefCell<Option<glib::SourceId>>,
        pub view_css_provider: gtk::CssProvider,
        pub split_css_provider: gtk::CssProvider,
        pub edit_css_provider: gtk::CssProvider,
        pub split_edit_css_provider: gtk::CssProvider,
        /// Session-only zoom factor (Ctrl+=, Ctrl+-, Ctrl+0). Not persisted.
        pub font_zoom: Cell<f64>,

        // Outline
        pub outline_split: RefCell<Option<adw::OverlaySplitView>>,
        pub outline_listbox: RefCell<Option<gtk::ListBox>>,
        /// Outline `show-sidebar` before the first adaptive breakpoint hid it;
        /// `None` while no breakpoint is active.
        pub outline_shown_before_compact: Cell<Option<bool>>,

        // Vault sidebar (file tree)
        pub vault_split: RefCell<Option<adw::OverlaySplitView>>,
        pub vault_list: RefCell<Option<gtk::ListView>>,
        pub vault_header: RefCell<Option<adw::HeaderBar>>,
        /// Set while the sidebar visibility is changed programmatically (split
        /// mode hide/restore) so the persist handler doesn't clobber the user's
        /// `vault_sidebar_shown` preference with a transient value.
        pub sidebar_persist_suppressed: std::cell::Cell<bool>,
        /// Menu + open-folder buttons move into the vault sidebar header while
        /// it is shown (leading edge) and back to the content header when hidden.
        pub open_button: RefCell<Option<gtk::Button>>,
        /// Root currently backing the vault tree model, so `open_file` can
        /// skip rebuilding (and losing expansion state) when the file's
        /// parent folder hasn't actually changed.
        pub vault_current_root: RefCell<Option<std::path::PathBuf>>,


        // Spell check adapters (Edit / Split source views)
        pub edit_spell_adapter: RefCell<Option<spelling::TextBufferAdapter>>,
        pub split_edit_spell_adapter: RefCell<Option<spelling::TextBufferAdapter>>,

        // Search
        pub search_bar: RefCell<Option<gtk::SearchBar>>,
        pub search_entry: RefCell<Option<gtk::SearchEntry>>,
        pub search_count_label: RefCell<Option<gtk::Label>>,
        pub search_replace_entry: RefCell<Option<gtk::Entry>>,
        pub search_replace_row: RefCell<Option<gtk::Box>>,
        pub search_match_index: Cell<i32>,
        pub search_match_count: Cell<i32>,
        pub search_match_marks: RefCell<Vec<gtk::TextMark>>,
        /// Merged document-order hit list (main buffer + rich-table cells).
        pub search_hits: RefCell<Vec<SearchHit>>,
        pub search_query: RefCell<String>,

        // File monitor
        pub file_monitor: RefCell<Option<gio::FileMonitor>>,
        /// Auto-save debounce SourceId; cancelled and replaced on each edit.
        pub auto_save_idle: RefCell<Option<glib::SourceId>>,
        /// Pending debounce for coalesced disk-change handling.
        pub disk_change_idle: RefCell<Option<glib::SourceId>>,
        /// 2 s mtime/size poller, only for document-portal paths.
        pub file_poll: RefCell<Option<glib::SourceId>>,
        /// Last seen (mtime, size) of the open file, for the poller.
        pub file_poll_sig: Cell<Option<(std::time::SystemTime, u64)>>,
        /// Set while we are writing the file ourselves so the file monitor
        /// doesn't surface a Reload toast for our own write.
        pub self_writing: Cell<bool>,

        // Menu button (needed to update recent files submenu)
        pub menu_button: RefCell<Option<gtk::MenuButton>>,

        // A.3: today-words tracking
        /// Last counted document word count, used for positive-delta diffing
        /// on save / 60s tick. Seeded on file open so the entire document
        /// isn't mistakenly attributed to "today's writing".
        pub last_doc_words: Cell<u32>,
        /// SourceId for the 60-s `track_today_words` tick. Cancelled on
        /// window close so the closure does not outlive the window.
        pub today_tick_source: RefCell<Option<glib::SourceId>>,
    }

    impl Default for ManuscriptWindow {
        fn default() -> Self {
            Self {
                current_file: RefCell::new(None),
                file_content: RefCell::new(String::new()),
                modified: Cell::new(false),
                view_mode: Cell::new(ViewMode::View),
                config: RefCell::new(AppConfig::load()),
                content_stack: RefCell::new(None),
                text_view: RefCell::new(None),
                edit_view: RefCell::new(None),
                split_edit_view: RefCell::new(None),
                split_render_view: RefCell::new(None),
                split_render_scroll: RefCell::new(None),
                edit_scroll: RefCell::new(None),
                split_edit_scroll: RefCell::new(None),
                header_bar: RefCell::new(None),
                mobile_action_bar: RefCell::new(None),
                toolbar_view: RefCell::new(None),
                mobile_bar_attached: Cell::new(false),
                narrow_active: Cell::new(false),
                focus_dim_active: Cell::new(false),
                typewriter_idle: RefCell::new(None),
                last_view_render_hash: Cell::new(0),
                last_split_render_hash: Cell::new(0),
                margins_idle: RefCell::new(None),
                focus_para_cache: Cell::new((-1, -1, -1)),
                focus_tag_table_size: Cell::new(0),
                last_view_css: RefCell::new(String::new()),
                last_split_css: RefCell::new(String::new()),
                last_edit_css: RefCell::new(String::new()),
                last_split_edit_css: RefCell::new(String::new()),
                split_paned: RefCell::new(None),
                split_line_map: RefCell::new(Vec::new()),
                view_clamp: RefCell::new(None),
                view_scroll: RefCell::new(None),
                title_label: RefCell::new(None),
                view_toggle: RefCell::new(None),
                edit_toggle: RefCell::new(None),
                split_toggle: RefCell::new(None),
                save_btn: RefCell::new(None),
                back_btn: RefCell::new(None),
                forward_btn: RefCell::new(None),
                nav_history: RefCell::new(History::default()),
                scroll_positions: RefCell::new(HashMap::new()),
                nav_guard: Cell::new(false),
                programmatic_edit: Cell::new(false),
                scroll_restore: RefCell::new(None),
                stats_label: RefCell::new(None),
                stats_debounce: RefCell::new(None),
                footer_bar: RefCell::new(None),
                footer_words_label: RefCell::new(None),
                footer_time_label: RefCell::new(None),
                footer_goal_label: RefCell::new(None),
                footer_goal_box: RefCell::new(None),
                footer_goal_icon: RefCell::new(None),
                toast_overlay: adw::ToastOverlay::new(),
                split_render_debounce: RefCell::new(None),
                view_css_provider: gtk::CssProvider::new(),
                split_css_provider: gtk::CssProvider::new(),
                edit_css_provider: gtk::CssProvider::new(),
                split_edit_css_provider: gtk::CssProvider::new(),
                font_zoom: Cell::new(1.0),
                outline_split: RefCell::new(None),
                outline_listbox: RefCell::new(None),
                outline_shown_before_compact: Cell::new(None),
                vault_split: RefCell::new(None),
                vault_list: RefCell::new(None),
                vault_header: RefCell::new(None),
                sidebar_persist_suppressed: std::cell::Cell::new(false),
                vault_current_root: RefCell::new(None),
                open_button: RefCell::new(None),
                edit_spell_adapter: RefCell::new(None),
                split_edit_spell_adapter: RefCell::new(None),
                search_bar: RefCell::new(None),
                search_entry: RefCell::new(None),
                search_count_label: RefCell::new(None),
                search_replace_entry: RefCell::new(None),
                search_replace_row: RefCell::new(None),
                search_match_index: Cell::new(-1),
                search_match_count: Cell::new(0),
                search_match_marks: RefCell::new(Vec::new()),
                search_hits: RefCell::new(Vec::new()),
                search_query: RefCell::new(String::new()),
                file_monitor: RefCell::new(None),
                auto_save_idle: RefCell::new(None),
                disk_change_idle: RefCell::new(None),
                file_poll: RefCell::new(None),
                file_poll_sig: Cell::new(None),
                self_writing: Cell::new(false),
                menu_button: RefCell::new(None),
                last_doc_words: Cell::new(0),
                today_tick_source: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ManuscriptWindow {
        const NAME: &'static str = "ManuscriptWindow";
        type Type = super::ManuscriptWindow;
        type ParentType = adw::ApplicationWindow;
    }

    impl ObjectImpl for ManuscriptWindow {}

    impl ManuscriptWindow {
        /// Config-driven renderer switches. Not part of the render hash —
        /// `apply_config` zeroes both hashes so a toggle always re-renders.
        pub fn render_options(&self) -> crate::renderer::RenderOptions {
            let cfg = self.config.borrow();
            crate::renderer::RenderOptions {
                rich_tables: cfg.rich_tables,
                keep_line_breaks: cfg.keep_line_breaks,
                max_image_width: cfg.max_image_width as i32,
            }
        }
    }
    impl WidgetImpl for ManuscriptWindow {}
    impl WindowImpl for ManuscriptWindow {}
    impl ApplicationWindowImpl for ManuscriptWindow {}
    impl AdwApplicationWindowImpl for ManuscriptWindow {}
}

glib::wrapper! {
    pub struct ManuscriptWindow(ObjectSubclass<imp::ManuscriptWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl ManuscriptWindow {
    pub fn new(app: &adw::Application) -> Self {
        let window: Self = glib::Object::builder()
            .property("application", app)
            .property("default-width", 900)
            .property("default-height", 700)
            .property("width-request", 360)
            .property("height-request", 294)
            .build();

        window.setup_ui();
        window.setup_actions();
        window.setup_drop_target();
        window.setup_theme_tracking();
        window.setup_key_capture();
        window.setup_fullscreen_tracking();
        window.setup_writer_modes();
        window.setup_split_scroll_sync();
        window.connect_close_request(glib::clone!(
            #[weak(rename_to = win)]
            window,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                win.save_session();
                // A.3: cancel the 60-s today-words tick so its closure
                // doesn't outlive the window.
                if let Some(id) = win.imp().today_tick_source.borrow_mut().take() {
                    id.remove();
                }
                glib::Propagation::Proceed
            }
        ));
        window
    }

    fn imp(&self) -> &imp::ManuscriptWindow {
        imp::ManuscriptWindow::from_obj(self)
    }

    /// Current local date as `YYYY-MM-DD`. Empty string on failure — callers
    /// treat that as "no date" and skip the roll-over.
    fn current_iso_date() -> String {
        gtk::glib::DateTime::now_local()
            .ok()
            .and_then(|dt| dt.format("%F").ok())
            .map(|gs| gs.to_string())
            .unwrap_or_default()
    }

    /// If the persisted `last_active_date` is older than today, zero
    /// `today_words` and bump the date. Safe to call from any path that
    /// touches today-words (apply_config, save, tick).
    fn roll_over_today_words_if_needed(&self) {
        let today = Self::current_iso_date();
        if today.is_empty() {
            return;
        }
        let mut cfg = self.imp().config.borrow_mut();
        if cfg.last_active_date != today {
            cfg.today_words = 0;
            cfg.last_active_date = today;
            let snapshot = cfg.clone();
            drop(cfg);
            snapshot.save();
        }
    }

    /// Compute current document word count and accumulate the positive delta
    /// since the last call into `config.today_words`. Negative deltas
    /// (deletions) are intentionally not subtracted — see spec.
    fn track_today_words(&self) {
        let imp = self.imp();
        let cfg_borrow = imp.config.borrow();
        if !cfg_borrow.writing_stats {
            return;
        }
        drop(cfg_borrow);

        self.roll_over_today_words_if_needed();

        // Mirror update_stats's word-count source.
        let text = match imp.view_mode.get() {
            ViewMode::Edit => imp
                .edit_view
                .borrow()
                .as_ref()
                .map(|sv| {
                    let buf = sv.buffer();
                    buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string()
                })
                .unwrap_or_default(),
            ViewMode::Split | ViewMode::View => imp.file_content.borrow().clone(),
        };
        let words = text.split_whitespace().count() as u32;

        let prev = imp.last_doc_words.replace(words);
        let delta = words.saturating_sub(prev);
        if delta == 0 {
            return;
        }

        let mut cfg = imp.config.borrow_mut();
        cfg.today_words = cfg.today_words.saturating_add(delta).min(u32::MAX / 2);
        let snapshot = cfg.clone();
        drop(cfg);
        snapshot.save();

        self.update_stats();
    }

    fn setup_ui(&self) {
        let imp = self.imp();
        let config = imp.config.borrow().clone();

        // -- Header bar --
        let title_widget = adw::WindowTitle::new("Manuscript", "");
        *imp.title_label.borrow_mut() = Some(title_widget.clone());

        let header = adw::HeaderBar::builder()
            .title_widget(&title_widget)
            .build();
        *imp.header_bar.borrow_mut() = Some(header.clone());

        // Outline toggle. Its `active` state is bound bidirectionally to the
        // outline sidebar's `show-sidebar` once the split exists (below), so a
        // breakpoint auto-hiding the sidebar can't leave the button out of sync
        // (which used to need a double-click to re-hide).
        let outline_btn = gtk::ToggleButton::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text("切换大纲 (F9)")
            .build();
        // Outline button on the right side of the header — matches the
        // outline panel's actual position on the right edge of the window.
        header.pack_end(&outline_btn);

        // Menu button with recent files submenu — packed first so it sits
        // on the far left of the header, left of Open.
        let menu = build_main_menu(&config.recent_files);

        // Vault sidebar toggle — leftmost widget in the content-pane header,
        // GNOME-style: shows/hides the vault sidebar (which has its own header).
        // Initial keyboard focus is handed to the document view at the end of
        // setup (see the idle grab_focus below), so no header button steals it.
        let vault_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("切换笔记库侧栏 (F10)")
            .active(crate::vault::should_show_sidebar(
                config.vault_root.as_deref(),
                config.vault_sidebar_shown,
            ))
            .build();
        header.pack_start(&vault_toggle);

        // Back button — packed right after the sidebar toggle and never
        // touched by `place_nav_buttons`, so its slot is stable. Hidden until
        // there is somewhere to go back to; stays visible on narrow screens.
        let back_btn = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("后退 (Alt+←)")
            .action_name("win.go-back")
            .visible(false)
            .build();
        header.pack_start(&back_btn);
        *imp.back_btn.borrow_mut() = Some(back_btn);
        let forward_btn = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("前进 (Alt+→)")
            .action_name("win.go-forward")
            .visible(false)
            .build();
        header.pack_start(&forward_btn);
        *imp.forward_btn.borrow_mut() = Some(forward_btn);

        let menu_btn = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("菜单")
            .build();
        header.pack_start(&menu_btn);
        *imp.menu_button.borrow_mut() = Some(menu_btn.clone());

        // Open button — opens a vault folder. Opening a single file is on
        // Ctrl+O (win.open) and quick-open (Ctrl+Shift+O).
        let open_btn = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("打开文件夹")
            .build();
        header.pack_start(&open_btn);
        *imp.open_button.borrow_mut() = Some(open_btn.clone());

        // Save button (hidden by default, shown in edit mode)
        let save_btn = gtk::Button::builder()
            .icon_name("document-save-symbolic")
            .tooltip_text("保存 (Ctrl+S)")
            .sensitive(false)
            .visible(false)
            .build();
        save_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.save_file();
            }
        ));
        header.pack_start(&save_btn);
        *imp.save_btn.borrow_mut() = Some(save_btn);

        // View mode toggle buttons
        let view_btn = gtk::ToggleButton::builder()
            .icon_name("view-reveal-symbolic")
            .tooltip_text("阅读模式")
            .active(true)
            .build();
        let edit_btn = gtk::ToggleButton::builder()
            .icon_name("text-editor-symbolic")
            .tooltip_text("编辑模式 (Ctrl+E)")
            .group(&view_btn)
            .build();
        let split_btn = gtk::ToggleButton::builder()
            .icon_name("view-dual-symbolic")
            .tooltip_text("分栏视图 (Ctrl+Shift+E)")
            .group(&view_btn)
            .build();

        let toggle_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        toggle_box.add_css_class("linked");
        // New file leads the document group. Hidden with the group on narrow
        // windows; still on the ☰ menu and Ctrl+N there.
        let new_btn = gtk::Button::builder()
            .icon_name("document-new-symbolic")
            .tooltip_text("新建文件 (Ctrl+N)")
            .action_name("win.new")
            .build();
        toggle_box.append(&new_btn);
        toggle_box.append(&view_btn);
        toggle_box.append(&edit_btn);
        toggle_box.append(&split_btn);
        header.pack_end(&toggle_box);

        // Word count + reading time (sits left of the toggle buttons; pack_end stacks right→left)
        let stats_label = gtk::Label::builder()
            .label("")
            .visible(false)
            .build();
        stats_label.add_css_class("dim-label");
        stats_label.add_css_class("caption");
        stats_label.set_margin_end(8);
        header.pack_end(&stats_label);
        *imp.stats_label.borrow_mut() = Some(stats_label.clone());

        *imp.view_toggle.borrow_mut() = Some(view_btn.clone());
        *imp.edit_toggle.borrow_mut() = Some(edit_btn.clone());
        *imp.split_toggle.borrow_mut() = Some(split_btn.clone());

        // -- Search bar --
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("搜索…")
            .hexpand(true)
            .build();

        let prev_btn = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("上一个匹配 (Shift+Enter)")
            .build();
        prev_btn.add_css_class("flat");
        let next_btn = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text("下一个匹配 (Enter)")
            .build();
        next_btn.add_css_class("flat");

        let count_label = gtk::Label::builder()
            .label("")
            .width_chars(7)
            .max_width_chars(10)
            .xalign(1.0)
            .build();
        count_label.add_css_class("dim-label");
        count_label.add_css_class("caption");

        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        search_row.append(&search_entry);
        search_row.append(&count_label);
        search_row.append(&prev_btn);
        search_row.append(&next_btn);

        *imp.search_count_label.borrow_mut() = Some(count_label);

        let replace_entry = gtk::Entry::builder()
            .placeholder_text("替换为…")
            .hexpand(true)
            .build();
        let replace_btn = gtk::Button::builder()
            .label("替换")
            .tooltip_text("替换当前匹配")
            .build();
        let replace_all_btn = gtk::Button::builder()
            .label("全部替换")
            .tooltip_text("替换所有匹配")
            .build();

        let replace_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        replace_row.append(&replace_entry);
        replace_row.append(&replace_btn);
        replace_row.append(&replace_all_btn);

        let search_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        search_box.append(&search_row);
        search_box.append(&replace_row);
        // GtkSearchBar reveals vertically, so its child's *width* is measured
        // at every window size even while the bar is hidden. The find/replace
        // entries + buttons want ~640 px, which forced the whole content to
        // that minimum and spammed "AdwToastOverlay exceeds window width" on
        // narrow windows. Let it shrink to 0 (entries reflow) — same fix as the
        // outline / vault list widgets.
        search_box.set_size_request(0, -1);

        let search_bar = gtk::SearchBar::builder()
            .child(&search_box)
            .show_close_button(true)
            .build();
        search_bar.connect_entry(&search_entry);

        prev_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.search_prev();
            }
        ));
        next_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.search_next();
            }
        ));

        *imp.search_bar.borrow_mut() = Some(search_bar.clone());
        *imp.search_entry.borrow_mut() = Some(search_entry.clone());
        *imp.search_replace_entry.borrow_mut() = Some(replace_entry.clone());
        *imp.search_replace_row.borrow_mut() = Some(replace_row.clone());

        // Clear match highlighting when the search bar closes.
        search_bar.connect_search_mode_enabled_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |bar| {
                if !bar.is_search_mode() {
                    win.clear_search_highlight();
                }
            }
        ));

        replace_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.replace_current();
            }
        ));
        replace_all_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.replace_all();
            }
        ));
        replace_entry.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.replace_current();
            }
        ));

        // Search entry signals
        search_entry.connect_search_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.perform_search();
            }
        ));

        search_entry.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.search_next();
            }
        ));

        search_entry.connect_next_match(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.search_next();
            }
        ));

        search_entry.connect_previous_match(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.search_prev();
            }
        ));

        // Shift+Enter for previous match via key controller
        let key_controller = gtk::EventControllerKey::new();
        key_controller.connect_key_pressed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _, state| {
                if keyval == gtk::gdk::Key::Return
                    && state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                {
                    win.search_prev();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        ));
        search_entry.add_controller(key_controller);

        // -- Content area --

        // View mode: rendered markdown
        let text_view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(config.gtk_wrap_mode())
            .left_margin(if config.content_width == 0 { 16 } else { 48 })
            .right_margin(if config.content_width == 0 { 16 } else { 48 })
            .top_margin(32)
            .bottom_margin(32)
            .pixels_above_lines(config.line_spacing as i32 / 2)
            .pixels_below_lines(config.line_spacing as i32 / 2)
            .build();
        text_view.add_css_class("manuscript-view");
        text_view.style_context().add_provider(&imp.view_css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        apply_font_css_cached(&imp.view_css_provider, &imp.last_view_css, "manuscript-view", &config, imp.font_zoom.get());

        // 0 in preferences means "unlimited" — use a large finite value so the
        // Clamp doesn't shrink to nothing on startup (must match apply_config).
        let view_clamp_max = if config.content_width == 0 {
            UNLIMITED_CONTENT_WIDTH
        } else {
            config.content_width as i32
        };
        // ClampScrollable, not Clamp: it forwards the scroll adjustments, so
        // the TextView stays scrollable and lays out / snapshots only the
        // visible lines. Under a plain Clamp (wrapped in a GtkViewport) the
        // TextView was allocated the whole document height and every frame
        // shaped and drew all of it — a 1 MB file took 10 s and 600 MB+ to
        // settle. No viewport also means no `scroll-to-focus` jump to the top
        // on the first click.
        let view_clamp = adw::ClampScrollable::builder()
            .maximum_size(view_clamp_max)
            .child(&text_view)
            .build();

        let view_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&view_clamp)
            .build();

        // Keyboard scrolling in View mode. The TextView is non-editable with
        // an invisible cursor, so the built-in arrow-key navigation has no
        // caret to move and gets silently consumed. Drive the scroll
        // vadjustment directly so Up/Down/PgUp/PgDn/Home/End/Space behave
        // like a typical document viewer.
        attach_view_scroll_keys(&text_view, &view_scroll);

        // Edit mode: GtkSourceView
        let edit_view = create_source_view(&config);
        edit_view.style_context().add_provider(&imp.edit_css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        apply_font_css_cached(&imp.edit_css_provider, &imp.last_edit_css, "manuscript-edit", &config, imp.font_zoom.get());
        attach_smart_list_enter(&edit_view);

        let edit_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&edit_view)
            .build();

        // Split mode
        let split_edit_view = create_source_view(&config);
        split_edit_view.style_context().add_provider(&imp.split_edit_css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        apply_font_css_cached(&imp.split_edit_css_provider, &imp.last_split_edit_css, "manuscript-edit", &config, imp.font_zoom.get());
        attach_smart_list_enter(&split_edit_view);
        let split_edit_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&split_edit_view)
            .build();

        let split_render_view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(config.gtk_wrap_mode())
            .left_margin(if config.content_width == 0 { 12 } else { 24 })
            .right_margin(if config.content_width == 0 { 12 } else { 24 })
            .top_margin(16)
            .bottom_margin(16)
            .pixels_above_lines(config.line_spacing as i32 / 2)
            .pixels_below_lines(config.line_spacing as i32 / 2)
            .build();
        split_render_view.add_css_class("manuscript-view");
        split_render_view.style_context().add_provider(&imp.split_css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        apply_font_css_cached(&imp.split_css_provider, &imp.last_split_css, "manuscript-view", &config, imp.font_zoom.get());

        // Automatic, not Never: with Never the scrolled window demands the
        // TextView's full minimum width, and a rich table's pinned width then
        // stops the Split divider from narrowing the preview. A table wider
        // than its columns' minimum scrolls horizontally instead.
        let split_render_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&split_render_view)
            .build();
        attach_view_scroll_keys(&split_render_view, &split_render_scroll);

        let split_paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&split_edit_scroll)
            .end_child(&split_render_scroll)
            .resize_start_child(true)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .build();

        // Stack for switching modes
        let content_stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(150)
            // Size to the VISIBLE child, not the widest of all three. Otherwise
            // the split view (two side-by-side panes, ~640 px min) forces the
            // whole window to that minimum even in single-pane view/edit mode —
            // the root of the persistent "AdwToastOverlay exceeds window width".
            .hhomogeneous(false)
            .build();
        content_stack.add_named(&view_scroll, Some("view"));
        content_stack.add_named(&edit_scroll, Some("edit"));
        content_stack.add_named(&split_paned, Some("split"));

        // Store widget refs
        *imp.content_stack.borrow_mut() = Some(content_stack.clone());
        *imp.text_view.borrow_mut() = Some(text_view.clone());
        *imp.edit_view.borrow_mut() = Some(edit_view.clone());
        *imp.split_edit_view.borrow_mut() = Some(split_edit_view.clone());

        // Spell check adapters are created lazily (`ensure_spell_adapters`,
        // from `apply_config`) only once spell check is enabled: building the
        // checker loads the enchant provider + dictionary, which a reader who
        // never edits (and spell check is off by default) shouldn't pay for
        // at startup — and a set language loaded it once per editor.
        if config.spell_check {
            self.ensure_spell_adapters(&config);
        }
        *imp.split_render_view.borrow_mut() = Some(split_render_view.clone());
        *imp.split_render_scroll.borrow_mut() = Some(split_render_scroll.clone());
        *imp.split_paned.borrow_mut() = Some(split_paned.clone());
        *imp.view_clamp.borrow_mut() = Some(view_clamp);
        *imp.view_scroll.borrow_mut() = Some(view_scroll.clone());
        *imp.edit_scroll.borrow_mut() = Some(edit_scroll.clone());
        *imp.split_edit_scroll.borrow_mut() = Some(split_edit_scroll.clone());

        attach_divider_double_click(&split_paned);

        // Outline sidebar
        let outline_listbox = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        outline_listbox.add_css_class("navigation-sidebar");
        // Allow the listbox to shrink below the sidebar's minimum width so
        // AdwOverlaySplitView's reveal animation can measure intermediate
        // widths without the row contents forcing a hard minimum (source of
        // "GtkScrolledWindow needs at least N" warnings during sidebar
        // transitions).
        outline_listbox.set_size_request(0, -1);

        let outline_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .min_content_width(0)
            .child(&outline_listbox)
            .build();

        let outline_title = adw::WindowTitle::new("大纲", "");
        let outline_header = adw::HeaderBar::builder()
            .title_widget(&outline_title)
            .show_end_title_buttons(false)
            .build();

        let outline_toolbar = adw::ToolbarView::builder().build();
        outline_toolbar.add_top_bar(&outline_header);
        outline_toolbar.set_content(Some(&outline_scroll));

        let outline_split = adw::OverlaySplitView::builder()
            // Pinned for the same reason as the vault; the breakpoint handlers
            // in `track_outline_for_breakpoint` own its visibility.
            .pin_sidebar(true)
            .sidebar(&outline_toolbar)
            .content(&content_stack)
            .show_sidebar(false)
            .min_sidebar_width(200.0)
            .max_sidebar_width(320.0)
            .sidebar_width_fraction(0.25)
            // Docked (side-by-side) on wide screens; the breakpoints below flip
            // it to an overlay at ≤1200sp so it doesn't squeeze the content.
            .collapsed(false)
            // Outline sits on the right edge; the left edge holds the vault.
            .sidebar_position(gtk::PackType::End)
            .enable_show_gesture(true)
            .enable_hide_gesture(true)
            .build();

        *imp.outline_listbox.borrow_mut() = Some(outline_listbox.clone());
        *imp.outline_split.borrow_mut() = Some(outline_split.clone());

        // Keep the outline toggle button in sync with the sidebar's actual
        // visibility (breakpoint auto-hide, swipe gesture, F9) so it never
        // needs a double-click to react.
        outline_split
            .bind_property("show-sidebar", &outline_btn, "active")
            .sync_create()
            .bidirectional()
            .build();

        // --- Vault sidebar (left) ---
        // Wraps outline_split (which occupies the right-hand outline slot)
        // as its content, so the two sidebars nest: vault on the far left,
        // outline on the far right, document content in the middle.
        // Window controls (close/min/max) live only on the outermost content
        // header — the sidebar header must not show its own set, or a duplicate
        // appears mid-window when docked and the controls fight for space in
        // the narrow overlay header.
        let vault_header = adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new("笔记库", ""))
            .show_end_title_buttons(false)
            .build();

        let vault_list = gtk::ListView::builder()
            // Single click opens a file / toggles a folder.
            .single_click_activate(true)
            .build();
        vault_list.add_css_class("navigation-sidebar");
        // Allow shrink below sidebar minimum during the reveal animation
        // (same fix as the outline listbox) to avoid "needs at least N"
        // warnings.
        vault_list.set_size_request(0, -1);

        let vault_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&vault_list)
            .build();

        let vault_toolbar = adw::ToolbarView::builder().build();
        vault_toolbar.add_top_bar(&vault_header);
        vault_toolbar.set_content(Some(&vault_scroll));

        // Content is set later (to the content-pane toolbar view) once it is
        // built, so the main header lives inside the content pane — the GNOME
        // two-pane layout where the sidebar has its own titled header bar.
        let vault_split = adw::OverlaySplitView::builder()
            // Pinned: libadwaita must not hide/show the sidebar itself when a
            // breakpoint flips `collapsed` — its auto-show on uncollapse
            // brought the vault back in Split mode, and got persisted as the
            // user's choice. Visibility is owned by `sync_vault_sidebar`.
            .pin_sidebar(true)
            .sidebar_position(gtk::PackType::Start)
            .sidebar(&vault_toolbar)
            // Auto-open only when a vault folder is pinned AND the user hasn't
            // collapsed it — a single file (no vault_root) opens hidden, and a
            // persisted collapsed state is respected on relaunch.
            .show_sidebar(crate::vault::should_show_sidebar(
                config.vault_root.as_deref(),
                config.vault_sidebar_shown,
            ))
            .min_sidebar_width(200.0)
            .max_sidebar_width(320.0)
            .sidebar_width_fraction(0.25)
            // Docked by default at desktop widths (sits beside the content);
            // the narrow breakpoint below flips it to overlay (collapsed=true)
            // so it doesn't squeeze the content on small windows.
            .collapsed(false)
            .enable_show_gesture(true)
            .enable_hide_gesture(true)
            .build();

        *imp.vault_split.borrow_mut() = Some(vault_split.clone());
        *imp.vault_list.borrow_mut() = Some(vault_list.clone());
        *imp.vault_header.borrow_mut() = Some(vault_header.clone());

        // Keep the header toggle button's pressed state in sync with the
        // sidebar no matter which side flips it (button click, F10 via the
        // toggle-vault-sidebar action, or Open Folder revealing the sidebar).
        // Bidirectional binding avoids a hand-rolled connect_toggled that
        // would have to guard against re-entrant set_show_sidebar calls.
        vault_split
            .bind_property("show-sidebar", &vault_toggle, "active")
            .sync_create()
            .bidirectional()
            .build();

        // Persist sidebar visibility. Kept as its own notify::show-sidebar
        // handler (separate from the bind_property above) so the config
        // borrow_mut is scoped to this single statement and never overlaps
        // another borrow of the same RefCell in the same call stack.
        vault_split.connect_show_sidebar_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |split| {
                // Skip programmatic changes (e.g. split-mode auto-hide) so the
                // user's persisted preference isn't overwritten by them.
                if win.imp().sidebar_persist_suppressed.get() {
                    return;
                }
                // Peeking at the overlay on a narrow window isn't a change of
                // the docked preference.
                if split.is_collapsed() {
                    return;
                }
                let mut cfg = win.imp().config.borrow_mut();
                cfg.vault_sidebar_shown = split.shows_sidebar();
                cfg.save();
            }
        ));

        // Menu + open-folder move into the sidebar header only while it is
        // DOCKED and shown, so vault-navigation controls live with the vault.
        // When the sidebar is a narrow overlay (collapsed) they stay in the
        // content header — cramming them into the ~200 px overlay header pushed
        // the window controls off-screen. Re-evaluated on both show-sidebar and
        // collapsed changes (the narrow breakpoint toggles `collapsed`).
        vault_split.connect_show_sidebar_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |split| win.place_nav_buttons(split.shows_sidebar() && !split.is_collapsed())
        ));
        vault_split.connect_collapsed_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |split| win.place_nav_buttons(split.shows_sidebar() && !split.is_collapsed())
        ));
        self.place_nav_buttons(vault_split.shows_sidebar() && !vault_split.is_collapsed());

        // Tree row rendering: expander + folder/file icon + ellipsized name.
        let vault_factory = gtk::SignalListItemFactory::new();
        vault_factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let expander = gtk::TreeExpander::new();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let icon = gtk::Image::new();
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            row.append(&icon);
            row.append(&label);
            expander.set_child(Some(&row));
            item.set_child(Some(&expander));
        });
        vault_factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
                return;
            };
            let Some(row) = expander.child().and_downcast::<gtk::Box>() else {
                return;
            };
            let Some(tree_row) = item.item().and_downcast::<gtk::TreeListRow>() else {
                return;
            };
            expander.set_list_row(Some(&tree_row));
            let Some(info) = tree_row.item().and_downcast::<gio::FileInfo>() else {
                return;
            };
            let Some(icon) = row.first_child().and_downcast::<gtk::Image>() else {
                return;
            };
            let Some(label) = row.last_child().and_downcast::<gtk::Label>() else {
                return;
            };
            let is_dir = info.file_type() == gio::FileType::Directory;
            icon.set_icon_name(Some(if is_dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));
            label.set_text(&info.name().to_string_lossy());
        });

        vault_list.set_factory(Some(&vault_factory));

        vault_list.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |list, pos| {
                let Some(model) = list.model() else { return };
                let Some(obj) = model.item(pos) else { return };
                let Some(tree_row) = obj.downcast_ref::<gtk::TreeListRow>() else {
                    return;
                };
                let Some(info) = tree_row.item().and_downcast::<gio::FileInfo>() else {
                    return;
                };
                if info.file_type() == gio::FileType::Directory {
                    tree_row.set_expanded(!tree_row.is_expanded());
                } else if let Some(path) = win.vault_file_path_for(&info) {
                    win.open_file(&path);
                }
            }
        ));

        // Build the initial model if a vault root is resolvable now.
        if let Some(root) = crate::vault::vault_root(
            config.vault_root.as_deref(),
            imp.current_file.borrow().as_deref(),
        ) {
            self.set_vault_root_ui(&root);
        }

        outline_listbox.connect_row_activated(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, row| {
                let (source_line, rendered_line) = unsafe {
                    (
                        row.data::<i32>("outline-source-line")
                            .map(|p| *p.as_ref())
                            .unwrap_or(0),
                        row.data::<i32>("outline-rendered-line")
                            .map(|p| *p.as_ref())
                            .unwrap_or(0),
                    )
                };
                win.scroll_outline_to(source_line, rendered_line);
                // On narrow screens the sidebar is an overlay covering the content —
                // close it after navigating so the user can see the result.
                let split = win.imp().outline_split.borrow().clone();
                if let Some(split) = split {
                    if split.is_collapsed() {
                        split.set_show_sidebar(false);
                    }
                }
            }
        ));

        // Mobile markdown shortcut bar — visible only on narrow breakpoint
        // while a source view has focus. Buttons wrap selection with markers
        // so users can format text without a hardware keyboard.
        let mobile_action_bar = gtk::ActionBar::builder().build();
        let md_bold = gtk::Button::builder().label("B").tooltip_text("粗体").build();
        md_bold.add_css_class("flat");
        let md_italic = gtk::Button::builder().label("I").tooltip_text("斜体").build();
        md_italic.add_css_class("flat");
        let md_heading = gtk::Button::builder().label("# ").tooltip_text("标题").build();
        md_heading.add_css_class("flat");
        let md_list = gtk::Button::builder().label("•").tooltip_text("列表").build();
        md_list.add_css_class("flat");
        let md_task = gtk::Button::builder().label("☐").tooltip_text("任务").build();
        md_task.add_css_class("flat");
        let md_link = gtk::Button::builder().label("🔗").tooltip_text("链接").build();
        md_link.add_css_class("flat");
        let md_code = gtk::Button::builder().label("</>").tooltip_text("行内代码").build();
        md_code.add_css_class("flat");
        mobile_action_bar.pack_start(&md_bold);
        mobile_action_bar.pack_start(&md_italic);
        mobile_action_bar.pack_start(&md_heading);
        mobile_action_bar.pack_start(&md_list);
        mobile_action_bar.pack_start(&md_task);
        mobile_action_bar.pack_start(&md_link);
        mobile_action_bar.pack_start(&md_code);
        *imp.mobile_action_bar.borrow_mut() = Some(mobile_action_bar.clone());

        md_bold.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                if let Some(sv) = win.get_active_source_view() {
                    wrap_selection(&sv, "**", "**");
                }
            }
        ));
        md_italic.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                if let Some(sv) = win.get_active_source_view() {
                    wrap_selection(&sv, "*", "*");
                }
            }
        ));
        md_heading.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                if let Some(sv) = win.get_active_source_view() {
                    insert_line_prefix(&sv, "# ");
                }
            }
        ));
        md_list.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                if let Some(sv) = win.get_active_source_view() {
                    insert_line_prefix(&sv, "- ");
                }
            }
        ));
        md_task.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                if let Some(sv) = win.get_active_source_view() {
                    insert_line_prefix(&sv, "- [ ] ");
                }
            }
        ));
        md_link.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                if let Some(sv) = win.get_active_source_view() {
                    insert_link(&sv);
                }
            }
        ));
        md_code.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                if let Some(sv) = win.get_active_source_view() {
                    wrap_selection(&sv, "`", "`");
                }
            }
        ));

        // Layout — the content pane's toolbar view holds the main header,
        // search bar, and (dynamically) the mobile action bar / footer. It
        // becomes the vault split's content, so the vault sidebar sits to its
        // left with its own header bar (GNOME two-pane pattern).
        let toolbar_view = adw::ToolbarView::builder().build();
        toolbar_view.add_top_bar(&header);
        toolbar_view.add_top_bar(&search_bar);
        // mobile_action_bar is attached/detached dynamically by
        // update_mobile_bar_visibility — see that method for why.
        toolbar_view.set_content(Some(&outline_split));
        *imp.toolbar_view.borrow_mut() = Some(toolbar_view.clone());

        vault_split.set_content(Some(&toolbar_view));
        imp.toast_overlay.set_child(Some(&vault_split));
        self.set_content(Some(&imp.toast_overlay));

        // Compact-mode setters shared by the narrow (≤600sp) and compact
        // (≤800sp) breakpoints. AdwBreakpoint applies only ONE breakpoint at a
        // time — the tightest matching one — so the narrower breakpoint must
        // REPEAT the wider one's setters (per the libadwaita docs). Both:
        //  - flip the vault sidebar to an overlay (a docked sidebar + content
        //    overflows the window below ~800px), and
        //  - hide the word-count/reading-time label, the window title, and the
        //    view/edit/split toggle group (still reachable from the ☰ menu) so
        //    the header fits a narrow window.
        // Full narrow-mode setters (vault overlay + hide the word-count label,
        // window title, and view/edit/split toggle group). Used only by the
        // narrow breakpoint — the wider compact breakpoint overlays the sidebar
        // but keeps the header buttons, since the header alone fits down to
        // ~480 px. Factored out so the intent is one place.
        let add_compact_setters = |bp: &adw::Breakpoint| {
            bp.add_setter(&vault_split, "collapsed", Some(&true.to_value()));
            bp.add_setter(&outline_split, "collapsed", Some(&true.to_value()));
            bp.add_setter(&stats_label, "visible", Some(&false.to_value()));
            bp.add_setter(&title_widget, "visible", Some(&false.to_value()));
            bp.add_setter(&toggle_box, "visible", Some(&false.to_value()));
        };

        // -- Narrow / mobile breakpoint (≤600sp): compact setters + phone extras.
        let narrow_bp = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 600sp").unwrap(),
        );
        add_compact_setters(&narrow_bp);
        // outline_split is `collapsed(true)` permanently — no setter needed.
        // Reduce text view margins for narrow screens
        narrow_bp.add_setter(&text_view, "left-margin", Some(&16i32.to_value()));
        narrow_bp.add_setter(&text_view, "right-margin", Some(&16i32.to_value()));
        narrow_bp.add_setter(&text_view, "top-margin", Some(&16i32.to_value()));
        narrow_bp.add_setter(&split_render_view, "left-margin", Some(&12i32.to_value()));
        narrow_bp.add_setter(&split_render_view, "right-margin", Some(&12i32.to_value()));
        narrow_bp.add_setter(&split_render_view, "top-margin", Some(&12i32.to_value()));
        // Break long words (long URLs, scaled-up headings) on narrow screens —
        // GTK_WRAP_WORD can't split single words wider than the viewport.
        narrow_bp.add_setter(&text_view, "wrap-mode", Some(&gtk::WrapMode::WordChar.to_value()));
        narrow_bp.add_setter(
            &split_render_view,
            "wrap-mode",
            Some(&gtk::WrapMode::WordChar.to_value()),
        );

        // When entering narrow mode, switch away from split if active and
        // remember the narrow state so the mobile action bar can show on focus.
        narrow_bp.connect_apply(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.imp().narrow_active.set(true);
                if win.imp().view_mode.get() == ViewMode::Split {
                    win.set_view_mode(ViewMode::View);
                }
                win.update_mobile_bar_visibility();
            }
        ));
        narrow_bp.connect_unapply(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.imp().narrow_active.set(false);
                win.update_mobile_bar_visibility();
                // The breakpoint just restored the margins it saved on apply,
                // which may predate a content-width change — recompute after.
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    win,
                    move || win.apply_content_margins()
                ));
            }
        ));

        // Compact breakpoint (≤800sp): ONLY flip the vault sidebar to an
        // overlay. A docked sidebar (200) + full header (~480) needs ~680 px,
        // so below ~800 it must overlay — but the header itself still fits down
        // to ~480 px, so the header buttons stay visible here. Header slimming
        // happens later, in the narrow breakpoint. Collapsing only changes
        // docked→overlay, so `vault_sidebar_shown` is never written on resize.
        let compact_bp = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 800sp").unwrap(),
        );
        compact_bp.add_setter(&vault_split, "collapsed", Some(&true.to_value()));
        compact_bp.add_setter(&outline_split, "collapsed", Some(&true.to_value()));

        // Medium breakpoint (≤1000sp): flip the outline to an overlay (it is
        // docked side-by-side only above 1000sp, where there's room for vault +
        // content + outline ≈ 900 px) and auto-hide it on entry so it doesn't
        // float over the content when the window first gets too narrow to dock.
        let medium_bp = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 1000sp").unwrap(),
        );
        medium_bp.add_setter(&outline_split, "collapsed", Some(&true.to_value()));
        self.track_outline_for_breakpoint(&medium_bp);
        self.track_outline_for_breakpoint(&compact_bp);
        self.track_outline_for_breakpoint(&narrow_bp);
        for bp in [&medium_bp, &compact_bp, &narrow_bp] {
            // Idle: after the setters (collapsed) have settled.
            let resync = glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_: &adw::Breakpoint| {
                    glib::idle_add_local_once(glib::clone!(
                        #[weak]
                        win,
                        move || win.sync_vault_sidebar()
                    ));
                }
            );
            bp.connect_apply(resync.clone());
            bp.connect_unapply(resync);
        }
        // Register widest → narrowest. AdwWindow applies the LAST-added
        // breakpoint whose condition matches, so the narrowest matching one
        // must be added last to win (per the libadwaita adaptive-layout docs).
        self.add_breakpoint(medium_bp);
        self.add_breakpoint(compact_bp);
        self.add_breakpoint(narrow_bp);

        // Track source-view focus to show/hide the mobile action bar.
        for view in [&edit_view, &split_edit_view] {
            view.connect_has_focus_notify(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_| {
                    win.update_mobile_bar_visibility();
                }
            ));
        }

        // Swipe gesture on the content area: horizontal swipe toggles
        // Read ↔ Edit on narrow screens. Skip when sidebar is open (sidebar
        // already eats the gesture).
        let content_swipe = gtk::GestureSwipe::new();
        content_swipe.set_propagation_phase(gtk::PropagationPhase::Bubble);
        content_swipe.connect_swipe(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, vx, vy| {
                if !win.imp().narrow_active.get() {
                    return;
                }
                // Horizontal-dominant swipe with meaningful velocity only.
                if vx.abs() < vy.abs() * 1.5 || vx.abs() < 300.0 {
                    return;
                }
                let next = match (win.imp().view_mode.get(), vx > 0.0) {
                    (ViewMode::View, false) => ViewMode::Edit,
                    (ViewMode::Edit, true) => ViewMode::View,
                    _ => return,
                };
                win.set_view_mode(next);
            }
        ));
        content_stack.add_controller(content_swipe);

        // -- Connect toggle signals --
        view_btn.connect_toggled(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |btn| {
                if btn.is_active() {
                    win.set_view_mode(ViewMode::View);
                }
            }
        ));
        edit_btn.connect_toggled(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |btn| {
                if btn.is_active() {
                    win.set_view_mode(ViewMode::Edit);
                }
            }
        ));
        split_btn.connect_toggled(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |btn| {
                if btn.is_active() {
                    win.set_view_mode(ViewMode::Split);
                }
            }
        ));

        // Connect open button
        open_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.pick_vault_folder();
            }
        ));

        // Track edits in edit view
        edit_view.buffer().connect_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_buf| {
                if !win.imp().programmatic_edit.get() {
                    win.mark_modified();
                }
                win.schedule_stats_update();
            }
        ));

        split_edit_view.buffer().connect_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |buf| {
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
                win.imp().file_content.replace(text);
                if !win.imp().programmatic_edit.get() {
                    win.mark_modified();
                }
                // Debounce split render: wait 250ms after last keystroke
                win.schedule_split_render();
                win.schedule_stats_update();
            }
        ));

        // Replace row only makes sense in Edit/Split — start hidden.
        self.update_replace_visibility();

        // Footer bar (A.2): built once, parked into AdwToolbarView's bottom
        // slot dynamically by `update_footer_visibility` based on the
        // `show_footer` preference. Same attach/detach pattern as the mobile
        // action bar to dodge GtkRevealer measurement warnings.
        let footer_bar = gtk::ActionBar::new();
        footer_bar.add_css_class("manuscript-footer");
        let footer_inner = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        footer_inner.add_css_class("caption");

        fn make_footer_segment(icon: &str) -> (gtk::Box, gtk::Label, gtk::Image) {
            let seg = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(14);
            img.add_css_class("dim-label");
            let label = gtk::Label::new(None);
            label.add_css_class("dim-label");
            label.add_css_class("caption");
            seg.append(&img);
            seg.append(&label);
            (seg, label, img)
        }

        let (words_seg, words_label, _) = make_footer_segment("text-x-generic-symbolic");
        let (time_seg, time_label, _) = make_footer_segment("alarm-symbolic");
        let (goal_seg, goal_label, goal_icon) = make_footer_segment("non-starred-symbolic");
        goal_seg.set_visible(false);

        footer_inner.append(&words_seg);
        footer_inner.append(&time_seg);
        footer_inner.append(&goal_seg);
        footer_bar.pack_end(&footer_inner);
        imp.footer_bar.replace(Some(footer_bar));
        imp.footer_words_label.replace(Some(words_label));
        imp.footer_time_label.replace(Some(time_label));
        imp.footer_goal_label.replace(Some(goal_label));
        imp.footer_goal_box.replace(Some(goal_seg));
        imp.footer_goal_icon.replace(Some(goal_icon));
        // Initial attach decision — config is already loaded into imp.config.
        self.update_footer_visibility();

        // A.3: 60-second tick to accumulate today_words even when the user
        // edits without saving. Coalesced via the stored SourceId so the
        // closure does not outlive the window — cancelled in close_request.
        let tick_id = glib::timeout_add_seconds_local(
            60,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    win.track_today_words();
                    glib::ControlFlow::Continue
                }
            ),
        );
        imp.today_tick_source.replace(Some(tick_id));

        // Welcome state
        self.show_welcome();

        // Hand initial keyboard focus to the visible document pane so no header
        // button (the sidebar toggle, menu, …) shows a focus ring on window
        // open. Deferred to idle so it runs after the window is mapped and GTK
        // has assigned its own default focus, which this then overrides.
        let stack_for_focus = content_stack.clone();
        glib::idle_add_local_once(move || {
            if let Some(child) = stack_for_focus.visible_child() {
                child.grab_focus();
            }
        });
    }

    /// Move the menu (☰) and open-folder buttons between the content header and
    /// the vault sidebar header. `in_sidebar` = the sidebar is shown, so the
    /// vault-navigation controls live with the vault. The sidebar toggle stays
    /// leading-first in the content header at all times; save stays in the
    /// content header; the outline toggle never moves.
    fn place_nav_buttons(&self, in_sidebar: bool) {
        let imp = self.imp();
        let (Some(content), Some(side), Some(menu), Some(open), Some(save)) = (
            imp.header_bar.borrow().clone(),
            imp.vault_header.borrow().clone(),
            imp.menu_button.borrow().clone(),
            imp.open_button.borrow().clone(),
            imp.save_btn.borrow().clone(),
        ) else {
            return;
        };

        // AdwHeaderBar wraps its children in an internal box, so `parent()`
        // isn't the bar — walk up via `ancestor()` and remove there, otherwise
        // the widget keeps its parent and `pack_start` trips
        // `gtk_widget_get_parent(child) == NULL`.
        for btn in [
            menu.upcast_ref::<gtk::Widget>(),
            open.upcast_ref(),
            save.upcast_ref(),
        ] {
            if let Some(hb) = btn
                .ancestor(adw::HeaderBar::static_type())
                .and_downcast::<adw::HeaderBar>()
            {
                hb.remove(btn);
            }
        }

        // Re-pack in a fixed order. In the sidebar header the open-folder
        // button sits on the leading edge and the menu (☰) on the trailing
        // edge, framing the "Vault" title. In the content header the sidebar
        // toggle is already leading-first (never removed); menu/open follow it
        // only when the sidebar is hidden. Save is always packed last in the
        // content header so its slot is stable across toggles.
        if in_sidebar {
            side.pack_start(&open);
            side.pack_end(&menu);
        } else {
            content.pack_start(&menu);
            content.pack_start(&open);
        }
        content.pack_start(&save);
    }

    /// (Re)build the vault file tree rooted at `root` and assign it to the
    /// vault sidebar's `ListView`. Each directory level gets its own
    /// `DirectoryList`; `TreeListModel`'s child-model closure creates the
    /// next level lazily as rows expand. Every level is wrapped in a
    /// `FilterListModel` (see `vault_md_filter`) so only directories and
    /// Markdown files are shown.
    fn set_vault_root_ui(&self, root: &std::path::Path) {
        let imp = self.imp();
        let Some(list) = imp.vault_list.borrow().clone() else {
            return;
        };
        let attrs = "standard::name,standard::type";
        let root_file = gio::File::for_path(root);
        let dir_list = gtk::DirectoryList::new(Some(attrs), Some(&root_file));
        let filtered = gtk::FilterListModel::new(Some(dir_list), Some(vault_md_filter()));
        let tree = gtk::TreeListModel::new(filtered, false, false, move |obj| {
            let info = obj.downcast_ref::<gio::FileInfo>()?;
            if info.file_type() != gio::FileType::Directory {
                return None;
            }
            // GtkDirectoryList always sets "standard::file" on every
            // FileInfo it emits regardless of the requested attribute
            // list (confirmed via the bundled Gtk-4.0.gir doc comment on
            // GtkDirectoryList: "The GFileInfos returned ... have the
            // 'standard::file' attribute set to the GFile they refer
            // to ... in the same way as g_file_enumerator_get_child()").
            let child = info.attribute_object("standard::file").and_downcast::<gio::File>()?;
            let child_list = gtk::DirectoryList::new(Some(attrs), Some(&child));
            let child_filtered =
                gtk::FilterListModel::new(Some(child_list), Some(vault_md_filter()));
            Some(child_filtered.upcast::<gio::ListModel>())
        });
        let selection = gtk::SingleSelection::new(Some(tree));
        list.set_model(Some(&selection));
        if let Some(header) = imp.vault_header.borrow().as_ref() {
            // A document-portal root is `/run/user/<uid>/doc/<id>` — show the
            // real folder's name (from the open file's host path) instead of
            // the opaque id.
            let portal_name = is_portal_path(&root.join("_"))
                .then(|| imp.current_file.borrow().clone())
                .flatten()
                .map(|f| display_dir(&f))
                .and_then(|d| d.rsplit('/').next().map(str::to_string))
                .filter(|n| !n.is_empty());
            let name = portal_name.unwrap_or_else(|| {
                root.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            });
            header.set_title_widget(Some(&adw::WindowTitle::new("笔记库", &name)));
        }
        *imp.vault_current_root.borrow_mut() = Some(root.to_path_buf());
    }

    /// Resolve the on-disk path for a `FileInfo` yielded by the vault tree
    /// model. Uses the same `"standard::file"` attribute guaranteed by
    /// `GtkDirectoryList` (see `set_vault_root_ui`).
    fn vault_file_path_for(&self, info: &gio::FileInfo) -> Option<std::path::PathBuf> {
        info.attribute_object("standard::file")
            .and_downcast::<gio::File>()
            .and_then(|f| f.path())
    }

    /// Expand ancestor rows and select `path`'s row in the vault tree, if
    /// present. `DirectoryList` populates asynchronously (each level fills
    /// in over one or more `items-changed` emissions), so a single
    /// top-to-bottom pass right after `set_vault_root_ui` will usually only
    /// see the root level. We do one immediate pass, and if that doesn't
    /// land the selection, retry on `items-changed` — bounded to a handful
    /// of attempts and disconnected the moment we either succeed or run
    /// out of attempts, so the handler never outlives its usefulness.
    fn reveal_current_in_vault(&self, path: &std::path::Path) {
        let imp = self.imp();
        let Some(list) = imp.vault_list.borrow().clone() else {
            return;
        };
        let Some(sel) = list.model().and_downcast::<gtk::SingleSelection>() else {
            return;
        };
        let Some(tree) = sel.model().and_downcast::<gtk::TreeListModel>() else {
            return;
        };
        let target = path.to_path_buf();

        if reveal_pass(&sel, &tree, &target) {
            return;
        }

        let attempts = std::rc::Rc::new(std::cell::Cell::new(0u8));
        let handler_id: std::rc::Rc<std::cell::RefCell<Option<glib::SignalHandlerId>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handler_id_for_disconnect = handler_id.clone();
        let id = tree.connect_items_changed(glib::clone!(
            #[weak]
            sel,
            #[strong]
            target,
            move |t, _, _, _| {
                attempts.set(attempts.get() + 1);
                let found = reveal_pass(&sel, t, &target);
                if found || attempts.get() >= 8 {
                    if let Some(id) = handler_id_for_disconnect.borrow_mut().take() {
                        t.disconnect(id);
                    }
                }
            }
        ));
        *handler_id.borrow_mut() = Some(id);
    }

    fn setup_actions(&self) {
        // Open
        let open_action = gio::SimpleAction::new("open", None);
        open_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| win.show_open_dialog()
        ));
        self.add_action(&open_action);

        // Save
        let save_action = gio::SimpleAction::new("save", None);
        save_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| win.save_file()
        ));
        self.add_action(&save_action);

        // Toggle edit
        let toggle_edit = gio::SimpleAction::new("toggle-edit", None);
        toggle_edit.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                let current = win.imp().view_mode.get();
                win.set_view_mode(if current == ViewMode::Edit {
                    ViewMode::View
                } else {
                    ViewMode::Edit
                });
            }
        ));
        self.add_action(&toggle_edit);

        // Toggle split
        let toggle_split = gio::SimpleAction::new("toggle-split", None);
        toggle_split.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                let current = win.imp().view_mode.get();
                win.set_view_mode(if current == ViewMode::Split {
                    ViewMode::View
                } else {
                    ViewMode::Split
                });
            }
        ));
        self.add_action(&toggle_split);

        // Search (Ctrl+F): toggle find bar, hide replace row.
        let search_action = gio::SimpleAction::new("search", None);
        search_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                win.open_search_bar(false);
            }
        ));
        self.add_action(&search_action);

        // Find & Replace (Ctrl+H): show find bar with replace row exposed.
        // Replace only makes sense in Edit/Split — silently no-op in View mode.
        let find_replace_action = gio::SimpleAction::new("find-replace", None);
        find_replace_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                let editing = matches!(
                    win.imp().view_mode.get(),
                    ViewMode::Edit | ViewMode::Split
                );
                win.open_search_bar(editing);
            }
        ));
        self.add_action(&find_replace_action);

        // Print
        let print_action = gio::SimpleAction::new("print", None);
        print_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                win.print_file();
            }
        ));
        self.add_action(&print_action);

        // Quick Open (Ctrl+Shift+O) — recents + sibling files, fuzzy
        let quick_open_action = gio::SimpleAction::new("quick-open", None);
        quick_open_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                win.show_quick_open();
            }
        ));
        self.add_action(&quick_open_action);

        // Export PDF
        let export_pdf_action = gio::SimpleAction::new("export-pdf", None);
        export_pdf_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                win.export_pdf_file();
            }
        ));
        self.add_action(&export_pdf_action);

        // Open recent (takes string parameter)
        let open_recent_action =
            gio::SimpleAction::new("open-recent", Some(glib::VariantTy::STRING));
        open_recent_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, param| {
                if let Some(param) = param {
                    if let Some(path_str) = param.str() {
                        let path = PathBuf::from(path_str);
                        win.open_file(&path);
                    }
                }
            }
        ));
        self.add_action(&open_recent_action);

        // Preferences
        let prefs_action = gio::SimpleAction::new("preferences", None);
        prefs_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                let config = win.imp().config.borrow().clone();
                let win2 = win.clone();
                preferences::show_preferences(&win, &config, move |new_config| {
                    win2.apply_config(new_config);
                });
            }
        ));
        self.add_action(&prefs_action);

        // Writing statistics (A.4) — gated on show_footer && writing_stats.
        let writing_stats_action = gio::SimpleAction::new("writing-stats", None);
        writing_stats_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                win.present_writing_stats_dialog();
            }
        ));
        {
            let cfg = self.imp().config.borrow();
            writing_stats_action.set_enabled(cfg.show_footer && cfg.writing_stats);
        }
        self.add_action(&writing_stats_action);

        // Shortcuts
        let shortcuts_action = gio::SimpleAction::new("shortcuts", None);
        shortcuts_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                show_shortcuts_window(&win);
            }
        ));
        self.add_action(&shortcuts_action);

        // About
        let about_action = gio::SimpleAction::new("about", None);
        about_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                show_about_dialog(&win);
            }
        ));
        self.add_action(&about_action);

        let go_back = gio::SimpleAction::new("go-back", None);
        go_back.set_enabled(false);
        go_back.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| win.navigate_history(false)
        ));
        self.add_action(&go_back);
        let go_forward = gio::SimpleAction::new("go-forward", None);
        go_forward.set_enabled(false);
        go_forward.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| win.navigate_history(true)
        ));
        self.add_action(&go_forward);

        let new_action = gio::SimpleAction::new("new", None);
        new_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                win.new_file();
            }
        ));
        self.add_action(&new_action);

        let reload_action = gio::SimpleAction::new("reload-file", None);
        reload_action.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                win.reload_from_disk();
            }
        ));
        self.add_action(&reload_action);

        let open_wikilink =
            gio::SimpleAction::new("open-wikilink", Some(glib::VariantTy::STRING));
        open_wikilink.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, param| {
                if let Some(name) = param.and_then(|p| p.get::<String>()) {
                    win.navigate_wikilink(&name);
                }
            }
        ));
        self.add_action(&open_wikilink);

        // Toggle vault sidebar. F10 is handled directly in
        // setup_key_capture (not via accelerator) so it works reliably
        // while a sourceview5::View has focus; this action backs any
        // future menu item and still flips the same split property. The
        // button's pressed state stays in sync via the bind_property set
        // up alongside vault_split in setup_ui.
        let toggle_vault = gio::SimpleAction::new("toggle-vault-sidebar", None);
        toggle_vault.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| {
                if let Some(split) = win.imp().vault_split.borrow().as_ref() {
                    split.set_show_sidebar(!split.shows_sidebar());
                };
            }
        ));
        self.add_action(&toggle_vault);

        // Open Folder… — pick a vault root via the GTK-drawn file chooser.
        let open_folder = gio::SimpleAction::new("open-folder", None);
        open_folder.connect_activate(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _| win.pick_vault_folder()
        ));
        self.add_action(&open_folder);
    }

    /// Open a native folder picker and, on acceptance, persist the chosen
    /// path as the vault root, rebuild the vault tree, and reveal the
    /// sidebar.
    fn pick_vault_folder(&self) {
        let chooser = make_file_chooser(
            self,
            "打开文件夹",
            gtk::FileChooserAction::SelectFolder,
            "打开",
        );
        chooser.connect_response(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[strong]
            chooser,
            move |_, resp| {
                if resp == gtk::ResponseType::Accept {
                    if let Some(path) = chooser.file().and_then(|f| f.path()) {
                        {
                            let mut cfg = win.imp().config.borrow_mut();
                            cfg.vault_root = Some(path.to_string_lossy().to_string());
                            cfg.save();
                        }
                        win.set_vault_root_ui(&path);
                        if let Some(split) = win.imp().vault_split.borrow().as_ref() {
                            split.set_show_sidebar(true);
                        }
                    }
                }
                // Teardown is handled by `make_file_chooser`, which hides and
                // then destroys the dialog once every response handler has
                // run. Destroying here too would free the widget while this
                // very closure is still executing.
            }
        ));
        chooser.show();
    }

    /// Close the currently open file and return to the welcome screen.
    /// Editor buffers are cleared, file watcher cancelled, title reset.
    fn close_file(&self) {
        let imp = self.imp();
        self.remember_scroll();
        self.stop_file_watch();
        imp.current_file.replace(None);
        imp.file_content.replace(String::new());
        imp.modified.set(false);
        // A.3: clear stale doc-word baseline so the next open seeds fresh.
        imp.last_doc_words.set(0);
        // Invalidate cached render so a subsequent re-open of the same file
        // (or a fresh file with identical contents) actually re-paints.
        imp.last_view_render_hash.set(0);
        imp.last_split_render_hash.set(0);
        if let Some(sv) = imp.edit_view.borrow().as_ref() {
            self.set_editor_text(sv, "");
        }
        if let Some(sv) = imp.split_edit_view.borrow().as_ref() {
            self.set_editor_text(sv, "");
        }
        self.set_view_mode(ViewMode::View);
        self.show_welcome();
        if let Some(title) = imp.title_label.borrow().as_ref() {
            title.set_title("Manuscript");
            title.set_subtitle("");
        }
        self.update_save_button();
        self.update_stats();
    }

    fn reload_from_disk(&self) {
        let imp = self.imp();
        let path = match imp.current_file.borrow().clone() {
            Some(p) => p,
            None => return,
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                let toast = adw::Toast::new(&format!("Reload failed: {}", e));
                imp.toast_overlay.add_toast(toast);
                return;
            }
        };
        // A.3: re-seed baseline from disk contents before any tracking runs.
        imp.last_doc_words.set(content.split_whitespace().count() as u32);
        imp.file_content.replace(content.clone());
        imp.modified.set(false);
        // Sync editor buffers so they match disk
        if let Some(sv) = imp.edit_view.borrow().as_ref() {
            self.set_editor_text(sv, &content);
        }
        if let Some(sv) = imp.split_edit_view.borrow().as_ref() {
            self.set_editor_text(sv, &content);
        }
        self.refresh_view();
        self.update_save_button();
        self.update_stats();
        let toast = adw::Toast::new("已从磁盘重新载入");
        toast.set_timeout(1);
        imp.toast_overlay.add_toast(toast);
    }

    /// Walk the rendered TextBuffer and extract headings (level + text + line number).
    /// Headings can be fragmented in the buffer because inline-code/strong/em tags
    /// toggle the heading tag off mid-line. We grab text up to the end of the heading
    /// line, skip the heading-rule line that follows H1/H2, and dedup by line.
    fn collect_headings(&self) -> Vec<(u8, String, i32)> {
        let imp = self.imp();
        let Some(tv) = imp.text_view.borrow().clone() else {
            return Vec::new();
        };
        let buffer = tv.buffer();
        let table = buffer.tag_table();
        let mut tags = Vec::new();
        for level in 1..=6u8 {
            if let Some(tag) = table.lookup(&format!("h{}", level)) {
                tags.push((level, tag));
            }
        }
        if tags.is_empty() {
            return Vec::new();
        }

        let mut out: Vec<(u8, String, i32)> = Vec::new();
        let mut iter = buffer.start_iter();
        let line_count = buffer.line_count();
        loop {
            let mut found = None;
            for (level, tag) in &tags {
                if iter.starts_tag(Some(tag)) {
                    found = Some(*level);
                    break;
                }
            }
            if let Some(level) = found {
                let line = iter.line();
                // Already collected this heading line — skip past it
                if out.last().map(|(_, _, ln)| *ln == line).unwrap_or(false) {
                    iter.forward_line();
                    continue;
                }
                // Capture whole heading line text (handles inline-code splitting the tag)
                let mut line_end = iter;
                if !line_end.ends_line() {
                    line_end.forward_to_line_end();
                }
                let text = buffer.text(&iter, &line_end, false).to_string();
                let trimmed = text.trim().to_string();
                if !trimmed.is_empty() {
                    out.push((level, trimmed, line));
                }
                // Move past the heading line. (Earlier versions skipped an
                // extra line for the H1/H2 underline; v0.2.9 removed that
                // underline, so a single forward_line is correct now.)
                iter = line_end;
                let _ = iter.forward_line();
                if iter.line() >= line_count {
                    break;
                }
                continue;
            }
            if !iter.forward_to_tag_toggle(None::<&gtk::TextTag>) {
                break;
            }
        }
        out
    }

    fn rebuild_outline(&self) {
        let imp = self.imp();
        let Some(listbox) = imp.outline_listbox.borrow().clone() else {
            return;
        };

        // Drop existing rows
        while let Some(child) = listbox.first_child() {
            listbox.remove(&child);
        }

        let rendered = self.collect_headings();
        let source = renderer::collect_source_headings(&self.current_source_text());

        if rendered.is_empty() && source.is_empty() {
            let empty = gtk::Label::builder()
                .label("无标题")
                .margin_top(12)
                .margin_bottom(12)
                .build();
            empty.add_css_class("dim-label");
            let row = gtk::ListBoxRow::builder()
                .selectable(false)
                .activatable(false)
                .child(&empty)
                .build();
            listbox.append(&row);
            return;
        }

        // Source headings are authoritative (they always exist as long as the user
        // has markdown loaded). Rendered headings are paired by index for scrolling
        // the read-mode TextView. If the rendered list is shorter (mid-render race,
        // unusual edge cases) we fall back to scrolling line 0 there.
        let count = source.len().max(rendered.len());
        for i in 0..count {
            let (level, text, source_line) = source
                .get(i)
                .cloned()
                .or_else(|| rendered.get(i).cloned())
                .unwrap();
            let rendered_line = rendered.get(i).map(|(_, _, l)| *l).unwrap_or(0);

            let label = gtk::Label::builder()
                .label(&text)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            label.set_margin_start(8 + (level as i32 - 1) * 12);
            label.set_margin_top(4);
            label.set_margin_bottom(4);
            label.set_margin_end(8);
            if level == 1 {
                label.add_css_class("heading");
            } else if level >= 4 {
                label.add_css_class("dim-label");
            }
            let row = gtk::ListBoxRow::builder().child(&label).build();
            unsafe {
                row.set_data("outline-source-line", source_line);
                row.set_data("outline-rendered-line", rendered_line);
            }
            listbox.append(&row);
        }
    }

    /// Return the current authoritative markdown source. In Edit/Split mode the
    /// source view buffer may have unsaved edits not yet flushed to `file_content`,
    /// so we pull from the live buffer in those modes.
    fn current_source_text(&self) -> String {
        let imp = self.imp();
        let live = match imp.view_mode.get() {
            ViewMode::Edit => imp.edit_view.borrow().clone(),
            ViewMode::Split => imp.split_edit_view.borrow().clone(),
            ViewMode::View => None,
        };
        if let Some(sv) = live {
            let buf = sv.buffer();
            return buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
        }
        imp.file_content.borrow().clone()
    }

    fn scroll_outline_to(&self, source_line: i32, rendered_line: i32) {
        let imp = self.imp();
        // Outline jumps are navigation: remember where we were so Back returns.
        if let Some(prev) = self.current_location() {
            let in_view = imp.view_mode.get() == ViewMode::View;
            let (tv, line) = if in_view {
                (imp.text_view.borrow().clone(), rendered_line)
            } else {
                (self.active_text_view(), source_line)
            };
            let offset = tv
                .and_then(|tv| tv.buffer().iter_at_line(line))
                .map(|it| it.offset())
                .unwrap_or(0);
            let target = Pos { offset, delta: 0, source: !in_view };
            if prev.pos != target {
                imp.nav_history.borrow_mut().push(prev);
                self.sync_history_ui();
            }
        }
        match imp.view_mode.get() {
            ViewMode::View => {
                // Top-align the heading exactly (scroll_to_mark would only
                // bring it into view).
                let (Some(tv), Some(scroll)) = (
                    imp.text_view.borrow().clone(),
                    imp.view_scroll.borrow().clone(),
                ) else { return };
                let buf = tv.buffer();
                let Some(iter) = buf.iter_at_line(rendered_line) else { return };
                let rect = tv.iter_location(&iter);
                let vadj = scroll.vadjustment();
                let max = (vadj.upper() - vadj.page_size()).max(0.0);
                let target = value_for_buffer_y(&tv, &vadj, rect.y() as f64).clamp(0.0, max);
                vadj.set_value(target);
            }
            ViewMode::Edit => {
                if let Some(sv) = imp.edit_view.borrow().clone() {
                    scroll_source_view_to_line(&sv, source_line);
                }
            }
            ViewMode::Split => {
                if let Some(sv) = imp.split_edit_view.borrow().clone() {
                    scroll_source_view_to_line(&sv, source_line);
                }
                if let Some(tv) = imp.split_render_view.borrow().clone() {
                    let buf = tv.buffer();
                    if let Some(iter) = buf.iter_at_line(rendered_line) {
                        let mark = buf.create_mark(None, &iter, false);
                        tv.scroll_to_mark(&mark, 0.1, true, 0.0, 0.0);
                        buf.delete_mark(&mark);
                    }
                }
            }
        }
    }

    /// Watch fullscreen state and apply distraction-free mode when enabled.
    fn setup_fullscreen_tracking(&self) {
        self.connect_fullscreened_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.apply_distraction_free();
            }
        ));
    }

    /// Apply distraction-free state based on current fullscreen + config toggle.
    /// In fullscreen with toggle ON: hide header, search bar, outline sidebar,
    /// and scrollbars. Restore everything when not fullscreen or when toggle OFF.
    fn apply_distraction_free(&self) {
        let imp = self.imp();
        let enabled = imp.config.borrow().distraction_free_fullscreen;
        let fs = self.is_fullscreen();
        let hide = fs && enabled;

        if let Some(h) = imp.header_bar.borrow().clone() {
            h.set_visible(!hide);
        }
        // SearchBar is a no-op when search_mode is false (it self-collapses),
        // so we don't touch it here. User-initiated search (Ctrl+F) still works.
        if hide {
            if let Some(split) = imp.outline_split.borrow().clone() {
                split.set_show_sidebar(false);
            }
        }
        let scroll_policy = if hide { gtk::PolicyType::External } else { gtk::PolicyType::Automatic };
        for slot in [
            imp.view_scroll.borrow().clone(),
            imp.edit_scroll.borrow().clone(),
            imp.split_edit_scroll.borrow().clone(),
            imp.split_render_scroll.borrow().clone(),
        ] {
            if let Some(sw) = slot {
                sw.set_vscrollbar_policy(scroll_policy);
            }
        }
    }

    /// Wire focus mode + typewriter mode to source view cursor movement.
    fn setup_writer_modes(&self) {
        let imp = self.imp();
        for (view_slot, scroll_slot, is_split) in [
            (&imp.edit_view, &imp.edit_scroll, false),
            (&imp.split_edit_view, &imp.split_edit_scroll, true),
        ] {
            let Some(view) = view_slot.borrow().clone() else { continue };
            let buf = view.buffer();
            let view_for_cursor = view.clone();
            buf.connect_cursor_position_notify(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |b| {
                    win.apply_focus_mode(b);
                    win.apply_typewriter_mode(&view_for_cursor);
                    if is_split {
                        win.sync_split_render_scroll();
                    }
                }
            ));
            // No connect_changed: every text edit moves the cursor, so the
            // cursor-position-notify above already runs apply_focus_mode.
            // The redundant changed handler used to double the per-keystroke
            // work (full-buffer tag scan + paragraph walk).
            // Recompute typewriter margins when the scrolled window is resized.
            // Coalesce via a single deferred idle: a single resize gesture
            // emits a burst of page-size-notify events.
            if let Some(scroll) = scroll_slot.borrow().clone() {
                scroll.vadjustment().connect_page_size_notify(glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_| {
                        let imp = win.imp();
                        if imp.margins_idle.borrow().is_some() {
                            return;
                        }
                        let id = glib::idle_add_local_once(glib::clone!(
                            #[weak(rename_to = w)]
                            win,
                            move || {
                                w.imp().margins_idle.replace(None);
                                w.sync_typewriter_margins();
                            }
                        ));
                        imp.margins_idle.replace(Some(id));
                    }
                ));
            }
        }
        // Initial margin pass — config may have typewriter enabled at startup.
        self.sync_typewriter_margins();
    }

    fn apply_focus_mode(&self, buf: &gtk::TextBuffer) {
        let imp = self.imp();
        let enabled = imp.config.borrow().focus_mode;
        // Hot path #1: when focus mode is off and no stale tag is currently
        // applied, skip all work. The full-buffer remove_tag_by_name + bounds
        // scan + line walk used to run on every keystroke.
        if !enabled && !imp.focus_dim_active.get() {
            return;
        }
        ensure_focus_tag(buf);
        let table = buf.tag_table();
        let cur_table_size = table.size() as i32;
        // Re-promote priority only when sourceview has added new syntax tags
        // since the last apply. Calling set_priority every keystroke kicks the
        // text layout into a full revalidation + Pango reshape (visible as the
        // dominant flamegraph tower).
        if enabled && cur_table_size != imp.focus_tag_table_size.get() {
            if let Some(tag) = table.lookup("focus-dim") {
                let max = (cur_table_size - 1).max(0);
                tag.set_priority(max);
            }
            imp.focus_tag_table_size.set(cur_table_size);
        }

        let cursor_offset = buf.cursor_position();
        let cursor = buf.iter_at_offset(cursor_offset);
        let total = buf.line_count();
        let cur_line = cursor.line();

        // Hot path #2: if the cursor is still inside the same paragraph as
        // the last apply and the buffer hasn't grown/shrunk in line count,
        // the dim regions are unchanged — skip the remove+apply entirely.
        if enabled {
            let (ps, pe, last_total) = imp.focus_para_cache.get();
            if last_total == total && ps <= cur_line && cur_line <= pe {
                return;
            }
        }

        let (b_start, b_end) = buf.bounds();
        buf.remove_tag_by_name("focus-dim", &b_start, &b_end);
        if !enabled {
            // Toggle was turned off — we just cleaned up. Remember so future
            // keystrokes skip the scan.
            imp.focus_dim_active.set(false);
            imp.focus_para_cache.set((-1, -1, -1));
            return;
        }
        imp.focus_dim_active.set(true);

        // GTK quirk: forward_to_line_end on an iter already at end-of-line jumps
        // to the next line's end. Use ends_line() on the start-of-line iter
        // instead — it returns true when the very first char is a paragraph
        // delimiter, i.e. the line is empty.
        let line_is_blank = |line: i32| -> bool {
            let Some(s) = buf.iter_at_line(line) else { return true };
            s.ends_line()
        };

        let mut ps_line = cur_line;
        while ps_line > 0 && !line_is_blank(ps_line - 1) {
            ps_line -= 1;
        }
        let mut pe_line = cur_line;
        while pe_line < total - 1 && !line_is_blank(pe_line + 1) {
            pe_line += 1;
        }

        let para_start = match buf.iter_at_line(ps_line) {
            Some(i) => i,
            None => return,
        };
        let mut para_end = match buf.iter_at_line(pe_line) {
            Some(i) => i,
            None => return,
        };
        para_end.forward_to_line_end();

        if b_start.offset() != para_start.offset() {
            buf.apply_tag_by_name("focus-dim", &b_start, &para_start);
        }
        if para_end.offset() != b_end.offset() {
            buf.apply_tag_by_name("focus-dim", &para_end, &b_end);
        }
        imp.focus_para_cache.set((ps_line, pe_line, total));
    }

    fn apply_typewriter_mode(&self, view: &sourceview5::View) {
        let imp = self.imp();
        if !imp.config.borrow().typewriter_mode {
            return;
        }
        // Coalesce: if a scroll is already queued, drop this one. Without this
        // fast typing accumulates an unbounded queue of idle callbacks.
        if imp.typewriter_idle.borrow().is_some() {
            return;
        }
        let view_clone = view.clone();
        let id = glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move || {
                win.imp().typewriter_idle.replace(None);
                let mark = view_clone.buffer().get_insert();
                view_clone.scroll_to_mark(&mark, 0.0, true, 0.0, 0.5);
            }
        ));
        imp.typewriter_idle.replace(Some(id));
    }

    /// Enable/disable overscroll: when typewriter is on we add half-viewport
    /// top/bottom margin to the source views so the cursor can be centered
    /// even at the start or end of the buffer.
    fn sync_typewriter_margins(&self) {
        let imp = self.imp();
        let enabled = imp.config.borrow().typewriter_mode;
        for (view_slot, scroll_slot) in [
            (&imp.edit_view, &imp.edit_scroll),
            (&imp.split_edit_view, &imp.split_edit_scroll),
        ] {
            let Some(view) = view_slot.borrow().clone() else { continue };
            let Some(scroll) = scroll_slot.borrow().clone() else { continue };
            if enabled {
                let h = scroll.height().max(200);
                let margin = (h / 2).max(120);
                view.set_top_margin(margin);
                view.set_bottom_margin(margin);
            } else {
                view.set_top_margin(16);
                view.set_bottom_margin(16);
            }
        }
    }

    /// Follow the editor's scroll in the rendered pane. Also re-sync when
    /// the rendered pane's height changes (remote images loading, table
    /// reflow), which would otherwise drift the alignment.
    fn setup_split_scroll_sync(&self) {
        let imp = self.imp();
        if let Some(edit_scroll) = imp.split_edit_scroll.borrow().as_ref() {
            edit_scroll.vadjustment().connect_value_changed(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_| win.sync_split_render_scroll()
            ));
        }
        if let Some(render_scroll) = imp.split_render_scroll.borrow().as_ref() {
            render_scroll.vadjustment().connect_upper_notify(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_| win.sync_split_render_scroll()
            ));
        }
    }

    /// In Split mode, scroll the rendered pane so it shows the same spot as
    /// the editor. The source line at the editor's reference point (with its
    /// fractional position) is mapped through the renderer's line map,
    /// interpolating between neighbouring anchors, so headings, paragraphs,
    /// code blocks, tables and images line up regardless of how tall they
    /// render.
    fn sync_split_render_scroll(&self) {
        let imp = self.imp();
        if imp.view_mode.get() != ViewMode::Split {
            return;
        }
        let Some(edit) = imp.split_edit_view.borrow().clone() else { return };
        let Some(edit_scroll) = imp.split_edit_scroll.borrow().clone() else { return };
        let Some(render) = imp.split_render_view.borrow().clone() else { return };
        let Some(scroll) = imp.split_render_scroll.borrow().clone() else { return };
        let vadj = scroll.vadjustment();
        let max = (vadj.upper() - vadj.page_size()).max(vadj.lower());

        // Reference point inside the viewport slides with scroll progress:
        // top edge at the start, centre mid-document, bottom edge at the end.
        // Aligning the preview at that point keeps both ends pinned and puts
        // the matching spot where the eye is, instead of only the top edge.
        let edit_vadj = edit_scroll.vadjustment();
        let edit_range = edit_vadj.upper() - edit_vadj.lower() - edit_vadj.page_size();
        let progress = if edit_range > 0.0 {
            ((edit_vadj.value() - edit_vadj.lower()) / edit_range).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let edit_ref = (progress * edit_vadj.page_size()) as i32;
        let render_ref = progress * vadj.page_size();

        // Reference point in editor buffer coordinates (margin-agnostic, so
        // the typewriter-mode half-viewport margins don't skew it).
        let (_, top_y) = edit.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, edit_ref);
        let src_line = if top_y <= 0 {
            0.0
        } else {
            let (iter, _) = edit.line_at_y(top_y);
            let (y, h) = edit.line_yrange(&iter);
            let frac = if h > 0 { ((top_y - y) as f64 / h as f64).clamp(0.0, 1.0) } else { 0.0 };
            iter.line() as f64 + frac
        };
        let total_lines = edit.buffer().line_count().max(1) as f64;

        let render_buf = render.buffer();
        // y of the *display* line holding the anchor: with soft breaks joined
        // into one paragraph, several source lines share a buffer line, so
        // line_yrange (whole paragraph) would collapse them onto its top.
        let y_at_offset = |off: i32| -> f64 {
            let iter = render_buf.iter_at_offset(off);
            render.iter_location(&iter).y() as f64
        };
        let (end_top, end_h) = render.line_yrange(&render_buf.end_iter());
        let end_y = (end_top + end_h) as f64;

        let map = imp.split_line_map.borrow();
        // Last anchor at or before src_line, and the one after it.
        let idx = map.partition_point(|&(l, _)| (l as f64) <= src_line);
        let target_y = if idx == 0 {
            // Before the first block (blank lines / frontmatter): interpolate
            // from the buffer top.
            match map.first() {
                Some(&(l, off)) if l > 0 => (src_line / l as f64) * y_at_offset(off),
                _ => 0.0,
            }
        } else {
            let (l0, o0) = map[idx - 1];
            let y0 = y_at_offset(o0);
            let (l1, y1) = match map.get(idx) {
                Some(&(l1, o1)) => (l1 as f64, y_at_offset(o1)),
                None => (total_lines, end_y),
            };
            let span = l1 - l0 as f64;
            let t = if span > 0.0 { ((src_line - l0 as f64) / span).clamp(0.0, 1.0) } else { 0.0 };
            y0 + t * (y1 - y0)
        };
        drop(map);

        // Buffer y → adjustment value: add the rendered view's own offset
        // between the buffer origin and its current scroll position.
        let (_, win_y) = render.buffer_to_window_coords(gtk::TextWindowType::Widget, 0, target_y as i32);
        let value = vadj.value() + win_y as f64 - render_ref;
        vadj.set_value(value.clamp(vadj.lower(), max));
    }

    /// Mobile action bar lives in the widget tree only when needed: narrow
    /// breakpoint active AND a source view holds the keyboard focus.
    /// Attaching/detaching avoids the GtkRevealer / GtkScrolledWindow
    /// width-measurement warnings that fire when an invisible bottom bar
    /// is permanently parented in AdwToolbarView.
    fn update_mobile_bar_visibility(&self) {
        let imp = self.imp();
        let narrow = imp.narrow_active.get();
        let focused = imp
            .edit_view
            .borrow()
            .as_ref()
            .map(|v| v.has_focus())
            .unwrap_or(false)
            || imp
                .split_edit_view
                .borrow()
                .as_ref()
                .map(|v| v.has_focus())
                .unwrap_or(false);
        let want = narrow && focused;
        let attached = imp.mobile_bar_attached.get();
        if want == attached {
            return;
        }
        let Some(toolbar) = imp.toolbar_view.borrow().clone() else { return };
        let Some(bar) = imp.mobile_action_bar.borrow().clone() else { return };
        if want {
            toolbar.add_bottom_bar(&bar);
            imp.mobile_bar_attached.set(true);
        } else {
            toolbar.remove(&bar);
            imp.mobile_bar_attached.set(false);
        }
    }

    /// Attach the footer ActionBar to the AdwToolbarView bottom slot when
    /// `config.show_footer` is on, detach when it isn't. Same attach/detach
    /// pattern as `update_mobile_bar_visibility` (set_visible(false) on a
    /// permanently-parented ActionBar surfaces GtkRevealer measurement
    /// warnings — don't go there).
    fn update_footer_visibility(&self) {
        let imp = self.imp();
        let want = imp.config.borrow().show_footer;
        let Some(toolbar) = imp.toolbar_view.borrow().clone() else { return };
        let Some(footer) = imp.footer_bar.borrow().clone() else { return };
        let currently_attached = footer.parent().is_some();
        if want && !currently_attached {
            toolbar.add_bottom_bar(&footer);
        } else if !want && currently_attached {
            toolbar.remove(&footer);
        }
    }

    fn setup_drop_target(&self) {
        let drop_target = gtk::DropTarget::new(gio::File::static_type(), gtk::gdk::DragAction::COPY);
        drop_target.connect_drop(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                if let Ok(file) = value.get::<gio::File>() {
                    if let Some(path) = file.path() {
                        win.open_file(&path);
                        return true;
                    }
                }
                false
            }
        ));
        self.add_controller(drop_target);
    }

    fn setup_theme_tracking(&self) {
        let style_manager = adw::StyleManager::default();
        style_manager.connect_dark_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                // Re-render when theme changes
                let imp = win.imp();
                if imp.current_file.borrow().is_some() {
                    win.refresh_view();
                }
                // Update sourceview color scheme
                let is_dark = win.is_dark_theme();
                let scheme_name = if is_dark { "Adwaita-dark" } else { "Adwaita" };
                let scheme_manager = sourceview5::StyleSchemeManager::default();
                if let Some(scheme) = scheme_manager.scheme(scheme_name) {
                    if let Some(view) = imp.edit_view.borrow().as_ref() {
                        if let Some(buf) = view.buffer().downcast_ref::<sourceview5::Buffer>() {
                            buf.set_style_scheme(Some(&scheme));
                        }
                    }
                    if let Some(view) = imp.split_edit_view.borrow().as_ref() {
                        if let Some(buf) = view.buffer().downcast_ref::<sourceview5::Buffer>() {
                            buf.set_style_scheme(Some(&scheme));
                        }
                    }
                }
            }
        ));
    }

    /// Capture keyboard shortcuts before GtkSourceView consumes them.
    fn setup_key_capture(&self) {
        let key_controller = gtk::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);

        key_controller.connect_key_pressed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifier| {
                let ctrl = modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                let shift = modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK);

                // Back / Forward keysyms (XF86Back / XF86Forward) — what many
                // mice and keyboards send for their side / browser buttons
                // instead of pointer buttons 8 / 9.
                if key == gtk::gdk::Key::Back || key == gtk::gdk::Key::Forward {
                    win.navigate_history(key == gtk::gdk::Key::Forward);
                    return glib::Propagation::Stop;
                }

                // F9 — toggle outline (no modifier required)
                if key == gtk::gdk::Key::F9 && modifier.is_empty() {
                    let split = win.imp().outline_split.borrow().clone();
                    if let Some(split) = split {
                        split.set_show_sidebar(!split.shows_sidebar());
                    }
                    return glib::Propagation::Stop;
                }

                // F10 — toggle vault sidebar (no modifier required). The
                // action-accelerator path (win.toggle-vault-sidebar) is
                // unreliable while a sourceview5::View has focus, so this
                // mirrors the F9/outline branch above instead.
                if key == gtk::gdk::Key::F10 && modifier.is_empty() {
                    let split = win.imp().vault_split.borrow().clone();
                    if let Some(split) = split {
                        split.set_show_sidebar(!split.shows_sidebar());
                    }
                    return glib::Propagation::Stop;
                }

                // Alt+Left / Alt+Right — history back / forward. Handled here
                // (not as accels) so GtkSourceView can't swallow them and the
                // action can't fire twice.
                if modifier.contains(gtk::gdk::ModifierType::ALT_MASK) && !ctrl && !shift {
                    if key == gtk::gdk::Key::Left {
                        win.navigate_history(false);
                        return glib::Propagation::Stop;
                    }
                    if key == gtk::gdk::Key::Right {
                        win.navigate_history(true);
                        return glib::Propagation::Stop;
                    }
                }

                // F11 — toggle fullscreen
                if key == gtk::gdk::Key::F11 && modifier.is_empty() {
                    win.set_fullscreened(!win.is_fullscreen());
                    return glib::Propagation::Stop;
                }

                // Escape — exit fullscreen if active
                if key == gtk::gdk::Key::Escape && modifier.is_empty() && win.is_fullscreen() {
                    win.set_fullscreened(false);
                    return glib::Propagation::Stop;
                }

                if ctrl {
                    // Map the keyval to the latin letter on the same physical key,
                    // so Ctrl+F (English) and Ctrl+а (Russian JCUKEN) both trigger
                    // the same action. GTK's accelerator system does this for
                    // installed accels, but our Capture-phase handler runs first
                    // and needs to do the same lookup explicitly.
                    let latin = match key {
                        gtk::gdk::Key::n | gtk::gdk::Key::N | gtk::gdk::Key::Cyrillic_te => Some('n'),
                        gtk::gdk::Key::o | gtk::gdk::Key::O | gtk::gdk::Key::Cyrillic_shcha => Some('o'),
                        gtk::gdk::Key::s | gtk::gdk::Key::S | gtk::gdk::Key::Cyrillic_yeru => Some('s'),
                        gtk::gdk::Key::f | gtk::gdk::Key::F | gtk::gdk::Key::Cyrillic_a => Some('f'),
                        gtk::gdk::Key::p | gtk::gdk::Key::P | gtk::gdk::Key::Cyrillic_ze => Some('p'),
                        gtk::gdk::Key::e | gtk::gdk::Key::E | gtk::gdk::Key::Cyrillic_u => Some('e'),
                        gtk::gdk::Key::y | gtk::gdk::Key::Y | gtk::gdk::Key::Cyrillic_en => Some('y'),
                        gtk::gdk::Key::q | gtk::gdk::Key::Q | gtk::gdk::Key::Cyrillic_shorti => Some('q'),
                        gtk::gdk::Key::w | gtk::gdk::Key::W | gtk::gdk::Key::Cyrillic_tse => Some('w'),
                        gtk::gdk::Key::b | gtk::gdk::Key::B | gtk::gdk::Key::Cyrillic_i => Some('b'),
                        gtk::gdk::Key::i | gtk::gdk::Key::I | gtk::gdk::Key::Cyrillic_sha => Some('i'),
                        gtk::gdk::Key::k | gtk::gdk::Key::K | gtk::gdk::Key::Cyrillic_el => Some('k'),
                        gtk::gdk::Key::h | gtk::gdk::Key::H | gtk::gdk::Key::Cyrillic_er => Some('h'),
                        gtk::gdk::Key::comma | gtk::gdk::Key::Cyrillic_be => Some(','),
                        _ => None,
                    };

                    let action = match latin {
                        Some('n') => Some("new"),
                        Some('o') if shift => Some("quick-open"),
                        Some('o') => Some("open"),
                        Some('s') => Some("save"),
                        Some('f') if shift => Some("find-replace"),
                        Some('f') => Some("search"),
                        Some('h') => Some("find-replace"),
                        Some('p') => Some("print"),
                        Some('e') if shift => Some("toggle-split"),
                        Some('e') => Some("toggle-edit"),
                        Some(',') => Some("preferences"),
                        _ => None,
                    };
                    if let Some(name) = action {
                        let _ = WidgetExt::activate_action(
                            &win,
                            &format!("win.{}", name),
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                    // Ctrl+Y → redo (in addition to Ctrl+Shift+Z)
                    if latin == Some('y') {
                        if let Some(view) = win.get_active_source_view() {
                            if view.buffer().can_redo() {
                                view.buffer().redo();
                            }
                        }
                        return glib::Propagation::Stop;
                    }
                    if latin == Some('q') {
                        win.close();
                        return glib::Propagation::Stop;
                    }
                    // Ctrl+W: close current file → on second press (welcome screen) close window
                    if latin == Some('w') {
                        if win.imp().current_file.borrow().is_some() {
                            win.close_file();
                        } else {
                            win.close();
                        }
                        return glib::Propagation::Stop;
                    }

                    // Session zoom (Ctrl+=, Ctrl++, Ctrl+-, Ctrl+0)
                    if matches!(
                        key,
                        gtk::gdk::Key::equal | gtk::gdk::Key::plus | gtk::gdk::Key::KP_Add
                    ) {
                        win.adjust_font_zoom(1.1);
                        return glib::Propagation::Stop;
                    }
                    if matches!(
                        key,
                        gtk::gdk::Key::minus | gtk::gdk::Key::KP_Subtract
                    ) {
                        win.adjust_font_zoom(1.0 / 1.1);
                        return glib::Propagation::Stop;
                    }
                    if matches!(key, gtk::gdk::Key::_0 | gtk::gdk::Key::KP_0) {
                        win.reset_font_zoom();
                        return glib::Propagation::Stop;
                    }

                    // Markdown wrap shortcuts — only meaningful in an editor
                    if let Some(sv) = win.get_active_source_view() {
                        match latin {
                            Some('b') => {
                                wrap_selection(&sv, "**", "**");
                                return glib::Propagation::Stop;
                            }
                            Some('i') => {
                                wrap_selection(&sv, "*", "*");
                                return glib::Propagation::Stop;
                            }
                            Some('k') => {
                                insert_link(&sv);
                                return glib::Propagation::Stop;
                            }
                            _ => {}
                        }
                    }
                }
                glib::Propagation::Proceed
            }
        ));
        self.add_controller(key_controller);

        // Mouse side buttons: 8 = back, 9 = forward. A legacy controller
        // rather than a GestureClick: it only observes raw presses and stops
        // just 8 / 9, without joining the gesture/sequence machinery that
        // every ordinary click on the window goes through.
        let mouse_nav = gtk::EventControllerLegacy::new();
        mouse_nav.set_propagation_phase(gtk::PropagationPhase::Capture);
        mouse_nav.connect_event(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, event| {
                if event.event_type() != gtk::gdk::EventType::ButtonPress {
                    return glib::Propagation::Proceed;
                }
                let button = event
                    .downcast_ref::<gtk::gdk::ButtonEvent>()
                    .map(|b| b.button())
                    .unwrap_or(0);
                if button == 8 || button == 9 {
                    win.navigate_history(button == 9);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        ));
        self.add_controller(mouse_nav);
    }

    fn show_welcome(&self) {
        let imp = self.imp();
        if let Some(text_view) = imp.text_view.borrow().as_ref() {
            let buffer = text_view.buffer();
            buffer.set_text("");

            let mut iter = buffer.end_iter();
            buffer.insert(&mut iter, "\n\n\n");

            let tag_table = buffer.tag_table();

            let welcome_tag = gtk::TextTag::builder()
                .name("welcome-title")
                .scale(1.8)
                .weight(700)
                .justification(gtk::Justification::Center)
                .build();
            if tag_table.lookup("welcome-title").is_none() {
                tag_table.add(&welcome_tag);
            }

            let subtitle_tag = gtk::TextTag::builder()
                .name("welcome-subtitle")
                .scale(1.1)
                .foreground("#77767b")
                .justification(gtk::Justification::Center)
                .build();
            if tag_table.lookup("welcome-subtitle").is_none() {
                tag_table.add(&subtitle_tag);
            }

            buffer.insert_with_tags_by_name(&mut iter, "Manuscript", &["welcome-title"]);
            buffer.insert(&mut iter, "\n\n");
            buffer.insert_with_tags_by_name(
                &mut iter,
                "打开 Markdown 文件开始阅读\nCtrl+O 或拖放文件",
                &["welcome-subtitle"],
            );
        }
    }

    pub fn open_file(&self, path: &std::path::Path) {
        let imp = self.imp();
        // Where we are leaving from; recorded only once the new file loaded.
        // A restore still settling belongs to the file we're leaving; left
        // running it would fire once on the new buffer with a stale offset.
        self.cancel_scroll_restore();
        let leaving = self.current_location();
        tracing::debug!(?leaving, guard = imp.nav_guard.get(), "open_file: leaving");
        // Re-clicking the open file (vault row, recents) just re-reads it —
        // keep the reader where they are.
        let same_file = leaving.as_ref().is_some_and(|l| l.path == path);

        match std::fs::read_to_string(path) {
            Ok(content) => {
                if let Some(prev) = leaving {
                    imp.scroll_positions
                        .borrow_mut()
                        .insert(prev.path.clone(), prev.pos);
                    if prev.path != path && !imp.nav_guard.get() {
                        imp.nav_history.borrow_mut().push(prev);
                        self.sync_history_ui();
                    }
                }
                // A.3: seed last_doc_words from the file we just loaded so
                // the entire document isn't attributed to "today's writing"
                // on the next tick / save diff.
                let seed_words = content.split_whitespace().count() as u32;
                imp.last_doc_words.set(seed_words);
                imp.file_content.replace(content);
                imp.current_file.replace(Some(path.to_path_buf()));
                imp.modified.set(false);

                if let Some(title) = imp.title_label.borrow().as_ref() {
                    let filename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "未命名".to_string());
                    title.set_title(&filename);
                    title.set_subtitle(&display_dir(&path));
                }

                // Add to recent files
                {
                    let mut config = imp.config.borrow_mut();
                    config.add_recent(&path.to_string_lossy());
                    config.save();
                }
                self.rebuild_recent_menu();

                self.refresh_view();
                self.update_save_button();
                self.update_stats();

                // Start file monitoring
                self.start_file_monitor(path);

                // Re-base the vault tree onto the file's folder unless the file
                // lives inside the current vault (vault clicks, wikilinks). A
                // file opened from elsewhere starts a new context, so a pinned
                // root moves with it — wikilinks then resolve next to it.
                let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                if let Some(parent) = abs.parent() {
                    let vault_cfg = imp.config.borrow().vault_root.clone();
                    let inside_pinned = vault_cfg.as_deref().is_some_and(|root| {
                        let root = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
                        abs.starts_with(&root)
                    });
                    if !inside_pinned {
                        if vault_cfg.is_some() {
                            let mut cfg = imp.config.borrow_mut();
                            cfg.vault_root = Some(parent.to_string_lossy().to_string());
                            cfg.save();
                        }
                        let changed = imp.vault_current_root.borrow().as_deref() != Some(parent);
                        if changed {
                            self.set_vault_root_ui(parent);
                        }
                    }
                }
                self.reveal_current_in_vault(path);

                // Restore this file's remembered position (top if never seen)
                // once layout has settled. Queued after refresh_view so it
                // also wins over the previous file's leftover scroll value.
                let saved = imp.scroll_positions.borrow().get(path).copied();
                tracing::debug!(path = %path.display(), ?saved, same_file, "open_file: restore position");
                if !same_file {
                    glib::idle_add_local_once(glib::clone!(
                        #[weak(rename_to = win)]
                        self,
                        move || win.apply_scroll_pos(saved)
                    ));
                }
            }
            Err(e) => {
                let toast = adw::Toast::new(&format!("Failed to open file: {}", e));
                imp.toast_overlay.add_toast(toast);
            }
        }
    }

    // -- Per-file scroll position & navigation history --

    /// The line at the top of the active view's viewport.
    fn current_pos(&self) -> Option<Pos> {
        let imp = self.imp();
        match imp.view_mode.get() {
            ViewMode::View => {
                let tv = imp.text_view.borrow().clone()?;
                let (_, y) = tv.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, 0);
                let offset = offset_at_y(tv.upcast_ref(), y);
                let line_y = tv.iter_location(&tv.buffer().iter_at_offset(offset)).y();
                Some(Pos { offset, delta: (y - line_y).max(0), source: false })
            }
            _ => {
                let tv = self.active_text_view()?;
                let (_, y) = tv.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, 0);
                // scroll_to_mark top-aligns the line on restore; a sub-line
                // remainder isn't worth fighting the editor's own scrolling.
                Some(Pos { offset: offset_at_y(tv.upcast_ref(), y), delta: 0, source: true })
            }
        }
    }

    fn current_location(&self) -> Option<Location> {
        let path = self.imp().current_file.borrow().clone()?;
        Some(Location { path, pos: self.current_pos()? })
    }

    /// Save the current file's scroll position into the per-file map.
    fn remember_scroll(&self) {
        if let Some(loc) = self.current_location() {
            self.imp().scroll_positions.borrow_mut().insert(loc.path, loc.pos);
        }
    }

    /// Scroll the active view to `pos`; `None` (or a position recorded in a
    /// different kind of buffer) means the top.
    fn apply_scroll_pos(&self, pos: Option<Pos>) {
        let imp = self.imp();
        // Drop any restore still settling from a previous navigation.
        self.cancel_scroll_restore();
        let in_view = imp.view_mode.get() == ViewMode::View;
        let (offset, delta) = pos
            .filter(|p| p.source != in_view)
            .map(|p| (p.offset, p.delta))
            .unwrap_or((0, 0));
        if in_view {
            let (Some(tv), Some(scroll)) =
                (imp.text_view.borrow().clone(), imp.view_scroll.borrow().clone())
            else { return };
            let vadj = scroll.vadjustment();
            // A freshly rendered document isn't laid out yet: `upper` is still
            // the old document's, the first layout can run at a stale width,
            // and far lines have estimated heights that GTK refines
            // incrementally (seconds on a long file with images). So a single
            // set_value lands off. Re-apply every frame until the user
            // scrolls (value moves away from what we set) or 5 s pass.
            let expected = std::rc::Rc::new(Cell::new(0.0f64));
            let apply = {
                let tv = tv.clone();
                let expected = expected.clone();
                move |vadj: &gtk::Adjustment| {
                    let it = tv.buffer().iter_at_offset(offset);
                    let y = (tv.iter_location(&it).y() + delta) as f64;
                    let max = (vadj.upper() - vadj.page_size()).max(0.0);
                    let v = value_for_buffer_y(&tv, vadj, y).clamp(0.0, max);
                    expected.set(v);
                    vadj.set_value(v);
                }
            };
            apply(&vadj);
            tracing::debug!(offset, delta, value = vadj.value(), upper = vadj.upper(), "apply_scroll_pos: view");
            if offset == 0 && delta == 0 {
                return;
            }
            // Every frame, not only on `upper` changes: GTK validates line
            // heights incrementally and the target's y keeps moving even when
            // the total height has settled.
            let tick_vadj = vadj.clone();
            let tick = tv.add_tick_callback(move |_, _| {
                let before = tick_vadj.value();
                apply(&tick_vadj);
                if (tick_vadj.value() - before).abs() > 1.0 {
                    tracing::debug!(upper = tick_vadj.upper(), value = tick_vadj.value(), "scroll restore: re-apply");
                }
                glib::ControlFlow::Continue
            });
            let value_id = vadj.connect_value_changed(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |v| {
                    // GTK clamping the value when `upper` shrinks also lands
                    // here — that's layout, not the user, so it's ignored.
                    let clamped = v.value() >= v.upper() - v.page_size() - 1.0;
                    if !clamped && (v.value() - expected.get()).abs() > 2.0 {
                        tracing::debug!(value = v.value(), expected = expected.get(), upper = v.upper(), "scroll restore: cancelled (foreign value)");
                        // Defer: disconnecting inside our own emission.
                        glib::idle_add_local_once(glib::clone!(
                            #[weak]
                            win,
                            move || win.cancel_scroll_restore()
                        ));
                    }
                }
            ));
            let timer = glib::timeout_add_local_once(
                std::time::Duration::from_secs(5),
                glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    move || {
                        // The source is finishing on its own — forget it
                        // before cancelling so it isn't removed twice.
                        let pending = win.imp().scroll_restore.borrow_mut().take();
                        if let Some(r) = pending {
                            tracing::debug!(value = r.vadj.value(), upper = r.vadj.upper(), "scroll restore: window ended");
                            r.tick.remove();
                            r.vadj.disconnect(r.value_id);
                        }
                    }
                ),
            );
            *imp.scroll_restore.borrow_mut() = Some(ScrollRestore { vadj, tick, value_id, timer });
        } else if let Some(sv) = self.active_text_view() {
            // scroll_to_mark (unlike scroll_to_iter) waits for line
            // validation, so it lands correctly on a just-loaded buffer.
            let buf = sv.buffer();
            let iter = buf.iter_at_offset(offset);
            buf.place_cursor(&iter);
            let mark = buf.create_mark(None, &iter, true);
            sv.scroll_to_mark(&mark, 0.0, true, 0.0, 0.0);
            buf.delete_mark(&mark);
        }
    }

    /// Stop re-applying a restored scroll position.
    fn cancel_scroll_restore(&self) {
        let pending = self.imp().scroll_restore.borrow_mut().take();
        if let Some(r) = pending {
            r.tick.remove();
            r.vadj.disconnect(r.value_id);
            r.timer.remove();
        }
    }

    /// Keep the `go-back` / `go-forward` actions and the header back button
    /// in sync with the history stacks.
    fn sync_history_ui(&self) {
        let imp = self.imp();
        let (can_back, can_fwd) = {
            let h = imp.nav_history.borrow();
            (h.can_back(), h.can_forward())
        };
        for (name, enabled) in [("go-back", can_back), ("go-forward", can_fwd)] {
            if let Some(a) = self
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                a.set_enabled(enabled);
            }
        }
        // Both arrows appear once there's any history (browser style); the
        // actions' enabled state greys out the one that can't move.
        for btn in [&imp.back_btn, &imp.forward_btn] {
            if let Some(btn) = btn.borrow().as_ref() {
                btn.set_visible(can_back || can_fwd);
            }
        }
    }

    /// Step back (`forward == false`) or forward through the history.
    fn navigate_history(&self, forward: bool) {
        let imp = self.imp();
        let Some(current) = self.current_location() else { return };
        let exists = |p: &std::path::Path| p.exists();
        let target = {
            let mut h = imp.nav_history.borrow_mut();
            if forward {
                h.forward(current.clone(), &exists)
            } else {
                h.back(current.clone(), &exists)
            }
        };
        self.sync_history_ui();
        tracing::debug!(forward, ?current, ?target, "navigate_history");
        let Some(target) = target else { return };

        if target.path != current.path {
            imp.nav_guard.set(true);
            self.open_file(&target.path);
            imp.nav_guard.set(false);
        }
        // Queued after open_file's own restore idle, so this position wins.
        let pos = target.pos;
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move || win.apply_scroll_pos(Some(pos))
        ));
    }

    /// Resolve a wikilink name against the vault and open it, creating the note
    /// if it does not exist yet.
    fn navigate_wikilink(&self, name: &str) {
        let imp = self.imp();
        let current = imp.current_file.borrow().clone();
        let vault_cfg = imp.config.borrow().vault_root.clone();
        let Some(root) =
            crate::vault::vault_root(vault_cfg.as_deref(), current.as_deref())
        else {
            imp.toast_overlay
                .add_toast(adw::Toast::new("请先打开文件才能使用 wiki 链接"));
            return;
        };
        let path = match crate::vault::resolve(&root, name) {
            Some(p) => p,
            None => match crate::vault::create_note(&root, name) {
                Ok(p) => p,
                Err(e) => {
                    imp.toast_overlay
                        .add_toast(adw::Toast::new(&format!("Could not create note: {e}")));
                    return;
                }
            },
        };
        self.open_file(&path);
    }

    /// Horizontal margins of the View / split-preview text views: tight when
    /// Content width is 0 (unlimited), roomy otherwise. Skipped while the
    /// narrow breakpoint is active — it owns the margins then, and its
    /// unapply handler calls this again after restoring its stale values.
    fn apply_content_margins(&self) {
        let imp = self.imp();
        if imp.narrow_active.get() {
            return;
        }
        let unlimited = imp.config.borrow().content_width == 0;
        let (view_m, split_m) = if unlimited { (16, 12) } else { (48, 24) };
        if let Some(tv) = imp.text_view.borrow().as_ref() {
            tv.set_left_margin(view_m);
            tv.set_right_margin(view_m);
        }
        if let Some(tv) = imp.split_render_view.borrow().as_ref() {
            tv.set_left_margin(split_m);
            tv.set_right_margin(split_m);
        }
    }

    /// Outline sidebar vs. adaptive breakpoints. On the first breakpoint to
    /// apply, remember whether the user had the outline shown and hide it;
    /// once NO breakpoint is active any more (checked in an idle, after
    /// libadwaita's own unapply work, and robust against medium->compact->
    /// narrow hops where one unapply is immediately followed by an apply),
    /// restore exactly that value.
    /// The one rule for the vault sidebar's visibility after a layout change
    /// (breakpoint, Split on/off): hidden in Split (no room beside two panes),
    /// never auto-opened as a narrow overlay (it would cover the text),
    /// otherwise the user's persisted docked preference.
    fn sync_vault_sidebar(&self) {
        let imp = self.imp();
        let Some(vsplit) = imp.vault_split.borrow().clone() else { return };
        let want = imp.view_mode.get() != ViewMode::Split
            && !vsplit.is_collapsed()
            && {
                let cfg = imp.config.borrow();
                crate::vault::should_show_sidebar(cfg.vault_root.as_deref(), cfg.vault_sidebar_shown)
            };
        if vsplit.shows_sidebar() != want {
            imp.sidebar_persist_suppressed.set(true);
            vsplit.set_show_sidebar(want);
            imp.sidebar_persist_suppressed.set(false);
        }
    }

    fn track_outline_for_breakpoint(&self, bp: &adw::Breakpoint) {
        bp.connect_apply(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                let imp = win.imp();
                let split = imp.outline_split.borrow().clone();
                if let Some(split) = split {
                    if imp.outline_shown_before_compact.get().is_none() {
                        imp.outline_shown_before_compact.set(Some(split.shows_sidebar()));
                    }
                    split.set_show_sidebar(false);
                }
            }
        ));
        bp.connect_unapply(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    win,
                    move || {
                        if win.current_breakpoint().is_some() {
                            return;
                        }
                        let imp = win.imp();
                        if let Some(shown) = imp.outline_shown_before_compact.take() {
                            let split = imp.outline_split.borrow().clone();
                            if let Some(split) = split {
                                split.set_show_sidebar(shown);
                            }
                        }
                    }
                ));
            }
        ));
    }

    fn rebuild_recent_menu(&self) {
        let imp = self.imp();
        let config = imp.config.borrow();

        let menu = build_main_menu(&config.recent_files);

        if let Some(btn) = imp.menu_button.borrow().as_ref() {
            btn.set_menu_model(Some(&menu));
        }
    }

    /// Cancel the inotify monitor, the portal poller and any pending debounce.
    fn stop_file_watch(&self) {
        let imp = self.imp();
        if let Some(monitor) = imp.file_monitor.borrow_mut().take() {
            monitor.cancel();
        }
        if let Some(id) = imp.disk_change_idle.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = imp.file_poll.borrow_mut().take() {
            id.remove();
        }
        imp.file_poll_sig.set(None);
    }

    fn file_sig(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
        let md = std::fs::metadata(path).ok()?;
        Some((md.modified().ok()?, md.len()))
    }

    /// Debounce bursts of events (an atomic save emits several) into one check.
    fn schedule_disk_check(&self) {
        let imp = self.imp();
        if let Some(prev) = imp.disk_change_idle.borrow_mut().take() {
            prev.remove();
        }
        let source = glib::timeout_add_local_once(
            std::time::Duration::from_millis(300),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move || {
                    win.imp().disk_change_idle.replace(None);
                    win.handle_disk_change();
                }
            ),
        );
        *imp.disk_change_idle.borrow_mut() = Some(source);
    }

    fn handle_disk_change(&self) {
        let imp = self.imp();
        // Skip self-writes (auto-save and manual save both flip
        // `self_writing` around `std::fs::write` for a few hundred ms).
        if imp.self_writing.get() {
            tracing::debug!("disk check: skipped, self write");
            return;
        }
        let Some(path) = imp.current_file.borrow().clone() else { return };
        // Mid-rename the file may briefly not exist; a later event follows.
        let Ok(disk) = std::fs::read_to_string(&path) else {
            tracing::debug!(path = %path.display(), "disk check: unreadable (mid-rename?)");
            return;
        };
        // No-op touch or our own write: nothing to reload.
        if disk == *imp.file_content.borrow() {
            tracing::debug!("disk check: content unchanged");
            return;
        }
        let in_editor = matches!(imp.view_mode.get(), ViewMode::Edit | ViewMode::Split);
        tracing::debug!(in_editor, modified = imp.modified.get(), "disk check: changed");
        // Show a Reload toast if the user is editing or has unsaved changes;
        // otherwise auto-reload silently (pure View with no unsaved work).
        if in_editor || imp.modified.get() {
            let toast = adw::Toast::builder()
                .title("文件已在磁盘上更改")
                .button_label("Reload")
                .action_name("win.reload-file")
                .timeout(6)
                .build();
            imp.toast_overlay.add_toast(toast);
        } else {
            self.reload_from_disk();
        }
    }

    fn start_file_monitor(&self, path: &std::path::Path) {
        let imp = self.imp();
        self.stop_file_watch();

        // Editors save atomically (temp file + rename), which GIO reports as
        // Created / MovedIn / Renamed rather than Changed, so watch moves too.
        let file = gio::File::for_path(path);
        match file.monitor_file(gio::FileMonitorFlags::WATCH_MOVES, None::<&gio::Cancellable>) {
            Ok(monitor) => {
                monitor.connect_changed(glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_, file, other, event| {
                        use gio::FileMonitorEvent as E;
                        tracing::debug!(?event, file = %file.parse_name(), other = ?other.map(|o| o.parse_name()), "file monitor event");
                        if matches!(
                            event,
                            E::ChangesDoneHint | E::Changed | E::Created | E::MovedIn | E::Renamed
                        ) {
                            win.schedule_disk_check();
                        }
                    }
                ));
                *imp.file_monitor.borrow_mut() = Some(monitor);
            }
            Err(e) => {
                tracing::warn!("Failed to start file monitor: {}", e);
            }
        }

        // The document-portal FUSE mount delivers no inotify events: poll.
        if is_portal_path(path) {
            imp.file_poll_sig.set(Self::file_sig(path));
            let id = glib::timeout_add_local(
                std::time::Duration::from_secs(2),
                glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    #[upgrade_or]
                    glib::ControlFlow::Break,
                    move || {
                        let imp = win.imp();
                        let Some(path) = imp.current_file.borrow().clone() else {
                            return glib::ControlFlow::Continue;
                        };
                        let sig = Self::file_sig(&path);
                        if sig.is_some() && sig != imp.file_poll_sig.get() {
                            imp.file_poll_sig.set(sig);
                            win.schedule_disk_check();
                        }
                        glib::ControlFlow::Continue
                    }
                ),
            );
            *imp.file_poll.borrow_mut() = Some(id);
        }
    }

    // -- Search --

    fn perform_search(&self) {
        let imp = self.imp();
        let query = imp
            .search_entry
            .borrow()
            .as_ref()
            .map(|e| e.text().to_string())
            .unwrap_or_default();

        // Get the active buffer based on view mode
        let buffer = self.active_text_buffer();
        let buffer = match buffer {
            Some(b) => b,
            None => return,
        };

        // Remove old search-match / search-current tags
        let start = buffer.start_iter();
        let end = buffer.end_iter();
        if buffer.tag_table().lookup("search-match").is_none() {
            let tag = gtk::TextTag::builder()
                .name("search-match")
                .background("#f5c211")
                .foreground("#000000")
                .build();
            buffer.tag_table().add(&tag);
        }
        if buffer.tag_table().lookup("search-current").is_none() {
            let tag = gtk::TextTag::builder()
                .name("search-current")
                .background("#33d17a")
                .foreground("#000000")
                .build();
            buffer.tag_table().add(&tag);
            // Bump priority to top so green wins over yellow when both apply
            let new_prio = buffer.tag_table().size() as i32 - 1;
            tag.set_priority(new_prio);
        }
        buffer.remove_tag_by_name("search-match", &start, &end);
        buffer.remove_tag_by_name("search-current", &start, &end);
        *imp.search_query.borrow_mut() = query.clone();

        // Drop any previously stored marks
        {
            let mut marks = imp.search_match_marks.borrow_mut();
            for mark in marks.drain(..) {
                buffer.delete_mark(&mark);
            }
        }

        self.clear_search_hits();
        if let Some(tv) = self.active_text_view() {
            if imp.view_mode.get() == ViewMode::View {
                renderer::clear_cell_search_tags(&tv);
            }
        }

        if query.is_empty() {
            imp.search_match_count.set(0);
            imp.search_match_index.set(-1);
            self.update_search_placeholder();
            return;
        }

        // Native GTK search — handles UTF-8 correctly, matches visible text only
        let flags = gtk::TextSearchFlags::CASE_INSENSITIVE | gtk::TextSearchFlags::VISIBLE_ONLY;
        let mut iter = buffer.start_iter();
        let mut marks: Vec<gtk::TextMark> = Vec::new();
        let mut main_offsets: Vec<i32> = Vec::new();
        while let Some((match_start, match_end)) = iter.forward_search(&query, flags, None) {
            buffer.apply_tag_by_name("search-match", &match_start, &match_end);
            main_offsets.push(match_start.offset());
            // left_gravity=true so the mark stays at the start of the match after edits
            marks.push(buffer.create_mark(None, &match_start, true));
            iter = match_end;
        }

        // View mode: also search inside rich-table cells (separate TextViews).
        let mut cell_marks: Vec<Vec<Vec<(gtk::TextView, gtk::TextMark)>>> = Vec::new();
        let mut tables: Vec<TableMatches> = Vec::new();
        if imp.view_mode.get() == ViewMode::View {
            if let Some(tv) = self.active_text_view() {
                renderer::clear_cell_search_tags(&tv);
                // Tables mount lazily near the viewport; search needs all cells.
                renderer::mount_all_tables(&tv);
                for (anchor_offset, cells) in renderer::table_cells(&tv) {
                    let mut table_marks = Vec::new();
                    let mut table_offsets = Vec::new();
                    for cell in cells {
                        let cb = cell.buffer();
                        renderer::ensure_cell_search_tags(&cb.tag_table());
                        let mut offs = Vec::new();
                        let mut cmarks = Vec::new();
                        let mut it = cb.start_iter();
                        while let Some((ms, me)) = it.forward_search(&query, flags, None) {
                            cb.apply_tag_by_name("search-match", &ms, &me);
                            offs.push(ms.offset());
                            cmarks.push((cell.clone(), cb.create_mark(None, &ms, true)));
                            it = me;
                        }
                        table_offsets.push(offs);
                        table_marks.push(cmarks);
                    }
                    tables.push(TableMatches { anchor_offset, cells: table_offsets });
                    cell_marks.push(table_marks);
                }
            }
        }

        let hits: Vec<SearchHit> = if tables.is_empty() {
            marks.iter().cloned().map(SearchHit::Main).collect()
        } else {
            merge_matches(&main_offsets, &tables)
                .into_iter()
                .map(|r| match r {
                    HitRef::Main(i) => SearchHit::Main(marks[i].clone()),
                    HitRef::Cell { table, cell, idx } => {
                        let (view, mark) = cell_marks[table][cell][idx].clone();
                        SearchHit::Cell { view: view.downgrade(), mark }
                    }
                })
                .collect()
        };

        let count = hits.len() as i32;
        *imp.search_match_marks.borrow_mut() = marks;
        *imp.search_hits.borrow_mut() = hits;
        imp.search_match_count.set(count);
        if count > 0 {
            imp.search_match_index.set(0);
            self.scroll_to_match(0);
        } else {
            imp.search_match_index.set(-1);
        }
        self.update_search_placeholder();
    }

    fn search_next(&self) {
        let imp = self.imp();
        let count = imp.search_match_count.get();
        if count <= 0 {
            return;
        }
        let idx = (imp.search_match_index.get() + 1) % count;
        imp.search_match_index.set(idx);
        self.scroll_to_match(idx);
        self.update_search_placeholder();
    }

    fn search_prev(&self) {
        let imp = self.imp();
        let count = imp.search_match_count.get();
        if count <= 0 {
            return;
        }
        let idx = imp.search_match_index.get() - 1;
        let idx = if idx < 0 { count - 1 } else { idx };
        imp.search_match_index.set(idx);
        self.scroll_to_match(idx);
        self.update_search_placeholder();
    }

    fn scroll_to_match(&self, match_index: i32) {
        let imp = self.imp();
        let hit = imp.search_hits.borrow().get(match_index as usize).cloned();
        let Some(hit) = hit else { return };
        let Some(tv) = self.active_text_view() else { return };
        let buf = tv.buffer();
        let query = imp.search_query.borrow().clone();
        let qlen = query.chars().count() as i32;

        // Apply "search-current" to this match only — clear everywhere (main
        // buffer and table cells), then re-apply.
        if !query.is_empty() {
            buf.remove_tag_by_name("search-current", &buf.start_iter(), &buf.end_iter());
            if imp.view_mode.get() == ViewMode::View {
                for (_, cells) in renderer::table_cells(&tv) {
                    for cell in cells {
                        let cb = cell.buffer();
                        if cb.tag_table().lookup("search-current").is_some() {
                            cb.remove_tag_by_name(
                                "search-current",
                                &cb.start_iter(),
                                &cb.end_iter(),
                            );
                        }
                    }
                }
            }
            match &hit {
                SearchHit::Main(mark) => {
                    let iter = buf.iter_at_mark(mark);
                    let mut end = iter.clone();
                    end.forward_chars(qlen);
                    buf.apply_tag_by_name("search-current", &iter, &end);
                }
                SearchHit::Cell { view, mark } => {
                    if let Some(cell) = view.upgrade() {
                        let cb = cell.buffer();
                        let iter = cb.iter_at_mark(mark);
                        let mut end = iter.clone();
                        end.forward_chars(qlen);
                        cb.apply_tag_by_name("search-current", &iter, &end);
                    }
                }
            }
        }

        // y of the match in the main text view's buffer coordinates.
        let y = match &hit {
            SearchHit::Main(mark) => {
                let iter = buf.iter_at_mark(mark);
                Some(tv.iter_location(&iter).y() as f64)
            }
            // The table's anchor y in the main buffer, plus the match's y
            // inside the table. A widget far off screen isn't allocated, so
            // mapping the cell straight into the TextView fails; the offset
            // within the table is 0 until the table has been laid out once.
            SearchHit::Cell { view, mark } => view.upgrade().and_then(|cell| {
                let anchor_offset = renderer::table_cells(&tv)
                    .into_iter()
                    .find(|(_, cells)| cells.contains(&cell))?
                    .0;
                let anchor_y = tv.iter_location(&buf.iter_at_offset(anchor_offset)).y() as f64;
                let rect = cell.iter_location(&cell.buffer().iter_at_mark(mark));
                let (wx, wy) = cell.buffer_to_window_coords(gtk::TextWindowType::Widget, rect.x(), rect.y());
                let inner = cell
                    .parent()
                    .and_then(|grid| cell.compute_point(&grid, &gtk::graphene::Point::new(wx as f32, wy as f32)))
                    .map_or(0.0, |p| p.y() as f64);
                Some(anchor_y + inner)
            }),
        };
        let Some(y) = y else { return };

        // View mode drives the vadjustment directly: a cell match has no mark
        // in the main buffer for scroll_to_mark to use.
        if imp.view_mode.get() == ViewMode::View {
            if let Some(scroll) = imp.view_scroll.borrow().clone() {
                let vadj = scroll.vadjustment();
                let max = (vadj.upper() - vadj.page_size()).max(0.0);
                let target = (value_for_buffer_y(&tv, &vadj, y) - vadj.page_size() * 0.2).clamp(0.0, max);
                vadj.set_value(target);
            }
        } else if let SearchHit::Main(mark) = &hit {
            tv.scroll_to_mark(mark, 0.1, true, 0.0, 0.2);
        }
    }

    fn update_search_placeholder(&self) {
        let imp = self.imp();
        let count = imp.search_match_count.get();
        let idx = imp.search_match_index.get();
        let query_empty = imp
            .search_entry
            .borrow()
            .as_ref()
            .map(|e| e.text().is_empty())
            .unwrap_or(true);

        if let Some(label) = imp.search_count_label.borrow().as_ref() {
            let text = if count > 0 {
                format!("{} / {}", idx + 1, count)
            } else if query_empty {
                String::new()
            } else {
                "0 / 0".to_string()
            };
            label.set_text(&text);
        }

        if let Some(entry) = imp.search_entry.borrow().as_ref() {
            entry.set_placeholder_text(Some("搜索…"));
            let _ = idx;
        }
    }

    fn active_text_buffer(&self) -> Option<gtk::TextBuffer> {
        let imp = self.imp();
        match imp.view_mode.get() {
            ViewMode::View => imp
                .text_view
                .borrow()
                .as_ref()
                .map(|tv| tv.buffer()),
            ViewMode::Edit => imp
                .edit_view
                .borrow()
                .as_ref()
                .map(|sv| sv.buffer().upcast()),
            ViewMode::Split => imp
                .split_edit_view
                .borrow()
                .as_ref()
                .map(|sv| sv.buffer().upcast()),
        }
    }

    /// Persist the current session (open file, view mode, scroll) to config.
    /// Called when the window is closing.
    fn save_session(&self) {
        let imp = self.imp();
        let last_file = imp
            .current_file
            .borrow()
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let last_view_mode = match imp.view_mode.get() {
            ViewMode::View => "view",
            ViewMode::Edit => "edit",
            ViewMode::Split => "split",
        };
        let last_scroll_line = self
            .active_text_view()
            .and_then(|tv| {
                let (_, y) = tv.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, 0);
                tv.iter_at_location(0, y).map(|it| it.line())
            })
            .unwrap_or(0);

        let mut cfg = imp.config.borrow_mut();
        cfg.last_file = last_file;
        cfg.last_view_mode = Some(last_view_mode.to_string());
        cfg.last_scroll_line = last_scroll_line;
        cfg.save();
    }

    /// Reopen the file/mode/scroll position recorded last time.
    /// Returns `true` if a session was restored, `false` otherwise.
    pub fn try_restore_session(&self) -> bool {
        let imp = self.imp();
        let cfg = imp.config.borrow().clone();
        if !cfg.restore_session {
            return false;
        }
        let Some(last_file) = cfg.last_file.as_ref() else {
            return false;
        };
        let path = std::path::PathBuf::from(last_file);
        if !path.exists() {
            return false;
        }
        self.open_file(&path);
        let mode = match cfg.last_view_mode.as_deref() {
            Some("edit") => ViewMode::Edit,
            Some("split") => ViewMode::Split,
            _ => ViewMode::View,
        };
        self.set_view_mode(mode);
        // Scroll once after the layout settles
        let line = cfg.last_scroll_line;
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move || {
                if let Some(tv) = win.active_text_view() {
                    if let Some(iter) = tv.buffer().iter_at_line(line) {
                        tv.scroll_to_iter(&mut iter.clone(), 0.0, true, 0.0, 0.0);
                    }
                }
            }
        ));
        true
    }

    fn active_text_view(&self) -> Option<gtk::TextView> {
        let imp = self.imp();
        match imp.view_mode.get() {
            ViewMode::View => imp.text_view.borrow().clone(),
            ViewMode::Edit => imp
                .edit_view
                .borrow()
                .as_ref()
                .map(|sv| sv.clone().upcast()),
            ViewMode::Split => imp
                .split_edit_view
                .borrow()
                .as_ref()
                .map(|sv| sv.clone().upcast()),
        }
    }

    // -- Print --

    fn print_file(&self) {
        let imp = self.imp();
        let content = imp.file_content.borrow().clone();
        if content.is_empty() {
            return;
        }
        let title = imp
            .current_file
            .borrow()
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
        let opts = self.export_options();
        crate::print::print_markdown(self, &content, title.as_deref(), &opts);
    }

    fn show_quick_open(&self) {
        let imp = self.imp();
        let recents = imp.config.borrow().recent_files.clone();
        let current = imp.current_file.borrow().clone();
        crate::quick_open::present(
            self,
            &recents,
            current.as_deref(),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |path| {
                    win.open_file(&path);
                }
            ),
        );
    }

    fn export_options(&self) -> crate::print::ExportOptions {
        let cfg = self.imp().config.borrow();
        crate::print::ExportOptions {
            font: cfg.export_font.clone(),
            margin_top_mm: cfg.export_margin_top_mm,
            margin_right_mm: cfg.export_margin_right_mm,
            margin_bottom_mm: cfg.export_margin_bottom_mm,
            margin_left_mm: cfg.export_margin_left_mm,
            keep_line_breaks: cfg.keep_line_breaks,
        }
    }

    fn export_pdf_file(&self) {
        let imp = self.imp();
        let content = imp.file_content.borrow().clone();
        if content.is_empty() {
            return;
        }
        let suggested = imp
            .current_file
            .borrow()
            .as_ref()
            .and_then(|p| p.file_stem().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "document".to_string());

        let pdf_filter = gtk::FileFilter::new();
        pdf_filter.set_name(Some("PDF documents"));
        pdf_filter.add_pattern("*.pdf");

        let dialog = make_file_chooser(self, "导出 PDF", gtk::FileChooserAction::Save, "保存");
        dialog.set_current_name(&format!("{suggested}.pdf"));
        dialog.add_filter(&pdf_filter);

        let title_for_op = imp
            .current_file
            .borrow()
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));

        dialog.connect_response(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[strong]
            content,
            #[strong]
            title_for_op,
            move |dialog, response| {
                if response != gtk::ResponseType::Accept {
                    return;
                }
                let Some(file) = dialog.file() else { return };
                let Some(mut path) = file.path() else { return };
                if path.extension().map(|e| e != "pdf").unwrap_or(true) {
                    path.set_extension("pdf");
                }
                let opts = win.export_options();
                crate::print::export_pdf(&win, &content, title_for_op.as_deref(), &path, &opts);
            }
        ));
        dialog.show();
    }

    fn refresh_view(&self) {
        let imp = self.imp();
        let content = imp.file_content.borrow().clone();
        let base_dir = imp
            .current_file
            .borrow()
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let is_dark = self.is_dark_theme();

        match imp.view_mode.get() {
            ViewMode::View => {
                if let Some(text_view) = imp.text_view.borrow().as_ref() {
                    let h = render_hash(&content, is_dark);
                    if imp.last_view_render_hash.get() != h {
                        let t = std::time::Instant::now();
                        renderer::render_markdown(text_view, &content, &base_dir, is_dark, imp.render_options());
                        tracing::debug!(ms = t.elapsed().as_secs_f64() * 1000.0, bytes = content.len(), "render: view");
                        imp.last_view_render_hash.set(h);
                    }
                }
            }
            ViewMode::Edit => {
                if let Some(edit_view) = imp.edit_view.borrow().as_ref() {
                    self.set_editor_text(edit_view, &content);
                }
            }
            ViewMode::Split => {
                if let Some(edit_view) = imp.split_edit_view.borrow().as_ref() {
                    self.set_editor_text(edit_view, &content);
                }
                self.refresh_split_render();
            }
        }
        self.rebuild_outline();
    }

    fn refresh_split_render(&self) {
        let imp = self.imp();
        let content = imp.file_content.borrow().clone();
        let base_dir = imp
            .current_file
            .borrow()
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let is_dark = self.is_dark_theme();

        let h = render_hash(&content, is_dark);
        let render_needed = imp.last_split_render_hash.get() != h;
        if render_needed {
            if let Some(render_view) = imp.split_render_view.borrow().as_ref() {
                let t = std::time::Instant::now();
                let map = renderer::render_markdown(render_view, &content, &base_dir, is_dark, imp.render_options());
                tracing::debug!(ms = t.elapsed().as_secs_f64() * 1000.0, bytes = content.len(), "render: split preview");
                *imp.split_line_map.borrow_mut() = map;
                imp.last_split_render_hash.set(h);
            }
        }

        if !render_needed {
            return;
        }

        // Re-align with the editor once layout has sized the new content;
        // later layout growth (images, tables) is caught by the upper-notify
        // hook in setup_split_scroll_sync.
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move || win.sync_split_render_scroll()
        ));
    }

    fn set_view_mode(&self, mode: ViewMode) {
        let imp = self.imp();
        let old_mode = imp.view_mode.get();
        if old_mode == mode {
            return;
        }

        // Sync content from editor before switching
        if old_mode == ViewMode::Edit {
            if let Some(edit_view) = imp.edit_view.borrow().as_ref() {
                let buf = edit_view.buffer();
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
                imp.file_content.replace(text);
            }
        } else if old_mode == ViewMode::Split {
            if let Some(edit_view) = imp.split_edit_view.borrow().as_ref() {
                let buf = edit_view.buffer();
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
                imp.file_content.replace(text);
            }
        }

        imp.view_mode.set(mode);

        // Update toggle buttons (without re-triggering signals by checking current state)
        match mode {
            ViewMode::View => {
                if let Some(btn) = imp.view_toggle.borrow().as_ref() {
                    if !btn.is_active() { btn.set_active(true); }
                }
            }
            ViewMode::Edit => {
                if let Some(btn) = imp.edit_toggle.borrow().as_ref() {
                    if !btn.is_active() { btn.set_active(true); }
                }
            }
            ViewMode::Split => {
                if let Some(btn) = imp.split_toggle.borrow().as_ref() {
                    if !btn.is_active() { btn.set_active(true); }
                }
            }
        }

        if let Some(stack) = imp.content_stack.borrow().as_ref() {
            match mode {
                ViewMode::View => stack.set_visible_child_name("view"),
                ViewMode::Edit => stack.set_visible_child_name("edit"),
                ViewMode::Split => {
                    stack.set_visible_child_name("split");
                    // Centre the divider once the paned has an allocation.
                    // A hidden stack child isn't laid out (hhomogeneous off),
                    // so its width is still 0 in an idle right after the
                    // switch — centring there pinned the divider to the left
                    // pane's minimum. Wait for the first frame with a width.
                    if let Some(paned) = imp.split_paned.borrow().as_ref() {
                        paned.add_tick_callback(|paned, _| {
                            if paned.width() <= 0 {
                                return glib::ControlFlow::Continue;
                            }
                            paned.set_position(paned.width() / 2);
                            glib::ControlFlow::Break
                        });
                    }
                }
            }
        }

        // Entering / leaving Split changes whether the vault fits.
        if mode == ViewMode::Split || old_mode == ViewMode::Split {
            self.sync_vault_sidebar();
        }

        self.refresh_view();
        self.update_save_button();
        self.update_replace_visibility();
        self.update_stats();
    }

    fn update_replace_visibility(&self) {
        let imp = self.imp();
        // Hide replace row when switching to View mode (replace cannot edit the
        // rendered buffer). When entering Edit/Split, keep it hidden until the
        // user explicitly requests Find & Replace via Ctrl+H.
        if !matches!(imp.view_mode.get(), ViewMode::Edit | ViewMode::Split) {
            if let Some(row) = imp.search_replace_row.borrow().as_ref() {
                row.set_visible(false);
            }
        }
    }

    /// Forget the merged hit list, deleting the marks it created in table
    /// cell buffers (main-buffer marks are owned by `search_match_marks`).
    /// Search re-runs on every keystroke, so leftover cell marks piled up.
    fn clear_search_hits(&self) {
        let hits = std::mem::take(&mut *self.imp().search_hits.borrow_mut());
        for hit in hits {
            if let SearchHit::Cell { view, mark } = hit {
                if !mark.is_deleted() {
                    if let Some(buf) = mark.buffer().or_else(|| view.upgrade().map(|v| v.buffer())) {
                        buf.delete_mark(&mark);
                    }
                }
            }
        }
    }

    fn clear_search_highlight(&self) {
        let imp = self.imp();
        if let Some(buffer) = self.active_text_buffer() {
            let s = buffer.start_iter();
            let e = buffer.end_iter();
            if buffer.tag_table().lookup("search-match").is_some() {
                buffer.remove_tag_by_name("search-match", &s, &e);
            }
            if buffer.tag_table().lookup("search-current").is_some() {
                buffer.remove_tag_by_name("search-current", &s, &e);
            }
        }
        {
            let mut marks = imp.search_match_marks.borrow_mut();
            if let Some(buffer) = self.active_text_buffer() {
                for m in marks.drain(..) {
                    buffer.delete_mark(&m);
                }
            } else {
                marks.clear();
            }
        }
        if let Some(tv) = imp.text_view.borrow().as_ref() {
            renderer::clear_cell_search_tags(tv);
        }
        self.clear_search_hits();
        imp.search_match_count.set(0);
        imp.search_match_index.set(-1);
        imp.search_query.borrow_mut().clear();
        if let Some(label) = imp.search_count_label.borrow().as_ref() {
            label.set_text("");
        }
    }

    fn open_search_bar(&self, with_replace: bool) {
        let imp = self.imp();
        if let Some(row) = imp.search_replace_row.borrow().as_ref() {
            row.set_visible(with_replace);
        }
        if let Some(bar) = imp.search_bar.borrow().clone() {
            if !bar.is_search_mode() {
                bar.set_search_mode(true);
            }
        }
        if let Some(entry) = imp.search_entry.borrow().as_ref() {
            entry.grab_focus();
        }
    }

    fn replace_current(&self) {
        let imp = self.imp();
        if !matches!(imp.view_mode.get(), ViewMode::Edit | ViewMode::Split) {
            return;
        }
        let idx = imp.search_match_index.get();
        if idx < 0 {
            return;
        }
        let query = imp
            .search_entry
            .borrow()
            .as_ref()
            .map(|e| e.text().to_string())
            .unwrap_or_default();
        if query.is_empty() {
            return;
        }
        let replacement = imp
            .search_replace_entry
            .borrow()
            .as_ref()
            .map(|e| e.text().to_string())
            .unwrap_or_default();
        let Some(buffer) = self.active_text_buffer() else {
            return;
        };
        let mark = {
            let marks = imp.search_match_marks.borrow();
            match marks.get(idx as usize) {
                Some(m) => m.clone(),
                None => return,
            }
        };
        let start = buffer.iter_at_mark(&mark);
        let mut end = start;
        end.forward_chars(query.chars().count() as i32);
        buffer.begin_user_action();
        buffer.delete(&mut start.clone(), &mut end);
        let mut insert_iter = buffer.iter_at_mark(&mark);
        let insert_offset = insert_iter.offset();
        buffer.insert(&mut insert_iter, &replacement);
        buffer.end_user_action();
        let replacement_chars = replacement.chars().count() as i32;
        let highlight_end_offset = insert_offset + replacement_chars;
        self.flash_replace_highlight(&buffer, insert_offset, highlight_end_offset);
        self.perform_search();
    }

    fn replace_all(&self) {
        let imp = self.imp();
        if !matches!(imp.view_mode.get(), ViewMode::Edit | ViewMode::Split) {
            return;
        }
        let query = imp
            .search_entry
            .borrow()
            .as_ref()
            .map(|e| e.text().to_string())
            .unwrap_or_default();
        if query.is_empty() {
            return;
        }
        let replacement = imp
            .search_replace_entry
            .borrow()
            .as_ref()
            .map(|e| e.text().to_string())
            .unwrap_or_default();
        let Some(buffer) = self.active_text_buffer() else {
            return;
        };

        let flags = gtk::TextSearchFlags::CASE_INSENSITIVE | gtk::TextSearchFlags::VISIBLE_ONLY;
        let replacement_len = replacement.chars().count() as i32;
        buffer.begin_user_action();
        let mut iter = buffer.start_iter();
        let mut count = 0u32;
        let mut replaced_ranges: Vec<(i32, i32)> = Vec::new();
        while let Some((match_start, match_end)) = iter.forward_search(&query, flags, None) {
            let start_offset = match_start.offset();
            buffer.delete(&mut match_start.clone(), &mut match_end.clone());
            let mut insert_iter = buffer.iter_at_offset(start_offset);
            buffer.insert(&mut insert_iter, &replacement);
            replaced_ranges.push((start_offset, start_offset + replacement_len));
            iter = buffer.iter_at_offset(start_offset + replacement_len);
            count += 1;
        }
        buffer.end_user_action();
        for (s, e) in replaced_ranges {
            self.flash_replace_highlight(&buffer, s, e);
        }
        self.perform_search();

        let msg = if count == 0 {
            "没有匹配项被替换".to_string()
        } else if count == 1 {
            "Replaced 1 match".to_string()
        } else {
            format!("Replaced {count} matches")
        };
        imp.toast_overlay.add_toast(adw::Toast::new(&msg));
    }

    /// Briefly tint the replaced range in the buffer with a green-ish background
    /// so the user can see what just changed. Tag is removed after 1.2s.
    fn flash_replace_highlight(&self, buffer: &gtk::TextBuffer, start_offset: i32, end_offset: i32) {
        if buffer.tag_table().lookup("replace-flash").is_none() {
            let is_dark = self.is_dark_theme();
            let bg = if is_dark { "#2c5a3e" } else { "#b9e3c3" };
            let tag = gtk::TextTag::builder()
                .name("replace-flash")
                .background(bg)
                .build();
            buffer.tag_table().add(&tag);
        }
        let start = buffer.iter_at_offset(start_offset);
        let end = buffer.iter_at_offset(end_offset);
        buffer.apply_tag_by_name("replace-flash", &start, &end);

        // Pin the range with marks so concurrent edits don't shift our removal.
        let start_mark = buffer.create_mark(None, &start, true);
        let end_mark = buffer.create_mark(None, &end, false);
        let buf = buffer.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
            let s = buf.iter_at_mark(&start_mark);
            let e = buf.iter_at_mark(&end_mark);
            buf.remove_tag_by_name("replace-flash", &s, &e);
            buf.delete_mark(&start_mark);
            buf.delete_mark(&end_mark);
        });
    }

    fn update_save_button(&self) {
        let imp = self.imp();
        if let Some(save_btn) = imp.save_btn.borrow().as_ref() {
            let editing = matches!(imp.view_mode.get(), ViewMode::Edit | ViewMode::Split);
            save_btn.set_visible(editing);
            save_btn.set_sensitive(imp.modified.get());
        }
    }

    /// Attach spell adapters to both editors if they don't have one yet.
    fn ensure_spell_adapters(&self, config: &AppConfig) {
        let imp = self.imp();
        for (view_slot, adapter_slot) in [
            (&imp.edit_view, &imp.edit_spell_adapter),
            (&imp.split_edit_view, &imp.split_edit_spell_adapter),
        ] {
            if adapter_slot.borrow().is_some() {
                continue;
            }
            let Some(view) = view_slot.borrow().clone() else { continue };
            if let Some(adapter) = attach_spell_adapter(&view, config) {
                *adapter_slot.borrow_mut() = Some(adapter);
            }
        }
    }

    /// Replace an editor buffer's text without it counting as a user edit —
    /// loading, reloading and mode switches all go through here. Otherwise
    /// the buffer's `changed` handler marks the file modified (• in the
    /// title, Save shown) and, with auto-save on, rewrites it to disk.
    fn set_editor_text(&self, view: &impl IsA<gtk::TextView>, text: &str) {
        let imp = self.imp();
        imp.programmatic_edit.set(true);
        view.as_ref().buffer().set_text(text);
        imp.programmatic_edit.set(false);
    }

    fn mark_modified(&self) {
        let imp = self.imp();
        if !imp.modified.get() {
            imp.modified.set(true);
            if let Some(title) = imp.title_label.borrow().as_ref() {
                let current = title.title().to_string();
                if !current.starts_with('•') {
                    title.set_title(&format!("• {}", current));
                }
            }
            self.update_save_button();
        }
        self.schedule_auto_save();
    }

    fn schedule_auto_save(&self) {
        let imp = self.imp();
        if !imp.config.borrow().auto_save {
            return;
        }
        if imp.current_file.borrow().is_none() {
            return;
        }
        // Debounce ~1.5s: cancel any pending save and queue a new one.
        if let Some(prev) = imp.auto_save_idle.borrow_mut().take() {
            prev.remove();
        }
        let source = glib::timeout_add_local_once(
            std::time::Duration::from_millis(1500),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move || {
                    win.imp().auto_save_idle.replace(None);
                    if win.imp().modified.get() && win.imp().current_file.borrow().is_some() {
                        win.save_file_silent();
                    }
                }
            ),
        );
        *imp.auto_save_idle.borrow_mut() = Some(source);
    }

    /// Same as `save_file` but skips the "File saved" toast. Used by auto-save
    /// so frequent writes don't spam the overlay.
    fn save_file_silent(&self) {
        let imp = self.imp();
        let content = match imp.view_mode.get() {
            ViewMode::Edit => imp
                .edit_view
                .borrow()
                .as_ref()
                .map(|sv| {
                    let b = sv.buffer();
                    b.text(&b.start_iter(), &b.end_iter(), false).to_string()
                }),
            ViewMode::Split => imp
                .split_edit_view
                .borrow()
                .as_ref()
                .map(|sv| {
                    let b = sv.buffer();
                    b.text(&b.start_iter(), &b.end_iter(), false).to_string()
                }),
            _ => None,
        };
        let Some(content) = content else { return };
        let Some(path) = imp.current_file.borrow().clone() else { return };
        self.write_file_internal(&path, content, true);
    }

    /// Start a new untitled document in Edit mode.
    pub fn new_file(&self) {
        let imp = self.imp();
        self.remember_scroll();
        self.stop_file_watch();
        imp.current_file.replace(None);
        imp.file_content.replace(String::new());
        imp.modified.set(false);
        // A.3: untitled doc starts at zero words.
        imp.last_doc_words.set(0);
        if let Some(sv) = imp.edit_view.borrow().as_ref() {
            self.set_editor_text(sv, "");
        }
        if let Some(sv) = imp.split_edit_view.borrow().as_ref() {
            self.set_editor_text(sv, "");
        }
        if let Some(title) = imp.title_label.borrow().as_ref() {
            title.set_title("未命名");
            title.set_subtitle("");
        }
        self.set_view_mode(ViewMode::Edit);
        if let Some(sv) = imp.edit_view.borrow().as_ref() {
            sv.grab_focus();
        }
        self.update_save_button();
        self.update_stats();
    }

    fn save_file(&self) {
        let imp = self.imp();

        let content = match imp.view_mode.get() {
            ViewMode::Edit => {
                if let Some(edit_view) = imp.edit_view.borrow().as_ref() {
                    let buf = edit_view.buffer();
                    buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string()
                } else {
                    return;
                }
            }
            ViewMode::Split => {
                if let Some(edit_view) = imp.split_edit_view.borrow().as_ref() {
                    let buf = edit_view.buffer();
                    buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string()
                } else {
                    return;
                }
            }
            ViewMode::View => return,
        };

        let path = imp.current_file.borrow().clone();
        match path {
            Some(path) => self.write_file(&path, content),
            None => self.show_save_as_dialog(content),
        }
    }

    fn write_file(&self, path: &std::path::Path, content: String) {
        self.write_file_internal(path, content, false);
    }

    fn write_file_internal(&self, path: &std::path::Path, content: String, silent: bool) {
        let imp = self.imp();
        imp.self_writing.set(true);
        let result = std::fs::write(path, &content);
        // Clear the suppression flag a short while after — long enough for the
        // FileMonitor ChangesDoneHint event to arrive on the main loop.
        glib::timeout_add_local_once(
            std::time::Duration::from_millis(500),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move || {
                    win.imp().self_writing.set(false);
                }
            ),
        );
        match result {
            Ok(_) => {
                imp.file_content.replace(content);
                imp.modified.set(false);
                // Keep the portal poller from treating our own write as external.
                if imp.file_poll.borrow().is_some() {
                    imp.file_poll_sig.set(Self::file_sig(path));
                }

                if let Some(title) = imp.title_label.borrow().as_ref() {
                    let current = title.title().to_string();
                    if let Some(stripped) = current.strip_prefix("• ") {
                        title.set_title(stripped);
                    }
                }
                self.update_save_button();

                if !silent {
                    let toast = adw::Toast::new("文件已保存");
                    toast.set_timeout(1);
                    imp.toast_overlay.add_toast(toast);
                }

                // A.3: commit positive word delta into today_words.
                self.track_today_words();
            }
            Err(e) => {
                let toast = adw::Toast::new(&format!("Failed to save: {}", e));
                imp.toast_overlay.add_toast(toast);
            }
        }
    }

    fn show_save_as_dialog(&self, content: String) {
        let dialog = make_file_chooser(self, "保存 Markdown 文件", gtk::FileChooserAction::Save, "保存");
        dialog.set_current_name("untitled.md");

        let content_cell: std::rc::Rc<std::cell::RefCell<Option<String>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Some(content)));
        dialog.connect_response(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[strong]
            content_cell,
            move |dialog, response| {
                if response != gtk::ResponseType::Accept {
                    return;
                }
                let Some(file) = dialog.file() else { return };
                let Some(path) = file.path() else { return };
                let Some(content) = content_cell.borrow_mut().take() else {
                    return;
                };
                win.write_file(&path, content);
                let imp = win.imp();
                imp.current_file.replace(Some(path.clone()));
                if let Some(title) = imp.title_label.borrow().as_ref() {
                    let filename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "未命名".to_string());
                    title.set_title(&filename);
                    title.set_subtitle(&display_dir(&path));
                }
                {
                    let mut config = imp.config.borrow_mut();
                    config.add_recent(&path.to_string_lossy());
                    config.save();
                }
                win.rebuild_recent_menu();
                win.start_file_monitor(&path);
            }
        ));
        dialog.show();
    }

    fn show_open_dialog(&self) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Markdown files"));
        filter.add_pattern("*.md");
        filter.add_pattern("*.markdown");
        filter.add_pattern("*.mkd");
        filter.add_mime_type("text/markdown");

        let all_filter = gtk::FileFilter::new();
        all_filter.set_name(Some("All files"));
        all_filter.add_pattern("*");

        let dialog = make_file_chooser(self, "打开 Markdown 文件", gtk::FileChooserAction::Open, "打开");
        dialog.add_filter(&filter);
        dialog.add_filter(&all_filter);

        dialog.connect_response(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |dialog, response| {
                if response == gtk::ResponseType::Accept {
                    if let Some(file) = dialog.file() {
                        if let Some(path) = file.path() {
                            win.open_file(&path);
                        }
                    }
                }
            }
        ));

        dialog.show();
    }

    fn apply_config(&self, config: AppConfig) {
        let imp = self.imp();
        imp.config.replace(config.clone());
        // A.3: date roll-over on startup / config swap. Cheap when same-day.
        self.roll_over_today_words_if_needed();
        let zoom = imp.font_zoom.get();

        // Apply to render views
        if let Some(tv) = imp.text_view.borrow().as_ref() {
            tv.set_wrap_mode(config.gtk_wrap_mode());
            tv.set_pixels_above_lines(config.line_spacing as i32 / 2);
            tv.set_pixels_below_lines(config.line_spacing as i32 / 2);
        }
        apply_font_css_cached(&imp.view_css_provider, &imp.last_view_css, "manuscript-view", &config, zoom);

        if let Some(clamp) = imp.view_clamp.borrow().as_ref() {
            // 0 in preferences means "unlimited" — large finite value (see const).
            let max = if config.content_width == 0 {
                UNLIMITED_CONTENT_WIDTH
            } else {
                config.content_width as i32
            };
            clamp.set_maximum_size(max);
        }
        self.apply_content_margins();

        if let Some(tv) = imp.split_render_view.borrow().as_ref() {
            tv.set_wrap_mode(config.gtk_wrap_mode());
            tv.set_pixels_above_lines(config.line_spacing as i32 / 2);
            tv.set_pixels_below_lines(config.line_spacing as i32 / 2);
        }
        apply_font_css_cached(&imp.split_css_provider, &imp.last_split_css, "manuscript-view", &config, zoom);

        // Apply to edit views
        if let Some(sv) = imp.edit_view.borrow().as_ref() {
            sv.set_wrap_mode(config.gtk_wrap_mode());
            sv.set_show_line_numbers(config.show_line_numbers);
        }
        apply_font_css_cached(&imp.edit_css_provider, &imp.last_edit_css, "manuscript-edit", &config, zoom);

        if let Some(sv) = imp.split_edit_view.borrow().as_ref() {
            sv.set_wrap_mode(config.gtk_wrap_mode());
            sv.set_show_line_numbers(config.show_line_numbers);
        }
        apply_font_css_cached(&imp.split_edit_css_provider, &imp.last_split_edit_css, "manuscript-edit", &config, zoom);

        // Spell check: create adapters on first enable, then flip the enabled
        // flag + swap language on existing ones.
        if config.spell_check {
            self.ensure_spell_adapters(&config);
        }
        for adapter_cell in [&imp.edit_spell_adapter, &imp.split_edit_spell_adapter] {
            if let Some(adapter) = adapter_cell.borrow().as_ref() {
                adapter.set_enabled(config.spell_check);
                if !config.spell_check_language.is_empty() {
                    if let Some(checker) = adapter.checker() {
                        let current = checker.language().map(|g| g.to_string()).unwrap_or_default();
                        if current != config.spell_check_language {
                            checker.set_language(&config.spell_check_language);
                        }
                    }
                }
            }
        }

        // Re-apply distraction-free state — toggle may have changed
        self.apply_distraction_free();

        // Re-apply focus + typewriter — toggle may have changed
        self.sync_typewriter_margins();
        for slot in [&imp.edit_view, &imp.split_edit_view] {
            if let Some(view) = slot.borrow().clone() {
                self.apply_focus_mode(&view.buffer());
                self.apply_typewriter_mode(&view);
            }
        }

        // Re-render if viewing. Render-hash invalidated unconditionally — most
        // preference changes (font, line spacing, rich tables) do not move the
        // text content or the dark flag, so the cached hash would match and the
        // walk would be skipped.
        if imp.current_file.borrow().is_some() {
            imp.last_view_render_hash.set(0);
            imp.last_split_render_hash.set(0);
            self.refresh_view();
        }

        // Footer (A.2): attach/detach + repaint stats destination.
        self.update_footer_visibility();
        self.update_stats();

        // A.4: keep the Writing statistics action enabled state in sync.
        if let Some(action) = self.lookup_action("writing-stats") {
            if let Some(simple) = action.downcast_ref::<gio::SimpleAction>() {
                let cfg = imp.config.borrow();
                simple.set_enabled(cfg.show_footer && cfg.writing_stats);
            }
        }
    }

    fn schedule_stats_update(&self) {
        let imp = self.imp();
        if let Some(id) = imp.stats_debounce.borrow_mut().take() {
            id.remove();
        }
        let id = glib::timeout_add_local_once(
            std::time::Duration::from_millis(500),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move || {
                    win.imp().stats_debounce.replace(None);
                    win.update_stats();
                }
            ),
        );
        imp.stats_debounce.replace(Some(id));
    }

    fn update_stats(&self) {
        let imp = self.imp();
        let header_label = match imp.stats_label.borrow().clone() {
            Some(l) => l,
            None => return,
        };
        let footer_words_opt = imp.footer_words_label.borrow().clone();
        let footer_time_opt = imp.footer_time_label.borrow().clone();
        let footer_goal_opt = imp.footer_goal_label.borrow().clone();
        let footer_goal_box_opt = imp.footer_goal_box.borrow().clone();
        let footer_goal_icon_opt = imp.footer_goal_icon.borrow().clone();
        let show_footer = imp.config.borrow().show_footer;

        let text = match imp.view_mode.get() {
            ViewMode::Edit => imp
                .edit_view
                .borrow()
                .as_ref()
                .map(|sv| {
                    let buf = sv.buffer();
                    buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string()
                })
                .unwrap_or_default(),
            ViewMode::Split | ViewMode::View => imp.file_content.borrow().clone(),
        };

        let words = text.split_whitespace().count();
        if words == 0 || imp.current_file.borrow().is_none() {
            header_label.set_label("");
            header_label.set_visible(false);
            if let Some(l) = &footer_words_opt { l.set_label(""); }
            if let Some(l) = &footer_time_opt { l.set_label(""); }
            if let Some(l) = &footer_goal_opt { l.set_label(""); }
            if let Some(b) = &footer_goal_box_opt { b.set_visible(false); }
            return;
        }
        let mins = words.div_ceil(200).max(1);
        let word_word = "字";
        let words_text = format!("{} {}", words, word_word);
        let time_text = format!("约 {} 分钟", mins);

        if show_footer {
            // Footer takes ownership of the readout; header clears.
            header_label.set_label("");
            header_label.set_visible(false);
            if let Some(l) = &footer_words_opt { l.set_label(&words_text); }
            if let Some(l) = &footer_time_opt { l.set_label(&time_text); }

            // Goal segment is visible only when writing-stats is on.
            // Icon flips from outline star to filled star when today >= goal.
            let (goal_visible, goal_text, goal_hit) = {
                let cfg = imp.config.borrow();
                if cfg.writing_stats {
                    let today = cfg.today_words;
                    let goal = cfg.daily_word_goal;
                    let t = if goal > 0 {
                        format!("{} / {}", today, goal)
                    } else {
                        format!("今日 {}", today)
                    };
                    let hit = goal > 0 && today >= goal;
                    (true, t, hit)
                } else {
                    (false, String::new(), false)
                }
            };
            if let Some(l) = &footer_goal_opt { l.set_label(&goal_text); }
            if let Some(b) = &footer_goal_box_opt { b.set_visible(goal_visible); }
            if let Some(icon) = &footer_goal_icon_opt {
                icon.set_icon_name(Some(
                    if goal_hit { "starred-symbolic" } else { "non-starred-symbolic" }
                ));
            }
        } else {
            // Header keeps the pre-A.2 behaviour (single compact label).
            let stats_text = format!("{} · {}", words_text, time_text);
            header_label.set_label(&stats_text);
            header_label.set_visible(true);
            if let Some(l) = &footer_words_opt { l.set_label(""); }
            if let Some(l) = &footer_time_opt { l.set_label(""); }
            if let Some(l) = &footer_goal_opt { l.set_label(""); }
            if let Some(b) = &footer_goal_box_opt { b.set_visible(false); }
        }
    }

    /// A.4 — Present a small `adw::Dialog` summarising today/goal/session
    /// stats. Built imperatively, mirroring the Quick Open dialog shape
    /// (ToolbarView + HeaderBar + Clamp + vertical Box). Close button is
    /// the only action; Esc dismisses by default.
    fn present_writing_stats_dialog(&self) {
        let imp = self.imp();
        // Roll over the day before reading, so a stale `today_words` from a
        // previous date renders as 0 instead of yesterday's tally.
        self.roll_over_today_words_if_needed();

        let (today_words, daily_goal) = {
            let cfg = imp.config.borrow();
            (cfg.today_words, cfg.daily_word_goal)
        };

        // Session words = current document word count, same source the
        // footer uses in `update_stats`.
        let session_text = match imp.view_mode.get() {
            ViewMode::Edit => imp
                .edit_view
                .borrow()
                .as_ref()
                .map(|sv| {
                    let buf = sv.buffer();
                    buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string()
                })
                .unwrap_or_default(),
            ViewMode::Split | ViewMode::View => imp.file_content.borrow().clone(),
        };
        let session_words = session_text.split_whitespace().count() as u32;

        let dialog = adw::Dialog::builder()
            .title("写作统计")
            .content_width(420)
            .build();

        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        toolbar.add_top_bar(&header);

        let clamp = adw::Clamp::builder()
            .maximum_size(360)
            .tightening_threshold(360)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        // Helper: build a "big number + subtitle" row.
        let make_row = |title: &str, big_markup: &str, subtitle: &str| -> gtk::Box {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .build();
            let title_label = gtk::Label::builder()
                .label(title)
                .xalign(0.0)
                .build();
            title_label.add_css_class("heading");
            row.append(&title_label);

            let big = gtk::Label::builder()
                .xalign(0.0)
                .build();
            big.set_markup(big_markup);
            row.append(&big);

            let sub = gtk::Label::builder()
                .label(subtitle)
                .xalign(0.0)
                .build();
            sub.add_css_class("dim-label");
            sub.add_css_class("caption");
            row.append(&sub);
            row
        };

        // "Today" row.
        let today_markup = format!(
            "<span size=\"200%\" weight=\"bold\">{}</span>",
            today_words
        );
        vbox.append(&make_row("今日", &today_markup, "words written today"));

        // "Goal" row.
        let goal_markup = if daily_goal == 0 {
            "<span size=\"200%\" weight=\"bold\">—</span>".to_string()
        } else {
            format!(
                "<span size=\"200%\" weight=\"bold\">{}</span>",
                daily_goal
            )
        };
        vbox.append(&make_row("目标", &goal_markup, "daily target"));

        // Progress bar only when goal > 0.
        if daily_goal > 0 {
            let frac = (today_words as f64 / daily_goal as f64).min(1.0);
            let progress = gtk::ProgressBar::new();
            progress.set_fraction(frac);
            progress.add_css_class("osd");
            vbox.append(&progress);
        }

        // Session row — small horizontal row with label + number suffix.
        let session_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(6)
            .build();
        let session_label = gtk::Label::builder()
            .label("本次会话")
            .xalign(0.0)
            .hexpand(true)
            .build();
        session_label.add_css_class("heading");
        let session_value = gtk::Label::builder()
            .label(format!("{}", session_words))
            .xalign(1.0)
            .build();
        session_value.add_css_class("numeric");
        session_row.append(&session_label);
        session_row.append(&session_value);
        vbox.append(&session_row);

        // Close button — right-aligned.
        let button_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(6)
            .build();
        let spacer = gtk::Box::builder()
            .hexpand(true)
            .build();
        button_row.append(&spacer);
        let close_btn = gtk::Button::builder()
            .label("关闭")
            .build();
        close_btn.add_css_class("suggested-action");
        button_row.append(&close_btn);
        vbox.append(&button_row);

        clamp.set_child(Some(&vbox));
        toolbar.set_content(Some(&clamp));
        dialog.set_child(Some(&toolbar));

        let dialog_for_close = dialog.clone();
        close_btn.connect_clicked(move |_| {
            dialog_for_close.close();
        });

        dialog.present(Some(self));
    }

    fn reapply_fonts(&self) {
        let imp = self.imp();
        let config = imp.config.borrow();
        let zoom = imp.font_zoom.get();
        apply_font_css_cached(&imp.view_css_provider, &imp.last_view_css, "manuscript-view", &config, zoom);
        apply_font_css_cached(&imp.split_css_provider, &imp.last_split_css, "manuscript-view", &config, zoom);
        apply_font_css_cached(&imp.edit_css_provider, &imp.last_edit_css, "manuscript-edit", &config, zoom);
        apply_font_css_cached(&imp.split_edit_css_provider, &imp.last_split_edit_css, "manuscript-edit", &config, zoom);
    }

    fn adjust_font_zoom(&self, factor: f64) {
        let imp = self.imp();
        let new_zoom = (imp.font_zoom.get() * factor).clamp(0.5, 3.0);
        imp.font_zoom.set(new_zoom);
        self.reapply_fonts();
    }

    fn reset_font_zoom(&self) {
        let imp = self.imp();
        if (imp.font_zoom.get() - 1.0).abs() < f64::EPSILON {
            return;
        }
        imp.font_zoom.set(1.0);
        self.reapply_fonts();
    }

    fn schedule_split_render(&self) {
        let imp = self.imp();
        // Cancel previous pending render
        if let Some(id) = imp.split_render_debounce.borrow_mut().take() {
            id.remove();
        }
        let id = glib::timeout_add_local_once(
            std::time::Duration::from_millis(250),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move || {
                    win.imp().split_render_debounce.replace(None);
                    win.refresh_split_render();
                }
            ),
        );
        imp.split_render_debounce.replace(Some(id));
    }

    fn get_active_source_view(&self) -> Option<sourceview5::View> {
        let imp = self.imp();
        match imp.view_mode.get() {
            ViewMode::Edit => imp.edit_view.borrow().clone(),
            ViewMode::Split => imp.split_edit_view.borrow().clone(),
            _ => None,
        }
    }

    fn is_dark_theme(&self) -> bool {
        adw::StyleManager::default().is_dark()
    }
}

// -- Helpers --

/// One top-to-bottom sweep of a vault `TreeListModel`, expanding ancestor
/// rows of `target` (so deeper levels get realized on the next
/// `items-changed` tick) and selecting `target`'s own row if it is already
/// present. Returns `true` once `target` has been found and selected —
/// callers use that to stop retrying. See `Window::reveal_current_in_vault`.
fn reveal_pass(sel: &gtk::SingleSelection, tree: &gtk::TreeListModel, target: &std::path::Path) -> bool {
    let n = tree.n_items();
    for i in 0..n {
        let Some(row) = tree.row(i) else { continue };
        let Some(info) = row.item().and_downcast::<gio::FileInfo>() else {
            continue;
        };
        let this = info
            .attribute_object("standard::file")
            .and_downcast::<gio::File>()
            .and_then(|f| f.path());
        match this {
            Some(p) if p == target => {
                sel.set_selected(i);
                return true;
            }
            // Expand ancestors so deeper rows get realized on the next tick.
            Some(p) if target.starts_with(&p) && !row.is_expanded() => {
                row.set_expanded(true);
            }
            _ => {}
        }
    }
    false
}

/// A GtkFilter keeping directories and Markdown files (`.md` / `.markdown`),
/// case-insensitive on the extension, minus dot-entries. Used at every level
/// of the vault tree.
fn vault_md_filter() -> gtk::CustomFilter {
    gtk::CustomFilter::new(|obj| {
        let Some(info) = obj.downcast_ref::<gio::FileInfo>() else { return false };
        let name = info.name();
        // Dot-entries (.git, .claude, .obsidian, …) are tooling, not notes.
        if name.to_string_lossy().starts_with('.') {
            return false;
        }
        if info.file_type() == gio::FileType::Directory {
            return true;
        }
        let lower = name.to_string_lossy().to_lowercase();
        lower.ends_with(".md") || lower.ends_with(".markdown")
    })
}

/// Fast content fingerprint used to skip a full markdown re-render when the
/// inputs are unchanged. Theme is included because dark/light flips swap
/// theme-dependent tags.
fn render_hash(content: &str, is_dark: bool) -> u64 {
    let mut h = DefaultHasher::new();
    content.hash(&mut h);
    is_dark.hash(&mut h);
    let v = h.finish();
    // Reserve 0 as the "no previous render yet" sentinel so the first render
    // always runs even on the rare hash collision with the initial zero.
    if v == 0 {
        1
    } else {
        v
    }
}

/// Register the `focus-dim` tag on a TextBuffer's tag table if absent.
/// Neutral mid-gray works for both light and dark themes.
fn ensure_focus_tag(buf: &gtk::TextBuffer) {
    let table = buf.tag_table();
    if table.lookup("focus-dim").is_some() {
        return;
    }
    let tag = gtk::TextTag::builder()
        .name("focus-dim")
        .foreground("#888888")
        .build();
    table.add(&tag);
}

/// Attach a libspelling adapter to a sourceview5::View and wire its menu model
/// into the right-click extra menu. Returns None if the buffer cannot be
/// downcast (should not happen — create_source_view always builds a sourceview5::Buffer).
fn attach_spell_adapter(view: &sourceview5::View, config: &AppConfig) -> Option<spelling::TextBufferAdapter> {
    let buffer = view.buffer().downcast::<sourceview5::Buffer>().ok()?;
    let checker = if config.spell_check_language.is_empty() {
        spelling::Checker::default()
    } else {
        let provider = spelling::Checker::default().provider();
        spelling::Checker::new(&provider, &config.spell_check_language)
    };
    let adapter = spelling::TextBufferAdapter::new(&buffer, &checker);
    adapter.set_enabled(config.spell_check);
    view.set_extra_menu(Some(&adapter.menu_model()));
    view.insert_action_group("spelling", Some(&adapter));
    Some(adapter)
}

fn scroll_source_view_to_line(sv: &sourceview5::View, line: i32) {
    let buf = sv.buffer();
    let Some(iter) = buf.iter_at_line(line) else {
        return;
    };
    buf.place_cursor(&iter);
    let mark = buf.create_mark(None, &iter, false);
    sv.scroll_to_mark(&mark, 0.0, true, 0.0, 0.1);
    buf.delete_mark(&mark);
}

fn create_source_view(config: &AppConfig) -> sourceview5::View {
    let buffer = sourceview5::Buffer::new(None::<&gtk::TextTagTable>);
    buffer.set_max_undo_levels(u32::MAX); // unlimited undo

    let lang_manager = sourceview5::LanguageManager::default();
    if let Some(lang) = lang_manager.language("markdown") {
        buffer.set_language(Some(&lang));
    }

    let scheme_manager = sourceview5::StyleSchemeManager::default();
    let is_dark = adw::StyleManager::default().is_dark();
    let scheme_name = if is_dark { "Adwaita-dark" } else { "Adwaita" };
    if let Some(scheme) = scheme_manager.scheme(scheme_name) {
        buffer.set_style_scheme(Some(&scheme));
    }

    let view = sourceview5::View::with_buffer(&buffer);
    view.set_editable(true);
    view.set_wrap_mode(config.gtk_wrap_mode());
    view.set_left_margin(16);
    view.set_right_margin(16);
    view.set_top_margin(16);
    view.set_bottom_margin(16);
    view.set_monospace(true);
    view.set_show_line_numbers(config.show_line_numbers);
    view.set_highlight_current_line(true);
    view.set_tab_width(4);
    view.set_auto_indent(true);
    view.add_css_class("manuscript-edit");

    view
}

/// Smart list continuation. On plain Enter inside a list item, continue the list:
/// `- foo` → new line gets `- ` prefix; `1. foo` → `2. `; `- [ ] foo` → `- [ ] `.
/// On an empty list item, the marker is removed (user exits the list).
/// Indentation of the previous line is preserved.
/// Returns `true` if Enter was handled (caller should stop further propagation).
fn try_continue_list(buf: &gtk::TextBuffer) -> bool {
    let cursor = buf.iter_at_offset(buf.cursor_position());
    let line_num = cursor.line();
    let Some(line_start) = buf.iter_at_line(line_num) else {
        return false;
    };
    let line_text = buf
        .text(&line_start, &cursor, false)
        .to_string();

    // Capture leading whitespace (indentation)
    let indent_len = line_text
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(char::len_utf8)
        .sum::<usize>();
    let indent = &line_text[..indent_len];
    let trimmed = &line_text[indent_len..];

    enum Marker {
        Bullet(char),
        Task(char),
        Ordered(u32),
    }

    let (marker, prefix_len) = if let Some(c) = trimmed.chars().next() {
        if matches!(c, '-' | '*' | '+') {
            // Task list: "- [ ] " or "- [x] "
            if trimmed.len() >= 6
                && trimmed.as_bytes()[1] == b' '
                && trimmed.as_bytes()[2] == b'['
                && matches!(trimmed.as_bytes()[3], b' ' | b'x' | b'X')
                && trimmed.as_bytes()[4] == b']'
                && trimmed.as_bytes()[5] == b' '
            {
                (Marker::Task(c), 6)
            } else if trimmed.len() >= 2 && trimmed.as_bytes()[1] == b' ' {
                (Marker::Bullet(c), 2)
            } else {
                return false;
            }
        } else if c.is_ascii_digit() {
            // Ordered: digits + ". "
            let digit_count: usize = trimmed
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .map(char::len_utf8)
                .sum();
            if trimmed.len() > digit_count + 1
                && trimmed.as_bytes()[digit_count] == b'.'
                && trimmed.as_bytes()[digit_count + 1] == b' '
            {
                let num: u32 = trimmed[..digit_count].parse().unwrap_or(1);
                (Marker::Ordered(num), digit_count + 2)
            } else {
                return false;
            }
        } else {
            return false;
        }
    } else {
        return false;
    };

    let after = &trimmed[prefix_len..];

    if after.is_empty() {
        // Empty item — clear the marker (and indent) so the user exits the list cleanly.
        let mut clear_start = line_start;
        let mut clear_end = cursor;
        buf.delete(&mut clear_start, &mut clear_end);
        return true;
    }

    let new_prefix = match marker {
        Marker::Bullet(c) => format!("\n{}{} ", indent, c),
        Marker::Task(c) => format!("\n{}{} [ ] ", indent, c),
        Marker::Ordered(n) => format!("\n{}{}. ", indent, n + 1),
    };

    let mut iter = cursor;
    buf.insert(&mut iter, &new_prefix);
    true
}

/// A restored scroll position still settling: re-applied every frame (`tick`)
/// until the user scrolls (`value_id` sees a foreign value) or `timer` ends.
pub struct ScrollRestore {
    vadj: gtk::Adjustment,
    tick: gtk::TickCallbackId,
    value_id: glib::SignalHandlerId,
    timer: glib::SourceId,
}

/// The vadjustment value that puts buffer `y` at the top of `tv`'s viewport —
/// measured from where the viewport is now rather than assuming how GTK
/// offsets the adjustment (top margin) against buffer coordinates.
fn value_for_buffer_y(tv: &gtk::TextView, vadj: &gtk::Adjustment, y: f64) -> f64 {
    let (_, top) = tv.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, 0);
    vadj.value() + y - top as f64
}

/// Char offset of the first character on the display line at buffer `y`.
/// `iter_at_location` reports a miss (None) for points over the margin or
/// past a line's end — x = 0 is inside the left margin, which silently
/// turned every position into offset 0 — so probe just inside the text area
/// and fall back to the start of the buffer line at `y`.
fn offset_at_y(tv: &gtk::TextView, y: i32) -> i32 {
    tv.iter_at_location(tv.left_margin() + 1, y)
        .map(|it| {
            let mut it = it;
            tv.backward_display_line_start(&mut it);
            it
        })
        .unwrap_or_else(|| tv.line_at_y(y).0)
        .offset()
}

/// Folder of `path` as the user knows it, for the header subtitle and
/// Quick Open. Inside Flatpak a file opened through the file chooser lives
/// at `/run/user/<uid>/doc/<id>/name` (document portal); show the real
/// host folder from the portal's `host-path` xattr instead, or nothing when
/// the portal is too old to expose it — an opaque id means nothing to the
/// reader. The home prefix is shortened to `~`.
pub fn display_dir(path: &std::path::Path) -> String {
    let host = if is_portal_path(path) {
        let attr = "xattr::document-portal.host-path";
        gio::File::for_path(path)
            .query_info(attr, gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
            .ok()
            .and_then(|info| info.attribute_string(attr))
            .map(|s| std::path::PathBuf::from(unescape_xattr(&s)))
    } else {
        Some(path.to_path_buf())
    };
    let Some(dir) = host.as_deref().and_then(|p| p.parent()) else {
        return String::new();
    };
    let home = glib::home_dir();
    match dir.strip_prefix(&home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Ok(rel) => format!("~/{}", rel.to_string_lossy()),
        Err(_) => dir.to_string_lossy().to_string(),
    }
}

/// GIO hex-escapes every xattr byte outside printable ASCII (and `\`
/// itself) as `\xHH`, so a Cyrillic folder name arrives as a run of escapes.
fn unescape_xattr(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && b.get(i + 1) == Some(&b'x') {
            let hex = b.get(i + 2..i + 4).and_then(|h| std::str::from_utf8(h).ok());
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_portal_path(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("/run/flatpak/doc/")
        || (s.starts_with("/run/user/") && s.split('/').nth(4) == Some("doc"))
}

/// Double-click on the Split divider resets the panes to equal halves.
///
/// GtkPaned's own capture-phase drag gesture claims every press on the
/// handle: the handle widget never sees the event, and a GestureClick on the
/// paned is denied, so neither `n_press` nor a gesture-based timer works
/// (grouping with the drag gesture didn't help either). A legacy controller
/// isn't part of sequence claiming — it sees every raw press — so we time
/// consecutive presses on the handle ourselves, and centre on the second
/// press's release.
fn attach_divider_double_click(paned: &gtk::Paned) {
    // (event time ms, x, y) of the previous press on the handle.
    let last_press: std::rc::Rc<std::cell::Cell<Option<(u32, f64, f64)>>> = Default::default();
    // Set by the second press, consumed by its release.
    let center_on_release: std::rc::Rc<std::cell::Cell<bool>> = Default::default();
    let legacy = gtk::EventControllerLegacy::new();
    legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
    legacy.connect_event(glib::clone!(
        #[weak]
        paned,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, event| {
            if event.event_type() == gtk::gdk::EventType::ButtonRelease && center_on_release.replace(false) {
                // The paned's drag gesture resets the position to where the
                // press started on every motion update, so centring on the
                // press itself gets undone by the slightest jitter. Centre
                // after the drag has ended.
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    paned,
                    move || paned.set_position(paned.width() / 2)
                ));
                return glib::Propagation::Proceed;
            }
            if event.event_type() != gtk::gdk::EventType::ButtonPress {
                return glib::Propagation::Proceed;
            }
            let primary = event
                .downcast_ref::<gtk::gdk::ButtonEvent>()
                .is_some_and(|b| b.button() == gtk::gdk::BUTTON_PRIMARY);
            let Some((x, y)) = event_point_in(&paned, event) else {
                return glib::Propagation::Proceed;
            };
            if !primary || !point_on_paned_handle(&paned, x, y) {
                last_press.set(None);
                return glib::Propagation::Proceed;
            }
            let (max_ms, max_dist) = gtk::Settings::default()
                .map(|st| (st.gtk_double_click_time().max(1) as u32, st.gtk_double_click_distance().max(1) as f64))
                .unwrap_or((400, 5.0));
            let now = event.time();
            let is_double = last_press.get().is_some_and(|(t, px, py)| {
                now.wrapping_sub(t) <= max_ms && (x - px).abs() <= max_dist && (y - py).abs() <= max_dist
            });
            tracing::debug!(x, y, is_double, "split divider press");
            if is_double {
                last_press.set(None);
                center_on_release.set(true);
            } else {
                last_press.set(Some((now, x, y)));
            }
            glib::Propagation::Proceed
        }
    ));
    paned.add_controller(legacy);
}

/// Translate a raw event's surface coordinates into `widget` coordinates.
fn event_point_in(widget: &gtk::Paned, event: &gtk::gdk::Event) -> Option<(f64, f64)> {
    let (ex, ey) = event.position()?;
    let native = widget.native()?;
    let (tx, ty) = native.surface_transform();
    let p = gtk::graphene::Point::new((ex - tx) as f32, (ey - ty) as f32);
    let p = native.compute_point(widget, &p)?;
    Some((p.x() as f64, p.y() as f64))
}

fn point_on_paned_handle(paned: &gtk::Paned, x: f64, y: f64) -> bool {
    let mut child = paned.first_child();
    while let Some(w) = child {
        if w.type_().name() == "GtkPanedHandle" {
            return w.compute_bounds(paned).is_some_and(|b| {
                // A few px of slack: the handle is 1 px wide, the drag
                // target is wider.
                let slack = 6.0;
                (x as f32) >= b.x() - slack
                    && (x as f32) <= b.x() + b.width() + slack
                    && (y as f32) >= b.y()
                    && (y as f32) <= b.y() + b.height()
            });
        }
        child = w.next_sibling();
    }
    false
}

/// Drive a ScrolledWindow's vertical adjustment from Up / Down / PgUp /
/// PgDn / Home / End / Space on a read-only TextView. Required because the
/// View-mode TextView has cursor_visible(false) + editable(false), so GTK's
/// built-in caret navigation has nothing to move and silently swallows the
/// arrow keys without scrolling.
fn attach_view_scroll_keys(view: &gtk::TextView, scroll: &gtk::ScrolledWindow) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let scroll_weak = scroll.downgrade();
    key.connect_key_pressed(move |_, keyval, _, state| {
        // Ignore key combos with Ctrl/Alt/Super — those are app shortcuts
        // (Ctrl+F, Ctrl+E, etc.) and must not be hijacked for scrolling.
        let modmask = state & (
            gtk::gdk::ModifierType::CONTROL_MASK
                | gtk::gdk::ModifierType::ALT_MASK
                | gtk::gdk::ModifierType::SUPER_MASK
        );
        if !modmask.is_empty() {
            return glib::Propagation::Proceed;
        }
        let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);

        let Some(sw) = scroll_weak.upgrade() else { return glib::Propagation::Proceed };
        let adj = sw.vadjustment();
        let page = adj.page_size().max(1.0);
        // Approximate one-line step: 1/16 of a page. Roughly font line height
        // at typical content widths.
        let line_step = (page / 16.0).max(20.0);

        let new_val = match keyval {
            gtk::gdk::Key::Up => adj.value() - line_step,
            gtk::gdk::Key::Down => adj.value() + line_step,
            gtk::gdk::Key::Page_Up => adj.value() - page * 0.9,
            gtk::gdk::Key::Page_Down => adj.value() + page * 0.9,
            gtk::gdk::Key::Home => adj.lower(),
            gtk::gdk::Key::End => (adj.upper() - page).max(adj.lower()),
            gtk::gdk::Key::space => {
                if shift { adj.value() - page * 0.9 } else { adj.value() + page * 0.9 }
            }
            _ => return glib::Propagation::Proceed,
        };
        let clamped = new_val.clamp(adj.lower(), (adj.upper() - page).max(adj.lower()));
        adj.set_value(clamped);
        glib::Propagation::Stop
    });
    view.add_controller(key);
}

fn attach_smart_list_enter(view: &sourceview5::View) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let view_weak = view.downgrade();
    key.connect_key_pressed(move |_, keyval, _, state| {
        if keyval != gtk::gdk::Key::Return && keyval != gtk::gdk::Key::KP_Enter {
            return glib::Propagation::Proceed;
        }
        // Only plain Enter (no Shift/Ctrl/Alt) — Shift+Enter stays as hard break
        let modifiers = state & (
            gtk::gdk::ModifierType::SHIFT_MASK
            | gtk::gdk::ModifierType::CONTROL_MASK
            | gtk::gdk::ModifierType::ALT_MASK
        );
        if !modifiers.is_empty() {
            return glib::Propagation::Proceed;
        }
        let Some(view) = view_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let buf = view.buffer();
        // Don't fire while a selection is active — let GtkSourceView replace it
        if buf.has_selection() {
            return glib::Propagation::Proceed;
        }
        buf.begin_user_action();
        let handled = try_continue_list(&buf);
        buf.end_user_action();
        if handled {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    view.add_controller(key);
}

/// Prepend `prefix` to the current line at the cursor. If the line already
/// starts with the prefix, do nothing (idempotent). Used by mobile action bar
/// buttons for headings, lists, and tasks.
fn insert_line_prefix(sv: &sourceview5::View, prefix: &str) {
    let buf = sv.buffer();
    let cursor = buf.iter_at_offset(buf.cursor_position());
    let line_num = cursor.line();
    let Some(line_start) = buf.iter_at_line(line_num) else { return };
    let mut line_end = line_start;
    line_end.forward_to_line_end();
    let line_text = buf.text(&line_start, &line_end, false).to_string();
    if line_text.starts_with(prefix) {
        return;
    }
    let mut insert_at = buf.iter_at_line(line_num).unwrap();
    buf.begin_user_action();
    buf.insert(&mut insert_at, prefix);
    buf.end_user_action();
    sv.grab_focus();
}

/// Wrap the current selection (or insert paired markers at the cursor) with `prefix`/`suffix`.
/// Used by Ctrl+B (`**`), Ctrl+I (`*`).
fn wrap_selection(sv: &sourceview5::View, prefix: &str, suffix: &str) {
    let buf = sv.buffer();
    buf.begin_user_action();
    if let Some((sel_start, sel_end)) = buf.selection_bounds() {
        let start_off = sel_start.offset();
        let end_off = sel_end.offset();
        let text = buf.text(&sel_start, &sel_end, false).to_string();
        let mut s = buf.iter_at_offset(start_off);
        let mut e = buf.iter_at_offset(end_off);
        buf.delete(&mut s, &mut e);
        let wrapped = format!("{}{}{}", prefix, text, suffix);
        let mut s2 = buf.iter_at_offset(start_off);
        buf.insert(&mut s2, &wrapped);
        let prefix_chars = prefix.chars().count() as i32;
        let text_chars = text.chars().count() as i32;
        let inner_start = buf.iter_at_offset(start_off + prefix_chars);
        let inner_end = buf.iter_at_offset(start_off + prefix_chars + text_chars);
        buf.select_range(&inner_start, &inner_end);
    } else {
        let pos = buf.cursor_position();
        let mut iter = buf.iter_at_offset(pos);
        let combined = format!("{}{}", prefix, suffix);
        buf.insert(&mut iter, &combined);
        let prefix_chars = prefix.chars().count() as i32;
        let cursor = buf.iter_at_offset(pos + prefix_chars);
        buf.place_cursor(&cursor);
    }
    buf.end_user_action();
}

/// Insert a Markdown link around the selection (or `[text](url)` at cursor) and
/// select the `url` placeholder so the user can immediately type it.
fn insert_link(sv: &sourceview5::View) {
    let buf = sv.buffer();
    buf.begin_user_action();
    if let Some((sel_start, sel_end)) = buf.selection_bounds() {
        let start_off = sel_start.offset();
        let end_off = sel_end.offset();
        let text = buf.text(&sel_start, &sel_end, false).to_string();
        let mut s = buf.iter_at_offset(start_off);
        let mut e = buf.iter_at_offset(end_off);
        buf.delete(&mut s, &mut e);
        let combined = format!("[{}](url)", text);
        let mut s2 = buf.iter_at_offset(start_off);
        buf.insert(&mut s2, &combined);
        // Layout `[<text>](url)` — select the literal "url"
        let text_chars = text.chars().count() as i32;
        let url_start = buf.iter_at_offset(start_off + 3 + text_chars);
        let url_end = buf.iter_at_offset(start_off + 6 + text_chars);
        buf.select_range(&url_start, &url_end);
    } else {
        let pos = buf.cursor_position();
        let mut iter = buf.iter_at_offset(pos);
        buf.insert(&mut iter, "[text](url)");
        // Select "text" so the user can replace it first
        let text_start = buf.iter_at_offset(pos + 1);
        let text_end = buf.iter_at_offset(pos + 5);
        buf.select_range(&text_start, &text_end);
    }
    buf.end_user_action();
}

/// Update a stored CssProvider with font settings (no leak — same provider is reused).
/// Cached: only calls `load_from_data` when the computed CSS string
/// has actually changed. Identical reloads (Preferences save with no font
/// change, repeated zoom resets) were forcing GTK to invalidate its style
/// cache and the font map.
fn apply_font_css_cached(
    provider: &gtk::CssProvider,
    cache: &RefCell<String>,
    class: &str,
    config: &AppConfig,
    zoom: f64,
) {
    let css = build_font_css(class, config, zoom);
    if cache.borrow().as_str() == css {
        return;
    }
    provider.load_from_data(&css);
    cache.replace(css);
}

fn build_font_css(class: &str, config: &AppConfig, zoom: f64) -> String {
    let extra = if class == "manuscript-view" {
        " background: transparent;"
    } else {
        ""
    };
    let size = (config.font_size as f64 * zoom).max(6.0);
    format!(
        ".{} {{ font-family: \"{}\", sans-serif; font-size: {:.2}pt;{} }}",
        class, config.font_family, size, extra
    )
}

/// Build a file chooser dialog for `parent`.
///
/// `GtkFileChooserNative` delegates to the platform portal
/// (`xdg-desktop-portal` → `xdg-desktop-portal-xapp`) on desktops that
/// advertise one. When that backend is broken the dialog fails with
/// "Couldn't show the contents of the folder / Operation was cancelled" and
/// the application is left with a stuck modal — verified on Linux Mint 22.3
/// (Cinnamon): a bare GTK4 probe hits the same failure, while GTK's own
/// `GtkFileChooserDialog` lists folders correctly.
///
/// So we deliberately use `GtkFileChooserDialog`, the dialog GTK draws
/// itself. It never touches the portal, works on every backend, and keeps the
/// full feature set (filters, current name, recent locations) this app relies
/// on. The cost is that it follows the GTK theme rather than the desktop's
/// native picker — a fair trade for a dialog that actually opens.
///
/// The returned dialog is ready to `show()`. Callers connect to `response`
/// exactly as they did with the native variant and must NOT call
/// `destroy()` themselves — this helper wires that up.
fn make_file_chooser(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    action: gtk::FileChooserAction,
    accept_label: &str,
) -> gtk::FileChooserDialog {
    let dialog = gtk::FileChooserDialog::builder()
        .title(title)
        .modal(true)
        .transient_for(parent)
        .action(action)
        .build();
    // GTK's own chooser has no default buttons; without these the dialog
    // would open with no way to accept or cancel.
    #[allow(deprecated)]
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button(accept_label, gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);

    // Dismiss on every response.
    //
    // `GtkFileChooserNative` tore itself down once the user answered; its
    // `GtkFileChooserDialog` replacement does not — the window would sit
    // there after Save or Cancel until something else closed it. Callers
    // can't reliably do this: their own `connect_response` handlers bail out
    // early on several paths (wrong response, no file, no path), and a
    // `destroy()` at the end of such a handler is skipped by each of those
    // returns.
    //
    // `connect_response` handlers run in connection order, so this one fires
    // first and the visibility reaper below only tears the dialog down once
    // the caller's handler has finished with it.
    dialog.connect_response(|dialog, _response| {
        // Hide immediately so the window disappears with the click, then
        // destroy on the next main-loop iteration. Destroying synchronously
        // here would free the dialog while the caller's handler — connected
        // after this one — still holds `dialog` and is about to read
        // `dialog.file()` from it.
        dialog.set_visible(false);
        let dialog = dialog.clone();
        glib::idle_add_local_once(move || {
            dialog.destroy();
        });
    });

    dialog
}

fn show_about_dialog(parent: &impl IsA<gtk::Widget>) {
    let dialog = adw::AboutDialog::builder()
        .application_name("Manuscript")
        .application_icon("io.gitlab.ilshat_apps.manuscript")
        .developer_name("Ilshat Ishdavletov")
        .version(env!("CARGO_PKG_VERSION"))
        .license_type(gtk::License::Gpl30)
        .website("https://gitlab.com/ilshat-apps/manuscript")
        .issue_url("https://gitlab.com/ilshat-apps/manuscript/-/issues")
        .comments("用 Rust 编写的快速、原生 GNOME Markdown 阅读与编辑器")
        .build();
    dialog.add_link("支持开发", "https://ko-fi.com/ilshat");
    dialog.present(Some(parent));
}

fn show_shortcuts_window(parent: &impl IsA<gtk::Window>) {
    let xml = r#"
    <interface>
      <object class="GtkShortcutsWindow" id="shortcuts">
        <property name="modal">true</property>
        <child>
          <object class="GtkShortcutsSection">
            <property name="section-name">shortcuts</property>
            <child>
              <object class="GtkShortcutsGroup">
                <property name="title">通用</property>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">新建文件</property>
                    <property name="accelerator">&lt;Control&gt;n</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">打开文件</property>
                    <property name="accelerator">&lt;Control&gt;o</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">快速打开（最近文件 + 当前文件夹）</property>
                    <property name="accelerator">&lt;Control&gt;&lt;Shift&gt;o</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">后退（上一个位置）</property>
                    <property name="accelerator">&lt;Alt&gt;Left</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">前进</property>
                    <property name="accelerator">&lt;Alt&gt;Right</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">保存文件</property>
                    <property name="accelerator">&lt;Control&gt;s</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">搜索</property>
                    <property name="accelerator">&lt;Control&gt;f</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">打印</property>
                    <property name="accelerator">&lt;Control&gt;p</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">切换编辑模式</property>
                    <property name="accelerator">&lt;Control&gt;e</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">切换分栏视图</property>
                    <property name="accelerator">&lt;Control&gt;&lt;Shift&gt;e</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">首选项</property>
                    <property name="accelerator">&lt;Control&gt;comma</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">关闭窗口</property>
                    <property name="accelerator">&lt;Control&gt;q</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">放大</property>
                    <property name="accelerator">&lt;Control&gt;equal</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">缩小</property>
                    <property name="accelerator">&lt;Control&gt;minus</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">重置缩放</property>
                    <property name="accelerator">&lt;Control&gt;0</property>
                  </object>
                </child>
              </object>
            </child>
            <child>
              <object class="GtkShortcutsGroup">
                <property name="title">编辑</property>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">粗体</property>
                    <property name="accelerator">&lt;Control&gt;b</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">斜体</property>
                    <property name="accelerator">&lt;Control&gt;i</property>
                  </object>
                </child>
                <child>
                  <object class="GtkShortcutsShortcut">
                    <property name="title">插入链接</property>
                    <property name="accelerator">&lt;Control&gt;k</property>
                  </object>
                </child>
              </object>
            </child>
          </object>
        </child>
      </object>
    </interface>
    "#;

    let builder = gtk::Builder::from_string(xml);
    let window: gtk::ShortcutsWindow = builder.object("shortcuts").unwrap();
    window.set_transient_for(Some(parent));
    window.present();
}

#[cfg(test)]
mod tests {
    use super::{is_portal_path, unescape_xattr};
    use std::path::Path;

    #[test]
    fn portal_paths_detected() {
        assert!(is_portal_path(Path::new("/run/user/1000/doc/RvqKsX6Q/sample.md")));
        assert!(is_portal_path(Path::new("/run/flatpak/doc/abc/sample.md")));
        assert!(!is_portal_path(Path::new("/run/user/1000/gvfs/x.md")));
        assert!(!is_portal_path(Path::new("/home/u/doc/x.md")));
    }

    #[test]
    fn xattr_escapes_decoded() {
        assert_eq!(unescape_xattr("/home/u/notes.md"), "/home/u/notes.md");
        assert_eq!(unescape_xattr(r"/home/u/\xd0\x97\xd0\xb0/a.md"), "/home/u/За/a.md");
        assert_eq!(unescape_xattr(r"/a/\x5c/b"), r"/a/\/b");
        // Truncated escape stays literal.
        assert_eq!(unescape_xattr(r"/a\x4"), r"/a\x4");
    }
}
