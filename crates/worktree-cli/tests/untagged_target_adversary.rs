//! Adversary cases for story:untagged-cargo-target-is-cache: an ignored `target/` without a valid
//! `CACHEDIR.TAG` counts as a tagged Cargo target only when it is named `target`, a tracked
//! `Cargo.toml` sits beside it, and a direct child is a profile holding both `.fingerprint/` and
//! `deps/` as real directories (Cargo before 1.100), or a unit directory
//! `build/<package>/<unit>/` holding both `fingerprint/` and `out/` as real directories (Cargo
//! 1.100 and later; `<unit>` is 16 lowercase hexadecimal digits). Each case below tries to make
//! that rule delete something that is not Cargo's rebuildable output, follow a symbolic link, or
//! discard a case the story retains.

use serde_json::Value;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MANIFEST: (&str, &str) = ("Cargo.toml", "[package]\nname = \"one\"\n");

/// A unit directory's name as Cargo 1.100 writes one: 16 lowercase hexadecimal digits.
const UNIT: &str = "0123456789abcdef";

/// Whether `profile` holds a unit directory `build/<package>/<unit>/` with `fingerprint/` as a
/// real directory, `<unit>` being 16 lowercase hexadecimal digits: what Cargo 1.100 writes.
fn holds_unit_fingerprint(profile: &Path) -> bool {
    let real =
        |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
    let names = |directory: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default()
    };
    let build = profile.join("build");
    real(&build)
        && names(&build).iter().any(|package| {
            real(package)
                && names(package).iter().any(|unit| {
                    let name = unit.file_name().unwrap().to_str().unwrap_or("");
                    name.len() == 16
                        && name
                            .bytes()
                            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                        && real(unit)
                        && real(&unit.join("fingerprint"))
                })
        })
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            // No detached `git maintenance run --auto` left inside a nested repository.
            "-c",
            "maintenance.auto=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    tree: PathBuf,
}

