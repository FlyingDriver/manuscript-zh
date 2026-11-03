//! Print rendering: markdown → Pango markup → paginated GtkPrintOperation.
//!
//! Previous implementation printed raw monospace text. This version walks the
//! pulldown-cmark event stream and emits Pango markup so headings, bold/italic,
//! code, lists, blockquotes, links and tables retain their formatting.
//!
//! Pagination: the full document is laid out into one PangoLayout, then iterated
//! line by line to find page break offsets. `draw_page` translates the cairo
//! origin so the slice for that page falls within the printable area and clips
//! the rest.

use gtk::prelude::*;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::cell::RefCell;
use std::rc::Rc;

const PANGO_SCALE: f64 = gtk::pango::SCALE as f64;
const PAGE_GUTTER_PT: f64 = 16.0;

/// Cell horizontal/vertical padding for cairo-drawn tables, in points.
const TABLE_CELL_H_PAD: f64 = 6.0;
const TABLE_CELL_V_PAD: f64 = 4.0;
/// Vertical breathing room above and below a table block.
const TABLE_BLOCK_MARGIN_PT: f64 = 12.0;

/// Sentinel codepoints used to mark table-block boundaries in the Pango markup
/// stream. U+E000..U+F8FF is the Unicode private-use area — these never appear
/// in real document text. Format: `\u{E000}<decimal-index>\u{E001}`. After
/// pagination splits the markup at these sentinels, the index resolves to a
/// `TableBlock` in `PrintState::tables`.
const TABLE_SENTINEL_OPEN: char = '\u{E000}';
const TABLE_SENTINEL_CLOSE: char = '\u{E001}';

/// One table captured from the markdown stream. Each cell holds Pango markup
/// (inline `<b>`, `<i>`, `<tt>`, `<a>` tags allowed) that is fed directly to
/// `pango::Layout::set_markup` at draw time.
struct TableBlock {
    columns: usize,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// One drawable unit on a printed page.
enum PageBlock {
    /// A slice of a text Pango layout. `layout_idx` selects which layout in
    /// `PrintState::text_layouts`; `layout_offset` is the y-offset (points)
    /// at the top of the slice; `clip_height` is the slice's height. For
    /// text-only documents (no tables) `layout_idx` is unused — that path
    /// keeps using the legacy `PrintState::layout` field.
    Text {
        layout_idx: usize,
        layout_offset: f64,
        clip_height: f64,
    },
    /// A (possibly partial) table. `body_offset_pts` is the y-distance (within
    /// the table's own coordinate space, header at y=0) at which this slice
    /// begins; `height_pts` is the slice's height (body only when
    /// `repeat_header` is true — the header row is drawn in addition).
    ///
    /// `repeat_header` is set on every continuation slice of a table that was
    /// split across pages. The pagination pass uses `false` for the very first
    /// slice (the header is already part of body via `body_offset_pts: 0.0`)
    /// and `true` for every following slice — the cairo draw then prepends
    /// row 0 (header) at the top of the slice and shifts the body rows down
    /// by header_h.
    Table {
        table_index: usize,
        body_offset_pts: f64,
        height_pts: f64,
        repeat_header: bool,
    },
}

/// Page + font settings shared by print and PDF export. Margins are in
/// millimetres because that's the unit users expect; we convert internally.
#[derive(Clone)]
pub struct ExportOptions {
    pub font: String,
    pub margin_top_mm: f64,
    pub margin_right_mm: f64,
    pub margin_bottom_mm: f64,
    pub margin_left_mm: f64,
    /// Render single newlines inside a paragraph as line breaks instead of
    /// spaces (mirrors `AppConfig::keep_line_breaks`).
    pub keep_line_breaks: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            font: "Serif 11".to_string(),
            margin_top_mm: 20.0,
            margin_right_mm: 20.0,
            margin_bottom_mm: 20.0,
            margin_left_mm: 20.0,
            keep_line_breaks: false,
        }
    }
}

struct PrintState {
    /// Full Pango markup (with `\u{E000}IDX\u{E001}` sentinels at table sites).
    markup: String,
    /// Captured tables, indexed by the sentinel value.
    tables: Vec<TableBlock>,
    /// Per-text-segment Pango layouts, indexed by segment id. Built in
    /// `connect_begin_print` and reused in `connect_draw_page` so we don't
    /// re-layout text for every page.
    text_layouts: Vec<gtk::pango::Layout>,
    /// One `Vec<PageBlock>` per page. Drives `draw_page`.
    page_blocks: Vec<Vec<PageBlock>>,
    page_height: f64,
    /// Printable page width in points (used by `draw_table_block`).
    page_width: f64,
    /// Font description shared by table-cell Pango layouts.
    font_desc: Option<gtk::pango::FontDescription>,

    // --- Legacy text-only layout state (kept private so existing call sites
    //     still compile during the migration). For text-only documents these
    //     drive the same code path as before; they are not consulted by the
    //     new `page_blocks` draw path.
    layout: Option<gtk::pango::Layout>,
    page_offsets: Vec<f64>,
    page_clip_heights: Vec<f64>,
}

pub fn print_markdown(
    parent: &impl IsA<gtk::Window>,
    markdown: &str,
    title: Option<&str>,
    opts: &ExportOptions,
) {
    run_print_op(parent, markdown, title, None, opts);
}

/// Export the document as a PDF to `dest_path`. Reuses the same Pango layout
/// pipeline as `print_markdown`, but routes GtkPrintOperation in `Export` mode
/// so it writes a file instead of opening the print dialog.
pub fn export_pdf(
    parent: &impl IsA<gtk::Window>,
    markdown: &str,
    title: Option<&str>,
    dest_path: &std::path::Path,
    opts: &ExportOptions,
) {
    run_print_op(parent, markdown, title, Some(dest_path), opts);
}

