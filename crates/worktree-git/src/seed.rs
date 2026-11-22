//! Seed a new tree's `target/` with the dependency artifacts of a built checkout.
//!
//! [`seed_target`] picks the seed among the candidates, checks that it was built by the rustc
//! the new tree runs and that no Cargo is building in it, and copies the tag,
//! `.rustc_info.json` and the dependency units of the `debug` profile. The domain names what is
//! copied and decides which unit is the workspace's own ([`SeedMembers`]); this module reads the
//! disk and copies.
//!
//! The seed is only read. Its lock files are opened without creating one and locked, shared,
//! through the open file, which changes no byte and no time of the file. Every file in the new
//! tree is a copy made with [`std::fs::copy`], which shares the file's blocks where the file
//! system can, and never a hard link to the seed: a tree that shares a file with its seed
//! writes into the seed when Cargo rebuilds that file in place. Everything written lies below
//! the new tree's `target/`, which this module creates itself and removes again only when its
//! own copy failed.

use crate::ProcessGit;
use crate::cache::{full_cargo_profile, signed_tag};
use b10x_worktree_domain::{
    CACHEDIR_TAG_FILE, CARGO_PROFILE_DEPS, CARGO_PROFILE_LOCKS, CARGO_PROFILE_MARKER,
    CARGO_RUSTC_INFO, CARGO_SEED_PROFILE, CARGO_SEED_TARGET, CARGO_UNIT_BUILD, SeedMembers,
    SeedSkip, TargetSeed, dep_info_names_workspace_source, deps_file_unit, is_cargo_unit_name,
    is_contained_link, seed_order, split_unit_directory,
};
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs::{File, FileTimes, Metadata, TryLockError};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// The extension of the dep-info file rustc writes beside a unit's output.
const DEP_INFO_EXTENSION: &str = "d";

/// One refusal of one candidate: the reason's code and its line.
type Skip = (SeedSkip, String);

/// Copy the dependency artifacts of the newest built checkout among `candidates` into the
/// `target/` of the new tree `worktree`, or say in one line why nothing was copied.
///
/// A candidate is built when its `target/` is a real directory with a valid `CACHEDIR.TAG` and a
/// `debug` profile only Cargo leaves ([`full_cargo_profile`]). The built candidates are tried
/// newest first ([`built_at`], [`seed_order`]); the first whose `.rustc_info.json` names the
/// rustc the new tree runs, whose profile locks are free and which holds a dependency unit is
/// copied. When none passes, the line names what the newest one failed.
pub(crate) fn seed_target(candidates: &[PathBuf], worktree: &Path) -> TargetSeed {
    let destination = worktree.join(CARGO_SEED_TARGET);
    match std::fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            return TargetSeed::skipped(
                SeedSkip::TargetExists,
                format!("the new tree already holds {CARGO_SEED_TARGET}"),
            );
        }
        Err(error) => {
            return TargetSeed::skipped(
                SeedSkip::TargetExists,
                format!("{}: {error}", destination.display()),
            );
        }
    }
    let built: Vec<(&PathBuf, SystemTime)> = candidates
        .iter()
        .filter(|candidate| candidate.as_path() != worktree)
        .filter_map(|candidate| Some((candidate, built_at(candidate)?)))
        .collect();
    if built.is_empty() {
        return TargetSeed::skipped(
            SeedSkip::NoBuiltCheckout,
            format!(
                "no built checkout: none of {} checkouts of this repository holds a \
                 {CARGO_SEED_TARGET}/{CARGO_SEED_PROFILE} Cargo built",
                candidates.len()
            ),
        );
    }
    let rustc = match rustc_verbose_version(worktree) {
        Ok(rustc) => rustc,
        Err(error) => {
            return TargetSeed::skipped(
                SeedSkip::RustcUnavailable,
                format!("rustc -vV could not be run in the new tree: {error}"),
            );
        }
    };
    if !target_ignored(worktree) {
        return TargetSeed::skipped(
            SeedSkip::TargetNotIgnored,
            format!("Git does not ignore {CARGO_SEED_TARGET}/ in the new tree"),
        );
    }
    let mut first_skip = None;
    for seed in seed_order(built) {
        match seed_from(seed, &rustc, &destination) {
            Ok(copied) => return copied,
            Err(skip) => {
                let failed = skip.0 == SeedSkip::CopyFailed;
                first_skip.get_or_insert(skip);
                if failed {
                    break;
                }
            }
        }
    }
    let (skip, reason) = first_skip.expect("at least one built candidate was tried");
    TargetSeed::skipped(skip, reason)
}

