//! Local archives that stand in for remote recovery proof.
//!
//! An archive holds every commit a tree adds over the advertised remote refs, the fingerprint of
//! the tree's complete on-disk content, and, when that content differs from HEAD, one binary patch
//! that recreates its tracked, untracked and ignored files over HEAD. A Git repository nested in
//! the tree's files, which Git lists only as `<path>/`, is held as a byte image of its whole
//! directory instead. The values here are I/O-free; the Git adapter writes and verifies the files.

use crate::{
    ARCHIVE_FORMAT_V3, BuildOutput, LocalRefusal, Refusal, WorktreeId, WorktreeRecord,
    relative_path, require_local,
};
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

/// `manifest.json` of one archive, format [`ARCHIVE_FORMAT`], [`ARCHIVE_FORMAT_V2`] or
/// [`ARCHIVE_FORMAT_V3`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveManifest {
    /// [`ARCHIVE_FORMAT_V3`] when [`Self::build_output`] is present, otherwise
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
    /// Cargo build layout left out of [`Self::patch`] and [`Self::worktree_tree`]; never
    /// serialized when absent, so an archive that left nothing out stays [`ARCHIVE_FORMAT`] or
    /// [`ARCHIVE_FORMAT_V2`] byte for byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_output: Option<BuildOutput>,
}

impl ArchiveManifest {
    /// The format identifier a manifest with these nested repository images, and nothing left
    /// out, is written as.
    pub fn format_for(nested_repositories: &[NestedRepositoryImage]) -> &'static str {
        if nested_repositories.is_empty() {
            ARCHIVE_FORMAT
        } else {
            ARCHIVE_FORMAT_V2
        }
    }

    /// The format identifier this manifest must carry for what it holds.
    pub fn expected_format(&self) -> &'static str {
        if self.build_output.is_some() {
            ARCHIVE_FORMAT_V3
        } else {
            Self::format_for(&self.nested_repositories)
        }
    }

    /// Refuse a manifest whose format does not match what it holds: [`ARCHIVE_FORMAT`] names no
    /// image and leaves nothing out, [`ARCHIVE_FORMAT_V2`] names at least one image and leaves
    /// nothing out, [`ARCHIVE_FORMAT_V3`] records a well-formed [`Self::build_output`]; each
    /// `nested-<n>.tar` in strictly increasing path-byte order, with a relative path and at least
    /// one entry, and whose bundle and patch are not [`ARCHIVE_BUNDLE_FILE`] and
    /// [`ARCHIVE_PATCH_FILE`].
    pub fn require_format(&self) -> Result<(), Refusal> {
        let invalid = |message: String| Err(Refusal::new("archive-invalid", message));
        if !matches!(
            self.format.as_str(),
            ARCHIVE_FORMAT | ARCHIVE_FORMAT_V2 | ARCHIVE_FORMAT_V3
        ) {
            return invalid(format!(
                "archive format {:?} is none of {ARCHIVE_FORMAT}, {ARCHIVE_FORMAT_V2} and \
                 {ARCHIVE_FORMAT_V3}",
                self.format
            ));
        }
        let expected = self.expected_format();
        if self.format != expected {
            return invalid(format!(
                "archive format {:?} with {} nested repository image(s) and {} build output must \
                 be {expected}",
                self.format,
                self.nested_repositories.len(),
                if self.build_output.is_some() {
                    "a"
                } else {
                    "no"
                }
            ));
        }
        if let Some(defect) = self.build_output.as_ref().and_then(BuildOutput::defect) {
            return invalid(defect);
        }
        // Every writer names these exact files; any other name could reach outside the archive.
        for (recorded, expected) in [
            (self.bundle.as_ref(), ARCHIVE_BUNDLE_FILE),
            (self.patch.as_ref(), ARCHIVE_PATCH_FILE),
        ] {
            if let Some(recorded) = recorded.filter(|recorded| recorded.file != expected) {
                return invalid(format!(
                    "archive file {:?} must be named {expected}",
                    recorded.file
                ));
            }
        }
        let mut previous: Option<&str> = None;
        for (index, image) in self.nested_repositories.iter().enumerate() {
            let file = archive_image_file(index + 1);
            let relative = relative_path(&image.path);
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

impl ArchiveManifest {
    /// Every file this manifest names inside its archive directory, itself included.
    pub fn recorded_files(&self) -> Vec<&str> {
        let mut files = vec![ARCHIVE_MANIFEST_FILE];
        files.extend(self.bundle.as_ref().map(|file| file.file.as_str()));
        files.extend(self.patch.as_ref().map(|file| file.file.as_str()));
        files.extend(
            self.nested_repositories
                .iter()
                .map(|image| image.image.file.as_str()),
        );
        files
    }

    /// HEAD followed by every unique commit, each once: the commits pruning must find on a remote.
    pub fn recorded_commits(&self) -> Vec<String> {
        let mut commits = vec![self.head.clone()];
        for commit in &self.unique_commits {
            if !commits.contains(commit) {
                commits.push(commit.clone());
            }
        }
        commits
    }
}

/// The verdict `prune-archives` gives one archive directory (`worktree.archive.PruneVerdict`).
///
/// Only [`PruneVerdict::Removable`] permits deletion; every other variant is a refusal that names
/// why, and nothing turns a refusal into a removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PruneVerdict {
    /// HEAD and every unique commit are ancestors of a freshly advertised ref, the manifest
    /// records no patch and no nested repository image, the registered tree path is absent, and
    /// the directory holds only the files the manifest names.
    Removable,
    /// Some recorded commit is an ancestor of no freshly advertised ref; patch equivalence does
    /// not count.
    CommitsNotOnRemote,
    /// The manifest records a patch: the tree's content differed from HEAD.
    UncommittedState,
    /// The manifest records at least one nested repository image.
    NestedRepositories,
    /// The registered tree path exists; the archive may be its recovery proof.
    TreeStillPresent,
    /// The repository is gone, has no configured remote, or no remote answered.
    RemoteProofUnavailable,
    /// The directory holds an entry the manifest does not name, or a named entry that is not a
    /// regular file.
    UnrecordedContent,
    /// `manifest.json` is missing, unreadable or of an unknown format.
    InvalidManifest,
}

impl PruneVerdict {
    /// The variant's specification name, as human-readable output prints it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Removable => "Removable",
            Self::CommitsNotOnRemote => "CommitsNotOnRemote",
            Self::UncommittedState => "UncommittedState",
            Self::NestedRepositories => "NestedRepositories",
            Self::TreeStillPresent => "TreeStillPresent",
            Self::RemoteProofUnavailable => "RemoteProofUnavailable",
            Self::UnrecordedContent => "UnrecordedContent",
            Self::InvalidManifest => "InvalidManifest",
        }
    }
}

