//! Finding programs: our own binary, and binaries on `PATH`.
//!
//! Two questions come up all over the place, and both are easy to get subtly
//! wrong, so they live here together:
//!
//! * *which binary am I?* — needed to re-execute ourselves as `__serve`,
//!   `__dial` and `__spawn`.
//! * *is this command installed?* — the net helpers (`pasta`, `slirp4netns`) and
//!   every app the user configures.

use std::path::PathBuf;

/// The running binary. Falls back to `/proc/self/exe`, which is what the
/// re-exec does internally anyway.
pub fn self_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"))
}

/// Looks `name` up on `PATH`, the way a shell would.
///
/// An absolute or relative path is taken as-is when it exists, so a config can
/// point at `./scripts/app.sh`.
pub fn in_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Whether `name` is installed.
pub fn exists(name: &str) -> bool {
    in_path(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_installed_programs() {
        assert!(exists("sh"));
        assert!(in_path("/bin/sh").is_some());
        assert!(!exists("definitely-not-a-real-binary"));
        assert!(in_path("/definitely/not/here").is_none());
    }

    #[test]
    fn our_own_binary_exists() {
        let path = self_path();
        assert!(path.exists(), "{}", path.display());
    }
}
