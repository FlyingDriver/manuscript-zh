//! Quick file open dialog (Ctrl+Shift+O).
//!
//! Lists recent files and any markdown files sitting next to the currently
//! open document, filtered by a fuzzy substring match. The first matching row
//! is activated on Enter so the keyboard flow is: open the dialog, type, hit
//! Enter — no mouse required.

use adw::prelude::*;
use gtk::glib;
use std::path::{Path, PathBuf};

/// Build candidate list: recent files + sibling `.md` files of the active
/// document. De-duplicated, missing files dropped, ordered by recents-first.
fn collect_candidates(recents: &[String], current_file: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for r in recents {
        let p = PathBuf::from(r);
        if !p.exists() {
            continue;
        }
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }

    if let Some(cur) = current_file {
        if let Some(parent) = cur.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                let mut siblings: Vec<PathBuf> = Vec::new();
                for e in entries.flatten() {
                    let p = e.path();
                    if !p.is_file() {
                        continue;
                    }
                    let ext_md = p
                        .extension()
                        .and_then(|s| s.to_str())
                        .map(|s| s.eq_ignore_ascii_case("md") || s.eq_ignore_ascii_case("markdown"))
                        .unwrap_or(false);
                    if !ext_md {
                        continue;
                    }
                    if seen.insert(p.clone()) {
                        siblings.push(p);
                    }
                }
                siblings.sort();
                out.extend(siblings);
            }
        }
    }

    out
}

/// Case-insensitive subsequence match — every character of `query` must appear
/// in `target` in order. Empty query matches everything. Returns `true` on hit.
fn fuzzy_match(query: &str, target: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut qi = 0;
    for ch in target.chars().flat_map(|c| c.to_lowercase()) {
        if qi < q.len() && ch == q[qi] {
            qi += 1;
            if qi == q.len() {
                return true;
            }
        }
    }
    qi == q.len()
}

/// Present the quick-open dialog. `on_pick` is called with the chosen path
/// after the dialog closes.
pub fn present<F>(parent: &impl IsA<gtk::Widget>, recents: &[String], current_file: Option<&Path>, on_pick: F)
where
    F: Fn(PathBuf) + 'static,
{
    let candidates = collect_candidates(recents, current_file);

    let dialog = adw::Dialog::builder()
        .title("快速打开")
        .content_width(560)
        .content_height(480)
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    toolbar.add_top_bar(&header);

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let entry = gtk::SearchEntry::builder()
        .placeholder_text("输入以筛选…")
        .hexpand(true)
        .build();
    vbox.append(&entry);

    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    list.add_css_class("boxed-list");
    scroller.set_child(Some(&list));
    vbox.append(&scroller);

    toolbar.set_content(Some(&vbox));
    dialog.set_child(Some(&toolbar));

    let candidates_rc = std::rc::Rc::new(candidates);
    let on_pick_rc: std::rc::Rc<dyn Fn(PathBuf)> = std::rc::Rc::new(on_pick);

    // Initial population
    rebuild_list(&list, &candidates_rc, "");

    // Filter on each keystroke
    let list_clone = list.clone();
    let cand_clone = candidates_rc.clone();
    entry.connect_search_changed(move |e| {
        rebuild_list(&list_clone, &cand_clone, &e.text());
    });

    // Enter activates first row
    let list_for_activate = list.clone();
    entry.connect_activate(move |_| {
        if let Some(row) = list_for_activate.row_at_index(0) {
            row.activate();
        }
    });

    // Row activated → fire on_pick + close
    let dialog_for_pick = dialog.clone();
    let on_pick_for_row = on_pick_rc.clone();
    list.connect_row_activated(move |_, row| {
        if let Some(path_str) = unsafe { row.data::<String>("manuscript-path") } {
            let path = unsafe { PathBuf::from(path_str.as_ref().clone()) };
            on_pick_for_row(path);
        }
        dialog_for_pick.close();
    });

    // Esc closes — Dialog handles by default. Down-arrow from entry moves focus to list.
    let key_ctrl = gtk::EventControllerKey::new();
    let list_for_key = list.clone();
    key_ctrl.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk::gdk::Key::Down {
            list_for_key.grab_focus();
            if let Some(row) = list_for_key.row_at_index(0) {
                list_for_key.select_row(Some(&row));
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    entry.add_controller(key_ctrl);

    dialog.present(Some(parent));
    entry.grab_focus();
}

fn rebuild_list(list: &gtk::ListBox, candidates: &[PathBuf], query: &str) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let q = query.trim();
    for path in candidates {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let parent_str = crate::window::display_dir(&path);

        let haystack = format!("{} {}", name, parent_str);
        if !fuzzy_match(q, &haystack) {
            continue;
        }

        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&name))
            .subtitle(glib::markup_escape_text(&parent_str))
            .activatable(true)
            .build();
        unsafe {
            row.set_data("manuscript-path", path.to_string_lossy().to_string());
        }
        list.append(&row);
    }
    // Select first row so Enter has a target
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
    }
}
