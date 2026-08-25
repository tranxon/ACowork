//! Linux clipboard reader.
//!
//! Linux has no single canonical clipboard API — file managers on Wayland
//! use the `text/uri-list` MIME on the data-control protocol, while X11
//! file managers typically use `text/uri-list` on the `CLIPBOARD`
//! selection. Reading either portably from Rust requires either:
//!
//!   - `arboard` (which delegates to `wl-clipboard-rs` on Wayland and
//!     `x11-clipboard` on X11), or
//!   - Spawning `wl-paste` / `xclip` as subprocesses.
//!
//! Neither is wired up here to avoid adding a dependency for what is
//! expected to be a small user share. The frontend treats `None` as
//! "nothing on the clipboard that we can act on" and falls through to
//! the file-name insertion path (ClipboardEvent.files) or the default
//! paste behaviour, so the user still sees a response.
//!
//! TODO(linux-clipboard): wire in `arboard` (text/uri-list + filename
//! list) once a Linux user reports the issue.

pub fn read_clipboard_file_paths() -> Option<Vec<String>> {
    None
}