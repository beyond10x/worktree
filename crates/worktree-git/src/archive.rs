//! Local archives: writing, verifying and discarding the state they hold.
//!
//! Every observation runs with a scratch object directory in front of the repository's own
//! objects, so hashing a tree's files or indexing a bundle never writes the repository, its refs
//! or the tree's index.

use crate::{AdvertisedTips, ProcessGit};
use b10x_worktree::GitPort as _;
use b10x_worktree_domain::{
    ARCHIVE_BUNDLE_FILE, ARCHIVE_FORMAT, ARCHIVE_HEAD_REF, ARCHIVE_MANIFEST_FILE,
    ARCHIVE_PATCH_FILE, ArchiveEvidence, ArchiveFile, ArchiveManifest, ArchiveReference,
    ArchiveRequest, ArchiveStateCheck, Refusal, WorktreeRecord,
};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Mode, blob id and path of one working-tree file, as Git would index it.
struct Entry {
    path: Vec<u8>,
    mode: &'static str,
    id: String,
}

/// A scratch object directory that reads the repository's objects through alternates.
struct Scratch {
    dir: tempfile::TempDir,
    objects: PathBuf,
    source: PathBuf,
    counter: std::cell::Cell<u32>,
}

impl Scratch {
    fn new(repository: &Path, parent: &Path) -> Result<Self, Refusal> {
        let source = ProcessGit::absolute_git_path(repository, "--git-common-dir")?.join("objects");
        let dir = tempfile::Builder::new()
            .prefix(".worktree-scratch-")
            .tempdir_in(parent)
            .map_err(|error| io_refusal("archive-scratch-failed", parent, &error))?;
        let objects = dir.path().join("objects");
        std::fs::create_dir(&objects)
            .map_err(|error| io_refusal("archive-scratch-failed", &objects, &error))?;
        Ok(Self {
            dir,
            objects,
            source,
            counter: std::cell::Cell::new(0),
        })
    }

    /// A fresh path below the scratch directory.
    fn path(&self, name: &str) -> PathBuf {
        let next = self.counter.get() + 1;
        self.counter.set(next);
        self.dir.path().join(format!("{name}-{next}"))
    }

    /// A Git command in `cwd` whose new objects land in scratch and whose index is `index`.
    fn git(&self, cwd: &Path, index: Option<&Path>) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(cwd)
            .env("GIT_OBJECT_DIRECTORY", &self.objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &self.source);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        command
    }

    /// A bare repository whose only objects are the source repository's, via alternates.
    fn bare_repository(&self, repository: &Path, name: &str) -> Result<PathBuf, Refusal> {
        let format = ProcessGit::output(repository, ["rev-parse", "--show-object-format"])?;
        let bare = self.path(name);
        let object_format = format!("--object-format={}", format.trim());
        run(
            Command::new("git").args([
                OsStr::new("init"),
                OsStr::new("--quiet"),
                OsStr::new("--bare"),
                OsStr::new(&object_format),
                bare.as_os_str(),
            ]),
            None,
        )?;
        let mut alternates = self.source.as_os_str().as_bytes().to_vec();
        alternates.push(b'\n');
        let file = bare.join("objects/info/alternates");
        std::fs::write(&file, alternates)
            .map_err(|error| io_refusal("archive-scratch-failed", &file, &error))?;
        Ok(bare)
    }
}