fn run_print_op(
    parent: &impl IsA<gtk::Window>,
    markdown: &str,
    title: Option<&str>,
    export_to: Option<&std::path::Path>,
    opts: &ExportOptions,
) {
    if markdown.is_empty() {
        return;
    }

    let (markup, tables) = build_markup(markdown, opts.keep_line_breaks);

    let state = Rc::new(RefCell::new(PrintState {
        markup,
        tables,
        text_layouts: Vec::new(),
        page_blocks: Vec::new(),
        page_height: 0.0,
        page_width: 0.0,
        font_desc: None,
        layout: None,
        page_offsets: Vec::new(),
        page_clip_heights: Vec::new(),
    }));

    let print_op = gtk::PrintOperation::new();
    print_op.set_unit(gtk::Unit::Points);
    if let Some(t) = title {
        print_op.set_job_name(t);
    }
    {
        // Apply page margins from ExportOptions regardless of mode — the
        // print dialog will start with the user's preferred margins, and PDF
        // export honors them directly.
        let page_setup = gtk::PageSetup::new();
        let paper = gtk::PaperSize::new(Some("iso_a4"));
        page_setup.set_paper_size(&paper);
        page_setup.set_top_margin(opts.margin_top_mm, gtk::Unit::Mm);
        page_setup.set_right_margin(opts.margin_right_mm, gtk::Unit::Mm);
        page_setup.set_bottom_margin(opts.margin_bottom_mm, gtk::Unit::Mm);
        page_setup.set_left_margin(opts.margin_left_mm, gtk::Unit::Mm);
        print_op.set_default_page_setup(Some(&page_setup));
    }
    if let Some(p) = export_to {
        print_op.set_export_filename(p);
    }

    let font_str = opts.font.clone();
    let state_b = state.clone();
    print_op.connect_begin_print(move |op, context| {
        let font = gtk::pango::FontDescription::from_string(&font_str);
        let width_pu = (context.width() * PANGO_SCALE) as i32;
        let page_h = context.height() - PAGE_GUTTER_PT;
        let page_w = context.width();

        let (markup, has_tables) = {
            let st = state_b.borrow();
            (st.markup.clone(), !st.tables.is_empty())
        };

        // Fast path: no tables in the document. Behaviour byte-identical to the
        // pre-Phase-4 code — one global Pango layout, line-walk pagination,
        // legacy page_offsets / page_clip_heights drive draw_page.
        if !has_tables {
            let layout = context.create_pango_layout();
            layout.set_font_description(Some(&font));
            layout.set_width(width_pu);
            layout.set_wrap(gtk::pango::WrapMode::WordChar);
            layout.set_markup(&markup);

            let mut offsets = vec![0.0_f64];
            let mut last_fit_bottoms: Vec<f64> = Vec::new();
            let mut last_fit_bottom: f64 = 0.0;
            let mut total_bottom: f64;
            let mut iter = layout.iter();
            loop {
                let log_rect = iter.line_extents().1;
                let line_top_pt = log_rect.y() as f64 / PANGO_SCALE;
                let line_bottom_pt = (log_rect.y() + log_rect.height()) as f64 / PANGO_SCALE;
                total_bottom = line_bottom_pt;
                let cur_offset = offsets.last().copied().unwrap_or(0.0);
                if line_bottom_pt - cur_offset > page_h {
                    last_fit_bottoms.push(last_fit_bottom - cur_offset);
                    offsets.push(line_top_pt);
                    last_fit_bottom = line_bottom_pt;
                } else {
                    last_fit_bottom = line_bottom_pt;
                }
                if !iter.next_line() {
                    break;
                }
            }
            let last_off = offsets.last().copied().unwrap_or(0.0);
            last_fit_bottoms.push(total_bottom - last_off);

            op.set_n_pages(offsets.len() as i32);

            let mut st = state_b.borrow_mut();
            st.layout = Some(layout);
            st.page_offsets = offsets;
            st.page_clip_heights = last_fit_bottoms;
            st.page_height = page_h;
            st.page_width = page_w;
            st.font_desc = Some(font);
            st.text_layouts.clear();
            st.page_blocks.clear();
            return;
        }

        // Mixed text / table path. Split markup at sentinels, build one layout
        // per text segment, then walk segments + tables in order, packing into
        // pages.
        let segments = split_markup_at_sentinels(&markup);
        let mut text_layouts: Vec<gtk::pango::Layout> = Vec::new();
        // For each markup segment we keep either a layout index (text) or a
        // table index. Same order as `segments`.
        enum Seg {
            Text(usize),
            Table(usize),
        }
        let mut seg_kinds: Vec<Seg> = Vec::with_capacity(segments.len());
        for seg in &segments {
            match seg {
                MarkupSegment::Text(s) => {
                    let layout = context.create_pango_layout();
                    layout.set_font_description(Some(&font));
                    layout.set_width(width_pu);
                    layout.set_wrap(gtk::pango::WrapMode::WordChar);
                    layout.set_markup(s);
                    seg_kinds.push(Seg::Text(text_layouts.len()));
                    text_layouts.push(layout);
                }
                MarkupSegment::Table(idx) => {
                    seg_kinds.push(Seg::Table(*idx));
                }
            }
        }

        // Pre-compute table heights using a temp Pango context so pagination
        // doesn't need to lay out cells twice.
        let tables_snapshot: Vec<TableLayoutInfo> = {
            let st = state_b.borrow();
            st.tables
                .iter()
                .map(|t| measure_table(t, page_w, &font, context))
                .collect()
        };

        let mut pages: Vec<Vec<PageBlock>> = vec![Vec::new()];
        let mut cursor_y: f64 = 0.0;

        for seg in &seg_kinds {
            match seg {
                Seg::Text(layout_idx) => {
                    let layout = &text_layouts[*layout_idx];
                    // Walk lines; whenever a line would overflow the page, emit
                    // the current Text block, push a new page, and continue.
                    let mut iter = layout.iter();
                    let mut segment_start_y: f64 = 0.0; // y within this layout
                    let mut last_fit_bottom: f64 = 0.0;
                    let mut have_any_line = false;
                    loop {
                        let log_rect = iter.line_extents().1;
                        let line_top_pt = log_rect.y() as f64 / PANGO_SCALE;
                        let line_bottom_pt =
                            (log_rect.y() + log_rect.height()) as f64 / PANGO_SCALE;
                        let line_h = line_bottom_pt - line_top_pt;

                        if cursor_y + line_h > page_h {
                            // Close out current text block on the current page.
                            if have_any_line {
                                pages.last_mut().unwrap().push(PageBlock::Text {
                                    layout_idx: *layout_idx,
                                    layout_offset: segment_start_y,
                                    clip_height: last_fit_bottom - segment_start_y,
                                });
                            }
                            // Start a new page; this line begins it.
                            pages.push(Vec::new());
                            cursor_y = 0.0;
                            segment_start_y = line_top_pt;
                            last_fit_bottom = line_bottom_pt;
                            cursor_y += line_h;
                            have_any_line = true;
                        } else {
                            if !have_any_line {
                                segment_start_y = line_top_pt;
                                have_any_line = true;
                            }
                            last_fit_bottom = line_bottom_pt;
                            cursor_y += line_h;
                        }

                        if !iter.next_line() {
                            break;
                        }
                    }
                    if have_any_line {
                        pages.last_mut().unwrap().push(PageBlock::Text {
                            layout_idx: *layout_idx,
                            layout_offset: segment_start_y,
                            clip_height: last_fit_bottom - segment_start_y,
                        });
                    }
                }
                Seg::Table(table_idx) => {
                    let info = &tables_snapshot[*table_idx];
                    // Add top margin if this table isn't at the top of the page.
                    if cursor_y > 0.0 {
                        cursor_y += TABLE_BLOCK_MARGIN_PT;
                        if cursor_y > page_h {
                            pages.push(Vec::new());
                            cursor_y = 0.0;
                        }
                    }
                    let total_height = info.total_height();
                    if cursor_y + total_height <= page_h {
                        // Fits on current page in one piece.
                        pages.last_mut().unwrap().push(PageBlock::Table {
                            table_index: *table_idx,
                            body_offset_pts: 0.0,
                            height_pts: total_height,
                            repeat_header: false,
                        });
                        cursor_y += total_height + TABLE_BLOCK_MARGIN_PT;
                    } else {
                        // Row-by-row split. Walk rows; when one wouldn't fit,
                        // emit the partial slice and start a new page. The
                        // FIRST slice carries the header naturally as row 0
                        // (`body_offset_pts = 0.0`, `repeat_header = false`).
                        // Continuation slices set `repeat_header = true` and
                        // their `body_offset_pts` points at the next body
                        // row's cumulative top; the page budget reserves
                        // header_h up front because the header is drawn in
                        // addition to the slice's body height.
                        let header_h = info.header_height();
                        let n_rows = info.row_heights.len();
                        // `slice_start_row` is the row index at which the
                        // current slice begins. For the first slice this is
                        // 0 (so the header sits naturally at the top); for
                        // continuation slices it's the first body row not
                        // yet drawn (we never set it to 0 again — the
                        // header is repeated by the draw path, not by the
                        // body offset).
                        let mut slice_start_row: usize = 0;
                        let mut is_continuation = false;
                        let mut row_idx: usize = 0;
                        let mut slice_body_h: f64 = 0.0;

                        // Pre-compute cumulative row tops so we can map row
                        // index -> y offset cheaply for body_offset_pts.
                        let mut row_tops: Vec<f64> = Vec::with_capacity(n_rows + 1);
                        row_tops.push(0.0);
                        for rh in &info.row_heights {
                            row_tops.push(row_tops.last().copied().unwrap_or(0.0) + rh);
                        }

                        while row_idx < n_rows {
                            let rh = info.row_heights[row_idx];
                            // Available height for body rows on the current
                            // page. For a continuation slice, the header
                            // consumes header_h at the top of the page.
                            let header_reserve = if is_continuation { header_h } else { 0.0 };
                            let available = page_h - cursor_y - header_reserve;
                            if slice_body_h + rh > available {
                                // Orphan-header guard for the first slice:
                                // if only the header (row 0) fit and the very
                                // next body row wouldn't, drop the orphan and
                                // start the whole table on the next page so
                                // the header co-renders with body row 1.
                                let only_header =
                                    !is_continuation && row_idx == 1 && slice_start_row == 0;
                                let emit = slice_body_h > 0.0 && !only_header;
                                if emit {
                                    let body_offset_pts = if is_continuation {
                                        row_tops[slice_start_row]
                                    } else {
                                        0.0
                                    };
                                    pages.last_mut().unwrap().push(PageBlock::Table {
                                        table_index: *table_idx,
                                        body_offset_pts,
                                        height_pts: slice_body_h,
                                        repeat_header: is_continuation,
                                    });
                                    // Now on a continuation page; reserve
                                    // header_h.
                                    slice_start_row = row_idx;
                                    is_continuation = true;
                                } else if only_header {
                                    // Restart the table from row 0 on the
                                    // next page — non-continuation; header
                                    // belongs to the first slice naturally.
                                    row_idx = 0;
                                }
                                pages.push(Vec::new());
                                cursor_y = 0.0;
                                slice_body_h = 0.0;
                                continue;
                            }
                            slice_body_h += rh;
                            row_idx += 1;
                        }
                        if slice_body_h > 0.0 {
                            let body_offset_pts = if is_continuation {
                                row_tops[slice_start_row]
                            } else {
                                0.0
                            };
                            pages.last_mut().unwrap().push(PageBlock::Table {
                                table_index: *table_idx,
                                body_offset_pts,
                                height_pts: slice_body_h,
                                repeat_header: is_continuation,
                            });
                            let consumed = if is_continuation {
                                header_h + slice_body_h
                            } else {
                                slice_body_h
                            };
                            cursor_y += consumed + TABLE_BLOCK_MARGIN_PT;
                        }
                    }
                }
            }
        }

        // Drop trailing empty page (can happen if a table-row-by-row split
        // closes out exactly at the page boundary).
        if pages.last().is_some_and(|p| p.is_empty()) && pages.len() > 1 {
            pages.pop();
        }

        op.set_n_pages(pages.len() as i32);

        let mut st = state_b.borrow_mut();
        st.text_layouts = text_layouts;
        st.page_blocks = pages;
        st.page_height = page_h;
        st.page_width = page_w;
        st.font_desc = Some(font);
        // Legacy fields stay empty for the table path.
        st.layout = None;
        st.page_offsets.clear();
        st.page_clip_heights.clear();
    });

    let state_d = state.clone();
    print_op.connect_draw_page(move |_, context, page_nr| {
        let st = state_d.borrow();
        let cairo = context.cairo_context();

        // Text-only fast path — preserved byte-identical from pre-Phase-4.
        if st.page_blocks.is_empty() {
            let Some(layout) = st.layout.as_ref() else { return };
            let offset = st.page_offsets.get(page_nr as usize).copied().unwrap_or(0.0);
            let clip_h = st
                .page_clip_heights
                .get(page_nr as usize)
                .copied()
                .unwrap_or(st.page_height)
                .min(st.page_height);

            cairo.save().ok();
            cairo.rectangle(0.0, 0.0, context.width(), clip_h);
            cairo.clip();
            cairo.translate(0.0, -offset);
            pangocairo::functions::show_layout(&cairo, layout);
            cairo.restore().ok();
            return;
        }

        // Mixed path. Replay block list, translating cairo origin as we go.
        let Some(blocks) = st.page_blocks.get(page_nr as usize) else { return };
        let Some(font) = st.font_desc.as_ref() else { return };
        let mut cursor_y: f64 = 0.0;
        for block in blocks {
            match block {
                PageBlock::Text { layout_idx, layout_offset, clip_height } => {
                    if let Some(layout) = st.text_layouts.get(*layout_idx) {
                        cairo.save().ok();
                        cairo.rectangle(0.0, cursor_y, context.width(), *clip_height);
                        cairo.clip();
                        cairo.translate(0.0, cursor_y - *layout_offset);
                        pangocairo::functions::show_layout(&cairo, layout);
                        cairo.restore().ok();
                    }
                    cursor_y += *clip_height;
                }
                PageBlock::Table {
                    table_index,
                    body_offset_pts,
                    height_pts,
                    repeat_header,
                } => {
                    // Match the top-of-table margin that pagination added when
                    // the table wasn't placed at y=0.
                    if cursor_y > 0.0 {
                        cursor_y += TABLE_BLOCK_MARGIN_PT;
                    }
                    // When the header is repeated on this slice, the draw
                    // path prepends row 0 above the body slice. The
                    // pagination pass already reserved header_h on this
                    // page, so we just need to advance cursor_y by the
                    // header height in addition to height_pts (which is
                    // body-only for repeat-header slices).
                    let mut header_h = 0.0;
                    if let Some(table) = st.tables.get(*table_index) {
                        if *repeat_header {
                            header_h = measure_table_header_height(
                                &cairo,
                                table,
                                st.page_width,
                                font,
                            );
                        }
                        draw_table_block(
                            &cairo,
                            table,
                            st.page_width,
                            cursor_y,
                            *body_offset_pts,
                            *height_pts,
                            font,
                            *repeat_header,
                        );
                    }
                    cursor_y += *height_pts + header_h + TABLE_BLOCK_MARGIN_PT;
                }
            }
        }
    });

    let action = if export_to.is_some() {
        gtk::PrintOperationAction::Export
    } else {
        gtk::PrintOperationAction::PrintDialog
    };
    let _ = print_op.run(action, Some(parent));
}

