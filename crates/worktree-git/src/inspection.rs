//! Non-following, bounded filesystem and Git observations.

use crate::ProcessGit;
use b10x_worktree::{GitPort, InspectionPort};
use b10x_worktree_domain::{InspectionDetails, Refusal, StorageEntry, StorageObservation};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

impl InspectionPort for ProcessGit {
    fn inspection_details(
        &self,
        repository: &Path,
        worktree: &Path,
        max_entries: u64,
    ) -> Result<InspectionDetails, Refusal> {
        let snapshot = self.worktree_snapshot(repository, worktree)?;
        let source = Self::output(
            worktree,
            [
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        )?;
        let branch = Self::output(worktree, ["branch", "--show-current"])?
            .trim_end()
            .to_owned();
        let storage = measure(worktree, max_entries)?;
        let mut details = InspectionDetails {
            snapshot,
            branch,
            tracked_changes: 0,
            untracked_entries: 0,
            ignored_entries: 0,
            ignored_paths: Vec::new(),
            storage,
        };
        read_status(&source, &mut details)?;
        let after = self.worktree_snapshot(repository, worktree)?;
        if after != details.snapshot {
            return Err(Refusal::new(
                "inspection-state-changed",
                "Git state changed during storage inspection; retry this record",
            ));
        }
        Ok(details)
    }
}

fn read_status(source: &str, details: &mut InspectionDetails) -> Result<(), Refusal> {
    let mut fields = source.split('\0').filter(|entry| !entry.is_empty());
    while let Some(entry) = fields.next() {
        let bytes = entry.as_bytes();
        if bytes.len() < 4 || bytes[2] != b' ' {
            return Err(Refusal::new(
                "invalid-git-status",
                "Git returned an invalid porcelain status record",
            ));
        }
        match &bytes[..2] {
            b"!!" => {
                details.ignored_entries += 1;
                if details.ignored_paths.len() < 64 {
                    details.ignored_paths.push(PathBuf::from(&entry[3..]));
                }
            }
            b"??" => details.untracked_entries += 1,
            _ => {
                details.tracked_changes += 1;
                if bytes[..2].iter().any(|byte| matches!(byte, b'R' | b'C'))
                    && fields.next().is_none()
                {
                    return Err(Refusal::new(
                        "invalid-git-status",
                        "Git rename/copy is missing its source path",
                    ));
                }
            }
        }
    }
    Ok(())
}

struct Scanner {
    report: StorageObservation,
    children: BTreeMap<PathBuf, StorageEntry>,
    visited: BTreeSet<(u64, u64)>,
    root_device: u64,
    limit: u64,
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "Unix has inode identities; the portable interface must represent their absence elsewhere"
)]
fn metadata_identity(metadata: &Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "Allocated blocks are unavailable on non-Unix platforms"
)]
fn allocation(metadata: &Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        Some(metadata.blocks().saturating_mul(512))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn measure(root: &Path, limit: u64) -> Result<StorageObservation, Refusal> {
    if limit == 0 {
        return Err(Refusal::new(
            "invalid-inspection-limit",
            "max entries must be greater than zero",
        ));
    }
    let metadata = std::fs::symlink_metadata(root).map_err(|error| {
        Refusal::new(
            "storage-root-unreadable",
            format!("{}: {error}", root.display()),
        )
    })?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(Refusal::new(
            "storage-root-not-directory",
            "storage root must be a real directory",
        ));
    }
    let mut scanner = Scanner {
        report: StorageObservation {
            logical_bytes: 0,
            allocated_bytes: allocation(&metadata).map(|_| 0),
            entries: 0,
            complete: true,
            children: Vec::new(),
            errors: Vec::new(),
        },
        children: BTreeMap::new(),
        visited: BTreeSet::new(),
        root_device: metadata_identity(&metadata).map_or(0, |identity| identity.0),
        limit,
    };
    scanner.visit(root, Path::new("."), 0);
    scanner.report.children = scanner.children.into_values().collect();
    scanner.report.children.sort_by(|a, b| {
        b.allocated_bytes
            .unwrap_or(b.logical_bytes)
            .cmp(&a.allocated_bytes.unwrap_or(a.logical_bytes))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(scanner.report)
}

impl Scanner {
    fn error(&mut self, code: &str, message: String) {
        self.report.complete = false;
        if self.report.errors.len() < 32 {
            self.report.errors.push(Refusal::new(code, message));
        }
    }

    fn visit(&mut self, path: &Path, bucket: &Path, depth: usize) {
        if self.report.entries >= self.limit || depth > 64 {
            self.error(
                "storage-traversal-limit",
                format!("bounded traversal stopped at {}", path.display()),
            );
            return;
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.error(
                    "storage-entry-unreadable",
                    format!("{}: {error}", path.display()),
                );
                return;
            }
        };
        self.report.entries += 1;
        if let Some(identity) = metadata_identity(&metadata) {
            if identity.0 != self.root_device {
                self.error(
                    "storage-other-filesystem",
                    format!("did not cross filesystem at {}", path.display()),
                );
                return;
            }
            if !self.visited.insert(identity) {
                return;
            }
        }
        self.count(bucket, &metadata);
        if !metadata.is_dir() {
            return;
        }
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                self.error(
                    "storage-directory-unreadable",
                    format!("{}: {error}", path.display()),
                );
                return;
            }
        };
        // Bound memory even when a single directory contains millions of children.
        let remaining =
            usize::try_from(self.limit.saturating_sub(self.report.entries)).unwrap_or(usize::MAX);
        let mut children = Vec::new();
        for entry in entries.take(remaining.saturating_add(1)) {
            match entry {
                Ok(entry) => children.push(entry.path()),
                Err(error) => self.error(
                    "storage-entry-unreadable",
                    format!("{}: {error}", path.display()),
                ),
            }
        }
        children.sort();
        for child in children {
            let child_bucket = if depth == 0 {
                Path::new(child.file_name().unwrap_or_default())
            } else {
                bucket
            };
            self.visit(&child, child_bucket, depth + 1);
            if self.report.entries >= self.limit {
                // Exact exhaustion may still be complete if this was the last child. A bounded
                // report conservatively marks it partial rather than claiming unobserved absence.
                self.error(
                    "storage-traversal-limit",
                    format!("entry limit {} reached", self.limit),
                );
                break;
            }
        }
    }

    fn count(&mut self, bucket: &Path, metadata: &Metadata) {
        let logical = metadata.len();
        let allocated = allocation(metadata);
        self.report.logical_bytes = self.report.logical_bytes.saturating_add(logical);
        if let (Some(total), Some(bytes)) = (&mut self.report.allocated_bytes, allocated) {
            *total = total.saturating_add(bytes);
        }
        let child = self
            .children
            .entry(bucket.to_path_buf())
            .or_insert_with(|| StorageEntry {
                path: bucket.to_path_buf(),
                logical_bytes: 0,
                allocated_bytes: allocated.map(|_| 0),
            });
        child.logical_bytes = child.logical_bytes.saturating_add(logical);
        if let (Some(total), Some(bytes)) = (&mut child.allocated_bytes, allocated) {
            *total = total.saturating_add(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_measurement_reports_partial_instead_of_zero() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("evidence.log"), "keep me").unwrap();
        let report = measure(root.path(), 1).unwrap();
        assert!(!report.complete);
        assert_eq!(report.entries, 1);
        assert!(!report.errors.is_empty());
        assert!(measure(&root.path().join("missing"), 100).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn storage_deduplicates_hardlinks_and_never_follows_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = root.path().join("a");
        std::fs::write(&file, vec![0_u8; 8192]).unwrap();
        std::fs::hard_link(&file, root.path().join("b")).unwrap();
        std::fs::write(outside.path().join("secret"), vec![0_u8; 65536]).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();
        std::os::unix::fs::symlink(root.path(), root.path().join("loop")).unwrap();
        let report = measure(root.path(), 100).unwrap();
        assert!(report.complete);
        assert_eq!(report.entries, 5);
        let bytes = report
            .children
            .iter()
            .find(|entry| entry.path == Path::new("a"))
            .unwrap();
        assert_eq!(bytes.logical_bytes, 8192);
        assert!(
            !report
                .children
                .iter()
                .any(|entry| entry.path == Path::new("b"))
        );
        assert!(report.logical_bytes < 65536);
        assert!(measure(&root.path().join("outside"), 100).is_err());
    }
}
