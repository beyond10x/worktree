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

use crate::{AdvertisedTips, ProcessGit};
use b10x_worktree::GitPort as _;
use b10x_worktree_domain::{
    ARCHIVE_BUNDLE_FILE, ARCHIVE_HEAD_REF, ARCHIVE_MANIFEST_FILE, ARCHIVE_PATCH_FILE,
    ArchiveEvidence, ArchiveFile, ArchiveManifest, ArchiveReference, ArchiveRequest,
    ArchiveStateCheck, NestedRepositoryImage, Refusal, WorktreeRecord, archive_image_file,
};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Nested repository roots relative to the tree root, as raw bytes in path-byte order.
type NestedRoots = Vec<Vec<u8>>;

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

/// Delete one imaged nested repository bottom-up, each entry only after re-observing it
/// immediately before deletion and finding exactly its record in `expected`.
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
            for entry in std::fs::read_dir(&absolute).map_err(failed)? {
                let entry = entry.map_err(failed)?;
                let mut child = relative.to_vec();
                child.push(b'/');
                child.extend_from_slice(entry.file_name().as_bytes());
                remove(worktree, &child, expected, removed, changed)?;
            }
            if observe(worktree, relative)? != **want {
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

/// The tree id of the on-disk content outside every nested repository root, its entries, and
/// those roots.
///
/// Every file is read from disk, so neither index flags nor attributes decide what is seen.
/// A nested repository is held by its image, never by this tree. Files below any other nested
/// `.git` stay invisible here; [`crate::hidden`] refuses removal for them.
fn capture(
    worktree: &Path,
    scratch: &Scratch,
    write: bool,
) -> Result<(String, Vec<Entry>, NestedRoots), Refusal> {
    let (paths, roots) = list_files(worktree, scratch, None)?;
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
    Ok((tree, entries, roots))
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
    let (worktree_tree, patch, roots) = write_patch(worktree, head, staging.path(), &scratch)?;
    let nested_repositories = write_images(worktree, head, &roots, staging.path())?;
    let manifest = ArchiveManifest {
        format: ArchiveManifest::format_for(&nested_repositories).to_owned(),
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
    };
    let mut encoded = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| Refusal::new("archive-write-failed", error.to_string()))?;
    encoded.push(b'\n');
    let manifest_path = staging.path().join(ARCHIVE_MANIFEST_FILE);
    std::fs::write(&manifest_path, encoded)
        .map_err(|error| io_refusal("archive-write-failed", &manifest_path, &error))?;

    // The tree must still be what was archived: same HEAD, same content, the same nested
    // repositories, each re-walked to its recorded fingerprint.
    let after = ProcessGit.worktree_snapshot(repository, worktree)?;
    let (after_tree, _, after_roots) = capture(worktree, &scratch, false)?;
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

/// Fingerprint the tree's content outside every nested repository and, when it differs from HEAD,
/// write and verify the binary patch that recreates it over HEAD. Also return the nested
/// repository roots the fingerprint excludes.
fn write_patch(
    worktree: &Path,
    head: &str,
    staging: &Path,
    scratch: &Scratch,
) -> Result<(String, Option<ArchiveFile>, NestedRoots), Refusal> {
    let (tree, _, roots) = capture(worktree, scratch, true)?;
    let head_tree = ProcessGit::output(worktree, ["rev-parse", &format!("{head}^{{tree}}")])?;
    if head_tree.trim() == tree {
        return Ok((tree, None, roots));
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
    Ok((tree, Some(describe(staging, ARCHIVE_PATCH_FILE)?), roots))
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
/// outside the nested repositories, and each nested repository's image and fingerprint. Return
/// the outer fingerprint with the per-file entries it was computed from.
fn require_state(
    manifest: &ArchiveManifest,
    archive: &Path,
    worktree: &Path,
) -> Result<(String, Vec<Entry>, Scratch), Refusal> {
    if let Some(patch) = &manifest.patch {
        require_file(archive, patch, ARCHIVE_PATCH_FILE)?;
    }
    let scratch = Scratch::new(worktree, archive.parent().unwrap_or(archive))?;
    let (tree, entries, roots) = capture(worktree, &scratch, false)?;
    if tree != manifest.worktree_tree {
        return Err(stale_state(archive));
    }
    require_images(manifest, archive, worktree, &roots)?;
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
    let (others, roots) = list_files(worktree, &scratch, Some(&index))?;
    let imaged = manifest
        .nested_repositories
        .iter()
        .map(|image| image.path.as_bytes());
    if !roots.iter().map(Vec::as_slice).eq(imaged) {
        return Err(stale_state(archive));
    }
    for entry in hash_files(worktree, &scratch, others, false)? {
        if archived.get(&entry.path) != Some(&(entry.mode, entry.id.clone())) {
            return Err(changed(&entry.path));
        }
        let path = worktree.join(OsStr::from_bytes(&entry.path));
        std::fs::remove_file(&path)
            .map_err(|error| io_refusal("archive-discard-failed", &path, &error))?;
    }
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
}
