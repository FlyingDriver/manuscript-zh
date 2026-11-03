use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub font_family: String,
    pub font_size: u32,
    pub line_spacing: u32,
    pub show_line_numbers: bool,
    pub wrap_mode: WrapMode,
    #[serde(default = "default_content_width")]
    pub content_width: u32,
    /// Explicit vault root for the sidebar tree and wikilink resolution.
    /// None = follow the parent directory of the currently-open file.
    #[serde(default)]
    pub vault_root: Option<String>,
    /// Whether the vault sidebar is revealed (persisted across sessions).
    #[serde(default)]
    pub vault_sidebar_shown: bool,
    #[serde(default)]
    pub recent_files: Vec<String>,
    /// Restore last session (open file, view mode, scroll) when launched without args
    #[serde(default = "default_true")]
    pub restore_session: bool,
    /// Last opened file (for session restore)
    #[serde(default)]
    pub last_file: Option<String>,
    /// Last view mode: "view" / "edit" / "split"
    #[serde(default)]
    pub last_view_mode: Option<String>,
    /// Last scroll position (line number, 0-based)
    #[serde(default)]
    pub last_scroll_line: i32,
    /// Open each file in a new window. When false, opening a file from the file
    /// manager replaces the file in the existing window.
    #[serde(default = "default_true")]
    pub open_files_in_new_window: bool,
    /// Inline spell check in Edit/Split source views (libspelling).
    #[serde(default)]
    pub spell_check: bool,
    /// Spell check language tag (e.g. "en_US", "ru_RU"). Empty = system default.
    #[serde(default)]
    pub spell_check_language: String,
    /// In fullscreen (F11), hide header bar, outline, search bar, scrollbar.
    #[serde(default = "default_true")]
    pub distraction_free_fullscreen: bool,
    /// Dim text outside the current paragraph in Edit/Split source views.
    #[serde(default)]
    pub focus_mode: bool,
    /// Keep current line near vertical center of viewport.
    #[serde(default)]
    pub typewriter_mode: bool,
    /// Auto-save the current file on a debounce timer after edits.
    #[serde(default)]
    pub auto_save: bool,
    /// Render markdown tables as embedded GtkGrid widgets instead of ASCII pipe
    /// rows. On by default; copy-paste and search skip cell contents (widget
    /// text is not part of the buffer), so plain tables stay available.
    #[serde(default = "default_true")]
    pub rich_tables: bool,
    /// Render a single newline inside a paragraph as a line break (Obsidian
    /// style) instead of a space (CommonMark / GitHub). Off by default so
    /// files hard-wrapped at 80 columns read as flowing paragraphs.
    #[serde(default)]
    pub keep_line_breaks: bool,
    /// Widest an image renders when the document doesn't size it itself.
    /// Per-image `{width=…}` / `<img width=…>` overrides this.
    #[serde(default = "default_max_image_width")]
    pub max_image_width: u32,
    /// PDF / print export: Pango font description (e.g. "Serif 11").
    #[serde(default = "default_export_font")]
    pub export_font: String,
    /// Page margins in millimetres for PDF / print export.
    #[serde(default = "default_export_margin_mm")]
    pub export_margin_top_mm: f64,
    #[serde(default = "default_export_margin_mm")]
    pub export_margin_right_mm: f64,
    #[serde(default = "default_export_margin_mm")]
    pub export_margin_bottom_mm: f64,
    #[serde(default = "default_export_margin_mm")]
    pub export_margin_left_mm: f64,
    /// Show a bottom footer bar that hosts the word-count and read-time
    /// indicators. When off, those indicators stay in the header subtitle.
    /// Master toggle for the writing-statistics feature too — goals only
    /// render when the footer has somewhere to draw them.
    #[serde(default)]
    pub show_footer: bool,
    /// Enable daily word-goal tracking. Sensitive only when show_footer is on
    /// (the goal label lives in the footer). When off, no tracking happens
    /// and no menu item appears.
    #[serde(default)]
    pub writing_stats: bool,
    /// Daily word target. 0 means "track today's words but don't render a
    /// goal". Capped at 5000 in the spin row.
    #[serde(default = "default_word_goal")]
    pub daily_word_goal: u32,
    /// Persisted state, not user-set: words written so far today.
    #[serde(default)]
    pub today_words: u32,
    /// Persisted state, ISO date (YYYY-MM-DD) of the day today_words was last
    /// incremented. On startup, if this is older than today, today_words
    /// resets to 0 before any tracking resumes.
    #[serde(default)]
    pub last_active_date: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WrapMode {
    Word,
    Char,
    None,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            font_family: "Inter".to_string(),
            font_size: 16,
            line_spacing: 6,
            show_line_numbers: true,
            wrap_mode: WrapMode::Word,
            content_width: 800,
            vault_root: None,
            vault_sidebar_shown: false,
            recent_files: Vec::new(),
            restore_session: true,
            last_file: None,
            last_view_mode: None,
            last_scroll_line: 0,
            open_files_in_new_window: true,
            spell_check: false,
            spell_check_language: String::new(),
            distraction_free_fullscreen: true,
            focus_mode: false,
            typewriter_mode: false,
            auto_save: false,
            rich_tables: true,
            keep_line_breaks: false,
            max_image_width: default_max_image_width(),
            export_font: default_export_font(),
            export_margin_top_mm: default_export_margin_mm(),
            export_margin_right_mm: default_export_margin_mm(),
            export_margin_bottom_mm: default_export_margin_mm(),
            export_margin_left_mm: default_export_margin_mm(),
            show_footer: false,
            writing_stats: false,
            daily_word_goal: default_word_goal(),
            today_words: 0,
            last_active_date: String::new(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let path = config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        } else {
            Self::default()
        }
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, data);
        }
    }

    /// Add a path to the recent files list (max 10 entries).
    pub fn add_recent(&mut self, path: &str) {
        self.recent_files.retain(|p| p != path);
        self.recent_files.insert(0, path.to_string());
        self.recent_files.truncate(10);
    }

    /// Remove a path from the recent files list.
    pub fn remove_recent(&mut self, path: &str) {
        self.recent_files.retain(|p| p != path);
    }

    pub fn gtk_wrap_mode(&self) -> gtk::WrapMode {
        match self.wrap_mode {
            WrapMode::Word => gtk::WrapMode::Word,
            WrapMode::Char => gtk::WrapMode::Char,
            WrapMode::None => gtk::WrapMode::None,
        }
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("io.gitlab.ilshat_apps")
        .join("manuscript.json")
}

fn default_content_width() -> u32 {
    800
}

/// Default cap for images the document doesn't size. 900 px reads well inside
/// the default 800 px content column and matches the width of a typical
/// screenshot; larger images scale down to it, smaller ones stay put.
fn default_max_image_width() -> u32 {
    900
}

fn default_true() -> bool {
    true
}

fn default_export_font() -> String {
    "Serif 11".to_string()
}

fn default_export_margin_mm() -> f64 {
    20.0
}

fn default_word_goal() -> u32 {
    500
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_without_vault_fields_uses_defaults() {
        // A legacy config JSON that predates the vault fields.
        let json = r#"{
            "font_family": "Inter",
            "font_size": 12,
            "line_spacing": 4,
            "show_line_numbers": false,
            "wrap_mode": "Word"
        }"#;
        let cfg: AppConfig = serde_json::from_str(json).expect("legacy config must load");
        assert_eq!(cfg.vault_root, None);
        assert_eq!(cfg.vault_sidebar_shown, false);
    }
}