/// What one entry directly inside an archive directory is, observed without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// Anything else: a fifo, socket or device.
    Other,
}

/// One entry directly inside an archive directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveDirectoryEntry {
    /// Entry name; a name that is not UTF-8 is carried lossily and so names no recorded file.
    pub name: String,
    /// What the entry is.
    pub kind: ArchiveEntryKind,
    /// Size in bytes for a regular file, otherwise 0.
    pub bytes: u64,
}

/// One observation of an archive directory: its parsed manifest and every entry in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveContents {
    /// The parsed manifest, or why it is missing, unreadable or of an unknown format.
    pub manifest: Result<ArchiveManifest, String>,
    /// Every entry directly in the directory.
    pub entries: Vec<ArchiveDirectoryEntry>,
}

impl ArchiveContents {
    /// Bytes of the regular files directly in the directory.
    pub fn bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|entry| entry.kind == ArchiveEntryKind::File)
            .map(|entry| entry.bytes)
            .sum()
    }
}

/// What the configured remotes hold of an archive's recorded commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteObservation {
    /// No remote proof could be observed, and why.
    Unavailable(String),
    /// The remotes answered; these recorded commits are held by no freshly advertised ref.
    Observed {
        /// Recorded commits that are an ancestor of no freshly advertised ref.
        not_on_remote: Vec<String>,
    },
}