/// Convert a chunk of raw HTML produced by pulldown-cmark into Pango markup
/// suitable for the print layout. Recognises a small set of common tags
/// (h1–h6, img, br, p, a) — everything else has its tags stripped while the
/// inner text is preserved. Empty runs collapse to at most one blank line so
/// `<p align="center">…</p>` blocks containing only `<img>` don't leave huge
/// blank chasms in the rendered PDF.
fn render_html_for_print(html: &str) -> String {
    // Decode the handful of named entities that appear in README-style markdown.
    // Order matters only for `&amp;` — keep it last so we don't double-decode.
    let prepped = html
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");

    let bytes = prepped.as_bytes();
    let mut out = String::with_capacity(prepped.len());
    let mut i = 0usize;

    // Heading wrapping state: when we hit <h1..h6>, capture text until </hN>
    // and emit a sized Pango span. We track the open level here.
    let mut heading_level: Option<u32> = None;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            let Some(end_rel) = prepped[i..].find('>') else {
                break;
            };
            let tag_slice = &prepped[i + 1..i + end_rel];
            i += end_rel + 1;

            let lowered = tag_slice.to_ascii_lowercase();
            let lowered = lowered.trim_start_matches('/');
            // Drop attributes — keep only the tag name.
            let name = lowered
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("");
            let closing = tag_slice.trim_start().starts_with('/');

            match name {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level: u32 = name[1..].parse().unwrap_or(1);
                    if closing {
                        if heading_level.is_some() {
                            out.push_str("</span>");
                            heading_level = None;
                        }
                    } else {
                        let size = match level {
                            1 => "xx-large",
                            2 => "x-large",
                            3 => "large",
                            _ => "medium",
                        };
                        out.push_str(&format!(
                            "<span weight=\"bold\" size=\"{}\">",
                            size
                        ));
                        heading_level = Some(level);
                    }
                }
                // Drop <img>, <picture>, <source> entirely — image embedding
                // in Pango layout is non-trivial and a missing placeholder is
                // less ugly than a phantom blank gap.
                "img" | "picture" | "source" => {}
                _ => {}
            }
            continue;
        }

        // Copy literal text, escaping for Pango markup.
        let ch_end = prepped[i..]
            .char_indices()
            .nth(1)
            .map(|(j, _)| i + j)
            .unwrap_or(prepped.len());
        let ch = &prepped[i..ch_end];
        match ch {
            "&" => out.push_str("&amp;"),
            "<" => out.push_str("&lt;"),
            ">" => out.push_str("&gt;"),
            _ => out.push_str(ch),
        }
        i = ch_end;
    }

    if heading_level.is_some() {
        out.push_str("</span>");
    }

    // Collapse runs of 3+ newlines (often produced by stripped indented
    // `<p>\n  <img …>\n</p>` blocks) to a single blank line.
    let mut collapsed = String::with_capacity(out.len());
    let mut nl_run = 0;
    for ch in out.chars() {
        if ch == '\n' {
            nl_run += 1;
            if nl_run <= 2 {
                collapsed.push('\n');
            }
        } else if ch.is_whitespace() {
            // Drop pure-whitespace runs between newlines so blank lines don't
            // accumulate from indentation.
            collapsed.push(ch);
        } else {
            nl_run = 0;
            collapsed.push(ch);
        }
    }
    // Trim trailing whitespace/newlines on each side — block HTML should not
    // contribute leading or trailing blank lines beyond what surrounding
    // markdown blocks already provide.
    let trimmed = collapsed.trim_matches(|c: char| c == '\n').to_string();
    if trimmed.chars().all(|c| c.is_whitespace()) {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[derive(Clone, Copy)]
enum ListKind {
    Unordered,
    Ordered(u64),
}

struct Walker {
    out: String,
    list_stack: Vec<ListKind>,
    blockquote_depth: usize,
    in_code_block: bool,
    pending_item_marker: bool,
    /// Captured tables. Emitted into the markup stream as
    /// `\u{E000}<idx>\u{E001}` sentinels that pagination splits on.
    tables: Vec<TableBlock>,
    /// Header row of the currently-being-built table, captured between
    /// `Start(TableHead)` and `End(TableHead)`.
    table_current_header: Option<Vec<String>>,
    /// Body rows of the currently-being-built table.
    table_current_body: Option<Vec<Vec<String>>>,
    /// Cells of the row currently being assembled.
    table_current_row: Option<Vec<String>>,
    /// In-progress cell markup buffer. While `Some`, `push_str` / `push_text`
    /// redirect into it instead of the main `out` stream.
    table_current_cell: Option<String>,
    /// Whether the row currently being built belongs to the table header.
    table_in_head: bool,
    in_heading: Option<HeadingLevel>,
}

impl Walker {
    fn new() -> Self {
        Self {
            out: String::new(),
            list_stack: Vec::new(),
            blockquote_depth: 0,
            in_code_block: false,
            pending_item_marker: false,
            tables: Vec::new(),
            table_current_header: None,
            table_current_body: None,
            table_current_row: None,
            table_current_cell: None,
            table_in_head: false,
            in_heading: None,
        }
    }

    fn enter_table(&mut self) {
        self.table_current_header = Some(Vec::new());
        self.table_current_body = Some(Vec::new());
        self.table_in_head = false;
    }

    fn end_table(&mut self) {
        let header = self.table_current_header.take().unwrap_or_default();
        let rows = self.table_current_body.take().unwrap_or_default();
        let columns = header.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        let idx = self.tables.len();
        self.tables.push(TableBlock { columns, header, rows });
        // Sentinel: opening PUA char, decimal index, closing PUA char. Pango
        // will treat the PUA codepoints as opaque characters (no glyphs) but
        // we strip them out before laying out any segment.
        self.out.push(TABLE_SENTINEL_OPEN);
        self.out.push_str(&idx.to_string());
        self.out.push(TABLE_SENTINEL_CLOSE);
        // Ensure following block content starts on a fresh line, matching the
        // ASCII path which always emitted "\n\n" after the table.
        self.out.push('\n');
    }

    fn push_str(&mut self, s: &str) {
        if let Some(cell) = self.table_current_cell.as_mut() {
            cell.push_str(s);
        } else {
            self.out.push_str(s);
        }
    }

    fn push_text(&mut self, t: &str) {
        let esc = escape(t);
        self.push_str(&esc);
    }

    fn newline(&mut self) {
        self.push_str("\n");
        self.write_blockquote_prefix();
    }

    fn write_blockquote_prefix(&mut self) {
        for _ in 0..self.blockquote_depth {
            self.push_str("│ ");
        }
    }

    fn flush_item_marker(&mut self) {
        if !self.pending_item_marker {
            return;
        }
        self.pending_item_marker = false;
        let depth = self.list_stack.len();
        // Indent two spaces per nesting level (relative to top list).
        if depth > 1 {
            self.push_str(&" ".repeat((depth - 1) * 2));
        }
        let marker = match self.list_stack.last_mut() {
            Some(ListKind::Unordered) => Some("• ".to_string()),
            Some(ListKind::Ordered(n)) => {
                let s = format!("{}. ", *n);
                *n += 1;
                Some(s)
            }
            None => None,
        };
        if let Some(m) = marker {
            self.push_str(&m);
        }
    }
}

fn heading_size_attr(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "xx-large",
        HeadingLevel::H2 => "x-large",
        HeadingLevel::H3 => "large",
        HeadingLevel::H4 => "medium",
        HeadingLevel::H5 => "medium",
        HeadingLevel::H6 => "medium",
    }
}

