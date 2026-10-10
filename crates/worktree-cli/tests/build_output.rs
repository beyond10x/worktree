//! Cargo build layout is left out of new archives and stripped from old ones.
//!
//! `archive` leaves the build layout of a target `discard-cache` recognises out of `dirty.patch`
//! and the fingerprint, and records it in a `worktree.archive/3` manifest; removal through such an
//! archive deletes that layout only while it is still build layout. `prune-archives
//! --strip-build-output` removes the `new file mode` sections of build layout from an archive
//! written before, after which the existing prune rule decides whether it may go.
#![cfg(unix)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";

fn git_output(path: &Path, args: &[&str], index: Option<&Path>) -> Output {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "maintenance.auto=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command.output().unwrap()
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = git_output(path, args, None);
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Name and bytes of every entry directly in `dir`, sorted.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap_or_default(),
            )
        })
        .collect();
    entries.sort();
    entries
}

/// Sum of the regular files directly in `dir`.
fn file_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| std::fs::symlink_metadata(entry.unwrap().path()).unwrap())
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.len())
        .sum()
}

/// The top-level keys of a pretty-printed manifest, in the order they were written.
fn top_level_keys(manifest: &str) -> Vec<String> {
    manifest
        .lines()
        .filter_map(|line| line.strip_prefix("  \""))
        .filter_map(|rest| rest.split_once("\":"))
        .map(|(key, _)| key.to_owned())
        .collect()
}

/// The files of a profile Cargo writes, relative to the target; the bytes each holds.
const PROFILE_FILES: [(&str, &[u8]); 6] = [
    ("debug/.fingerprint/one-1/lib-one", b"fingerprint\n"),
    ("debug/.fingerprint/one-1/dep-lib-one", &[0, 1, 2, 3]),
    ("debug/deps/libone-1.rlib", &[0, 159, 146, 150, 255, 0, 7]),
    ("debug/deps/one-1.d", b"one: src/lib.rs\n"),
    (
        "debug/build/one-2/output",
        b"cargo:rerun-if-changed=build.rs\n",
    ),
    ("debug/incremental/one-3/s-1/query-cache.bin", &[9; 300]),
];

/// One repository with a bare remote, below one activated profile.
struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
}

