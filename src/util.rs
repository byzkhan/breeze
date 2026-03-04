use std::path::{Component, Path, PathBuf};

/// Normalize a path by resolving `..` and `.` components without touching the filesystem.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => { /* skip . */ }
            c => normalized.push(c.as_os_str()),
        }
    }
    normalized
}

/// Resolve a path against the agent CWD and verify it stays within CWD.
/// Rejects empty or root-level CWD to prevent sandbox bypass.
pub fn resolve_path(cwd: &str, path: &str) -> Result<PathBuf, String> {
    let cwd_normalized = normalize_path(Path::new(cwd));
    if cwd.is_empty() || cwd_normalized == Path::new("/") {
        return Err("Cannot resolve path: working directory is not set or is root".to_string());
    }

    let p = Path::new(path);
    let resolved = if p.is_absolute() {
        normalize_path(p)
    } else {
        normalize_path(&Path::new(cwd).join(p))
    };

    if !resolved.starts_with(&cwd_normalized) {
        return Err(format!("Path '{}' resolves outside working directory", path));
    }

    Ok(resolved)
}

/// Truncate output to a character limit.
pub fn truncate_output(output: &mut String, max_chars: usize) {
    let char_count = output.chars().count();
    if char_count > max_chars {
        let truncate_at = output
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(output.len());
        output.truncate(truncate_at);
        output.push_str(&format!(
            "\n... (output truncated, showed {max_chars} of {char_count} chars)"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_parent() {
        let p = normalize_path(Path::new("/a/b/../c"));
        assert_eq!(p, PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_skips_curdir() {
        let p = normalize_path(Path::new("/a/./b"));
        assert_eq!(p, PathBuf::from("/a/b"));
    }

    #[test]
    fn resolve_rejects_escape() {
        let r = resolve_path("/home/user/project", "../../../etc/passwd");
        assert!(r.is_err());
    }

    #[test]
    fn resolve_accepts_subpath() {
        let r = resolve_path("/home/user/project", "src/main.rs");
        assert_eq!(r.unwrap(), PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn resolve_rejects_root_cwd() {
        let r = resolve_path("/", "foo");
        assert!(r.is_err());
    }

    #[test]
    fn truncate_shortens_long_output() {
        let mut s = "abcdefghij".to_string();
        truncate_output(&mut s, 5);
        assert!(s.starts_with("abcde"));
        assert!(s.contains("truncated"));
    }
}
