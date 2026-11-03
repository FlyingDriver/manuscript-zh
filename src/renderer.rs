use gtk::prelude::*;
use gtk::pango;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::path::Path;
use unicode_width::UnicodeWidthStr;

use crate::highlighter;

#[derive(Default, Clone, Copy)]
struct RunStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
}

#[derive(Clone)]
pub(crate) struct TextRun {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: Option<String>,
}

pub(crate) type Cell = Vec<TextRun>;

fn flatten_cell(cell: &Cell) -> String {
    cell.iter().map(|r| r.text.as_str()).collect::<String>()
}

/// Source-line → rendered-buffer-offset anchor, recorded at every block
/// start and soft/hard break. Split mode interpolates between neighbouring
/// anchors to scroll the preview to the exact spot the editor shows.
pub type LineAnchor = (u32, i32);

/// Config-driven rendering switches.
#[derive(Clone, Copy, Default)]
pub struct RenderOptions {
    /// Tables as embedded GtkGrid widgets instead of box-drawing text.
    pub rich_tables: bool,
    /// Single newline inside a paragraph → line break (else a space, per
    /// CommonMark).
    pub keep_line_breaks: bool,
    /// Widest an image may render when the document doesn't say otherwise.
    /// Per-image sizing (`<img width=…>` or the `![alt](src){width=…}`
    /// extension) overrides this.
    pub max_image_width: i32,
}

/// A length as written in the document: bare pixels (`640`), pixels with a
/// unit (`640px`), or a percentage (`45%`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SizeSpec {
    Px(i32),
    /// Percent of the available width (for `width`) or of the natural height
    /// (for `height`).
    Percent(f64),
}

/// What one document image said about its own size. Both fields are
/// optional; an unset axis falls back to the natural size.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ImageSize {
    pub width: Option<SizeSpec>,
    pub height: Option<SizeSpec>,
}

impl ImageSize {
    pub fn is_empty(&self) -> bool {
        self.width.is_none() && self.height.is_none()
    }

    /// Resolve against the image's natural pixel size and the width available
    /// to the text column. Returns the size to request on the Picture widget.
    ///
    /// Rules, in order:
    /// * explicit `width` wins; `height` then follows the aspect ratio unless
    ///   it was given too
    /// * only `height` given → width follows the aspect ratio
    /// * neither → natural size, capped at `max_width`
    fn resolve(
        &self,
        natural_w: i32,
        natural_h: i32,
        avail_w: i32,
        max_width: i32,
    ) -> (i32, i32) {
        let nw = natural_w.max(1);
        let nh = natural_h.max(1);
        let avail = avail_w.max(1);
        let cap = max_width.max(1);

        let px_from = |spec: SizeSpec, basis: i32| -> i32 {
            match spec {
                SizeSpec::Px(p) => p,
                SizeSpec::Percent(pct) => ((basis as f64) * pct / 100.0).round() as i32,
            }
            .max(1)
        };

        match (self.width, self.height) {
            (Some(w), Some(h)) => (
                px_from(w, avail).min(avail),
                px_from(h, nh),
            ),
            (Some(w), None) => {
                let out_w = px_from(w, avail).min(avail);
                let out_h = ((nh as f64) * (out_w as f64) / (nw as f64)).round() as i32;
                (out_w, out_h.max(1))
            }
            (None, Some(h)) => {
                let out_h = px_from(h, nh);
                let out_w = ((nw as f64) * (out_h as f64) / (nh as f64)).round() as i32;
                (out_w.min(avail).max(1), out_h)
            }
            (None, None) => fit_image_dims(nw, nh, cap, avail),
        }
    }
}

impl SizeSpec {
    /// Parse `640`, `640px`, `45%`, `45.5%`. Anything else is not a size we
    /// understand, so the caller falls back to the natural size.
    fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim();
        if s.is_empty() {
            return None;
        }
        if let Some(num) = s.strip_suffix('%') {
            let pct: f64 = num.trim().parse().ok()?;
            return (pct.is_finite() && pct > 0.0).then_some(SizeSpec::Percent(pct));
        }
        let num = s
            .strip_suffix("px")
            .or_else(|| s.strip_suffix("PX"))
            .unwrap_or(s)
            .trim();
        let px: f64 = num.parse().ok()?;
        if !px.is_finite() || px <= 0.0 {
            return None;
        }
        Some(SizeSpec::Px(px.round() as i32))
    }

    /// Width in pixels for an `HTMLStyle`-ish `width: Npx` / `width: N%` body.
    fn parse_css(raw: &str) -> Option<Self> {
        let v = raw.trim();
        // Reject values we know we can't honour (auto, inherit, calc(), …).
        if v.is_empty() {
            return None;
        }
        SizeSpec::parse(v)
    }
}

/// Render markdown content into a GtkTextView. Returns the line map
/// (ascending in both source line and rendered offset).
pub fn render_markdown(
    text_view: &gtk::TextView,
    markdown: &str,
    base_dir: &Path,
    is_dark: bool,
    opts: RenderOptions,
) -> Vec<LineAnchor> {
    let rich_tables = opts.rich_tables;
    let buffer = text_view.buffer();

    // Clean up old dynamic tags before clearing buffer
    cleanup_dynamic_tags(&buffer);

    buffer.set_text("");

    // Set up link click handler (only once)
    setup_link_handler(text_view);
    setup_table_copy_handler(text_view);

    // Create tags for formatting
    create_tags(&buffer, is_dark);
    apply_margin_tags(text_view);
    setup_margin_tracking(text_view);

    let body = strip_frontmatter(markdown);
    // Frontmatter lines are gone from `body`; expand_wikilinks keeps every
    // newline in place, so body line N is source line N + frontmatter_lines.
    let frontmatter_lines = markdown[..markdown.len() - body.len()]
        .bytes()
        .filter(|&b| b == b'\n')
        .count() as u32;
    // Wikilinks `[[name]]` aren't part of CommonMark / GFM. Rather than
    // tracking a separate parser, we rewrite them to ordinary markdown links
    // with a custom `manuscript://wikilink/` scheme. The existing link
    // machinery then handles styling and clicks, and the link handler in
    // setup_link_handler can recognise the scheme to do something sensible.
    let body_owned = expand_wikilinks(body);
    // Per-image sizing: `![alt](pic.png){width=640}` isn't CommonMark either.
    // Rewriting it to `<img width=…>` reuses the HTML path, which already
    // parses sizes — one code path for both spellings.
    let body_owned = expand_image_sizes(&body_owned);
    let body: &str = &body_owned;

    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(body, options).into_offset_iter();

    let mut context = RenderContext::new();
    let mut last_end: usize = 0;
    // After a top-level list ends we already inserted `list-end-spacer` (a \n with
    // 8px above). Suppress blank-line replay once so adjacent lists/paragraphs don't
    // accumulate a double gap.
    let mut suppress_blank_lines = false;
    let mut line_map: Vec<LineAnchor> = Vec::new();
    let mut line_counter = LineCounter::default();
    // Images were emitted from the current HTML block; close their line when
    // the block ends.
    let mut html_images = false;

    for (event, range) in parser {
        if html_images && !matches!(event, Event::Html(_)) {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, "\n");
            html_images = false;
        }
        // Count blank lines in source between previous event end and this event start.
        // Emit extra \n for each blank line to preserve user's visual spacing.
        if range.start > last_end && !suppress_blank_lines {
            let gap = &body[last_end..range.start];
            let newlines = gap.bytes().filter(|&b| b == b'\n').count();
            if newlines >= 2 {
                let extra = newlines - 1;
                let mut end = buffer.end_iter();
                for _ in 0..extra {
                    buffer.insert(&mut end, "\n");
                }
            }
        }
        suppress_blank_lines = false;
        last_end = range.end;

        let is_block_start = matches!(
            event,
            Event::Start(
                Tag::Paragraph
                    | Tag::Heading { .. }
                    | Tag::CodeBlock(_)
                    | Tag::Item
                    | Tag::Table(_)
                    | Tag::BlockQuote(_)
                    | Tag::FootnoteDefinition(_)
            ) | Event::Rule
        );
        let is_break = matches!(event, Event::SoftBreak | Event::HardBreak);
        // A tight list item can open with inline content other than plain
        // text (`1. `code` …`, `- **bold**`, a link). The bullet must land
        // before it — otherwise it's emitted at the first Text event and
        // appears *after* the code span.
        if context.pending_item_bullet
            && matches!(
                event,
                Event::Code(_)
                    | Event::InlineHtml(_)
                    | Event::FootnoteReference(_)
                    | Event::Start(
                        Tag::Emphasis
                            | Tag::Strong
                            | Tag::Strikethrough
                            | Tag::Link { .. }
                            | Tag::Image { .. }
                    )
            )
        {
            context.flush_text(&buffer);
            emit_item_bullet(&mut context, &buffer);
        }
        // Block anchors point at where the block begins in the rendered
        // buffer, so flush pending text and read the offset before handling.
        let block_anchor = if is_block_start && !context.in_table_cell() {
            context.flush_text(&buffer);
            let line = frontmatter_lines + line_counter.line_at(body, range.start);
            Some((line, buffer.end_iter().offset()))
        } else {
            None
        };

        match event {
            Event::Start(tag) => handle_start_tag(&mut context, &buffer, &tag),
            Event::End(tag_end) => {
                handle_end_tag(
                    &mut context,
                    &buffer,
                    text_view,
                    &tag_end,
                    base_dir,
                    is_dark,
                    rich_tables,
                    opts.max_image_width,
                );
                if matches!(tag_end, TagEnd::List(_)) && context.list_depth == 0 {
                    suppress_blank_lines = true;
                }
            }
            Event::Text(text) => {
                // Tight list items (`- foo\n  - bar`) skip the Start(Paragraph)
                // event entirely, so the bullet emission has to happen at the
                // first inline event inside the item. Without this the bullet
                // would only fire later at Start(List), landing after the
                // item's own text.
                if context.pending_item_bullet {
                    emit_item_bullet(&mut context, &buffer);
                }
                if context.callout_pending {
                    context.callout_match_buf.push_str(&text);
                    let buf_trimmed = context.callout_match_buf.trim_start();
                    // Fast-path abort: prefix can't lead to `[!TYPE]`.
                    let viable = buf_trimmed.is_empty()
                        || (buf_trimmed.starts_with('[')
                            && (buf_trimmed.len() < 2 || buf_trimmed.as_bytes()[1] == b'!'));
                    if !viable {
                        let pending = std::mem::take(&mut context.callout_match_buf);
                        context.callout_pending = false;
                        context.push_text(&pending);
                        continue;
                    }
                    // Stay pending: the parse runs at the first SoftBreak /
                    // HardBreak / End(Paragraph) so an optional `Custom Title`
                    // following `]` on the same line is captured.
                    continue;
                }
                context.push_text(&text);
            }
            Event::Code(code) => {
                if context.in_table_cell() {
                    context.flush_run();
                    context.current_run_style.code = true;
                    context.push_text(&code);
                    context.flush_run();
                    context.current_run_style.code = false;
                } else {
                    context.flush_text(&buffer);
                    let mut end = buffer.end_iter();
                    let mut tags = vec!["inline-code"];
                    context.add_context_tags(&mut tags);
                    buffer.insert_with_tags_by_name(&mut end, &code, &tags);
                }
            }
            Event::SoftBreak => {
                if context.callout_pending {
                    // Line-end reached: attempt to resolve the buffered marker.
                    // On a match, the title line is emitted and the line break
                    // itself is consumed.
                    if try_resolve_callout(&mut context, &buffer) {
                        continue;
                    }
                }
                if context.callout_skip_break {
                    context.callout_skip_break = false;
                    continue;
                }
                if opts.keep_line_breaks {
                    context.flush_text(&buffer);
                    let mut end = buffer.end_iter();
                    buffer.insert(&mut end, "\n");
                } else {
                    // CommonMark: a soft break is a space. Flushed right away
                    // so the line-map anchor below sees the right offset.
                    context.push_text(" ");
                    context.flush_text(&buffer);
                }
            }
            Event::HardBreak => {
                if context.callout_pending {
                    if try_resolve_callout(&mut context, &buffer) {
                        continue;
                    }
                }
                if context.callout_skip_break {
                    context.callout_skip_break = false;
                    continue;
                }
                context.flush_text(&buffer);
                let mut end = buffer.end_iter();
                buffer.insert(&mut end, "\n");
            }
            Event::TaskListMarker(checked) => {
                context.task_list_marker = Some(checked);
            }
            Event::Rule => {
                context.flush_text(&buffer);
                insert_separator(text_view, &buffer);
            }
            Event::FootnoteReference(label) => {
                context.flush_text(&buffer);
                let n = context.footnote_number(&label);
                let mut end = buffer.end_iter();
                let mut tags = vec!["footnote-ref"];
                context.add_context_tags(&mut tags);
                buffer.insert_with_tags_by_name(&mut end, &format!("[{n}]"), &tags);
            }
            // Block-level HTML (`<p align="center">...</p>` etc.): text is
            // still dropped (stripped inner text brought raw whitespace and
            // literal `&nbsp;` into the buffer), but `<img>` is rendered —
            // README-style screenshot rows and badges live in HTML blocks.
            // A block arrives as several Html events (one per line); the
            // images go side by side and the line is closed once the block
            // ends (`html_images` below).
            Event::Html(html) => {
                for (src, size) in parse_html_images(&html) {
                    context.flush_text(&buffer);
                    insert_image(
                        text_view,
                        &buffer,
                        &src,
                        base_dir,
                        true,
                        size,
                        opts.max_image_width,
                    );
                    html_images = true;
                }
            }
            Event::InlineHtml(html) => {
                let stripped = strip_html_tags(&html);
                if !stripped.is_empty() {
                    context.push_text(&stripped);
                }
            }
            _ => {}
        }

        if let Some((line, offset)) = block_anchor {
            push_anchor(&mut line_map, line, offset);
        } else if is_break && !context.in_table_cell() {
            // A break's anchor is the line after it, where the new rendered
            // line starts.
            let line = frontmatter_lines + line_counter.line_at(body, range.end);
            push_anchor(&mut line_map, line, buffer.end_iter().offset());
        }
    }
    if html_images {
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, "\n");
    }
    // Kept on the view for Copy: a multi-line selection is copied as the
    // Markdown source lines it covers (see `setup_table_copy_handler`).
    // SAFETY: single-threaded GTK; these keys only ever hold these types.
    unsafe {
        text_view.set_data(LINE_MAP_KEY, line_map.clone());
        text_view.set_data(SOURCE_KEY, markdown.to_string());
    }
    line_map
}

const LINE_MAP_KEY: &str = "manuscript-line-map";
const SOURCE_KEY: &str = "manuscript-source";

/// `src` values of every `<img …>` tag in an HTML fragment (case-insensitive
/// tag/attribute names, single- or double-quoted or bare values).
#[cfg(test)]
fn html_img_sources(html: &str) -> Vec<String> {
    parse_html_images(html).into_iter().map(|(src, _)| src).collect()
}

