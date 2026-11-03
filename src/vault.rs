//! Pure vault logic: wikilink resolution and note creation. No GTK.
use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 12;
const MAX_FILES: usize = 5000;

/// The vault root: the explicit config directory if it exists, otherwise the
/// parent directory of the currently-open file.
pub fn vault_root(vault_root_cfg: Option<&str>, current_file: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = vault_root_cfg {
        let p = PathBuf::from(root);
        if p.is_dir() {
            return Some(p);
        }
    }
    current_file.and_then(|f| f.parent().map(|p| p.to_path_buf()))
}

/// Whether the vault sidebar should be shown on launch. It is auto-shown only
/// when a vault folder is pinned (`vault_root` set) AND the user has not
/// collapsed it (`vault_sidebar_shown`). A single file (no `vault_root`) never
/// auto-opens the sidebar, and a persisted collapsed state is respected.
pub fn should_show_sidebar(vault_root_cfg: Option<&str>, vault_sidebar_shown: bool) -> bool {
    vault_root_cfg.is_some() && vault_sidebar_shown
}

/// Interpret a wikilink target as a path relative to the vault root, appending
/// `.md` when it has no extension. Returns `None` if the target would escape the
/// vault: absolute paths (`/x`, `C:\x`) and any `..` parent-traversal are refused.
/// Plain and `.`/current-dir components are allowed.
fn safe_relative(name: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut rel = PathBuf::from(name.trim());
    if rel.extension().is_none() {
        rel.set_extension("md");
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => return None, // RootDir / Prefix (absolute) / ParentDir (..)
        }
    }
    Some(rel)
}

/// Resolve a wikilink target to a file path within `root`.
///
/// - `sub/note` (contains `/`) is treated as a path relative to `root`.
/// - `note` (basename) is matched case-insensitively across a bounded recursive
///   walk; on collision the shortest path (shallowest) wins.
pub fn resolve(root: &Path, name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    if name.contains('/') {
        let rel = safe_relative(name)?;
        let candidate = root.join(rel);
        return candidate.is_file().then_some(candidate);
    }

    let target = if Path::new(name).extension().is_some() {
        name.to_string()
    } else {
        format!("{name}.md")
    };
    let target_lower = target.to_lowercase();

    let mut best: Option<PathBuf> = None;
    let mut best_depth = usize::MAX;
    let mut count = 0usize;
    walk(root, 0, &mut count, &mut |path, depth| {
        if depth < best_depth {
            if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                if fname.to_lowercase() == target_lower {
                    best = Some(path.to_path_buf());
                    best_depth = depth;
                }
            }
        }
    });
    best
}

/// Bounded recursive file walk. Skips hidden (dot) entries; stops at `MAX_DEPTH`
/// or once `MAX_FILES` files have been visited. `f` is called per regular file
/// with its depth (root files are depth 0).
fn walk(dir: &Path, depth: usize, count: &mut usize, f: &mut impl FnMut(&Path, usize)) {
    if depth > MAX_DEPTH || *count > MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if *count > MAX_FILES {
            return;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            walk(&path, depth + 1, count, f);
        } else if ft.is_file() {
            *count += 1;
            f(&path, depth);
        }
    }
}