/// The decision for one archive directory, before any deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneDecision {
    /// The verdict.
    pub verdict: PruneVerdict,
    /// Why, in words.
    pub reason: String,
    /// Recorded commits no freshly advertised ref holds; 0 when the remotes were not asked.
    pub commits_not_on_remote: u64,
}

/// Decide whether one archive may be pruned, checking in the specification's order:
/// [`PruneVerdict::InvalidManifest`], [`PruneVerdict::UnrecordedContent`],
/// [`PruneVerdict::TreeStillPresent`], [`PruneVerdict::NestedRepositories`],
/// [`PruneVerdict::UncommittedState`], [`PruneVerdict::RemoteProofUnavailable`],
/// [`PruneVerdict::CommitsNotOnRemote`], else [`PruneVerdict::Removable`].
///
/// The observations are injected: `tree_present` reports whether the manifest's registered tree
/// path exists (an inspection failure is an `Err` and refuses as present), and `remote` is asked
/// only once every local check has passed.
pub fn decide_archive_prune(
    contents: &ArchiveContents,
    tree_present: impl FnOnce(&ArchiveManifest) -> Result<bool, String>,
    remote: impl FnOnce(&ArchiveManifest) -> RemoteObservation,
) -> PruneDecision {
    let decision = |verdict, reason: String| PruneDecision {
        verdict,
        reason,
        commits_not_on_remote: 0,
    };
    let manifest = match require_local(contents, tree_present) {
        Ok(manifest) => manifest,
        Err((refusal, reason)) => {
            let verdict = match refusal {
                LocalRefusal::InvalidManifest => PruneVerdict::InvalidManifest,
                LocalRefusal::UnrecordedContent => PruneVerdict::UnrecordedContent,
                LocalRefusal::TreeStillPresent => PruneVerdict::TreeStillPresent,
            };
            return decision(verdict, reason);
        }
    };
    if !manifest.nested_repositories.is_empty() {
        let paths = manifest
            .nested_repositories
            .iter()
            .map(|image| image.path.as_str())
            .collect::<Vec<_>>();
        return decision(
            PruneVerdict::NestedRepositories,
            format!("images nested repositories {}", paths.join(", ")),
        );
    }
    if manifest.patch.is_some() {
        return decision(
            PruneVerdict::UncommittedState,
            "records uncommitted content no remote holds".into(),
        );
    }
    match remote(manifest) {
        RemoteObservation::Unavailable(reason) => {
            decision(PruneVerdict::RemoteProofUnavailable, reason)
        }
        RemoteObservation::Observed { not_on_remote } if not_on_remote.is_empty() => decision(
            PruneVerdict::Removable,
            "every recorded commit is an ancestor of a freshly advertised ref".into(),
        ),
        RemoteObservation::Observed { not_on_remote } => PruneDecision {
            verdict: PruneVerdict::CommitsNotOnRemote,
            reason: format!(
                "no freshly advertised ref holds {}; the archive is their only copy",
                not_on_remote.join(", ")
            ),
            commits_not_on_remote: not_on_remote.len() as u64,
        },
    }
}

/// The report of one archive directory assessed by `prune-archives`
/// (`worktree.archive.ArchivePruneAssessment`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivePruneAssessment {
    /// Directory name: `<id>` or a superseded `<id>.superseded-<n>`.
    pub archive_directory: String,
    /// Absolute path of the archive directory.
    pub path: PathBuf,
    /// The manifest's worktree id; absent when the manifest is invalid.
    pub worktree_id: Option<String>,
    /// The manifest's repository root; absent when the manifest is invalid.
    pub repository_root: Option<PathBuf>,
    /// Bytes of the regular files directly in the directory.
    pub bytes: u64,
    /// The verdict.
    pub verdict: PruneVerdict,
    /// Why, in words; under `--apply`, a deletion that was refused says so here.
    pub reason: String,
    /// Recorded commits no freshly advertised ref holds; 0 when the remotes were not asked.
    pub commits_not_on_remote: u64,
    /// True only under `--apply`, for a removable archive that was deleted.
    pub removed: bool,
}

