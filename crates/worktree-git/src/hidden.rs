//! State that `git status` does not report but that removing the tree would destroy.

use crate::ProcessGit;
use b10x_worktree_domain::Refusal;
use std::path::Path;

/// Namespaces Git stores per worktree; removing the tree deletes every ref in them.
const PER_WORKTREE_REFS: [&str; 3] = ["refs/worktree/", "refs/bisect/", "refs/rewritten/"];

/// Refuse the first kind of hidden state found in `worktree`.
pub(crate) fn require_none(worktree: &Path) -> Result<(), Refusal> {
    require_no_index_flags(worktree)?;
    require_staged_content_on_disk(worktree)?;
    require_no_per_worktree_refs(worktree)?;
    require_no_nested_dot_git(worktree, worktree)
}

fn hidden(path: &[u8], reason: &str) -> Refusal {
    Refusal::new(
        "worktree-hidden-state",
        format!("{}: {reason}", String::from_utf8_lossy(path)),
    )
}

/// Assume-unchanged and skip-worktree entries tell Git status not to look at the file.
fn require_no_index_flags(worktree: &Path) -> Result<(), Refusal> {
    let listing = ProcessGit::output_bytes(worktree, ["ls-files", "-z", "-v"])?;
    for entry in listing
        .split(|byte| *byte == 0)
        .filter(|entry| entry.len() > 2)
    {
        let (tag, path) = (entry[0], &entry[2..]);
        if tag.is_ascii_lowercase() {
            return Err(hidden(
                path,
                "marked assume-unchanged, so Git status does not report its edits; clear it with \
                 `git update-index --no-assume-unchanged`",
            ));
        }
        if tag == b'S' {
            return Err(hidden(
                path,
                "marked skip-worktree, so Git status does not report its edits; clear it with \
                 `git update-index --no-skip-worktree` or leave the sparse checkout",
            ));
        }
    }
    Ok(())
}

/// Staged content that is neither HEAD's nor the working copy's exists only in the index.
fn require_staged_content_on_disk(worktree: &Path) -> Result<(), Refusal> {
    let status = ProcessGit::output_bytes(
        worktree,
        [
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=no",
            "--ignore-submodules=none",
        ],
    )?;
    for entry in status
        .split(|byte| *byte == 0)
        .filter(|entry| entry.len() > 3)
    {
        let (staged, unstaged, path) = (entry[0], entry[1], &entry[3..]);
        if staged == b'U' || unstaged == b'U' || (staged != b' ' && unstaged != b' ') {
            return Err(hidden(
                path,
                "its staged content differs from both HEAD and the working copy, so only the \
                 index holds it; commit it, or stage or unstage the whole file",
            ));
        }
    }
    Ok(())
}

fn require_no_per_worktree_refs(worktree: &Path) -> Result<(), Refusal> {
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend(PER_WORKTREE_REFS);
    let refs = ProcessGit::output(worktree, args)?;
    let refs = refs
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if refs.is_empty() {
        return Ok(());
    }
    Err(Refusal::new(
        "worktree-local-refs",
        format!(
            "{} held by this tree alone would be deleted with it; move each to a shared branch or \
             delete it first",
            refs.join(", ")
        ),
    ))
}

/// Git lists nothing below a directory or file named `.git`, but removal deletes it.
fn require_no_nested_dot_git(root: &Path, directory: &Path) -> Result<(), Refusal> {
    let unreadable = |error: std::io::Error| {
        Refusal::new(
            "worktree-state-unreadable",
            format!("{}: {error}", directory.display()),
        )
    };
    for entry in std::fs::read_dir(directory).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        if entry.file_name() == ".git" {
            if directory == root {
                continue;
            }
            let relative = path.strip_prefix(root).unwrap_or(&path);
            return Err(hidden(
                relative.as_os_str().as_encoded_bytes(),
                "Git neither lists nor archives anything below a nested .git, and removal would \
                 delete it",
            ));
        }
        if entry.file_type().map_err(unreadable)?.is_dir() {
            require_no_nested_dot_git(root, &path)?;
        }
    }
    Ok(())
}
