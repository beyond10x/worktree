//! Seeding a new tree's Cargo target: which artifacts of a built checkout a new tree of the same
//! repository starts with, and what the tree's record says about it.
//!
//! The first `cargo build` in a new tree compiles every dependency again, although a built
//! checkout of the same repository holds the same units. `create` can copy them: the tag,
//! `.rustc_info.json` and, of the `debug` profile, the dependency units. The workspace's own
//! units are left out, because Cargo's hash of a path package holds the checkout's path and so
//! no other checkout can use them. Nothing here reads the disk: an adapter observes the seed and
//! copies, and this module names what is copied and decides which unit is a member's.
//!
//! A wrongly included unit costs bytes and a wrongly excluded one a recompile; neither gives a
//! wrong build, since Cargo checks every unit's fingerprint itself.

use crate::is_cargo_unit_name;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The directory of a checkout a seed is read from and written to. A target placed elsewhere
/// (`CARGO_TARGET_DIR`, `build.target-dir`) is neither read nor seeded.
pub const CARGO_SEED_TARGET: &str = "target";

/// The one profile a seed copies.
pub const CARGO_SEED_PROFILE: &str = "debug";

/// The lock files of a profile directory. While it builds, Cargo 1.99 and 1.100 hold the first
/// shared and the other two exclusively; a Cargo that writes only the first holds it
/// exclusively.
pub const CARGO_PROFILE_LOCKS: [&str; 3] =
    [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

/// Why a `create` asked to seed the new tree's target copied nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeedSkip {
    /// No candidate checkout holds a tagged `target/` with a full `debug` profile.
    NoBuiltCheckout,
    /// `rustc -vV` could not be run in the new tree.
    RustcUnavailable,
    /// The built checkout's `.rustc_info.json` is missing or unreadable. Cargo writes none when
    /// it runs with an empty `RUSTC_WRAPPER`.
    RustcInfoMissing,
    /// The built checkout's `.rustc_info.json` names another rustc than the new tree runs.
    RustcMismatch,
    /// A running Cargo holds a lock of the built checkout's profile.
    LockHeld,
    /// The built checkout's profile holds no dependency unit.
    NoDependencyUnits,
    /// The new tree already has a `target`.
    TargetExists,
    /// Git does not ignore `target/` in the new tree, so a seed would leave it with untracked
    /// files.
    TargetNotIgnored,
    /// The copy failed; what it had written was removed again.
    CopyFailed,
    /// This Git adapter cannot seed a target.
    Unsupported,
}

/// What a `create` asked to seed the new tree's target did (`create --seed-target`): the seed's
/// path with the files and bytes copied, or the reason nothing was copied. The tree's record
/// keeps it, and `create --json` carries it as `seed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSeed {
    /// The checkout whose `target/` was copied from; absent when nothing was copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<PathBuf>,
    /// File names written below the new tree's `target/`.
    pub files: u64,
    /// Bytes of the files copied; a second name of a file already copied adds none.
    pub bytes: u64,
    /// Why nothing was copied; absent when something was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SeedSkip>,
    /// The same in one line, naming the checkout it concerns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TargetSeed {
    /// A seed copied from `seed`.
    #[must_use]
    pub fn copied(seed: PathBuf, files: u64, bytes: u64) -> Self {
        Self {
            seed: Some(seed),
            files,
            bytes,
            skipped: None,
            reason: None,
        }
    }

    /// Nothing was copied, for `skip`, as `reason` says in one line.
    #[must_use]
    pub fn skipped(skip: SeedSkip, reason: impl Into<String>) -> Self {
        Self {
            seed: None,
            files: 0,
            bytes: 0,
            skipped: Some(skip),
            reason: Some(reason.into().replace(['\n', '\r'], " ")),
        }
    }

    /// The one line `create` prints about the seed.
    #[must_use]
    pub fn line(&self) -> String {
        match (&self.seed, &self.reason) {
            (Some(seed), _) => format!(
                "target seeded from {}: {} files, {} bytes",
                seed.display(),
                self.files,
                self.bytes
            ),
            (None, Some(reason)) => format!("target not seeded: {reason}"),
            (None, None) => "target not seeded".to_owned(),
        }
    }
}