/// One `<img …>` found in an HTML fragment: its `src` plus any size it
/// declared through `width` / `height` attributes or an inline
/// `style="width: …; height: …"` declaration.
///
/// Attributes are read case-insensitively and may be quoted with `"`, `'` or
/// left bare. `width="100%"` is a percentage of the text column;
/// `width="640"` and `width="640px"` are pixels. Unparseable values are
/// dropped so the image falls back to its natural size.
fn parse_html_images(html: &str) -> Vec<(String, ImageSize)> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("<img") {
        let tag_start = from + rel;
        let tag_end = lower[tag_start..].find('>').map(|e| tag_start + e).unwrap_or(lower.len());
        let tag = &html[tag_start..tag_end];
        let tag_lower = &lower[tag_start..tag_end];

        // --- src ---
        let src = attr_value(tag, tag_lower, "src", true);
        if let Some(src) = src.filter(|s| !s.is_empty()) {
            // --- explicit width / height attributes ---
            let mut size = ImageSize::default();
            if let Some(w) = attr_value(tag, tag_lower, "width", false) {
                size.width = SizeSpec::parse(&w);
            }
            if let Some(h) = attr_value(tag, tag_lower, "height", false) {
                size.height = SizeSpec::parse(&h);
            }
            // --- style="width: 100px; height: 50%" ---
            if let Some(style) = attr_value(tag, tag_lower, "style", false) {
                let (w, h) = parse_css_size(&style);
                if size.width.is_none() {
                    size.width = w;
                }
                if size.height.is_none() {
                    size.height = h;
                }
            }
            out.push((src, size));
        }
        from = tag_end;
    }
    out
}

/// Read one attribute's value from an `<img …>` tag. `tag` and `tag_lower`
/// must be the same slice, one original and one ASCII-lowercased.
///
/// `exact_prefix` is for `src`: only the whole attribute name counts, so a
/// `data-src` never shadows it.
fn attr_value(tag: &str, tag_lower: &str, name: &str, exact_prefix: bool) -> Option<String> {
    let mut search = 0;
    while let Some(r) = tag_lower[search..].find(name) {
        let at = search + r;
        search = at + name.len();
        // Whole attribute name: must start the tag body (whitespace before) or
        // follow whitespace — never inside another name like `data-src`.
        let before_ok = at > 0
            && tag_lower[..at].ends_with(|c: char| c.is_ascii_whitespace());
        // `<img src=…`: `src` directly after the tag name.
        let after_tag_name = exact_prefix
            && tag_lower[..at].ends_with("img");
        if !before_ok && !after_tag_name {
            continue;
        }
        let rest = tag[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
            _ => rest.split(|c: char| c.is_ascii_whitespace()).next().unwrap_or(""),
        };
        return Some(value.to_string());
    }
    None
}

/// Pull `width` / `height` out of an inline `style` attribute body.
fn parse_css_size(style: &str) -> (Option<SizeSpec>, Option<SizeSpec>) {
    let mut width = None;
    let mut height = None;
    for decl in style.split(';') {
        let Some((prop, value)) = decl.split_once(':') else { continue };
        match prop.trim().to_ascii_lowercase().as_str() {
            "width" => width = SizeSpec::parse_css(value),
            "height" => height = SizeSpec::parse_css(value),
            _ => {}
        }
    }
    (width, height)
}

/// Incremental byte-offset → line lookup. Parser offsets are mostly
/// monotonic, so the scan stays O(n) overall; a backwards jump rescans.
#[derive(Default)]
struct LineCounter {
    pos: usize,
    line: u32,
}

impl LineCounter {
    fn line_at(&mut self, text: &str, pos: usize) -> u32 {
        let pos = pos.min(text.len());
        if pos < self.pos {
            self.pos = 0;
            self.line = 0;
        }
        self.line += text.as_bytes()[self.pos..pos].iter().filter(|&&b| b == b'\n').count() as u32;
        self.pos = pos;
        self.line
    }
}

/// Append an anchor, keeping the map strictly ascending in source line and
/// non-decreasing in offset. Same-line anchors keep the first (block start).
fn push_anchor(map: &mut Vec<LineAnchor>, line: u32, offset: i32) {
    if let Some(&(last_line, last_off)) = map.last() {
        if line <= last_line || offset < last_off {
            return;
        }
    }
    map.push((line, offset));
}

/// Collect headings from raw markdown with their original source line numbers.
/// Used by the outline panel so clicking a row scrolls the source view (Edit/Split)
/// to the correct line — rendered-buffer line numbers don't match source lines.
pub fn collect_source_headings(markdown: &str) -> Vec<(u8, String, i32)> {
    let body = strip_frontmatter(markdown);
    let body_start_byte = markdown.len() - body.len();
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(body, options).into_offset_iter();

    let mut out: Vec<(u8, String, i32)> = Vec::new();
    let mut current: Option<(u8, usize, String)> = None;
    // Headings come in document order: count newlines incrementally from the
    // previous heading (from the start each time was quadratic — ~0.5 s on a
    // 1 MB file with 4500 headings).
    let (mut counted_to, mut lines_before) = (0usize, 0i32);

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let lvl = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                current = Some((lvl, body_start_byte + range.start, String::new()));
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, _, ref mut s)) = current {
                    s.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((lvl, byte_offset, text)) = current.take() {
                    let to = byte_offset.min(markdown.len()).max(counted_to);
                    lines_before += markdown.as_bytes()[counted_to..to]
                        .iter()
                        .filter(|&&b| b == b'\n')
                        .count() as i32;
                    counted_to = to;
                    let line = lines_before;
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        out.push((lvl, trimmed, line));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Rewrite Obsidian-style wikilinks (`[[name]]` and `[[name|label]]`) into
/// CommonMark links with a `manuscript://wikilink/` scheme. Skipped inside
/// fenced code blocks and inline code spans so user-typed `[[…]]` in code
/// samples stays literal.
fn expand_wikilinks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_fence = false;
    let mut fence_marker: Option<char> = None; // ` or ~
    let mut fence_len = 0usize;

    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(|c: char| c == ' ' || c == '\t');
        let leading = line.len() - trimmed.len();
        // Track fenced code block boundaries (``` or ~~~ at line start).
        if leading <= 3 {
            let first = trimmed.chars().next();
            if matches!(first, Some('`') | Some('~')) {
                let ch = first.unwrap();
                let run_len = trimmed.chars().take_while(|c| *c == ch).count();
                if run_len >= 3 {
                    if !in_fence {
                        in_fence = true;
                        fence_marker = Some(ch);
                        fence_len = run_len;
                        out.push_str(line);
                        continue;
                    } else if fence_marker == Some(ch) && run_len >= fence_len {
                        in_fence = false;
                        fence_marker = None;
                        fence_len = 0;
                        out.push_str(line);
                        continue;
                    }
                }
            }
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        // Walk the line, copying as-is inside inline code spans (` … `) and
        // rewriting `[[…]]` outside them. We iterate by byte indices so that
        // `line.find("]]")` slicing stays valid for arbitrary UTF-8 input;
        // the only ASCII bytes we branch on (`[`, '`') can't appear inside a
        // multi-byte char, so the cursor never lands mid-codepoint.
        let mut i = 0;
        let mut in_inline_code = false;
        while i < line.len() {
            let rest = &line[i..];
            let b = rest.as_bytes()[0];
            if b == b'`' {
                in_inline_code = !in_inline_code;
                out.push('`');
                i += 1;
                continue;
            }
            if !in_inline_code && rest.starts_with("[[") {
                if let Some(close_rel) = line[i + 2..].find("]]") {
                    let inner = &line[i + 2..i + 2 + close_rel];
                    if !inner.contains('\n') {
                        let (name, label) = match inner.find('|') {
                            Some(pipe) => (&inner[..pipe], &inner[pipe + 1..]),
                            None => (inner, inner),
                        };
                        out.push('[');
                        out.push_str(label.trim());
                        out.push_str("](manuscript://wikilink/");
                        out.push_str(&percent_encode_path(name.trim()));
                        out.push(')');
                        i += 2 + close_rel + 2;
                        continue;
                    }
                }
            }
            // Default: copy one char preserving UTF-8.
            let ch_end = rest
                .char_indices()
                .nth(1)
                .map(|(j, _)| i + j)
                .unwrap_or(line.len());
            out.push_str(&line[i..ch_end]);
            i = ch_end;
        }
    }
    out
}

/// Rewrite per-image size syntax into the HTML form the renderer already
/// understands.
///
/// Two spellings are supported, both immediately following the image:
///
/// ```text
/// ![alt](pic.png){width=640}         ![alt](pic.png){width=45%}
/// ![alt](pic.png){height=300}        ![alt](pic.png){width=640 height=360}
/// ```
///
/// The whole image is rewritten to an `<img>` tag carrying `width` / `height`
/// attributes, which `html_images` parses. Tildes inside the braces are also
/// accepted (`{width=640px}`). Fenced code blocks and inline code spans are
/// left untouched so documentation about the syntax doesn't render images.
///
/// A `{…}` that holds no recognisable size is left verbatim, so ordinary text
/// after an image is never disturbed.
fn expand_image_sizes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_fence = false;
    let mut fence_marker: Option<char> = None;
    let mut fence_len = 0usize;

    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(|c: char| c == ' ' || c == '\t');
        let leading = line.len() - trimmed.len();

        // --- fenced code block tracking (same rules as expand_wikilinks) ---
        if leading <= 3 {
            if let Some(ch @ ('`' | '~')) = trimmed.chars().next() {
                let run_len = trimmed.chars().take_while(|c| *c == ch).count();
                if run_len >= 3 {
                    if !in_fence {
                        in_fence = true;
                        fence_marker = Some(ch);
                        fence_len = run_len;
                        out.push_str(line);
                        continue;
                    } else if fence_marker == Some(ch) && run_len >= fence_len {
                        in_fence = false;
                        fence_marker = None;
                        fence_len = 0;
                        out.push_str(line);
                        continue;
                    }
                }
            }
        }
        if in_fence {
            out.push_str(line);
            continue;
        }

        // --- walk the line, skipping inline code spans ---
        let mut i = 0;
        let mut in_inline_code = false;
        while i < line.len() {
            let rest = &line[i..];
            if rest.starts_with('`') {
                in_inline_code = !in_inline_code;
                out.push('`');
                i += 1;
                continue;
            }
            if !in_inline_code && rest.starts_with("![") {
                if let Some((consumed, replacement)) = rewrite_sized_image(rest) {
                    out.push_str(&replacement);
                    i += consumed;
                    continue;
                }
            }
            let ch_end = rest
                .char_indices()
                .nth(1)
                .map(|(j, _)| i + j)
                .unwrap_or(line.len());
            out.push_str(&line[i..ch_end]);
            i = ch_end;
        }
    }
    out
}

/// If `rest` starts with `![alt](src){sizes}`, return how many bytes are
/// consumed and the `<img …>` replacement. `None` when there is no size
/// block or it holds nothing usable, so the caller copies the text verbatim.
fn rewrite_sized_image(rest: &str) -> Option<(usize, String)> {
    // ![alt](src)
    let after_bang = rest.strip_prefix("![")?;
    let close_bracket = find_unescaped(after_bang, ']')?;
    let alt = &after_bang[..close_bracket];
    let after_alt = &after_bang[close_bracket + 1..];
    let after_paren = after_alt.strip_prefix('(')?;
    let close_paren = find_unescaped(after_paren, ')')?;
    let dest = after_paren[..close_paren].trim();
    if dest.is_empty() {
        return None;
    }
    // Optional `"title"` after the destination.
    let (dest, title) = split_dest_title(dest);
    let after_dest = &after_paren[close_paren + 1..];

    // {sizes}
    let after_brace = after_dest.strip_prefix('{')?;
    let close_brace = after_brace.find('}')?;
    let spec = &after_brace[..close_brace];

    let mut width: Option<SizeSpec> = None;
    let mut height: Option<SizeSpec> = None;
    for token in spec.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let Some((key, value)) = token.split_once('=') else { continue };
        let key = key.trim().trim_end_matches(':');
        match key.to_ascii_lowercase().as_str() {
            "width" | "w" => width = SizeSpec::parse(value),
            "height" | "h" => height = SizeSpec::parse(value),
            _ => {}
        }
    }
    let size = ImageSize { width, height };
    if size.is_empty() {
        // `{…}` holds something else — leave the document alone.
        return None;
    }

    let consumed = 2 + close_bracket + 1 + 1 + close_paren + 1 + 1 + close_brace + 1;
    let mut tag = String::from("<img src=\"");
    tag.push_str(&html_attr_escape(dest));
    tag.push('"');
    if !alt.trim().is_empty() {
        tag.push_str(" alt=\"");
        tag.push_str(&html_attr_escape(alt.trim()));
        tag.push('"');
    }
    if let Some(t) = title {
        tag.push_str(" title=\"");
        tag.push_str(&html_attr_escape(t));
        tag.push('"');
    }
    if let Some(w) = width {
        tag.push_str(" width=\"");
        tag.push_str(&size_to_attr(w));
        tag.push('"');
    }
    if let Some(h) = height {
        tag.push_str(" height=\"");
        tag.push_str(&size_to_attr(h));
        tag.push('"');
    }
    tag.push('>');
    Some((consumed, tag))
}

/// Byte index of `needle` in `s`, skipping backslash-escaped occurrences.
fn find_unescaped(s: &str, needle: char) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == needle {
            return Some(idx);
        }
    }
    None
}

/// Split `path "title"` into its two parts. A bare path has no title.
fn split_dest_title(dest: &str) -> (&str, Option<&str>) {
    let dest = dest.trim();
    if let Some(space) = dest.find(char::is_whitespace) {
        let (path, rest) = dest.split_at(space);
        let rest = rest.trim();
        if rest.len() >= 2
            && ((rest.starts_with('"') && rest.ends_with('"'))
                || (rest.starts_with('\'') && rest.ends_with('\'')))
        {
            return (path, Some(&rest[1..rest.len() - 1]));
        }
    }
    (dest, None)
}

fn size_to_attr(spec: SizeSpec) -> String {
    match spec {
        SizeSpec::Px(p) => format!("{}px", p),
        SizeSpec::Percent(p) => {
            if (p.fract()).abs() < f64::EPSILON {
                format!("{}%", p as i64)
            } else {
                format!("{}%", p)
            }
        }
    }
}

fn html_attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Lightweight percent-encoder for the wikilink path component. Encodes
/// anything that isn't an unreserved ASCII URL char.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let b = *b;
        let safe = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Reverse of `percent_encode_path`: decode `%XX` byte escapes back to a
/// UTF-8 string. Unrecognized sequences are passed through literally.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip a leading YAML frontmatter block (`---\n...\n---\n`) if present.
/// Returns the body without the frontmatter; if no valid frontmatter is found,
/// returns the original input.
fn strip_frontmatter(input: &str) -> &str {
    let after_open = match input.strip_prefix("---\n").or_else(|| input.strip_prefix("---\r\n")) {
        Some(s) => s,
        None => return input,
    };
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" || trimmed == "..." {
            let end = offset + line.len();
            return &after_open[end..];
        }
        offset += line.len();
    }
    input
}

/// Emit the bullet (or task-marker disc) for the current list item using
/// the task_list_marker state captured between Start(Item) and the first
/// inner block-level event. Clears `pending_item_bullet` and the marker.
fn emit_item_bullet(ctx: &mut RenderContext, buffer: &gtk::TextBuffer) {
    if !ctx.pending_item_bullet {
        return;
    }
    ctx.pending_item_bullet = false;
    let marker = ctx.task_list_marker.take();
    let number = match ctx.list_numbers.last_mut() {
        Some(Some(n)) => {
            let cur = *n;
            *n += 1;
            Some(cur)
        }
        _ => None,
    };
    let bullet = match (marker, number) {
        (Some(true), _) => "\u{25CF} ".to_string(),  // ● filled disc — checked
        (Some(false), _) => "\u{25CB} ".to_string(), // ○ empty disc — unchecked
        (None, Some(n)) => format!("{n}. "),
        (None, None) => if ctx.list_depth > 1 { "\u{25e6} " } else { "\u{2022} " }.to_string(),
    };
    let tag_name = if ctx.list_depth > 1 { "list-item-nested" } else { "list-item" };
    let mut end = buffer.end_iter();
    buffer.insert_with_tags_by_name(&mut end, &bullet, &[tag_name]);
}

