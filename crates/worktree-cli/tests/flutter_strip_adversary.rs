//! Adversarial cases for the Flutter part of `prune-archives --strip-build-output`.
//!
//! The strip deletes archived bytes, so every case asks the same two questions: is any section
//! stripped that is not a Flutter app's (or cargo's) build output, and is the rewritten archive
//! still the exact tree minus what was stripped?
#![cfg(unix)]

use b10x_worktree_domain::{ScannedPatch, ScannedSection, plan_patch_strip};
use serde_json::Value;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
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

fn at(tree: &Path, relative: &[u8]) -> PathBuf {
    tree.join(OsStr::from_bytes(relative))
}

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("crate");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        write(&repository.join("source"), "original\n");
        write(&repository.join(".gitignore"), "/target/\n");
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

    /// A managed tree whose HEAD, pushed, holds `committed`; `work` then changes it, the tree is
    /// archived and retired, and the archive's path is returned.
    fn archived(&self, id: &str, committed: &[(&[u8], &[u8])], work: &dyn Fn(&Path)) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository.to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "flutter strip adversary",
        ]);
        let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        for (path, bytes) in committed {
            write(&at(&tree, path), bytes);
        }
        git(&tree, &["add", "."]);
        git(&tree, &["commit", "--quiet", "-m", id]);
        let branch = format!("HEAD:refs/heads/{id}");
        git(&tree, &["push", "--quiet", "origin", &branch]);
        work(&tree);
        let archive = self.ok(&["archive", id])["archive"].clone();
        assert_eq!(
            archive["manifest"]["format"], "worktree.archive/1",
            "{archive}"
        );
        self.ok(&["finish", id]);
        let report = self.ok(&[
            "gc",
            "--repo",
            self.repository.to_str().unwrap(),
            "--apply",
            "--id",
            id,
        ]);
        assert!(report["assessments"][0]["evidence"].is_object(), "{report}");
        assert!(!tree.exists());
        PathBuf::from(archive["path"].as_str().unwrap())
    }

    fn strip(&self, extra: &[&str]) -> Value {
        let mut args = vec![
            "prune-archives",
            "--strip-build-output",
            "--repo",
            self.repository.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.ok(&args)
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
}

/// Apply the archive's patch over its HEAD in a scratch index: the `-z` listing of every entry,
/// as raw `<mode> <blob> <stage>\t<path>` records, and the tree id `write-tree` gives.
fn applied(repository: &Path, archive: &Path) -> (BTreeSet<Vec<u8>>, String) {
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let index = scratch.path().join("index");
    let ok = |args: &[&str]| {
        let output = git_output(repository, args, Some(&index));
        assert!(output.status.success(), "{args:?}: {output:?}");
        output.stdout
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
    let listing = ok(&["ls-files", "-s", "-z"])
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    let tree = String::from_utf8(ok(&["write-tree"]))
        .unwrap()
        .trim()
        .to_owned();
    (listing, tree)
}

fn record_path(record: &[u8]) -> &[u8] {
    let tab = record.iter().position(|byte| *byte == b'\t').unwrap();
    &record[tab + 1..]
}

/// One Flutter app tree carrying every shape the brief names around `build/`, `.dart_tool/` and
/// `pubspec.yaml`. Only output directories beside a `pubspec.yaml` lose sections; the rewritten
/// archive applies over HEAD to exactly the archived tree minus those files, its `worktree_tree`
/// is that tree, and its totals are the sums.
#[test]
#[allow(clippy::too_many_lines)]
fn adversary_flutter_strip_removes_only_output_and_leaves_a_usable_archive() {
    let fixture = Fixture::new();
    let committed: Vec<(&[u8], &[u8])> = vec![
        (b"pubspec.yaml", b"name: app\n"),
        (b"lib/main.dart", b"void main() {}\n"),
        // A package whose `build/` HEAD tracks partly.
        (b"pkg/pubspec.yaml", b"name: pkg\n"),
        (b"pkg/build/gen.dart", b"// tracked source\n"),
        // HEAD holds `build` as a file beside a pubspec; the tree modifies it. (A file turned
        // into a directory is refused by `gc` before any archive is relied on.)
        (b"filebuild/pubspec.yaml", b"name: filebuild\n"),
        (b"filebuild/build", b"a file named build\n"),
        // A pubspec.yaml HEAD holds as a directory.
        (b"dirpub/pubspec.yaml/README", b"not a manifest\n"),
        // A pubspec.yaml the tree deletes.
        (b"gone/pubspec.yaml", b"name: gone\n"),
        // A non-ASCII app root: Git C-quotes it in the patch.
        ("\u{e4}pp/pubspec.yaml".as_bytes(), b"name: aepp\n"),
    ];
    let archive = fixture.archived("app", &committed, &|tree| {
        // Source that must survive.
        write(&tree.join("pubspec.yaml"), "name: app\nversion: 2.0.0\n");
        write(
            &tree.join("pkg/build/new_source.dart"),
            "// untracked source\n",
        );
        write(
            &tree.join("filebuild/build"),
            "a modified file named build\n",
        );
        write(&tree.join("dirpub/build/out.bin"), [9u8; 4]);
        write(&tree.join("lib/build/x.dart"), "// ancestor pubspec only\n");
        write(&tree.join("nochild/build/out.bin"), [8u8; 4]);
        write(
            &tree.join("nochild/build/sub/pubspec.yaml"),
            "name: child\n",
        );
        write(&tree.join("newpkg/pubspec.yaml"), "name: newpkg\n");
        write(&tree.join("newpkg/lib/a.dart"), "// new source\n");
        write(&tree.join("build.dart"), "// a file beside build/\n");
        std::fs::remove_file(tree.join("gone/pubspec.yaml")).unwrap();
        // Output that is stripped.
        write(&tree.join(".dart_tool/package_config.json"), "{}\n");
        write(&at(tree, b"build/caf\xe9 \"q\"\tx.bin"), [0u8, 1, 255]);
        // An untagged cargo target inside `build/`: `archive` keeps it, the patch recognises it.
        write(
            &tree.join("build/rust/target/debug/.fingerprint/one/lib"),
            "fp\n",
        );
        write(
            &tree.join("build/rust/target/debug/deps/libone.rlib"),
            [1u8; 7],
        );
        write(&tree.join("newpkg/build/out.bin"), [2u8; 5]);
        write(&tree.join("gone/build/out.bin"), [3u8; 6]);
        write(&tree.join("\u{e4}pp/build/web/main.dart.js"), "js\n");
        write(&tree.join("\u{e4}pp/.dart_tool/version"), "3\n");
    });
    let stripped: BTreeSet<Vec<u8>> = [
        &b".dart_tool/package_config.json"[..],
        b"build/caf\xe9 \"q\"\tx.bin",
        b"build/rust/target/debug/.fingerprint/one/lib",
        b"build/rust/target/debug/deps/libone.rlib",
        b"newpkg/build/out.bin",
        b"gone/build/out.bin",
        "\u{e4}pp/build/web/main.dart.js".as_bytes(),
        "\u{e4}pp/.dart_tool/version".as_bytes(),
    ]
    .into_iter()
    .map(<[u8]>::to_vec)
    .collect();
    let (before, _) = applied(&fixture.repository, &archive);
    let before_paths: BTreeSet<Vec<u8>> = before.iter().map(|r| record_path(r).to_vec()).collect();
    for path in &stripped {
        assert!(
            before_paths.contains(path),
            "{:?} not archived",
            String::from_utf8_lossy(path)
        );
    }

    let dry = fixture.strip(&[]);
    let item = dry["archives"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["archive_directory"] == "app")
        .unwrap()
        .clone();
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(item["sections_stripped"], stripped.len(), "{item}");

    fixture.strip(&["--apply", "--id", "app"]);
    let (after, tree) = applied(&fixture.repository, &archive);
    let expected: BTreeSet<Vec<u8>> = before
        .iter()
        .filter(|record| !stripped.contains(record_path(record)))
        .cloned()
        .collect();
    let lossy = |set: &BTreeSet<Vec<u8>>| {
        set.iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(lossy(&after), lossy(&expected));
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], "worktree.archive/3", "{manifest}");
    assert_eq!(manifest["worktree_tree"], tree.as_str(), "{manifest}");
    let output = &manifest["build_output"];
    let targets: Vec<(String, u64, u64)> = output["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|target| {
            assert_eq!(target["origin"], "strip", "{output}");
            (
                target["path"].as_str().unwrap().to_owned(),
                target["files"].as_u64().unwrap(),
                target["bytes"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        targets,
        [
            (".dart_tool".to_owned(), 1, 3),
            ("build".to_owned(), 3, 3 + 3 + 7),
            ("gone/build".to_owned(), 1, 6),
            ("newpkg/build".to_owned(), 1, 5),
            ("\u{e4}pp/.dart_tool".to_owned(), 1, 2),
            ("\u{e4}pp/build".to_owned(), 1, 3),
        ],
        "{output}"
    );
    assert_eq!(output["files"], 8, "{output}");
    assert_eq!(
        output["bytes"],
        targets.iter().map(|(_, _, bytes)| bytes).sum::<u64>(),
        "{output}"
    );
    assert_eq!(fixture.prune_verdict("app"), "uncommitted-state");
    // A second strip finds nothing more.
    let again = fixture.strip(&[]);
    assert_eq!(
        again["archives"][0]["verdict"], "nothing-to-strip",
        "{again}"
    );
}

fn section(path: &[u8]) -> ScannedSection {
    ScannedSection {
        path: Some(path.to_vec()),
        new_file: true,
        tracked: false,
        signed_tag: false,
        patch_bytes: 10,
        content_bytes: 3,
    }
}

fn plan(sections: Vec<ScannedSection>, files: &[&[u8]], directories: &[&[u8]]) -> ScannedPatch {
    let set = |paths: &[&[u8]]| paths.iter().map(|path| path.to_vec()).collect();
    ScannedPatch::against_head(sections, set(files), set(directories))
}

/// A path named twice below an output directory keeps both sections; a non-UTF-8 app root is
/// never recognised; a `pubspec.yaml` the patch modifies still names its app root.
#[test]
fn adversary_flutter_domain_edges_hold() {
    let twice = plan(
        vec![
            section(b"build/x"),
            section(b"build/x"),
            section(b"build/y"),
        ],
        &[b"pubspec.yaml"],
        &[],
    );
    assert_eq!(plan_patch_strip(&twice).strip, [false, false, true]);

    let latin = plan(
        vec![section(b"\xff/build/x")],
        &[b"\xff/pubspec.yaml"],
        &[b"\xff"],
    );
    assert_eq!(plan_patch_strip(&latin).strip, [false]);

    let mut modified = section(b"pubspec.yaml");
    modified.new_file = false;
    let patch = plan(
        vec![modified, section(b".dart_tool/x")],
        &[b"pubspec.yaml"],
        &[],
    );
    assert_eq!(plan_patch_strip(&patch).strip, [false, true]);

    // A `.dart_tool` HEAD holds as a file: nothing below it is output.
    let patch = plan(
        vec![section(b".dart_tool/x")],
        &[b"pubspec.yaml", b".dart_tool"],
        &[],
    );
    assert_eq!(plan_patch_strip(&patch).strip, [false]);
}

/// The contract records the outermost directory whose rule strips the path. A Flutter output inside
/// a recognised cargo target, on a path that is not cargo layout, is stripped by the Flutter rule
/// and recorded as its own target, nested in the cargo target.
#[test]
fn adversary_flutter_output_inside_a_cargo_target_is_recorded_by_its_own_rule() {
    let mut tag = section(b"x/target/CACHEDIR.TAG");
    tag.signed_tag = true;
    let patch = plan(
        vec![
            tag,
            section(b"x/target/debug/.fingerprint/one/lib"),
            section(b"x/target/debug/deps/libone.rlib"),
            section(b"x/target/foo/pubspec.yaml"),
            section(b"x/target/foo/build/out.bin"),
        ],
        &[b"README.md"],
        &[],
    );
    let plan = plan_patch_strip(&patch);
    assert_eq!(plan.strip, [true, true, true, false, true], "{plan:?}");
    let paths: Vec<&str> = plan
        .targets
        .iter()
        .map(|target| target.path.as_str())
        .collect();
    assert_eq!(paths, ["x/target", "x/target/foo/build"], "{plan:?}");
}
