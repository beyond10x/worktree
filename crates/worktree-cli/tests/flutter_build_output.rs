//! A Flutter app's output is build output for `archive`, `finish --archive` and `discard-cache`.
//!
//! One classifier decides it for them and for `prune-archives --strip-build-output`: `P/build/`
//! and `P/.dart_tool/` beside a `pubspec.yaml` that is a regular file, where the directory is real
//! and neither HEAD nor the index tracks anything at or below it. `archive` leaves the untracked
//! files below one out of `dirty.patch` and the fingerprint and records the directory in
//! `build_output`; `discard-cache` deletes an ignored one as `flutter-output`.
#![cfg(unix)]

use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn git_in(path: &Path, args: &[&str], index: Option<&Path>) -> String {
    let output = git_output(path, args, index);
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}

fn git(path: &Path, args: &[&str]) -> String {
    git_in(path, args, None)
}

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// SHA-256 through the system tool, so the test does not share the implementation under test.
fn sha256_hex(bytes: &[u8]) -> String {
    use std::io::Write as _;
    let mut child = Command::new("sha256sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

const TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";

/// What `flutter build` and `flutter test` leave in an app root `P`, relative to it.
const FLUTTER_FILES: [(&str, &[u8]); 4] = [
    (
        "build/app/outputs/flutter-apk/app-release.apk",
        &[0, 80, 75, 3, 4, 9],
    ),
    ("build/unit_test_assets/AssetManifest.bin", &[7; 40]),
    (".dart_tool/package_config.json", b"{\"configVersion\":2}\n"),
    (".dart_tool/flutter_build/one/app.dill", &[1, 2, 3, 4, 5]),
];

/// One repository of a Flutter app at its root, with a bare remote, below one activated profile.
struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
}

impl Fixture {
    /// HEAD holds `.gitignore` (ignoring every `build/`, `.dart_tool/` and `/target/`), a
    /// `pubspec.yaml`, `lib/main.dart` and `committed`, each added with `-f`.
    fn new(committed: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("app");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        write(
            &repository.join(".gitignore"),
            "build/\n.dart_tool/\n/target/\n",
        );
        write(&repository.join("pubspec.yaml"), "name: one\n");
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

    /// The refusal a failing command reports on standard error.
    fn refused(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(!output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice(&output.stderr).unwrap()
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
            "flutter output",
        ]);
        let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        write(&tree.join("work.txt"), format!("{id}\n"));
        git(&tree, &["add", "work.txt"]);
        git(&tree, &["commit", "--quiet", "-m", id]);
        let target = format!("HEAD:refs/heads/{id}");
        git(&tree, &["push", "--quiet", "origin", &target]);
        tree
    }

    /// Write a Flutter app's output below `app` (relative to the tree, `""` for its root).
    /// Returns the files and bytes per output directory, `build` then `.dart_tool`.
    fn flutter_build(tree: &Path, app: &str) -> [(u64, u64); 2] {
        let mut counts = [(0, 0); 2];
        for (path, bytes) in FLUTTER_FILES {
            write(&tree.join(app).join(path), bytes);
            let slot = usize::from(path.starts_with(".dart_tool"));
            counts[slot].0 += 1;
            counts[slot].1 += bytes.len() as u64;
        }
        counts
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
    std::fs::read_to_string(directory.join("dirty.patch")).unwrap_or_default()
}

/// `(path, files, bytes)` of every recorded target, with its origin checked.
fn targets(archive: &Value) -> Vec<(String, u64, u64)> {
    archive["manifest"]["build_output"]["targets"]
        .as_array()
        .unwrap_or_else(|| panic!("{archive}"))
        .iter()
        .map(|target| {
            assert_eq!(target["origin"], "archive", "{target}");
            (
                target["path"].as_str().unwrap().to_owned(),
                target["files"].as_u64().unwrap(),
                target["bytes"].as_u64().unwrap(),
            )
        })
        .collect()
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

#[test]
fn archive_leaves_flutter_output_out_beside_a_pubspec() {
    let fixture = Fixture::new(&[("app/pubspec.yaml", "name: nested\n")]);
    let tree = fixture.tree("flutter");
    let [build, dart_tool] = Fixture::flutter_build(&tree, "");
    let [nested_build, _] = Fixture::flutter_build(&tree, "app");
    std::fs::remove_dir_all(tree.join("app/.dart_tool")).unwrap();
    // A package whose `pubspec.yaml` is untracked: a regular file on disk is enough.
    write(&tree.join("pkg/pubspec.yaml"), "name: pkg\n");
    write(&tree.join("pkg/build/out.bin"), [3u8; 12]);
    write(&tree.join("notes.txt"), "never committed\n");

    let archive = fixture.archive("flutter");
    let manifest = &archive["manifest"];
    assert_eq!(manifest["format"], "worktree.archive/3", "{archive}");
    assert_eq!(
        targets(&archive),
        [
            (".dart_tool".to_owned(), dart_tool.0, dart_tool.1),
            ("app/build".to_owned(), nested_build.0, nested_build.1),
            ("build".to_owned(), build.0, build.1),
            ("pkg/build".to_owned(), 1, 12),
        ]
    );
    let files = build.0 + dart_tool.0 + nested_build.0 + 1;
    assert_eq!(manifest["build_output"]["files"], files, "{archive}");

    let patch = patch_of(&archive);
    for kept in ["b/notes.txt", "b/pkg/pubspec.yaml"] {
        assert!(patch.contains(kept), "{kept}: {patch}");
    }
    for left_out in ["build/", ".dart_tool/"] {
        assert!(!patch.contains(left_out), "{left_out}: {patch}");
    }
    // The fingerprint is HEAD plus exactly the two files kept.
    let index = fixture.root.path().join("expected-index");
    git_in(&tree, &["read-tree", "HEAD"], Some(&index));
    git_in(
        &tree,
        &["add", "-f", "notes.txt", "pkg/pubspec.yaml"],
        Some(&index),
    );
    let expected = git_in(&tree, &["write-tree"], Some(&index));
    assert_eq!(manifest["worktree_tree"], expected.as_str(), "{archive}");
}

#[test]
fn archive_keeps_a_build_directory_with_no_pubspec_beside() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("plain");
    // No `pubspec.yaml` beside either; one beside `tool/build` is a directory.
    write(&tree.join("other/build/out.bin"), [1u8; 9]);
    write(&tree.join("other/.dart_tool/version"), "3\n");
    write(&tree.join("tool/pubspec.yaml/readme"), "a directory\n");
    write(&tree.join("tool/build/out.bin"), [2u8; 9]);
    // A symbolic link named `build` beside a pubspec is no real directory.
    write(&tree.join("linked/pubspec.yaml"), "name: linked\n");
    write(&tree.join("linked/real/out.bin"), [4u8; 9]);
    std::os::unix::fs::symlink("real", tree.join("linked/build")).unwrap();

    let archive = fixture.archive("plain");
    let manifest = &archive["manifest"];
    assert_eq!(manifest["format"], "worktree.archive/1", "{archive}");
    assert!(manifest.get("build_output").is_none(), "{archive}");
    let patch = patch_of(&archive);
    for kept in [
        "b/other/build/out.bin",
        "b/other/.dart_tool/version",
        "b/tool/build/out.bin",
        "b/tool/pubspec.yaml/readme",
        "b/linked/build",
        "b/linked/real/out.bin",
    ] {
        assert!(patch.contains(kept), "{kept}: {patch}");
    }
}

#[test]
fn archive_keeps_a_tracked_build_directory_beside_a_pubspec() {
    let fixture = Fixture::new(&[("build/build.yaml", "targets: {}\n")]);
    let tree = fixture.tree("tracked");
    write(&tree.join("build/generated.dart"), "// generated\n");
    write(&tree.join("build/out.bin"), [5u8; 9]);
    // The index alone tracks a file below `.dart_tool/`.
    write(&tree.join(".dart_tool/staged.json"), "{}\n");
    write(&tree.join(".dart_tool/package_config.json"), "{}\n");
    git(&tree, &["add", "-f", ".dart_tool/staged.json"]);

    let archive = fixture.archive("tracked");
    let manifest = &archive["manifest"];
    assert!(manifest.get("build_output").is_none(), "{archive}");
    let patch = patch_of(&archive);
    for kept in [
        "b/build/generated.dart",
        "b/build/out.bin",
        "b/.dart_tool/staged.json",
        "b/.dart_tool/package_config.json",
    ] {
        assert!(patch.contains(kept), "{kept}: {patch}");
    }
}

#[test]
fn finish_archive_removes_a_tree_through_left_out_flutter_output() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("finished");
    Fixture::flutter_build(&tree, "");
    write(&tree.join("notes.txt"), "never committed\n");

    let finished = fixture.ok(&["finish", "--archive", "finished"]);
    let archive = &finished["archive"];
    assert_eq!(
        targets(archive)
            .into_iter()
            .map(|(path, _, _)| path)
            .collect::<Vec<_>>(),
        [".dart_tool", "build"],
        "{finished}"
    );
    let dry = fixture.gc("--dry-run", "finished");
    assert_eq!(dry["eligible"], true, "{dry}");
    let applied = fixture.gc("--apply", "finished");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!tree.exists(), "{}", tree.display());

    // A left-out file that is no longer below a recognised output directory refuses removal:
    // here the index starts to track one below `build/`, so `build/` is no Flutter output.
    let tree = fixture.tree("changed");
    Fixture::flutter_build(&tree, "");
    fixture.ok(&["finish", "--archive", "changed"]);
    let apk = "build/app/outputs/flutter-apk/app-release.apk";
    git(&tree, &["add", "-f", apk]);
    let applied = fixture.gc("--apply", "changed");
    assert!(applied["evidence"].is_null(), "{applied}");
    assert!(applied["refusal"].is_object(), "{applied}");
    for kept in FLUTTER_FILES.map(|(path, _)| path) {
        assert!(tree.join(kept).is_file(), "{kept}");
    }
}