/// An entry below the archive root that is not an archive directory: reported, never deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedArchiveEntry {
    /// Absolute path.
    pub path: PathBuf,
    /// Size in bytes for a regular file, otherwise 0.
    pub bytes: u64,
}

/// Totals of one `prune-archives` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ArchivePruneTotals {
    /// Archive directories assessed.
    pub archives: u64,
    /// Bytes of the archives assessed removable.
    pub removable_bytes: u64,
    /// Bytes of the archives refused.
    pub refused_bytes: u64,
    /// Bytes deleted under `--apply`.
    pub removed_bytes: u64,
}

/// The report of one `prune-archives` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivePruneReport {
    /// Whether this was an `--apply` run.
    pub applied: bool,
    /// One assessment per archive directory in the selection.
    pub archives: Vec<ArchivePruneAssessment>,
    /// Entries below the archive root that are not archive directories.
    pub skipped: Vec<SkippedArchiveEntry>,
    /// Totals over [`Self::archives`].
    pub totals: ArchivePruneTotals,
}

impl ArchivePruneReport {
    /// Build a report and its totals.
    pub fn new(
        applied: bool,
        archives: Vec<ArchivePruneAssessment>,
        skipped: Vec<SkippedArchiveEntry>,
    ) -> Self {
        let mut totals = ArchivePruneTotals {
            archives: archives.len() as u64,
            ..ArchivePruneTotals::default()
        };
        for item in &archives {
            if item.removed {
                totals.removed_bytes += item.bytes;
            }
            if item.verdict == PruneVerdict::Removable {
                totals.removable_bytes += item.bytes;
            } else {
                totals.refused_bytes += item.bytes;
            }
        }
        Self {
            applied,
            archives,
            skipped,
            totals,
        }
    }

    /// Archives an `--apply` run selected but did not delete.
    pub fn refused(&self) -> Vec<&ArchivePruneAssessment> {
        if !self.applied {
            return Vec::new();
        }
        self.archives.iter().filter(|item| !item.removed).collect()
    }
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
            build_output: None,
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
    fn build_output_is_version_3_and_only_version_3_carries_it() {
        let head = "a".repeat(40);
        let output = BuildOutput::merged(
            None,
            vec![crate::BuildOutputTarget {
                path: "target".into(),
                origin: crate::BuildOutputOrigin::Archive,
                files: 2,
                bytes: 20,
            }],
        );
        for images in [Vec::new(), vec![image(1, "evidence/a")]] {
            let mut left_out = with_images(images);
            left_out.build_output.clone_from(&output);
            left_out.format = left_out.expected_format().into();
            assert_eq!(left_out.format, ARCHIVE_FORMAT_V3);
            assert!(left_out.require_matches(&record(), &head).is_ok());
            let value = serde_json::to_value(&left_out).unwrap();
            assert_eq!(value["build_output"]["targets"][0]["origin"], "archive");
            let round_trip: ArchiveManifest = serde_json::from_value(value).unwrap();
            assert_eq!(round_trip, left_out);

            let mut older = left_out.clone();
            older.format = ArchiveManifest::format_for(&older.nested_repositories).into();
            let mut empty = left_out.clone();
            empty.build_output = None;
            let mut wrong_totals = left_out.clone();
            if let Some(output) = wrong_totals.build_output.as_mut() {
                output.bytes += 1;
            }
            for candidate in [older, empty, wrong_totals] {
                assert_eq!(
                    candidate.require_format().unwrap_err().code,
                    "archive-invalid",
                    "{candidate:?}"
                );
            }
        }
    }

    #[test]
    fn manifests_refuse_unknown_fields() {
        let mut value = serde_json::to_value(manifest()).unwrap();
        value["extra"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<ArchiveManifest>(value).is_err());
    }

    fn file(name: &str) -> ArchiveDirectoryEntry {
        ArchiveDirectoryEntry {
            name: name.into(),
            kind: ArchiveEntryKind::File,
            bytes: 10,
        }
    }

    fn contents(manifest: ArchiveManifest) -> ArchiveContents {
        let entries = manifest.recorded_files().into_iter().map(file).collect();
        ArchiveContents {
            manifest: Ok(manifest),
            entries,
        }
    }