/// Run one Git command, feeding `input` on stdin, and return stdout.
fn run(command: &mut Command, input: Option<Vec<u8>>) -> Result<Vec<u8>, Refusal> {
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Refusal::new("git-unavailable", error.to_string()))?;
    // Feed stdin from another thread so that a large output cannot deadlock against it.
    let writer = input.map(|bytes| {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        std::thread::spawn(move || stdin.write_all(&bytes))
    });
    let output = child
        .wait_with_output()
        .map_err(|error| Refusal::new("git-command-failed", error.to_string()))?;
    if let Some(writer) = writer {
        writer
            .join()
            .map_err(|_| Refusal::new("git-command-failed", "stdin writer panicked"))?
            .map_err(|error| Refusal::new("git-command-failed", error.to_string()))?;
    }
    if !output.status.success() {
        return Err(Refusal::new(
            "git-command-failed",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output.stdout)
}

fn text(bytes: Vec<u8>) -> Result<String, Refusal> {
    String::from_utf8(bytes).map_err(|error| Refusal::new("git-output-not-utf8", error.to_string()))
}

fn io_refusal(code: &str, path: &Path, error: &std::io::Error) -> Refusal {
    Refusal::new(code, format!("{}: {error}", path.display()))
}

fn unsupported(path: &[u8], reason: &str) -> Refusal {
    Refusal::new(
        "archive-unsupported-entry",
        format!("{}: {reason}", String::from_utf8_lossy(path)),
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Digest one archive file as it is on disk now.
fn describe(archive: &Path, name: &str) -> Result<ArchiveFile, Refusal> {
    let path = archive.join(name);
    let bytes =
        std::fs::read(&path).map_err(|error| io_refusal("archive-write-failed", &path, &error))?;
    Ok(ArchiveFile {
        file: name.to_owned(),
        sha256: sha256_hex(&bytes),
        bytes: bytes.len() as u64,
    })
}

/// Refuse an archive file that is missing, renamed or no longer has its recorded digest.
fn require_file(archive: &Path, file: &ArchiveFile, name: &str) -> Result<(), Refusal> {
    if file.file != name {
        return Err(Refusal::new(
            "archive-invalid",
            format!(
                "{} names {:?} where {name} is required",
                archive.display(),
                file.file
            ),
        ));
    }
    let path = archive.join(name);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Refusal::new(
                "archive-incomplete",
                format!("{} is missing", path.display()),
            ));
        }
        Err(error) => return Err(io_refusal("archive-unreadable", &path, &error)),
    };
    if bytes.len() as u64 != file.bytes || sha256_hex(&bytes) != file.sha256 {
        return Err(Refusal::new(
            "archive-digest-mismatch",
            format!(
                "{} no longer has the SHA-256 {} it was verified at",
                path.display(),
                file.sha256
            ),
        ));
    }
    Ok(())
}

/// Read, parse and bind the manifest to this record and HEAD; also return its digest.
fn read_manifest(
    record: &WorktreeRecord,
    archive: &Path,
    head: &str,
) -> Result<(ArchiveManifest, String), Refusal> {
    let path = archive.join(ARCHIVE_MANIFEST_FILE);
    let bytes =
        std::fs::read(&path).map_err(|error| io_refusal("archive-invalid", &path, &error))?;
    let manifest: ArchiveManifest = serde_json::from_slice(&bytes)
        .map_err(|error| Refusal::new("archive-invalid", format!("{}: {error}", path.display())))?;
    manifest.require_matches(record, head)?;
    Ok((manifest, sha256_hex(&bytes)))
}

/// Every file below the worktree, tracked or not, ignored or not; nested repositories refuse.
fn list_files(
    worktree: &Path,
    scratch: &Scratch,
    index: Option<&Path>,
) -> Result<Vec<Vec<u8>>, Refusal> {
    // An empty index turns `--others` into every file on disk; without `--exclude` options the
    // listing includes ignored files.
    let empty = scratch.path("empty-index");
    let index = index.unwrap_or(&empty);
    let listing = run(
        scratch
            .git(worktree, Some(index))
            .args(["ls-files", "-z", "--others"]),
        None,
    )?;
    let mut paths = Vec::new();
    for path in listing
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        if path.ends_with(b"/") {
            return Err(unsupported(
                path,
                "nested repositories and submodules are not archived",
            ));
        }
        if path.contains(&b'\n') {
            return Err(unsupported(
                path,
                "paths containing a newline are not archived",
            ));
        }
        paths.push(path.to_vec());
    }
    Ok(paths)
}

