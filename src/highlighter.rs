use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::{SyntaxReference, SyntaxSet};

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

/// Find syntax definition by language name (e.g. "rust", "python", "js").
pub fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    ss.find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
}

/// Eagerly load syntax/theme sets. Call from a background thread at startup so the
/// first code block render doesn't pay the ~100ms init cost.
pub fn prewarm() {
    let _ = syntax_set();
    let _ = theme_set();
}

/// A highlighted span with *char* offsets (TextBuffer offsets count chars,
/// not bytes) and RGB foreground color.
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub fg: (u8, u8, u8),
}

pub type Highlighted = std::rc::Rc<Vec<Vec<HighlightSpan>>>;

thread_local! {
    /// Highlight results keyed by (syntax, code, theme). syntect is the most
    /// expensive part of a render, and every re-render — each pause while
    /// typing in Split, theme switches, re-opening a file — used to redo
    /// every code block although they rarely change. Bounded by clearing.
    static CACHE: std::cell::RefCell<std::collections::HashMap<u64, Highlighted>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

const CACHE_MAX: usize = 256;

/// Highlight code lines with syntect, memoised. Returns per-line spans.
pub fn highlight_code(syntax: &SyntaxReference, lines: &[&str], is_dark: bool) -> Highlighted {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    syntax.name.hash(&mut h);
    lines.hash(&mut h);
    is_dark.hash(&mut h);
    let key = h.finish();
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let result = std::rc::Rc::new(highlight_uncached(syntax, lines, is_dark));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_MAX {
            c.clear();
        }
        c.insert(key, result.clone());
    });
    result
}

fn highlight_uncached(syntax: &SyntaxReference, lines: &[&str], is_dark: bool) -> Vec<Vec<HighlightSpan>> {
    let ss = syntax_set();
    let ts = theme_set();
    let theme_name = if is_dark {
        "base16-ocean.dark"
    } else {
        "base16-ocean.light"
    };
    let theme = ts
        .themes
        .get(theme_name)
        .unwrap_or(&ts.themes["base16-ocean.dark"]);

    let mut h = HighlightLines::new(syntax, theme);
    let mut result = Vec::with_capacity(lines.len());

    for line in lines {
        let spans = h.highlight_line(line, ss);
        match spans {
            Ok(ranges) => {
                let mut line_spans = Vec::new();
                let mut offset = 0;
                for (style, text) in &ranges {
                    let len = text.chars().count();
                    if len > 0 {
                        line_spans.push(HighlightSpan {
                            start: offset,
                            end: offset + len,
                            fg: (style.foreground.r, style.foreground.g, style.foreground.b),
                        });
                    }
                    offset += len;
                }
                result.push(line_spans);
            }
            Err(_) => result.push(Vec::new()),
        }
    }

    result
}