/// An archive `archive` wrote before it recognised Flutter output kept that output in
/// `dirty.patch` and the fingerprint and recorded only the cargo target. Verification leaves out
/// layout only below recorded directories, so it still verifies and removal through it works.
#[test]
fn an_archive_written_before_flutter_was_recognised_still_verifies() {
    let fixture = Fixture::new(&[("Cargo.toml", "[package]\nname = \"one\"\n")]);
    let tree = fixture.tree("older");
    write(&tree.join("target/CACHEDIR.TAG"), TAG);
    write(&tree.join("target/debug/.fingerprint/one-1/lib-one"), "f\n");
    write(&tree.join("target/debug/deps/libone-1.rlib"), [9u8; 16]);
    Fixture::flutter_build(&tree, "");
    write(&tree.join("notes.txt"), "never committed\n");

    let archive = fixture.archive("older");
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let current = targets(&archive);
    assert_eq!(
        current
            .iter()
            .map(|(path, _, _)| path.as_str())
            .collect::<Vec<_>>(),
        [".dart_tool", "build", "target"],
        "{archive}"
    );

    // Rewrite it to what the release before wrote: the Flutter files enter the fingerprint
    // (`git hash-object --no-filters`, as `archive` hashes) and `dirty.patch` (the same `git diff`
    // arguments `archive` uses), and `build_output` keeps only the cargo target. The fingerprint
    // is rebuilt from HEAD, and without the Flutter files it is the one `archive` just recorded.
    let fingerprint = |paths: &[&str], name: &str| {
        let index = fixture.root.path().join(name);
        git_in(&tree, &["read-tree", "HEAD"], Some(&index));
        let mut info = String::new();
        for path in paths {
            let id = git(&tree, &["hash-object", "-w", "--no-filters", path]);
            writeln!(info, "100644 {id}\t{path}").unwrap();
        }
        let info_file = fixture.root.path().join(format!("{name}-info"));
        std::fs::write(&info_file, info).unwrap();
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&tree)
            .args(["update-index", "--index-info"])
            .env("GIT_INDEX_FILE", &index)
            .stdin(std::fs::File::open(&info_file).unwrap());
        assert!(command.status().unwrap().success());
        git_in(&tree, &["write-tree"], Some(&index))
    };
    assert_eq!(
        fingerprint(&["notes.txt"], "current-index"),
        archive["manifest"]["worktree_tree"].as_str().unwrap()
    );
    let mut older = vec!["notes.txt"];
    older.extend(FLUTTER_FILES.map(|(path, _)| path));
    let older_tree = fingerprint(&older, "older-index");
    let head = git(&tree, &["rev-parse", "HEAD"]);
    let patch = git_output(
        &tree,
        &[
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
            &head,
            &older_tree,
        ],
        None,
    )
    .stdout;
    assert!(String::from_utf8_lossy(&patch).contains("b/build/unit_test_assets/AssetManifest.bin"));
    std::fs::write(directory.join("dirty.patch"), &patch).unwrap();
    let manifest_path = directory.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["worktree_tree"] = Value::from(older_tree.as_str());
    manifest["patch"]["sha256"] = Value::from(sha256_hex(&patch));
    manifest["patch"]["bytes"] = Value::from(patch.len() as u64);
    let (_, files, bytes) = current
        .into_iter()
        .find(|(path, _, _)| path == "target")
        .unwrap();
    manifest["build_output"] = serde_json::json!({
        "targets": [{"path": "target", "origin": "archive", "files": files, "bytes": bytes}],
        "files": files,
        "bytes": bytes,
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    fixture.ok(&["finish", "older"]);
    let dry = fixture.gc("--dry-run", "older");
    assert_eq!(dry["eligible"], true, "{dry}");
    let applied = fixture.gc("--apply", "older");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!tree.exists(), "{}", tree.display());
}