fn build_markup(markdown: &str, keep_line_breaks: bool) -> (String, Vec<TableBlock>) {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(markdown, options);
    let mut w = Walker::new();

    for event in parser {
        match event {
            Event::Start(tag) => start_tag(&mut w, &tag),
            Event::End(tag_end) => end_tag(&mut w, &tag_end),
            Event::Text(t) => {
                if w.in_code_block {
                    // preserve raw text inside <tt>
                    w.push_str(&escape(&t));
                } else {
                    w.flush_item_marker();
                    w.push_text(&t);
                }
            }
            Event::Code(code) => {
                w.flush_item_marker();
                w.push_str("<tt>");
                w.push_text(&code);
                w.push_str("</tt>");
            }
            Event::SoftBreak if keep_line_breaks => w.newline(),
            Event::SoftBreak => w.push_str(" "),
            Event::HardBreak => w.newline(),
            Event::Rule => {
                w.push_str("\n");
                w.push_str(&"─".repeat(40));
                w.push_str("\n\n");
            }
            Event::TaskListMarker(checked) => {
                let mark = if checked { "☑ " } else { "☐ " };
                w.push_str(mark);
            }
            Event::FootnoteReference(label) => {
                w.push_str(&format!("<sup>[{}]</sup>", escape(&label)));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let rendered = render_html_for_print(&html);
                if !rendered.is_empty() {
                    // Insert pre-built markup (already escaped + tagged).
                    w.out.push_str(&rendered);
                }
            }
            _ => {}
        }
    }
    (w.out, w.tables)
}