/// Resolve a buffered `[!TYPE] optional title` line into a callout. Returns
/// `true` when a callout was successfully emitted (caller should treat the
/// triggering line break as consumed), `false` when the buffer didn't match
/// — the caller is responsible for emitting any buffered text and continuing
/// normally.
fn try_resolve_callout(ctx: &mut RenderContext, buffer: &gtk::TextBuffer) -> bool {
    if !ctx.callout_pending {
        return false;
    }
    let buf = std::mem::take(&mut ctx.callout_match_buf);
    ctx.callout_pending = false;
    let trimmed = buf.trim_start();
    let close = match trimmed.find(']') {
        Some(i) => i,
        None => {
            ctx.push_text(&buf);
            return false;
        }
    };
    let head = &trimmed[..=close];
    let tail = &trimmed[close + 1..];
    let Some((kind, rest_in_head)) = parse_callout_marker(head) else {
        ctx.push_text(&buf);
        return false;
    };
    ctx.callout_kind = Some(kind);
    ctx.callout_skip_break = true;
    let (icon, default_title) = callout_decoration(kind);
    let mut title_text = String::new();
    if !rest_in_head.is_empty() {
        title_text.push_str(rest_in_head);
    }
    let tail_trim = tail.trim();
    if !tail_trim.is_empty() {
        if !title_text.is_empty() && !title_text.ends_with(' ') {
            title_text.push(' ');
        }
        title_text.push_str(tail_trim);
    }
    if title_text.is_empty() {
        title_text.push_str(default_title);
    }
    ctx.flush_text(buffer);
    let mut end = buffer.end_iter();
    let mut tags = vec!["callout-title"];
    ctx.add_context_tags(&mut tags);
    let line = format!("{}{}\n", icon, title_text);
    buffer.insert_with_tags_by_name(&mut end, &line, &tags);
    true
}

/// Return the icon prefix and default title for a recognized callout kind.
fn callout_decoration(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "callout-note" => ("\u{2139}\u{FE0F} ", "Note"),       // ℹ️
        "callout-tip" => ("\u{1F4A1} ", "Tip"),                // 💡
        "callout-warning" => ("\u{26A0}\u{FE0F} ", "Warning"), // ⚠️
        "callout-danger" => ("\u{26D4} ", "Danger"),           // ⛔
        "callout-important" => ("\u{2757} ", "Important"),     // ❗
        _ => ("", ""),
    }
}

/// Parse a GFM callout marker like `[!NOTE]` (case-insensitive). Returns the
/// matched tag name (callout-note / -tip / -warning / -danger / -important)
/// and any remaining text on the same line. The recognized aliases follow
/// GitHub's spec: NOTE, TIP, IMPORTANT, WARNING, CAUTION. `INFO` and `DANGER`
/// are accepted as common synonyms for note / danger.
fn parse_callout_marker(text: &str) -> Option<(&'static str, &str)> {
    let s = text.trim_start();
    let body = s.strip_prefix("[!")?;
    let close = body.find(']')?;
    let raw_kind = &body[..close];
    let rest = body[close + 1..].trim_start();
    let kind = raw_kind.trim().to_ascii_uppercase();
    let tag = match kind.as_str() {
        "NOTE" | "INFO" => "callout-note",
        "TIP" => "callout-tip",
        "WARNING" | "CAUTION" => "callout-warning",
        "DANGER" => "callout-danger",
        "IMPORTANT" => "callout-important",
        _ => return None,
    };
    Some((tag, rest))
}

/// Strip HTML tags from a fragment, preserving inner text.
/// `<br>`, `<br/>`, `<br />` become `\n`. Comments `<!-- ... -->` are dropped.
/// Everything else: tag opener/closer removed, inner text kept verbatim.
fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Comment
            if bytes[i..].starts_with(b"<!--") {
                if let Some(end) = input[i + 4..].find("-->") {
                    i = i + 4 + end + 3;
                    continue;
                } else {
                    break;
                }
            }
            // Find tag end
            let rest = &input[i..];
            let end_rel = match rest.find('>') {
                Some(e) => e,
                None => break,
            };
            let tag_body = &rest[1..end_rel];
            let name_end = tag_body
                .find(|c: char| c.is_whitespace() || c == '/')
                .unwrap_or(tag_body.len());
            let name = tag_body[..name_end].trim_start_matches('/').to_ascii_lowercase();
            if name == "br" {
                out.push('\n');
            }
            i += end_rel + 1;
        } else {
            let ch_end = input[i..]
                .char_indices()
                .nth(1)
                .map(|(j, _)| i + j)
                .unwrap_or(input.len());
            out.push_str(&input[i..ch_end]);
            i = ch_end;
        }
    }
    out
}

/// Remove stale `linkurl-*` tags from the tag table.
/// These are created per-link with a unique offset in the name, so they accumulate
/// on every re-render (file-watch triggers this). Color tags (`fg-R-G-B`) are keyed
/// by color and reused — no cleanup needed.
fn cleanup_dynamic_tags(buffer: &gtk::TextBuffer) {
    let tag_table = buffer.tag_table();
    let mut to_remove: Vec<gtk::TextTag> = Vec::new();
    tag_table.foreach(|tag| {
        if let Some(name) = tag.name() {
            if name.starts_with("linkurl-") {
                to_remove.push(tag.clone());
            }
        }
    });
    for tag in to_remove {
        tag_table.remove(&tag);
    }
}

/// Left margins of block tags, as designed against a TextView left margin of
/// `MARGIN_DESIGN_BASE` (the wide View mode). A tag's left margin *replaces*
/// the TextView's rather than adding to it, so fixed pixel values broke the
/// proportions wherever the view margin differs — on narrow windows (16 px)
/// lists sat ~40 px right of the text, in the Split preview (24 px) too.
/// `apply_margin_tags` re-bases them on the live view margin instead.
const MARGIN_DESIGN_BASE: i32 = 48;
const MARGIN_TAGS: &[(&str, i32)] = &[
    // Quotes / callouts sit slightly outside the text column (tinted bar).
    ("blockquote", 32),
    ("callout-note", 32),
    ("callout-tip", 32),
    ("callout-warning", 32),
    ("callout-danger", 32),
    ("callout-important", 32),
    ("code-block", 48),
    ("table", 48),
    ("footnote-def", 16),
    ("list-item", 56),
    ("list-item-nested", 84),
    // List text column = list margin + ~one marker width.
    ("code-block-in-list", 72),
    ("code-block-in-nested-list", 100),
];

/// Re-base the block tags' left margins on `text_view`'s current left margin.
fn apply_margin_tags(text_view: &gtk::TextView) {
    let base = text_view.left_margin();
    let table = text_view.buffer().tag_table();
    for (name, design) in MARGIN_TAGS {
        if let Some(tag) = table.lookup(name) {
            tag.set_left_margin((base + design - MARGIN_DESIGN_BASE).max(0));
        }
    }
}

/// Keep the margin tags following the view margin (narrow breakpoint,
/// content-width changes). Installed once per TextView.
fn setup_margin_tracking(text_view: &gtk::TextView) {
    const KEY: &str = "manuscript-margin-tracking";
    // SAFETY: single-threaded GTK; the flag is only ever a `bool`.
    unsafe {
        if text_view.data::<bool>(KEY).is_some() {
            return;
        }
        text_view.set_data(KEY, true);
    }
    text_view.connect_notify_local(Some("left-margin"), |tv, _| apply_margin_tags(tv));
}

fn create_tags(buffer: &gtk::TextBuffer, is_dark: bool) {
    let tag_table = buffer.tag_table();

    type TagSetup = fn(&gtk::TextTag, bool);
    let tags: &[(&str, TagSetup)] = &[
        // Headings — Obsidian-like hierarchy: dramatic scale jumps, weight,
        // and a per-level foreground accent that adapts to dark/light theme.
        ("h1", |t, dark| {
            t.set_scale(2.4);
            t.set_weight(800);
            t.set_pixels_above_lines(22);
            t.set_pixels_below_lines(4);
            t.set_foreground(Some(if dark { "#c8d7ff" } else { "#1c3d8a" }));
        }),
        ("h2", |t, dark| {
            t.set_scale(1.9);
            t.set_weight(800);
            t.set_pixels_above_lines(18);
            t.set_pixels_below_lines(3);
            t.set_foreground(Some(if dark { "#a6c8ff" } else { "#2456b3" }));
        }),
        ("h3", |t, dark| {
            t.set_scale(1.5);
            t.set_weight(700);
            t.set_pixels_above_lines(14);
            t.set_pixels_below_lines(2);
            t.set_foreground(Some(if dark { "#8fb6ff" } else { "#3568c5" }));
        }),
        ("h4", |t, dark| {
            t.set_scale(1.3);
            t.set_weight(700);
            t.set_pixels_above_lines(12);
            t.set_pixels_below_lines(2);
            t.set_foreground(Some(if dark { "#7da5ef" } else { "#4174c9" }));
        }),
        ("h5", |t, dark| {
            t.set_scale(1.15);
            t.set_weight(600);
            t.set_pixels_above_lines(10);
            t.set_pixels_below_lines(2);
            t.set_foreground(Some(if dark { "#6f97e2" } else { "#5481cf" }));
        }),
        ("h6", |t, dark| {
            t.set_scale(1.05);
            t.set_weight(600);
            t.set_pixels_above_lines(8);
            t.set_pixels_below_lines(2);
            t.set_foreground(Some(if dark { "#9a9996" } else { "#77767b" }));
        }),
        // Inline formatting
        ("bold", |t, _| {
            t.set_weight(700);
        }),
        ("italic", |t, _| {
            t.set_style(pango::Style::Italic);
        }),
        ("strikethrough", |t, _| {
            t.set_strikethrough(true);
        }),
        ("inline-code", |t, dark| {
            t.set_family(Some("monospace"));
            t.set_scale(0.88);
            // Softer hue than the previous purple — sits next to coloured
            // headings without competing for attention.
            if dark {
                t.set_foreground(Some("#ffd6a5"));
                t.set_background(Some("#2a2826"));
            } else {
                t.set_foreground(Some("#b45309"));
                t.set_background(Some("#fef3c7"));
            }
        }),
        // Block elements
        ("blockquote", |t, dark| {
            t.set_style(pango::Style::Italic);
            t.set_foreground(Some(if dark { "#c8d4e8" } else { "#3b4a64" }));
            // Subtle paragraph background gives the quote a "card" feel and
            // helps it stand out from body text without a real border widget.
            t.set_paragraph_background(Some(if dark { "#1d2535" } else { "#eef3fb" }));
            t.set_pixels_above_lines(8);
            t.set_pixels_below_lines(8);
            t.set_pixels_inside_wrap(4);
        }),
        // GFM-style callouts: paragraph background + indent.
        ("callout-note", |t, dark| {
            t.set_pixels_above_lines(8);
            t.set_pixels_below_lines(8);
            t.set_paragraph_background(Some(if dark { "#1c2740" } else { "#e0ecff" }));
        }),
        ("callout-tip", |t, dark| {
            t.set_pixels_above_lines(8);
            t.set_pixels_below_lines(8);
            t.set_paragraph_background(Some(if dark { "#1a2e2a" } else { "#dcfce7" }));
        }),
        ("callout-warning", |t, dark| {
            t.set_pixels_above_lines(8);
            t.set_pixels_below_lines(8);
            t.set_paragraph_background(Some(if dark { "#322a17" } else { "#fef3c7" }));
        }),
        ("callout-danger", |t, dark| {
            t.set_pixels_above_lines(8);
            t.set_pixels_below_lines(8);
            t.set_paragraph_background(Some(if dark { "#3a1c1c" } else { "#fee2e2" }));
        }),
        ("callout-important", |t, dark| {
            t.set_pixels_above_lines(8);
            t.set_pixels_below_lines(8);
            t.set_paragraph_background(Some(if dark { "#2a1f3d" } else { "#ede9fe" }));
        }),
        ("callout-title", |t, _| {
            t.set_weight(700);
            t.set_pixels_above_lines(2);
            t.set_pixels_below_lines(4);
        }),
        ("code-block", |t, dark| {
            t.set_family(Some("monospace"));
            t.set_scale(0.88);
            if dark {
                t.set_paragraph_background(Some("#212135"));
                t.set_foreground(Some("#c0caf5"));
            } else {
                t.set_paragraph_background(Some("#f0f2f5"));
            }
            t.set_right_margin(32);
            t.set_pixels_above_lines(4);
            t.set_pixels_below_lines(4);
        }),
        // Lists — island of text with left margin and vertical spacing
        ("list-item", |t, _| {
            t.set_indent(-20);
            t.set_pixels_above_lines(4);
            t.set_pixels_below_lines(4);
            t.set_pixels_inside_wrap(4);
        }),
        ("list-item-nested", |t, _| {
            t.set_indent(-20);
            t.set_pixels_above_lines(4);
            t.set_pixels_below_lines(4);
            t.set_pixels_inside_wrap(4);
        }),
        // A short line, not a full empty one: at body size the spacer read
        // as a double gap after every list.
        ("list-end-spacer", |t, _| {
            t.set_scale(0.5);
            t.set_pixels_above_lines(4);
        }),
        // Code block inside a list item: indented past the marker to the
        // item's text column (margin in MARGIN_TAGS). Created after
        // "code-block", so its margin wins.
        ("code-block-in-list", |_, _| {}),
        ("code-block-in-nested-list", |_, _| {}),
        // Paragraphs — breathing room.
        // pixels_inside_wrap guards against the fallback-font ascender clip
        // observed on systems without Inter: Pango's reported line height
        // for some sans-serif fallbacks (Cantarell at the same point size)
        // is short enough that the top of the next wrapped line cuts into
        // the bottom of the previous one. Adding a few pixels of inside-wrap
        // padding keeps capital letters and ascenders fully visible.
        ("paragraph", |t, _| {
            t.set_pixels_above_lines(6);
            t.set_pixels_below_lines(6);
            t.set_pixels_inside_wrap(4);
        }),
        // Tables
        ("table", |t, dark| {
            t.set_family(Some("monospace"));
            t.set_scale(0.85);
            let bg = if dark { "#1e1e2e" } else { "#f6f8fa" };
            t.set_paragraph_background(Some(bg));
            t.set_right_margin(32);
            t.set_pixels_above_lines(2);
            t.set_pixels_below_lines(2);
        }),
        ("table-header", |t, _| {
            t.set_weight(700);
        }),
        ("footnote-ref", |t, dark| {
            t.set_scale(0.7);
            t.set_rise(4 * pango::SCALE);
            t.set_foreground(Some(if dark { "#7dd3fc" } else { "#0369a1" }));
        }),
        ("footnote-def", |t, dark| {
            t.set_scale(0.82);
            t.set_foreground(Some(if dark { "#9a9996" } else { "#77767b" }));
            t.set_pixels_above_lines(4);
            t.set_pixels_below_lines(1);
        }),
    ];

    // Theme-dependent tags that need to be recreated on theme change
    const THEME_DEPENDENT: &[&str] = &[
        "h1", "h2", "h3", "h4", "h5", "h6",
        "inline-code", "blockquote", "code-block", "table",
        "footnote-ref", "footnote-def",
        "callout-note", "callout-tip", "callout-warning",
        "callout-danger", "callout-important",
    ];

    for &(name, configure) in tags {
        if let Some(existing) = tag_table.lookup(name) {
            if THEME_DEPENDENT.contains(&name) {
                // Remove and recreate theme-dependent tags
                tag_table.remove(&existing);
            } else {
                // Static tag, skip
                continue;
            }
        }
        let tag = gtk::TextTag::builder().name(name).build();
        configure(&tag, is_dark);
        tag_table.add(&tag);
    }
}

