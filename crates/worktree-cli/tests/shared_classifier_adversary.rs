//! Adversarial cases for the shared build-output classifier: `archive`, `finish --archive`,
//! `discard-cache` and the removal through a format 3 archive, against a Flutter app's output.
//!
//! Each case asks whether a command deletes, or leaves out of an archive, something that is
//! neither recreated by a build nor archived.
#![cfg(unix)]

use serde_json::Value;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git_output(path: &Path, args: &[&str]) -> Output {
    Command::new("git")
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
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = git_output(path, args);
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

const TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";

/// One repository of a Flutter app at its root, with a bare remote, below one activated profile.
struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
}

impl Fixture {
    /// HEAD holds `.gitignore` (ignoring every `build/`, `.dart_tool/` and `/target/`),
    /// `lib/main.dart`, `pubspec.yaml` when `pubspec`, and `committed`, each added with `-f`.
    fn new(pubspec: bool, committed: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("app");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        write(
            &repository.join(".gitignore"),
            "build/\n.dart_tool/\n/target/\n",
        );
        if pubspec {
            write(&repository.join("pubspec.yaml"), "name: one\n");
        }
        write(&repository.join("lib/main.dart"), "void main() {}\n");
        for (path, contents) in committed {
            write(&repository.join(path), contents);
        }
        git(&repository, &["add", "-f", "."]);
        git(&repository, &["commit", "--quiet", "-m", "fixture"]);
        let remote = root.path().join("app.git");
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

    fn run(&self, args: &[&str]) -> Output {
        let outside = self.root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        Command::new(env!("CARGO_BIN_EXE_worktree"))
            .arg("--json")
            .args(args)
            .current_dir(outside)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
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

    /// A managed tree with one pushed commit.
    fn tree(&self, id: &str) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository.to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "shared classifier adversary",
        ]);
        let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        write(&tree.join("work.txt"), format!("{id}\n"));
        git(&tree, &["add", "work.txt"]);
        git(&tree, &["commit", "--quiet", "-m", id]);
        let target = format!("HEAD:refs/heads/{id}");
        git(&tree, &["push", "--quiet", "origin", &target]);
        tree
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

    fn discard(&self, tree: &Path, extra: &[&str]) -> Value {
        let mut args = vec!["discard-cache", tree.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.ok(&args)["cache"].clone()
    }
}

fn patch_of(archive: &Value) -> String {
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    String::from_utf8_lossy(&std::fs::read(directory.join("dirty.patch")).unwrap_or_default())
        .into_owned()
}

fn paths(cache: &Value, field: &str) -> Vec<String> {
    let mut paths: Vec<String> = cache[field]
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
        .collect();
    paths.sort();
    paths
}

/// A repository with commits no remote holds, below a Flutter app's `build/`: Git reports the
/// ignored `build/` as one entry and never looks inside, so `discard-cache` sees only a
/// recognised output directory. The nested repository's commits exist nowhere else, so deleting
/// it loses them; `archive` images such a repository instead.
#[test]
fn adversary_discard_cache_keeps_a_nested_repository_inside_flutter_output() {
    let fixture = Fixture::new(true, &[]);
    let tree = fixture.tree("nested");
    write(
        &tree.join("build/app/outputs/app-release.apk"),
        [0u8, 80, 75],
    );
    let nested = tree.join("build/notes");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    write(
        &nested.join("journal.md"),
        "written by hand, committed nowhere else\n",
    );
    git(&nested, &["add", "journal.md"]);
    git(&nested, &["commit", "--quiet", "-m", "only copy"]);

    let planned = fixture.discard(&tree, &["--dry-run"]);
    let discarded = paths(&planned, "discarded");
    fixture.discard(&tree, &[]);
    assert!(
        nested.join(".git").is_dir() && nested.join("journal.md").is_file(),
        "discard-cache deleted a nested repository whose commits no remote holds; it planned \
         {discarded:?}"
    );
}

/// The same nested repository, archived: it is imaged, never left out as build output, and the
/// image survives removal of the tree.
#[test]
fn adversary_archive_images_a_nested_repository_inside_flutter_output() {
    let fixture = Fixture::new(true, &[]);
    let tree = fixture.tree("imaged");
    write(
        &tree.join("build/app/outputs/app-release.apk"),
        [0u8, 80, 75],
    );
    let nested = tree.join("build/notes");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    write(&nested.join("journal.md"), "committed nowhere else\n");
    git(&nested, &["add", "journal.md"]);
    git(&nested, &["commit", "--quiet", "-m", "only copy"]);

    let finished = fixture.ok(&["finish", "--archive", "imaged"]);
    let archive = &finished["archive"];
    let images: Vec<&str> = archive["manifest"]["nested_repositories"]
        .as_array()
        .unwrap_or_else(|| panic!("{archive}"))
        .iter()
        .map(|image| image["path"].as_str().unwrap())
        .collect();
    assert_eq!(images, ["build/notes"], "{archive}");
    let applied = fixture.gc("--apply", "imaged");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!tree.exists());
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let image = archive["manifest"]["nested_repositories"][0]["image"]["file"]
        .as_str()
        .unwrap();
    assert!(directory.join(image).is_file(), "{archive}");
}