/// Hash each file's raw bytes, without filters, as Git would store it. `write` keeps the blobs.
fn hash_files(
    worktree: &Path,
    scratch: &Scratch,
    paths: Vec<Vec<u8>>,
    write: bool,
) -> Result<Vec<Entry>, Refusal> {
    let links = scratch.path("links");
    let mut modes = Vec::with_capacity(paths.len());
    let mut input = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let absolute = worktree.join(OsStr::from_bytes(path));
        let metadata = std::fs::symlink_metadata(&absolute)
            .map_err(|error| io_refusal("worktree-state-unreadable", &absolute, &error))?;
        let hashed = if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&absolute)
                .map_err(|error| io_refusal("worktree-state-unreadable", &absolute, &error))?;
            std::fs::create_dir_all(&links)
                .map_err(|error| io_refusal("archive-scratch-failed", &links, &error))?;
            let copy = links.join(index.to_string());
            std::fs::write(&copy, target.as_os_str().as_bytes())
                .map_err(|error| io_refusal("archive-scratch-failed", &copy, &error))?;
            modes.push("120000");
            copy.into_os_string()
        } else if metadata.is_file() {
            modes.push(if metadata.permissions().mode() & 0o100 == 0 {
                "100644"
            } else {
                "100755"
            });
            OsString::from(OsStr::from_bytes(path))
        } else {
            return Err(unsupported(
                path,
                "only regular files and symlinks are archived",
            ));
        };
        input.extend_from_slice(hashed.as_bytes());
        input.push(b'\n');
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut command = scratch.git(worktree, None);
    command.arg("hash-object");
    if write {
        command.arg("-w");
    }
    command.args(["--no-filters", "--stdin-paths"]);
    let ids = text(run(&mut command, Some(input))?)?;
    let ids = ids.lines().collect::<Vec<_>>();
    if ids.len() != paths.len() {
        return Err(Refusal::new(
            "worktree-state-unreadable",
            "git hash-object did not hash every file",
        ));
    }
    Ok(paths
        .into_iter()
        .zip(modes)
        .zip(ids)
        .map(|((path, mode), id)| Entry {
            path,
            mode,
            id: id.to_owned(),
        })
        .collect())
}

/// The tree id of the complete on-disk content, plus its entries.
///
/// Every file is read from disk, so neither index flags nor attributes decide what is seen.
/// Files below a nested `.git` stay invisible here; [`crate::hidden`] refuses removal for them.
fn capture(
    worktree: &Path,
    scratch: &Scratch,
    write: bool,
) -> Result<(String, Vec<Entry>), Refusal> {
    let paths = list_files(worktree, scratch, None)?;
    let entries = hash_files(worktree, scratch, paths, write)?;
    let index = scratch.path("state-index");
    let mut info = Vec::new();
    for entry in &entries {
        info.extend_from_slice(format!("{} {}\t", entry.mode, entry.id).as_bytes());
        info.extend_from_slice(&entry.path);
        info.push(0);
    }
    run(
        scratch
            .git(worktree, Some(&index))
            .args(["update-index", "-z", "--index-info"]),
        Some(info),
    )?;
    let mut write_tree = scratch.git(worktree, Some(&index));
    write_tree.arg("write-tree");
    if !write {
        write_tree.arg("--missing-ok");
    }
    let tree = text(run(&mut write_tree, None)?)?.trim().to_owned();
    Ok((tree, entries))
}

