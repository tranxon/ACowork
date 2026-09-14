//! RuntimeGitQueryService — implements [`GitQueryService`] (ADR-078).
//!
//! Executes read-only git commands via the system git CLI inside the
//! Runtime process (the authoritative workspace owner, ADR-009 v2). The
//! Gateway only reverse-proxies these endpoints — it never touches the
//! filesystem (ADR-009 red line).
//!
//! Command conventions (ADR-078 decision 2):
//! - `std::process::Command` + `spawn_blocking` (project convention,
//!   see `tools/builtin/shell.rs` — avoids `tokio::process::Command`'s
//!   async named-pipe I/O issues on Windows);
//! - no shell string interpolation; args via `Command::arg`, `--`
//!   separator guards dash-leading paths;
//! - `cwd = repo_root`; uniform timeout (10s);
//! - `GIT_OPTIONAL_LOCKS=0` (read-only guarantee, ADR-078 §1.3
//!   invariant 1) + `LC_ALL=C` (stable English stderr for error
//!   matching) on every command;
//! - stdout capped (status 1 MiB / 5000 entries; diff pre-checks blob
//!   sizes before reading, > 2 MiB degrades to `kind=binary`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;

use crate::usecases::git_query::{
    GitChangeDto, GitCommitDto, GitDiffKind, GitDiffParams, GitDiffResponse, GitError,
    GitIndexStatus, GitLogParams, GitLogResponse, GitQueryService, GitStatusParams,
    GitStatusResponse, GitWorktreeStatus,
};
use crate::usecases::workspace_mutation_impl::{resolve_within_static, resolve_workspace_root};
use crate::usecases::WorkspaceError;

/// Max levels to walk up from the workspace root looking for `.git`
/// (ADR-078 decision 3 — covers the "workspace is a repo subdirectory"
/// topology without crossing into unrelated outer repos).
const MAX_REPO_UP: usize = 6;
/// Default git command timeout (ADR-078 decision 2).
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Status porcelain output byte cap — beyond this we truncate at the
/// last complete NUL-terminated entry and flag `truncated`.
const STATUS_BYTE_CAP: usize = 1 << 20; // 1 MiB
/// Status porcelain entry cap.
const STATUS_ENTRY_CAP: usize = 5_000;
/// Max diff side size — larger blobs/files degrade to `kind=binary`
/// (ADR-078 decision 4, revision 2026-09-14).
const DIFF_SIZE_CAP: u64 = 2 * 1024 * 1024; // 2 MiB
/// Default / max `git log` limit (ADR-078 decision 4).
const LOG_DEFAULT_LIMIT: u32 = 50;
const LOG_MAX_LIMIT: u32 = 200;

/// Captured stdout/stderr of a git invocation.
struct GitOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Read-outcome of a git blob (`git show HEAD:<path>` / `git show :<path>`).
enum BlobRead {
    /// The rev:path does not exist (e.g. untracked file in HEAD).
    Missing,
    /// The blob exceeds [`DIFF_SIZE_CAP`] — caller returns `kind=binary`.
    TooLarge,
    /// Raw blob bytes.
    Ok(Vec<u8>),
}

/// Concrete [`GitQueryService`] implementation (ADR-040).
///
/// Holds only the agent `work_dir` (resolved once at boot) — like the
/// workspace query service, it resolves `workspace_id` to an absolute
/// path via `agent_workspaces.json`; HTTP handlers never see the
/// filesystem layout directly.
pub struct RuntimeGitQueryService {
    work_dir: PathBuf,
    _agent_id: String,
}

impl RuntimeGitQueryService {
    pub fn new(work_dir: PathBuf, agent_id: String) -> Self {
        Self {
            work_dir,
            _agent_id: agent_id,
        }
    }
}

// ── Command plumbing ───────────────────────────────────────────────────────

/// Base git command with `cwd = repo_root`.
fn git_cmd(repo_root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root);
    cmd
}

/// Run a git command under the ADR-078 conventions:
/// `GIT_OPTIONAL_LOCKS=0` + `LC_ALL=C`, 10s timeout via
/// `spawn_blocking` + `tokio::time::timeout`.
///
/// NOTE: on timeout the spawned process is not killed (v1 accepts a
/// rare stray `git` process — these commands are sub-second in
/// practice, and `Command::output()` drains both pipes so there is no
/// deadlock risk).
async fn run_git(mut cmd: Command) -> Result<GitOutput, GitError> {
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("LC_ALL", "C");
    let handle = tokio::task::spawn_blocking(move || cmd.output());
    match tokio::time::timeout(GIT_TIMEOUT, handle).await {
        Err(_) => Err(GitError::Timeout(GIT_TIMEOUT.as_secs())),
        Ok(Err(join_err)) => Err(GitError::GitFailed(format!(
            "spawn_blocking join error: {join_err}"
        ))),
        Ok(Ok(Err(io_err))) => {
            if io_err.kind() == std::io::ErrorKind::NotFound {
                Err(GitError::GitUnavailable(io_err.to_string()))
            } else {
                Err(GitError::Io(io_err))
            }
        }
        Ok(Ok(Ok(out))) => Ok(GitOutput {
            status: out.status,
            stdout: out.stdout,
            stderr: out.stderr,
        }),
    }
}