    fn held() -> RemoteObservation {
        RemoteObservation::Observed {
            not_on_remote: Vec::new(),
        }
    }

    #[test]
    fn prune_verdicts_are_checked_in_the_specified_order() {
        let absent = |_: &ArchiveManifest| Ok(false);
        let unasked = |_: &ArchiveManifest| -> RemoteObservation { panic!("remote asked") };

        let invalid = ArchiveContents {
            manifest: Err("missing".into()),
            entries: vec![file("stray")],
        };
        let verdict = |contents: &ArchiveContents| {
            decide_archive_prune(contents, |_| Ok(true), unasked).verdict
        };
        assert_eq!(verdict(&invalid), PruneVerdict::InvalidManifest);

        let mut everything = with_images(vec![image(1, "evidence/a")]);
        everything.patch = Some(ArchiveFile {
            file: ARCHIVE_PATCH_FILE.into(),
            sha256: "f".repeat(64),
            bytes: 1,
        });
        let mut stray = contents(everything.clone());
        stray.entries.push(file("notes.txt"));
        assert_eq!(verdict(&stray), PruneVerdict::UnrecordedContent);
        let mut linked = contents(everything.clone());
        linked.entries[0].kind = ArchiveEntryKind::Symlink;
        assert_eq!(verdict(&linked), PruneVerdict::UnrecordedContent);

        assert_eq!(
            verdict(&contents(everything.clone())),
            PruneVerdict::TreeStillPresent
        );
        assert_eq!(
            decide_archive_prune(
                &contents(everything.clone()),
                |_| Err("denied".into()),
                unasked
            )
            .verdict,
            PruneVerdict::TreeStillPresent
        );
        assert_eq!(
            decide_archive_prune(&contents(everything.clone()), absent, unasked).verdict,
            PruneVerdict::NestedRepositories
        );
        everything.nested_repositories.clear();
        everything.format = ARCHIVE_FORMAT.into();
        assert_eq!(
            decide_archive_prune(&contents(everything.clone()), absent, unasked).verdict,
            PruneVerdict::UncommittedState
        );
        everything.patch = None;
        assert_eq!(
            decide_archive_prune(&contents(everything.clone()), absent, |_| {
                RemoteObservation::Unavailable("offline".into())
            })
            .verdict,
            PruneVerdict::RemoteProofUnavailable
        );
        let missing = decide_archive_prune(&contents(everything.clone()), absent, |manifest| {
            RemoteObservation::Observed {
                not_on_remote: manifest.recorded_commits(),
            }
        });
        assert_eq!(missing.verdict, PruneVerdict::CommitsNotOnRemote);
        assert_eq!(missing.commits_not_on_remote, 1);
        assert_eq!(
            decide_archive_prune(&contents(everything), absent, |_| held()).verdict,
            PruneVerdict::Removable
        );
    }

    #[test]
    fn recorded_commits_start_with_head_and_hold_each_commit_once() {
        let mut manifest = manifest();
        manifest.unique_commits = vec!["a".repeat(40), "b".repeat(40)];
        assert_eq!(
            manifest.recorded_commits(),
            vec!["a".repeat(40), "b".repeat(40)]
        );
    }

    #[test]
    fn report_totals_split_removable_and_refused_bytes() {
        let item = |verdict, bytes, removed| ArchivePruneAssessment {
            archive_directory: "tree".into(),
            path: "/archives/repo/tree".into(),
            worktree_id: Some("tree".into()),
            repository_root: None,
            bytes,
            verdict,
            reason: String::new(),
            commits_not_on_remote: 0,
            removed,
        };
        let report = ArchivePruneReport::new(
            true,
            vec![
                item(PruneVerdict::Removable, 5, true),
                item(PruneVerdict::CommitsNotOnRemote, 7, false),
            ],
            Vec::new(),
        );
        assert_eq!(report.totals.archives, 2);
        assert_eq!(report.totals.removable_bytes, 5);
        assert_eq!(report.totals.refused_bytes, 7);
        assert_eq!(report.totals.removed_bytes, 5);
        assert_eq!(report.refused().len(), 1);
    }
}