fn start_tag(w: &mut Walker, tag: &Tag) {
    match tag {
        Tag::Paragraph => {
            w.flush_item_marker();
        }
        Tag::Heading { level, .. } => {
            w.in_heading = Some(*level);
            let size = heading_size_attr(*level);
            w.push_str(&format!("<span weight=\"bold\" size=\"{}\">", size));
        }
        Tag::BlockQuote(_) => {
            w.blockquote_depth += 1;
            w.write_blockquote_prefix();
        }
        Tag::CodeBlock(_) => {
            w.in_code_block = true;
            w.push_str("<tt><span background=\"#eeeeee\">");
        }
        Tag::List(start) => {
            w.list_stack.push(match start {
                Some(n) => ListKind::Ordered(*n),
                None => ListKind::Unordered,
            });
        }
        Tag::Item => {
            w.pending_item_marker = true;
        }
        Tag::Emphasis => w.push_str("<i>"),
        Tag::Strong => w.push_str("<b>"),
        Tag::Strikethrough => w.push_str("<s>"),
        Tag::Link { dest_url, .. } => {
            // Underline + colored; print is not interactive.
            let _ = dest_url;
            w.push_str("<span foreground=\"#1c71d8\" underline=\"single\">");
        }
        Tag::Image { title, .. } => {
            // Print alt/title text in italics; images not rendered.
            w.push_str("<i>[image: ");
            w.push_text(title);
            w.push_str("]</i>");
        }
        Tag::Table(_) => {
            w.enter_table();
        }
        Tag::TableHead => {
            w.table_in_head = true;
            w.table_current_row = Some(Vec::new());
        }
        Tag::TableRow => {
            w.table_in_head = false;
            w.table_current_row = Some(Vec::new());
        }
        Tag::TableCell => {
            w.table_current_cell = Some(String::new());
        }
        Tag::FootnoteDefinition(label) => {
            w.push_str(&format!("\n<sup>[{}]</sup> ", escape(label)));
        }
        _ => {}
    }
}