/// The name of a unit directory of Cargo before 1.100, `<package>-<unit>`, split into its
/// package and its unit, the unit being 16 lowercase hexadecimal digits. `None` for any other
/// name, such as a package directory of Cargo 1.100.
#[must_use]
pub fn split_unit_directory(name: &str) -> Option<(&str, &str)> {
    let (package, unit) = name.rsplit_once('-')?;
    (!package.is_empty() && is_cargo_unit_name(unit.as_bytes())).then_some((package, unit))
}

/// The unit a file in `deps/` belongs to: the 16 lowercase hexadecimal digits that follow the
/// last `-` and end the name or stand before a `.`, as in `libone-<unit>.rlib`, `one-<unit>.d`
/// and `one-<unit>`. `None` for a name without one.
#[must_use]
pub fn deps_file_unit(name: &str) -> Option<&str> {
    name.match_indices('-')
        .filter_map(|(index, _)| {
            let rest = &name[index + 1..];
            let unit = rest.get(..16)?;
            (is_cargo_unit_name(unit.as_bytes())
                && matches!(rest.as_bytes().get(16), None | Some(b'.')))
            .then_some(unit)
        })
        .next_back()
}

/// The source files a dep-info file (`<crate>-<unit>.d`, in Makefile syntax as rustc writes it)
/// names as prerequisites of its first rule. A space inside a path is written `\ `.
#[must_use]
pub fn dep_info_sources(text: &str) -> Vec<String> {
    let Some(rule) = text
        .lines()
        .find(|line| !line.starts_with('#') && line.contains(": "))
    else {
        return Vec::new();
    };
    let prerequisites = rule.split_once(": ").map_or("", |(_, right)| right);
    let mut sources = Vec::new();
    let mut current = String::new();
    let mut characters = prerequisites.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&' ') => {
                characters.next();
                current.push(' ');
            }
            ' ' => {
                if !current.is_empty() {
                    sources.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        sources.push(current);
    }
    sources
}

/// Whether a dep-info file's text names a source by a relative path, which makes its unit one
/// of the workspace's own.
///
/// Cargo runs rustc in the workspace root, with the crate root as a relative path, exactly for
/// a package from a path source below the workspace root: a member, or a path dependency
/// there. Every other package (a registry, a Git or a directory source, a path dependency
/// outside the workspace root) is compiled from an absolute path, and rustc writes each source
/// as it was given.
#[must_use]
pub fn dep_info_names_workspace_source(text: &str) -> bool {
    dep_info_sources(text)
        .iter()
        .any(|source| Path::new(source).is_relative())
}

/// Whether a symbolic link pointing at `link` stays inside the directory that holds it: a
/// relative path without a `..`. Such a link means the same in a copy of the directory; any
/// other could lead from the copy back into the directory copied.
#[must_use]
pub fn is_contained_link(link: &Path) -> bool {
    !link.as_os_str().is_empty()
        && link.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

/// The units of a seed's profile that are the workspace's own, which a seed leaves out.
///
/// The rule reads no manifest and runs no Cargo: a unit is the workspace's own when its
/// dep-info file names a source by a relative path ([`dep_info_names_workspace_source`]), and
/// so is every other unit of a package of the same name, which covers a build script's run (it
/// has no dep-info file). It errs in three ways, none of which gives a wrong build: a
/// dependency that shares a member's package name is left out and compiled once; a path
/// dependency outside the workspace root is copied although no other checkout can use it,
/// since its hash holds its path; and a unit of the workspace without a dep-info file, in a
/// package none of whose units has one, is copied and never used.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedMembers {
    packages: BTreeSet<String>,
    units: BTreeSet<String>,
}

impl SeedMembers {
    /// Record that a unit of `package` names a source of the workspace.
    pub fn member_package(&mut self, package: &str) {
        self.packages.insert(package.to_owned());
    }

    /// Record that the unit `unit` (16 hexadecimal digits) names a source of the workspace, as a
    /// dep-info file in `deps/` shows it, where the file's name holds no package.
    pub fn member_unit(&mut self, unit: &str) {
        self.units.insert(unit.to_owned());
    }

