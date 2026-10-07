//! Local archives that stand in for remote recovery proof.
//!
//! An archive holds every commit a tree adds over the advertised remote refs, the fingerprint of
//! the tree's complete on-disk content, and, when that content differs from HEAD, one binary patch
//! that recreates its tracked, untracked and ignored files over HEAD. A Git repository nested in
//! the tree's files, which Git lists only as `<path>/`, is held as a byte image of its whole
//! directory instead. The values here are I/O-free; the Git adapter writes and verifies the files.

use crate::{Refusal, WorktreeId, WorktreeRecord};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Independent, immutable manifest format identifier of an archive that images no nested
/// repository.
pub const ARCHIVE_FORMAT: &str = "worktree.archive/1";
/// Independent, immutable manifest format identifier of an archive that images at least one
/// nested repository in [`ArchiveManifest::nested_repositories`].
pub const ARCHIVE_FORMAT_V2: &str = "worktree.archive/2";
/// File name prefix of a nested repository image; the image of the `n`th root, counting from 1
/// in path-byte order, is `nested-<n>.tar`.
pub const ARCHIVE_IMAGE_PREFIX: &str = "nested-";

/// File name of the `ordinal`th nested repository image, counting from 1.
pub fn archive_image_file(ordinal: usize) -> String {
    format!("{ARCHIVE_IMAGE_PREFIX}{ordinal}.tar")
}
/// Manifest file name inside one archive directory.
pub const ARCHIVE_MANIFEST_FILE: &str = "manifest.json";
/// Bundle file name inside one archive directory.
pub const ARCHIVE_BUNDLE_FILE: &str = "commits.bundle";
/// Patch file name inside one archive directory.
pub const ARCHIVE_PATCH_FILE: &str = "dirty.patch";
/// The only ref an archive bundle carries; it names the archived HEAD.
pub const ARCHIVE_HEAD_REF: &str = "refs/worktree-archive/head";

/// One file an archive holds, with the digest it was verified at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveFile {
    /// File name relative to the archive directory.
    pub file: String,
    /// Lowercase hexadecimal SHA-256 of the file's bytes.
    pub sha256: String,
    /// File size in bytes.
    pub bytes: u64,
}

/// A byte image of one Git repository nested in the tree's files.
///
/// The image is a tar file holding the repository's root directory and every entry below it,
/// its `.git` included, with permission bits, modification times and symlink targets; entry names
/// are relative to the tree root, so `tar -xpf <image> -C <tree>` restores it. `-p` keeps the
/// recorded permission bits; without it an unprivileged tar narrows them by the umask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NestedRepositoryImage {
    /// Repository root relative to the tree root.
    pub path: String,
    /// The image file, `nested-<n>.tar`.
    pub image: ArchiveFile,
    /// Lowercase hexadecimal SHA-256 of the canonical listing of the root and every entry below
    /// it: kind, permission bits, size, the SHA-256 of a file's content or a symlink's target, and
    /// the path relative to the tree root, sorted by path bytes. The image read back and the
    /// directory on disk must both produce it.
    pub fingerprint: String,
    /// Number of entries the listing holds, the root included.
    pub entries: u64,
}

/// `manifest.json` of one archive, format [`ARCHIVE_FORMAT`] or [`ARCHIVE_FORMAT_V2`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveManifest {
    /// [`ARCHIVE_FORMAT_V2`] when [`Self::nested_repositories`] is not empty, otherwise
    /// [`ARCHIVE_FORMAT`].
    pub format: String,
    /// Registered worktree id.
    pub id: WorktreeId,
    /// Canonical repository root the tree belongs to.
    pub repository_root: PathBuf,
    /// Registered worktree path.
    pub path: PathBuf,
    /// HEAD at archive time.
    pub head: String,
    /// Checked-out branch at archive time; absent for a detached HEAD.
    pub branch: Option<String>,
    /// Commits HEAD added over every advertised remote ref when the archive was written.
    pub unique_commits: Vec<String>,
    /// Bundle carrying the unique commits as [`ARCHIVE_HEAD_REF`]; absent when there are none.
    pub bundle: Option<ArchiveFile>,
    /// Git tree id of the tree's on-disk content outside every imaged nested repository, read
    /// without filters or index flags.
    ///
    /// It is recorded for every archive, including one of a tree Git reports clean, because Git
    /// status can be told not to look at a file. Two observations of the same bytes, modes and
    /// paths produce the same id, so it is what a later removal compares the tree against.
    pub worktree_tree: String,
    /// Binary patch from HEAD to [`Self::worktree_tree`]; absent when they are equal.
    pub patch: Option<ArchiveFile>,
    /// Unix seconds when the archive was written.
    pub created_at: i64,
    /// Byte images of the Git repositories nested in the tree, in path-byte order; never
    /// serialized when empty, so an archive without them stays [`ARCHIVE_FORMAT`] byte for byte.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested_repositories: Vec<NestedRepositoryImage>,
}