fn end_tag(w: &mut Walker, tag: &TagEnd) {
    match tag {
        TagEnd::Paragraph => {
            // Blank line between paragraphs unless inside a list item (single \n)
            if w.list_stack.is_empty() {
                w.push_str("\n\n");
            } else {
                w.push_str("\n");
            }
        }
        TagEnd::Heading(_) => {
            w.in_heading = None;
            w.push_str("</span>\n\n");
        }
        TagEnd::BlockQuote(_) => {
            w.blockquote_depth = w.blockquote_depth.saturating_sub(1);
            w.push_str("\n");
        }
        TagEnd::CodeBlock => {
            w.push_str("</span></tt>\n\n");
            w.in_code_block = false;
        }
        TagEnd::List(_) => {
            w.list_stack.pop();
            if w.list_stack.is_empty() {
                w.push_str("\n");
            }
        }
        TagEnd::Item => {
            w.push_str("\n");
        }
        TagEnd::Emphasis => w.push_str("</i>"),
        TagEnd::Strong => w.push_str("</b>"),
        TagEnd::Strikethrough => w.push_str("</s>"),
        TagEnd::Link => w.push_str("</span>"),
        TagEnd::Image => {}
        TagEnd::Table => {
            w.end_table();
        }
        TagEnd::TableHead => {
            if let Some(row) = w.table_current_row.take() {
                if let Some(header) = w.table_current_header.as_mut() {
                    *header = row;
                }
            }
            w.table_in_head = false;
        }
        TagEnd::TableRow => {
            if let Some(row) = w.table_current_row.take() {
                if let Some(body) = w.table_current_body.as_mut() {
                    body.push(row);
                }
            }
        }
        TagEnd::TableCell => {
            if let (Some(cell), Some(row)) = (
                w.table_current_cell.take(),
                w.table_current_row.as_mut(),
            ) {
                row.push(cell);
            }
        }
        TagEnd::FootnoteDefinition => {
            w.push_str("\n");
        }
        _ => {}
    }
}