/// Create `<root>/<name>.md` (nested parent dirs made as needed), seeded with a
/// `# <stem>` heading. If the file already exists it is returned untouched.
/// Targets that would escape the vault (absolute paths, `..` traversal) are
/// refused with an `InvalidInput` error — same containment rule as `resolve`.
pub fn create_note(root: &Path, name: &str) -> std::io::Result<PathBuf> {
    let rel = safe_relative(name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "note path escapes the vault",
        )
    })?;
    let path = root.join(rel);
    if path.is_file() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Untitled");
    std::fs::write(&path, format!("# {stem}\n"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn vault_root_prefers_explicit_dir() {
        let dir = tempdir().unwrap();
        let explicit = dir.path().to_string_lossy().to_string();
        let file = dir.path().join("sub").join("note.md");
        let got = vault_root(Some(&explicit), Some(&file));
        assert_eq!(got, Some(dir.path().to_path_buf()));
    }

    #[test]
    fn vault_root_falls_back_to_file_parent() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("note.md");
        let got = vault_root(None, Some(&file));
        assert_eq!(got, Some(dir.path().to_path_buf()));
    }

    #[test]
    fn vault_root_none_when_nothing_available() {
        assert_eq!(vault_root(None, None), None);
    }

    #[test]
    fn vault_root_ignores_missing_explicit_dir() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("note.md");
        let got = vault_root(Some("/no/such/dir/anywhere"), Some(&file));
        assert_eq!(got, Some(dir.path().to_path_buf()));
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn resolve_sibling() {
        let d = tempdir().unwrap();
        touch(&d.path().join("note.md"));
        assert_eq!(resolve(d.path(), "note"), Some(d.path().join("note.md")));
    }

    #[test]
    fn resolve_nested_by_basename() {
        let d = tempdir().unwrap();
        touch(&d.path().join("sub/deep/note.md"));
        assert_eq!(resolve(d.path(), "note"), Some(d.path().join("sub/deep/note.md")));
    }

    #[test]
    fn resolve_collision_shortest_path_wins() {
        let d = tempdir().unwrap();
        touch(&d.path().join("note.md"));
        touch(&d.path().join("a/b/note.md"));
        assert_eq!(resolve(d.path(), "note"), Some(d.path().join("note.md")));
    }

    #[test]
    fn resolve_case_insensitive() {
        let d = tempdir().unwrap();
        touch(&d.path().join("note.md"));
        assert_eq!(resolve(d.path(), "Note"), Some(d.path().join("note.md")));
    }

    #[test]
    fn resolve_relative_form() {
        let d = tempdir().unwrap();
        touch(&d.path().join("sub/note.md"));
        assert_eq!(resolve(d.path(), "sub/note"), Some(d.path().join("sub/note.md")));
    }

    #[test]
    fn resolve_extension_not_doubled() {
        let d = tempdir().unwrap();
        touch(&d.path().join("note.md"));
        assert_eq!(resolve(d.path(), "note.md"), Some(d.path().join("note.md")));
    }

    #[test]
    fn resolve_miss_returns_none() {
        let d = tempdir().unwrap();
        assert_eq!(resolve(d.path(), "ghost"), None);
    }

    #[test]
    fn resolve_skips_hidden_dirs() {
        let d = tempdir().unwrap();
        touch(&d.path().join(".trash/note.md"));
        assert_eq!(resolve(d.path(), "note"), None);
    }

    #[test]
    fn resolve_rejects_absolute_path_even_when_target_exists() {
        let vault = tempdir().unwrap();
        let outside = tempdir().unwrap();
        // A real file outside the vault; without the guard, root.join(<abs>)
        // discards the vault and resolves straight to it.
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, b"x").unwrap();
        let abs_no_ext = outside.path().join("secret"); // ".md" is appended by resolve
        assert_eq!(resolve(vault.path(), &abs_no_ext.to_string_lossy()), None);
    }

    #[test]
    fn resolve_rejects_parent_traversal_even_when_target_exists() {
        let parent = tempdir().unwrap();
        let vault = parent.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        // A real file in the vault's PARENT; "../secret" would reach it without the guard.
        std::fs::write(parent.path().join("secret.md"), b"x").unwrap();
        assert_eq!(resolve(&vault, "../secret"), None);
        assert_eq!(resolve(&vault, "sub/../../secret"), None);
    }

    #[test]
    fn create_note_at_root() {
        let d = tempdir().unwrap();
        let p = create_note(d.path(), "New Note").unwrap();
        assert_eq!(p, d.path().join("New Note.md"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "# New Note\n");
    }

    #[test]
    fn create_note_nested_makes_dirs() {
        let d = tempdir().unwrap();
        let p = create_note(d.path(), "sub/child").unwrap();
        assert_eq!(p, d.path().join("sub/child.md"));
        assert!(p.is_file());
    }

    #[test]
    fn create_note_idempotent_no_clobber() {
        let d = tempdir().unwrap();
        let existing = d.path().join("keep.md");
        std::fs::write(&existing, "ORIGINAL").unwrap();
        let p = create_note(d.path(), "keep").unwrap();
        assert_eq!(p, existing);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "ORIGINAL");
    }

    #[test]
    fn create_note_refuses_traversal() {
        let parent = tempdir().unwrap();
        let vault = parent.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        // Must NOT create a file outside the vault.
        assert!(create_note(&vault, "../escape").is_err());
        assert!(create_note(&vault, "/tmp/escape").is_err());
        assert!(!parent.path().join("escape.md").exists());
    }

    #[test]
    fn should_show_sidebar_respects_persisted_collapsed() {
        // Folder pinned but user collapsed the sidebar → stays hidden (the bug).
        assert!(!should_show_sidebar(Some("/some/vault"), false));
        // Folder pinned and shown → shown.
        assert!(should_show_sidebar(Some("/some/vault"), true));
        // No vault (single file) never auto-opens, regardless of the stored flag.
        assert!(!should_show_sidebar(None, true));
        assert!(!should_show_sidebar(None, false));
    }
}