/// Refuse a bundle that Git rejects or that does not itself carry every object HEAD adds over
/// `tips`.
fn verify_bundle(
    repository: &Path,
    bundle: &Path,
    head: &str,
    tips: &AdvertisedTips,
    scratch: &Scratch,
) -> Result<(), Refusal> {
    let invalid = |refusal: Refusal| {
        Refusal::new(
            "archive-bundle-invalid",
            format!("{}: {}", bundle.display(), refusal.message),
        )
    };
    run(
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["bundle", "verify", "--quiet"])
            .arg(bundle),
        None,
    )
    .map_err(invalid)?;
    let heads = text(
        run(
            Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["bundle", "list-heads"])
                .arg(bundle),
            None,
        )
        .map_err(invalid)?,
    )?;
    if !heads
        .lines()
        .any(|line| line.split_once(' ') == Some((head, ARCHIVE_HEAD_REF)))
    {
        return Err(Refusal::new(
            "archive-incomplete",
            format!(
                "{} does not name HEAD {head} as {ARCHIVE_HEAD_REF}",
                bundle.display()
            ),
        ));
    }

    // Index the bundle's pack on its own, then compare what it holds with what HEAD needs.
    let isolated = scratch.bare_repository(repository, "verify")?;
    run(
        Command::new("git")
            .arg("-C")
            .arg(&isolated)
            .args(["bundle", "unbundle"])
            .arg(bundle),
        None,
    )
    .map_err(invalid)?;
    let mut carried = BTreeSet::new();
    let packs = isolated.join("objects/pack");
    for entry in std::fs::read_dir(&packs)
        .map_err(|error| io_refusal("archive-bundle-invalid", &packs, &error))?
    {
        let entry = entry.map_err(|error| io_refusal("archive-bundle-invalid", &packs, &error))?;
        if entry.path().extension() != Some(OsStr::new("idx")) {
            continue;
        }
        let index = std::fs::read(entry.path())
            .map_err(|error| io_refusal("archive-bundle-invalid", &entry.path(), &error))?;
        let listing = text(run(
            Command::new("git")
                .arg("-C")
                .arg(&isolated)
                .arg("show-index"),
            Some(index),
        )?)?;
        carried.extend(
            listing
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .map(str::to_owned),
        );
    }
    let mut needed = vec!["rev-list".to_owned(), "--objects".to_owned()];
    needed.extend(ProcessGit::unique_range(head, tips));
    let needed = ProcessGit::containment_output(&isolated, &needed)?;
    let missing = needed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|object| !carried.contains(*object))
        .collect::<Vec<_>>();
    if let Some(first) = missing.first() {
        return Err(Refusal::new(
            "archive-incomplete",
            format!(
                "{} lacks {} object(s) that HEAD {head} adds over the advertised refs, first {first}",
                bundle.display(),
                missing.len()
            ),
        ));
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), Refusal> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| io_refusal("archive-root-invalid", path, &error))
}

fn path_exists(path: &Path) -> Result<bool, Refusal> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_refusal("archive-root-invalid", path, &error)),
    }
}

/// Write, verify and publish one archive.
pub(crate) fn write(request: &ArchiveRequest<'_>) -> Result<ArchiveEvidence, Refusal> {
    let record = request.record;
    let worktree = record.path.as_path();
    let repository = record.repository_root.as_path();
    let head = request.head;
    ProcessGit::validate_object_id(head)?;
    let parent = request.destination.parent().ok_or_else(|| {
        Refusal::new(
            "archive-root-invalid",
            format!("{} has no parent", request.destination.display()),
        )
    })?;
    private_directory(parent)?;
    if !request.replace && path_exists(request.destination)? {
        return Err(Refusal::new(
            "archive-exists",
            format!("{} already holds an archive", request.destination.display()),
        ));
    }
    let staging = tempfile::Builder::new()
        .prefix(&format!(".{}.staging-", record.id))
        .tempdir_in(parent)
        .map_err(|error| io_refusal("archive-write-failed", parent, &error))?;
    let scratch = Scratch::new(repository, parent)?;

    let (_, tips) = ProcessGit::observe_recovery(repository, head)?;
    let unique = ProcessGit::unique_commits(repository, head, &tips)?
        .into_iter()
        .map(|(commit, _)| commit)
        .collect::<Vec<_>>();
    let branch = ProcessGit::output(worktree, ["branch", "--show-current"])?
        .trim()
        .to_owned();
    let bundle = if unique.is_empty() {
        None
    } else {
        Some(write_bundle(
            repository,
            head,
            &tips,
            staging.path(),
            &scratch,
        )?)
    };
    // Fingerprint every archive, even of a tree Git reports clean: status can be told not to look.
    let (worktree_tree, patch) = write_patch(worktree, head, staging.path(), &scratch)?;
    let manifest = ArchiveManifest {
        format: ARCHIVE_FORMAT.to_owned(),
        id: record.id.clone(),
        repository_root: record.repository_root.clone(),
        path: record.path.clone(),
        head: head.to_owned(),
        branch: (!branch.is_empty()).then_some(branch),
        unique_commits: unique,
        bundle,
        worktree_tree,
        patch,
        created_at: request.created_at,
    };
    let mut encoded = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| Refusal::new("archive-write-failed", error.to_string()))?;
    encoded.push(b'\n');
    let manifest_path = staging.path().join(ARCHIVE_MANIFEST_FILE);
    std::fs::write(&manifest_path, encoded)
        .map_err(|error| io_refusal("archive-write-failed", &manifest_path, &error))?;

    // The tree must still be what was archived: same HEAD, same content.
    let after = ProcessGit.worktree_snapshot(repository, worktree)?;
    if after.head != head || capture(worktree, &scratch, false)?.0 != manifest.worktree_tree {
        return Err(Refusal::new(
            "archive-stale",
            format!(
                "{} changed while it was archived; retry",
                worktree.display()
            ),
        ));
    }
    for file in manifest.bundle.iter().chain(manifest.patch.iter()) {
        require_file(staging.path(), file, &file.file)?;
    }
    let superseded = publish(staging, request, parent)?;
    Ok(ArchiveEvidence {
        path: request.destination.to_path_buf(),
        manifest,
        superseded,
        blocker: None,
    })
}

