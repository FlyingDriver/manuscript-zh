use adw::prelude::*;
use gtk::glib;

use crate::config::{AppConfig, WrapMode};

/// Show the preferences dialog.
pub fn show_preferences(
    parent: &impl IsA<gtk::Widget>,
    config: &AppConfig,
    on_save: impl Fn(AppConfig) + 'static,
) {
    let dialog = adw::PreferencesDialog::builder()
        .title("首选项")
        .search_enabled(false)
        .build();

    let config = config.clone();
    let recent_files = config.recent_files.clone();

    // -- Appearance group --
    let appearance_group = adw::PreferencesGroup::builder()
        .title("外观")
        .build();

    // Font family
    let font_row = adw::ActionRow::builder()
        .title("字体")
        .build();
    let font_btn = gtk::FontButton::new();
    let font_desc = format!("{} {}", config.font_family, config.font_size);
    font_btn.set_font(&font_desc);
    font_btn.set_use_font(true);
    font_btn.set_valign(gtk::Align::Center);
    font_row.add_suffix(&font_btn);
    appearance_group.add(&font_row);

    // Line spacing
    let spacing_row = adw::SpinRow::builder()
        .title("行距")
        .subtitle("行间像素")
        .adjustment(&gtk::Adjustment::new(
            config.line_spacing as f64,
            0.0,
            24.0,
            1.0,
            4.0,
            0.0,
        ))
        .build();
    appearance_group.add(&spacing_row);

    // Content width
    let width_row = adw::SpinRow::builder()
        .title("内容宽度")
        .subtitle("文本最大宽度（像素，0 表示不限制）")
        .adjustment(&gtk::Adjustment::new(
            config.content_width as f64,
            0.0,
            2000.0,
            50.0,
            100.0,
            0.0,
        ))
        .build();
    appearance_group.add(&width_row);

    // -- Editor group --
    let editor_group = adw::PreferencesGroup::builder()
        .title("编辑器")
        .build();

    // Show line numbers
    let line_numbers_row = adw::SwitchRow::builder()
        .title("显示行号")
        .active(config.show_line_numbers)
        .build();
    editor_group.add(&line_numbers_row);

    // Restore session
    let restore_row = adw::SwitchRow::builder()
        .title("恢复上次会话")
        .subtitle("启动时重新打开上次的文件、视图模式和滚动位置")
        .active(config.restore_session)
        .build();
    editor_group.add(&restore_row);

    // Open files in new window
    let new_window_row = adw::SwitchRow::builder()
        .title("在新窗口中打开文件")
        .subtitle("从文件管理器打开的每个文件都使用独立窗口")
        .active(config.open_files_in_new_window)
        .build();
    editor_group.add(&new_window_row);

    // Show footer
    let footer_row = adw::SwitchRow::builder()
        .title("显示页脚")
        .subtitle("底部栏显示字数和阅读时间")
        .active(config.show_footer)
        .build();
    editor_group.add(&footer_row);

    // Spell check
    let spell_row = adw::SwitchRow::builder()
        .title("拼写检查")
        .subtitle("在编辑器中给拼错的单词加下划线")
        .active(config.spell_check)
        .build();
    editor_group.add(&spell_row);

    // Spell check language — list installed hunspell/myspell dictionaries from
    // the standard search paths. libspelling-rs 0.3 bindings for list_languages
    // are ABI-incompatible with the newer system libspelling, so we scan the
    // dictionary directory directly instead.
    let lang_codes: Vec<String> = list_installed_dictionaries();
    let spell_lang_row = adw::ComboRow::builder()
        .title("拼写检查语言")
        .subtitle("已安装的词典")
        .build();
    let lang_model = gtk::StringList::new(&[]);
    lang_model.append("Default (system)");
    let mut selected_idx: u32 = 0;
    for (idx, code) in lang_codes.iter().enumerate() {
        lang_model.append(code);
        if !config.spell_check_language.is_empty() && code == &config.spell_check_language {
            selected_idx = (idx + 1) as u32;
        }
    }
    spell_lang_row.set_model(Some(&lang_model));
    spell_lang_row.set_selected(selected_idx);
    editor_group.add(&spell_lang_row);

    // -- Writer group --
    let writer_group = adw::PreferencesGroup::builder()
        .title("写作")
        .build();

    let df_row = adw::SwitchRow::builder()
        .title("免打扰全屏")
        .subtitle("全屏时隐藏标题栏、大纲、搜索栏和滚动条 (F11)")
        .active(config.distraction_free_fullscreen)
        .build();
    writer_group.add(&df_row);

    let focus_row = adw::SwitchRow::builder()
        .title("专注模式")
        .subtitle("在编辑器中淡化当前段落以外的文字")
        .active(config.focus_mode)
        .build();
    writer_group.add(&focus_row);

    let typewriter_row = adw::SwitchRow::builder()
        .title("打字机模式")
        .subtitle("让当前行保持在视口中央附近")
        .active(config.typewriter_mode)
        .build();
    writer_group.add(&typewriter_row);

    let auto_save_row = adw::SwitchRow::builder()
        .title("自动保存")
        .subtitle("最后一次编辑约 1.5 秒后自动保存当前文件")
        .active(config.auto_save)
        .build();
    writer_group.add(&auto_save_row);

    let rich_tables_row = adw::SwitchRow::builder()
        .title("富文本表格")
        .subtitle("以带边框的网格渲染表格并支持单元格内格式。关闭后使用纯文本表格，其单元格可搜索、可复制。")
        .active(config.rich_tables)
        .build();
    writer_group.add(&rich_tables_row);

    let line_breaks_row = adw::SwitchRow::builder()
        .title("保留单个换行")
        .subtitle("把段落内的单个换行显示为换行。关闭后各行会合并为一个段落，与 GitHub 的行为一致。")
        .active(config.keep_line_breaks)
        .build();
    writer_group.add(&line_breaks_row);

    // Default image width cap — documents that size an image themselves
    // (`![alt](pic.png){width=640}` or `<img width=...>`) override it.
    let img_width_row = adw::SpinRow::with_range(120.0, 4096.0, 20.0);
    img_width_row.set_title("图片最大宽度");
    img_width_row.set_subtitle("未指定尺寸的图片按此上限缩放；单张图片可用 {width=640} 或 <img width=\"640\"> 单独覆盖");
    img_width_row.set_value(config.max_image_width as f64);
    writer_group.add(&img_width_row);

    // Writing statistics — gated on Show footer
    let stats_row = adw::SwitchRow::builder()
        .title("写作统计")
        .subtitle("页脚中的每日字数目标")
        .active(config.writing_stats)
        .build();
    writer_group.add(&stats_row);

    // Daily word goal — gated on Writing statistics
    let goal_row = adw::SpinRow::builder()
        .title("每日字数目标")
        .subtitle("0 = 隐藏目标，仅统计字数")
        .adjustment(&gtk::Adjustment::new(
            config.daily_word_goal as f64,
            0.0,
            5000.0,
            50.0,
            100.0,
            0.0,
        ))
        .digits(0)
        .build();
    writer_group.add(&goal_row);

    // Initial sensitivity + live cascade
    stats_row.set_sensitive(config.show_footer);
    goal_row.set_sensitive(config.show_footer && config.writing_stats);
    footer_row.connect_active_notify(glib::clone!(
        #[weak]
        stats_row,
        #[weak]
        goal_row,
        move |row| {
            let on = row.is_active();
            stats_row.set_sensitive(on);
            goal_row.set_sensitive(on && stats_row.is_active());
        }
    ));
    stats_row.connect_active_notify(glib::clone!(
        #[weak]
        goal_row,
        #[weak]
        footer_row,
        move |row| {
            goal_row.set_sensitive(footer_row.is_active() && row.is_active());
        }
    ));

    // Wrap mode
    let wrap_row = adw::ComboRow::builder()
        .title("自动换行")
        .build();
    let wrap_options = gtk::StringList::new(&["Word", "Character", "None"]);
    wrap_row.set_model(Some(&wrap_options));
    wrap_row.set_selected(match config.wrap_mode {
        WrapMode::Word => 0,
        WrapMode::Char => 1,
        WrapMode::None => 2,
    });
    editor_group.add(&wrap_row);

    // -- PDF / Print export group --
    let export_group = adw::PreferencesGroup::builder()
        .title("PDF / 打印导出")
        .description("打印或导出 PDF 时使用的字体与页边距")
        .build();

    let export_font_btn = gtk::FontButton::builder()
        .use_font(true)
        .use_size(true)
        .build();
    export_font_btn.set_font(&config.export_font);
    let export_font_row = adw::ActionRow::builder().title("字体").build();
    export_font_row.add_suffix(&export_font_btn);
    export_font_row.set_activatable_widget(Some(&export_font_btn));
    export_group.add(&export_font_row);

    let margin_top_row = adw::SpinRow::builder()
        .title("上边距（毫米）")
        .adjustment(&gtk::Adjustment::new(
            config.export_margin_top_mm,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .digits(0)
        .build();
    export_group.add(&margin_top_row);

    let margin_right_row = adw::SpinRow::builder()
        .title("右边距（毫米）")
        .adjustment(&gtk::Adjustment::new(
            config.export_margin_right_mm,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .digits(0)
        .build();
    export_group.add(&margin_right_row);

    let margin_bottom_row = adw::SpinRow::builder()
        .title("下边距（毫米）")
        .adjustment(&gtk::Adjustment::new(
            config.export_margin_bottom_mm,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .digits(0)
        .build();
    export_group.add(&margin_bottom_row);

    let margin_left_row = adw::SpinRow::builder()
        .title("左边距（毫米）")
        .adjustment(&gtk::Adjustment::new(
            config.export_margin_left_mm,
            0.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .digits(0)
        .build();
    export_group.add(&margin_left_row);

    // -- Page --
    let page = adw::PreferencesPage::builder()
        .title("通用")
        .icon_name("preferences-system-symbolic")
        .build();
    page.add(&appearance_group);
    page.add(&editor_group);
    page.add(&writer_group);
    page.add(&export_group);
    dialog.add(&page);

    // Save on close
    let font_btn_ref = font_btn.clone();
    dialog.connect_closed(move |_| {
        let font_str = font_btn_ref.font().unwrap_or_default().to_string();
        // Parse "Family Size" from font string
        let desc = gtk::pango::FontDescription::from_string(&font_str);
        let family = desc
            .family()
            .map(|f| f.to_string())
            .unwrap_or_else(|| "Sans".to_string());
        let size = desc.size() / gtk::pango::SCALE;
        let size = if size <= 0 { 16 } else { size as u32 };

        let new_config = AppConfig {
            font_family: family,
            font_size: size,
            line_spacing: spacing_row.value() as u32,
            show_line_numbers: line_numbers_row.is_active(),
            wrap_mode: match wrap_row.selected() {
                1 => WrapMode::Char,
                2 => WrapMode::None,
                _ => WrapMode::Word,
            },
            content_width: width_row.value() as u32,
            vault_root: config.vault_root.clone(),
            vault_sidebar_shown: config.vault_sidebar_shown,
            recent_files: recent_files.clone(),
            restore_session: restore_row.is_active(),
            last_file: config.last_file.clone(),
            last_view_mode: config.last_view_mode.clone(),
            last_scroll_line: config.last_scroll_line,
            open_files_in_new_window: new_window_row.is_active(),
            spell_check: spell_row.is_active(),
            spell_check_language: {
                let sel = spell_lang_row.selected();
                if sel == 0 {
                    String::new()
                } else {
                    lang_codes
                        .get((sel - 1) as usize)
                        .cloned()
                        .unwrap_or_default()
                }
            },
            distraction_free_fullscreen: df_row.is_active(),
            focus_mode: focus_row.is_active(),
            typewriter_mode: typewriter_row.is_active(),
            auto_save: auto_save_row.is_active(),
            rich_tables: rich_tables_row.is_active(),
            keep_line_breaks: line_breaks_row.is_active(),
            max_image_width: img_width_row.value() as u32,
            export_font: export_font_btn.font().map(|s| s.to_string()).unwrap_or_else(default_export_font_str),
            export_margin_top_mm: margin_top_row.value(),
            export_margin_right_mm: margin_right_row.value(),
            export_margin_bottom_mm: margin_bottom_row.value(),
            export_margin_left_mm: margin_left_row.value(),
            show_footer: footer_row.is_active(),
            writing_stats: stats_row.is_active(),
            daily_word_goal: goal_row.value() as u32,
            today_words: config.today_words,
            last_active_date: config.last_active_date.clone(),
        };
        new_config.save();
        on_save(new_config);
    });

    dialog.present(Some(parent));
}

fn default_export_font_str() -> String {
    "Serif 11".to_string()
}

/// Scan standard hunspell/myspell dictionary paths for installed languages.
/// Returns codes like "en_US", "ru_RU", "de_DE" sorted alphabetically.
fn list_installed_dictionaries() -> Vec<String> {
    let candidates = [
        "/usr/share/hunspell",
        "/usr/share/myspell/dicts",
        "/app/share/hunspell",
        "/app/share/myspell/dicts",
    ];
    let mut codes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for dir in candidates {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("dic") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                codes.insert(stem.to_string());
            }
        }
    }
    codes.into_iter().collect()
}