impl ArchiveManifest {
    /// The format identifier a manifest with these nested repository images is written as.
    pub fn format_for(nested_repositories: &[NestedRepositoryImage]) -> &'static str {
        if nested_repositories.is_empty() {
            ARCHIVE_FORMAT
        } else {
            ARCHIVE_FORMAT_V2
        }
    }

    /// Refuse a manifest whose format does not match what it holds: [`ARCHIVE_FORMAT`] names no
    /// image and [`ARCHIVE_FORMAT_V2`] at least one, each `nested-<n>.tar` in strictly increasing
    /// path-byte order, with a relative path and at least one entry.
    pub fn require_format(&self) -> Result<(), Refusal> {
        let invalid = |message: String| Err(Refusal::new("archive-invalid", message));
        if !matches!(self.format.as_str(), ARCHIVE_FORMAT | ARCHIVE_FORMAT_V2) {
            return invalid(format!(
                "archive format {:?} is neither {ARCHIVE_FORMAT} nor {ARCHIVE_FORMAT_V2}",
                self.format
            ));
        }
        let expected = Self::format_for(&self.nested_repositories);
        if self.format != expected {
            return invalid(format!(
                "archive format {:?} with {} nested repository image(s) must be {expected}",
                self.format,
                self.nested_repositories.len()
            ));
        }
        let mut previous: Option<&str> = None;
        for (index, image) in self.nested_repositories.iter().enumerate() {
            let file = archive_image_file(index + 1);
            let relative = !image.path.is_empty()
                && !image.path.starts_with('/')
                && !image.path.ends_with('/')
                && image
                    .path
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | ".." | ".git"));
            if image.image.file != file
                || !relative
                || image.entries == 0
                || previous.is_some_and(|previous| previous.as_bytes() >= image.path.as_bytes())
            {
                return invalid(format!(
                    "nested repository image {} for {:?} is not {file} for a distinct relative \
                     path in path-byte order with at least one entry",
                    image.image.file, image.path
                ));
            }
            previous = Some(&image.path);
        }
        Ok(())
    }

    /// Refuse a manifest that was not written for this exact record, or for another HEAD.
    pub fn require_matches(&self, record: &WorktreeRecord, head: &str) -> Result<(), Refusal> {
        self.require_format()?;
        if self.id != record.id
            || self.path != record.path
            || self.repository_root != record.repository_root
        {
            return Err(Refusal::new(
                "archive-invalid",
                format!(
                    "archive was written for {} at {}, not {} at {}",
                    self.id,
                    self.path.display(),
                    record.id,
                    record.path.display()
                ),
            ));
        }
        if self.head != head {
            return Err(Refusal::new(
                "archive-stale",
                format!(
                    "archive holds HEAD {} but the tree is at {head}; rerun `worktree archive \
                     --replace`",
                    self.head
                ),
            ));
        }
        Ok(())
    }
}

/// The archive a removal relied on, carried inside its recovery proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveReference {
    /// Archive directory.
    pub path: PathBuf,
    /// SHA-256 of the manifest bytes that were verified.
    pub manifest_sha256: String,
    /// Commits held by no advertised ref at verification time, each carried by the bundle.
    pub commits: Vec<String>,
    /// Content fingerprint the linked tree was verified against; absent for unlinked residue.
    pub worktree_tree: Option<String>,
}

/// Which tree state an archive verification must establish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveStateCheck<'a> {
    /// Git already unlinked the tree and its residue is proven to be HEAD's tracked content, so
    /// only its commits need the archive.
    Unlinked,
    /// The tree is linked; its complete on-disk content must equal the archived fingerprint.
    Linked {
        /// Linked worktree path to observe.
        worktree: &'a std::path::Path,
    },
}

/// Inputs for writing one archive.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveRequest<'a> {
    /// Registered record the archive is written for.
    pub record: &'a WorktreeRecord,
    /// HEAD observed immediately before archiving.
    pub head: &'a str,
    /// Final archive directory.
    pub destination: &'a std::path::Path,
    /// Move an existing archive aside instead of refusing.
    pub replace: bool,
    /// Caller-supplied time recorded in the manifest.
    pub created_at: i64,
}