/// Bundle every commit HEAD adds over `tips` as [`ARCHIVE_HEAD_REF`] and verify it.
fn write_bundle(
    repository: &Path,
    head: &str,
    tips: &AdvertisedTips,
    staging: &Path,
    scratch: &Scratch,
) -> Result<ArchiveFile, Refusal> {
    // The ref lives in a scratch repository, so the source repository gains no ref.
    let source = scratch.bare_repository(repository, "bundle")?;
    run(
        Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["update-ref", ARCHIVE_HEAD_REF, head]),
        None,
    )?;
    let mut revisions = format!("{ARCHIVE_HEAD_REF}\n--not\n");
    for tip in tips.keys() {
        revisions.push_str(tip);
        revisions.push('\n');
    }
    let bundle = staging.join(ARCHIVE_BUNDLE_FILE);
    run(
        Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["bundle", "create", "--quiet"])
            .arg(&bundle)
            .arg("--stdin"),
        Some(revisions.into_bytes()),
    )?;
    verify_bundle(repository, &bundle, head, tips, scratch)?;
    describe(staging, ARCHIVE_BUNDLE_FILE)
}

/// Fingerprint the tree's complete content and, when it differs from HEAD, write and verify the
/// binary patch that recreates it over HEAD.
fn write_patch(
    worktree: &Path,
    head: &str,
    staging: &Path,
    scratch: &Scratch,
) -> Result<(String, Option<ArchiveFile>), Refusal> {
    let (tree, _) = capture(worktree, scratch, true)?;
    let head_tree = ProcessGit::output(worktree, ["rev-parse", &format!("{head}^{{tree}}")])?;
    if head_tree.trim() == tree {
        return Ok((tree, None));
    }
    let patch = run(
        scratch.git(worktree, None).args([
            "diff",
            "--binary",
            "--full-index",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            head,
            tree.as_str(),
        ]),
        None,
    )?;
    let patch_path = staging.join(ARCHIVE_PATCH_FILE);
    std::fs::write(&patch_path, &patch)
        .map_err(|error| io_refusal("archive-write-failed", &patch_path, &error))?;
    require_patch_recreates(worktree, scratch, head, &patch_path, &tree)?;
    Ok((tree, Some(describe(staging, ARCHIVE_PATCH_FILE)?)))
}

