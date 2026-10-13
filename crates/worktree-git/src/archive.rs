//! Local archives: writing, verifying and discarding the state they hold.
//!
//! Every observation runs with a scratch object directory in front of the repository's own
//! objects, so hashing a tree's files or indexing a bundle never writes the repository, its refs
//! or the tree's index.
//!
//! A Git repository nested in the tree's files is listed by Git only as `<path>/`, with nothing
//! below it indexed, so no patch can hold it. It is archived as a byte image instead: a tar file
//! of its root directory and every entry below it, written from and verified against a canonical
//! listing whose SHA-256 is the image's fingerprint.

use crate::{AdvertisedTips, ProcessGit, cache};
use b10x_worktree::GitPort as _;
use b10x_worktree_domain::{
    ARCHIVE_BUNDLE_FILE, ARCHIVE_HEAD_REF, ARCHIVE_MANIFEST_FILE, ARCHIVE_PATCH_FILE,
    ArchiveContents, ArchiveDirectoryEntry, ArchiveEntryKind, ArchiveEvidence, ArchiveFile,
    ArchiveManifest, ArchiveReference, ArchiveRequest, ArchiveStateCheck, BuildOutput,
    BuildOutputOrigin, BuildOutputTarget, CACHEDIR_TAG_FILE, CARGO_PROFILE_MARKER,
    ExcludeObservations, FlutterFacts, FlutterOutputKind, KEEP_FILE, KeepSource,
    NestedRepositoryImage, NewFileSize, PatchStripPlan, Refusal, ScannedPatch, ScannedSection,
    TargetFacts, WorktreeRecord, archive_image_file, diff_git_path, exclude_queries,
    exclude_query_path, flutter_output, is_extended_header, keep_source, layout_directory,
    layout_target, relative_path,
};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Nested repository roots relative to the tree root, as raw bytes in path-byte order.
type NestedRoots = Vec<Vec<u8>>;

/// Untracked build layout paths, each with the directory it is layout below.
type LayoutPaths = Vec<(Vec<u8>, Vec<u8>)>;

/// Paths relative to a tree root, as raw bytes.
type PathSet = BTreeSet<Vec<u8>>;

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

fn hex(digest: &[u8]) -> String {
    digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// SHA-256 and length of everything `reader` yields.
fn hash_reader(mut reader: impl Read) -> std::io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hex(&hasher.finalize()), total))
}

/// What one entry of a nested repository image is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Directory,
    Symlink,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "dir",
            Self::Symlink => "symlink",
        }
    }
}

/// One entry of a nested repository's canonical listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    /// Path relative to the tree root, as raw bytes.
    path: Vec<u8>,
    kind: Kind,
    /// Permission bits, `mode & 0o7777`.
    mode: u32,
    /// File length, link-target length, or 0 for a directory.
    size: u64,
    /// SHA-256 of a file's content or a symlink's target; none for a directory.
    sha256: Option<String>,
}

/// SHA-256 of a listing's deterministic serialisation: one NUL-terminated record per entry,
/// `<kind> <mode> <size> <sha256 or ->` and a tab before the path bytes, in path-byte order.
fn fingerprint(listing: &[Record]) -> String {
    let mut serialised = Vec::new();
    for record in listing {
        serialised.extend_from_slice(
            format!(
                "{} {:o} {} {}\t",
                record.kind.label(),
                record.mode,
                record.size,
                record.sha256.as_deref().unwrap_or("-")
            )
            .as_bytes(),
        );
        serialised.extend_from_slice(&record.path);
        serialised.push(0);
    }
    sha256_hex(&serialised)
}

fn lossy(path: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(path)
}

/// A refusal for an entry that changed between two observations of the same pass.
fn changed_while_read(path: &[u8]) -> Refusal {
    Refusal::new(
        "archive-stale",
        format!("{} changed while it was read; retry", lossy(path)),
    )
}

/// Observe one entry below `worktree` without following a symlink, hashing a file's content as
/// it is now.
fn observe(worktree: &Path, relative: &[u8]) -> Result<Record, Refusal> {
    if relative.contains(&b'\n') {
        return Err(unsupported(
            relative,
            "paths containing a newline are not archived",
        ));
    }
    let absolute = worktree.join(OsStr::from_bytes(relative));
    let unreadable =
        |error: std::io::Error| io_refusal("worktree-state-unreadable", &absolute, &error);
    let metadata = std::fs::symlink_metadata(&absolute).map_err(unreadable)?;
    let mode = metadata.mode() & 0o7777;
    let kind = metadata.file_type();
    let (kind, size, sha256) = if kind.is_symlink() {
        let target = std::fs::read_link(&absolute).map_err(unreadable)?;
        let target = target.as_os_str().as_bytes();
        (Kind::Symlink, target.len() as u64, Some(sha256_hex(target)))
    } else if kind.is_dir() {
        (Kind::Directory, 0, None)
    } else if kind.is_file() {
        let file = std::fs::File::open(&absolute).map_err(unreadable)?;
        let opened = file.metadata().map_err(unreadable)?;
        if !opened.is_file() || opened.ino() != metadata.ino() || opened.dev() != metadata.dev() {
            return Err(changed_while_read(relative));
        }
        let (sha256, size) = hash_reader(file).map_err(unreadable)?;
        (Kind::File, size, Some(sha256))
    } else {
        return Err(unsupported(
            relative,
            "only regular files, directories and symlinks are archived",
        ));
    };
    Ok(Record {
        path: relative.to_vec(),
        kind,
        mode,
        size,
        sha256,
    })
}

/// The canonical listing of a nested repository: `root` and every entry below it, symlinks not
/// followed, sorted by path bytes.
fn list_root(worktree: &Path, root: &[u8]) -> Result<Vec<Record>, Refusal> {
    fn walk(worktree: &Path, relative: &[u8], listing: &mut Vec<Record>) -> Result<(), Refusal> {
        let record = observe(worktree, relative)?;
        let directory = record.kind == Kind::Directory;
        listing.push(record);
        if !directory {
            return Ok(());
        }
        let absolute = worktree.join(OsStr::from_bytes(relative));
        let unreadable =
            |error: std::io::Error| io_refusal("worktree-state-unreadable", &absolute, &error);
        for entry in std::fs::read_dir(&absolute).map_err(unreadable)? {
            let entry = entry.map_err(unreadable)?;
            let mut child = relative.to_vec();
            child.push(b'/');
            child.extend_from_slice(entry.file_name().as_bytes());
            walk(worktree, &child, listing)?;
        }
        Ok(())
    }
    let mut listing = Vec::new();
    walk(worktree, root, &mut listing)?;
    listing.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(listing)
}

/// The part of `path` below `directory`, when `path` lies strictly below it.
fn below<'a>(path: &'a [u8], directory: &[u8]) -> Option<&'a [u8]> {
    path.strip_prefix(directory)
        .and_then(|rest| rest.strip_prefix(b"/"))
}

/// Refuse a nested repository that depends on state outside its own directory, or that Git
/// would not recognise as one.
fn require_self_contained(root: &[u8], listing: &[Record]) -> Result<(), Refusal> {
    let mut dot_git = root.to_vec();
    dot_git.extend_from_slice(b"/.git");
    let gitfile = ".git is not a directory, so the repository it names lives outside the image \
                   (a submodule or a linked worktree)";
    match listing.iter().find(|record| record.path == dot_git) {
        None => {
            return Err(unsupported(
                root,
                "Git lists it as a nested repository but it holds no .git",
            ));
        }
        Some(record) if record.kind != Kind::Directory => {
            return Err(unsupported(&dot_git, gitfile));
        }
        Some(_) => {}
    }
    let mut git_directories = Vec::new();
    for record in listing {
        if record.path.rsplit(|byte| *byte == b'/').next() == Some(b".git") {
            if record.kind != Kind::Directory {
                return Err(unsupported(&record.path, gitfile));
            }
            git_directories.push(record.path.as_slice());
        }
    }
    for record in listing {
        for directory in &git_directories {
            let Some(rest) = below(&record.path, directory) else {
                continue;
            };
            let reason = if rest == b"commondir" {
                "a .git holding commondir shares another repository's state"
            } else if rest == b"objects/info/alternates"
                && (record.kind != Kind::File || record.size > 0)
            {
                "non-empty objects/info/alternates borrows objects from another repository"
            } else if rest.starts_with(b"worktrees/")
                || (rest == b"worktrees" && record.kind != Kind::Directory)
            {
                "a .git with worktrees/ entries has linked worktrees outside the image"
            } else if record.kind == Kind::Symlink {
                "a symlink inside a .git makes the repository depend on what it points at"
            } else {
                continue;
            };
            return Err(unsupported(&record.path, reason));
        }
    }
    Ok(())
}

/// Refuse a nested repository at or below which HEAD or the tree's index records anything: such
/// a path is tracked content, not untracked state an image may hold.
fn require_untracked(worktree: &Path, head: &str, roots: &[Vec<u8>]) -> Result<(), Refusal> {
    let committed =
        ProcessGit::output_bytes(worktree, ["ls-tree", "-r", "-z", "--full-tree", head])?;
    let indexed = ProcessGit::output_bytes(worktree, ["ls-files", "-z", "--stage"])?;
    for (source, listing) in [("HEAD", committed), ("the index", indexed)] {
        for entry in listing
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
                continue;
            };
            let (fields, path) = (&entry[..tab], &entry[tab + 1..]);
            for root in roots {
                if path == root.as_slice() && fields.starts_with(b"160000 ") {
                    return Err(unsupported(
                        root,
                        &format!("{source} records it as a submodule (mode 160000)"),
                    ));
                }
                if path == root.as_slice() || below(path, root).is_some() {
                    return Err(unsupported(
                        root,
                        &format!(
                            "{source} tracks {} at or below this nested repository",
                            lossy(path)
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// A reader that fails instead of ending early, so an image never holds a short file.
struct Exact(std::io::Take<std::fs::File>);

impl Read for Exact {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.0.read(buffer)?;
        if read == 0 && self.0.limit() > 0 && !buffer.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file shrank while it was imaged",
            ));
        }
        Ok(read)
    }
}

/// Size of the tar header's link-name field.
const LINK_NAME_FIELD: usize = 100;

/// Append a symlink whose target is stored byte for byte, with a GNU long-link entry when it does
/// not fit the header: the `tar` crate's own link-name setter normalises the target as a path.
fn append_symlink<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    header: &mut tar::Header,
    name: &Path,
    target: &[u8],
) -> std::io::Result<()> {
    let field = &mut header.as_old_mut().linkname;
    field.fill(0);
    if target.len() > LINK_NAME_FIELD {
        let mut long = tar::Header::new_gnu();
        let marker = b"././@LongLink";
        if let Some(gnu) = long.as_gnu_mut() {
            gnu.name[..marker.len()].copy_from_slice(marker);
        }
        long.set_mode(0o644);
        long.set_size(target.len() as u64 + 1);
        long.set_entry_type(tar::EntryType::GNULongLink);
        long.set_cksum();
        builder.append(&long, target.chain(&[0][..]))?;
        field.copy_from_slice(&target[..LINK_NAME_FIELD]);
    } else {
        field[..target.len()].copy_from_slice(target);
    }
    builder.append_data(header, name, std::io::empty())
}

/// Write the entries of `listing`, in its order, from disk into a new tar file at `image`: GNU
/// headers, names relative to the tree root, permission bits, owner ids and modification times,
/// symlinks stored as links.
fn write_image(worktree: &Path, listing: &[Record], image: &Path) -> Result<(), Refusal> {
    let failed = |error: &std::io::Error| io_refusal("archive-write-failed", image, error);
    let file = std::fs::File::create_new(image).map_err(|error| failed(&error))?;
    let mut builder = tar::Builder::new(std::io::BufWriter::new(file));
    for record in listing {
        let absolute = worktree.join(OsStr::from_bytes(&record.path));
        let unreadable =
            |error: std::io::Error| io_refusal("worktree-state-unreadable", &absolute, &error);
        let metadata = std::fs::symlink_metadata(&absolute).map_err(unreadable)?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(metadata.mode() & 0o7777);
        header.set_mtime(u64::try_from(metadata.mtime()).unwrap_or(0));
        header.set_uid(u64::from(metadata.uid()));
        header.set_gid(u64::from(metadata.gid()));
        header.set_size(0);
        let name = Path::new(OsStr::from_bytes(&record.path));
        let kind = metadata.file_type();
        let appended = if kind.is_symlink() {
            let target = std::fs::read_link(&absolute).map_err(unreadable)?;
            header.set_entry_type(tar::EntryType::Symlink);
            append_symlink(
                &mut builder,
                &mut header,
                name,
                target.as_os_str().as_bytes(),
            )
        } else if kind.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            builder.append_data(&mut header, name, std::io::empty())
        } else if kind.is_file() {
            let file = std::fs::File::open(&absolute).map_err(unreadable)?;
            let opened = file.metadata().map_err(unreadable)?;
            if !opened.is_file() || opened.ino() != metadata.ino() || opened.dev() != metadata.dev()
            {
                return Err(changed_while_read(&record.path));
            }
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(opened.len());
            builder.append_data(&mut header, name, Exact(file.take(opened.len())))
        } else {
            return Err(unsupported(
                &record.path,
                "only regular files, directories and symlinks are archived",
            ));
        };
        appended.map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                changed_while_read(&record.path)
            } else {
                failed(&error)
            }
        })?;
    }
    let writer = builder.into_inner().map_err(|error| failed(&error))?;
    let file = writer.into_inner().map_err(|error| failed(error.error()))?;
    file.sync_all().map_err(|error| failed(&error))
}