/// Repo discovery: walk up from the workspace root (max 6 levels) until
/// a `.git` entry (directory, or file for worktrees / submodules /
/// `gitdir:` pointers) is found. The parent of `.git` is the repo root.
fn discover_repo_root(workspace_root: &Path) -> Option<PathBuf> {
    let mut dir: Option<&Path> = Some(workspace_root);
    for _ in 0..=MAX_REPO_UP {
        let d = dir?;
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Map a WorkspaceError to a GitError (HTTP layer maps both to
/// deterministic status codes).
fn workspace_to_git_error(e: WorkspaceError) -> GitError {
    match e {
        WorkspaceError::WorkspaceNotFound(id) => GitError::WorkspaceNotFound(id),
        WorkspaceError::InvalidPath(p) => GitError::InvalidPath(p),
        other => GitError::GitFailed(other.to_string()),
    }
}

/// `git status` stderr — "not a git repository" (git emits it even
/// under `LC_ALL=C`; versions vary slightly in wording).
fn looks_like_not_a_repo(stderr: &str) -> bool {
    stderr.contains("not a git repository")
}

// ── Porcelain `-z` parsing (ADR-078 decision 2/3) ──────────────────────────

/// Parse `git status --porcelain=v1 -z --branch` output.
///
/// Returns `(changes, branch, truncated)`. porcelain paths are
/// repo-root-relative; every change is filtered to the workspace root
/// (paths outside the workspace are dropped — ADR-078 decision 3, the
/// "workspace is a repo subdirectory" topology). Response paths are
/// workspace-root-relative with forward slashes.
fn parse_status_porcelain(
    raw: &[u8],
    workspace_root: &Path,
    repo_root: &Path,
) -> (Vec<GitChangeDto>, String, bool) {
    let mut changes = Vec::new();
    let mut branch = "HEAD".to_string();
    let mut truncated = false;

    let cap_end = truncate_at_nul(raw, STATUS_BYTE_CAP);
    if cap_end < raw.len() {
        truncated = true;
    }
    let slice = &raw[..cap_end];
    let mut entries = slice.split(|&b| b == 0);

    // First entry (with `--branch`) is the `## <branch>` header.
    if let Some(first) = entries.next().filter(|f| f.starts_with(b"## ")) {
        branch = parse_branch_line(&first[3..]);
    }

    while let Some(entry) = entries.next() {
        // `XY <path>` — X = index status, Y = worktree status.
        // Valid entries always have a space at index 2 (X and Y are both
        // single status chars, possibly spaces, e.g. ` M a.rs`).
        if entry.len() < 4 || entry[2] != b' ' {
            continue;
        }
        let x = entry[0] as char;
        let y = entry[1] as char;
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        // Rename entries are two NUL-separated paths: `R  <new>\0<old>`.
        let old = if x == 'R' || y == 'R' {
            entries
                .next()
                .map(|p| String::from_utf8_lossy(p).into_owned())
        } else {
            None
        };

        if let Some(change) =
            make_change(x, y, &path, old.as_deref(), workspace_root, repo_root)
        {
            changes.push(change);
        }
        if changes.len() >= STATUS_ENTRY_CAP {
            truncated = true;
            break;
        }
    }

    (changes, branch, truncated)
}

/// Find the largest byte prefix of `raw` (≤ `cap`) that ends at a NUL
/// boundary, so no entry is split mid-path.
fn truncate_at_nul(raw: &[u8], cap: usize) -> usize {
    if raw.len() <= cap {
        return raw.len();
    }
    match raw[..cap].iter().rposition(|&b| b == 0) {
        Some(pos) => pos + 1,
        None => 0,
    }
}

/// Parse the `## <branch>` header line (ADR-078 decision 5 — the
/// header already carries ahead/behind, but v1 shows only the name).
fn parse_branch_line(line: &[u8]) -> String {
    let s = String::from_utf8_lossy(line);
    let s = s.trim();
    // Detached HEAD.
    if s.starts_with("HEAD (no branch)") {
        return "HEAD".to_string();
    }
    // Empty repository (git <2.35: "Initial commit on main").
    for prefix in ["No commits yet on ", "Initial commit on "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    // "main" | "main...origin/main [ahead 1, behind 2]".
    s.split("...").next().unwrap_or("HEAD").trim().to_string()
}

/// Index status from porcelain column X.
fn index_status(x: char) -> GitIndexStatus {
    match x {
        'M' => GitIndexStatus::Modified,
        'A' => GitIndexStatus::Added,
        'D' => GitIndexStatus::Deleted,
        'R' | 'C' => GitIndexStatus::Renamed,
        _ => GitIndexStatus::Unmodified,
    }
}

/// Worktree status from porcelain column Y.
fn worktree_status(y: char) -> GitWorktreeStatus {
    match y {
        'M' | 'T' => GitWorktreeStatus::Modified,
        'D' => GitWorktreeStatus::Deleted,
        '?' => GitWorktreeStatus::Untracked,
        _ => GitWorktreeStatus::Unmodified,
    }
}

/// Convert a repo-root-relative porcelain path to a workspace-root-
/// relative path, or `None` when it lies outside the workspace root
/// (dropped by the ADR-078 decision 3 range filter).
fn repo_to_workspace_rel(
    repo_root: &Path,
    workspace_root: &Path,
    repo_rel: &str,
) -> Option<String> {
    let abs = repo_root.join(repo_rel);
    let rel = abs.strip_prefix(workspace_root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Build a [`GitChangeDto`] from porcelain XY + paths, applying the
/// workspace-root range filter.
fn make_change(
    x: char,
    y: char,
    path_repo_rel: &str,
    old_repo_rel: Option<&str>,
    workspace_root: &Path,
    repo_root: &Path,
) -> Option<GitChangeDto> {
    let ws_path = repo_to_workspace_rel(repo_root, workspace_root, path_repo_rel)?;
    let ws_old = old_repo_rel.and_then(|o| repo_to_workspace_rel(repo_root, workspace_root, o));
    let index = index_status(x);
    let worktree = worktree_status(y);
    Some(GitChangeDto {
        path: ws_path,
        old_path: ws_old,
        index,
        worktree,
        staged: index != GitIndexStatus::Unmodified,
    })
}

/// Status sort: staged first, then M / U / D / A / R groups (ADR-078
/// decision 4 — v1 fixed order, no user sorting).
fn status_sort_key(c: &GitChangeDto) -> (u8, u8, &str) {
    let staged_rank = if c.staged { 0 } else { 1 };
    let type_rank = if c.worktree == GitWorktreeStatus::Untracked {
        1 // U
    } else if c.index == GitIndexStatus::Modified
        || c.worktree == GitWorktreeStatus::Modified
    {
        0 // M
    } else if c.index == GitIndexStatus::Deleted
        || c.worktree == GitWorktreeStatus::Deleted
    {
        2 // D
    } else if c.index == GitIndexStatus::Added {
        3 // A
    } else if c.index == GitIndexStatus::Renamed {
        4 // R
    } else {
        5
    };
    (staged_rank, type_rank, &c.path)
}

// ── Blob read helpers ──────────────────────────────────────────────────────

/// `git cat-file -s <rev>:<path>` — blob size in bytes, `None` when the
/// rev:path does not exist. `rev` may be `"HEAD"` or `""` (index).
async fn cat_file_size(
    repo_root: &Path,
    rev: &str,
    path: &str,
) -> Result<Option<u64>, GitError> {
    let mut cmd = git_cmd(repo_root);
    cmd.arg("cat-file").arg("-s").arg(format!("{rev}:{path}"));
    let out = run_git(cmd).await?;
    if !out.status.success() {
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(s.trim().parse::<u64>().ok())
}

/// `git show <rev>:<path>` — raw blob bytes for a diff side, with
/// size pre-check (`cat-file -s` before reading, so a huge blob never
/// enters memory — ADR-078 decision 4 > 2 MiB → `TooLarge`). `rev` is
/// `"HEAD"` or `""` (index).
async fn read_blob(
    repo_root: &Path,
    rev: &str,
    path: &str,
) -> Result<BlobRead, GitError> {
    let Some(size) = cat_file_size(repo_root, rev, path).await? else {
        return Ok(BlobRead::Missing);
    };
    if size > DIFF_SIZE_CAP {
        return Ok(BlobRead::TooLarge);
    }
    let mut cmd = git_cmd(repo_root);
    cmd.arg("show").arg(format!("{rev}:{path}"));
    let out = run_git(cmd).await?;
    if !out.status.success() {
        return Ok(BlobRead::Missing);
    }
    Ok(BlobRead::Ok(out.stdout))
}

// ── Trait impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl GitQueryService for RuntimeGitQueryService {
    async fn status(&self, params: &GitStatusParams) -> Result<GitStatusResponse, GitError> {
        let workspace_root = resolve_workspace_root(&self.work_dir, params.workspace_id.as_deref())
            .map_err(workspace_to_git_error)?;
        let canonical_ws = std::fs::canonicalize(&workspace_root).unwrap_or(workspace_root);

        let Some(repo_root) = discover_repo_root(&canonical_ws) else {
            // Explicit non-repo state — not an error (ADR-078 invariant 5).
            return Ok(GitStatusResponse {
                is_repo: false,
                branch: None,
                error: Some("not_a_repo".to_string()),
                truncated: false,
                changes: vec![],
            });
        };

        let mut cmd = git_cmd(&repo_root);
        cmd.args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--branch",
        ]);
        let out = run_git(cmd).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if looks_like_not_a_repo(&stderr) {
                return Ok(GitStatusResponse {
                    is_repo: false,
                    branch: None,
                    error: Some("not_a_repo".to_string()),
                    truncated: false,
                    changes: vec![],
                });
            }
            return Err(GitError::GitFailed(format!(
                "git status: {}",
                stderr.trim()
            )));
        }

        let (mut changes, branch, truncated) =
            parse_status_porcelain(&out.stdout, &canonical_ws, &repo_root);
        changes.sort_by(|a, b| status_sort_key(a).cmp(&status_sort_key(b)));

        Ok(GitStatusResponse {
            is_repo: true,
            branch: Some(branch),
            error: None,
            truncated,
            changes,
        })
    }

    async fn diff(&self, params: &GitDiffParams) -> Result<GitDiffResponse, GitError> {
        if params.cached > 1 {
            return Err(GitError::BadRequest("cached must be 0 or 1".into()));
        }

        let (canonical_ws, _, _) = resolve_within_static(
            &self.work_dir,
            params.workspace_id.as_deref(),
            &params.path,
        )
        .map_err(workspace_to_git_error)?;

        let repo_root = discover_repo_root(&canonical_ws)
            .ok_or_else(|| GitError::NotARepo(canonical_ws.display().to_string()))?;

        // ADR-078 decision 3: workspace-relative → absolute → validate →
        // strip repo_root prefix → repo-root-relative.
        let abs_path = canonical_ws.join(&params.path);
        let repo_rel = abs_path
            .strip_prefix(&repo_root)
            .map_err(|_| GitError::InvalidPath("path is outside the repository root".into()))?
            .to_string_lossy()
            .replace('\\', "/");

        // Current XY state of this exact path (cheap: single-path status).
        let xy = self.path_status(&repo_root, &repo_rel).await?;

        let Some((x, y, rename_old)) = xy else {
            // Tracked & clean — the two sides are identical (NoChange).
            return self.diff_clean(&repo_root, &abs_path, &repo_rel, params.cached).await;
        };

        // Renames: HEAD holds the OLD path; worktree/index hold the NEW.
        let head_repo_rel = if x == 'R' || y == 'R' {
            rename_old.as_deref().unwrap_or(&repo_rel)
        } else {
            &repo_rel
        };

        let is_untracked = x == '?' && y == '?';
        let is_worktree_deleted = y == 'D';

        // ── original (HEAD side) ─────────────────────────────────────────
        let original = if is_untracked {
            Vec::new()
        } else {
            match read_blob(&repo_root, "HEAD", head_repo_rel).await? {
                BlobRead::Missing => Vec::new(), // staged-added new file
                BlobRead::TooLarge => {
                    return Ok(binary_diff_response());
                }
                BlobRead::Ok(bytes) => bytes,
            }
        };

        // ── modified side (worktree, or index when cached=1) ────────────
        let modified = if params.cached == 1 {
            if is_untracked {
                Vec::new()
            } else {
                match read_blob(&repo_root, "", &repo_rel).await? {
                    BlobRead::Missing => Vec::new(),
                    BlobRead::TooLarge => {
                        return Ok(binary_diff_response());
                    }
                    BlobRead::Ok(bytes) => bytes,
                }
            }
        } else if is_worktree_deleted {
            Vec::new()
        } else {
            match std::fs::metadata(&abs_path) {
                Ok(md) if md.len() > DIFF_SIZE_CAP => return Ok(binary_diff_response()),
                Ok(_) => std::fs::read(&abs_path)?,
                Err(_) => Vec::new(),
            }
        };

        Ok(assemble_diff(
            original,
            modified,
            is_untracked,
            is_worktree_deleted,
        ))
    }

    async fn log(&self, params: &GitLogParams) -> Result<GitLogResponse, GitError> {
        let workspace_root = resolve_workspace_root(&self.work_dir, params.workspace_id.as_deref())
            .map_err(workspace_to_git_error)?;
        let canonical_ws = std::fs::canonicalize(&workspace_root).unwrap_or(workspace_root);

        let repo_root = discover_repo_root(&canonical_ws)
            .ok_or_else(|| GitError::NotARepo(canonical_ws.display().to_string()))?;

        let limit = params
            .limit
            .unwrap_or(LOG_DEFAULT_LIMIT)
            .min(LOG_MAX_LIMIT);

        let mut cmd = git_cmd(&repo_root);
        cmd.args([
            "log",
            "--no-ext-diff",
            "-n",
            &limit.to_string(),
            "--pretty=format:%H%x1f%h%x1f%an%x1f%aI%x1f%s",
        ]);
        if let Some(path) = params.path.as_deref().filter(|p| !p.is_empty()) {
            let (canonical_ws, _, _) = resolve_within_static(
                &self.work_dir,
                params.workspace_id.as_deref(),
                path,
            )
            .map_err(workspace_to_git_error)?;
            let repo_rel = canonical_ws
                .join(path)
                .strip_prefix(&repo_root)
                .map_err(|_| GitError::InvalidPath("path is outside the repository root".into()))?
                .to_string_lossy()
                .replace('\\', "/");
            cmd.arg("--").arg(repo_rel);
        }

        let out = run_git(cmd).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if looks_like_not_a_repo(&stderr) {
                return Err(GitError::NotARepo(stderr.trim().to_string()));
            }
            return Err(GitError::GitFailed(format!(
                "git log: {}",
                stderr.trim()
            )));
        }

        let text = String::from_utf8_lossy(&out.stdout);
        let commits = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let mut parts = line.splitn(5, '\x1f');
                let hash = parts.next().unwrap_or("").to_string();
                let short_hash = parts.next().unwrap_or("").to_string();
                let author = parts.next().unwrap_or("").to_string();
                let date = parts.next().unwrap_or("").to_string();
                let subject = parts.next().unwrap_or("").to_string();
                GitCommitDto {
                    hash,
                    short_hash,
                    author,
                    date,
                    subject,
                }
            })
            .collect();

        Ok(GitLogResponse { commits })
    }
}