fn insert_separator(text_view: &gtk::TextView, buffer: &gtk::TextBuffer) {
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "\n");

    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(8);
    separator.set_margin_bottom(8);
    // A child anchor allocates its widget at natural width — 1 px for a
    // separator, so every `---` rendered as a dot. Same fix as rich tables:
    // a wrap pinned to the usable width and re-pinned on resize.
    let wrap = gtk::Box::new(gtk::Orientation::Vertical, 0);
    wrap.append(&separator);
    wrap.set_size_request(table_wrap_width(text_view), -1);
    register_table_wrap_resize(text_view, &wrap);

    let mut end = buffer.end_iter();
    let anchor = buffer.create_child_anchor(&mut end);
    text_view.add_child_at_anchor(&wrap, &anchor);

    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "\n");
}

// -- Render context to track nested state --

struct RenderContext {
    /// Stack of active tags
    tag_stack: Vec<ActiveTag>,
    /// Accumulated text for current element
    text_buf: String,
    /// Current list nesting depth
    list_depth: u32,
    /// One entry per open list: `Some(next number)` for ordered lists,
    /// `None` for bullet lists.
    list_numbers: Vec<Option<u64>>,
    /// Cached counters — avoid O(n) scans of tag_stack in the hot flush path
    blockquote_depth: u32,
    item_depth: u32,
    table_cell_depth: u32,
    footnote_def_depth: u32,
    /// Code block language
    code_lang: Option<String>,
    /// Table state
    table_rows: Vec<Vec<Cell>>,
    table_current_row: Vec<Cell>,
    /// Per-cell collection state (Some inside a TableCell).
    current_cell: Option<Cell>,
    current_run_style: RunStyle,
    current_link_url: Option<String>,
    /// Task list marker for current item
    task_list_marker: Option<bool>,
    /// Map footnote labels to a 1-based index in the order they're first seen.
    footnote_numbers: Vec<(String, u32)>,
    /// Whether the first footnote definition has already opened — controls the
    /// extra blank line above the footnote group.
    footnote_group_open: bool,
    /// True at the start of every paragraph inside a top-level blockquote,
    /// before the first non-whitespace text. Cleared when we either fire a
    /// callout, give up on detection, or hit a paragraph-ending event.
    callout_pending: bool,
    /// Accumulator for pulldown-cmark's shredded `[!TYPE]` events. The
    /// parser emits the bracket sequence as three (or more) Event::Text
    /// fragments (`"["`, `"!NOTE"`, `"]"`), so we buffer them in order to
    /// match the marker reliably.
    callout_match_buf: String,
    /// Tag name (e.g. "callout-note") if the current paragraph inside a
    /// blockquote was recognised as a GFM callout; replaces the regular
    /// "blockquote" tag in add_context_tags for that paragraph only.
    callout_kind: Option<&'static str>,
    /// True after we just stripped a `[!TYPE]` marker — used to also drop
    /// the SoftBreak that immediately follows it.
    callout_skip_break: bool,
    /// True after Start(Item) and until the bullet character has been
    /// emitted. Bullet emission is deferred to Start(Paragraph) inside the
    /// item so we can observe the optional Event::TaskListMarker that
    /// pulldown-cmark emits AFTER Start(Item) for `- [ ]` / `- [x]` items.
    pending_item_bullet: bool,
}

#[derive(Debug)]
enum ActiveTag {
    /// Heading + the buffer offset where the heading region started, so we can
    /// apply the h-tag to the entire region (including inline-code/strong/em
    /// fragments) when the heading ends.
    Heading(HeadingLevel, i32),
    Emphasis,
    Strong,
    Strikethrough,
    Link(String),
    BlockQuote,
    CodeBlock,
    List,
    Item,
    Paragraph,
    Image(String, String),
    Table,
    TableHead,
    TableRow,
    TableCell,
    FootnoteDefinition,
}

impl RenderContext {
    fn new() -> Self {
        Self {
            tag_stack: Vec::with_capacity(8),
            text_buf: String::with_capacity(256),
            list_depth: 0,
            list_numbers: Vec::new(),
            blockquote_depth: 0,
            item_depth: 0,
            table_cell_depth: 0,
            footnote_def_depth: 0,
            code_lang: None,
            table_rows: Vec::new(),
            table_current_row: Vec::new(),
            current_cell: None,
            current_run_style: RunStyle::default(),
            current_link_url: None,
            task_list_marker: None,
            footnote_numbers: Vec::new(),
            footnote_group_open: false,
            callout_pending: false,
            callout_match_buf: String::new(),
            callout_kind: None,
            callout_skip_break: false,
            pending_item_bullet: false,
        }
    }

    fn footnote_number(&mut self, label: &str) -> u32 {
        if let Some((_, n)) = self.footnote_numbers.iter().find(|(l, _)| l == label) {
            return *n;
        }
        let n = (self.footnote_numbers.len() as u32) + 1;
        self.footnote_numbers.push((label.to_string(), n));
        n
    }

    fn push_tag(&mut self, tag: ActiveTag) {
        match &tag {
            ActiveTag::BlockQuote => self.blockquote_depth += 1,
            ActiveTag::Item => self.item_depth += 1,
            ActiveTag::TableCell => self.table_cell_depth += 1,
            ActiveTag::FootnoteDefinition => self.footnote_def_depth += 1,
            _ => {}
        }
        self.tag_stack.push(tag);
    }

    fn pop_tag(&mut self) -> Option<ActiveTag> {
        let popped = self.tag_stack.pop();
        match &popped {
            Some(ActiveTag::BlockQuote) => self.blockquote_depth -= 1,
            Some(ActiveTag::Item) => self.item_depth -= 1,
            Some(ActiveTag::TableCell) => self.table_cell_depth -= 1,
            Some(ActiveTag::FootnoteDefinition) => self.footnote_def_depth -= 1,
            _ => {}
        }
        popped
    }

    fn in_footnote_def(&self) -> bool {
        self.footnote_def_depth > 0
    }

    fn push_text(&mut self, text: &str) {
        self.text_buf.push_str(text);
    }

    fn in_table_cell(&self) -> bool {
        self.table_cell_depth > 0
    }

    fn in_item(&self) -> bool {
        self.item_depth > 0
    }

    fn take_text(&mut self) -> String {
        std::mem::take(&mut self.text_buf)
    }

    /// Add context tags (list-item, blockquote, footnote-def) based on current tag stack.
    fn add_context_tags(&self, tags: &mut Vec<&str>) {
        if self.blockquote_depth > 0 {
            // When the current top-level blockquote was parsed as a GFM
            // callout, swap the regular "blockquote" tag for the callout
            // variant so the coloured background wins over the italic
            // quote treatment.
            if let Some(kind) = self.callout_kind {
                tags.push(kind);
            } else {
                tags.push("blockquote");
            }
        }
        if self.item_depth > 0 {
            if self.list_depth > 1 {
                tags.push("list-item-nested");
            } else {
                tags.push("list-item");
            }
        }
        if self.footnote_def_depth > 0 {
            tags.push("footnote-def");
        }
    }

    /// Flush a TextRun into the current table cell (call when style flags or
    /// link state change, or at cell end).
    fn flush_run(&mut self) {
        if self.text_buf.is_empty() {
            return;
        }
        if let Some(cell) = self.current_cell.as_mut() {
            let s = self.current_run_style;
            cell.push(TextRun {
                text: std::mem::take(&mut self.text_buf),
                bold: s.bold,
                italic: s.italic,
                strike: s.strike,
                code: s.code,
                link: self.current_link_url.clone(),
            });
        }
    }

    /// Flush accumulated plain text to the buffer (used before inline formatting starts).
    fn flush_text(&mut self, buffer: &gtk::TextBuffer) {
        if self.current_cell.is_some() {
            self.flush_run();
            return;
        }
        if !self.text_buf.is_empty() {
            let text = std::mem::take(&mut self.text_buf);
            let mut end = buffer.end_iter();

            let mut tags: Vec<&str> = Vec::new();
            self.add_context_tags(&mut tags);

            if tags.is_empty() {
                buffer.insert(&mut end, &text);
            } else {
                buffer.insert_with_tags_by_name(&mut end, &text, &tags);
            }
        }
    }
}

fn handle_start_tag(ctx: &mut RenderContext, buffer: &gtk::TextBuffer, tag: &Tag) {
    match tag {
        Tag::Heading { level, .. } => {
            ctx.flush_text(buffer);
            let start_offset = buffer.end_iter().offset();
            ctx.push_tag(ActiveTag::Heading(*level, start_offset));
        }
        Tag::Paragraph => {
            ctx.flush_text(buffer);
            // Every paragraph inside a top-level blockquote is a fresh chance
            // to detect a GFM callout marker. Pulldown-cmark merges adjacent
            // `>` blocks (separated only by a blank line) into one BlockQuote
            // with multiple Paragraphs, so per-paragraph detection is the
            // only way to catch each callout independently.
            if ctx.blockquote_depth > 0 && ctx.callout_kind.is_none() {
                ctx.callout_pending = true;
                ctx.callout_match_buf.clear();
            }
            if ctx.pending_item_bullet {
                emit_item_bullet(ctx, buffer);
            }
            ctx.push_tag(ActiveTag::Paragraph);
        }
        Tag::Emphasis => {
            // Flush text accumulated before this inline element
            ctx.flush_text(buffer);
            if ctx.current_cell.is_some() {
                ctx.current_run_style.italic = true;
            }
            ctx.push_tag(ActiveTag::Emphasis);
        }
        Tag::Strong => {
            ctx.flush_text(buffer);
            if ctx.current_cell.is_some() {
                ctx.current_run_style.bold = true;
            }
            ctx.push_tag(ActiveTag::Strong);
        }
        Tag::Strikethrough => {
            ctx.flush_text(buffer);
            if ctx.current_cell.is_some() {
                ctx.current_run_style.strike = true;
            }
            ctx.push_tag(ActiveTag::Strikethrough);
        }
        Tag::Link { dest_url, .. } => {
            ctx.flush_text(buffer);
            if ctx.current_cell.is_some() {
                ctx.current_link_url = Some(dest_url.to_string());
            }
            ctx.push_tag(ActiveTag::Link(dest_url.to_string()));
        }
        Tag::BlockQuote(_) => {
            ctx.flush_text(buffer);
            ctx.push_tag(ActiveTag::BlockQuote);
        }
        Tag::CodeBlock(kind) => {
            ctx.flush_text(buffer);
            let lang = match kind {
                pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                    let l = lang.split(',').next().unwrap_or("").trim();
                    if l.is_empty() { None } else { Some(l.to_string()) }
                }
                pulldown_cmark::CodeBlockKind::Indented => None,
            };
            ctx.code_lang = lang;
            ctx.push_tag(ActiveTag::CodeBlock);
        }
        Tag::List(start) => {
            ctx.flush_text(buffer);
            // If we're entering a nested list right after Start(Item) without
            // an intervening Paragraph, the bullet for the parent item hasn't
            // been emitted yet — do it now so the parent doesn't lose its
            // marker.
            if ctx.pending_item_bullet {
                emit_item_bullet(ctx, buffer);
            }
            // Nested list: ensure newline before it (parent item content just ended)
            if ctx.list_depth > 0 {
                let mut end = buffer.end_iter();
                buffer.insert(&mut end, "\n");
            }
            ctx.list_depth += 1;
            ctx.list_numbers.push(*start);
            ctx.push_tag(ActiveTag::List);
        }
        Tag::Item => {
            ctx.flush_text(buffer);
            ctx.push_tag(ActiveTag::Item);
            // Defer the bullet — pulldown-cmark emits Event::TaskListMarker
            // *after* Start(Item) for `- [ ]` / `- [x]` items, so deciding
            // the bullet here would always lose one event. emit_item_bullet
            // runs at Start(Paragraph) / Start(List) by which point the
            // marker (if any) has been observed.
            ctx.pending_item_bullet = true;
        }
        Tag::Image { dest_url, title, .. } => {
            ctx.flush_text(buffer);
            ctx.push_tag(ActiveTag::Image(dest_url.to_string(), title.to_string()));
        }
        Tag::Table(_alignments) => {
            ctx.flush_text(buffer);
            ctx.table_rows.clear();
            ctx.table_current_row.clear();
            ctx.push_tag(ActiveTag::Table);
        }
        Tag::TableHead => {
            ctx.table_current_row.clear();
            ctx.push_tag(ActiveTag::TableHead);
        }
        Tag::TableRow => {
            ctx.table_current_row.clear();
            ctx.push_tag(ActiveTag::TableRow);
        }
        Tag::TableCell => {
            ctx.text_buf.clear();
            ctx.current_cell = Some(Vec::new());
            ctx.current_run_style = RunStyle::default();
            ctx.current_link_url = None;
            ctx.push_tag(ActiveTag::TableCell);
        }
        Tag::FootnoteDefinition(label) => {
            ctx.flush_text(buffer);
            let n = ctx.footnote_number(label);
            let prefix = if ctx.footnote_group_open {
                format!("[{n}] ")
            } else {
                ctx.footnote_group_open = true;
                format!("\n[{n}] ")
            };
            let mut end = buffer.end_iter();
            buffer.insert_with_tags_by_name(&mut end, &prefix, &["footnote-def"]);
            ctx.push_tag(ActiveTag::FootnoteDefinition);
        }
        _ => {}
    }
}