/// The listing an image holds, read back with the `tar` crate in the image's own order.
fn read_image(image: &Path) -> Result<Vec<Record>, String> {
    let describe = |error: std::io::Error| format!("{}: {error}", image.display());
    let file = std::fs::File::open(image).map_err(describe)?;
    let mut archive = tar::Archive::new(std::io::BufReader::new(file));
    let mut listing = Vec::new();
    for entry in archive.entries().map_err(describe)? {
        let mut entry = entry.map_err(describe)?;
        let mut path = entry.path_bytes().into_owned();
        let mode = entry.header().mode().map_err(describe)? & 0o7777;
        let entry_type = entry.header().entry_type();
        let (kind, size, sha256) = match entry_type {
            tar::EntryType::Regular => {
                let declared = entry.header().size().map_err(describe)?;
                let (sha256, size) = hash_reader(&mut entry).map_err(describe)?;
                if size != declared {
                    return Err(format!(
                        "{}: {} holds {size} of {declared} bytes",
                        image.display(),
                        lossy(&path)
                    ));
                }
                (Kind::File, size, Some(sha256))
            }
            tar::EntryType::Directory => {
                if path.len() > 1 && path.ends_with(b"/") {
                    path.pop();
                }
                (Kind::Directory, 0, None)
            }
            tar::EntryType::Symlink => {
                let target = entry.link_name_bytes().unwrap_or_default().into_owned();
                (
                    Kind::Symlink,
                    target.len() as u64,
                    Some(sha256_hex(&target)),
                )
            }
            other => {
                return Err(format!(
                    "{}: {} is a {other:?} entry, which no image holds",
                    image.display(),
                    lossy(&path)
                ));
            }
        };
        listing.push(Record {
            path,
            kind,
            mode,
            size,
            sha256,
        });
    }
    Ok(listing)
}

/// Image every nested repository root, in path-byte order, into `staging`.
fn write_images(
    worktree: &Path,
    head: &str,
    roots: &[Vec<u8>],
    staging: &Path,
) -> Result<Vec<NestedRepositoryImage>, Refusal> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    require_untracked(worktree, head, roots)?;
    let mut images = Vec::with_capacity(roots.len());
    for (index, root) in roots.iter().enumerate() {
        let path = String::from_utf8(root.clone()).map_err(|_| {
            unsupported(
                root,
                "a nested repository path that is not UTF-8 cannot be named in the manifest",
            )
        })?;
        let listing = list_root(worktree, root)?;
        require_self_contained(root, &listing)?;
        let file = archive_image_file(index + 1);
        let image = staging.join(&file);
        write_image(worktree, &listing, &image)?;
        let read_back = read_image(&image)
            .map_err(|message| Refusal::new("archive-verification-failed", message))?;
        if read_back != listing {
            return Err(Refusal::new(
                "archive-verification-failed",
                format!(
                    "{} does not read back as the listing of {path} it was written from",
                    image.display()
                ),
            ));
        }
        images.push(NestedRepositoryImage {
            path,
            image: describe(staging, &file)?,
            fingerprint: fingerprint(&listing),
            entries: listing.len() as u64,
        });
    }
    Ok(images)
}