#[test]
fn discard_cache_deletes_flutter_output_beside_a_pubspec() {
    let fixture = Fixture::new(&[("app/pubspec.yaml", "name: nested\n")]);
    let tree = fixture.tree("discard");
    Fixture::flutter_build(&tree, "");
    Fixture::flutter_build(&tree, "app");
    // An untracked `pubspec.yaml` is a regular file on disk too.
    write(&tree.join("pkg/pubspec.yaml"), "name: pkg\n");
    write(&tree.join("pkg/.dart_tool/version"), "3\n");

    let planned = fixture.discard(&tree, &["--dry-run"]);
    let expected = [
        ".dart_tool",
        "app/.dart_tool",
        "app/build",
        "build",
        "pkg/.dart_tool",
    ];
    let mut discarded = paths(&planned, "discarded");
    discarded.sort();
    assert_eq!(discarded, expected, "{planned}");
    for entry in planned["discarded"].as_array().unwrap() {
        assert_eq!(entry["kind"], "flutter-output", "{entry}");
    }
    assert!(tree.join("build").is_dir());

    // Committed and pushed, the tree holds nothing but cache, so `finish` collects it.
    git(&tree, &["add", "pkg/pubspec.yaml"]);
    git(&tree, &["commit", "--quiet", "-m", "pkg"]);
    git(
        &tree,
        &["push", "--quiet", "origin", "HEAD:refs/heads/discard"],
    );
    let finished = fixture.ok(&["finish", "--discard-cache", tree.to_str().unwrap()]);
    let mut discarded = paths(&finished["cache"], "discarded");
    discarded.sort();
    assert_eq!(discarded, expected, "{finished}");
    for directory in expected {
        assert!(!tree.join(directory).exists(), "{directory}");
    }
    assert!(tree.join("pubspec.yaml").is_file());
    assert_eq!(fixture.gc("--dry-run", "discard")["eligible"], true);
}