fn handle_end_tag(
    ctx: &mut RenderContext,
    buffer: &gtk::TextBuffer,
    text_view: &gtk::TextView,
    tag_end: &TagEnd,
    base_dir: &Path,
    is_dark: bool,
    rich_tables: bool,
    max_image_width: i32,
) {
    match tag_end {
        TagEnd::Heading(_level) => {
            // Flush any remaining accumulated text into the buffer first
            // (inline events inside the heading already inserted their parts).
            ctx.flush_text(buffer);
            if let Some(ActiveTag::Heading(level, start_offset)) = ctx.pop_tag() {
                let tag_name = heading_tag_name(level);
                // Tag the whole heading region — including inline-code/strong/em fragments.
                let start = buffer.iter_at_offset(start_offset);
                let end = buffer.end_iter();
                buffer.apply_tag_by_name(tag_name, &start, &end);
                let mut end = buffer.end_iter();
                buffer.insert(&mut end, "\n");

                // No underline rule — H1/H2 are visually distinct enough via scale
                // (2.0/1.6) and weight (800/700) alone.
            }
        }
        TagEnd::Paragraph => {
            // End-of-paragraph with the callout buffer still pending means
            // the marker was a single-line callout with no body
            // (`> [!NOTE]` followed by a blank line). Resolve it here so the
            // title still gets emitted; if it doesn't match, fall back to
            // emitting the buffered text as plain content.
            if ctx.callout_pending {
                if !try_resolve_callout(ctx, buffer) {
                    let pending = std::mem::take(&mut ctx.callout_match_buf);
                    if !pending.is_empty() {
                        ctx.push_text(&pending);
                    }
                    ctx.callout_pending = false;
                }
            }
            let text = ctx.take_text();
            ctx.pop_tag();
            let in_item = ctx.in_item();
            let in_footnote = ctx.in_footnote_def();

            if !text.is_empty() {
                let mut end = buffer.end_iter();
                let mut tag_names: Vec<&str> = Vec::new();
                if !in_item && !in_footnote {
                    tag_names.push("paragraph");
                }
                ctx.add_context_tags(&mut tag_names);

                if tag_names.is_empty() {
                    buffer.insert(&mut end, &text);
                } else {
                    buffer.insert_with_tags_by_name(&mut end, &text, &tag_names);
                }
            }
            // Always add newline at paragraph end (unless inside list item or footnote)
            // — paragraph may consist only of inline elements (bold, links) that
            // already inserted their text, leaving text_buf empty.
            // Tag the newline with "paragraph" so pixels_below_lines applies.
            if !in_item && !in_footnote {
                let mut end = buffer.end_iter();
                buffer.insert_with_tags_by_name(&mut end, "\n", &["paragraph"]);
            } else if in_footnote {
                let mut end = buffer.end_iter();
                buffer.insert_with_tags_by_name(&mut end, "\n", &["footnote-def"]);
            }
            // Callouts are scoped to a single paragraph: clear the state so
            // the next paragraph in the same BlockQuote (or a sibling block
            // after it) starts a fresh detection round.
            if ctx.blockquote_depth > 0 {
                ctx.callout_kind = None;
                ctx.callout_pending = false;
                ctx.callout_skip_break = false;
            }
        }
        TagEnd::Emphasis => {
            ctx.pop_tag();
            if ctx.current_cell.is_some() {
                ctx.flush_run();
                ctx.current_run_style.italic = false;
            } else {
                let text = ctx.take_text();
                if !text.is_empty() {
                    let mut tags = vec!["italic"];
                    ctx.add_context_tags(&mut tags);
                    let mut end = buffer.end_iter();
                    buffer.insert_with_tags_by_name(&mut end, &text, &tags);
                }
            }
        }
        TagEnd::Strong => {
            ctx.pop_tag();
            if ctx.current_cell.is_some() {
                ctx.flush_run();
                ctx.current_run_style.bold = false;
            } else {
                let text = ctx.take_text();
                if !text.is_empty() {
                    let mut tags = vec!["bold"];
                    ctx.add_context_tags(&mut tags);
                    let mut end = buffer.end_iter();
                    buffer.insert_with_tags_by_name(&mut end, &text, &tags);
                }
            }
        }
        TagEnd::Strikethrough => {
            ctx.pop_tag();
            if ctx.current_cell.is_some() {
                ctx.flush_run();
                ctx.current_run_style.strike = false;
            } else {
                let text = ctx.take_text();
                if !text.is_empty() {
                    let mut tags = vec!["strikethrough"];
                    ctx.add_context_tags(&mut tags);
                    let mut end = buffer.end_iter();
                    buffer.insert_with_tags_by_name(&mut end, &text, &tags);
                }
            }
        }
        TagEnd::Link => {
            if ctx.current_cell.is_some() {
                ctx.flush_run();
                ctx.current_link_url = None;
                ctx.pop_tag();
            } else {
                let text = ctx.take_text();
                if let Some(ActiveTag::Link(url)) = ctx.pop_tag() {
                    let mut end = buffer.end_iter();
                    let start_offset = end.offset();

                    let link_tag_name = format!("linkurl-{}", start_offset);
                    let is_wikilink = url.starts_with("manuscript://wikilink/");
                    let link_tag = gtk::TextTag::builder()
                        .name(&link_tag_name)
                        .foreground(if is_wikilink {
                            // Wikilinks render in a violet accent without the
                            // underline so they read as "internal references"
                            // rather than external URLs.
                            if is_dark { "#c4a7e7" } else { "#7c3aed" }
                        } else {
                            if is_dark { "#78aeed" } else { "#1c71d8" }
                        })
                        .underline(if is_wikilink {
                            pango::Underline::None
                        } else {
                            pango::Underline::Single
                        })
                        .build();
                    unsafe {
                        link_tag.set_data("url", url);
                    }
                    if buffer.tag_table().lookup(&link_tag_name).is_none() {
                        buffer.tag_table().add(&link_tag);
                    }

                    buffer.insert_with_tags_by_name(&mut end, &text, &[&link_tag_name]);
                }
            }
        }
        TagEnd::BlockQuote(_) => {
            ctx.flush_text(buffer);
            ctx.pop_tag();
        }
        TagEnd::CodeBlock => {
            let code = ctx.take_text();
            let lang = ctx.code_lang.take();
            ctx.pop_tag();

            let list_tag = match ctx.list_depth {
                0 => None,
                1 => Some("code-block-in-list"),
                _ => Some("code-block-in-nested-list"),
            };
            insert_code_block(buffer, text_view, &code, lang.as_deref(), is_dark, list_tag);
        }
        TagEnd::List(_) => {
            ctx.list_depth = ctx.list_depth.saturating_sub(1);
            ctx.list_numbers.pop();
            ctx.pop_tag();
            // Spacer after top-level list
            if ctx.list_depth == 0 {
                let mut end = buffer.end_iter();
                buffer.insert_with_tags_by_name(&mut end, "\n", &["list-end-spacer"]);
            }
        }
        TagEnd::Item => {
            // Item with no inner Paragraph / List would leave the bullet
            // unrendered — flush it here as a fallback.
            if ctx.pending_item_bullet {
                emit_item_bullet(ctx, buffer);
            }
            ctx.flush_text(buffer);
            ctx.pop_tag();
            // An item whose content ended with a block that already closed its
            // line — a nested list (its last item's own End(Item)) or a code
            // block — must not add a second newline: that left a blank line
            // before the next sibling, and before the list-end spacer a
            // double gap.
            let mut last = buffer.end_iter();
            let ends_with_newline = last.backward_char() && last.char() == '\n';
            if !ends_with_newline {
                let mut end = buffer.end_iter();
                buffer.insert(&mut end, "\n");
            }
        }
        TagEnd::Image => {
            // Discard the alt text accumulated between Start(Image) and
            // End(Image) — insert_image handles its own captions / fallbacks
            // and we don't want the alt rendered separately below the image.
            //
            // Size never arrives here: `![alt](src){width=…}` is rewritten to
            // `<img width=…>` during preprocessing, so a plain Markdown image
            // is always unsized and takes the default cap.
            let _alt = ctx.take_text();
            if let Some(ActiveTag::Image(src, _title)) = ctx.pop_tag() {
                insert_image(
                    text_view,
                    buffer,
                    &src,
                    base_dir,
                    false,
                    ImageSize::default(),
                    max_image_width,
                );
            }
        }
        TagEnd::TableCell => {
            ctx.flush_run();
            ctx.pop_tag();
            let cell = ctx.current_cell.take().unwrap_or_default();
            ctx.table_current_row.push(cell);
        }
        TagEnd::TableHead => {
            ctx.pop_tag();
            let row = std::mem::take(&mut ctx.table_current_row);
            ctx.table_rows.push(row);
        }
        TagEnd::TableRow => {
            ctx.pop_tag();
            let row = std::mem::take(&mut ctx.table_current_row);
            ctx.table_rows.push(row);
        }
        TagEnd::Table => {
            ctx.pop_tag();
            let rows = std::mem::take(&mut ctx.table_rows);
            if rich_tables {
                insert_table_grid(text_view, buffer, &rows, is_dark);
            } else {
                insert_table(buffer, &rows, is_dark);
            }
        }
        TagEnd::FootnoteDefinition => {
            ctx.flush_text(buffer);
            ctx.pop_tag();
        }
        _ => {}
    }
}

fn heading_tag_name(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "h1",
        HeadingLevel::H2 => "h2",
        HeadingLevel::H3 => "h3",
        HeadingLevel::H4 => "h4",
        HeadingLevel::H5 => "h5",
        HeadingLevel::H6 => "h6",
    }
}

fn insert_code_block(
    buffer: &gtk::TextBuffer,
    _text_view: &gtk::TextView,
    code: &str,
    lang: Option<&str>,
    is_dark: bool,
    list_tag: Option<&str>,
) {
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "\n");
    let tags: Vec<&str> = std::iter::once("code-block").chain(list_tag).collect();

    // Code content with TextTags (fills width properly)
    let syntax = lang.and_then(highlighter::find_syntax);

    let lines: Vec<&str> = code.lines().collect();
    let highlighted = syntax.map(|s| highlighter::highlight_code(s, &lines, is_dark));

    for (i, line) in lines.iter().enumerate() {
        let line_start_offset = buffer.end_iter().offset();

        let mut end = buffer.end_iter();
        buffer.insert_with_tags_by_name(&mut end, line, &tags);

        if let Some(spans_per_line) = highlighted.as_ref() {
            if let Some(spans) = spans_per_line.get(i) {
                for span in spans {
                    let start = buffer.iter_at_offset(line_start_offset + span.start as i32);
                    let end = buffer.iter_at_offset(line_start_offset + span.end as i32);

                    let color_tag_name =
                        format!("fg-{}-{}-{}", span.fg.0, span.fg.1, span.fg.2);
                    if buffer.tag_table().lookup(&color_tag_name).is_none() {
                        let tag = gtk::TextTag::builder()
                            .name(&color_tag_name)
                            .foreground(format!(
                                "#{:02x}{:02x}{:02x}",
                                span.fg.0, span.fg.1, span.fg.2
                            ))
                            .build();
                        buffer.tag_table().add(&tag);
                    }
                    buffer.apply_tag_by_name(&color_tag_name, &start, &end);
                }
            }
        }

        if i < lines.len() - 1 {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, "\n");
        }
    }

    // Top level: close the line plus a blank line of breathing room. Inside
    // a list item just close the line — the next item follows directly.
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, if list_tag.is_some() { "\n" } else { "\n\n" });
}

/// Create the per-cell inline TextTags (bold / italic / strike / inline-code).
///
/// Each cell TextView has its own TextBuffer with its own TagTable, so these
/// tags must be created on every cell's table — they cannot be shared with
/// the main buffer's tag table (re-adding the same tag panics with
/// "tag already in table"). Idempotent: if a tag with the same name is
/// already present we skip it, which lets callers invoke this freely.
fn create_cell_inline_tags(table: &gtk::TextTagTable, is_dark: bool) {
    if table.lookup("bold").is_none() {
        let t = gtk::TextTag::builder().name("bold").build();
        t.set_weight(700);
        table.add(&t);
    }
    if table.lookup("italic").is_none() {
        let t = gtk::TextTag::builder().name("italic").build();
        t.set_style(pango::Style::Italic);
        table.add(&t);
    }
    if table.lookup("strike").is_none() {
        let t = gtk::TextTag::builder().name("strike").build();
        t.set_strikethrough(true);
        table.add(&t);
    }
    if table.lookup("inline-code").is_none() {
        let t = gtk::TextTag::builder().name("inline-code").build();
        t.set_family(Some("monospace"));
        t.set_scale(0.88);
        // Mirror the colours used by the main buffer's `inline-code` tag in
        // `create_tags`, so code in cells reads identically to code in body.
        if is_dark {
            t.set_foreground(Some("#ffd6a5"));
            t.set_background(Some("#2a2826"));
        } else {
            t.set_foreground(Some("#b45309"));
            t.set_background(Some("#fef3c7"));
        }
        table.add(&t);
    }
}

fn insert_table_grid(
    text_view: &gtk::TextView,
    buffer: &gtk::TextBuffer,
    rows: &[Vec<Cell>],
    is_dark: bool,
) {
    if rows.iter().all(|r| r.is_empty()) {
        return;
    }

    // Outer wrap holds the rounded border + clips the inner grid lines.
    // hexpand alone is ignored by TextView::add_child_at_anchor (child
    // anchors allocate the widget at its natural width), so the wrap is
    // pinned to the content width (`register_table_wrap_resize`).
    //
    // The grid itself is mounted lazily (`mount_tables_near_viewport`): every
    // cell is a TextView with its own tag table, and a long document's
    // thousands of cells cost seconds of CSS + layout before the window could
    // even show. Until then the wrap is an empty frame of estimated height.
    let wrap = gtk::Box::new(gtk::Orientation::Vertical, 0);
    wrap.add_css_class("manuscript-table-wrap");
    wrap.set_overflow(gtk::Overflow::Hidden);
    wrap.set_halign(gtk::Align::Start);
    wrap.set_size_request(table_wrap_width(text_view), estimated_table_height(text_view, rows.len()));
    register_table_wrap_resize(text_view, &wrap);

    // Presses inside the table bubble up to the main TextView after the cell
    // has handled them: it put its cursor on the table's anchor (the table's
    // first line) and, on drag, scrolled to it — a tall table jumped to its
    // first row while selecting. Stop them here; the cells already saw them.
    // A legacy controller claims nothing, so the cell's own gestures run on.
    let block_presses = gtk::EventControllerLegacy::new();
    block_presses.connect_event(|_, event| {
        use gtk::gdk::EventType::*;
        match event.event_type() {
            ButtonPress | ButtonRelease | TouchBegin | TouchEnd => gtk::glib::Propagation::Stop,
            _ => gtk::glib::Propagation::Proceed,
        }
    });
    wrap.add_controller(block_presses);

    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "\n");
    let mut end = buffer.end_iter();
    let anchor = buffer.create_child_anchor(&mut end);
    text_view.add_child_at_anchor(&wrap, &anchor);
    // Child widgets aren't part of TextBuffer serialization; keep a Markdown
    // copy on the anchor so `setup_table_copy_handler` can put it on the clipboard.
    unsafe {
        anchor.set_data(TABLE_TEXT_KEY, table_to_markdown(rows));
    }
    register_pending_table(
        text_view,
        PendingTable { anchor, wrap: wrap.downgrade(), rows: rows.to_vec(), is_dark },
    );
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "\n");
}

/// Placeholder height for an unmounted table. Once tables have been mounted
/// in this view, their average measured row height (`TableCalibration`);
/// before that, one text line plus the cell's 4 + 4 px margins and 1 px
/// border per row. Wrapped cells make a real table taller.
fn estimated_table_height(text_view: &gtk::TextView, rows: usize) -> i32 {
    let row = table_calibration(text_view).row_height().unwrap_or_else(|| {
        let metrics = text_view.pango_context().metrics(None, None);
        (metrics.height() as f64 / pango::SCALE as f64).ceil().max(12.0) + 9.0
    });
    (rows as f64 * row).round() as i32 + 2
}

/// Measured table heights so far in one view, for the placeholder estimate.
/// Kept across re-renders (a Split re-render on every pause in typing would
/// otherwise start from the rough guess again).
#[derive(Default)]
struct TableCalibration {
    /// Content height (frame borders excluded) and rows of every mounted table.
    height: std::cell::Cell<f64>,
    rows: std::cell::Cell<f64>,
    /// Row height the pending placeholders were last sized with.
    applied: std::cell::Cell<f64>,
}

impl TableCalibration {
    fn row_height(&self) -> Option<f64> {
        (self.rows.get() > 0.0).then(|| self.height.get() / self.rows.get())
    }
}

fn table_calibration(text_view: &gtk::TextView) -> std::rc::Rc<TableCalibration> {
    static KEY: &str = "table-calibration";
    unsafe {
        if let Some(c) = text_view.data::<std::rc::Rc<TableCalibration>>(KEY) {
            return c.as_ref().clone();
        }
        let c = std::rc::Rc::new(TableCalibration::default());
        text_view.set_data(KEY, c.clone());
        c
    }
}