/// Move a verified staging directory into place; an archive already there is moved aside.
fn publish(
    staging: tempfile::TempDir,
    request: &ArchiveRequest<'_>,
    parent: &Path,
) -> Result<Option<PathBuf>, Refusal> {
    let id = &request.record.id;
    let superseded = if path_exists(request.destination)? {
        let mut aside = parent.join(format!("{id}.superseded-{}", request.created_at));
        let mut attempt = 1;
        while path_exists(&aside)? {
            attempt += 1;
            aside = parent.join(format!("{id}.superseded-{}-{attempt}", request.created_at));
        }
        std::fs::rename(request.destination, &aside)
            .map_err(|error| io_refusal("archive-write-failed", request.destination, &error))?;
        Some(aside)
    } else {
        None
    };
    let staged = staging.keep();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| io_refusal("archive-write-failed", &staged, &error))?;
    std::fs::rename(&staged, request.destination)
        .map_err(|error| io_refusal("archive-write-failed", request.destination, &error))?;
    Ok(superseded)
}

/// Refuse a patch that does not recreate exactly the captured tree over HEAD.
fn require_patch_recreates(
    worktree: &Path,
    scratch: &Scratch,
    head: &str,
    patch: &Path,
    tree: &str,
) -> Result<(), Refusal> {
    let index = scratch.path("apply-index");
    run(
        scratch
            .git(worktree, Some(&index))
            .args(["read-tree", head]),
        None,
    )?;
    run(
        scratch
            .git(worktree, Some(&index))
            .args(["apply", "--cached", "--binary", "--whitespace=nowarn"])
            .arg(patch),
        None,
    )?;
    let recreated = text(run(
        scratch.git(worktree, Some(&index)).arg("write-tree"),
        None,
    )?)?;
    if recreated.trim() != tree {
        return Err(Refusal::new(
            "archive-verification-failed",
            format!(
                "{} recreates tree {} instead of {tree}",
                patch.display(),
                recreated.trim()
            ),
        ));
    }
    Ok(())
}

/// Complete verification of an archive as recovery proof.
pub(crate) fn verify(
    record: &WorktreeRecord,
    archive: &Path,
    head: &str,
    state: ArchiveStateCheck<'_>,
) -> Result<ArchiveReference, Refusal> {
    let (manifest, manifest_sha256) = read_manifest(record, archive, head)?;
    let repository = record.repository_root.as_path();
    if let Some(bundle) = &manifest.bundle {
        require_file(archive, bundle, ARCHIVE_BUNDLE_FILE)?;
    }
    let worktree_tree = match state {
        ArchiveStateCheck::Unlinked => None,
        ArchiveStateCheck::Linked { worktree } => {
            Some(require_state(&manifest, archive, worktree)?.0)
        }
    };
    let parent = archive.parent().unwrap_or(archive);
    let scratch = Scratch::new(repository, parent)?;
    let (_, tips) = ProcessGit::observe_recovery(repository, head)?;
    let commits = ProcessGit::unique_commits(repository, head, &tips)?
        .into_iter()
        .map(|(commit, _)| commit)
        .collect::<Vec<_>>();
    if !commits.is_empty() {
        if manifest.bundle.is_none() {
            return Err(Refusal::new(
                "archive-incomplete",
                format!(
                    "HEAD {head} adds {} commit(s) over the advertised refs and {} holds no bundle",
                    commits.len(),
                    archive.display()
                ),
            ));
        }
        verify_bundle(
            repository,
            &archive.join(ARCHIVE_BUNDLE_FILE),
            head,
            &tips,
            &scratch,
        )?;
    }
    Ok(ArchiveReference {
        path: archive.to_path_buf(),
        manifest_sha256,
        commits,
        worktree_tree,
    })
}

/// Refuse a linked tree whose complete on-disk content is not the archived fingerprint, and
/// return the fingerprint with the per-file entries it was computed from.
fn require_state(
    manifest: &ArchiveManifest,
    archive: &Path,
    worktree: &Path,
) -> Result<(String, Vec<Entry>, Scratch), Refusal> {
    if let Some(patch) = &manifest.patch {
        require_file(archive, patch, ARCHIVE_PATCH_FILE)?;
    }
    let scratch = Scratch::new(worktree, archive.parent().unwrap_or(archive))?;
    let (tree, entries) = capture(worktree, &scratch, false)?;
    if tree != manifest.worktree_tree {
        return Err(stale_state(archive));
    }
    Ok((tree, entries, scratch))
}