/// A `build/` or `.dart_tool/` with no regular `pubspec.yaml` beside it is no Flutter output: it
/// is discarded as `ignored-output`, and a symbolic link named `build` is unlinked, never followed.
#[test]
fn discard_cache_names_a_build_directory_with_no_pubspec_beside_ignored_output() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("retain");
    write(&tree.join("other/build/out.bin"), [1u8; 9]);
    write(&tree.join("other/.dart_tool/version"), "3\n");
    write(&tree.join("tool/pubspec.yaml/readme"), "a directory\n");
    write(&tree.join("tool/build/out.bin"), [2u8; 9]);
    // A symbolic link named `build` beside a pubspec, ignored by name: no real directory.
    write(&tree.join("linked/pubspec.yaml"), "name: linked\n");
    write(&tree.join("linked/real/out.bin"), [4u8; 9]);
    std::os::unix::fs::symlink("real", tree.join("linked/build")).unwrap();
    write(
        &fixture.repository.join(".git/info/exclude"),
        "/linked/build\n",
    );

    let cache = fixture.discard(&tree, &[]);
    let mut discarded = paths(&cache, "discarded");
    discarded.sort();
    assert_eq!(
        discarded,
        [
            "linked/build",
            "other/.dart_tool",
            "other/build",
            "tool/build"
        ],
        "{cache}"
    );
    for entry in cache["discarded"].as_array().unwrap() {
        assert_eq!(entry["kind"], "ignored-output", "{entry}");
    }
    assert_eq!(
        paths(&cache, "retained_ignored"),
        Vec::<String>::new(),
        "{cache}"
    );
    for gone in ["other/build", "other/.dart_tool", "tool/build"] {
        assert!(!tree.join(gone).exists(), "{gone}");
    }
    assert!(std::fs::symlink_metadata(tree.join("linked/build")).is_err());
    assert!(tree.join("linked/real/out.bin").is_file());
    assert!(tree.join("tool/pubspec.yaml/readme").is_file());
    // The untracked files beside them still differ from HEAD.
    let refusal = fixture.refused(&["finish", "--discard-cache", tree.to_str().unwrap()]);
    assert!(refusal["code"].is_string(), "{refusal}");
}