impl RuntimeGitQueryService {
    /// Single-path `git status --porcelain=v1 -z` → `(x, y, old)` for
    /// the requested repo-relative path, or `None` when the path is not
    /// in the status output (tracked & clean).
    ///
    /// Scans the full status output instead of `-- <path>`: git's pathspec
    /// matches the *old* path of a rename pair (and returns nothing for
    /// the new path), so a single-path query would miss `R` entries.
    async fn path_status(
        &self,
        repo_root: &Path,
        repo_rel: &str,
    ) -> Result<Option<(char, char, Option<String>)>, GitError> {
        let mut cmd = git_cmd(repo_root);
        cmd.args(["status", "--porcelain=v1", "-z", "--untracked-files=all"]);
        let out = run_git(cmd).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if looks_like_not_a_repo(&stderr) {
                return Err(GitError::NotARepo(stderr.trim().to_string()));
            }
            return Err(GitError::GitFailed(format!(
                "git status: {}",
                stderr.trim()
            )));
        }

        let cap_end = truncate_at_nul(&out.stdout, STATUS_BYTE_CAP);
        let mut entries = out.stdout[..cap_end].split(|&b| b == 0);
        while let Some(entry) = entries.next() {
            // `XY <path>` (no `##` header — `--branch` is omitted here).
            if entry.len() < 4 || entry[2] != b' ' {
                continue;
            }
            let x = entry[0] as char;
            let y = entry[1] as char;
            let path = String::from_utf8_lossy(&entry[3..]).into_owned();
            // Rename: current entry is the new path, next is the old.
            let old = if x == 'R' || y == 'R' {
                entries
                    .next()
                    .map(|p| String::from_utf8_lossy(p).into_owned())
            } else {
                None
            };
            if path == repo_rel || old.as_deref() == Some(repo_rel) {
                return Ok(Some((x, y, old)));
            }
        }
        Ok(None)
    }

    /// Diff for a tracked-and-clean path (absent from status output).
    async fn diff_clean(
        &self,
        repo_root: &Path,
        abs_path: &Path,
        repo_rel: &str,
        cached: u8,
    ) -> Result<GitDiffResponse, GitError> {
        let original = match read_blob(repo_root, "HEAD", repo_rel).await? {
            BlobRead::Missing => Vec::new(),
            BlobRead::TooLarge => return Ok(binary_diff_response()),
            BlobRead::Ok(bytes) => bytes,
        };
        let modified = if cached == 1 {
            match read_blob(repo_root, "", repo_rel).await? {
                BlobRead::Missing => Vec::new(),
                BlobRead::TooLarge => return Ok(binary_diff_response()),
                BlobRead::Ok(bytes) => bytes,
            }
        } else {
            match std::fs::metadata(abs_path) {
                Ok(md) if md.len() > DIFF_SIZE_CAP => return Ok(binary_diff_response()),
                Ok(_) => std::fs::read(abs_path)?,
                Err(_) => Vec::new(),
            }
        };
        Ok(assemble_diff(original, modified, false, false))
    }
}