    /// Take in the unit directories of Cargo before 1.100 (`<package>-<unit>`, the names in
    /// `.fingerprint/` and `build/`): a directory of a recorded unit names a member package, and
    /// every directory of a member package names a member unit. Call it once every dep-info file
    /// was read, with every such name.
    pub fn resolve<S: AsRef<str>>(&mut self, unit_directories: &[S]) {
        let split = || {
            unit_directories
                .iter()
                .filter_map(|name| split_unit_directory(name.as_ref()))
        };
        for (package, unit) in split() {
            if self.units.contains(unit) {
                self.packages.insert(package.to_owned());
            }
        }
        for (package, unit) in split() {
            if self.packages.contains(package) {
                self.units.insert(unit.to_owned());
            }
        }
    }

    /// Whether an entry of `.fingerprint/` or `build/` is copied: not a unit directory of a
    /// member (`<package>-<unit>`, Cargo before 1.100) and not a member's package directory
    /// (`<package>`, Cargo 1.100).
    #[must_use]
    pub fn copies_unit_store_entry(&self, name: &str) -> bool {
        match split_unit_directory(name) {
            Some((package, unit)) => !self.packages.contains(package) && !self.units.contains(unit),
            None => !self.packages.contains(name),
        }
    }

    /// Whether a file in `deps/` is copied: its name carries no member's unit.
    #[must_use]
    pub fn copies_deps_file(&self, name: &str) -> bool {
        deps_file_unit(name).is_none_or(|unit| !self.units.contains(unit))
    }
}