/// A cargo target named `build` (Cargo-signed, holding a profile) beside a `pubspec.yaml`.
/// Cargo's rule leaves out only its layout: `build/notes.md` is no layout, `discard-cache` keeps
/// it, and 0.16.0's `archive` kept it in `dirty.patch`. The story says cargo behaviour is
/// unchanged and that one classifier decides for both commands.
#[test]
fn adversary_archive_keeps_what_discard_cache_keeps_in_a_cargo_target_named_build() {
    let fixture = Fixture::new(true, &[("Cargo.toml", "[package]\nname = \"one\"\n")]);
    let tree = fixture.tree("cargo");
    write(&tree.join("build/CACHEDIR.TAG"), TAG);
    write(&tree.join("build/debug/.fingerprint/one-1/lib-one"), "f\n");
    write(&tree.join("build/debug/deps/libone-1.rlib"), [9u8; 16]);
    write(
        &tree.join("build/notes.md"),
        "hand-written, never committed\n",
    );

    let planned = fixture.discard(&tree, &["--dry-run"]);
    assert_eq!(
        paths(&planned, "retained_ignored"),
        ["build/notes.md"],
        "{planned}"
    );

    let archive = fixture.archive("cargo");
    let patch = patch_of(&archive);
    assert!(
        patch.contains("b/build/notes.md"),
        "discard-cache keeps build/notes.md as no build output, yet archive left it out of \
         dirty.patch: {}",
        archive["manifest"]["build_output"]
    );
}

/// The `pubspec.yaml` beside left-out output is deleted after the archive was written: the
/// recorded `build/` no longer qualifies, so removal refuses and nothing below it is deleted.
#[test]
fn adversary_gc_refuses_when_the_pubspec_beside_left_out_output_is_gone() {
    let fixture = Fixture::new(true, &[]);
    let tree = fixture.tree("gone");
    write(
        &tree.join("build/app/outputs/app-release.apk"),
        [0u8, 80, 75],
    );
    let finished = fixture.ok(&["finish", "--archive", "gone"]);
    assert!(
        finished["archive"]["manifest"]["build_output"].is_object(),
        "{finished}"
    );
    std::fs::remove_file(tree.join("pubspec.yaml")).unwrap();
    let applied = fixture.gc("--apply", "gone");
    assert!(applied["evidence"].is_null(), "{applied}");
    assert!(applied["refusal"].is_object(), "{applied}");
    assert!(tree.join("build/app/outputs/app-release.apk").is_file());
}

