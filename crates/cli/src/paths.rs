//! Download-path sandbox policy.
//!
//! Default: `out_dir` must resolve (after `Path::canonicalize()` when it exists, or
//! symbolic resolution when it does not) under either `dirs::home_dir()` or the current
//! working directory. `--allow-path` disables the sandbox.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

pub fn validate_out_dir(out_dir: &Path, allow_path: bool) -> Result<()> {
    if allow_path {
        return Ok(());
    }
    let resolved = resolve(out_dir)?;
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let home_res = resolve(&home).unwrap_or(home);
    let cwd_res = resolve(&cwd).unwrap_or(cwd);
    if resolved.starts_with(&home_res) || resolved.starts_with(&cwd_res) {
        Ok(())
    } else {
        Err(anyhow!(
            "out_dir {} is outside the allowed path sandbox ($HOME or CWD). Pass --allow-path to override.",
            resolved.display()
        ))
    }
}

/// Resolve a path without requiring it to exist. When the path exists we canonicalize it;
/// otherwise we resolve the parent and rejoin.
fn resolve(p: &Path) -> Result<PathBuf> {
    if p.exists() {
        return Ok(p.canonicalize()?);
    }
    let Some(parent) = p.parent() else {
        return Ok(p.to_path_buf());
    };
    if parent.as_os_str().is_empty() {
        return Ok(p.to_path_buf());
    }
    let parent_canon = if parent.exists() {
        parent.canonicalize()?
    } else {
        // walk upward to find existing ancestor
        let mut walk = parent.to_path_buf();
        while !walk.exists() {
            if !walk.pop() {
                break;
            }
        }
        if walk.as_os_str().is_empty() {
            parent.to_path_buf()
        } else {
            walk.canonicalize()?
        }
    };
    let tail = match p.strip_prefix(parent) {
        Ok(t) => t.to_path_buf(),
        Err(_) => PathBuf::from(p.file_name().unwrap_or_default()),
    };
    Ok(parent_canon.join(tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allow_path_bypasses() {
        validate_out_dir(Path::new("/etc/foo"), true).unwrap();
    }

    #[test]
    fn test_under_home_accepted() {
        let home = dirs::home_dir().unwrap();
        let under = home.join("rutracker-test-dir");
        validate_out_dir(&under, false).unwrap();
    }

    #[test]
    fn test_under_cwd_accepted() {
        let cwd = std::env::current_dir().unwrap();
        let under = cwd.join("some-test-subdir");
        validate_out_dir(&under, false).unwrap();
    }

    #[test]
    fn test_etc_rejected() {
        let err = validate_out_dir(Path::new("/etc/rutracker"), false).unwrap_err();
        assert!(err.to_string().contains("sandbox"));
    }

    /// Architect-finding M2 regression: a symlink inside $HOME that targets outside
    /// the sandbox must not allow an escape. The current canonicalize-the-parent logic
    /// should resolve the symlink to its real target, then reject on `starts_with`.
    #[cfg(unix)]
    #[test]
    fn test_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;
        let home = dirs::home_dir().unwrap();
        let link_name = format!("rutracker-symlink-escape-test-{}", std::process::id());
        let link_path = home.join(&link_name);
        // Clean up any prior run
        let _ = std::fs::remove_file(&link_path);
        let target = PathBuf::from("/etc");
        symlink(&target, &link_path).unwrap();

        let sneaky = link_path.join("would-be-file");
        let result = validate_out_dir(&sneaky, false);

        // Clean up before asserting so a failure doesn't leak the symlink.
        let _ = std::fs::remove_file(&link_path);

        let err = result.expect_err("symlink escape to /etc must be rejected");
        assert!(
            err.to_string().contains("sandbox"),
            "error must mention sandbox; got: {err}"
        );
    }
}
