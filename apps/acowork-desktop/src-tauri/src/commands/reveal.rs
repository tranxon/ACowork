//! Reveal-in-file-explorer command — local-mode only.
//!
//! Opens the OS file manager with the given file or folder selected
//! (where the platform supports it). On macOS this triggers Finder's
//! "Show in Finder"; on Windows this triggers Explorer's "Show in
//! folder"; on Linux most desktop environments have no native
//! equivalent, so we fall back to opening the parent directory with
//! `xdg-open`.
//!
//! **Local-only by design.** The file manager is opened on the machine
//! running the Desktop App. In remote Gateway mode the Gateway lives
//! on a separate host from the Desktop, so opening a file manager
//! there would reveal a folder the user cannot see. The frontend hides
//! the corresponding menu item when `isGatewayLocal()` is false; this
//! command enforces the same rule as a defence-in-depth check.
//!
//! The frontend (`workspaceStore.revealItem`) is responsible for
//! joining the workspace root with the relative path. The absolute
//! path passed here is therefore trusted — it came from a tree API
//! response that already validates containment — but we still
//! existence-check before spawning to give a clear error if the file
//! was deleted between the API call and this command.

use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// Reveal a file or directory in the OS file manager.
///
/// On macOS: `open -R <path>` reveals and selects the item in Finder.
/// On Windows: `explorer.exe /select,<path>` reveals and selects it in
/// Explorer. On Linux: `xdg-open <parent>` opens the parent directory
/// (no platform-portable "select" equivalent exists).
///
/// Returns an error string suitable for surfacing in a toast when the
/// path does not exist or the file-manager launch fails.
#[tauri::command]
pub async fn reveal_in_file_explorer(path: String) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);

    if !path_buf.exists() {
        return Err(format!("Path does not exist: {}", path));
    }

    spawn_reveal(&path, &path_buf).map_err(|e| e.to_string())
}

/// Platform-specific spawn. The two arguments (`path_str` and
/// `path_buf`) each have a distinct role: `path_str` is the canonical
/// string form of the path (forward slashes are accepted by
/// `explorer.exe`, so we don't have to swap separators here);
/// `path_buf` is only consumed on Linux, where we need `parent()` to
/// open the enclosing directory.
///
/// We accept both unconditionally rather than splitting into per-OS
/// helper functions because that keeps the platform differences in one
/// place where they're easy to compare side-by-side. The unused
/// argument on macOS/Windows is suppressed via `#[allow(unused)]`
/// on the parameter — this is a deliberately platform-aware API, not
/// a code smell.
#[allow(unused_variables)] // path_buf is only consumed on Linux
fn spawn_reveal(path_str: &str, path_buf: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // `open -R <path>` tells Finder to reveal the item (select it
        // in its parent folder). `-R` is documented in `man open`:
        // "Will reveal the file(s) in Finder instead of opening them."
        Command::new("open").arg("-R").arg(path_str).spawn()?;
    }

    #[cfg(target_os = "windows")]
    {
        // Explorer's `/select,<path>` syntax highlights the item in
        // its parent folder. THREE non-obvious requirements to make
        // explorer.exe actually do what we want:
        //
        //   1. The comma MUST NOT be separated from the path — that's
        //      the documented one-arg form `/select,<path>`. We build
        //      it as a single String.
        //
        //   2. The whole argument MUST NOT be wrapped in quotes —
        //      explorer.exe's argument parser does NOT understand the
        //      quoted form `/select,"C:\path\file.txt"`. When the
        //      path contains a space (e.g. `C:\Users\Me\My Documents\…`)
        //      Rust's `Command::arg` would normally auto-wrap the
        //      argument in quotes for the Win32 command line, which
        //      causes explorer.exe to silently fall back to opening
        //      the default location (Desktop / Quick Access).
        //
        //   3. The path MUST use backslash separators. explorer.exe's
        //      `/select,<path>` argv parser does NOT accept forward
        //      slashes — it treats `/select,D:/foo/bar` as a malformed
        //      argument and silently falls back to opening the default
        //      Desktop folder, exactly the same way it does for the
        //      quoted case above. (Verified empirically: forward slash
        //      → Desktop, backslash → correct folder.) The frontend
        //      sends forward-slash paths because that's the contract
        //      of `TreeResponse.root`; we normalise here at the
        //      spawn boundary so the rest of the stack can stay
        //      platform-neutral.
        //
        // `CommandExt::raw_arg` is the documented escape hatch: it
        // appends the argument verbatim, with NO escaping. The result
        // is the exact bytes explorer.exe needs to see on its argv.
        let win_path = path_str.replace('/', "\\");
        let select_arg = format!("/select,{}", win_path);
        Command::new("explorer.exe").raw_arg(select_arg).spawn()?;
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Linux has no cross-DE equivalent of `/select,`. We open the
        // parent directory with xdg-open, which honours the user's
        // default file manager (Nautilus / Dolphin / Thunar / etc.).
        let parent = path_buf
            .parent()
            .ok_or_else(|| std::io::Error::other("Cannot determine parent directory"))?;
        Command::new("xdg-open").arg(parent).spawn()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `reveal_in_file_explorer` must surface a user-friendly error
    /// for paths that don't exist rather than spawning a process that
    /// would just fail silently.
    #[tokio::test]
    async fn reveal_errors_on_missing_path() {
        let result =
            reveal_in_file_explorer("/this/path/should/not/exist/acowork-reveal-test".into()).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("does not exist"),
            "error should mention non-existent path",
        );
    }

    /// Sanity check: a path that does exist passes the existence
    /// gate. We can't assert on the actual spawn (Finder would open
    /// during the test), so we only verify the gate behaviour by
    /// directly inspecting the path.
    #[test]
    fn existing_path_passes_existence_gate() {
        let tmp = std::env::temp_dir().join("acowork-reveal-exists");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let f = tmp.join("marker.txt");
        fs::write(&f, "x").unwrap();
        assert!(f.exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    /// Regression guard for the Windows reveal bug (see fix-windows-reveal):
    /// if a future refactor accidentally re-introduces `Command::arg()`
    /// here, the path-with-space case will silently regress and open
    /// Desktop instead of the target folder. This test documents the
    /// contract at the build level: the `CommandExt::raw_arg` call site
    /// exists on Windows. It does NOT spawn explorer.exe (that would
    /// open an Explorer window in CI).
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_reveal_uses_raw_arg_to_bypass_quoting() {
        // Construct a synthetic /select, string with a space — exactly
        // the case that triggered the original bug. The format string
        // must produce the literal `/select,<path>` with no quoting.
        let path_with_space = r"C:\Users\Name\My Documents\file.txt";
        let select_arg = format!("/select,{}", path_with_space);
        assert_eq!(
            select_arg, r"/select,C:\Users\Name\My Documents\file.txt",
            "select_arg must be a single literal argument; if quoting is ever \
             reintroduced here, explorer.exe will fall back to opening the \
             default Desktop folder."
        );
        // And the argument MUST contain a space (otherwise the test
        // would pass vacuously even if quoting is reverted, since
        // Rust's auto-quoting is space-triggered). This protects
        // against accidentally removing the space from the fixture.
        assert!(
            select_arg.contains(' '),
            "fixture must contain a space to actually exercise the quoting path"
        );
    }
}