/// A file the index alone tracks below `build/` (`git add -f`) keeps `build/` from being a
/// Flutter app's output; the archive holds it, and removal through the archive, which resets the
/// index to HEAD first, still deletes only what the archive holds.
#[test]
fn adversary_an_index_only_file_below_build_stays_archived_through_removal() {
    let fixture = Fixture::new(true, &[]);
    let tree = fixture.tree("staged");
    write(&tree.join(".dart_tool/package_config.json"), "{}\n");
    write(&tree.join("build/staged.txt"), "staged only\n");
    write(&tree.join("build/beside.bin"), [6u8; 7]);
    git(&tree, &["add", "-f", "build/staged.txt"]);

    let finished = fixture.ok(&["finish", "--archive", "staged"]);
    let archive = &finished["archive"];
    let recorded: Vec<&str> = archive["manifest"]["build_output"]["targets"]
        .as_array()
        .unwrap_or_else(|| panic!("{archive}"))
        .iter()
        .map(|target| target["path"].as_str().unwrap())
        .collect();
    assert_eq!(recorded, [".dart_tool"], "{archive}");
    let patch = patch_of(archive);
    for kept in ["b/build/staged.txt", "b/build/beside.bin"] {
        assert!(patch.contains(kept), "{kept}: {patch}");
    }
    let applied = fixture.gc("--apply", "staged");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!tree.exists());
}

/// HEAD tracks a file below `build/` that the index no longer does (`git rm --cached`): something
/// is still tracked there, so `discard-cache` keeps the directory.
#[test]
fn adversary_discard_cache_keeps_a_build_directory_only_head_tracks_below() {
    let fixture = Fixture::new(true, &[("build/build.yaml", "targets: {}\n")]);
    let tree = fixture.tree("head");
    git(&tree, &["rm", "--cached", "--quiet", "build/build.yaml"]);
    write(&tree.join("build/out.bin"), [5u8; 9]);

    let cache = fixture.discard(&tree, &[]);
    assert_eq!(paths(&cache, "discarded"), Vec::<String>::new(), "{cache}");
    assert!(tree.join("build/build.yaml").is_file());
    assert!(tree.join("build/out.bin").is_file());
}

/// A `pubspec.yaml` that is a symbolic link is no regular file: neither `archive` nor
/// `discard-cache` recognises the `build/` beside it.
#[test]
fn adversary_a_symlinked_pubspec_recognises_nothing() {
    let fixture = Fixture::new(false, &[]);
    let tree = fixture.tree("linked");
    write(&tree.join("real.yaml"), "name: one\n");
    std::os::unix::fs::symlink("real.yaml", tree.join("pubspec.yaml")).unwrap();
    write(&tree.join("build/out.bin"), [4u8; 9]);

    let cache = fixture.discard(&tree, &["--dry-run"]);
    assert_eq!(paths(&cache, "discarded"), Vec::<String>::new(), "{cache}");
    let archive = fixture.archive("linked");
    assert!(
        archive["manifest"].get("build_output").is_none(),
        "{archive}"
    );
    assert!(patch_of(&archive).contains("b/build/out.bin"));
}

/// An app root whose name is not UTF-8 cannot be named in a manifest: its `build/` stays in the
/// archive and in the tree. `discard-cache` refuses such a tree outright, which deletes nothing.
#[test]
fn adversary_a_non_utf8_app_root_keeps_its_build_directory() {
    let fixture = Fixture::new(false, &[]);
    let tree = fixture.tree("bytes");
    let app = tree.join(OsStr::from_bytes(b"app\xff"));
    write(&app.join("pubspec.yaml"), "name: odd\n");
    write(&app.join("build/out.bin"), [3u8; 9]);

    let output = fixture.run(&["discard-cache", tree.to_str().unwrap()]);
    if output.status.success() {
        let cache: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            paths(&cache["cache"], "discarded"),
            Vec::<String>::new(),
            "{cache}"
        );
    }
    assert!(app.join("build/out.bin").is_file());
    let archive = fixture.archive("bytes");
    assert!(
        archive["manifest"].get("build_output").is_none(),
        "{archive}"
    );
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let patch = std::fs::read(directory.join("dirty.patch")).unwrap();
    let needle = b"build/out.bin";
    assert!(
        patch.windows(needle.len()).any(|window| window == needle),
        "{}",
        String::from_utf8_lossy(&patch)
    );
}