/// The cell TextViews of a table, attached to a fresh grid (row-major).
fn build_table_grid(rows: &[Vec<Cell>], is_dark: bool) -> (gtk::Grid, Vec<gtk::TextView>) {
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let grid = gtk::Grid::builder()
        .column_spacing(0)
        .row_spacing(0)
        .build();
    grid.add_css_class("manuscript-table");
    grid.set_hexpand(true);

    let row_count = rows.len();
    let empty_cell: Cell = Vec::new();
    let mut cell_views: Vec<gtk::TextView> = Vec::with_capacity(row_count * num_cols);

    for (row_idx, row) in rows.iter().enumerate() {
        for col_idx in 0..num_cols {
            let cell_runs: &Cell = row.get(col_idx).unwrap_or(&empty_cell);

            let tv = gtk::TextView::new();
            tv.set_editable(false);
            tv.set_cursor_visible(false);
            tv.set_wrap_mode(gtk::WrapMode::WordChar);
            tv.set_left_margin(8);
            tv.set_right_margin(8);
            tv.set_top_margin(4);
            tv.set_bottom_margin(4);
            tv.set_hexpand(true);
            // WordChar reports a 0 minimum width for empty cells inside a
            // child-anchor — same gotcha as GtkPicture with can_shrink=true.
            // Pin a sensible minimum so columns are usable.
            tv.set_size_request(120, -1);
            tv.add_css_class("manuscript-table-cell");
            if row_idx == 0 {
                tv.add_css_class("manuscript-table-header");
            }
            if row_idx > 0 && row_idx % 2 == 0 {
                tv.add_css_class("manuscript-table-row-even");
            }
            if col_idx + 1 == num_cols {
                tv.add_css_class("last-col");
            }
            if row_idx + 1 == row_count {
                tv.add_css_class("last-row");
            }

            let cell_buffer = tv.buffer();
            create_cell_inline_tags(&cell_buffer.tag_table(), is_dark);

            for run in cell_runs {
                let mut tag_names: Vec<&str> = Vec::new();
                if run.bold {
                    tag_names.push("bold");
                }
                if run.italic {
                    tag_names.push("italic");
                }
                if run.strike {
                    tag_names.push("strike");
                }
                if run.code {
                    tag_names.push("inline-code");
                }

                if let Some(url) = run.link.as_ref() {
                    // Unique per (cell-buffer, offset) — the cell buffer's
                    // own tag table grows, but each cell buffer is small
                    // and short-lived (re-rendered on every refresh).
                    let mut end = cell_buffer.end_iter();
                    let start_offset = end.offset();
                    let cell_ptr = cell_buffer.as_ptr() as usize;
                    let link_tag_name =
                        format!("linkurl-cell-{}-{}", cell_ptr, start_offset);
                    let is_wikilink = url.starts_with("manuscript://wikilink/");
                    let link_tag = gtk::TextTag::builder()
                        .name(&link_tag_name)
                        .foreground(if is_wikilink {
                            if is_dark {
                                "#c4a7e7"
                            } else {
                                "#7c3aed"
                            }
                        } else if is_dark {
                            "#78aeed"
                        } else {
                            "#1c71d8"
                        })
                        .underline(if is_wikilink {
                            pango::Underline::None
                        } else {
                            pango::Underline::Single
                        })
                        .build();
                    unsafe {
                        link_tag.set_data("url", url.clone());
                    }
                    if cell_buffer.tag_table().lookup(&link_tag_name).is_none() {
                        cell_buffer.tag_table().add(&link_tag);
                    }
                    let mut names: Vec<&str> = tag_names.clone();
                    names.push(&link_tag_name);
                    cell_buffer.insert_with_tags_by_name(&mut end, &run.text, &names);
                } else {
                    let mut end = cell_buffer.end_iter();
                    if tag_names.is_empty() {
                        cell_buffer.insert(&mut end, &run.text);
                    } else {
                        cell_buffer.insert_with_tags_by_name(
                            &mut end,
                            &run.text,
                            &tag_names,
                        );
                    }
                }
            }

            setup_cell_link_handler(&tv);
            grid.attach(&tv, col_idx as i32, row_idx as i32, 1, 1);
            cell_views.push(tv);
        }
    }
    (grid, cell_views)
}

struct PendingTable {
    anchor: gtk::TextChildAnchor,
    wrap: gtk::glib::WeakRef<gtk::Box>,
    rows: Vec<Vec<Cell>>,
    is_dark: bool,
}

type PendingTables = std::rc::Rc<std::cell::RefCell<Vec<PendingTable>>>;

const PENDING_TABLES_KEY: &str = "pending-tables";

fn pending_tables(text_view: &gtk::TextView) -> Option<PendingTables> {
    unsafe { text_view.data::<PendingTables>(PENDING_TABLES_KEY).map(|p| p.as_ref().clone()) }
}

/// Queue a table for lazy mounting. The first call on a TextView installs
/// the watcher: the enclosing ScrolledWindow's vadjustment `value-changed`
/// and `page-size` notify schedule one coalesced idle check each burst.
fn register_pending_table(text_view: &gtk::TextView, table: PendingTable) {
    let reg = match pending_tables(text_view) {
        Some(reg) => reg,
        None => {
            let reg: PendingTables = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            unsafe { text_view.set_data(PENDING_TABLES_KEY, reg.clone()) };
            if let Some(sw) = text_view
                .ancestor(gtk::ScrolledWindow::static_type())
                .and_downcast::<gtk::ScrolledWindow>()
            {
                let vadj = sw.vadjustment();
                let tv = text_view.downgrade();
                vadj.connect_value_changed(move |_| {
                    if let Some(tv) = tv.upgrade() {
                        schedule_table_mount(&tv);
                    }
                });
                let tv = text_view.downgrade();
                vadj.connect_page_size_notify(move |_| {
                    if let Some(tv) = tv.upgrade() {
                        schedule_table_mount(&tv);
                    }
                });
            }
            reg
        }
    };
    {
        let mut pending = reg.borrow_mut();
        // A re-render clears the buffer, which deletes every old anchor.
        pending.retain(|t| !t.anchor.is_deleted());
        pending.push(table);
    }
    schedule_table_mount(text_view);
}

fn schedule_table_mount(text_view: &gtk::TextView) {
    static SCHEDULED_KEY: &str = "table-mount-scheduled";
    unsafe {
        if text_view.data::<bool>(SCHEDULED_KEY).is_some() {
            return;
        }
        text_view.set_data(SCHEDULED_KEY, true);
    }
    let tv = text_view.downgrade();
    gtk::glib::idle_add_local_once(move || {
        let Some(tv) = tv.upgrade() else { return };
        unsafe {
            let _ = tv.steal_data::<bool>(SCHEDULED_KEY);
        }
        mount_tables_near_viewport(&tv);
    });
}

/// Mount the pending tables within a screen above and two below the
/// viewport. Nothing is mounted before the view has a size.
fn mount_tables_near_viewport(text_view: &gtk::TextView) {
    let Some(sw) = text_view
        .ancestor(gtk::ScrolledWindow::static_type())
        .and_downcast::<gtk::ScrolledWindow>()
    else {
        return;
    };
    let page = sw.vadjustment().page_size();
    if page <= 0.0 {
        return;
    }
    let (_, top) = text_view.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, 0);
    let (lo, hi) = (top as f64 - page, top as f64 + 2.0 * page);
    let buffer = text_view.buffer();
    mount_tables(text_view, |t| {
        let y = text_view.iter_location(&buffer.iter_at_child_anchor(&t.anchor)).y() as f64;
        let h = t.wrap.upgrade().map_or(0, |w| w.height_request()) as f64;
        y + h >= lo && y <= hi
    });
}

/// Mount every pending table — in-document search needs the cell buffers.
pub fn mount_all_tables(text_view: &gtk::TextView) {
    mount_tables(text_view, |_| true);
}

/// Mount the pending tables `select` picks. A table above the viewport
/// changes height when its real grid replaces the estimate, which would
/// shift the text being read; the line at the viewport top is held in place
/// over the next few frames, until the new heights are laid out.
fn mount_tables(text_view: &gtk::TextView, select: impl Fn(&PendingTable) -> bool) {
    let Some(reg) = pending_tables(text_view) else { return };
    let picked: Vec<PendingTable> = {
        let mut pending = reg.borrow_mut();
        pending.retain(|t| !t.anchor.is_deleted());
        let (picked, rest) = std::mem::take(&mut *pending).into_iter().partition(|t| select(t));
        *pending = rest;
        picked
    };
    if picked.is_empty() {
        return;
    }
    let buffer = text_view.buffer();
    let (_, top) = text_view.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, 0);
    let above_top = |anchor: &gtk::TextChildAnchor| {
        text_view.iter_location(&buffer.iter_at_child_anchor(anchor)).y() < top
    };
    let mut above = false;
    for t in picked {
        let Some(wrap) = t.wrap.upgrade() else { continue };
        above |= above_top(&t.anchor);
        let (grid, cells) = build_table_grid(&t.rows, t.is_dark);
        wrap.append(&grid);
        wrap.set_size_request(wrap.width_request(), -1);
        register_table_cells(text_view, &t.anchor, &cells);
        calibrate_on_layout(text_view, &wrap, t.rows.len());
    }
    tracing::debug!(top, above, "tables: mounted");
    if above {
        hold_viewport_top(text_view, top);
    }
}

/// Record a mounted table's real height once it is laid out — a TextView's
/// `measure` before its first allocation reports a fraction of it — and feed
/// it to the placeholder estimate. The first frames still carry the
/// placeholder's allocation (measuring those fed each estimate back into the
/// next, shrinking them table after table), so wait until the height holds
/// for two frames after the mount; give up after a second.
fn calibrate_on_layout(text_view: &gtk::TextView, wrap: &gtk::Box, rows: usize) {
    let tv = text_view.downgrade();
    let (frames, last) = (std::cell::Cell::new(0u32), std::cell::Cell::new(-1i32));
    wrap.add_tick_callback(move |wrap, _| {
        let Some(tv) = tv.upgrade() else { return gtk::glib::ControlFlow::Break };
        frames.set(frames.get() + 1);
        if frames.get() > 60 {
            return gtk::glib::ControlFlow::Break;
        }
        let h = wrap.height();
        let stable = frames.get() > 2 && h > 0 && h == last.replace(h);
        if !stable {
            return gtk::glib::ControlFlow::Continue;
        }
        let calib = table_calibration(&tv);
        tracing::debug!(h, rows, "tables: laid out");
        calib.height.set(calib.height.get() + (h - 2) as f64);
        calib.rows.set(calib.rows.get() + rows as f64);
        resize_placeholders(&tv);
        gtk::glib::ControlFlow::Break
    });
}

/// Re-size the pending placeholders once the measured average row height
/// has moved noticeably from the one they were sized with, so the scrollbar
/// stops shifting under a drag as tables mount.
fn resize_placeholders(text_view: &gtk::TextView) {
    let calib = table_calibration(text_view);
    let Some(row) = calib.row_height() else { return };
    let applied = calib.applied.get();
    if applied > 0.0 && (row - applied).abs() / applied <= 0.03 {
        return;
    }
    tracing::debug!(row, applied, "tables: re-estimate placeholders");
    calib.applied.set(row);
    let Some(reg) = pending_tables(text_view) else { return };
    let buffer = text_view.buffer();
    let (_, top) = text_view.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, 0);
    let mut above = false;
    for t in reg.borrow().iter().filter(|t| !t.anchor.is_deleted()) {
        let Some(wrap) = t.wrap.upgrade() else { continue };
        let h = estimated_table_height(text_view, t.rows.len());
        if wrap.height_request() != h {
            above |= text_view.iter_location(&buffer.iter_at_child_anchor(&t.anchor)).y() < top;
            wrap.set_size_request(wrap.width_request(), h);
        }
    }
    if above {
        hold_viewport_top(text_view, top);
    }
}

/// Keep the text now at buffer `top` at the top of the viewport for a few
/// frames while the layout above it changes height.
fn hold_viewport_top(text_view: &gtk::TextView, top: i32) {
    let Some(vadj) = text_view
        .ancestor(gtk::ScrolledWindow::static_type())
        .and_downcast::<gtk::ScrolledWindow>()
        .map(|sw| sw.vadjustment())
    else {
        return;
    };
    let Some(iter) = text_view.iter_at_location(text_view.left_margin() + 1, top) else { return };
    let offset = iter.offset();
    let delta = top - text_view.iter_location(&iter).y();
    tracing::debug!(top, offset, delta, "tables: hold viewport top");
    let frames = std::rc::Rc::new(std::cell::Cell::new(0u32));
    text_view.add_tick_callback(move |tv, _| {
        let y = tv.iter_location(&tv.buffer().iter_at_offset(offset)).y() + delta;
        let (_, now) = tv.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, 0);
        if y != now {
            vadj.set_value(vadj.value() + (y - now) as f64);
        }
        frames.set(frames.get() + 1);
        if frames.get() < 6 {
            gtk::glib::ControlFlow::Continue
        } else {
            gtk::glib::ControlFlow::Break
        }
    });
}

const TABLE_TEXT_KEY: &str = "manuscript-table-text";

/// GFM pipe table for a table block: header row, `| --- |` separator, body
/// rows. Ragged rows are padded with empty cells; `|` in text is escaped.
fn table_to_markdown(rows: &[Vec<Cell>]) -> String {
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return String::new();
    }
    let line = |row: &Vec<Cell>| -> String {
        let mut out = String::from("|");
        for i in 0..num_cols {
            let text = row.get(i).map(flatten_cell).unwrap_or_default();
            out.push(' ');
            out.push_str(&text.replace('\n', " ").replace('|', "\\|"));
            out.push_str(" |");
        }
        out
    };
    let mut lines = vec![line(&rows[0])];
    lines.push(format!("|{}", " --- |".repeat(num_cols)));
    lines.extend(rows[1..].iter().map(line));
    lines.join("\n")
}

/// Copy in the rendered views. A selection inside one rendered line copies
/// exactly what's selected (default handler). A selection spanning several
/// lines or a table copies the Markdown *source* lines it covers instead —
/// rendered text pasted into a .md loses its structure (`•` isn't a list
/// marker, headings lose `#`, single newlines join into one paragraph), and
/// a table anchor contributes nothing to the default copy at all. Source
/// lines come from the line map `render_markdown` stores on the view; if
/// that's unavailable, tables are spliced in from their anchors' Markdown.
fn setup_table_copy_handler(text_view: &gtk::TextView) {
    static HANDLER_KEY: &str = "table-copy-handler-set";
    unsafe {
        if text_view.data::<bool>(HANDLER_KEY).is_some() {
            return;
        }
        text_view.set_data(HANDLER_KEY, true);
    }

    text_view.connect_copy_clipboard(|tv| {
        let buffer = tv.buffer();
        let Some((start, end)) = buffer.selection_bounds() else {
            return;
        };
        // `slice` keeps U+FFFC for child anchors (tables, images).
        let selected = buffer.slice(&start, &end, true);
        if !selected.contains('\n') && !selected.contains('\u{FFFC}') {
            return;
        }

        // SAFETY: single-threaded GTK; keys only ever hold these types.
        let source = unsafe {
            let map = tv.data::<Vec<LineAnchor>>(LINE_MAP_KEY).map(|p| p.as_ref().clone());
            let src = tv.data::<String>(SOURCE_KEY).map(|p| p.as_ref().clone());
            map.zip(src)
                .and_then(|(map, src)| source_lines_for_range(&map, &src, start.offset(), end.offset()))
        };
        let text = match source {
            Some(md) => md,
            None => match spliced_with_tables(&buffer, &start, &end) {
                Some(t) => t,
                None => return,
            },
        };
        tv.clipboard().set_text(&text);
        tv.stop_signal_emission_by_name("copy-clipboard");
    });
}

/// Markdown source lines covering rendered offsets `start..end`: from the
/// line of the last anchor at or before `start` up to (not including) the
/// line of the first anchor after the last one before `end`. Leading and
/// trailing blank lines are trimmed. `None` if nothing usable.
fn source_lines_for_range(map: &[LineAnchor], source: &str, start: i32, end: i32) -> Option<String> {
    if map.is_empty() {
        return None;
    }
    let lines: Vec<&str> = source.lines().collect();
    let first = map.partition_point(|&(_, off)| off <= start);
    let l0 = if first == 0 { 0 } else { map[first - 1].0 as usize };
    let last = map.partition_point(|&(_, off)| off < end);
    let l1 = map.get(last).map(|&(l, _)| l as usize).unwrap_or(lines.len());
    let l1 = l1.min(lines.len());
    if l0 >= l1 {
        return None;
    }
    let mut slice = &lines[l0..l1];
    while slice.first().is_some_and(|l| l.trim().is_empty()) {
        slice = &slice[1..];
    }
    while slice.last().is_some_and(|l| l.trim().is_empty()) {
        slice = &slice[..slice.len() - 1];
    }
    (!slice.is_empty()).then(|| slice.join("\n"))
}