/// The candidate to seed from: the one whose profile last gained a unit directory. `candidates`
/// pairs each built checkout with that time; the order returned is the order in which they are
/// tried, latest first, and for equal times the order given.
///
/// Cargo creates a unit directory when it first builds a unit of a new identity (another
/// version or feature set of a dependency, another profile setting, another rustc); compiling
/// an edited member again writes into the directory it has. The time a profile last gained one
/// is therefore the time the checkout last took in a change of its dependency graph, and the
/// latest such checkout is the one most likely to hold the units of the lock file a new tree
/// starts from.
#[must_use]
pub fn seed_order<T>(mut candidates: Vec<(T, std::time::SystemTime)>) -> Vec<T> {
    candidates.sort_by_key(|(_, built)| std::cmp::Reverse(*built));
    candidates
        .into_iter()
        .map(|(candidate, _)| candidate)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const UNIT: &str = "0123456789abcdef";
    const OTHER: &str = "fedcba9876543210";
    const NONE: [&str; 0] = [];

    #[test]
    fn a_unit_directory_splits_at_its_last_hyphen() {
        assert_eq!(
            split_unit_directory("serde-core-0123456789abcdef"),
            Some(("serde-core", UNIT))
        );
        assert_eq!(split_unit_directory("serde_core"), None);
        assert_eq!(split_unit_directory("one-0123456789ABCDEF"), None);
        assert_eq!(split_unit_directory("one-0123"), None);
        assert_eq!(split_unit_directory("-0123456789abcdef"), None);
    }

    #[test]
    fn a_deps_file_names_its_unit_before_the_extension() {
        for name in [
            "libone-0123456789abcdef.rlib",
            "one-0123456789abcdef.d",
            "one-0123456789abcdef",
            "one-0123456789abcdef.one.cgu.0.rcgu.o",
            "one-fedcba9876543210-0123456789abcdef.d",
        ] {
            assert_eq!(deps_file_unit(name), Some(UNIT), "{name}");
        }
        for name in ["libone.so", "one-0123456789abcdefg", "one-0123.d", ""] {
            assert_eq!(deps_file_unit(name), None, "{name}");
        }
    }

    #[test]
    fn dep_info_sources_are_the_first_rules_prerequisites() {
        let text = "/t/debug/deps/libone-0.rmeta: src/lib.rs src/a\\ b.rs /abs/c.rs\n\n\
                    /t/debug/deps/libone-0.rlib: src/lib.rs\n\nsrc/lib.rs:\n\
                    # env-dep:OUT_DIR=/t/debug/build/one-1/out\n";
        assert_eq!(
            dep_info_sources(text),
            ["src/lib.rs", "src/a b.rs", "/abs/c.rs"]
        );
        assert_eq!(dep_info_sources("# env-dep:A=b: c\n"), NONE);
        assert_eq!(dep_info_sources(""), NONE);
    }

    #[test]
    fn a_relative_source_makes_a_unit_the_workspaces_own() {
        assert!(dep_info_names_workspace_source(
            "/t/debug/deps/libone-0.rmeta: crates/one/src/lib.rs /t/debug/build/x/out/gen.rs\n"
        ));
        // A dependency: every source absolute, also one inside a checkout (a vendored source)
        // and one its build script generated.
        assert!(!dep_info_names_workspace_source(
            "/t/debug/deps/libdep-0.rmeta: /work/one/vendor/dep/src/lib.rs \
             /registry/dep-1.0.0/src/lib.rs /t/debug/build/dep-1/out/generated.rs\n"
        ));
        assert!(!dep_info_names_workspace_source(""));
    }

    #[test]
    fn a_link_stays_inside_its_directory_only_as_a_relative_path_without_parents() {
        for link in ["libone.so.1", "sub/file", "./file"] {
            assert!(is_contained_link(Path::new(link)), "{link}");
        }
        for link in ["/abs/file", "../file", "sub/../../file", ""] {
            assert!(!is_contained_link(Path::new(link)), "{link}");
        }
    }

    #[test]
    fn members_are_left_out_by_unit_and_by_package_name() {
        let mut members = SeedMembers::default();
        // `deps/app-<UNIT>.d` named a relative source; `.fingerprint/` shows whose unit it is.
        members.member_unit(UNIT);
        let directories = [
            "app-0123456789abcdef",
            "app-fedcba9876543210",
            "dep-1111111111111111",
        ];
        members.resolve(&directories);
        assert!(!members.copies_unit_store_entry("app-0123456789abcdef"));
        // The member's build-script run has no dep-info file; its package name leaves it out.
        assert!(!members.copies_unit_store_entry("app-fedcba9876543210"));
        assert!(members.copies_unit_store_entry("dep-1111111111111111"));
        assert!(!members.copies_deps_file("libapp-0123456789abcdef.rlib"));
        assert!(!members.copies_deps_file(&format!("app-{OTHER}.d")));
        assert!(members.copies_deps_file("libdep-1111111111111111.rlib"));
        assert!(members.copies_deps_file("libdep.so"));
    }

    #[test]
    fn a_member_package_directory_of_the_unit_layout_is_left_out() {
        let mut members = SeedMembers::default();
        members.member_package("app");
        members.resolve::<&str>(&[]);
        assert!(!members.copies_unit_store_entry("app"));
        assert!(members.copies_unit_store_entry("dep"));
        assert!(members.copies_unit_store_entry("app-core"));
    }

    #[test]
    fn the_checkout_whose_profile_last_gained_a_unit_is_tried_first() {
        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        assert_eq!(
            seed_order(vec![("old", at(10)), ("new", at(30)), ("mid", at(20))]),
            ["new", "mid", "old"]
        );
        assert_eq!(
            seed_order(vec![("first", at(10)), ("second", at(10))]),
            ["first", "second"]
        );
    }

    #[test]
    fn a_seed_reads_as_one_line_and_serialises_without_empty_fields() {
        let copied = TargetSeed::copied("/work/one".into(), 3, 42);
        assert_eq!(
            copied.line(),
            "target seeded from /work/one: 3 files, 42 bytes"
        );
        assert_eq!(
            serde_json::to_string(&copied).unwrap(),
            r#"{"seed":"/work/one","files":3,"bytes":42}"#
        );
        let skipped = TargetSeed::skipped(SeedSkip::LockHeld, "one\ntwo");
        assert_eq!(skipped.line(), "target not seeded: one two");
        assert_eq!(
            serde_json::to_string(&skipped).unwrap(),
            r#"{"files":0,"bytes":0,"skipped":"lock-held","reason":"one two"}"#
        );
    }
}