/// Whether the nested repositories on disk are exactly those `manifest` images, each with its
/// recorded fingerprint. `roots` are the roots Git lists now.
fn nested_state_matches(
    manifest: &ArchiveManifest,
    worktree: &Path,
    roots: &[Vec<u8>],
) -> Result<bool, Refusal> {
    let recorded = manifest
        .nested_repositories
        .iter()
        .map(|image| image.path.as_bytes());
    if !roots.iter().map(Vec::as_slice).eq(recorded) {
        return Ok(false);
    }
    for image in &manifest.nested_repositories {
        let listing = list_root(worktree, image.path.as_bytes())?;
        if listing.len() as u64 != image.entries || fingerprint(&listing) != image.fingerprint {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Refuse an image file that no longer has its recorded digest, and nested repositories on disk
/// that are not exactly those the archive images.
fn require_images(
    manifest: &ArchiveManifest,
    archive: &Path,
    worktree: &Path,
    roots: &[Vec<u8>],
) -> Result<(), Refusal> {
    for (index, image) in manifest.nested_repositories.iter().enumerate() {
        require_file(archive, &image.image, &archive_image_file(index + 1))?;
    }
    if !nested_state_matches(manifest, worktree, roots)? {
        return Err(stale_state(archive));
    }
    Ok(())
}

/// Give the owner full access to the tree root and every directory from it down to the parent of
/// `relative`, so that the discard can replace or delete `relative`.
///
/// This follows Git's own removal (`make_directories_owner_writable`, issue #16): a directory
/// without the owner write bit would otherwise stop the discard after it had already changed part
/// of the tree. Git tracks no directory mode, so no fingerprint outside a nested repository moves.
/// No symlink is followed, and a directory on another filesystem or owned by another user than
/// the tree root's is left alone. The walk stops at the first component that is missing or not a
/// directory, leaving that case to the operation exactly as before.
fn make_parents_owner_writable(worktree: &Path, relative: &[u8]) -> Result<(), Refusal> {
    let failed =
        |path: &Path, error: &std::io::Error| io_refusal("archive-discard-failed", path, error);
    let root = std::fs::symlink_metadata(worktree).map_err(|error| failed(worktree, &error))?;
    let (device, owner) = (root.dev(), root.uid());
    let mut directory = worktree.to_path_buf();
    let mut parents = relative.split(|byte| *byte == b'/').collect::<Vec<_>>();
    parents.pop();
    for component in std::iter::once(None).chain(parents.into_iter().map(Some)) {
        if let Some(component) = component {
            directory.push(OsStr::from_bytes(component));
        }
        let metadata = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => metadata,
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(failed(&directory, &error)),
        };
        let mode = metadata.mode() & 0o7777;
        if metadata.dev() == device && metadata.uid() == owner && mode & 0o700 != 0o700 {
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(mode | 0o700))
                .map_err(|error| failed(&directory, &error))?;
        }
    }
    Ok(())
}

/// Delete one imaged nested repository bottom-up, each entry only after re-observing it
/// immediately before deletion and finding exactly its record in `expected`.
///
/// A directory is given owner `rwx` only after its record, mode included, has been re-verified;
/// its final check before removal then leaves the mode out. The root's parents are made
/// owner-writable as the discard does for every other entry.
fn remove_imaged_root(
    worktree: &Path,
    root: &[u8],
    expected: &[Record],
    archive: &Path,
) -> Result<(), Refusal> {
    fn remove(
        worktree: &Path,
        relative: &[u8],
        expected: &BTreeMap<&[u8], &Record>,
        removed: &mut usize,
        changed: &dyn Fn(&[u8]) -> Refusal,
    ) -> Result<(), Refusal> {
        let Some(want) = expected.get(relative) else {
            return Err(changed(relative));
        };
        let absolute = worktree.join(OsStr::from_bytes(relative));
        let failed =
            |error: std::io::Error| io_refusal("archive-discard-failed", &absolute, &error);
        if observe(worktree, relative)? != **want {
            return Err(changed(relative));
        }
        if want.kind == Kind::Directory {
            let opened = want.mode & 0o700 != 0o700;
            if opened {
                std::fs::set_permissions(
                    &absolute,
                    std::fs::Permissions::from_mode(want.mode | 0o700),
                )
                .map_err(failed)?;
            }
            for entry in std::fs::read_dir(&absolute).map_err(failed)? {
                let entry = entry.map_err(failed)?;
                let mut child = relative.to_vec();
                child.push(b'/');
                child.extend_from_slice(entry.file_name().as_bytes());
                remove(worktree, &child, expected, removed, changed)?;
            }
            let again = observe(worktree, relative)?;
            let unchanged = if opened {
                again.kind == Kind::Directory
            } else {
                again == **want
            };
            if !unchanged {
                return Err(changed(relative));
            }
            std::fs::remove_dir(&absolute).map_err(failed)?;
        } else {
            std::fs::remove_file(&absolute).map_err(failed)?;
        }
        *removed += 1;
        Ok(())
    }
    let changed = |path: &[u8]| -> Refusal {
        Refusal::new(
            "archive-stale",
            format!(
                "{} changed after it was verified against {}",
                lossy(path),
                archive.display()
            ),
        )
    };
    let by_path = expected
        .iter()
        .map(|record| (record.path.as_slice(), record))
        .collect::<BTreeMap<_, _>>();
    make_parents_owner_writable(worktree, root)?;
    let mut removed = 0;
    remove(worktree, root, &by_path, &mut removed, &changed)?;
    if removed != expected.len() {
        return Err(changed(root));
    }
    Ok(())
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

/// Read and parse a manifest whose format matches what it holds; also return its digest.
fn parse_manifest(archive: &Path) -> Result<(ArchiveManifest, String), Refusal> {
    let path = archive.join(ARCHIVE_MANIFEST_FILE);
    let bytes =
        std::fs::read(&path).map_err(|error| io_refusal("archive-invalid", &path, &error))?;
    let manifest: ArchiveManifest = serde_json::from_slice(&bytes)
        .map_err(|error| Refusal::new("archive-invalid", format!("{}: {error}", path.display())))?;
    manifest.require_format()?;
    Ok((manifest, sha256_hex(&bytes)))
}

/// Read, parse and bind the manifest to this record and HEAD; also return its digest.
fn read_manifest(
    record: &WorktreeRecord,
    archive: &Path,
    head: &str,
) -> Result<(ArchiveManifest, String), Refusal> {
    let (manifest, digest) = parse_manifest(archive)?;
    manifest.require_matches(record, head)?;
    Ok((manifest, digest))
}

/// The nested repository roots, relative to `worktree`, whose images in `archive` still match
/// the tree: every image has its recorded digest, and the roots on disk are exactly those imaged,
/// each with its recorded fingerprint. Anything else refuses.
pub(crate) fn covered_roots(worktree: &Path, archive: &Path) -> Result<Vec<PathBuf>, Refusal> {
    let (manifest, _) = parse_manifest(archive)?;
    if manifest.path != worktree {
        return Err(Refusal::new(
            "archive-invalid",
            format!(
                "{} was written for {}, not {}",
                archive.display(),
                manifest.path.display(),
                worktree.display()
            ),
        ));
    }
    let scratch = Scratch::new(worktree, archive.parent().unwrap_or(archive))?;
    let (_, roots) = list_files(worktree, &scratch, None)?;
    require_images(&manifest, archive, worktree, &roots)?;
    Ok(manifest
        .nested_repositories
        .iter()
        .map(|image| PathBuf::from(&image.path))
        .collect())
}

/// Every file below the worktree, tracked or not, ignored or not, and separately every nested
/// repository root Git lists as `<path>/` (without the slash), sorted by path bytes.
fn list_files(
    worktree: &Path,
    scratch: &Scratch,
    index: Option<&Path>,
) -> Result<(Vec<Vec<u8>>, NestedRoots), Refusal> {
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
    let mut roots = Vec::new();
    for path in listing
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        if path.contains(&b'\n') {
            return Err(unsupported(
                path,
                "paths containing a newline are not archived",
            ));
        }
        match path.strip_suffix(b"/") {
            Some(root) => roots.push(root.to_vec()),
            None => paths.push(path.to_vec()),
        }
    }
    roots.sort();
    Ok((paths, roots))
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

/// Whether a capture leaves build layout out: cargo's, and a Flutter app's output.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LeaveOut<'a> {
    /// Every file counts: formats 1 and 2.
    Nothing,
    /// Build layout below a recognised cargo target or Flutter output directory, untracked in
    /// HEAD and the index, is left out.
    BuildLayout {
        /// HEAD the tracked paths are read from.
        head: &'a str,
        /// The only directories layout may be left out below, when given: verifying an archive
        /// leaves out layout only below the directories its manifest records.
        only: Option<BTreeSet<Vec<u8>>>,
    },
}

impl<'a> LeaveOut<'a> {
    /// What writing an archive at `head` leaves out: layout below every recognised directory.
    fn everything(head: &'a str) -> Self {
        Self::BuildLayout { head, only: None }
    }

    /// What verifying against `manifest` leaves out: for a format 3 archive, build layout below
    /// the directories it records and nowhere else, so an archive written before a kind of
    /// output was recognised still holds that output in its fingerprint and still verifies.
    fn for_manifest(manifest: &'a ArchiveManifest) -> Self {
        if manifest.build_output.is_some() {
            Self::BuildLayout {
                head: &manifest.head,
                only: Some(
                    left_out_targets(manifest)
                        .into_iter()
                        .map(<[u8]>::to_vec)
                        .collect(),
                ),
            }
        } else {
            Self::Nothing
        }
    }
}

/// Build layout observed on disk.
#[derive(Debug, Default)]
struct Layout {
    /// Each left-out file, with the directory it was left out below and its size.
    files: BTreeMap<Vec<u8>, (Vec<u8>, u64)>,
    /// The directories among them that are a Flutter app's output rather than a cargo target.
    flutter: BTreeSet<Vec<u8>>,
}

impl Layout {
    /// The record of what was left out, per directory; `None` when nothing was.
    fn build_output(&self) -> Option<BuildOutput> {
        let mut per_directory: BTreeMap<&[u8], (u64, u64)> = BTreeMap::new();
        for (directory, bytes) in self.files.values() {
            let entry = per_directory.entry(directory).or_default();
            entry.0 += 1;
            entry.1 += bytes;
        }
        let targets = per_directory
            .into_iter()
            .map(|(path, (files, bytes))| BuildOutputTarget {
                path: String::from_utf8_lossy(path).into_owned(),
                origin: BuildOutputOrigin::Archive,
                files,
                bytes,
            })
            .collect();
        BuildOutput::merged(None, targets)
    }

    /// Paths of the directories something was left out below.
    fn used_targets(&self) -> BTreeSet<&[u8]> {
        self.files
            .values()
            .map(|(directory, _)| directory.as_slice())
            .collect()
    }

    /// How a message names the layout below `directory`.
    fn describe(&self, directory: &[u8]) -> String {
        if self.flutter.contains(directory) {
            format!("a Flutter app's output below {}", lossy(directory))
        } else {
            format!("cargo build layout below {}", lossy(directory))
        }
    }
}

/// The cargo targets of a tree that a manifest can name (UTF-8 relative paths): those
/// `discard-cache` recognises among the ignored entries Git reports, and every directory in which
/// one of `paths` is a `CACHEDIR.TAG` or below a profile's `.fingerprint/` that the archive's own
/// structural rule ([`cache::archive_target`]) recognises on disk. One tracked file below a target
/// hides the target from the first and never from the second; the tracked file itself stays
/// archived because it is never build layout.
fn recognised_targets(worktree: &Path, paths: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, Refusal> {
    let mut targets = cache::cargo_targets(worktree)?
        .into_iter()
        .map(|target| target.into_os_string().into_vec())
        .collect::<BTreeSet<_>>();
    let mut candidates = BTreeSet::new();
    for path in paths {
        let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
        let prefix = |count: usize| components[..count].join(&b'/');
        let last = components.len() - 1;
        if last >= 1 && components[last] == CACHEDIR_TAG_FILE.as_bytes() {
            candidates.insert(prefix(last));
        }
        for (index, component) in components.iter().enumerate().take(last).skip(2) {
            if *component == CARGO_PROFILE_MARKER.as_bytes() {
                candidates.insert(prefix(index - 1));
                if index >= 3 {
                    candidates.insert(prefix(index - 2));
                }
            }
        }
    }
    candidates.retain(|candidate| !targets.contains(candidate));
    if !candidates.is_empty() {
        let tracked = cache::tracked_paths(worktree)?;
        for candidate in candidates {
            let relative = Path::new(OsStr::from_bytes(&candidate));
            if cache::archive_target(worktree, relative, &tracked)? {
                targets.insert(candidate);
            }
        }
    }
    Ok(targets
        .into_iter()
        .filter(|target| std::str::from_utf8(target).is_ok_and(relative_path))
        .collect())
}

/// What is known on disk of a recognised target: whether its `CACHEDIR.TAG` is Cargo's. Every
/// recognised target holds a profile.
fn facts_on_disk(worktree: &Path, target: &[u8]) -> TargetFacts {
    TargetFacts {
        signed_tag: cache::signed_tag(&worktree.join(OsStr::from_bytes(target))),
        holds_profile: true,
    }
}

/// Whether `target/prefix` below `worktree` is a cargo profile directory now.
fn profile_on_disk(worktree: &Path, target: &[u8], prefix: &[u8]) -> bool {
    let mut directory = target.to_vec();
    directory.push(b'/');
    directory.extend_from_slice(prefix);
    cache::cargo_profile(&worktree.join(OsStr::from_bytes(&directory)))
}

/// Whether any of `paths` has an ancestor named like a Flutter app's output directory: only
/// then can the one classifier recognise one, and only then are tracked paths read for it.
fn names_flutter_output(paths: &[Vec<u8>]) -> bool {
    paths.iter().any(|path| {
        let mut components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
        components.pop();
        components
            .into_iter()
            .any(|component| FlutterOutputKind::named(component).is_some())
    })
}

/// The build output directories of a tree as seen on disk: recognised cargo targets, and the
/// observations the one classifier needs for a Flutter app's output.
struct Recognised<'a> {
    worktree: &'a Path,
    /// Recognised cargo targets, restricted to `only` when given.
    targets: Vec<Vec<u8>>,
    /// Every path HEAD or the index tracks.
    tracked: BTreeSet<Vec<u8>>,
    /// Every path the index tracks, read only when a path may lie below a Flutter app's output:
    /// the archive's cargo rule reads it to decide whether such a directory is a cargo target.
    indexed: BTreeSet<PathBuf>,
    /// The only directories that count, when given.
    only: Option<BTreeSet<Vec<u8>>>,
    /// What the tree's keep file keeps: never build layout. An invalid keep file keeps
    /// everything, so nothing is left out.
    keep: cache::KeepSnapshot,
}

impl<'a> Recognised<'a> {
    /// Observe the directories `paths` may be build layout below, at `head`.
    fn observe(
        worktree: &'a Path,
        head: &str,
        paths: &[Vec<u8>],
        only: Option<&BTreeSet<Vec<u8>>>,
    ) -> Result<Self, Refusal> {
        let mut targets = recognised_targets(worktree, paths)?;
        if let Some(only) = only {
            targets.retain(|target| only.contains(target));
        }
        let flutter = names_flutter_output(paths);
        let tracked = if targets.is_empty() && !flutter {
            BTreeSet::new()
        } else {
            cache::head_or_index_paths(worktree, head)?
        };
        let indexed = if flutter {
            cache::tracked_paths(worktree)?
        } else {
            BTreeSet::new()
        };
        Ok(Self {
            worktree,
            targets,
            tracked,
            indexed,
            only: only.cloned(),
            keep: cache::KeepSnapshot::read(worktree)?,
        })
    }

    /// The outermost directory `path` is build layout below, observed on disk now, and whether
    /// it is a Flutter app's output. A path the keep file keeps ([`cache::KeepSnapshot`]; all of
    /// them when it is invalid), one with a `.git` component, and one at or below a Git directory
    /// (`git_directory`, [`cache::git_directory`] on disk) is never build layout
    /// ([`layout_directory`]). `facts`, `profile`, `flutter` and `git_directory` may memoise
    /// their observations.
    fn directory_of(
        &self,
        path: &[u8],
        facts: impl FnMut(&[u8]) -> TargetFacts,
        profile: impl FnMut(&[u8], &[u8]) -> bool,
        mut flutter: impl FnMut(&[u8], &[u8]) -> Option<FlutterFacts>,
        git_directory: impl FnMut(&[u8]) -> bool,
    ) -> Option<(Vec<u8>, bool)> {
        if self.tracked.contains(path) || self.keep.keeps(Path::new(OsStr::from_bytes(path))) {
            return None;
        }
        let cargo = layout_target(path, &self.targets, facts, profile)
            .map(|index| self.targets[index].as_slice());
        let output = flutter_output(path, |directory, manifest| {
            if self
                .only
                .as_ref()
                .is_some_and(|only| !only.contains(directory))
            {
                return None;
            }
            flutter(directory, manifest)
        });
        let directory = layout_directory(path, cargo, output, git_directory)?;
        Some((directory.to_vec(), output == Some(directory)))
    }

    /// The same, observed afresh with nothing memoised.
    fn directory_now(&self, path: &[u8]) -> Option<(Vec<u8>, bool)> {
        self.directory_of(
            path,
            |target| facts_on_disk(self.worktree, target),
            |target, prefix| profile_on_disk(self.worktree, target, prefix),
            |directory, manifest| {
                cache::flutter_facts(
                    self.worktree,
                    directory,
                    manifest,
                    &self.tracked,
                    &self.indexed,
                )
            },
            |directory| git_directory_on_disk(self.worktree, directory),
        )
    }
}

/// Whether `directory`, relative to `worktree`, is a Git directory on disk now: the one predicate
/// `discard-cache` keeps a repository by ([`cache::git_directory`]).
fn git_directory_on_disk(worktree: &Path, directory: &[u8]) -> bool {
    cache::git_directory(&worktree.join(OsStr::from_bytes(directory)))
}

/// Which of `paths` are build layout below a directory recognised on disk, restricted to `only`
/// when given.
fn observe_layout(
    worktree: &Path,
    head: &str,
    paths: &[Vec<u8>],
    only: Option<&BTreeSet<Vec<u8>>>,
) -> Result<Layout, Refusal> {
    let recognised = Recognised::observe(worktree, head, paths, only)?;
    if recognised.targets.is_empty() && !names_flutter_output(paths) {
        return Ok(Layout::default());
    }
    let facts = recognised
        .targets
        .iter()
        .map(|target| (target.clone(), facts_on_disk(worktree, target)))
        .collect::<BTreeMap<_, _>>();
    let mut profiles: BTreeMap<Vec<u8>, bool> = BTreeMap::new();
    let mut outputs: BTreeMap<Vec<u8>, Option<FlutterFacts>> = BTreeMap::new();
    let mut git_directories: BTreeMap<Vec<u8>, bool> = BTreeMap::new();
    let mut layout = Layout::default();
    for path in paths {
        let found = recognised.directory_of(
            path,
            |target| facts[target],
            |target, prefix| {
                let mut key = target.to_vec();
                key.push(b'/');
                key.extend_from_slice(prefix);
                *profiles
                    .entry(key)
                    .or_insert_with(|| profile_on_disk(worktree, target, prefix))
            },
            |directory, manifest| {
                *outputs.entry(directory.to_vec()).or_insert_with(|| {
                    cache::flutter_facts(
                        worktree,
                        directory,
                        manifest,
                        &recognised.tracked,
                        &recognised.indexed,
                    )
                })
            },
            |directory| {
                *git_directories
                    .entry(directory.to_vec())
                    .or_insert_with(|| git_directory_on_disk(worktree, directory))
            },
        );
        if let Some((directory, flutter)) = found {
            let absolute = worktree.join(OsStr::from_bytes(path));
            let metadata = std::fs::symlink_metadata(&absolute)
                .map_err(|error| io_refusal("worktree-state-unreadable", &absolute, &error))?;
            if flutter {
                layout.flutter.insert(directory.clone());
            }
            layout
                .files
                .insert(path.clone(), (directory, metadata.len()));
        }
    }
    Ok(layout)
}

/// The tree id of the on-disk content outside every nested repository root and whatever
/// `leave_out` excludes, its entries, those roots, and the build layout left out.
///
/// Every file is read from disk, so neither index flags nor attributes decide what is seen.
/// A nested repository is held by its image, never by this tree. Files below any other nested
/// `.git` stay invisible here; [`crate::hidden`] refuses removal for them.
fn capture(
    worktree: &Path,
    scratch: &Scratch,
    write: bool,
    leave_out: LeaveOut<'_>,
) -> Result<(String, Vec<Entry>, NestedRoots, Layout), Refusal> {
    let (mut paths, roots) = list_files(worktree, scratch, None)?;
    let layout = match leave_out {
        LeaveOut::Nothing => Layout::default(),
        LeaveOut::BuildLayout { head, only } => {
            observe_layout(worktree, head, &paths, only.as_ref())?
        }
    };
    paths.retain(|path| !layout.files.contains_key(path));
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
    Ok((tree, entries, roots, layout))
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

/// Write `manifest` to `path` as every archive writes it: pretty JSON and a final newline.
fn write_manifest(path: &Path, manifest: &ArchiveManifest) -> Result<(), Refusal> {
    let mut encoded = serde_json::to_vec_pretty(manifest)
        .map_err(|error| Refusal::new("archive-write-failed", error.to_string()))?;
    encoded.push(b'\n');
    std::fs::write(path, encoded)
        .and_then(|()| std::fs::File::open(path)?.sync_all())
        .map_err(|error| io_refusal("archive-write-failed", path, &error))
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
    // Build layout, cargo's or a Flutter app's, is left out: the next build recreates it.
    let (worktree_tree, patch, roots, build_output) =
        write_patch(worktree, head, staging.path(), &scratch)?;
    let nested_repositories = write_images(worktree, head, &roots, staging.path())?;
    let mut manifest = ArchiveManifest {
        format: String::new(),
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
        nested_repositories,
        build_output,
    };
    manifest.expected_format().clone_into(&mut manifest.format);
    write_manifest(&staging.path().join(ARCHIVE_MANIFEST_FILE), &manifest)?;

    // The tree must still be what was archived: same HEAD, same content, the same nested
    // repositories, each re-walked to its recorded fingerprint.
    let after = ProcessGit.worktree_snapshot(repository, worktree)?;
    let (after_tree, _, after_roots, _) =
        capture(worktree, &scratch, false, LeaveOut::everything(head))?;
    if after.head != head
        || after_tree != manifest.worktree_tree
        || !nested_state_matches(&manifest, worktree, &after_roots)?
    {
        return Err(Refusal::new(
            "archive-stale",
            format!(
                "{} changed while it was archived; retry",
                worktree.display()
            ),
        ));
    }
    let images = manifest
        .nested_repositories
        .iter()
        .map(|image| &image.image);
    for file in manifest
        .bundle
        .iter()
        .chain(manifest.patch.iter())
        .chain(images)
    {
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

/// Fingerprint the tree's content outside every nested repository and its build layout and,
/// when it differs from HEAD, write and verify the binary patch that recreates it over HEAD. Also
/// return the nested repository roots the fingerprint excludes and the build layout it left out.
fn write_patch(
    worktree: &Path,
    head: &str,
    staging: &Path,
    scratch: &Scratch,
) -> Result<
    (
        String,
        Option<ArchiveFile>,
        NestedRoots,
        Option<BuildOutput>,
    ),
    Refusal,
> {
    let (tree, _, roots, layout) = capture(worktree, scratch, true, LeaveOut::everything(head))?;
    let build_output = layout.build_output();
    let head_tree = ProcessGit::output(worktree, ["rev-parse", &format!("{head}^{{tree}}")])?;
    if head_tree.trim() == tree {
        return Ok((tree, None, roots, build_output));
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
    Ok((
        tree,
        Some(describe(staging, ARCHIVE_PATCH_FILE)?),
        roots,
        build_output,
    ))
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

/// Refuse a linked tree whose complete on-disk content is not the archived state: the fingerprint
/// outside the nested repositories and, for a format 3 archive, outside the build layout below the
/// directories it records, and each nested repository's image and fingerprint. Layout below a
/// directory the archive did not record stays in the fingerprint, so it refuses; the refusal names
/// that directory when one is recognised now. Return the outer fingerprint with the per-file
/// entries it was computed from.
fn require_state(
    manifest: &ArchiveManifest,
    archive: &Path,
    worktree: &Path,
) -> Result<(String, Vec<Entry>, Scratch), Refusal> {
    if let Some(patch) = &manifest.patch {
        require_file(archive, patch, ARCHIVE_PATCH_FILE)?;
    }
    let scratch = Scratch::new(worktree, archive.parent().unwrap_or(archive))?;
    let (tree, entries, roots, _) =
        capture(worktree, &scratch, false, LeaveOut::for_manifest(manifest))?;
    if tree != manifest.worktree_tree {
        if manifest.build_output.is_some() {
            let recorded = left_out_targets(manifest);
            let (paths, _) = list_files(worktree, &scratch, None)?;
            let layout = observe_layout(worktree, &manifest.head, &paths, None)?;
            if let Some(other) = layout
                .used_targets()
                .into_iter()
                .find(|target| !recorded.contains(*target))
            {
                return Err(Refusal::new(
                    "archive-stale",
                    format!(
                        "{} was not left out by {}; rerun `worktree archive --replace`",
                        layout.describe(other),
                        archive.display()
                    ),
                ));
            }
        }
        return Err(stale_state(archive));
    }
    require_images(manifest, archive, worktree, &roots)?;
    Ok((tree, entries, scratch))
}

/// The recognised directories whose build layout an archive left out, re-observed on disk.
struct LeftOut<'a> {
    /// The tree's directories recognised now, restricted to those the manifest records.
    recognised: Option<Recognised<'a>>,
}

impl<'a> LeftOut<'a> {
    /// Recognise the tree's build output directories now, keeping those `manifest` left layout
    /// out below. A format 1 or 2 archive left nothing out.
    fn observe(
        manifest: &ArchiveManifest,
        worktree: &'a Path,
        paths: &[Vec<u8>],
    ) -> Result<Self, Refusal> {
        let recorded = left_out_targets(manifest)
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect::<BTreeSet<_>>();
        let recognised = if recorded.is_empty() {
            None
        } else {
            Some(Recognised::observe(
                worktree,
                &manifest.head,
                paths,
                Some(&recorded),
            )?)
        };
        Ok(Self { recognised })
    }

    /// The recorded directory `path` is build layout below, observed on disk now.
    fn target_of(&self, path: &[u8]) -> Option<Vec<u8>> {
        self.recognised
            .as_ref()
            .and_then(|recognised| recognised.directory_now(path))
            .map(|(directory, _)| directory)
    }

    /// Split untracked paths into build layout, each with its directory, and everything else.
    fn split(&self, paths: Vec<Vec<u8>>) -> (LayoutPaths, Vec<Vec<u8>>) {
        let mut layout = Vec::new();
        let mut others = Vec::new();
        for path in paths {
            match self.target_of(&path) {
                Some(target) => layout.push((path, target)),
                None => others.push(path),
            }
        }
        (layout, others)
    }

    /// Delete each layout file as cache, re-observed immediately before deletion: still a file or
    /// symlink that is build layout below the same recorded directory, recognised now, and
    /// neither kept, with a `.git` component nor at or below a Git directory on disk now
    /// ([`Recognised::directory_of`]), with the keep file's bytes unchanged since the paths were
    /// split ([`cache::KeepSnapshot::unchanged`]). Anything else refuses; such a path is never
    /// deleted as layout.
    fn delete(
        &self,
        layout: LayoutPaths,
        changed: &dyn Fn(&[u8]) -> Refusal,
    ) -> Result<(), Refusal> {
        let Some(recognised) = &self.recognised else {
            return match layout.first() {
                Some((relative, _)) => Err(changed(relative)),
                None => Ok(()),
            };
        };
        for (relative, target) in layout {
            let path = recognised.worktree.join(OsStr::from_bytes(&relative));
            let kind = std::fs::symlink_metadata(&path)
                .map_err(|error| io_refusal("worktree-state-unreadable", &path, &error))?
                .file_type();
            if !(kind.is_file() || kind.is_symlink())
                || !recognised.keep.unchanged(recognised.worktree)
                || self.target_of(&relative).as_ref() != Some(&target)
            {
                return Err(changed(&relative));
            }
            make_parents_owner_writable(recognised.worktree, &relative)?;
            std::fs::remove_file(&path)
                .map_err(|error| io_refusal("archive-discard-failed", &path, &error))?;
        }
        Ok(())
    }
}

/// The target paths whose build layout the archive left out.
fn left_out_targets(manifest: &ArchiveManifest) -> BTreeSet<&[u8]> {
    manifest
        .build_output
        .iter()
        .flat_map(|output| &output.targets)
        .map(|target| target.path.as_bytes())
        .collect()
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
/// the last fingerprint is refused, never overwritten. Every entry is replaced or deleted only
/// after the directories holding it are made owner-writable, so a directory without the owner
/// write bit cannot stop the discard halfway.
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
        make_parents_owner_writable(worktree, path)?;
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
    let (others, roots) = list_files(worktree, &scratch, Some(&index))?;
    let imaged = manifest
        .nested_repositories
        .iter()
        .map(|image| image.path.as_bytes());
    if !roots.iter().map(Vec::as_slice).eq(imaged) {
        return Err(stale_state(archive));
    }
    // The index is HEAD now, so every path here is untracked. Build layout the archive left out
    // is deleted as cache; everything else must still match the archive.
    let left_out = LeftOut::observe(&manifest, worktree, &others)?;
    let (layout, others) = left_out.split(others);
    for entry in hash_files(worktree, &scratch, others, false)? {
        if archived.get(&entry.path) != Some(&(entry.mode, entry.id.clone())) {
            return Err(changed(&entry.path));
        }
        make_parents_owner_writable(worktree, &entry.path)?;
        let path = worktree.join(OsStr::from_bytes(&entry.path));
        std::fs::remove_file(&path)
            .map_err(|error| io_refusal("archive-discard-failed", &path, &error))?;
    }
    left_out.delete(layout, &changed)?;
    remove_imaged_roots(&manifest, archive, worktree)?;
    // Git reports an empty ignored directory as ignored state, and no archive holds a directory
    // without files, so empty directories go too. The caller re-observes the tree afterwards.
    prune_empty_directories(worktree, true);
    Ok(())
}

/// Remove every nested repository a verified archive images, entry by entry against the listing
/// its image holds. The images themselves stay.
fn remove_imaged_roots(
    manifest: &ArchiveManifest,
    archive: &Path,
    worktree: &Path,
) -> Result<(), Refusal> {
    for image in &manifest.nested_repositories {
        let file = archive.join(&image.image.file);
        let expected =
            read_image(&file).map_err(|message| Refusal::new("archive-invalid", message))?;
        if expected.len() as u64 != image.entries || fingerprint(&expected) != image.fingerprint {
            return Err(Refusal::new(
                "archive-invalid",
                format!(
                    "{} does not hold the listing {} records for {}",
                    file.display(),
                    ARCHIVE_MANIFEST_FILE,
                    image.path
                ),
            ));
        }
        remove_imaged_root(worktree, image.path.as_bytes(), &expected, archive)?;
    }
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

/// Observe one archive directory for pruning: every entry directly in it, links not followed,
/// and its manifest, read only when `manifest.json` is a regular file.
pub(crate) fn contents(archive: &Path) -> Result<ArchiveContents, Refusal> {
    let mut entries = Vec::new();
    let unreadable = |error: std::io::Error| io_refusal("archive-unreadable", archive, &error);
    for entry in std::fs::read_dir(archive).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(unreadable)?;
        let kind = metadata.file_type();
        entries.push(ArchiveDirectoryEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind: if kind.is_file() {
                ArchiveEntryKind::File
            } else if kind.is_dir() {
                ArchiveEntryKind::Directory
            } else if kind.is_symlink() {
                ArchiveEntryKind::Symlink
            } else {
                ArchiveEntryKind::Other
            },
            bytes: if kind.is_file() { metadata.len() } else { 0 },
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let manifest = match entries
        .iter()
        .find(|entry| entry.name == ARCHIVE_MANIFEST_FILE)
    {
        None => Err(format!(
            "{} is missing",
            archive.join(ARCHIVE_MANIFEST_FILE).display()
        )),
        Some(entry) if entry.kind != ArchiveEntryKind::File => Err(format!(
            "{} is not a regular file",
            archive.join(ARCHIVE_MANIFEST_FILE).display()
        )),
        Some(_) => parse_manifest(archive)
            .map(|(manifest, _)| manifest)
            .map_err(|refusal| refusal.to_string()),
    };
    Ok(ArchiveContents { manifest, entries })
}

/// Delete an archive assessed removable: each file the manifest names, then the manifest, then
/// the directory with a non-recursive remove. The manifest on disk must still be `manifest`; an
/// entry that is no longer a regular file, or one that appeared, refuses and is kept.
pub(crate) fn delete(archive: &Path, manifest: &ArchiveManifest) -> Result<u64, Refusal> {
    let changed = |message: String| Refusal::new("archive-changed", message);
    let (on_disk, _) = parse_manifest(archive)?;
    if &on_disk != manifest {
        return Err(changed(format!(
            "{} changed after it was assessed",
            archive.join(ARCHIVE_MANIFEST_FILE).display()
        )));
    }
    let mut files = manifest.recorded_files();
    // The manifest goes last, so an interrupted deletion still names what is left.
    files.retain(|file| *file != ARCHIVE_MANIFEST_FILE);
    files.push(ARCHIVE_MANIFEST_FILE);
    let mut freed = 0;
    for file in files {
        let path = archive.join(file);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_refusal("archive-unreadable", &path, &error)),
        };
        if !metadata.file_type().is_file() {
            return Err(changed(format!(
                "{} is no longer a regular file",
                path.display()
            )));
        }
        std::fs::remove_file(&path)
            .map_err(|error| io_refusal("archive-delete-failed", &path, &error))?;
        freed += metadata.len();
    }
    std::fs::remove_dir(archive).map_err(|error| {
        changed(format!(
            "{} was not empty after its recorded files were deleted, so it is kept: {error}",
            archive.display()
        ))
    })?;
    Ok(freed)
}

/// A refusal for a patch that `--strip-build-output` cannot use.
fn unusable(message: String) -> Refusal {
    Refusal::new("archive-patch-unusable", message)
}

/// Stream `patch` line by line, newline included, calling `visit(line, starts_section)` for each,
/// and return the SHA-256 and length of every byte read. Bytes before the first `diff --git` line
/// refuse: the patch is then not one `git diff` wrote.
fn walk_patch(
    patch: &Path,
    mut visit: impl FnMut(&[u8], bool) -> Result<(), Refusal>,
) -> Result<(String, u64), Refusal> {
    use std::io::BufRead as _;
    let unreadable = |error: std::io::Error| io_refusal("archive-unreadable", patch, &error);
    let file = std::fs::File::open(patch).map_err(unreadable)?;
    let mut reader = std::io::BufReader::with_capacity(1 << 16, file);
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut line = Vec::new();
    let mut first = true;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).map_err(unreadable)?;
        if read == 0 {
            break;
        }
        hasher.update(&line);
        total += read as u64;
        let starts = line.starts_with(b"diff --git ");
        if first && !starts {
            return Err(unusable(format!(
                "{} holds bytes before its first diff --git section",
                patch.display()
            )));
        }
        first = false;
        visit(&line, starts)?;
    }
    Ok((hex(&hasher.finalize()), total))
}

/// Refuse a patch whose streamed digest is not the one its manifest records.
fn require_streamed(
    patch: &Path,
    recorded: &ArchiveFile,
    digest: &(String, u64),
) -> Result<(), Refusal> {
    if digest.0 != recorded.sha256 || digest.1 != recorded.bytes {
        return Err(Refusal::new(
            "archive-digest-mismatch",
            format!(
                "{} no longer has the SHA-256 {} it was verified at",
                patch.display(),
                recorded.sha256
            ),
        ));
    }
    Ok(())
}

/// The text a `new file mode` section adds, collected line by line: its added lines, with the
/// newline a `\ No newline at end of file` marker removes taken off. A binary section has none.
#[derive(Debug, Default)]
struct AddedText {
    binary: bool,
    in_hunk: bool,
    after_added: bool,
    bytes: Vec<u8>,
}

impl AddedText {
    /// Observe one line of the section after its `diff --git` line, newline included.
    fn observe(&mut self, line: &[u8]) {
        if self.binary {
            return;
        }
        if !self.in_hunk
            && (line.starts_with(b"GIT binary patch") || line.starts_with(b"Binary files "))
        {
            self.binary = true;
            return;
        }
        if line.starts_with(b"@@") {
            self.in_hunk = true;
            self.after_added = false;
            return;
        }
        if !self.in_hunk {
            return;
        }
        if let Some(added) = line.strip_prefix(b"+") {
            self.bytes.extend_from_slice(added);
            self.after_added = true;
            return;
        }
        if line.starts_with(b"\\") && self.after_added && self.bytes.last() == Some(&b'\n') {
            self.bytes.pop();
        }
        self.after_added = false;
    }

    /// The text added, or `None` for a binary section.
    fn text(self) -> Option<Vec<u8>> {
        (!self.binary).then_some(self.bytes)
    }
}

/// Per index of a section naming the root keep file, the text it adds (`None`: binary).
type KeepTexts = BTreeMap<usize, Option<Vec<u8>>>;

/// Stream an archive's `dirty.patch` into its `diff --git` sections, verifying the digest the
/// manifest records. Nothing is written.
#[cfg(test)]
pub(crate) fn scan_patch(
    archive: &Path,
    manifest: &ArchiveManifest,
) -> Result<Vec<ScannedSection>, Refusal> {
    scan_sections(archive, manifest).map(|(sections, _)| sections)
}

/// [`scan_patch`], also collecting the text each section naming the root keep file adds.
fn scan_sections(
    archive: &Path,
    manifest: &ArchiveManifest,
) -> Result<(Vec<ScannedSection>, KeepTexts), Refusal> {
    let recorded = manifest
        .patch
        .as_ref()
        .ok_or_else(|| unusable(format!("{} records no patch", archive.display())))?;
    let patch = archive.join(ARCHIVE_PATCH_FILE);
    let mut sections: Vec<(ScannedSection, NewFileSize, Option<AddedText>)> = Vec::new();
    let mut in_header = false;
    let digest = walk_patch(&patch, |line, starts| {
        if starts {
            let header = line.strip_suffix(b"\n").unwrap_or(line);
            let decoded = diff_git_path(header);
            let keep = (decoded.as_deref() == Some(KEEP_FILE.as_bytes())).then(AddedText::default);
            sections.push((
                ScannedSection {
                    path: decoded,
                    new_file: false,
                    tracked: false,
                    signed_tag: false,
                    patch_bytes: line.len() as u64,
                    content_bytes: 0,
                },
                NewFileSize::default(),
                keep,
            ));
            in_header = true;
            return Ok(());
        }
        if let Some((section, size, keep)) = sections.last_mut() {
            section.patch_bytes += line.len() as u64;
            if in_header && is_extended_header(line) {
                section.new_file |= line.starts_with(b"new file mode ");
            } else {
                in_header = false;
            }
            size.observe(line);
            if let Some(keep) = keep {
                keep.observe(line);
            }
        }
        Ok(())
    })?;
    require_streamed(&patch, recorded, &digest)?;
    let mut keep_texts = KeepTexts::new();
    let sections = sections
        .into_iter()
        .enumerate()
        .map(|(index, (mut section, size, keep))| {
            section.content_bytes = size.bytes();
            section.signed_tag = size.signed_tag();
            if let Some(keep) = keep {
                keep_texts.insert(index, keep.text());
            }
            section
        })
        .collect();
    Ok((sections, keep_texts))
}

/// Scan an archive's patch ([`scan_patch`]) with the paths of HEAD's files and directories, read
/// from Git and never from the patch's own headers, and what Git's exclude engine says of the
/// paths the domain asks about ([`observe_exclusion`]). The domain marks each section HEAD tracks
/// ([`ScannedPatch::against_head`]) and decides from those observations what is build layout or
/// ignored output.
pub(crate) fn scan_patch_against_head(
    archive: &Path,
    manifest: &ArchiveManifest,
) -> Result<ScannedPatch, Refusal> {
    let (sections, keep_texts) = scan_sections(archive, manifest)?;
    let head = HeadTree::read(archive, manifest)?;
    let mut patch =
        ScannedPatch::against_head(sections, head.files.clone(), head.directories.clone());
    patch.exclusion = observe_exclusion(archive, manifest, &patch, &head, &keep_texts)?;
    Ok(patch)
}

/// The name of a per-directory exclude file.
const GITIGNORE: &str = ".gitignore";

/// HEAD's tree as `git ls-tree` lists it, read from the repository, or from the archive's own
/// bundle through scratch objects when the repository no longer holds it; nothing is written to
/// the repository.
struct HeadTree {
    /// The paths HEAD's tree holds as anything but a directory.
    files: PathSet,
    /// The paths HEAD's tree holds as a directory.
    directories: PathSet,
    /// Path and blob of every `.gitignore` HEAD's tree holds as a regular file.
    ignore_files: Vec<(Vec<u8>, String)>,
    /// The blob of the root keep file, when HEAD's tree holds it as a regular file.
    keep_blob: Option<String>,
    /// The scratch objects HEAD was unbundled into, when the repository does not hold it.
    scratch: Option<Scratch>,
}

impl HeadTree {
    fn read(archive: &Path, manifest: &ArchiveManifest) -> Result<Self, Refusal> {
        ProcessGit::validate_object_id(&manifest.head)?;
        let repository = manifest.repository_root.as_path();
        let arguments =
            |tree: &str| ["ls-tree", "-r", "-t", "-z", "--full-tree", tree].map(String::from);
        let (listing, scratch) =
            if let Ok(listing) = ProcessGit::output_bytes(repository, arguments(&manifest.head)) {
                (listing, None)
            } else {
                let scratch = Scratch::new(repository, archive.parent().unwrap_or(archive))?;
                let tree = archived_head_tree(&scratch, repository, archive, manifest)?;
                let listing = run(scratch.git(repository, None).args(arguments(&tree)), None)?;
                (listing, Some(scratch))
            };
        let mut head = Self {
            files: BTreeSet::new(),
            directories: BTreeSet::new(),
            ignore_files: Vec::new(),
            keep_blob: None,
            scratch,
        };
        for record in listing
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let malformed = || {
                unusable(format!(
                    "git ls-tree {} wrote a malformed record",
                    manifest.head
                ))
            };
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(malformed)?;
            let (info, path) = (&record[..tab], record[tab + 1..].to_vec());
            let mut fields = info.split(|byte| *byte == b' ');
            let (Some(mode), Some(kind), Some(object)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(malformed());
            };
            if kind == b"tree" {
                head.directories.insert(path);
                continue;
            }
            // Git reads an exclude file only when it is a regular file, never through a link.
            if matches!(mode, b"100644" | b"100755") {
                let object = String::from_utf8(object.to_vec()).map_err(|_| malformed())?;
                ProcessGit::validate_object_id(&object)?;
                if path == KEEP_FILE.as_bytes() {
                    head.keep_blob = Some(object);
                } else if path.rsplit(|byte| *byte == b'/').next() == Some(GITIGNORE.as_bytes())
                    && exclude_query_path(&path)
                {
                    head.ignore_files.push((path.clone(), object));
                }
            }
            head.files.insert(path);
        }
        Ok(head)
    }

    /// The content of each blob in `objects`, read in one `git cat-file --batch`.
    fn blobs(&self, repository: &Path, objects: &[&str]) -> Result<Vec<Vec<u8>>, Refusal> {
        if objects.is_empty() {
            return Ok(Vec::new());
        }
        let mut command = if let Some(scratch) = &self.scratch {
            scratch.git(repository, None)
        } else {
            let mut command = Command::new("git");
            command.arg("-C").arg(repository);
            command
        };
        command.args(["cat-file", "--batch"]);
        let mut input = objects.join("\n").into_bytes();
        input.push(b'\n');
        let output = run(&mut command, Some(input))?;
        let malformed = || unusable("git cat-file --batch wrote a malformed record".into());
        let mut rest = output.as_slice();
        let mut blobs = Vec::with_capacity(objects.len());
        for object in objects {
            let newline = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .ok_or_else(malformed)?;
            let header = std::str::from_utf8(&rest[..newline]).map_err(|_| malformed())?;
            rest = &rest[newline + 1..];
            let fields = header.split(' ').collect::<Vec<_>>();
            let [found, "blob", size] = fields.as_slice() else {
                return Err(unusable(format!(
                    "{object} is no blob HEAD holds: {header}"
                )));
            };
            let size = size.parse::<usize>().map_err(|_| malformed())?;
            if found != object || rest.len() <= size || rest[size] != b'\n' {
                return Err(malformed());
            }
            blobs.push(rest[..size].to_vec());
            rest = &rest[size + 1..];
        }
        Ok(blobs)
    }
}

/// What Git's exclude engine says of the paths the domain asks about ([`exclude_queries`]),
/// observed in scratch work trees beside the archive and never in a tree's or the primary
/// checkout's working files: under HEAD's exclude rules, every `.gitignore` HEAD's tree holds
/// (read from Git) and the repository's `info/exclude`; and under the keep file, from the source
/// the domain names ([`keep_source`]). No `core.excludesFile` is read. The scratch is removed on
/// every path out.
fn observe_exclusion(
    archive: &Path,
    manifest: &ArchiveManifest,
    patch: &ScannedPatch,
    head: &HeadTree,
    keep_texts: &KeepTexts,
) -> Result<ExcludeObservations, Refusal> {
    let mut observed = ExcludeObservations::default();
    let queries = exclude_queries(patch);
    if queries.paths.is_empty() {
        return Ok(observed);
    }
    let repository = manifest.repository_root.as_path();
    let keep = match keep_source(patch) {
        KeepSource::Absent => None,
        KeepSource::Head => {
            if let Some(blob) = &head.keep_blob {
                head.blobs(repository, &[blob.as_str()])?.pop()
            } else {
                observed.keep_unreadable = true;
                None
            }
        }
        KeepSource::Patch(index) => {
            if let Some(Some(text)) = keep_texts.get(&index) {
                Some(text.clone())
            } else {
                observed.keep_unreadable = true;
                None
            }
        }
        KeepSource::Unreadable => {
            observed.keep_unreadable = true;
            None
        }
    };
    if observed.keep_unreadable {
        // Every path counts as kept: nothing is stripped, so nothing more is asked.
        return Ok(observed);
    }
    let info_exclude =
        ProcessGit::absolute_git_path(repository, "--git-common-dir")?.join("info/exclude");
    let exclude = match std::fs::read(&info_exclude) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_refusal("archive-unreadable", &info_exclude, &error)),
    };
    let objects = head
        .ignore_files
        .iter()
        .map(|(_, object)| object.as_str())
        .collect::<Vec<_>>();
    let ignore_files = head.blobs(repository, &objects)?;
    let ignores = !ignore_files.is_empty() || !exclude.is_empty();
    let keeps = keep.as_ref().is_some_and(|keep| !keep.is_empty());
    if !ignores && !keeps {
        return Ok(observed);
    }
    let owned;
    let scratch = if let Some(scratch) = &head.scratch {
        scratch
    } else {
        owned = Scratch::new(repository, archive.parent().unwrap_or(archive))?;
        &owned
    };
    if ignores {
        let tree = scratch_directory(scratch, "exclude-tree")?;
        for ((path, _), content) in head.ignore_files.iter().zip(&ignore_files) {
            let file = tree.join(OsStr::from_bytes(path));
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| io_refusal("archive-scratch-failed", parent, &error))?;
            }
            std::fs::write(&file, content)
                .map_err(|error| io_refusal("archive-scratch-failed", &file, &error))?;
        }
        let mut input = Vec::new();
        for path in &queries.paths {
            input.extend_from_slice(path);
            input.push(0);
        }
        for directory in &queries.directories {
            input.extend_from_slice(directory);
            input.extend_from_slice(b"/\0");
        }
        for path in check_ignore(scratch, &tree, &exclude, input)? {
            match path.strip_suffix(b"/") {
                Some(directory) => observed.ignored_directories.insert(directory.to_vec()),
                None => observed.ignored.insert(path),
            };
        }
    }
    if let Some(keep) = keep.filter(|_| keeps) {
        let tree = scratch_directory(scratch, "keep-tree")?;
        let mut input = Vec::new();
        for path in &queries.paths {
            input.extend_from_slice(path);
            input.push(0);
        }
        observed.kept = check_ignore(scratch, &tree, &keep, input)?
            .into_iter()
            .collect();
    }
    Ok(observed)
}