impl Fixture {
    /// A managed tree whose commit holds `.gitignore` = `gitignore` and also tracks `files`, each
    /// force-added past `.gitignore`.
    fn new(gitignore: &str, files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        write(&repository.join(".gitignore"), gitignore);
        git(&repository, &["add", "."]);
        for (path, contents) in files {
            write(&repository.join(path), contents);
            git(&repository, &["add", "--force", "--", path]);
        }
        git(&repository, &["commit", "-m", "fixture"]);
        let remote = root.path().join("remote.git");
        git(
            root.path(),
            &[
                "clone",
                "--bare",
                repository.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repository, &["fetch", "--quiet", "origin"]);
        let profile = root.path().join("profile.toml");
        write(
            &profile,
            "version = 1\nname = 'test'\nexpire_after_seconds = 604800\nprotect_workspace_root = false\n",
        );
        let mut fixture = Self {
            root,
            repository,
            tree: PathBuf::new(),
        };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        let created = fixture.ok(&[
            "create",
            "--repo",
            fixture.repository.to_str().unwrap(),
            "--id",
            "cache",
            "--purpose",
            "untagged target adversary",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        fixture
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_worktree"))
            .arg("--json")
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn discard(&self, extra: &[&str]) -> Value {
        let mut args = vec!["discard-cache", self.tree.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.ok(&args)["cache"].clone()
    }

    /// A full Cargo profile: `.fingerprint/` and `deps/`, both real directories.
    fn profile(&self, relative: &str) {
        write(
            &self.tree.join(relative).join(".fingerprint/one/dep-lib"),
            "fingerprint",
        );
        write(
            &self.tree.join(relative).join("deps/libone.rlib"),
            &"x".repeat(8192),
        );
    }

    /// One unit of the profile `profile` as Cargo 1.100 lays it out: `build/one/<name>/` holding
    /// `fingerprint/` and `out/`, both real directories with one file each. Returns the unit
    /// directory.
    fn unit(&self, profile: &str, name: &str) -> PathBuf {
        let unit = self.tree.join(profile).join("build/one").join(name);
        write(&unit.join("fingerprint/lib-one"), "fingerprint");
        write(&unit.join("out/libone.rlib"), &"x".repeat(8192));
        unit
    }

    /// A directory holding `.fingerprint/` and no `deps/`.
    fn fingerprint_only(&self, relative: &str) {
        write(
            &self.tree.join(relative).join(".fingerprint/one/dep-lib"),
            "fingerprint",
        );
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.tree.join(relative))
            .unwrap_or_else(|error| panic!("{relative} was deleted: {error}"))
    }
}

fn paths(cache: &Value, field: &str) -> Vec<String> {
    cache[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            entry
                .get("path")
                .unwrap_or(entry)
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn kinds(cache: &Value) -> Vec<String> {
    cache["discarded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// `(path, kind)` of every discarded entry.
fn discarded(cache: &Value) -> Vec<(String, String)> {
    paths(cache, "discarded")
        .into_iter()
        .zip(kinds(cache))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(path, kind)| ((*path).to_owned(), (*kind).to_owned()))
        .collect()
}

/// `tmp/` is Cargo's `CARGO_TARGET_TMPDIR`; `CARGO_TARGET_TMP` documents that "no profile is ever
/// called this", and the classifier never treats it as one. So a `tmp/` holding `.fingerprint/`
/// and `deps/` is not "a profile holding both", and the only profile here holds `.fingerprint/`
/// alone: the target is no Cargo target, and goes whole as `ignored-output`.
#[test]
fn a_tmp_holding_both_markers_does_not_qualify_an_untagged_target() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    fixture.profile("target/tmp");
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");
    fixture.fingerprint_only("target/debug");
    write(&fixture.tree.join("target/debug/notes.md"), "not kept\n");
    write(&fixture.tree.join("target/.rustc_info.json"), "{}");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[("target", "ignored-output")]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(!fixture.tree.join("target").exists());
}

/// The same loophole through the other call site: Git reports `target/release/` and
/// `target/tmp/` on their own (a tracked file inside `target/`), and the parent counts only
/// because `tmp/` holds both markers. Neither is named as Cargo's.
#[test]
fn a_tmp_holding_both_markers_does_not_qualify_the_parent_of_a_profile_git_reports() {
    let fixture = Fixture::new("/target\n", &[MANIFEST, ("target/README.md", "kept\n")]);
    fixture.profile("target/tmp");
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");
    fixture.fingerprint_only("target/release");
    write(&fixture.tree.join("target/release/notes.md"), "not kept\n");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("target/release", "ignored-output"),
            ("target/tmp", "ignored-output")
        ]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert_eq!(fixture.read("target/README.md"), "kept\n");
}

/// Before this unit the classifier passed `tagged = true` only for a target whose tag was valid,
/// so the `CACHEDIR.TAG` it treated as Cargo's root metadata was always Cargo's. An untagged
/// target now gets `tagged = true` too, and a `CACHEDIR.TAG` with a bad signature is not a file
/// Cargo writes: the target does not go whole as `cargo-target`, and the file goes on its own.
#[test]
fn a_bad_signature_tag_in_an_untagged_target_is_not_cargo_metadata() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    fixture.profile("target/debug");
    write(
        &fixture.tree.join("target/CACHEDIR.TAG"),
        "Signature: not the cache directory tag\nwritten by hand\n",
    );

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("target/CACHEDIR.TAG", "ignored-output"),
            ("target/debug", "cargo-profile")
        ]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(!fixture.tree.join("target/debug").exists());
}

/// Every target the unit's tests build sits at the tree root beside the root manifest, so a rule
/// that consulted the root `Cargo.toml` for every target would pass them all. Below the root
/// only the manifest in the target's own parent counts.
#[test]
fn an_untagged_target_below_the_root_counts_only_beside_its_own_manifest() {
    let fixture = Fixture::new(
        "target/\n",
        &[
            MANIFEST,
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
        ],
    );
    // Beside an untracked manifest, with the root manifest tracked.
    write(
        &fixture.tree.join("crates/b/Cargo.toml"),
        "[package]\nname = \"b\"\n",
    );
    fixture.profile("crates/a/target/debug");
    fixture.profile("crates/b/target/debug");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("crates/a/target", "cargo-target"),
            ("crates/b/target", "ignored-output")
        ]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(fixture.tree.join("crates/b/Cargo.toml").is_file());
}

#[test]
fn case_variants_of_the_target_name_are_not_cargo_targets() {
    let fixture = Fixture::new("/Target\n/TARGET\n", &[MANIFEST]);
    for name in ["Target", "TARGET"] {
        fixture.profile(&format!("{name}/debug"));
        write(&fixture.tree.join(name).join("tmp/output.log"), "scratch\n");
    }

    let applied = fixture.discard(&[]);
    let mut gone = discarded(&applied);
    gone.sort();
    assert_eq!(
        gone,
        pairs(&[("TARGET", "ignored-output"), ("Target", "ignored-output")]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
}

#[test]
fn markers_that_are_regular_files_do_not_qualify_an_untagged_target() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    write(&fixture.tree.join("target/debug/.fingerprint"), "a file\n");
    write(&fixture.tree.join("target/debug/deps/libone.rlib"), "rlib");
    write(
        &fixture.tree.join("target/release/.fingerprint/one/dep-lib"),
        "fingerprint",
    );
    write(&fixture.tree.join("target/release/deps"), "a file\n");
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[("target", "ignored-output")]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
}

/// Once a real full profile qualifies the target, a sibling profile whose `deps` is a symlink
/// out of the tree, a symlink inside `tmp/` and one inside the profile go with the target; what
/// they point at must not.
#[test]
fn symlinks_inside_a_discarded_untagged_target_are_not_followed() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    let outside = fixture.root.path().join("outside");
    write(&outside.join("deps/keep.rlib"), "outside deps\n");
    write(&outside.join("scratch/keep.log"), "outside scratch\n");
    fixture.profile("target/debug");
    symlink(outside.join("deps"), fixture.tree.join("target/debug/link")).unwrap();
    write(
        &fixture.tree.join("target/bench/.fingerprint/one/dep-lib"),
        "fingerprint",
    );
    symlink(outside.join("deps"), fixture.tree.join("target/bench/deps")).unwrap();
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");
    symlink(
        outside.join("scratch"),
        fixture.tree.join("target/tmp/link"),
    )
    .unwrap();

    let applied = fixture.discard(&[]);
    assert_eq!(paths(&applied, "discarded"), ["target"], "{applied}");
    assert_eq!(kinds(&applied), ["cargo-target"]);
    assert!(!fixture.tree.join("target").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("deps/keep.rlib")).unwrap(),
        "outside deps\n"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("scratch/keep.log")).unwrap(),
        "outside scratch\n"
    );
}