/// Result of a successful `archive` operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEvidence {
    /// Archive directory.
    pub path: PathBuf,
    /// Verified manifest, as written.
    pub manifest: ArchiveManifest,
    /// Where a replaced archive was moved, if one was.
    pub superseded: Option<PathBuf>,
    /// State no archive can hold that will still make cleanup refuse this tree.
    pub blocker: Option<Refusal>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Lifecycle;

    fn record() -> WorktreeRecord {
        WorktreeRecord {
            id: WorktreeId::new("tree").unwrap(),
            repository_root: "/workspace/repo".into(),
            path: "/managed/repo/tree".into(),
            purpose: "p".into(),
            owner: "o".into(),
            lifecycle: Lifecycle::Finished,
            created_at: 0,
            last_seen_at: 0,
            finished_at: None,
            head: None,
        }
    }

    fn manifest() -> ArchiveManifest {
        ArchiveManifest {
            format: ARCHIVE_FORMAT.into(),
            id: WorktreeId::new("tree").unwrap(),
            repository_root: "/workspace/repo".into(),
            path: "/managed/repo/tree".into(),
            head: "a".repeat(40),
            branch: None,
            unique_commits: Vec::new(),
            bundle: None,
            worktree_tree: "c".repeat(40),
            patch: None,
            created_at: 1,
            nested_repositories: Vec::new(),
        }
    }

    fn image(ordinal: usize, path: &str) -> NestedRepositoryImage {
        NestedRepositoryImage {
            path: path.into(),
            image: ArchiveFile {
                file: archive_image_file(ordinal),
                sha256: "d".repeat(64),
                bytes: 10240,
            },
            fingerprint: "e".repeat(64),
            entries: 3,
        }
    }

    fn with_images(images: Vec<NestedRepositoryImage>) -> ArchiveManifest {
        let mut manifest = manifest();
        manifest.format = ArchiveManifest::format_for(&images).into();
        manifest.nested_repositories = images;
        manifest
    }

    #[test]
    fn a_manifest_without_images_serializes_exactly_as_version_1() {
        let value = serde_json::to_value(manifest()).unwrap();
        assert_eq!(value["format"], ARCHIVE_FORMAT);
        assert!(value.get("nested_repositories").is_none(), "{value}");
        let parsed: ArchiveManifest = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, manifest());
    }

    #[test]
    fn images_are_version_2_and_each_format_must_match_what_it_holds() {
        let head = "a".repeat(40);
        let imaged = with_images(vec![image(1, "evidence/a"), image(2, "evidence/b")]);
        assert_eq!(imaged.format, ARCHIVE_FORMAT_V2);
        assert!(imaged.require_matches(&record(), &head).is_ok());
        let round_trip: ArchiveManifest =
            serde_json::from_value(serde_json::to_value(&imaged).unwrap()).unwrap();
        assert_eq!(round_trip, imaged);

        let mut version_1_with_images = imaged.clone();
        version_1_with_images.format = ARCHIVE_FORMAT.into();
        let mut version_2_without_images = manifest();
        version_2_without_images.format = ARCHIVE_FORMAT_V2.into();
        for candidate in [version_1_with_images, version_2_without_images] {
            assert_eq!(
                candidate
                    .require_matches(&record(), &head)
                    .unwrap_err()
                    .code,
                "archive-invalid"
            );
        }
    }

    #[test]
    fn images_must_be_numbered_ordered_relative_and_non_empty() {
        let mut empty = image(1, "evidence/a");
        empty.entries = 0;
        for images in [
            vec![image(2, "evidence/a")],
            vec![image(1, "evidence/b"), image(2, "evidence/a")],
            vec![image(1, "evidence/a"), image(2, "evidence/a")],
            vec![image(1, "/evidence/a")],
            vec![image(1, "evidence/../a")],
            vec![image(1, "evidence//a")],
            vec![image(1, "")],
            vec![empty],
        ] {
            let candidate = with_images(images);
            assert_eq!(
                candidate.require_format().unwrap_err().code,
                "archive-invalid",
                "{candidate:?}"
            );
        }
    }

    #[test]
    fn images_refuse_unknown_fields() {
        let mut value = serde_json::to_value(with_images(vec![image(1, "evidence/a")])).unwrap();
        value["nested_repositories"][0]["extra"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<ArchiveManifest>(value).is_err());
    }

    #[test]
    fn a_manifest_for_another_head_is_stale() {
        let error = manifest()
            .require_matches(&record(), &"b".repeat(40))
            .unwrap_err();
        assert_eq!(error.code, "archive-stale");
    }

    #[test]
    fn a_manifest_for_another_record_or_format_is_invalid() {
        let head = "a".repeat(40);
        let mut other_path = manifest();
        other_path.path = "/managed/repo/other".into();
        let mut other_format = manifest();
        other_format.format = "worktree.archive/0".into();
        for candidate in [other_path, other_format] {
            assert_eq!(
                candidate
                    .require_matches(&record(), &head)
                    .unwrap_err()
                    .code,
                "archive-invalid"
            );
        }
        assert!(manifest().require_matches(&record(), &head).is_ok());
    }

    #[test]
    fn manifests_refuse_unknown_fields() {
        let mut value = serde_json::to_value(manifest()).unwrap();
        value["extra"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<ArchiveManifest>(value).is_err());
    }
}