/// A binary-degraded diff (no content returned — ADR-078 decision 4).
fn binary_diff_response() -> GitDiffResponse {
    GitDiffResponse {
        kind: GitDiffKind::Binary,
        original: String::new(),
        modified: String::new(),
    }
}

/// Final kind classification + UTF-8 conversion. Binary is detected via
/// a NUL byte in either side (sufficient for a text editor context).
fn assemble_diff(
    original: Vec<u8>,
    modified: Vec<u8>,
    is_untracked: bool,
    is_deleted: bool,
) -> GitDiffResponse {
    let kind = if is_untracked {
        GitDiffKind::Untracked
    } else if is_deleted {
        GitDiffKind::Deleted
    } else if original.contains(&0) || modified.contains(&0) {
        GitDiffKind::Binary
    } else if original == modified {
        GitDiffKind::NoChange
    } else {
        GitDiffKind::Modified
    };
    // ADR-078 decision 4: binary diffs degrade to a placeholder — no
    // content is returned (mirrors binary_diff_response for >2 MiB).
    let (original, modified) = if kind == GitDiffKind::Binary {
        (String::new(), String::new())
    } else {
        (
            String::from_utf8_lossy(&original).into_owned(),
            String::from_utf8_lossy(&modified).into_owned(),
        )
    };
    GitDiffResponse {
        kind,
        original,
        modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Skip-guard for integration tests that need a real git binary.
    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run git in `dir` with deterministic author identity + the same
    /// read-only lock convention as `run_git`.
    fn git_in<I, S>(dir: &Path, args: I) -> std::process::Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.local")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.local")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .expect("git spawn failed")
    }

    /// `git init -b main` + one committed file (`a.txt` = "hello\n").
    fn init_repo(dir: &Path) {
        let init = git_in(dir, ["init", "-q", "-b", "main"]);
        assert!(init.status.success(), "git init failed: {:?}", init);
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        assert!(git_in(dir, ["add", "."]).status.success());
        let c = git_in(dir, ["commit", "-q", "-m", "init"]);
        assert!(c.status.success(), "git commit failed: {:?}", c);
    }

    // ── parse_branch_line ──────────────────────────────────────────────

    #[test]
    fn branch_line_plain() {
        assert_eq!(parse_branch_line(b"main"), "main");
    }

    #[test]
    fn branch_line_with_tracking_info() {
        // `--branch` header already carries ahead/behind — v1 shows only
        // the name (ADR-078 decision 5).
        assert_eq!(parse_branch_line(b"main...origin/main [ahead 1, behind 2]"), "main");
    }

    #[test]
    fn branch_line_detached_head() {
        assert_eq!(parse_branch_line(b"HEAD (no branch)"), "HEAD");
    }

    #[test]
    fn branch_line_empty_repo() {
        assert_eq!(parse_branch_line(b"No commits yet on main"), "main");
        assert_eq!(parse_branch_line(b"Initial commit on dev"), "dev");
    }

    // ── parse_status_porcelain (pure, no git) ───────────────────────────

    #[test]
    fn porcelain_parses_worktree_modified_and_untracked() {
        let ws = Path::new("/repo");
        let repo = Path::new("/repo");
        let raw = b"## main\x00 M src/main.rs\x00?? new.txt\x00";
        let (changes, branch, truncated) = parse_status_porcelain(raw, ws, repo);
        assert_eq!(branch, "main");
        assert!(!truncated);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "src/main.rs");
        assert_eq!(changes[0].worktree, GitWorktreeStatus::Modified);
        assert!(!changes[0].staged);
        assert_eq!(changes[1].worktree, GitWorktreeStatus::Untracked);
        assert!(!changes[1].staged);
    }

    #[test]
    fn porcelain_parses_staged_and_index_worktree_combos() {
        let ws = Path::new("/repo");
        let repo = Path::new("/repo");
        let raw = b"## main\x00M  staged.rs\x00MM both.rs\x00 D deleted.rs\x00";
        let (changes, _, _) = parse_status_porcelain(raw, ws, repo);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].path, "staged.rs");
        assert_eq!(changes[0].index, GitIndexStatus::Modified);
        assert!(changes[0].staged);
        assert_eq!(changes[1].path, "both.rs");
        assert_eq!(changes[1].index, GitIndexStatus::Modified);
        assert_eq!(changes[1].worktree, GitWorktreeStatus::Modified);
        assert!(changes[1].staged);
        assert_eq!(changes[2].path, "deleted.rs");
        assert_eq!(changes[2].worktree, GitWorktreeStatus::Deleted);
        assert!(!changes[2].staged);
    }

    #[test]
    fn porcelain_rename_captures_old_path() {
        let ws = Path::new("/repo");
        let repo = Path::new("/repo");
        let raw = b"## main\x00R  new.txt\x00old.txt\x00";
        let (changes, _, _) = parse_status_porcelain(raw, ws, repo);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "new.txt");
        assert_eq!(changes[0].old_path.as_deref(), Some("old.txt"));
        assert_eq!(changes[0].index, GitIndexStatus::Renamed);
        assert!(changes[0].staged);
    }

    #[test]
    fn porcelain_utf8_paths_survive_z_mode() {
        let ws = Path::new("/repo");
        let repo = Path::new("/repo");
        let raw = "## main\x00?? 中文文件.txt\x00".as_bytes();
        let (changes, _, _) = parse_status_porcelain(raw, ws, repo);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "中文文件.txt");
    }

    #[test]
    fn porcelain_filters_outside_workspace_paths() {
        let ws = Path::new("/repo/sub/ws");
        let repo = Path::new("/repo");
        let raw = b"## main\x00 M sub/ws/inside.rs\x00 M outside.rs\x00?? sub/ws/u.txt\x00";
        let (changes, _, _) = parse_status_porcelain(raw, ws, repo);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "inside.rs");
        assert_eq!(changes[1].path, "u.txt");
    }

    #[test]
    fn truncate_at_nul_keeps_entries_whole() {
        let raw = b"## main\x00 M a.rs\x00 M b.rs\x00";
        // cap 落在第二条 entry 内部 → 截断到最后一个完整 NUL 边界
        // （保留 header + 完整的第一条，丢弃被切开的第二条）
        let end = truncate_at_nul(raw, 20);
        assert!(end <= 20);
        assert_eq!(end, 16); // `## main` + NUL + ` M a.rs` + NUL
        assert!(raw[..end].ends_with(b"\x00"));
        assert_eq!(&raw[..end], b"## main\x00 M a.rs\x00");
        // 截断后的切片解析完整（no split path）；truncated 标志由全局
        // STATUS_BYTE_CAP 决定，切片本身不会再次触发
        let (changes, _, _) =
            parse_status_porcelain(&raw[..end], Path::new("/repo"), Path::new("/repo"));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "a.rs");
    }

    // ── repo discovery ──────────────────────────────────────────────────

    #[test]
    fn discover_repo_root_walks_up_but_limited_to_6() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let mut deep = root.to_path_buf();
        for i in 0..8 {
            deep = deep.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();

        // workspace == repo root
        assert_eq!(discover_repo_root(root), Some(root.to_path_buf()));
        // 6 层深（d0..d5）→ 可发现
        let depth6 = root.join("d0/d1/d2/d3/d4/d5");
        assert_eq!(discover_repo_root(&depth6), Some(root.to_path_buf()));
        // 8 层深 → 超出 MAX_REPO_UP
        assert_eq!(discover_repo_root(&deep), None);
    }

    #[test]
    fn discover_repo_root_none_when_not_a_repo() {
        let tmp = tempdir().unwrap();
        assert_eq!(discover_repo_root(tmp.path()), None);
    }

    // ── path coordinate conversion ──────────────────────────────────────

    #[test]
    fn repo_to_workspace_rel_strips_repo_prefix_and_rejects_outside() {
        let repo = Path::new("/repo");
        let ws = Path::new("/repo/sub/ws");
        assert_eq!(
            repo_to_workspace_rel(repo, ws, "sub/ws/file.rs").as_deref(),
            Some("file.rs")
        );
        assert_eq!(repo_to_workspace_rel(repo, ws, "outside.rs"), None);
    }

    #[test]
    fn make_change_rename_old_path_and_staged_flag() {
        let ws = Path::new("/repo");
        let repo = Path::new("/repo");
        let c = make_change('R', ' ', "new.txt", Some("old.txt"), ws, repo).unwrap();
        assert_eq!(c.path, "new.txt");
        assert_eq!(c.old_path.as_deref(), Some("old.txt"));
        assert_eq!(c.index, GitIndexStatus::Renamed);
        assert!(c.staged);
    }

    #[test]
    fn status_sort_key_puts_staged_first() {
        let mk = |path: &str, staged: bool| GitChangeDto {
            path: path.to_string(),
            old_path: None,
            index: if staged {
                GitIndexStatus::Modified
            } else {
                GitIndexStatus::Unmodified
            },
            worktree: GitWorktreeStatus::Modified,
            staged,
        };
        assert!(status_sort_key(&mk("a.rs", true)) < status_sort_key(&mk("b.rs", false)));
        // same staged+M rank, tie-broken by path → compare the (u8,u8) prefix
        let (r1, t1, _) = status_sort_key(&mk("a.rs", true));
        let (r2, t2, _) = status_sort_key(&mk("z.rs", true));
        assert_eq!((r1, t1), (r2, t2));
    }

    // ── integration: real git repo ──────────────────────────────────────

    #[tokio::test]
    async fn integration_status_modified_untracked_rename() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);

        let svc = RuntimeGitQueryService::new(ws.clone(), "agent-1".to_string());

        // clean repo
        let st = svc.status(&GitStatusParams::default()).await.unwrap();
        assert!(st.is_repo);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert!(st.changes.is_empty());

        // modified + untracked (UTF-8 path) + staged rename
        std::fs::write(ws.join("a.txt"), "hello world\n").unwrap();
        std::fs::write(ws.join("新文件.txt"), "中文\n").unwrap();
        std::fs::write(ws.join("b.txt"), "b\n").unwrap();
        assert!(git_in(&ws, ["add", "b.txt"]).status.success());
        assert!(git_in(&ws, ["commit", "-q", "-m", "second"]).status.success());
        assert!(git_in(&ws, ["mv", "b.txt", "b2.txt"]).status.success());

        let st = svc.status(&GitStatusParams::default()).await.unwrap();
        assert!(st.is_repo);
        let names: Vec<&str> = st.changes.iter().map(|c| c.path.as_str()).collect();
        assert!(names.contains(&"a.txt"), "names: {names:?}");
        assert!(names.contains(&"新文件.txt"), "names: {names:?}");
        let renamed = st.changes.iter().find(|c| c.path == "b2.txt").unwrap();
        assert_eq!(renamed.old_path.as_deref(), Some("b.txt"));
        assert!(renamed.staged);
        let untracked = st.changes.iter().find(|c| c.path == "新文件.txt").unwrap();
        assert_eq!(untracked.worktree, GitWorktreeStatus::Untracked);
        assert!(!untracked.staged);
    }

    #[tokio::test]
    async fn integration_diff_five_kinds() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        let svc = RuntimeGitQueryService::new(ws.clone(), "agent-1".to_string());

        // NoChange — tracked & clean
        let d = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "a.txt".into(),
                cached: 0,
            })
            .await
            .unwrap();
        assert_eq!(d.kind, GitDiffKind::NoChange);

        // Modified
        std::fs::write(ws.join("a.txt"), "hello world\n").unwrap();
        let d = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "a.txt".into(),
                cached: 0,
            })
            .await
            .unwrap();
        assert_eq!(d.kind, GitDiffKind::Modified);
        assert_eq!(d.original, "hello\n");
        assert_eq!(d.modified, "hello world\n");

        // Untracked — original is empty
        std::fs::write(ws.join("new.txt"), "new\n").unwrap();
        let d = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "new.txt".into(),
                cached: 0,
            })
            .await
            .unwrap();
        assert_eq!(d.kind, GitDiffKind::Untracked);
        assert_eq!(d.original, "");
        assert_eq!(d.modified, "new\n");

        // Deleted — HEAD text preserved, modified empty
        std::fs::remove_file(ws.join("a.txt")).unwrap();
        let d = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "a.txt".into(),
                cached: 0,
            })
            .await
            .unwrap();
        assert_eq!(d.kind, GitDiffKind::Deleted);
        assert_eq!(d.original, "hello\n"); // HEAD still holds the initial content
        assert_eq!(d.modified, "");
        std::fs::write(ws.join("a.txt"), "hello world\n").unwrap();

        // Binary — NUL byte on either side degrades to kind=binary
        std::fs::write(ws.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        assert!(git_in(&ws, ["add", "bin.dat"]).status.success());
        assert!(git_in(&ws, ["commit", "-q", "-m", "add bin"]).status.success());
        std::fs::write(ws.join("bin.dat"), [0u8, 9, 9, 9]).unwrap();
        let d = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "bin.dat".into(),
                cached: 0,
            })
            .await
            .unwrap();
        assert_eq!(d.kind, GitDiffKind::Binary);
        assert!(d.original.is_empty() && d.modified.is_empty());
    }

    #[tokio::test]
    async fn integration_diff_rename_uses_old_path_for_head() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        std::fs::write(ws.join("b.txt"), "b\n").unwrap();
        assert!(git_in(&ws, ["add", "b.txt"]).status.success());
        assert!(git_in(&ws, ["commit", "-q", "-m", "add b"]).status.success());
        assert!(git_in(&ws, ["mv", "b.txt", "b2.txt"]).status.success());
        // content unchanged → NoChange; original must come from HEAD:old-path
        let svc = RuntimeGitQueryService::new(ws.clone(), "agent-1".to_string());
        let d = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "b2.txt".into(),
                cached: 0,
            })
            .await
            .unwrap();
        assert_eq!(d.kind, GitDiffKind::NoChange);
        assert_eq!(d.original, "b\n");
        assert_eq!(d.modified, "b\n");
    }

    #[tokio::test]
    async fn integration_diff_cached_1_reads_index() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        std::fs::write(ws.join("c.txt"), "c\n").unwrap();
        assert!(git_in(&ws, ["add", "c.txt"]).status.success());
        assert!(git_in(&ws, ["commit", "-q", "-m", "add c"]).status.success());
        std::fs::write(ws.join("c.txt"), "c staged\n").unwrap();
        assert!(git_in(&ws, ["add", "c.txt"]).status.success());

        let svc = RuntimeGitQueryService::new(ws.clone(), "agent-1".to_string());
        let d = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "c.txt".into(),
                cached: 1,
            })
            .await
            .unwrap();
        assert_eq!(d.kind, GitDiffKind::Modified);
        assert_eq!(d.original, "c\n");
        assert_eq!(d.modified, "c staged\n");
    }

    #[tokio::test]
    async fn integration_diff_rejects_path_traversal() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        let svc = RuntimeGitQueryService::new(ws.clone(), "agent-1".to_string());
        let r = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "../evil.txt".into(),
                cached: 0,
            })
            .await;
        match r {
            Err(GitError::InvalidPath(_)) => {}
            other => panic!("expected InvalidPath, got {other:?}"),
        }
        let r = svc
            .log(&GitLogParams {
                workspace_id: None,
                path: Some("/etc/passwd".into()),
                limit: None,
            })
            .await;
        match r {
            Err(GitError::InvalidPath(_)) => {}
            other => panic!("expected InvalidPath, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn integration_workspace_subdir_of_repo_coordinates() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("sub/ws")).unwrap();
        init_repo(&repo); // repo/a.txt committed
        // workspace-tracked file
        std::fs::write(repo.join("sub/ws/tracked.txt"), "t\n").unwrap();
        assert!(git_in(&repo, ["add", "sub/ws/tracked.txt"]).status.success());
        assert!(git_in(&repo, ["commit", "-q", "-m", "add tracked"]).status.success());
        // modify inside + outside the workspace
        std::fs::write(repo.join("sub/ws/tracked.txt"), "t2\n").unwrap();
        std::fs::write(repo.join("outside.txt"), "out\n").unwrap();

        let svc = RuntimeGitQueryService::new(repo.join("sub/ws"), "agent-1".to_string());

        // status is range-filtered to the workspace
        let st = svc.status(&GitStatusParams::default()).await.unwrap();
        assert!(st.is_repo);
        assert_eq!(st.changes.len(), 1, "changes: {:?}", st.changes);
        assert_eq!(st.changes[0].path, "tracked.txt");

        // diff: workspace-relative → repo-root-relative conversion
        let d = svc
            .diff(&GitDiffParams {
                workspace_id: None,
                path: "tracked.txt".into(),
                cached: 0,
            })
            .await
            .unwrap();
        assert_eq!(d.kind, GitDiffKind::Modified);
        assert_eq!(d.original, "t\n");
        assert_eq!(d.modified, "t2\n");

        // log: repo-wide (2 commits) and path-scoped (1 commit)
        let lg = svc.log(&GitLogParams::default()).await.unwrap();
        assert_eq!(lg.commits.len(), 2);
        let lg = svc
            .log(&GitLogParams {
                workspace_id: None,
                path: Some("tracked.txt".into()),
                limit: None,
            })
            .await
            .unwrap();
        assert_eq!(lg.commits.len(), 1);
    }

    #[tokio::test]
    async fn integration_log_limit_and_subject() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        for i in 0..5 {
            std::fs::write(ws.join("a.txt"), format!("v{i}\n")).unwrap();
            assert!(git_in(&ws, ["add", "a.txt"]).status.success());
            let msg = format!("c{i}");
            assert!(git_in(&ws, ["commit", "-q", "-m", msg.as_str()]).status.success());
        }
        let svc = RuntimeGitQueryService::new(ws.clone(), "agent-1".to_string());
        let lg = svc
            .log(&GitLogParams {
                workspace_id: None,
                path: None,
                limit: Some(3),
            })
            .await
            .unwrap();
        assert_eq!(lg.commits.len(), 3);
        assert_eq!(lg.commits[0].subject, "c4"); // newest first
        // over-cap limit clamps to LOG_MAX_LIMIT (200)
        let lg = svc
            .log(&GitLogParams {
                workspace_id: None,
                path: None,
                limit: Some(9999),
            })
            .await
            .unwrap();
        assert_eq!(lg.commits.len(), 6);
    }
}