/// Git reports a nested repository inside an ignored `target/` as `!! target/`. A `.git` directly
/// in `target/` makes the target that repository's root: its `.git`, its committed files and the
/// profile beside them are all the repository's working tree, and the whole target is retained.
#[test]
fn an_untagged_target_that_is_a_nested_repository_keeps_its_repository() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    let nested = fixture.tree.join("target");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-b", "main"]);
    write(&nested.join("notes.md"), "nested record\n");
    git(&nested, &["add", "notes.md"]);
    git(&nested, &["commit", "-m", "record"]);
    let head = git(&nested, &["rev-parse", "HEAD"]);
    fixture.profile("target/debug");

    let applied = fixture.discard(&[]);
    assert!(paths(&applied, "discarded").is_empty(), "{applied}");
    assert_eq!(paths(&applied, "retained_ignored"), ["target"], "{applied}");
    assert_eq!(git(&nested, &["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.read("target/notes.md"), "nested record\n");
    assert!(fixture.tree.join("target/debug/deps/libone.rlib").exists());
}

/// A file force-added inside an untagged target's profile makes Git descend into `target/`; the
/// tracked file must survive whatever Git reports instead.
#[test]
fn a_tracked_file_inside_an_untagged_target_profile_survives() {
    let fixture = Fixture::new("/target\n", &[MANIFEST, ("target/debug/keep.md", "kept\n")]);
    fixture.profile("target/debug");
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");
    write(&fixture.tree.join("target/.rustc_info.json"), "{}");

    let applied = fixture.discard(&[]);
    for discarded in paths(&applied, "discarded") {
        assert!(
            !Path::new("target/debug/keep.md").starts_with(&discarded),
            "{discarded} holds a tracked file: {applied}"
        );
    }
    assert_eq!(fixture.read("target/debug/keep.md"), "kept\n");
}

/// Cargo 1.100 writes no `.fingerprint/` and no `deps/` into a profile: each unit lies in
/// `build/<package>/<unit>/` and holds `fingerprint/` and `out/`. A `target/` made before such a
/// build has no tag and counts as tagged through that unit directory, with its `tmp/`.
#[test]
fn an_untagged_target_holding_a_unit_profile_counts_as_tagged() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    fixture.unit("target/debug", UNIT);
    write(&fixture.tree.join("target/debug/.cargo-lock"), "");
    write(
        &fixture.tree.join("target/debug/one.d"),
        "one: src/lib.rs\n",
    );
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(
        discarded(&planned),
        pairs(&[("target", "cargo-target")]),
        "{planned}"
    );
    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[("target", "cargo-target")]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(!fixture.tree.join("target").exists());
}

/// The same rule below the root and beside other content: the profile goes as `cargo-profile`,
/// the scratch as `cargo-target-tmp`, and a file that is no layout as `ignored-output`.
#[test]
fn an_untagged_target_holding_a_unit_profile_falls_to_its_parts_beside_other_content() {
    let fixture = Fixture::new(
        "target/\n",
        &[
            MANIFEST,
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
        ],
    );
    fixture.unit("crates/a/target/release", UNIT);
    write(
        &fixture.tree.join("crates/a/target/tmp/output.log"),
        "scratch\n",
    );
    write(&fixture.tree.join("crates/a/target/notes.md"), "not kept\n");

    let applied = fixture.discard(&[]);
    let mut gone = discarded(&applied);
    gone.sort();
    assert_eq!(
        gone,
        pairs(&[
            ("crates/a/target/notes.md", "ignored-output"),
            ("crates/a/target/release", "cargo-profile"),
            ("crates/a/target/tmp", "cargo-target-tmp"),
        ]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
}

/// The cases of the test below, in path order: `control` is a unit profile, every other differs
/// from one in the point its name gives.
const CASES: [&str; 14] = [
    "build-link",
    "control",
    "fingerprint-file",
    "fingerprint-link",
    "hex-15",
    "hex-17",
    "hex-other",
    "hex-upper",
    "out-file",
    "out-link",
    "out-missing",
    "package-link",
    "tmp-only",
    "unit-link",
];

/// What the unit rule must not take for Cargo's: each `crates/<case>/target` sits beside its own
/// tracked `Cargo.toml` and holds one profile that differs from a unit profile in one point.
/// None counts as tagged; `control`, which differs in nothing, does. Nothing a symbolic link
/// points at is read as layout or deleted.
#[test]
fn a_unit_layout_that_is_not_cargos_does_not_qualify_an_untagged_target() {
    let manifests: Vec<(String, String)> = CASES
        .iter()
        .map(|case| {
            (
                format!("crates/{case}/Cargo.toml"),
                format!("[package]\nname = \"{case}\"\n"),
            )
        })
        .collect();
    let mut tracked = vec![MANIFEST];
    tracked.extend(
        manifests
            .iter()
            .map(|(path, contents)| (path.as_str(), contents.as_str())),
    );
    let fixture = Fixture::new("target/\n", &tracked);
    // A complete unit layout outside the tree, for the links to point at.
    let outside = fixture.root.path().join("outside");
    let outside_unit = outside.join("build/one").join(UNIT);
    write(&outside_unit.join("fingerprint/lib-one"), "outside\n");
    write(&outside_unit.join("out/libone.rlib"), "outside\n");

    let profile = |case: &str| format!("crates/{case}/target/debug");
    let replace = |path: &Path, with: &dyn Fn(&Path)| {
        std::fs::remove_dir_all(path).unwrap();
        with(path);
    };
    fixture.unit(&profile("control"), UNIT);

    let unit = fixture.unit(&profile("fingerprint-file"), UNIT);
    replace(&unit.join("fingerprint"), &|path| write(path, "a file\n"));
    let unit = fixture.unit(&profile("fingerprint-link"), UNIT);
    replace(&unit.join("fingerprint"), &|path| {
        symlink(outside_unit.join("fingerprint"), path).unwrap();
    });
    let unit = fixture.unit(&profile("out-file"), UNIT);
    replace(&unit.join("out"), &|path| write(path, "a file\n"));
    let unit = fixture.unit(&profile("out-link"), UNIT);
    replace(&unit.join("out"), &|path| {
        symlink(outside_unit.join("out"), path).unwrap();
    });
    // No `out/`, and the `deps/` of the older layout beside the unit: neither rule is met, and
    // half of each is not one.
    let unit = fixture.unit(&profile("out-missing"), UNIT);
    std::fs::remove_dir_all(unit.join("out")).unwrap();
    write(
        &fixture
            .tree
            .join(profile("out-missing"))
            .join("deps/libone.rlib"),
        "rlib",
    );

    let linked = fixture.tree.join(profile("build-link"));
    std::fs::create_dir_all(&linked).unwrap();
    symlink(outside.join("build"), linked.join("build")).unwrap();
    let linked = fixture.tree.join(profile("package-link")).join("build");
    std::fs::create_dir_all(&linked).unwrap();
    symlink(outside.join("build/one"), linked.join("one")).unwrap();
    let linked = fixture.tree.join(profile("unit-link")).join("build/one");
    std::fs::create_dir_all(&linked).unwrap();
    symlink(&outside_unit, linked.join(UNIT)).unwrap();

    fixture.unit(&profile("hex-15"), &UNIT[..15]);
    fixture.unit(&profile("hex-17"), &format!("{UNIT}0"));
    fixture.unit(&profile("hex-other"), "0123456789abcdeg");
    fixture.unit(&profile("hex-upper"), "0123456789ABCDEF");
    // `tmp/` is Cargo's test scratch, never a profile, whatever it holds.
    fixture.unit("crates/tmp-only/target/tmp", UNIT);

    let applied = fixture.discard(&[]);
    let mut gone = discarded(&applied);
    gone.sort();
    let expected: Vec<(String, String)> = CASES
        .iter()
        .map(|case| {
            let kind = if *case == "control" {
                "cargo-target"
            } else {
                "ignored-output"
            };
            (format!("crates/{case}/target"), kind.to_owned())
        })
        .collect();
    assert_eq!(gone, expected, "{applied}");
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert_eq!(
        std::fs::read_to_string(outside_unit.join("fingerprint/lib-one")).unwrap(),
        "outside\n"
    );
    assert_eq!(
        std::fs::read_to_string(outside_unit.join("out/libone.rlib")).unwrap(),
        "outside\n"
    );
}

/// The story's premise, run against real Cargo: a `target/` made before Cargo's first build has
/// no `CACHEDIR.TAG`, and its profile is recognised while the record the keep file keeps beside
/// it stays.
#[test]
fn a_target_real_cargo_built_after_mkdir_is_recognised() {
    let manifest =
        "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";
    let fixture = Fixture::new(
        "/target\n",
        &[
            ("Cargo.toml", manifest),
            ("src/lib.rs", "pub fn one() {}\n"),
            (".worktreekeep", "/target/records/\n"),
        ],
    );
    write(&fixture.tree.join("target/records/notes.md"), "a record\n");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let built = Command::new(cargo)
        .args(["build", "--offline", "--quiet"])
        .current_dir(&fixture.tree)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_BUILD_BUILD_DIR")
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(!fixture.tree.join("target/CACHEDIR.TAG").exists());
    // The pinned Cargo (`rust-toolchain.toml`) writes a unit directory per compile unit, and no
    // `.fingerprint/` into the profile.
    assert!(holds_unit_fingerprint(&fixture.tree.join("target/debug")));

    let applied = fixture.discard(&[]);
    assert!(
        discarded(&applied).contains(&("target/debug".to_owned(), "cargo-profile".to_owned())),
        "{applied}"
    );
    assert!(
        paths(&applied, "discarded")
            .iter()
            .all(|path| !path.starts_with("target/records")),
        "{applied}"
    );
    assert_eq!(
        paths(&applied, "retained_ignored"),
        ["target/records/notes.md"],
        "{applied}"
    );
    assert_eq!(fixture.read("target/records/notes.md"), "a record\n");
    assert!(!fixture.tree.join("target/debug").exists());
}

/// The other half, run against the same Cargo: a `target/` Cargo creates itself carries a valid
/// `CACHEDIR.TAG`, and what the pinned Cargo lays out below it is reported as Cargo's, whole as
/// `cargo-target` or by its profile as `cargo-profile`.
#[test]
fn a_target_real_cargo_created_is_reported_as_cargo_cache() {
    let manifest =
        "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";
    let fixture = Fixture::new(
        "/target\n",
        &[
            ("Cargo.toml", manifest),
            ("src/lib.rs", "pub fn one() {}\n"),
        ],
    );
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let built = Command::new(cargo)
        .args(["build", "--offline", "--quiet"])
        .current_dir(&fixture.tree)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_BUILD_BUILD_DIR")
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let tag = std::fs::read(fixture.tree.join("target/CACHEDIR.TAG")).unwrap();
    assert!(
        tag.starts_with(b"Signature: 8a477f597d28d172789f06886806bc55"),
        "{}",
        String::from_utf8_lossy(&tag)
    );
    assert!(holds_unit_fingerprint(&fixture.tree.join("target/debug")));

    let applied = fixture.discard(&[]);
    let gone = discarded(&applied);
    assert!(
        gone.contains(&("target".to_owned(), "cargo-target".to_owned()))
            || gone.contains(&("target/debug".to_owned(), "cargo-profile".to_owned())),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(!fixture.tree.join("target/debug").exists());
}