/// When Cargo last created a unit directory in the candidate's `debug` profile, or `None` when
/// the candidate holds no built target: the latest modification time among `.fingerprint/`,
/// `build/` and the directories directly in `build/`, the directories Cargo creates a unit
/// directory in (`.fingerprint/<package>-<unit>/` before Cargo 1.100,
/// `build/<package>/<unit>/` from 1.100 on). Compiling a unit again writes into the directory
/// it has and moves none of these times.
fn built_at(candidate: &Path) -> Option<SystemTime> {
    let target = candidate.join(CARGO_SEED_TARGET);
    let profile = target.join(CARGO_SEED_PROFILE);
    if !real_directory(&target) || !signed_tag(&target) || !full_cargo_profile(&profile) {
        return None;
    }
    let build = profile.join(CARGO_UNIT_BUILD);
    let packages = std::fs::read_dir(&build)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path());
    [profile.join(CARGO_PROFILE_MARKER), build]
        .into_iter()
        .chain(packages)
        .filter_map(|directory| std::fs::symlink_metadata(directory).ok()?.modified().ok())
        .max()
}

/// What `rustc -vV` prints with the new tree as working directory, so that a toolchain file
/// there applies. `RUSTC` names the compiler when it is set, as it does for Cargo.
fn rustc_verbose_version(worktree: &Path) -> Result<String, String> {
    let rustc = std::env::var_os("RUSTC")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("-vV")
        .current_dir(worktree)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

/// Whether Git ignores `target/` in the new tree. A seed in a directory Git does not ignore
/// would leave the new tree with untracked files.
fn target_ignored(worktree: &Path) -> bool {
    ProcessGit::status(
        worktree,
        [
            OsStr::new("check-ignore"),
            OsStr::new("--quiet"),
            OsStr::new("--"),
            OsStr::new(&format!("{CARGO_SEED_TARGET}/")),
        ],
    )
    .is_ok()
}

/// The first line of the `rustc -vV` output `text` holds, as `rustc 1.99.1 (…)`.
fn rustc_name(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// Check one built candidate and copy from it.
fn seed_from(seed: &Path, rustc: &str, destination: &Path) -> Result<TargetSeed, Skip> {
    let target = seed.join(CARGO_SEED_TARGET);
    let profile = target.join(CARGO_SEED_PROFILE);
    same_rustc(&target.join(CARGO_RUSTC_INFO), rustc)?;
    // Held until this function returns: a Cargo started in the seed meanwhile waits for it.
    let _locks = lock_profile(&profile)?;
    let plan = Plan::of(&target).map_err(|error| {
        (
            SeedSkip::CopyFailed,
            format!("{} could not be read: {error}", target.display()),
        )
    })?;
    if plan.units == 0 {
        return Err((
            SeedSkip::NoDependencyUnits,
            format!(
                "{} holds no dependency unit in its {CARGO_SEED_PROFILE} profile",
                target.display()
            ),
        ));
    }
    // Created here and not in `Plan::copy`: when the directory appeared since `seed_target`
    // looked, it is another process's and is left as it is.
    if let Err(error) = std::fs::create_dir(destination) {
        return Err((
            SeedSkip::CopyFailed,
            format!("{} could not be created: {error}", destination.display()),
        ));
    }
    match plan.copy(&target, destination) {
        Ok((files, bytes)) => Ok(TargetSeed::copied(seed.to_path_buf(), files, bytes)),
        Err(error) => {
            // This function created `destination` itself; nothing else is ever removed.
            let removed = std::fs::remove_dir_all(destination);
            Err((
                SeedSkip::CopyFailed,
                format!(
                    "the copy from {} failed: {error}; the new tree's {CARGO_SEED_TARGET}/ {}",
                    target.display(),
                    match removed {
                        Ok(()) => "was removed again".to_owned(),
                        Err(error) => format!("could not be removed: {error}"),
                    }
                ),
            ))
        }
    }
}

/// Whether the seed's `.rustc_info.json` holds the output of `rustc -vV` the new tree's rustc
/// gives. Cargo keeps there every rustc call it caches, as
/// `{"outputs": {"<key>": {"stdout": "<output>", …}}}`; the verbose version is one of them.
fn same_rustc(info: &Path, rustc: &str) -> Result<(), Skip> {
    let text = std::fs::read(info).map_err(|error| {
        (
            SeedSkip::RustcInfoMissing,
            format!(
                "{} cannot be read, so the rustc that built it is unknown (Cargo writes \
                 none when RUSTC_WRAPPER is empty): {error}",
                info.display()
            ),
        )
    })?;
    let outputs: Vec<String> = serde_json::from_slice::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            Some(
                value
                    .get("outputs")?
                    .as_object()?
                    .values()
                    .filter_map(|output| output.get("stdout")?.as_str().map(str::to_owned))
                    .collect(),
            )
        })
        .unwrap_or_default();
    if outputs.iter().any(|output| output == rustc) {
        return Ok(());
    }
    let stored = outputs
        .iter()
        .find(|output| output.starts_with("rustc ") && output.contains("\nhost: "))
        .map_or("no rustc version", |output| rustc_name(output));
    Err((
        SeedSkip::RustcMismatch,
        format!(
            "{} names {stored}, the new tree runs {}",
            info.display(),
            rustc_name(rustc)
        ),
    ))
}