/// A new, empty directory below the scratch directory, as an absolute path.
fn scratch_directory(scratch: &Scratch, name: &str) -> Result<PathBuf, Refusal> {
    let path = scratch.path(name);
    std::fs::create_dir(&path)
        .map_err(|error| io_refusal("archive-scratch-failed", &path, &error))?;
    std::path::absolute(&path).map_err(|error| io_refusal("archive-scratch-failed", &path, &error))
}

/// The NUL-separated `input` paths Git's exclude engine ignores in `tree`, a scratch work tree,
/// with `exclude` as the repository's `info/exclude` and nothing else beside the `.gitignore`
/// files `tree` holds: `git check-ignore --no-index --stdin -z` in a scratch repository, with
/// `core.excludesFile` pointing at an empty file and no user or system configuration. A path
/// ending in `/` is asked as a directory. Every path is asked in this one process.
fn check_ignore(
    scratch: &Scratch,
    tree: &Path,
    exclude: &[u8],
    input: Vec<u8>,
) -> Result<Vec<Vec<u8>>, Refusal> {
    let failed =
        |path: &Path, error: &std::io::Error| io_refusal("archive-scratch-failed", path, error);
    let git_dir = scratch_directory(scratch, "exclude-git")?;
    let isolated = |command: &mut Command| {
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_LITERAL_PATHSPECS")
            .env_remove("GIT_GLOB_PATHSPECS")
            .env_remove("GIT_NOGLOB_PATHSPECS")
            .env_remove("GIT_ICASE_PATHSPECS");
    };
    let mut init = Command::new("git");
    init.args(["init", "--quiet", "--bare"]).arg(&git_dir);
    isolated(&mut init);
    run(&mut init, None)?;
    let info = git_dir.join("info");
    std::fs::create_dir_all(&info).map_err(|error| failed(&info, &error))?;
    let exclude_file = info.join("exclude");
    std::fs::write(&exclude_file, exclude).map_err(|error| failed(&exclude_file, &error))?;
    let no_excludes =
        std::path::absolute(scratch.path("no-excludes")).map_err(|error| failed(tree, &error))?;
    std::fs::write(&no_excludes, b"").map_err(|error| failed(&no_excludes, &error))?;
    let mut setting = OsString::from("core.excludesFile=");
    setting.push(&no_excludes);
    let mut command = Command::new("git");
    command
        .current_dir(tree)
        .arg("-c")
        .arg(setting)
        .arg("--git-dir")
        .arg(&git_dir)
        .arg("--work-tree")
        .arg(tree)
        .args(["check-ignore", "--no-index", "--stdin", "-z"]);
    isolated(&mut command);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Refusal::new("git-unavailable", error.to_string()))?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    // Feed stdin from another thread so that a large output cannot deadlock against it.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child
        .wait_with_output()
        .map_err(|error| Refusal::new("git-command-failed", error.to_string()))?;
    writer
        .join()
        .map_err(|_| Refusal::new("git-command-failed", "stdin writer panicked"))?
        .map_err(|error| Refusal::new("git-command-failed", error.to_string()))?;
    // `check-ignore` exits 1 when it ignores none of the paths.
    match output.status.code() {
        Some(0) => {}
        Some(1) if output.stdout.is_empty() => return Ok(Vec::new()),
        _ => {
            return Err(unusable(format!(
                "git check-ignore failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}
/// Stream `source` into a new file at `destination`, keeping each whole section `strip` does not
/// mark, and verify that `source` still has its recorded digest and the scanned section count.
fn rewrite_patch(
    source: &Path,
    destination: &Path,
    strip: &[bool],
    recorded: &ArchiveFile,
) -> Result<ArchiveFile, Refusal> {
    use std::io::Write as _;
    let failed = |error: &std::io::Error| io_refusal("archive-write-failed", destination, error);
    let file = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| failed(&error))?;
    let mut writer = std::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut sections = 0usize;
    let mut keep = false;
    let digest = walk_patch(source, |line, starts| {
        if starts {
            keep = !*strip.get(sections).ok_or_else(|| {
                unusable(format!(
                    "{} holds more sections than were assessed",
                    source.display()
                ))
            })?;
            sections += 1;
        }
        if keep {
            writer.write_all(line).map_err(|error| failed(&error))?;
            hasher.update(line);
            written += line.len() as u64;
        }
        Ok(())
    })?;
    require_streamed(source, recorded, &digest)?;
    if sections != strip.len() {
        return Err(unusable(format!(
            "{} holds {sections} sections where {} were assessed",
            source.display(),
            strip.len()
        )));
    }
    let file = writer.into_inner().map_err(|error| failed(error.error()))?;
    file.sync_all().map_err(|error| failed(&error))?;
    Ok(ArchiveFile {
        file: ARCHIVE_PATCH_FILE.to_owned(),
        sha256: hex(&hasher.finalize()),
        bytes: written,
    })
}

/// The tree id of the archive's HEAD, read through scratch objects: from the repository, or from
/// the archive's own bundle when the repository no longer holds the commit.
fn archived_head_tree(
    scratch: &Scratch,
    repository: &Path,
    archive: &Path,
    manifest: &ArchiveManifest,
) -> Result<String, Refusal> {
    ProcessGit::validate_object_id(&manifest.head)?;
    let revision = format!("{}^{{tree}}", manifest.head);
    let read = || {
        run(
            scratch
                .git(repository, None)
                .args(["rev-parse", "--verify", "--quiet", &revision]),
            None,
        )
        .and_then(text)
        .map(|tree| tree.trim().to_owned())
    };
    if let Ok(tree) = read() {
        return Ok(tree);
    }
    let Some(bundle) = &manifest.bundle else {
        return Err(unusable(format!(
            "{} does not hold HEAD {} and the archive has no bundle",
            repository.display(),
            manifest.head
        )));
    };
    require_file(archive, bundle, ARCHIVE_BUNDLE_FILE)?;
    run(
        scratch
            .git(repository, None)
            .args(["bundle", "unbundle"])
            .arg(archive.join(ARCHIVE_BUNDLE_FILE)),
        None,
    )?;
    read().map_err(|refusal| {
        unusable(format!(
            "HEAD {} is in neither {} nor the archive's bundle: {}",
            manifest.head,
            repository.display(),
            refusal.message
        ))
    })
}

/// Rewrite an archive assessed strippable: write the new patch in a scratch directory beside the
/// archive, verify it applies over HEAD in a scratch index and recompute `worktree_tree` from it,
/// then replace `dirty.patch` (or delete it when nothing remains) and write the format 3 manifest
/// last. The manifest on disk must still be `manifest`; any refusal leaves the archive untouched.
pub(crate) fn strip(
    archive: &Path,
    manifest: &ArchiveManifest,
    plan: &PatchStripPlan,
) -> Result<ArchiveManifest, Refusal> {
    let changed = |message: String| Refusal::new("archive-changed", message);
    let manifest_path = archive.join(ARCHIVE_MANIFEST_FILE);
    let unchanged = || -> Result<(), Refusal> {
        let (on_disk, _) = parse_manifest(archive)?;
        if &on_disk != manifest {
            return Err(changed(format!(
                "{} changed after it was assessed",
                manifest_path.display()
            )));
        }
        Ok(())
    };
    unchanged()?;
    let recorded = manifest
        .patch
        .as_ref()
        .ok_or_else(|| unusable(format!("{} records no patch", archive.display())))?;
    let repository = manifest.repository_root.as_path();
    let parent = archive.parent().unwrap_or(archive);
    let scratch = Scratch::new(repository, parent)?;
    let patch_path = archive.join(ARCHIVE_PATCH_FILE);
    let staged_patch = scratch.path("dirty.patch");
    let rewritten = rewrite_patch(&patch_path, &staged_patch, &plan.strip, recorded)?;
    let head_tree = archived_head_tree(&scratch, repository, archive, manifest)?;
    let remains = plan.sections_kept > 0;
    let worktree_tree = if remains {
        let index = scratch.path("strip-index");
        let not_applied = |refusal: Refusal| {
            unusable(format!(
                "the stripped patch does not apply over HEAD {}: {}",
                manifest.head, refusal.message
            ))
        };
        run(
            scratch
                .git(repository, Some(&index))
                .args(["read-tree", &head_tree]),
            None,
        )
        .map_err(not_applied)?;
        run(
            scratch
                .git(repository, Some(&index))
                .args(["apply", "--cached", "--binary", "--whitespace=nowarn"])
                .arg(&staged_patch),
            None,
        )
        .map_err(not_applied)?;
        text(run(
            scratch.git(repository, Some(&index)).arg("write-tree"),
            None,
        )?)?
        .trim()
        .to_owned()
    } else {
        head_tree
    };
    let mut next = manifest.clone();
    next.patch = remains.then_some(rewritten);
    next.worktree_tree = worktree_tree;
    next.build_output = BuildOutput::merged(manifest.build_output.as_ref(), plan.targets.clone());
    next.expected_format().clone_into(&mut next.format);
    next.require_format()?;
    let staged_manifest = scratch.path("manifest.json");
    write_manifest(&staged_manifest, &next)?;

    // Immediately before replacing anything, the archive must still be the one assessed.
    unchanged()?;
    let on_disk = std::fs::symlink_metadata(&patch_path)
        .map_err(|error| io_refusal("archive-unreadable", &patch_path, &error))?;
    if !on_disk.is_file() || on_disk.len() != recorded.bytes {
        return Err(changed(format!(
            "{} changed after it was assessed",
            patch_path.display()
        )));
    }
    // A crash between this replacement and the manifest rename leaves a patch the old manifest's
    // digest no longer describes: every later strip refuses it as unusable, and prune, seeing a
    // recorded patch, never finds it removable (fail-closed).
    // No recovery is attempted: the original bytes are gone, and keeping them under a second
    // name would put a file in the archive its manifest does not name.
    let replaced = if remains {
        std::fs::rename(&staged_patch, &patch_path)
    } else {
        std::fs::remove_file(&patch_path)
    };
    replaced.map_err(|error| io_refusal("archive-write-failed", &patch_path, &error))?;
    std::fs::rename(&staged_manifest, &manifest_path)
        .map_err(|error| io_refusal("archive-write-failed", &manifest_path, &error))?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const ROOT: &[u8] = b"evidence/fixture";

    /// A named change to a nested repository root.
    type Change<'a> = (&'a str, &'a dyn Fn(&Path));

    /// A tree holding, at [`ROOT`], a directory with every entry shape an image must keep.
    fn awkward_root() -> tempfile::TempDir {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().join(OsStr::from_bytes(ROOT));
        std::fs::create_dir_all(root.join(".git/objects/info")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join(".git/objects/info/alternates"), "").unwrap();
        std::fs::write(root.join("plain.txt"), "plain\n").unwrap();
        std::fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o750))
            .unwrap();
        std::fs::write(root.join("frozen"), [0, 255, 1]).unwrap();
        std::fs::set_permissions(root.join("frozen"), std::fs::Permissions::from_mode(0o444))
            .unwrap();
        std::fs::write(root.join(OsStr::from_bytes(b"latin-\xe9")), "not utf-8\n").unwrap();
        let long = "n".repeat(180);
        std::fs::write(root.join(&long), "long name\n").unwrap();
        symlink("a//b/./c/", root.join("unnormalised")).unwrap();
        symlink("t".repeat(150), root.join("long-target")).unwrap();
        symlink("/outside/the/tree", root.join("absolute")).unwrap();
        tree
    }

    fn image_of(tree: &Path, listing: &[Record]) -> (tempfile::TempDir, PathBuf) {
        let staging = tempfile::tempdir().unwrap();
        let image = staging.path().join(archive_image_file(1));
        write_image(tree, listing, &image).unwrap();
        (staging, image)
    }

    #[test]
    fn an_image_reads_back_as_exactly_the_listing_it_was_written_from() {
        let tree = awkward_root();
        let listing = list_root(tree.path(), ROOT).unwrap();
        assert_eq!(listing[0].path, ROOT);
        assert_eq!(listing[0].kind, Kind::Directory);
        assert!(listing.windows(2).all(|pair| pair[0].path < pair[1].path));
        let (_staging, image) = image_of(tree.path(), &listing);

        assert_eq!(read_image(&image).unwrap(), listing);
        let frozen = listing
            .iter()
            .find(|record| record.path.ends_with(b"/frozen"))
            .unwrap();
        assert_eq!(frozen.mode, 0o444);
        let link = listing
            .iter()
            .find(|record| record.path.ends_with(b"/unnormalised"))
            .unwrap();
        assert_eq!(
            link.sha256.as_deref(),
            Some(sha256_hex(b"a//b/./c/").as_str())
        );
        require_self_contained(ROOT, &listing).unwrap();
    }

    #[test]
    fn the_fingerprint_moves_with_any_byte_mode_or_name() {
        let tree = awkward_root();
        let root = tree.path().join(OsStr::from_bytes(ROOT));
        let original = fingerprint(&list_root(tree.path(), ROOT).unwrap());
        let changes: [&dyn Fn(); 3] = [
            &|| std::fs::write(root.join("plain.txt"), "plain!\n").unwrap(),
            &|| {
                std::fs::set_permissions(
                    root.join("plain.txt"),
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            },
            &|| std::fs::rename(root.join("plain.txt"), root.join("plain.md")).unwrap(),
        ];
        let mut seen = vec![original];
        for change in changes {
            change();
            let next = fingerprint(&list_root(tree.path(), ROOT).unwrap());
            assert!(!seen.contains(&next), "{next}");
            seen.push(next);
        }
    }

    #[test]
    fn removing_an_imaged_root_deletes_exactly_its_listing() {
        let tree = awkward_root();
        let listing = list_root(tree.path(), ROOT).unwrap();
        let (_staging, image) = image_of(tree.path(), &listing);
        let expected = read_image(&image).unwrap();

        remove_imaged_root(tree.path(), ROOT, &expected, &image).unwrap();
        assert!(!tree.path().join(OsStr::from_bytes(ROOT)).exists());
        assert!(tree.path().join("evidence").is_dir());
        assert!(image.is_file());
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn removing_an_imaged_root_opens_read_only_directories_it_has_verified() {
        let tree = awkward_root();
        let root = tree.path().join(OsStr::from_bytes(ROOT));
        std::fs::create_dir(root.join("sealed")).unwrap();
        std::fs::write(root.join("sealed/kept.txt"), "kept\n").unwrap();
        set_mode(&root.join("sealed"), 0o555);
        set_mode(&root, 0o555);
        set_mode(&tree.path().join("evidence"), 0o555);
        let listing = list_root(tree.path(), ROOT).unwrap();

        remove_imaged_root(tree.path(), ROOT, &listing, tree.path()).unwrap();
        assert!(!root.exists());
        let parent = std::fs::metadata(tree.path().join("evidence")).unwrap();
        assert_eq!(parent.mode() & 0o777, 0o755);
    }

    #[test]
    fn a_read_only_directory_whose_mode_changed_after_verification_is_stale_and_kept() {
        let tree = awkward_root();
        let root = tree.path().join(OsStr::from_bytes(ROOT));
        std::fs::create_dir(root.join("sealed")).unwrap();
        std::fs::write(root.join("sealed/kept.txt"), "kept\n").unwrap();
        let listing = list_root(tree.path(), ROOT).unwrap();
        set_mode(&root.join("sealed"), 0o555);

        let refusal = remove_imaged_root(tree.path(), ROOT, &listing, tree.path()).unwrap_err();
        assert_eq!(refusal.code, "archive-stale", "{refusal}");
        assert!(root.join("sealed/kept.txt").is_file());
        set_mode(&root.join("sealed"), 0o755);
    }

    #[test]
    fn parents_are_opened_down_to_the_entry_and_the_walk_stops_at_a_missing_or_other_component() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tree.path().join("a/b")).unwrap();
        std::fs::write(tree.path().join("a/file"), "x").unwrap();
        set_mode(&tree.path().join("a/b"), 0o555);
        set_mode(&tree.path().join("a"), 0o500);

        make_parents_owner_writable(tree.path(), b"a/b/c.txt").unwrap();
        for (path, mode) in [("a", 0o700), ("a/b", 0o755)] {
            let metadata = std::fs::metadata(tree.path().join(path)).unwrap();
            assert_eq!(metadata.mode() & 0o777, mode, "{path}");
        }
        make_parents_owner_writable(tree.path(), b"a/missing/deeper/c.txt").unwrap();
        make_parents_owner_writable(tree.path(), b"a/file/c.txt").unwrap();
    }

    #[test]
    fn removing_an_imaged_root_refuses_a_changed_or_unexpected_entry() {
        let changes: [Change<'_>; 3] = [
            ("changed", &|root| {
                std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
            }),
            ("unexpected", &|root| {
                std::fs::write(root.join("empty/late.txt"), "late\n").unwrap();
            }),
            ("missing", &|root| {
                std::fs::remove_file(root.join("plain.txt")).unwrap();
            }),
        ];
        for (name, change) in changes {
            let tree = awkward_root();
            let root = tree.path().join(OsStr::from_bytes(ROOT));
            let listing = list_root(tree.path(), ROOT).unwrap();
            let (_staging, image) = image_of(tree.path(), &listing);
            change(&root);

            let refusal = remove_imaged_root(tree.path(), ROOT, &listing, &image).unwrap_err();
            assert_eq!(refusal.code, "archive-stale", "{name}: {refusal}");
            assert!(image.is_file(), "{name}");
        }
        // The changed file itself is kept: it is observed before anything is deleted next to it.
        let tree = awkward_root();
        let root = tree.path().join(OsStr::from_bytes(ROOT));
        let listing = list_root(tree.path(), ROOT).unwrap();
        std::fs::write(root.join(".git/HEAD"), "edited\n").unwrap();
        let _ = remove_imaged_root(tree.path(), ROOT, &listing, tree.path());
        assert_eq!(std::fs::read(root.join(".git/HEAD")).unwrap(), b"edited\n");
    }

    #[test]
    fn a_repository_depending_on_state_outside_it_is_unsupported() {
        let cases: [Change<'_>; 6] = [
            ("commondir", &|root| {
                std::fs::write(root.join(".git/commondir"), "../other\n").unwrap();
            }),
            ("alternates", &|root| {
                std::fs::write(root.join(".git/objects/info/alternates"), "/elsewhere\n").unwrap();
            }),
            ("worktrees", &|root| {
                std::fs::create_dir_all(root.join(".git/worktrees/linked")).unwrap();
            }),
            ("symlink in .git", &|root| {
                symlink("/elsewhere/objects", root.join(".git/objects/pack")).unwrap();
            }),
            ("inner gitfile", &|root| {
                std::fs::create_dir_all(root.join("vendor/inner")).unwrap();
                std::fs::write(root.join("vendor/inner/.git"), "gitdir: /elsewhere\n").unwrap();
            }),
            ("root gitfile", &|root| {
                std::fs::remove_dir_all(root.join(".git")).unwrap();
                std::fs::write(root.join(".git"), "gitdir: /elsewhere\n").unwrap();
            }),
        ];
        for (name, change) in cases {
            let tree = awkward_root();
            change(&tree.path().join(OsStr::from_bytes(ROOT)));
            let listing = list_root(tree.path(), ROOT).unwrap();
            let refusal = require_self_contained(ROOT, &listing).unwrap_err();
            assert_eq!(
                refusal.code, "archive-unsupported-entry",
                "{name}: {refusal}"
            );
        }
    }

    #[test]
    fn a_fifo_or_a_newline_below_a_nested_root_is_unsupported() {
        let tree = awkward_root();
        let root = tree.path().join(OsStr::from_bytes(ROOT));
        let made = Command::new("mkfifo")
            .arg(root.join("pipe"))
            .status()
            .unwrap();
        assert!(made.success());
        let refusal = list_root(tree.path(), ROOT).unwrap_err();
        assert_eq!(refusal.code, "archive-unsupported-entry", "{refusal}");

        let tree = awkward_root();
        let root = tree.path().join(OsStr::from_bytes(ROOT));
        std::fs::write(root.join("two\nlines"), "x").unwrap();
        let refusal = list_root(tree.path(), ROOT).unwrap_err();
        assert_eq!(refusal.code, "archive-unsupported-entry", "{refusal}");
    }

    const PATCH: &[u8] =
        b"diff --git a/target/debug/.fingerprint/a b/target/debug/.fingerprint/a\n\
new file mode 100644\n\
index 0000000000000000000000000000000000000000..78981922613b2afb6025042ff6bd878ac1994e85\n\
--- /dev/null\n\
+++ b/target/debug/.fingerprint/a\n\
@@ -0,0 +1 @@\n\
+a\n\
diff --git \"a/target/debug/lib\\303\\251\" \"b/target/debug/lib\\303\\251\"\n\
new file mode 100644\n\
index 0000000000000000000000000000000000000000..e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n\
diff --git a/notes.txt b/notes.txt\n\
new file mode 100644\n\
index 0000000000000000000000000000000000000000..78981922613b2afb6025042ff6bd878ac1994e85\n\
--- /dev/null\n\
+++ b/notes.txt\n\
@@ -0,0 +1 @@\n\
+a\n";

    /// An archive directory holding `patch` as `dirty.patch`, and a manifest recording it.
    fn patched_archive(patch: &[u8]) -> (tempfile::TempDir, ArchiveManifest) {
        let archive = tempfile::tempdir().unwrap();
        std::fs::write(archive.path().join(ARCHIVE_PATCH_FILE), patch).unwrap();
        let manifest = ArchiveManifest {
            format: b10x_worktree_domain::ARCHIVE_FORMAT.into(),
            id: b10x_worktree_domain::WorktreeId::new("tree").unwrap(),
            repository_root: "/workspace/repo".into(),
            path: "/managed/repo/tree".into(),
            head: "a".repeat(40),
            branch: None,
            unique_commits: Vec::new(),
            bundle: None,
            worktree_tree: "b".repeat(40),
            patch: Some(describe(archive.path(), ARCHIVE_PATCH_FILE).unwrap()),
            created_at: 1,
            nested_repositories: Vec::new(),
            build_output: None,
        };
        (archive, manifest)
    }

    #[test]
    fn a_scanned_patch_splits_into_whole_sections_with_decoded_paths() {
        let (archive, manifest) = patched_archive(PATCH);
        let sections = scan_patch(archive.path(), &manifest).unwrap();
        let paths = sections
            .iter()
            .map(|section| section.path.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                b"target/debug/.fingerprint/a".to_vec(),
                "target/debug/lib\u{e9}".as_bytes().to_vec(),
                b"notes.txt".to_vec(),
            ]
        );
        assert!(sections.iter().all(|section| section.new_file));
        assert_eq!(
            sections
                .iter()
                .map(|section| section.patch_bytes)
                .sum::<u64>(),
            PATCH.len() as u64
        );
        assert_eq!(sections[0].content_bytes, 2);

        let rewritten = archive.path().join("rewritten");
        let kept = rewrite_patch(
            &archive.path().join(ARCHIVE_PATCH_FILE),
            &rewritten,
            &[true, true, false],
            manifest.patch.as_ref().unwrap(),
        )
        .unwrap();
        let bytes = std::fs::read(&rewritten).unwrap();
        assert!(bytes.starts_with(b"diff --git a/notes.txt b/notes.txt\n"));
        assert!(PATCH.ends_with(&bytes));
        assert_eq!(kept.bytes, bytes.len() as u64);
        assert_eq!(kept.sha256, sha256_hex(&bytes));
    }

    #[test]
    fn a_patch_with_a_preamble_a_changed_digest_or_other_sections_is_refused() {
        let mut preamble = b"From a mail\n".to_vec();
        preamble.extend_from_slice(PATCH);
        let (archive, manifest) = patched_archive(&preamble);
        let refusal = scan_patch(archive.path(), &manifest).unwrap_err();
        assert_eq!(refusal.code, "archive-patch-unusable", "{refusal}");

        let (archive, manifest) = patched_archive(PATCH);
        std::fs::write(archive.path().join(ARCHIVE_PATCH_FILE), &PATCH[1..]).unwrap();
        let refusal = scan_patch(archive.path(), &manifest).unwrap_err();
        assert_eq!(refusal.code, "archive-patch-unusable", "{refusal}");
        std::fs::write(archive.path().join(ARCHIVE_PATCH_FILE), PATCH.repeat(2)).unwrap();
        let refusal = scan_patch(archive.path(), &manifest).unwrap_err();
        assert_eq!(refusal.code, "archive-digest-mismatch", "{refusal}");

        let (archive, manifest) = patched_archive(PATCH);
        let refusal = rewrite_patch(
            &archive.path().join(ARCHIVE_PATCH_FILE),
            &archive.path().join("rewritten"),
            &[true, false],
            manifest.patch.as_ref().unwrap(),
        )
        .unwrap_err();
        assert_eq!(refusal.code, "archive-patch-unusable", "{refusal}");
    }

    /// Whether a section's path is tracked is read from HEAD's tree in the repository, whatever
    /// the patch's headers say: every section of [`PATCH`] claims `new file mode`.
    #[test]
    fn sections_are_marked_tracked_from_heads_tree_not_the_patch() {
        let repository = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(repository.path())
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=f@example.invalid",
                ])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "--quiet"]);
        // `.fingerprint` a file, so `.fingerprint/a` lies below a tracked file; `libé` a tracked
        // directory; `notes.txt` untracked.
        let root = repository.path();
        std::fs::create_dir_all(root.join("target/debug/lib\u{e9}")).unwrap();
        std::fs::write(root.join("target/debug/.fingerprint"), "file\n").unwrap();
        std::fs::write(root.join("target/debug/lib\u{e9}/inner"), "inner\n").unwrap();
        git(&["add", "-f", "."]);
        git(&["commit", "--quiet", "-m", "tracked"]);
        let head = git(&["rev-parse", "HEAD"]).trim().to_owned();

        let (archive, mut manifest) = patched_archive(PATCH);
        manifest.repository_root = root.to_path_buf();
        manifest.head = head;
        let scanned = scan_patch_against_head(archive.path(), &manifest).unwrap();
        // HEAD's paths come with the scan, for the domain's Flutter app roots.
        assert!(
            scanned
                .head_files
                .contains(&b"target/debug/.fingerprint"[..])
        );
        assert!(scanned.head_directories.contains(&b"target/debug"[..]));
        assert!(!scanned.head_files.contains(&b"notes.txt"[..]));
        let sections = scanned.sections;
        assert!(sections.iter().all(|section| section.new_file));
        assert_eq!(
            sections
                .iter()
                .map(|section| section.tracked)
                .collect::<Vec<_>>(),
            [true, true, false]
        );
    }
}