/// Fallback without a line map: the rendered selection text with each table
/// anchor replaced by its Markdown. `None` if the selection holds no table.
fn spliced_with_tables(buffer: &gtk::TextBuffer, start: &gtk::TextIter, end: &gtk::TextIter) -> Option<String> {
    let mut tables: Vec<(i32, String)> = Vec::new();
    let mut iter = *start;
    while iter < *end {
        if let Some(anchor) = iter.child_anchor() {
            let md = unsafe { anchor.data::<String>(TABLE_TEXT_KEY) }.map(|p| unsafe { p.as_ref() }.clone());
            if let Some(md) = md {
                tables.push((iter.offset(), md));
            }
        }
        if !iter.forward_char() {
            break;
        }
    }
    if tables.is_empty() {
        return None;
    }
    let mut text = String::new();
    let mut pos = *start;
    for (off, md) in tables {
        let anchor_iter = buffer.iter_at_offset(off);
        text.push_str(&buffer.text(&pos, &anchor_iter, false));
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&md);
        text.push('\n');
        pos = buffer.iter_at_offset(off + 1);
    }
    if pos < *end {
        text.push_str(&buffer.text(&pos, end, false));
    }
    Some(text)
}

type TableCellRegistry = std::rc::Rc<
    std::cell::RefCell<Vec<(gtk::TextChildAnchor, Vec<gtk::glib::WeakRef<gtk::TextView>>)>>,
>;

/// Remember a table's anchor + cell TextViews (row-major) on the main
/// TextView so in-document search can reach text inside cells.
fn register_table_cells(
    text_view: &gtk::TextView,
    anchor: &gtk::TextChildAnchor,
    cells: &[gtk::TextView],
) {
    static KEY: &str = "table-cell-registry";
    let registry: TableCellRegistry = unsafe {
        if let Some(existing) = text_view.data::<TableCellRegistry>(KEY) {
            existing.as_ref().clone()
        } else {
            let reg: TableCellRegistry =
                std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            text_view.set_data(KEY, reg.clone());
            reg
        }
    };
    let mut reg = registry.borrow_mut();
    // A re-render clears the buffer, which deletes every old anchor.
    reg.retain(|(a, _)| !a.is_deleted());
    reg.push((anchor.clone(), cells.iter().map(|c| c.downgrade()).collect()));
}

/// Tables currently mounted in `text_view`, in document order: the anchor's
/// char offset in the main buffer plus the cell TextViews (row-major).
/// Stale entries (anchor deleted by a re-render) are dropped.
pub fn table_cells(text_view: &gtk::TextView) -> Vec<(i32, Vec<gtk::TextView>)> {
    static KEY: &str = "table-cell-registry";
    let registry: Option<TableCellRegistry> =
        unsafe { text_view.data::<TableCellRegistry>(KEY).map(|p| p.as_ref().clone()) };
    let Some(registry) = registry else { return Vec::new() };
    let buffer = text_view.buffer();
    let mut reg = registry.borrow_mut();
    reg.retain(|(a, _)| !a.is_deleted());
    let mut out: Vec<(i32, Vec<gtk::TextView>)> = reg
        .iter()
        .map(|(a, cells)| {
            let off = buffer.iter_at_child_anchor(a).offset();
            (off, cells.iter().filter_map(|w| w.upgrade()).collect())
        })
        .collect();
    out.sort_by_key(|(o, _)| *o);
    out
}

/// Create `search-match` / `search-current` in a cell's own tag table.
/// `search-current` is created last so it outranks `search-match` and the
/// cell's inline tags (bold, code, link colours).
pub fn ensure_cell_search_tags(table: &gtk::TextTagTable) {
    if table.lookup("search-match").is_none() {
        let tag = gtk::TextTag::builder()
            .name("search-match")
            .background("#f5c211")
            .foreground("#000000")
            .build();
        table.add(&tag);
    }
    if table.lookup("search-current").is_none() {
        let tag = gtk::TextTag::builder()
            .name("search-current")
            .background("#33d17a")
            .foreground("#000000")
            .build();
        table.add(&tag);
        tag.set_priority(table.size() - 1);
    }
}

/// Remove search highlight tags from every cell of every mounted table.
pub fn clear_cell_search_tags(text_view: &gtk::TextView) {
    for (_, cells) in table_cells(text_view) {
        for cell in cells {
            let buf = cell.buffer();
            let (s, e) = (buf.start_iter(), buf.end_iter());
            for name in ["search-match", "search-current"] {
                if buf.tag_table().lookup(name).is_some() {
                    buf.remove_tag_by_name(name, &s, &e);
                }
            }
        }
    }
}

fn insert_table(buffer: &gtk::TextBuffer, rows: &[Vec<Cell>], _is_dark: bool) {
    if rows.is_empty() {
        return;
    }

    // Calculate column widths
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return;
    }

    // Pre-flatten every cell to a plain string once so the width measurement
    // and the line-building pass don't both walk the run vector.
    let flat_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(flatten_cell).collect::<Vec<_>>())
        .collect();

    let mut col_widths = vec![0usize; num_cols];
    for row in &flat_rows {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                col_widths[i] = col_widths[i].max(UnicodeWidthStr::width(cell.as_str())).max(3);
            }
        }
    }

    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "\n");

    // Render separator line
    let sep_line: String = col_widths
        .iter()
        .map(|w| "-".repeat(w + 2))
        .collect::<Vec<_>>()
        .join("+");
    let sep = format!("|{}|", sep_line);

    for (row_idx, row) in flat_rows.iter().enumerate() {
        // Build padded row
        let mut line = String::from("|");
        for (i, width) in col_widths.iter().enumerate() {
            let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
            line.push(' ');
            line.push_str(cell);
            // Pad with spaces to fill the monospace column (emoji/CJK take 2 columns)
            let visible_width = UnicodeWidthStr::width(cell);
            let pad = width.saturating_sub(visible_width) + 1;
            for _ in 0..pad {
                line.push(' ');
            }
            line.push('|');
        }

        let mut end = buffer.end_iter();
        if row_idx == 0 {
            // Header row: bold
            buffer.insert_with_tags_by_name(&mut end, &line, &["table", "table-header"]);
        } else {
            buffer.insert_with_tags_by_name(&mut end, &line, &["table"]);
        }
        buffer.insert(&mut end, "\n");

        // Separator after header
        if row_idx == 0 {
            let mut end = buffer.end_iter();
            buffer.insert_with_tags_by_name(&mut end, &sep, &["table"]);
            buffer.insert(&mut end, "\n");
        }
    }

    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "\n");
}

thread_local! {
    /// In-memory cache of remote image textures, keyed by URL. Cleared only on
    /// app exit — kept tiny and bounded by document size in practice.
    static URL_IMAGE_CACHE: std::cell::RefCell<std::collections::HashMap<String, gtk::gdk::Texture>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

thread_local! {
    /// Decoded local images with their display size, keyed by (path, mtime,
    /// scale factor). Every render — file open, theme switch, each Split
    /// re-render while typing — used to decode each image twice at full size
    /// on the main thread (once for its dimensions, again via
    /// `Picture::set_filename`). A hit costs one `stat`; an edited image (new
    /// mtime) is decoded afresh. The display size is cached too: probing it
    /// with `Pixbuf::file_info` fully parses an SVG every time.
    static FILE_IMAGE_CACHE: std::cell::RefCell<
        std::collections::HashMap<
            (std::path::PathBuf, Option<std::time::SystemTime>, i32, i32),
            (gtk::gdk::Texture, i32, i32),
        >,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Load a local image for display. Returns the decoded texture together with
/// the image's **natural** pixel size, which the caller needs in order to
/// honour a per-image width/height override.
///
/// The texture is decoded no larger than `decode_width × scale` — a 4000 px
/// photo shown at 360 px no longer occupies ~48 MB of RGBA. `decode_width` is
/// an upper bound: the caller passes whatever the image may end up drawn at,
/// so the decode stays cheap without capping what a document can request.
fn load_local_image(
    path: &Path,
    scale: i32,
    decode_width: i32,
) -> Option<(gtk::gdk::Texture, i32, i32)> {
    let scale = scale.max(2);
    let decode_width = decode_width.max(1);
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let key = (path.to_path_buf(), mtime, scale, decode_width);
    if let Some(hit) = FILE_IMAGE_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Some(hit);
    }
    let loaded = match gtk::gdk_pixbuf::Pixbuf::file_info(path) {
        Some((_, nw, nh)) => {
            let target = (decode_width * scale).min(nw).max(1);
            let pixbuf =
                gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, target, -1, true).ok()?;
            #[allow(deprecated)]
            let tex = gtk::gdk::Texture::for_pixbuf(&pixbuf);
            // Natural size, NOT the decoded size — the document may ask for
            // any width, and only the natural size keeps the aspect ratio.
            (tex, nw.max(1), nh.max(1))
        }
        None => {
            // Unknown to gdk-pixbuf's header probe — let GDK try a full decode.
            let tex = gtk::gdk::Texture::from_file(&gtk::gio::File::for_path(path)).ok()?;
            let (nw, nh) = (tex.width().max(1), tex.height().max(1));
            (tex, nw, nh)
        }
    };
    FILE_IMAGE_CACHE.with(|c| c.borrow_mut().insert(key, loaded.clone()));
    Some(loaded)
}

/// Scale `tw×th` down so the width is at most `cap` and at most `avail`.
/// Images smaller than both limits keep their natural size — they are never
/// upscaled. This is the unsized-image path; a per-image width/height goes
/// through `ImageSize::resolve` instead.
fn fit_image_dims(tw: i32, th: i32, cap: i32, avail: i32) -> (i32, i32) {
    let limit = cap.min(avail).max(1);
    if tw > limit {
        let h = (th as f64 * limit as f64 / tw as f64).round() as i32;
        (limit, h.max(1))
    } else {
        (tw.max(1), th.max(1))
    }
}

fn make_picture_widget() -> gtk::Picture {
    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    picture.set_halign(gtk::Align::Start);
    picture.set_overflow(gtk::Overflow::Hidden);
    picture.add_css_class("manuscript-image");
    picture
}

/// Mount `picture` at a new anchor. A Markdown image gets a line of its
/// own; `inline` (images from one HTML block — screenshot pairs, badge rows)
/// keeps them side by side on one line, separated by a space, and the caller
/// closes the line.
fn attach_picture(text_view: &gtk::TextView, buffer: &gtk::TextBuffer, picture: &gtk::Picture, inline: bool) {
    let mut last = buffer.end_iter();
    let at_line_start = !last.backward_char() || last.char() == '\n';
    let mut end = buffer.end_iter();
    if !inline {
        buffer.insert(&mut end, "\n");
    } else if !at_line_start {
        // Next to a previous picture of the same block — or after text.
        let after_picture = last.child_anchor().is_some();
        buffer.insert(&mut end, if after_picture { " " } else { "\n" });
    }
    let mut end = buffer.end_iter();
    let anchor = buffer.create_child_anchor(&mut end);
    text_view.add_child_at_anchor(picture, &anchor);
    if !inline {
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, "\n");
    }
}

fn insert_image(
    text_view: &gtk::TextView,
    buffer: &gtk::TextBuffer,
    src: &str,
    base_dir: &Path,
    inline: bool,
    size: ImageSize,
    max_width: i32,
) {
    if src.starts_with("http://") || src.starts_with("https://") {
        insert_remote_image(text_view, buffer, src, inline, size, max_width);
        return;
    }

    let p = base_dir.join(src);
    if !p.exists() {
        let mut end = buffer.end_iter();
        buffer.insert_with_tags_by_name(
            &mut end,
            &format!("[图片未找到: {}]", src),
            &["italic"],
        );
        return;
    }

    let picture = make_picture_widget();
    let avail = image_avail_width(text_view);
    // The decode bound: never larger than what this image may be drawn at.
    // A declared width raises it; otherwise the default cap applies.
    let decode_width = size
        .width
        .map(|w| match w {
            SizeSpec::Px(px) => px.min(avail),
            SizeSpec::Percent(pct) => ((avail as f64) * pct / 100.0).round() as i32,
        })
        .unwrap_or(max_width)
        .min(avail)
        .max(1);

    match load_local_image(&p, text_view.scale_factor(), decode_width) {
        Some((tex, nat_w, nat_h)) => {
            let (dw, dh) = size.resolve(nat_w, nat_h, avail, max_width);
            picture.set_paintable(Some(&tex));
            picture.set_size_request(dw, dh);
        }
        None => {
            let (dw, dh) = size.resolve(320, 240, avail, max_width);
            picture.set_size_request(dw, dh);
        }
    }
    attach_picture(text_view, buffer, &picture, inline);
}

/// Width physically available to an inline image: the TextView's usable
/// content width. Falls back to a sensible default before first allocation.
fn image_avail_width(text_view: &gtk::TextView) -> i32 {
    let margins = text_view.left_margin() + text_view.right_margin();
    let tv_w = text_view.width();
    let sw_w = text_view
        .ancestor(gtk::ScrolledWindow::static_type())
        .map(|sw| sw.width())
        .unwrap_or(0);
    let avail = match (tv_w > 0, sw_w > 0) {
        (true, true) => tv_w.min(sw_w),
        (true, false) => tv_w,
        (false, true) => sw_w,
        (false, false) => 800 + margins,
    };
    (avail - margins - 8).max(64)
}

/// Insert a placeholder Picture and asynchronously fetch the URL via gio.
/// On success the texture is cached and applied to the widget; on failure the
/// placeholder remains (a thin neutral box).
fn insert_remote_image(
    text_view: &gtk::TextView,
    buffer: &gtk::TextBuffer,
    src: &str,
    inline: bool,
    size: ImageSize,
    max_width: i32,
) {
    use gtk::gio::prelude::FileExt;

    let avail = image_avail_width(text_view);
    let picture = make_picture_widget();
    // Placeholder box until the bytes arrive — also honours a declared size,
    // so the layout doesn't jump when the texture lands.
    let (ph_w, ph_h) = size.resolve(avail.min(max_width.max(1)), 200, avail, max_width);
    picture.set_size_request(ph_w, ph_h);

    if let Some(tex) = URL_IMAGE_CACHE.with(|c| c.borrow().get(src).cloned()) {
        let (w, h) = size.resolve(tex.width(), tex.height(), avail, max_width);
        picture.set_size_request(w, h);
        picture.set_paintable(Some(&tex));
        attach_picture(text_view, buffer, &picture, inline);
        return;
    }

    attach_picture(text_view, buffer, &picture, inline);

    let file = gtk::gio::File::for_uri(src);
    let pic_weak = picture.downgrade();
    let src_owned = src.to_string();
    file.load_bytes_async(gtk::gio::Cancellable::NONE, move |res| {
        let bytes = match res {
            Ok((bytes, _etag)) => bytes,
            Err(_) => return,
        };
        let stream = gtk::gio::MemoryInputStream::from_bytes(&bytes);
        let pixbuf = match gtk::gdk_pixbuf::Pixbuf::from_stream(
            &stream,
            gtk::gio::Cancellable::NONE,
        ) {
            Ok(p) => p,
            Err(_) => return,
        };
        let tex = gtk::gdk::Texture::for_pixbuf(&pixbuf);
        URL_IMAGE_CACHE.with(|c| {
            c.borrow_mut().insert(src_owned, tex.clone());
        });
        if let Some(pic) = pic_weak.upgrade() {
            let (w, h) = size.resolve(tex.width(), tex.height(), avail, max_width);
            pic.set_size_request(w, h);
            pic.set_paintable(Some(&tex));
        }
    });
}