/// Take every lock of the profile that exists, shared, without waiting and without creating a
/// file. A building Cargo holds one of them exclusively (`.cargo-build-lock` and
/// `.cargo-artifact-lock` where it writes them, as Cargo 1.99 and 1.100 do, beside a shared
/// `.cargo-lock`; `.cargo-lock` itself before), so a shared lock that cannot be had means a
/// build, and refuses the candidate, as does a lock that cannot be asked for. Held shared, the
/// locks keep a Cargo from starting to build in the seed, which asks for one exclusively, and
/// let another `create` read the same seed at the same time.
fn lock_profile(profile: &Path) -> Result<Vec<File>, Skip> {
    let mut held = Vec::new();
    for name in CARGO_PROFILE_LOCKS {
        let path = profile.join(name);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err((SeedSkip::LockHeld, format!("{}: {error}", path.display())));
            }
        };
        match file.try_lock_shared() {
            Ok(()) => held.push(file),
            Err(TryLockError::WouldBlock) => {
                return Err((
                    SeedSkip::LockHeld,
                    format!("{} is locked: a Cargo is building there", path.display()),
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err((
                    SeedSkip::LockHeld,
                    format!("{} cannot be locked: {error}", path.display()),
                ));
            }
        }
    }
    Ok(held)
}

/// One name to write below the new tree's `target/`.
enum Entry {
    /// A directory, with the seed's modification time.
    Directory(SystemTime),
    /// A regular file: its metadata in the seed.
    File(Metadata),
    /// A symbolic link that stays inside its directory, and what it points at.
    Link(PathBuf),
}

/// What one seed gives: every name to write, relative to `target/`, a directory before what it
/// holds.
#[derive(Default)]
struct Plan {
    entries: Vec<(PathBuf, Entry)>,
    /// Entries of the unit stores planned: unit directories of Cargo before 1.100, package
    /// directories of Cargo 1.100.
    units: usize,
}