// =============================================================================
// Phase 4: cairo-drawn table support
// =============================================================================

/// One markup chunk produced by `split_markup_at_sentinels`.
enum MarkupSegment {
    /// Plain Pango markup, ready for `set_markup` after sentinels are removed.
    Text(String),
    /// Reference to a `TableBlock` in `PrintState::tables`.
    Table(usize),
}

/// Walk the markup, splitting at `\u{E000}<idx>\u{E001}` sentinels. Empty text
/// segments (e.g. between two adjacent tables) are dropped — they would create
/// zero-height layouts that contribute nothing but extra index slots.
fn split_markup_at_sentinels(markup: &str) -> Vec<MarkupSegment> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut chars = markup.chars().peekable();
    while let Some(c) = chars.next() {
        if c == TABLE_SENTINEL_OPEN {
            // Flush accumulated text.
            if !buf.is_empty() {
                out.push(MarkupSegment::Text(std::mem::take(&mut buf)));
            }
            let mut idx_str = String::new();
            for c2 in chars.by_ref() {
                if c2 == TABLE_SENTINEL_CLOSE {
                    break;
                }
                idx_str.push(c2);
            }
            if let Ok(idx) = idx_str.parse::<usize>() {
                out.push(MarkupSegment::Table(idx));
            }
        } else {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        out.push(MarkupSegment::Text(buf));
    }
    out
}

/// Pre-computed layout info for one table: per-row heights and column width.
struct TableLayoutInfo {
    /// row_heights[0] is the header; remaining indices are body rows in order.
    row_heights: Vec<f64>,
}

impl TableLayoutInfo {
    fn total_height(&self) -> f64 {
        self.row_heights.iter().sum()
    }

    /// Height of the header row (row 0). Used by the paginator to reserve
    /// space on continuation pages and by the draw path to advance past the
    /// repeated header. Zero if the table has no rows at all (defensive).
    fn header_height(&self) -> f64 {
        self.row_heights.first().copied().unwrap_or(0.0)
    }
}

/// Measure a table at the given printable width. Equal column widths.
fn measure_table(
    table: &TableBlock,
    page_width: f64,
    font: &gtk::pango::FontDescription,
    print_ctx: &gtk::PrintContext,
) -> TableLayoutInfo {
    let cols = table.columns.max(1);
    let col_w = (page_width / cols as f64).max(1.0);
    let inner_w = (col_w - 2.0 * TABLE_CELL_H_PAD).max(1.0);
    let inner_w_pu = (inner_w * PANGO_SCALE) as i32;

    let mut row_heights = Vec::with_capacity(1 + table.rows.len());
    let make_layout = |markup: &str| -> gtk::pango::Layout {
        let layout = print_ctx.create_pango_layout();
        layout.set_font_description(Some(font));
        layout.set_width(inner_w_pu);
        layout.set_wrap(gtk::pango::WrapMode::WordChar);
        layout.set_markup(markup);
        layout
    };

    // Header row (always present, even if empty — keeps `body_offset_pts`
    // consistent across the row-split paginator).
    {
        let mut max_h: f64 = 0.0;
        for cell in &table.header {
            let l = make_layout(cell);
            let (_, log) = l.extents();
            let h = log.height() as f64 / PANGO_SCALE;
            if h > max_h {
                max_h = h;
            }
        }
        row_heights.push(max_h + 2.0 * TABLE_CELL_V_PAD);
    }

    for row in &table.rows {
        let mut max_h: f64 = 0.0;
        for cell in row {
            let l = make_layout(cell);
            let (_, log) = l.extents();
            let h = log.height() as f64 / PANGO_SCALE;
            if h > max_h {
                max_h = h;
            }
        }
        row_heights.push(max_h + 2.0 * TABLE_CELL_V_PAD);
    }

    TableLayoutInfo { row_heights }
}

/// Measure the header row's height for a table using a layout created from
/// the supplied cairo context. Used by the draw pass to advance `cursor_y`
/// after a slice whose header was repeated. Kept separate from `measure_table`
/// because that one needs a `PrintContext`, which we don't have on the draw
/// side.
fn measure_table_header_height(
    cr: &gtk::cairo::Context,
    table: &TableBlock,
    page_width: f64,
    font: &gtk::pango::FontDescription,
) -> f64 {
    let cols = table.columns.max(1);
    let col_w = (page_width / cols as f64).max(1.0);
    let inner_w = (col_w - 2.0 * TABLE_CELL_H_PAD).max(1.0);
    let inner_w_pu = (inner_w * PANGO_SCALE) as i32;

    let mut max_h: f64 = 0.0;
    for cell in &table.header {
        let layout = pangocairo::functions::create_layout(cr);
        layout.set_font_description(Some(font));
        layout.set_width(inner_w_pu);
        layout.set_wrap(gtk::pango::WrapMode::WordChar);
        layout.set_markup(cell);
        let (_, log) = layout.extents();
        let h = log.height() as f64 / PANGO_SCALE;
        if h > max_h {
            max_h = h;
        }
    }
    max_h + 2.0 * TABLE_CELL_V_PAD
}