impl Fixture {
    /// `tracked_manifest` commits a `Cargo.toml` beside the `/target/` ignore rule.
    fn new(tracked_manifest: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("crate");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        write(&repository.join("source"), "original\n");
        write(&repository.join(".gitignore"), "/target/\n");
        if tracked_manifest {
            write(
                &repository.join("Cargo.toml"),
                "[package]\nname = \"one\"\nversion = \"0.1.0\"\n",
            );
        }
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "--quiet", "-m", "fixture"]);
        let remote = root.path().join("crate.git");
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
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
        let profile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/default.toml");
        let fixture = Self { root, repository };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        fixture
    }

    fn command(&self, args: &[&str]) -> Command {
        let outside = self.root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_worktree"));
        command
            .args(args)
            .current_dir(outside)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut json = vec!["--json"];
        json.extend_from_slice(args);
        self.command(&json).output().unwrap()
    }

    fn human(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// A managed tree with one committed change; `push` publishes it on a branch of its id.
    fn tree(&self, id: &str, push: bool) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository.to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "build output",
        ]);
        let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        write(&tree.join("work.txt"), format!("{id}\n"));
        git(&tree, &["add", "work.txt"]);
        git(&tree, &["commit", "--quiet", "-m", id]);
        if push {
            let target = format!("HEAD:refs/heads/{id}");
            git(&tree, &["push", "--quiet", "origin", &target]);
        }
        tree
    }

    /// What `cargo build` leaves in `target/`: a tag, compiler metadata, one debug profile and
    /// the test scratch directory. Returns the files and their bytes.
    fn build(tree: &Path, tagged: bool) -> (u64, u64) {
        let target = tree.join("target");
        let mut files = vec![
            (".rustc_info.json".to_owned(), b"{\"rustc\":1}\n".to_vec()),
            ("tmp/scratch/state.json".to_owned(), b"{}\n".to_vec()),
        ];
        if tagged {
            files.push(("CACHEDIR.TAG".to_owned(), TAG.as_bytes().to_vec()));
        }
        files.extend(
            PROFILE_FILES
                .iter()
                .map(|(path, bytes)| ((*path).to_owned(), bytes.to_vec())),
        );
        for (path, bytes) in &files {
            write(&target.join(path), bytes);
        }
        (
            files.len() as u64,
            files.iter().map(|(_, bytes)| bytes.len() as u64).sum(),
        )
    }

    fn archive(&self, id: &str) -> Value {
        self.ok(&["archive", id])["archive"].clone()
    }

    fn gc(&self, mode: &str, id: &str) -> Value {
        let report = self.ok(&[
            "gc",
            "--repo",
            self.repository.to_str().unwrap(),
            mode,
            "--id",
            id,
        ]);
        report["assessments"][0].clone()
    }

    /// Finish and remove the tree with `gc --apply`, as an operator would.
    fn retire(&self, id: &str, tree: &Path) {
        self.ok(&["finish", id]);
        let assessment = self.gc("--apply", id);
        assert!(assessment["evidence"].is_object(), "{assessment}");
        assert!(!tree.exists(), "{}", tree.display());
    }

    fn strip_output(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "prune-archives",
            "--strip-build-output",
            "--repo",
            self.repository.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn strip(&self, extra: &[&str]) -> Value {
        let output = self.strip_output(extra);
        assert!(output.status.success(), "{extra:?}: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The one strip assessment of `directory` in a dry-run.
    fn strip_assessment(&self, directory: &str) -> Value {
        let report = self.strip(&[]);
        let matches: Vec<&Value> = report["archives"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["archive_directory"] == directory)
            .collect();
        assert_eq!(matches.len(), 1, "{report}");
        matches[0].clone()
    }

    fn prune_verdict(&self, directory: &str) -> Value {
        let report = self.ok(&[
            "prune-archives",
            "--repo",
            self.repository.to_str().unwrap(),
        ]);
        report["archives"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["archive_directory"] == directory)
            .unwrap_or_else(|| panic!("{report}"))["verdict"]
            .clone()
    }

    /// `--strip-build-output --apply --id <directory>` is refused with `verdict` and changes no
    /// byte of the archive.
    fn strip_apply_is_refused(&self, archive: &Path, verdict: &str) {
        let before = snapshot(archive);
        let directory = archive.file_name().unwrap().to_str().unwrap();
        let output = self.strip_output(&["--apply", "--id", directory]);
        assert!(!output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["archives"][0]["verdict"], verdict, "{report}");
        assert_eq!(report["archives"][0]["applied"], false, "{report}");
        assert_eq!(snapshot(archive), before);
    }

    /// A retired tree whose format-1 archive holds a cargo build in `dirty.patch`: written while
    /// the target was not recognisable on disk (no tag, no tracked `Cargo.toml`), as every
    /// archive before this release was. `extra` adds content that is not build output.
    fn legacy_archive(&self, id: &str, push: bool, extra: &dyn Fn(&Path)) -> PathBuf {
        let tree = self.tree(id, push);
        Self::build(&tree, false);
        // Not UTF-8 safe for Git's path quoting, and a space: both must decode.
        write(
            &tree.join("target/debug/deps/libone-\u{e9}.rlib"),
            [1u8, 2, 3],
        );
        write(&tree.join("target/debug/build/with space/out.txt"), "x\n");
        extra(&tree);
        let archive = self.archive(id);
        assert_eq!(
            archive["manifest"]["format"], "worktree.archive/1",
            "{archive}"
        );
        assert!(
            archive["manifest"].get("build_output").is_none(),
            "{archive}"
        );
        let patch = std::fs::read_to_string(
            PathBuf::from(archive["path"].as_str().unwrap()).join("dirty.patch"),
        )
        .unwrap();
        assert!(patch.contains("target/debug/.fingerprint/"), "{patch}");
        self.retire(id, &tree);
        PathBuf::from(archive["path"].as_str().unwrap())
    }
}

#[test]
fn archive_leaves_cargo_layout_out_and_records_its_size() {
    let fixture = Fixture::new(true);
    let tree = fixture.tree("built", true);
    let (files, bytes) = Fixture::build(&tree, true);

    let archive = fixture.archive("built");
    let manifest = &archive["manifest"];
    assert_eq!(manifest["format"], "worktree.archive/3", "{archive}");
    assert!(manifest["patch"].is_null(), "{archive}");
    let head_tree = git(&tree, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(manifest["worktree_tree"], head_tree.as_str());
    let output = &manifest["build_output"];
    assert_eq!(output["targets"][0]["origin"], "archive", "{output}");
    assert_eq!(output["files"], files, "{output}");
    assert_eq!(output["bytes"], bytes, "{output}");
    let targets = output["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1, "{output}");
    assert_eq!(targets[0]["path"], "target");
    assert_eq!(targets[0]["files"], files);
    assert_eq!(targets[0]["bytes"], bytes);

    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(&on_disk, manifest);
    assert!(!directory.join("dirty.patch").exists());

    let text = fixture.human(&["archive", "--replace", "built"]);
    assert!(
        text.contains(&format!(
            "left out {files} file(s), {bytes} bytes of cargo build output under target"
        )),
        "{text}"
    );
}

#[test]
fn archive_keeps_non_layout_files_under_target() {
    let fixture = Fixture::new(true);
    let tree = fixture.tree("records", true);
    Fixture::build(&tree, true);
    write(
        &tree.join("target/ess-conformance/report.json"),
        "{\"passed\":12}\n",
    );
    write(&tree.join("target/notes.md"), "a session's scratch\n");

    let archive = fixture.archive("records");
    let manifest = &archive["manifest"];
    assert_eq!(manifest["format"], "worktree.archive/3", "{archive}");
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let patch = std::fs::read_to_string(directory.join("dirty.patch")).unwrap();
    assert!(
        patch.contains("b/target/ess-conformance/report.json"),
        "{patch}"
    );
    assert!(patch.contains("b/target/notes.md"), "{patch}");
    for left_out in [
        "target/debug/",
        "target/tmp/",
        "target/CACHEDIR.TAG",
        "target/.rustc_info",
    ] {
        assert!(!patch.contains(left_out), "{left_out}: {patch}");
    }
}

/// A target that counts as tagged without a valid tag may hold a `CACHEDIR.TAG` Cargo did not
/// write: only a Cargo-signed tag is build layout, as `discard-cache` keeps an unsigned one.
#[test]
fn archive_keeps_an_unsigned_cachedir_tag() {
    let fixture = Fixture::new(true);
    let tree = fixture.tree("unsigned", true);
    Fixture::build(&tree, false);
    write(&tree.join("target/CACHEDIR.TAG"), "not cargo's\n");

    let archive = fixture.archive("unsigned");
    assert_eq!(
        archive["manifest"]["format"], "worktree.archive/3",
        "{archive}"
    );
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let patch = std::fs::read_to_string(directory.join("dirty.patch")).unwrap();
    assert!(patch.contains("b/target/CACHEDIR.TAG"), "{patch}");
    assert!(!patch.contains("target/debug/"), "{patch}");
}

#[test]
fn archive_without_build_output_keeps_format_2_bytes() {
    let fixture = Fixture::new(true);
    let tree = fixture.tree("nested", true);
    // An ignored directory that is no cargo target, holding a nested repository.
    write(&tree.join(".gitignore"), "/target/\nevidence/\n");
    git(&tree, &["commit", "--quiet", "-am", "ignore evidence"]);
    let nested = tree.join("evidence/fixture");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    write(&nested.join("a.txt"), "one\n");
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "--quiet", "-m", "nested"]);
    write(
        &tree.join("evidence/debug/notes.txt"),
        "not a cargo target\n",
    );

    let archive = fixture.archive("nested");
    assert_eq!(
        archive["manifest"]["format"], "worktree.archive/2",
        "{archive}"
    );
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let manifest = std::fs::read_to_string(directory.join("manifest.json")).unwrap();
    assert_eq!(
        top_level_keys(&manifest),
        [
            "format",
            "id",
            "repository_root",
            "path",
            "head",
            "branch",
            "unique_commits",
            "bundle",
            "worktree_tree",
            "patch",
            "created_at",
            "nested_repositories",
        ],
        "{manifest}"
    );
    assert!(manifest.ends_with("}\n"), "{manifest}");
    let patch = std::fs::read_to_string(directory.join("dirty.patch")).unwrap();
    assert!(patch.contains("b/evidence/debug/notes.txt"), "{patch}");

    // Without images it stays format 1, key for key.
    let plain = fixture.tree("plain", true);
    write(&plain.join("notes.txt"), "uncommitted\n");
    let archive = fixture.archive("plain");
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let manifest = std::fs::read_to_string(directory.join("manifest.json")).unwrap();
    assert_eq!(archive["manifest"]["format"], "worktree.archive/1");
    assert_eq!(
        top_level_keys(&manifest),
        [
            "format",
            "id",
            "repository_root",
            "path",
            "head",
            "branch",
            "unique_commits",
            "bundle",
            "worktree_tree",
            "patch",
            "created_at",
        ],
        "{manifest}"
    );
}

#[test]
fn gc_removes_a_tree_whose_left_out_layout_is_still_cache() {
    let fixture = Fixture::new(true);
    // Local-only work and an uncommitted file: only the archive proves recovery.
    let tree = fixture.tree("cached", false);
    Fixture::build(&tree, true);
    write(&tree.join("notes.txt"), "never committed\n");
    let archive = fixture.archive("cached");
    assert_eq!(archive["manifest"]["format"], "worktree.archive/3");
    fixture.ok(&["finish", "cached"]);
    // A build after the archive grows the same profile: still cache.
    write(&tree.join("target/debug/deps/libtwo-4.rlib"), [4u8; 64]);

    let dry = fixture.gc("--dry-run", "cached");
    assert!(dry["refusal"].is_null(), "{dry}");
    let applied = fixture.gc("--apply", "cached");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert_eq!(
        applied["evidence"]["recovery"]["kind"], "archive",
        "{applied}"
    );
    assert!(!tree.exists(), "{}", tree.display());
}

#[test]
fn gc_refuses_when_a_left_out_path_is_no_longer_build_layout() {
    let fixture = Fixture::new(true);
    let tree = fixture.tree("changed", false);
    Fixture::build(&tree, true);
    // A second profile, recognised by its `.fingerprint/`.
    write(&tree.join("target/bench/.fingerprint/one-1/lib-one"), "f\n");
    write(&tree.join("target/bench/deps/libone-1.rlib"), [7u8; 32]);
    let archive = fixture.archive("changed");
    let targets = &archive["manifest"]["build_output"]["targets"];
    assert_eq!(targets[0]["path"], "target", "{archive}");
    fixture.ok(&["finish", "changed"]);
    // Without its `.fingerprint/`, `bench/` is no profile: its files are no longer build layout.
    std::fs::remove_dir_all(tree.join("target/bench/.fingerprint")).unwrap();

    let dry = fixture.gc("--dry-run", "changed");
    assert_eq!(dry["refusal"]["code"], "archive-stale", "{dry}");
    let applied = fixture.gc("--apply", "changed");
    assert_eq!(applied["refusal"]["code"], "archive-stale", "{applied}");
    assert!(tree.join("target/bench/deps/libone-1.rlib").is_file());
    assert!(tree.join("target/debug/deps/libone-1.rlib").is_file());
}

/// A second cargo target that appears after the archive was written is recognised, so its layout
/// is left out of the fingerprint and the fingerprint still matches; only the check that every
/// target with left-out layout is one the archive recorded refuses it.
#[test]
fn gc_refuses_layout_below_a_target_the_archive_did_not_record() {
    let fixture = Fixture::new(true);
    let tree = fixture.tree("second", false);
    Fixture::build(&tree, true);
    write(&tree.join("notes.txt"), "never committed\n");
    let archive = fixture.archive("second");
    let targets = &archive["manifest"]["build_output"]["targets"];
    assert_eq!(targets.as_array().unwrap().len(), 1, "{archive}");
    assert_eq!(targets[0]["path"], "target", "{archive}");
    fixture.ok(&["finish", "second"]);
    // A tagged, ignored target holding a profile, built after the archive.
    write(&fixture.repository.join(".git/info/exclude"), "/build/\n");
    write(&tree.join("build/CACHEDIR.TAG"), TAG);
    write(&tree.join("build/debug/.fingerprint/two-1/lib-two"), "f\n");
    write(&tree.join("build/debug/deps/libtwo-1.rlib"), [2u8; 16]);

    for mode in ["--dry-run", "--apply"] {
        let assessment = fixture.gc(mode, "second");
        assert_eq!(
            assessment["refusal"]["code"], "archive-stale",
            "{mode}: {assessment}"
        );
        let message = assessment["refusal"]["message"].as_str().unwrap();
        assert!(
            message.contains("cargo build layout below build was not left out"),
            "{mode}: {message}"
        );
    }
    for kept in [
        "build/CACHEDIR.TAG",
        "build/debug/.fingerprint/two-1/lib-two",
        "build/debug/deps/libtwo-1.rlib",
        "target/debug/deps/libone-1.rlib",
        "target/CACHEDIR.TAG",
        "notes.txt",
    ] {
        assert!(tree.join(kept).is_file(), "{kept}");
    }
}

#[test]
fn strip_dry_run_lists_bytes_and_changes_nothing() {
    let fixture = Fixture::new(false);
    let archive = fixture.legacy_archive("legacy", false, &|tree| {
        write(&tree.join("notes.txt"), "never committed\n");
    });
    let before = snapshot(&archive);
    let patch_bytes = std::fs::metadata(archive.join("dirty.patch"))
        .unwrap()
        .len();

    let item = fixture.strip_assessment("legacy");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(item["bytes"], file_bytes(&archive), "{item}");
    // The tag-less profile files, `.rustc_info.json`, `tmp/` and the two awkward names.
    assert_eq!(item["sections_stripped"], PROFILE_FILES.len() + 4, "{item}");
    assert_eq!(item["sections_kept"], 1, "{item}");
    let strip = item["strip_bytes"].as_u64().unwrap();
    let kept = item["kept_patch_bytes"].as_u64().unwrap();
    assert!(strip > 0, "{item}");
    assert_eq!(strip + kept, patch_bytes, "{item}");
    assert_eq!(item["nested_images"], 0);
    assert_eq!(item["prune_verdict_after"], "uncommitted-state", "{item}");
    assert_eq!(item["applied"], false);

    let text = fixture.human(&[
        "prune-archives",
        "--strip-build-output",
        "--repo",
        fixture.repository.to_str().unwrap(),
    ]);
    let line = text
        .lines()
        .find(|line| line.starts_with("legacy\t"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("Strippable"), "{line}");
    assert!(line.contains(&format!("strips {strip} bytes")), "{line}");
    assert_eq!(snapshot(&archive), before);
}

#[test]
fn strip_apply_rewrites_patch_and_archive_becomes_removable() {
    let fixture = Fixture::new(false);
    let archive = fixture.legacy_archive("pushed", true, &|_| {});
    let before = file_bytes(&archive);
    assert_eq!(fixture.prune_verdict("pushed"), "uncommitted-state");
    let head = {
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
        manifest["head"].as_str().unwrap().to_owned()
    };

    let dry = fixture.strip_assessment("pushed");
    assert_eq!(dry["prune_verdict_after"], "removable", "{dry}");
    assert_eq!(dry["sections_kept"], 0, "{dry}");

    let output = fixture
        .command(&[
            "prune-archives",
            "--strip-build-output",
            "--repo",
            fixture.repository.to_str().unwrap(),
            "--apply",
            "--id",
            "pushed",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let after = file_bytes(&archive);
    let freed = before - after;
    assert!(freed > 0);
    assert!(
        text.contains(&format!("stripped pushed freed {freed} bytes")),
        "{text}"
    );
    assert!(
        text.contains(&format!("freed {freed} bytes in total")),
        "{text}"
    );

    let manifest: Value =
        serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], "worktree.archive/3", "{manifest}");
    assert!(manifest["patch"].is_null(), "{manifest}");
    assert!(!archive.join("dirty.patch").exists());
    let head_tree = git(
        &fixture.repository,
        &["rev-parse", &format!("{head}^{{tree}}")],
    );
    assert_eq!(manifest["worktree_tree"], head_tree.as_str(), "{manifest}");
    let output = &manifest["build_output"];
    assert_eq!(output["targets"][0]["origin"], "strip", "{output}");
    assert_eq!(output["files"], PROFILE_FILES.len() + 4, "{output}");
    assert_eq!(output["targets"][0]["path"], "target", "{output}");
    let names: Vec<String> = snapshot(&archive)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["manifest.json"]);

    assert_eq!(fixture.prune_verdict("pushed"), "removable");
    let removed = fixture.ok(&[
        "prune-archives",
        "--repo",
        fixture.repository.to_str().unwrap(),
        "--apply",
        "--id",
        "pushed",
    ]);
    assert_eq!(removed["archives"][0]["removed"], true, "{removed}");
    assert!(!archive.exists());
}

#[test]
fn strip_keeps_tracked_file_changes_under_target() {
    let fixture = Fixture::new(false);
    let archive = fixture.legacy_archive("tracked", true, &|tree| {
        // A file Git tracks below the profile is never build layout, edited or not.
        write(&tree.join("target/debug/kept.txt"), "committed\n");
        git(tree, &["add", "-f", "target/debug/kept.txt"]);
        git(
            tree,
            &["commit", "--quiet", "-m", "track a file under target"],
        );
        let id = "HEAD:refs/heads/tracked";
        git(tree, &["push", "--quiet", "--force", "origin", id]);
        write(&tree.join("target/debug/kept.txt"), "edited\n");
        write(&tree.join("target/ess-conformance/report.json"), "{}\n");
    });

    let item = fixture.strip_assessment("tracked");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(item["sections_kept"], 2, "{item}");
    fixture.strip(&["--apply", "--id", "tracked"]);

    let manifest: Value =
        serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], "worktree.archive/3", "{manifest}");
    let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap();
    assert!(
        patch.contains("diff --git a/target/debug/kept.txt b/target/debug/kept.txt"),
        "{patch}"
    );
    assert!(patch.contains("+edited"), "{patch}");
    assert!(
        patch.contains("b/target/ess-conformance/report.json"),
        "{patch}"
    );
    assert!(!patch.contains(".fingerprint"), "{patch}");
    assert!(!patch.contains("rustc_info"), "{patch}");
    assert_eq!(
        manifest["patch"]["bytes"],
        std::fs::metadata(archive.join("dirty.patch"))
            .unwrap()
            .len()
    );

    // The rewritten patch recreates exactly the recorded fingerprint over HEAD.
    let scratch = tempfile::tempdir().unwrap();
    let index = scratch.path().join("index");
    let head = manifest["head"].as_str().unwrap();
    let repository = &fixture.repository;
    let ok = |args: &[&str]| {
        let output = git_output(repository, args, Some(&index));
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    ok(&["read-tree", head]);
    ok(&[
        "apply",
        "--cached",
        "--binary",
        archive.join("dirty.patch").to_str().unwrap(),
    ]);
    assert_eq!(
        ok(&["write-tree"]),
        manifest["worktree_tree"].as_str().unwrap()
    );
    assert_eq!(fixture.prune_verdict("tracked"), "uncommitted-state");
}

#[test]
fn strip_refuses_present_tree_and_invalid_manifest() {
    let fixture = Fixture::new(false);
    let tree = fixture.tree("present", true);
    Fixture::build(&tree, false);
    let present = PathBuf::from(fixture.archive("present")["path"].as_str().unwrap());
    assert!(present.join("dirty.patch").is_file());
    assert_eq!(
        fixture.strip_assessment("present")["verdict"],
        "tree-still-present"
    );
    fixture.strip_apply_is_refused(&present, "tree-still-present");
    assert!(tree.is_dir());

    let broken = fixture.legacy_archive("broken", true, &|_| {});
    write(
        &broken.join("manifest.json"),
        "{\"format\":\"worktree.archive/9\"}\n",
    );
    // A manifest that names no repository is outside repository scope; `--id` selects it.
    let named = fixture.strip(&["--id", "broken"]);
    assert_eq!(
        named["archives"][0]["verdict"], "invalid-manifest",
        "{named}"
    );
    assert!(
        named["archives"][0]["prune_verdict_after"].is_null(),
        "{named}"
    );
    fixture.strip_apply_is_refused(&broken, "invalid-manifest");
}

#[test]
fn strip_apply_without_id_refuses() {
    let fixture = Fixture::new(false);
    let archive = fixture.legacy_archive("unnamed", true, &|_| {});
    let before = snapshot(&archive);

    let output = fixture.strip_output(&["--apply"]);
    assert!(!output.status.success(), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false, "{error}");
    assert!(
        error["message"].as_str().unwrap().contains("--id"),
        "{error}"
    );
    assert_eq!(snapshot(&archive), before);
}

/// Apply an archive's `dirty.patch` over its HEAD in a scratch index and return `ls-files -s`
/// of the result, one `<mode> <blob> <stage>\t<path>` line each.
fn applied_listing(repository: &Path, archive: &Path) -> String {
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let index = scratch.path().join("index");
    let ok = |args: &[&str]| {
        let output = git_output(repository, args, Some(&index));
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    ok(&["read-tree", manifest["head"].as_str().unwrap()]);
    if archive.join("dirty.patch").exists() {
        ok(&[
            "apply",
            "--cached",
            "--binary",
            archive.join("dirty.patch").to_str().unwrap(),
        ]);
    }
    ok(&["ls-files", "-s"])
}

/// A tracked file under the target replaced by a symlink is a type change: Git writes it as a
/// `deleted file mode` section and a `new file mode 120000` section of the same, tracked path.
/// The contract keeps every change to a tracked file; stripping the second half restores the
/// tree without the symlink.
#[test]
fn strip_keeps_a_tracked_file_replaced_by_a_symlink_under_target() {
    let fixture = Fixture::new(false);
    let archive = fixture.legacy_archive("typechange", true, &|tree| {
        write(&tree.join("target/debug/kept.txt"), "committed\n");
        git(tree, &["add", "-f", "target/debug/kept.txt"]);
        git(
            tree,
            &["commit", "--quiet", "-m", "track a file under target"],
        );
        let id = "HEAD:refs/heads/typechange";
        git(tree, &["push", "--quiet", "--force", "origin", id]);
        std::fs::remove_file(tree.join("target/debug/kept.txt")).unwrap();
        std::os::unix::fs::symlink("../../source", tree.join("target/debug/kept.txt")).unwrap();
    });
    let before = applied_listing(&fixture.repository, &archive);
    assert!(
        before
            .lines()
            .any(|line| line.starts_with("120000 ") && line.ends_with("\ttarget/debug/kept.txt")),
        "{before}"
    );

    fixture.strip(&["--apply", "--id", "typechange"]);

    let after = applied_listing(&fixture.repository, &archive);
    assert!(
        after
            .lines()
            .any(|line| line.starts_with("120000 ") && line.ends_with("\ttarget/debug/kept.txt")),
        "the archive no longer recreates the tracked path as the symlink it was:\n{after}"
    );
}

/// A file Git tracks below the target is never build layout, but it must not stop the rest of the
/// target's layout from being left out: the contract excludes the tracked file, not the target.
#[test]
fn archive_leaves_layout_out_beside_a_tracked_file_under_target() {
    let fixture = Fixture::new(true);
    let tree = fixture.tree("beside", true);
    write(&tree.join("target/debug/kept.txt"), "committed\n");
    git(&tree, &["add", "-f", "target/debug/kept.txt"]);
    git(
        &tree,
        &["commit", "--quiet", "-m", "track a file under target"],
    );
    git(
        &tree,
        &[
            "push",
            "--quiet",
            "--force",
            "origin",
            "HEAD:refs/heads/beside",
        ],
    );
    Fixture::build(&tree, true);

    let archive = fixture.archive("beside");
    assert_eq!(
        archive["manifest"]["format"], "worktree.archive/3",
        "{archive}"
    );
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let patch = std::fs::read_to_string(directory.join("dirty.patch")).unwrap_or_default();
    assert!(!patch.contains("target/debug/deps/"), "{patch}");
}

/// Removal through such an archive deletes the layout left out beside the tracked file as cache
/// and leaves the tracked file to Git.
#[test]
fn gc_removes_a_tree_with_a_tracked_file_under_its_target() {
    let fixture = Fixture::new(true);
    let tree = fixture.tree("tracked", true);
    write(&tree.join("target/debug/kept.txt"), "committed\n");
    git(&tree, &["add", "-f", "target/debug/kept.txt"]);
    git(
        &tree,
        &["commit", "--quiet", "-m", "track a file under target"],
    );
    git(
        &tree,
        &[
            "push",
            "--quiet",
            "--force",
            "origin",
            "HEAD:refs/heads/tracked",
        ],
    );
    Fixture::build(&tree, true);
    let archive = fixture.archive("tracked");
    assert_eq!(archive["manifest"]["format"], "worktree.archive/3");
    assert!(archive["manifest"]["patch"].is_null(), "{archive}");

    fixture.retire("tracked", &tree);
}