impl Plan {
    /// Read `target`: the tag, `.rustc_info.json` and the dependency units of the profile.
    fn of(target: &Path) -> io::Result<Self> {
        let profile = Path::new(CARGO_SEED_PROFILE);
        let fingerprints = profile.join(CARGO_PROFILE_MARKER);
        let build = profile.join(CARGO_UNIT_BUILD);
        let deps = profile.join(CARGO_PROFILE_DEPS);
        let fingerprint_names = names(&target.join(&fingerprints))?;
        let build_names = names(&target.join(&build))?;
        let deps_names = names(&target.join(&deps))?;
        let members = members(target, &fingerprint_names, &build_names, &deps_names)?;

        let mut plan = Self::default();
        for name in [CACHEDIR_TAG_FILE, CARGO_RUSTC_INFO] {
            let metadata = std::fs::symlink_metadata(target.join(name))?;
            if metadata.is_file() {
                plan.entries.push((name.into(), Entry::File(metadata)));
            }
        }
        plan.directory(target, profile)?;
        // A unit directory that holds what cannot be copied is left out of both stores, so
        // that no fingerprint arrives without the output it vouches for.
        let mut left_out = BTreeSet::new();
        for (store, store_names) in [(&build, &build_names), (&fingerprints, &fingerprint_names)] {
            if store_names.is_empty() {
                continue;
            }
            plan.directory(target, store)?;
            for name in store_names {
                if !members.copies_unit_store_entry(name) || left_out.contains(name) {
                    continue;
                }
                let relative = store.join(name);
                if !real_directory(&target.join(&relative)) {
                    continue;
                }
                let before = plan.entries.len();
                if plan.subtree(target, &relative)? {
                    plan.units += 1;
                } else {
                    plan.entries.truncate(before);
                    left_out.insert(name.clone());
                }
            }
        }
        if !deps_names.is_empty() {
            plan.directory(target, &deps)?;
            for name in &deps_names {
                if !members.copies_deps_file(name) {
                    continue;
                }
                let before = plan.entries.len();
                if !plan.entry(target, &deps.join(name))? {
                    plan.entries.truncate(before);
                }
            }
        }
        Ok(plan)
    }

    fn directory(&mut self, target: &Path, relative: &Path) -> io::Result<()> {
        let modified = std::fs::symlink_metadata(target.join(relative))?.modified()?;
        self.entries
            .push((relative.to_path_buf(), Entry::Directory(modified)));
        Ok(())
    }