/// Draw a (possibly partial) table starting at `top_y`. `body_offset_pts` is
/// the y-offset within the table's own row sequence at which to start drawing;
/// `slice_height` bounds the slice. The header row is row index 0 and gets a
/// light grey background fill.
///
/// When `repeat_header` is `true`, the header row (row 0) is drawn first at
/// `top_y` regardless of `body_offset_pts`, then body rows starting at
/// `body_offset_pts` are drawn below it shifted down by header_h. `slice_height`
/// in that case represents the body-only height; the header is in addition.
#[allow(clippy::too_many_arguments)]
fn draw_table_block(
    cr: &gtk::cairo::Context,
    table: &TableBlock,
    page_width: f64,
    top_y: f64,
    body_offset_pts: f64,
    slice_height: f64,
    font: &gtk::pango::FontDescription,
    repeat_header: bool,
) {
    let cols = table.columns.max(1);
    let col_w = (page_width / cols as f64).max(1.0);
    let inner_w = (col_w - 2.0 * TABLE_CELL_H_PAD).max(1.0);
    let inner_w_pu = (inner_w * PANGO_SCALE) as i32;

    // Re-measure (we don't carry TableLayoutInfo into draw). Cheap relative to
    // pango glyph runs.
    let make_layout_no_ctx = |markup: &str| -> gtk::pango::Layout {
        // Create a layout from the cairo context. pangocairo will use the
        // current cairo font map.
        let layout = pangocairo::functions::create_layout(cr);
        layout.set_font_description(Some(font));
        layout.set_width(inner_w_pu);
        layout.set_wrap(gtk::pango::WrapMode::WordChar);
        layout.set_markup(markup);
        layout
    };

    // Helper: draw one row (header bg if ri == 0, cell borders, cell text).
    let draw_row = |draw_y: f64, ri: usize, cells: &[String], row_h: f64| {
        let layouts: Vec<gtk::pango::Layout> = (0..cols)
            .map(|ci| make_layout_no_ctx(cells.get(ci).map(String::as_str).unwrap_or("")))
            .collect();
        if ri == 0 {
            cr.set_source_rgb(0.96, 0.96, 0.96);
            cr.rectangle(0.0, draw_y, col_w * cols as f64, row_h);
            let _ = cr.fill();
        }
        for (ci, layout) in layouts.iter().enumerate() {
            let cell_x = ci as f64 * col_w;
            cr.set_source_rgb(0.85, 0.85, 0.85);
            cr.set_line_width(0.5);
            cr.rectangle(cell_x, draw_y, col_w, row_h);
            let _ = cr.stroke();
            cr.set_source_rgb(0.1, 0.1, 0.1);
            cr.move_to(cell_x + TABLE_CELL_H_PAD, draw_y + TABLE_CELL_V_PAD);
            pangocairo::functions::show_layout(cr, layout);
        }
    };

    // Compute one row's height by measuring cell layouts.
    let measure_row_h = |cells: &[String]| -> f64 {
        let layouts: Vec<gtk::pango::Layout> = (0..cols)
            .map(|ci| make_layout_no_ctx(cells.get(ci).map(String::as_str).unwrap_or("")))
            .collect();
        let max_inner: f64 = layouts
            .iter()
            .map(|l| {
                let (_, log) = l.extents();
                log.height() as f64 / PANGO_SCALE
            })
            .fold(0.0_f64, f64::max);
        max_inner + 2.0 * TABLE_CELL_V_PAD
    };

    // When asked to repeat the header on a continuation page, draw row 0
    // first and shift the body draw cursor below it. body_offset_pts still
    // refers to the original (untransformed) row sequence, so the body
    // rows skip everything before body_offset_pts in the same v1.3 way.
    let mut body_top_y = top_y;
    if repeat_header {
        let header_h = measure_row_h(&table.header);
        draw_row(top_y, 0, &table.header, header_h);
        body_top_y = top_y + header_h;
    }

    // Walk all rows, skipping rows that fall before `body_offset_pts` and
    // stopping once we exceed `body_offset_pts + slice_height`. The header
    // row (ri == 0) is intentionally walked too — when repeat_header is
    // false and body_offset_pts is 0.0, it gets drawn as part of the body;
    // when repeat_header is true the caller has already drawn it above and
    // body_offset_pts is positioned past row 0 so the skip clause excludes
    // it naturally.
    let slice_end = body_offset_pts + slice_height;
    let mut row_top: f64 = 0.0;
    let total_rows = 1 + table.rows.len();
    for ri in 0..total_rows {
        let cells: &[String] = if ri == 0 {
            &table.header
        } else {
            &table.rows[ri - 1]
        };
        let row_h = measure_row_h(cells);

        let row_bottom = row_top + row_h;
        // Skip rows entirely above the slice.
        if row_bottom <= body_offset_pts {
            row_top = row_bottom;
            continue;
        }
        // Stop once we're past the end of the slice.
        if row_top >= slice_end {
            break;
        }

        let draw_y = body_top_y + (row_top - body_offset_pts);
        draw_row(draw_y, ri, cells, row_h);

        row_top = row_bottom;
    }
}