/// Width to pin a table wrap to: the TextView's usable content width, but
/// never wider than the enclosing ScrolledWindow's viewport. The pinned
/// request becomes part of the TextView's minimum width, so tracking the
/// TextView alone is a ratchet — it can grow but never shrink back (a Split
/// divider couldn't narrow the preview past a table). The viewport is free
/// to shrink, and once the pin follows it the TextView shrinks too.
fn table_wrap_width(text_view: &gtk::TextView) -> i32 {
    let margins = text_view.left_margin() + text_view.right_margin();
    let tv_w = text_view.width();
    let sw_w = text_view
        .ancestor(gtk::ScrolledWindow::static_type())
        .map(|sw| sw.width())
        .unwrap_or(0);
    let avail = match (tv_w > 0, sw_w > 0) {
        (true, true) => tv_w.min(sw_w),
        (true, false) => tv_w,
        (false, true) => sw_w,
        // Before first allocation — a default that fits AdwClamp's typical
        // max content width.
        (false, false) => 720 + margins,
    };
    (avail - margins - 8).max(320)
}

type TableWrapRegistry = std::rc::Rc<std::cell::RefCell<Vec<gtk::glib::WeakRef<gtk::Box>>>>;

/// Subscribe a table wrap to the viewport width. The TextView keeps a
/// single watcher (installed once via `set_data`) that walks a registry of
/// live wraps and re-pins each one's `set_size_request` to the current
/// usable content width.
///
/// Signal: `page-size` on the enclosing ScrolledWindow's hadjustment, which
/// changes whenever the viewport width does (window resize, sidebar reveal,
/// Split divider drag) and also fires on the first allocation — tables
/// rendered before the view was allocated start at a fallback width and get
/// corrected here. NOT `notify::width`: GtkWidget has no `width` property in
/// GTK4, so that handler never fired (the pre-v1.5.2 "reactive width" was
/// silently dead). The re-pin runs in an idle so `width()` reads the
/// finished allocation, coalesced to one idle per burst.
///
/// Why this doesn't loop: the pin does feed the TextView's minimum width
/// (hence `table_wrap_width` caps at the viewport), but the viewport width —
/// and so `page-size` — doesn't depend on the pin, and the `cur_w != new_w`
/// guard makes a repeat re-pin a no-op.
fn register_table_wrap_resize(text_view: &gtk::TextView, wrap: &gtk::Box) {
    static REGISTRY_KEY: &str = "table-resize-watcher";

    let registry: TableWrapRegistry = unsafe {
        if let Some(existing) = text_view.data::<TableWrapRegistry>(REGISTRY_KEY) {
            existing.as_ref().clone()
        } else {
            let reg: TableWrapRegistry = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            text_view.set_data(REGISTRY_KEY, reg.clone());

            if let Some(sw) = text_view
                .ancestor(gtk::ScrolledWindow::static_type())
                .and_downcast::<gtk::ScrolledWindow>()
            {
                let tv_weak = text_view.downgrade();
                let reg_for_cb = reg.clone();
                let pending = std::rc::Rc::new(std::cell::Cell::new(false));
                sw.hadjustment().connect_page_size_notify(move |_| {
                    if pending.replace(true) {
                        return;
                    }
                    let tv_weak = tv_weak.clone();
                    let reg = reg_for_cb.clone();
                    let pending = pending.clone();
                    gtk::glib::idle_add_local_once(move || {
                        pending.set(false);
                        let Some(tv) = tv_weak.upgrade() else { return };
                        let new_w = table_wrap_width(&tv);
                        reg.borrow_mut().retain(|w| {
                            let Some(b) = w.upgrade() else { return false };
                            let (cur_w, cur_h) = b.size_request();
                            if cur_w != new_w {
                                // Keep an unmounted table's estimated height.
                                b.set_size_request(new_w, cur_h);
                            }
                            true
                        });
                    });
                });
            }

            reg
        }
    };

    let weak = wrap.downgrade();
    registry.borrow_mut().push(weak);
}

/// Open a URL through the desktop's default handler. `manuscript://wikilink/`
/// URLs are dispatched to the window's `win.open-wikilink` action (routed via
/// the widget's action muxer — no window reference needed here).
fn open_link_url(text_view: &gtk::TextView, url: &str) {
    if let Some(rest) = url.strip_prefix("manuscript://wikilink/") {
        let name = percent_decode(rest);
        let _ = text_view.activate_action("win.open-wikilink", Some(&name.to_variant()));
        return;
    }
    let _ = gtk::gio::AppInfo::launch_default_for_uri(
        url,
        None::<&gtk::gio::AppLaunchContext>,
    );
}

/// Translate a click at widget coords (x, y) on `text_view` to a buffer iter,
/// look for a `linkurl-*` tag at that iter, and open its stashed URL.
///
/// Shared by main-buffer and rich-table cell TextViews — both stash the URL
/// via `unsafe tag.set_data("url", String)` on a tag whose name starts with
/// `linkurl-`.
fn handle_link_click(text_view: &gtk::TextView, x: f64, y: f64) {
    let (bx, by) =
        text_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    if let Some(iter) = text_view.iter_at_location(bx, by) {
        for tag in iter.tags() {
            if let Some(name) = tag.name() {
                if name.starts_with("linkurl-") {
                    let url: Option<String> =
                        unsafe { tag.data::<String>("url").map(|p| p.as_ref().clone()) };
                    if let Some(url) = url {
                        open_link_url(text_view, &url);
                    }
                    return;
                }
            }
        }
    }
}

/// Set up click handler for links in the text view.
fn setup_link_handler(text_view: &gtk::TextView) {
    static HANDLER_KEY: &str = "link-handler-set";
    unsafe {
        if text_view.data::<bool>(HANDLER_KEY).is_some() {
            return;
        }
        text_view.set_data(HANDLER_KEY, true);
    }

    let hand_cursor = gtk::gdk::Cursor::from_name("pointer", None);
    let text_cursor = gtk::gdk::Cursor::from_name("text", None);

    let hovering_link = std::rc::Rc::new(std::cell::Cell::new(false));
    let hovering_link2 = hovering_link.clone();

    let motion = gtk::EventControllerMotion::new();
    let tv = text_view.clone();
    let hand = hand_cursor.clone();
    let text = text_cursor.clone();
    motion.connect_motion(move |_, x, y| {
        let (bx, by) =
            tv.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        if let Some(iter) = tv.iter_at_location(bx, by) {
            let on_link = iter
                .tags()
                .iter()
                .any(|t| t.name().map(|n| n.starts_with("linkurl-")).unwrap_or(false));
            if on_link != hovering_link2.get() {
                hovering_link2.set(on_link);
                tv.set_cursor(if on_link {
                    hand.as_ref()
                } else {
                    text.as_ref()
                });
            }
        }
    });
    text_view.add_controller(motion);

    let click = gtk::GestureClick::new();
    click.set_button(1);
    let tv = text_view.clone();
    click.connect_released(move |_, _, x, y| {
        handle_link_click(&tv, x, y);
    });
    text_view.add_controller(click);
}

/// Install a click handler on a rich-table cell TextView so its
/// `linkurl-cell-*` tags dispatch through the same opener as main-buffer links.
///
/// Idempotent — flags the TextView via `unsafe set_data` so repeated calls on
/// the same widget don't stack controllers (cells are recreated on each render
/// so this is mostly belt-and-braces).
fn setup_cell_link_handler(cell_view: &gtk::TextView) {
    static HANDLER_KEY: &str = "cell-link-handler-set";
    unsafe {
        if cell_view.data::<bool>(HANDLER_KEY).is_some() {
            return;
        }
        cell_view.set_data(HANDLER_KEY, true);
    }

    let click = gtk::GestureClick::new();
    click.set_button(1);
    let tv = cell_view.clone();
    click.connect_released(move |_, _, x, y| {
        handle_link_click(&tv, x, y);
    });
    cell_view.add_controller(click);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(t: &str) -> Cell {
        vec![TextRun {
            text: t.to_string(),
            bold: false,
            italic: false,
            strike: false,
            code: false,
            link: None,
        }]
    }

    #[test]
    fn table_markdown_basic_escape_empty_ragged() {
        let rows = vec![
            vec![cell("a"), cell("b|c"), cell("")],
            vec![cell("1")],
        ];
        assert_eq!(
            table_to_markdown(&rows),
            "| a | b\\|c |  |\n| --- | --- | --- |\n| 1 |  |  |"
        );
        assert_eq!(table_to_markdown(&[]), "");
    }

    #[test]
    fn source_lines_cover_selection() {
        let src = "# Title\n\npara one\n\n- a\n- b\n\n| x | y |\n| - | - |\n| 1 | 2 |\n\nend\n";
        // (source line, rendered offset) for: title, para, item a, item b, table, end.
        let map = vec![(0, 0), (2, 6), (4, 16), (5, 19), (7, 22), (11, 24)];
        // From inside the paragraph to inside item b.
        assert_eq!(
            source_lines_for_range(&map, src, 8, 20).as_deref(),
            Some("para one\n\n- a\n- b")
        );
        // Just the table anchor: whole table, trailing blank trimmed.
        assert_eq!(
            source_lines_for_range(&map, src, 22, 23).as_deref(),
            Some("| x | y |\n| - | - |\n| 1 | 2 |")
        );
        // Through the last block.
        assert_eq!(source_lines_for_range(&map, src, 22, 30).as_deref(), Some("| x | y |\n| - | - |\n| 1 | 2 |\n\nend"));
        assert!(source_lines_for_range(&[], src, 0, 5).is_none());
    }

    #[test]
    fn source_headings_lines() {
        let md = "---\ntitle: x\n---\n# One\n\ntext\n\n## Two `code`\n```\n# not a heading\n```\n### Three\n";
        assert_eq!(
            collect_source_headings(md),
            vec![
                (1, "One".to_string(), 3),
                (2, "Two code".to_string(), 7),
                (3, "Three".to_string(), 11),
            ]
        );
    }

    #[test]
    fn img_sources_from_html() {
        let html = r#"<p align="center"><img src="data/a.png" width="45%"> &nbsp; <IMG alt='x' SRC='b.svg'/><img data-src="no.png"></p>"#;
        assert_eq!(html_img_sources(html), vec!["data/a.png", "b.svg"]);
        assert_eq!(html_img_sources("<img src=c/d.png>"), vec!["c/d.png"]);
    }

    // --- per-image sizing -------------------------------------------------

    #[test]
    fn size_spec_parses_px_and_percent() {
        assert_eq!(SizeSpec::parse("640"), Some(SizeSpec::Px(640)));
        assert_eq!(SizeSpec::parse("640px"), Some(SizeSpec::Px(640)));
        assert_eq!(SizeSpec::parse(" 42 "), Some(SizeSpec::Px(42)));
        assert_eq!(SizeSpec::parse("45%"), Some(SizeSpec::Percent(45.0)));
        assert_eq!(SizeSpec::parse("12.5%"), Some(SizeSpec::Percent(12.5)));
        // Nothing we can honour → None, so the image keeps its natural size.
        assert_eq!(SizeSpec::parse(""), None);
        assert_eq!(SizeSpec::parse("auto"), None);
        assert_eq!(SizeSpec::parse("0"), None);
        assert_eq!(SizeSpec::parse("-5"), None);
        assert_eq!(SizeSpec::parse("0%"), None);
    }

    #[test]
    fn html_img_width_attribute_is_honoured() {
        let got = parse_html_images(r#"<img src="a.png" width="640">"#);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "a.png");
        assert_eq!(got[0].1.width, Some(SizeSpec::Px(640)));

        let pct = parse_html_images(r#"<img src="b.png" width="45%">"#);
        assert_eq!(pct[0].1.width, Some(SizeSpec::Percent(45.0)));

        let both = parse_html_images(r#"<img src="c.png" width="300" height="120">"#);
        assert_eq!(both[0].1.width, Some(SizeSpec::Px(300)));
        assert_eq!(both[0].1.height, Some(SizeSpec::Px(120)));
    }

    #[test]
    fn html_img_style_attribute_is_honoured() {
        let got = parse_html_images(r#"<img src="a.png" style="width: 250px; height: 50%">"#);
        assert_eq!(got[0].1.width, Some(SizeSpec::Px(250)));
        assert_eq!(got[0].1.height, Some(SizeSpec::Percent(50.0)));
    }

    #[test]
    fn html_img_capitalised_attributes_work() {
        let got = parse_html_images(r#"<IMG SRC="a.png" WIDTH="320">"#);
        assert_eq!(got[0].0, "a.png");
        assert_eq!(got[0].1.width, Some(SizeSpec::Px(320)));
    }

    #[test]
    fn html_img_without_size_has_empty_size() {
        let got = parse_html_images(r#"<img src="a.png" alt="x">"#);
        assert!(got[0].1.is_empty());
    }

    #[test]
    fn markdown_size_syntax_becomes_img_tag() {
        let out = expand_image_sizes("![alt](pic.png){width=640}\n");
        assert!(out.contains("<img"), "got: {out}");
        assert!(out.contains("src=\"pic.png\""), "got: {out}");
        assert!(out.contains("width=\"640px\""), "got: {out}");
        // The original Markdown must be gone.
        assert!(!out.contains("!["), "got: {out}");
    }

    #[test]
    fn markdown_size_syntax_percent_and_height() {
        let pct = expand_image_sizes("![a](p.png){width=45%}");
        assert!(pct.contains("width=\"45%\""), "got: {pct}");

        let h = expand_image_sizes("![a](p.png){height=300}");
        assert!(h.contains("height=\"300px\""), "got: {h}");
    }

    #[test]
    fn markdown_image_without_size_is_untouched() {
        let src = "![alt](pic.png) and text";
        assert_eq!(expand_image_sizes(src), src);
    }

    #[test]
    fn markdown_size_in_code_block_is_untouched() {
        let doc = "```\n![a](p.png){width=640}\n```\n";
        assert_eq!(expand_image_sizes(doc), doc);
    }

    #[test]
    fn markdown_size_in_inline_code_is_untouched() {
        let doc = "use `![a](p.png){width=640}` here";
        assert_eq!(expand_image_sizes(doc), doc);
    }

    #[test]
    fn unresolvable_size_braces_are_untouched() {
        // Something else in braces after an image — not our syntax.
        let doc = "![a](p.png){.cls}";
        assert_eq!(expand_image_sizes(doc), doc);
    }

    #[test]
    fn size_resolve_prefers_declared_width() {
        let size = ImageSize { width: Some(SizeSpec::Px(500)), height: None };
        // 1000x500 natural → declared width wins, height follows ratio.
        assert_eq!(size.resolve(1000, 500, 800, 900), (500, 250));
    }

    #[test]
    fn size_resolve_caps_at_available_width() {
        let size = ImageSize { width: Some(SizeSpec::Px(5000)), height: None };
        // Never wider than the text column.
        assert_eq!(size.resolve(1000, 500, 600, 900).0, 600);
    }

    #[test]
    fn size_resolve_height_only_follows_ratio() {
        let size = ImageSize { width: None, height: Some(SizeSpec::Px(100)) };
        // 1000x500 natural, height 100 → width 200.
        assert_eq!(size.resolve(1000, 500, 800, 900), (200, 100));
    }

    #[test]
    fn size_resolve_percent_uses_available_width() {
        let size = ImageSize { width: Some(SizeSpec::Percent(50.0)), height: None };
        assert_eq!(size.resolve(1000, 500, 800, 900).0, 400);
    }

    #[test]
    fn size_resolve_unsized_caps_but_never_upscales() {
        let none = ImageSize::default();
        // Larger than the cap → scaled down to the cap.
        assert_eq!(none.resolve(2000, 1000, 2000, 900), (900, 450));
        // Smaller than the cap → left alone.
        assert_eq!(none.resolve(300, 150, 2000, 900), (300, 150));
    }
}