    /// Plan the directory `relative` with everything below it. `false` when it holds an entry
    /// that cannot be copied; the caller then drops what was planned.
    fn subtree(&mut self, target: &Path, relative: &Path) -> io::Result<bool> {
        self.directory(target, relative)?;
        for name in names(&target.join(relative))? {
            if !self.entry(target, &relative.join(name))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Plan one entry: a regular file, a directory with what it holds, or a symbolic link that
    /// stays inside its directory. `false` for anything else (a link that leaves its directory
    /// could lead back into the seed; a socket, pipe or device is nothing Cargo wrote).
    fn entry(&mut self, target: &Path, relative: &Path) -> io::Result<bool> {
        let path = target.join(relative);
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_file() {
            self.entries
                .push((relative.to_path_buf(), Entry::File(metadata)));
            Ok(true)
        } else if metadata.is_dir() {
            self.subtree(target, relative)
        } else if metadata.is_symlink() {
            let link = std::fs::read_link(path)?;
            if !is_contained_link(&link) {
                return Ok(false);
            }
            self.entries
                .push((relative.to_path_buf(), Entry::Link(link)));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Write the plan below `destination`, which must not exist. Returns the names of files
    /// written and the bytes copied.
    ///
    /// Each file is copied with [`std::fs::copy`] and given the seed's times: Cargo compares
    /// the modification times of a unit's outputs with those its fingerprint recorded. Two
    /// names of one file in the seed (same device and inode) become two names of one file here,
    /// a hard link between the two copies. Directories get the seed's modification time once
    /// everything is written, since writing into one moves its time.
    fn copy(&self, target: &Path, destination: &Path) -> io::Result<(u64, u64)> {
        let mut copies: HashMap<(u64, u64), PathBuf> = HashMap::new();
        let (mut files, mut bytes) = (0_u64, 0_u64);
        for (relative, entry) in &self.entries {
            let to = destination.join(relative);
            match entry {
                Entry::Directory(_) => std::fs::create_dir(to)?,
                Entry::Link(link) => {
                    std::os::unix::fs::symlink(link, to)?;
                    files += 1;
                }
                Entry::File(metadata) => {
                    let identity = (metadata.dev(), metadata.ino());
                    if let Some(first) = copies.get(&identity) {
                        std::fs::hard_link(first, to)?;
                    } else {
                        copy_file(&target.join(relative), &to, metadata)?;
                        bytes += metadata.len();
                        if metadata.nlink() > 1 {
                            copies.insert(identity, to);
                        }
                    }
                    files += 1;
                }
            }
        }
        for (relative, entry) in &self.entries {
            if let Entry::Directory(modified) = entry {
                File::open(destination.join(relative))?.set_modified(*modified)?;
            }
        }
        Ok((files, bytes))
    }
}

/// Copy one regular file with the standard library's copy, which on Linux asks the kernel to
/// copy the range and so shares the blocks on a file system that can, then give the copy the
/// seed's access and modification times. The copy keeps the permission bits.
fn copy_file(from: &Path, to: &Path, metadata: &Metadata) -> io::Result<()> {
    std::fs::copy(from, to)?;
    let times = FileTimes::new()
        .set_accessed(metadata.accessed()?)
        .set_modified(metadata.modified()?);
    // Times are set through an open file; a copy without read permission opens for writing.
    File::open(to)
        .or_else(|_| File::options().write(true).open(to))?
        .set_times(times)
}

/// The workspace's own units in the seed's profile, from its dep-info files: `deps/*.d` and
/// `build/<package>-<unit>/*.d` (Cargo before 1.100), `build/<package>/<unit>/out/*.d` (1.100).
fn members(
    target: &Path,
    fingerprint_names: &[String],
    build_names: &[String],
    deps_names: &[String],
) -> io::Result<SeedMembers> {
    let profile = target.join(CARGO_SEED_PROFILE);
    let mut members = SeedMembers::default();
    let deps = profile.join(CARGO_PROFILE_DEPS);
    for name in deps_names {
        if let Some(unit) = deps_file_unit(name)
            && names_workspace_source(&deps.join(name))
        {
            members.member_unit(unit);
        }
    }
    let build = profile.join(CARGO_UNIT_BUILD);
    for name in build_names {
        let directory = build.join(name);
        if !real_directory(&directory) {
            continue;
        }
        if let Some((package, _)) = split_unit_directory(name) {
            if holds_workspace_dep_info(&directory)? {
                members.member_package(package);
            }
            continue;
        }
        for unit in names(&directory)? {
            if is_cargo_unit_name(unit.as_bytes())
                && holds_workspace_dep_info(&directory.join(unit).join("out")).unwrap_or(false)
            {
                members.member_package(name);
                break;
            }
        }
    }
    let unit_directories: Vec<&String> = fingerprint_names.iter().chain(build_names).collect();
    members.resolve(&unit_directories);
    Ok(members)
}

/// Whether a dep-info file directly in `directory` names a source of the workspace.
fn holds_workspace_dep_info(directory: &Path) -> io::Result<bool> {
    Ok(names(directory)?
        .iter()
        .any(|name| names_workspace_source(&directory.join(name))))
}

/// Whether `path` is a dep-info file (`*.d`, a regular file) that names a source of the
/// workspace. One that cannot be read names none.
fn names_workspace_source(path: &Path) -> bool {
    path.extension() == Some(OsStr::new(DEP_INFO_EXTENSION))
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && std::fs::read(path)
            .is_ok_and(|text| dep_info_names_workspace_source(&String::from_utf8_lossy(&text)))
}

/// The names in `directory`, sorted; none when it does not exist or is no real directory. A
/// name that is not UTF-8 is none Cargo wrote and is left out.
fn names(directory: &Path) -> io::Result<Vec<String>> {
    if !real_directory(directory) {
        return Ok(Vec::new());
    }
    let mut names = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.file_name().into_string().ok()))
        .filter_map(Result::transpose)
        .collect::<io::Result<Vec<String>>>()?;
    names.sort_unstable();
    Ok(names)
}

fn real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}