fn stale_state(archive: &Path) -> Refusal {
    Refusal::new(
        "archive-stale",
        format!(
            "the tree's content differs from the content {} holds; rerun `worktree archive \
             --replace`",
            archive.display()
        ),
    )
}

/// Local verification that a linked tree's content is the archived state.
pub(crate) fn verify_state(
    record: &WorktreeRecord,
    archive: &Path,
    head: &str,
) -> Result<(), Refusal> {
    let (manifest, _) = read_manifest(record, archive, head)?;
    require_state(&manifest, archive, &record.path).map(|_| ())
}

/// Return a verified archived tree to HEAD, discarding only content that still matches the
/// archive at the moment it is discarded.
///
/// The index is reset first and the working copy is left alone; each tracked file Git then
/// reports as differing is re-hashed immediately before it is overwritten, so an edit made after
/// the last fingerprint is refused, never overwritten.
pub(crate) fn discard(record: &WorktreeRecord, archive: &Path, head: &str) -> Result<(), Refusal> {
    let (manifest, _) = read_manifest(record, archive, head)?;
    let worktree = record.path.as_path();
    let (_, entries, scratch) = require_state(&manifest, archive, worktree)?;
    let archived = entries
        .into_iter()
        .map(|entry| (entry.path, (entry.mode, entry.id)))
        .collect::<BTreeMap<_, _>>();
    let changed = |path: &[u8]| -> Refusal {
        Refusal::new(
            "archive-stale",
            format!(
                "{} changed after it was verified against {}",
                String::from_utf8_lossy(path),
                archive.display()
            ),
        )
    };

    run(
        Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["read-tree", "--reset", "HEAD"]),
        None,
    )?;
    // `--refresh` exits non-zero when files differ, which is exactly the case being handled.
    let _ = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["update-index", "-q", "--refresh"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let differing = run(
        Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["diff-files", "-z", "--name-only"]),
        None,
    )?;
    for path in differing
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let absolute = worktree.join(OsStr::from_bytes(path));
        let unchanged = match std::fs::symlink_metadata(&absolute) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                !archived.contains_key(path)
            }
            Err(error) => return Err(io_refusal("worktree-state-unreadable", &absolute, &error)),
            Ok(_) => {
                let current = hash_files(worktree, &scratch, vec![path.to_vec()], false)?;
                current.first().map(|entry| (entry.mode, entry.id.clone()))
                    == archived.get(path).cloned()
            }
        };
        if !unchanged {
            return Err(changed(path));
        }
        run(
            Command::new("git")
                .arg("-C")
                .arg(worktree)
                .args(["checkout-index", "--force", "-u", "--"])
                .arg(OsStr::from_bytes(path)),
            None,
        )?;
    }

    let index = PathBuf::from(
        ProcessGit::output(
            worktree,
            ["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?
        .trim(),
    );
    let others = list_files(worktree, &scratch, Some(&index))?;
    for entry in hash_files(worktree, &scratch, others, false)? {
        if archived.get(&entry.path) != Some(&(entry.mode, entry.id.clone())) {
            return Err(changed(&entry.path));
        }
        let path = worktree.join(OsStr::from_bytes(&entry.path));
        std::fs::remove_file(&path)
            .map_err(|error| io_refusal("archive-discard-failed", &path, &error))?;
    }
    // Git reports an empty ignored directory as ignored state, and no archive holds a directory
    // without files, so empty directories go too. The caller re-observes the tree afterwards.
    prune_empty_directories(worktree, true);
    Ok(())
}

/// Remove empty directories below `directory` bottom-up, without following symlinks, leaving
/// `.git` and anything Git or the filesystem refuses to remove.
fn prune_empty_directories(directory: &Path, root: bool) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            prune_empty_directories(&entry.path(), false);
        }
    }
    if !root {
        // Fails on a non-empty directory, which is exactly what must stay.
        let _ = std::fs::remove_dir(directory);
    }
}