/// A Flutter app's output directory holding a `.git` at any depth, as a directory, a file or a
/// symbolic link, may hold a repository whose commits no remote does: `discard-cache` never
/// deletes it whole but splits it down to the repository, which it retains, and deletes an
/// output directory holding none whole.
#[test]
fn discard_cache_splits_flutter_output_holding_a_git_entry_at_any_depth() {
    let fixture = Fixture::new(&[
        ("app/pubspec.yaml", "name: app\n"),
        ("pkg/pubspec.yaml", "name: pkg\n"),
        ("plain/pubspec.yaml", "name: plain\n"),
    ]);
    let tree = fixture.tree("dotgit");
    Fixture::flutter_build(&tree, "");
    Fixture::flutter_build(&tree, "app");
    Fixture::flutter_build(&tree, "pkg");
    Fixture::flutter_build(&tree, "plain");
    // A repository deep below `build/`.
    let nested = tree.join("build/app/outputs/notes");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    write(&nested.join("journal.md"), "committed nowhere else\n");
    git(&nested, &["add", "journal.md"]);
    git(&nested, &["commit", "--quiet", "-m", "only copy"]);
    // A gitfile below `app/.dart_tool/`, and a symbolic link named `.git` below `pkg/build/`.
    write(
        &tree.join("app/.dart_tool/linked/.git"),
        "gitdir: ../../../elsewhere\n",
    );
    std::fs::create_dir_all(tree.join("pkg/build/web")).unwrap();
    std::os::unix::fs::symlink("../../lib", tree.join("pkg/build/web/.git")).unwrap();

    let planned = fixture.discard(&tree, &["--dry-run"]);
    let mut discarded = paths(&planned, "discarded");
    discarded.sort();
    let expected = [
        ".dart_tool",
        "app/.dart_tool/flutter_build",
        "app/.dart_tool/package_config.json",
        "app/build",
        "build/app/outputs/flutter-apk",
        "build/unit_test_assets",
        "pkg/.dart_tool",
        "pkg/build/app",
        "pkg/build/unit_test_assets",
        "plain/.dart_tool",
        "plain/build",
    ];
    assert_eq!(discarded, expected, "{planned}");
    let whole = [
        ".dart_tool",
        "app/build",
        "pkg/.dart_tool",
        "plain/.dart_tool",
        "plain/build",
    ];
    for entry in planned["discarded"].as_array().unwrap() {
        let kind = if whole.contains(&entry["path"].as_str().unwrap()) {
            "flutter-output"
        } else {
            "ignored-output"
        };
        assert_eq!(entry["kind"], kind, "{entry}");
    }
    let mut retained = paths(&planned, "retained_ignored");
    retained.sort();
    assert_eq!(
        retained,
        [
            "app/.dart_tool/linked",
            "build/app/outputs/notes",
            "pkg/build/web"
        ],
        "{planned}"
    );

    let applied = fixture.discard(&tree, &[]);
    let mut discarded = paths(&applied, "discarded");
    discarded.sort();
    assert_eq!(discarded, expected, "{applied}");
    for directory in expected {
        assert!(!tree.join(directory).exists(), "{directory}");
    }
    assert!(nested.join(".git").is_dir() && nested.join("journal.md").is_file());
    assert!(tree.join("app/.dart_tool/linked/.git").is_file());
    assert!(
        std::fs::symlink_metadata(tree.join("pkg/build/web/.git"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(tree.join("lib/main.dart").is_file());
}

/// A cargo target named `build/` beside a `pubspec.yaml` is classified by the cargo rule alone in
/// the strip as in `archive` and `discard-cache`: cargo's layout below it is stripped from an
/// archive written before, and `build/notes.md`, which is no layout, stays in `dirty.patch`.
#[test]
fn strip_keeps_a_non_layout_file_in_a_cargo_target_named_build_beside_a_pubspec() {
    let fixture = Fixture::new(&[("Cargo.toml", "[package]\nname = \"one\"\n")]);
    let tree = fixture.tree("strip");
    write(&tree.join("build/CACHEDIR.TAG"), TAG);
    write(&tree.join("build/debug/.fingerprint/one-1/lib-one"), "f\n");
    write(&tree.join("build/debug/deps/libone-1.rlib"), [9u8; 16]);
    write(
        &tree.join("build/notes.md"),
        "hand-written, never committed\n",
    );

    // While the index tracks every file below `build/`, `archive` leaves nothing out: the archive
    // is the one a release before this classifier wrote.
    git(&tree, &["add", "-f", "build"]);
    let archive = fixture.archive("strip");
    git(&tree, &["read-tree", "HEAD"]);
    assert!(
        archive["manifest"].get("build_output").is_none(),
        "{archive}"
    );
    for path in ["b/build/notes.md", "b/build/debug/deps/libone-1.rlib"] {
        assert!(patch_of(&archive).contains(path), "{path}");
    }
    fixture.ok(&["finish", "strip"]);
    assert!(fixture.gc("--apply", "strip")["evidence"].is_object());
    assert!(!tree.exists());

    let strip = |extra: &[&str]| {
        let mut args = vec![
            "prune-archives",
            "--strip-build-output",
            "--repo",
            fixture.repository.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        fixture.ok(&args)
    };
    let dry = strip(&[]);
    assert_eq!(dry["archives"][0]["verdict"], "strippable", "{dry}");
    assert_eq!(dry["archives"][0]["sections_stripped"], 3, "{dry}");
    strip(&["--apply", "--id", "strip"]);
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    let recorded: Vec<(&str, u64)> = manifest["build_output"]["targets"]
        .as_array()
        .unwrap_or_else(|| panic!("{manifest}"))
        .iter()
        .map(|target| {
            (
                target["path"].as_str().unwrap(),
                target["files"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(recorded, [("build", 3)], "{manifest}");
    let patch = std::fs::read_to_string(directory.join("dirty.patch")).unwrap();
    assert!(patch.contains("b/build/notes.md"), "{patch}");
    assert!(!patch.contains("b/build/debug/"), "{patch}");
}
