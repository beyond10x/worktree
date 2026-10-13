//! `prune-archives --strip-build-output` strips ignored output from existing archives.
//!
//! A section that purely adds an untracked file is stripped when Git's exclude engine ignores
//! its path under HEAD's rules (every `.gitignore` HEAD's tree holds and the repository's
//! `info/exclude`, never the user's `core.excludesFile`) and the keep file does not keep it.
//! Repository content stays whatever the rules say. Each stripped ignored path is recorded as a
//! `strip` target per outermost ignored directory holding no kept path, or per file.
#![cfg(unix)]

use b10x_worktree_domain::{
    BuildOutputOrigin, ExcludeObservations, ScannedPatch, ScannedSection, plan_patch_strip,
};
use serde_json::Value;
use std::collections::BTreeSet;
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

/// One repository with a bare remote, below one activated profile. HEAD commits `committed`.
struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
}

impl Fixture {
    fn new(committed: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("site");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        write(&repository.join("source"), "original\n");
        for (path, contents) in committed {
            write(&repository.join(path), contents);
        }
        git(&repository, &["add", "-f", "."]);
        git(&repository, &["commit", "--quiet", "-m", "fixture"]);
        let remote = root.path().join("site.git");
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

    /// The CLI, with `global` as the user's Git configuration (`/dev/null` when `None`).
    fn run_with(&self, args: &[&str], global: Option<&Path>) -> Output {
        let outside = self.root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        Command::new(env!("CARGO_BIN_EXE_worktree"))
            .arg("--json")
            .args(args)
            .current_dir(outside)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env(
                "GIT_CONFIG_GLOBAL",
                global.unwrap_or(Path::new("/dev/null")),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run_with(args, None);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// A managed tree with one pushed commit; `work` changes it, then it is archived and retired.
    /// Returns the archive's path.
    fn archived(&self, id: &str, work: &dyn Fn(&Path)) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository.to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "ignored output strip",
        ]);
        let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        write(&tree.join("work.txt"), format!("{id}\n"));
        git(&tree, &["add", "work.txt"]);
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

    fn strip_with(&self, extra: &[&str], global: Option<&Path>) -> Output {
        let mut args = vec![
            "prune-archives",
            "--strip-build-output",
            "--repo",
            self.repository.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.run_with(&args, global)
    }

    fn strip(&self, extra: &[&str]) -> Value {
        let output = self.strip_with(extra, None);
        assert!(output.status.success(), "{extra:?}: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The one dry-run assessment of `directory`, run with `global` as the user's configuration.
    fn assessment_with(&self, directory: &str, global: Option<&Path>) -> Value {
        let output = self.strip_with(&[], global);
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        report["archives"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["archive_directory"] == directory)
            .unwrap_or_else(|| panic!("{report}"))
            .clone()
    }

    fn assessment(&self, directory: &str) -> Value {
        self.assessment_with(directory, None)
    }
}

/// The `b/` paths of every `diff --git` section of the archive's patch, in patch order.
fn patch_paths(archive: &Path) -> Vec<String> {
    let Ok(patch) = std::fs::read(archive.join("dirty.patch")) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&patch)
        .lines()
        .filter_map(|line| line.strip_prefix("diff --git a/"))
        .map(|rest| rest.split_once(" b/").unwrap().1.to_owned())
        .collect()
}

/// Apply the archive's patch over its HEAD in a scratch index: the paths the index then holds,
/// and the tree id `write-tree` gives.
fn applied(repository: &Path, archive: &Path) -> (BTreeSet<String>, String) {
    let manifest = manifest(archive);
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
    let paths = ok(&["ls-files"]).lines().map(str::to_owned).collect();
    (paths, ok(&["write-tree"]).trim().to_owned())
}

fn manifest(archive: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap()
}

/// The `(path, origin, files, bytes)` of every recorded build output target.
fn targets(archive: &Path) -> Vec<(String, String, u64, u64)> {
    manifest(archive)["build_output"]["targets"]
        .as_array()
        .map(|targets| {
            targets
                .iter()
                .map(|target| {
                    (
                        target["path"].as_str().unwrap().to_owned(),
                        target["origin"].as_str().unwrap().to_owned(),
                        target["files"].as_u64().unwrap(),
                        target["bytes"].as_u64().unwrap(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// No scratch directory is left beside the archive.
fn assert_no_scratch(archive: &Path) {
    let leftovers = std::fs::read_dir(archive.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.'))
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

const INDEX_JS: &str = "module.exports = 1;\n";
const REVIEW: &str = "# review 1\n";
const TODO: &str = "- write the story\n";

/// The acceptance fixture's work: one file below an ignored `node_modules/`, one below an
/// ignored `target/` that is no cargo target, and one in `notes/`, which HEAD does not ignore.
fn three_files(tree: &Path) {
    write(&tree.join("website/node_modules/x/index.js"), INDEX_JS);
    write(&tree.join("target/review-1/a.md"), REVIEW);
    write(&tree.join("notes/todo.md"), TODO);
}

const IGNORE: &str = "node_modules/\n/target/\n";

#[test]
fn ignored_output_is_stripped_and_the_rest_applies_over_head() {
    let fixture = Fixture::new(&[(".gitignore", IGNORE)]);
    let archive = fixture.archived("ignored", &three_files);
    assert_eq!(
        patch_paths(&archive),
        [
            "notes/todo.md",
            "target/review-1/a.md",
            "website/node_modules/x/index.js"
        ]
    );
    let before = snapshot(&archive);

    let item = fixture.assessment("ignored");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(item["sections_stripped"], 2, "{item}");
    assert_eq!(item["sections_kept"], 1, "{item}");
    assert_eq!(snapshot(&archive), before, "a dry-run writes nothing");
    assert_no_scratch(&archive);

    let report = fixture.strip(&["--apply", "--id", "ignored"]);
    assert_eq!(report["archives"][0]["applied"], true, "{report}");
    assert_no_scratch(&archive);
    assert_eq!(patch_paths(&archive), ["notes/todo.md"]);
    let manifest = manifest(&archive);
    assert_eq!(manifest["format"], "worktree.archive/3", "{manifest}");
    assert_eq!(
        targets(&archive),
        [
            ("target".into(), "strip".into(), 1, REVIEW.len() as u64),
            (
                "website/node_modules".into(),
                "strip".into(),
                1,
                INDEX_JS.len() as u64
            ),
        ]
    );
    let (paths, tree) = applied(&fixture.repository, &archive);
    assert!(paths.contains("notes/todo.md"), "{paths:?}");
    assert!(!paths.contains("target/review-1/a.md"), "{paths:?}");
    assert!(
        !paths.contains("website/node_modules/x/index.js"),
        "{paths:?}"
    );
    assert_eq!(manifest["worktree_tree"], tree.as_str(), "{manifest}");

    // Stripped once, nothing ignored is left.
    assert_eq!(fixture.assessment("ignored")["verdict"], "nothing-to-strip");
}

#[test]
fn a_path_heads_keep_file_keeps_stays() {
    let fixture = Fixture::new(&[
        (".gitignore", IGNORE),
        (".worktreekeep", "target/review-*/\n"),
    ]);
    let archive = fixture.archived("kept", &three_files);

    let item = fixture.assessment("kept");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(item["sections_stripped"], 1, "{item}");
    assert_eq!(item["sections_kept"], 2, "{item}");
    fixture.strip(&["--apply", "--id", "kept"]);
    assert_eq!(
        patch_paths(&archive),
        ["notes/todo.md", "target/review-1/a.md"]
    );
    assert_eq!(
        targets(&archive),
        [(
            "website/node_modules".into(),
            "strip".into(),
            1,
            INDEX_JS.len() as u64
        )]
    );
    let (paths, _) = applied(&fixture.repository, &archive);
    assert!(paths.contains("target/review-1/a.md"), "{paths:?}");
}

/// A keep file the patch purely adds keeps what it names, and its own section stays even when
/// HEAD's rules ignore it.
#[test]
fn a_keep_file_the_patch_adds_keeps_and_stays() {
    let ignore = format!("{IGNORE}/.worktreekeep\n");
    let fixture = Fixture::new(&[(".gitignore", ignore.as_str())]);
    let archive = fixture.archived("added", &|tree| {
        three_files(tree);
        write(&tree.join(".worktreekeep"), "# records\ntarget/review-*/");
    });

    let item = fixture.assessment("added");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(item["sections_stripped"], 1, "{item}");
    assert_eq!(item["sections_kept"], 3, "{item}");
    fixture.strip(&["--apply", "--id", "added"]);
    assert_eq!(
        patch_paths(&archive),
        [".worktreekeep", "notes/todo.md", "target/review-1/a.md"]
    );
}

/// The user's `core.excludesFile`, set or Git's default under `XDG_CONFIG_HOME`, is never read;
/// the repository's `info/exclude` is.
#[test]
fn a_path_only_the_users_excludes_file_ignores_stays() {
    let fixture = Fixture::new(&[(".gitignore", IGNORE)]);
    let archive = fixture.archived("global", &|tree| {
        write(&tree.join("drafts/plan.md"), "a plan\n");
        write(&tree.join("xdg/note.md"), "a note\n");
        write(&tree.join("local/cache.bin"), [0u8, 1, 2]);
    });
    write(&fixture.repository.join(".git/info/exclude"), "/local/\n");
    let excludes = fixture.root.path().join("global-excludes");
    write(&excludes, "drafts/\nxdg/\n");
    write(&fixture.root.path().join("config/git/ignore"), "xdg/\n");
    let global = fixture.root.path().join("global-config");
    write(
        &global,
        format!("[core]\n\texcludesFile = {}\n", excludes.display()),
    );

    for user in [None, Some(global.as_path())] {
        let item = fixture.assessment_with("global", user);
        assert_eq!(item["verdict"], "strippable", "{user:?}: {item}");
        assert_eq!(item["sections_stripped"], 1, "{user:?}: {item}");
        assert_eq!(item["sections_kept"], 2, "{user:?}: {item}");
    }
    let output = fixture.strip_with(&["--apply", "--id", "global"], Some(&global));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(patch_paths(&archive), ["drafts/plan.md", "xdg/note.md"]);
    assert_eq!(targets(&archive), [("local".into(), "strip".into(), 1, 3)]);
}

/// A bare repository below an ignored directory keeps every section; the ignored file beside it
/// is stripped, and the stripped patch still applies.
#[test]
fn a_git_directory_the_patch_shows_stays_whatever_the_rules_say() {
    let fixture = Fixture::new(&[(".gitignore", "fixtures/\n")]);
    let relative = "fixtures/repo.git";
    let seed = fixture.root.path().join("seed.txt");
    write(&seed, "only copy\n");
    let archive = fixture.archived("bare", &|tree| {
        let bare = tree.join(relative);
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--quiet", "--bare", "-b", "main"]);
        // An object, so the patch shows `objects/` beside `HEAD`.
        git(&bare, &["hash-object", "-w", seed.to_str().unwrap()]);
        write(&tree.join("fixtures/run.log"), "ran\n");
    });
    let repository_sections = patch_paths(&archive)
        .iter()
        .filter(|path| path.starts_with(&format!("{relative}/")))
        .count();
    assert!(repository_sections >= 2, "{:?}", patch_paths(&archive));

    let item = fixture.assessment("bare");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(item["sections_stripped"], 1, "{item}");
    assert_eq!(item["sections_kept"], repository_sections, "{item}");
    fixture.strip(&["--apply", "--id", "bare"]);
    let paths = patch_paths(&archive);
    assert!(paths.contains(&format!("{relative}/HEAD")), "{paths:?}");
    assert!(!paths.contains(&"fixtures/run.log".to_owned()), "{paths:?}");
    // The outermost ignored directory holds kept paths, so the file itself is recorded.
    assert_eq!(
        targets(&archive),
        [("fixtures/run.log".into(), "strip".into(), 1, 4)]
    );
    applied(&fixture.repository, &archive);
}

/// A section under a `.git` component stays although HEAD's rules ignore its directory. Git
/// refuses such a path, so the archive's patch is rewritten by hand to hold one; the apply that
/// cannot verify the kept section over HEAD then leaves the archive byte for byte.
#[test]
fn a_path_under_a_git_component_stays_whatever_the_rules_say() {
    let fixture = Fixture::new(&[(".gitignore", "vendor/\n")]);
    let archive = fixture.archived("component", &|tree| {
        write(&tree.join("vendor/lib/x.js"), "x\n");
    });
    let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap();
    let mut forged = patch.clone();
    forged.push_str(&patch.replace("vendor/lib/x.js", "vendor/.git/config"));
    std::fs::write(archive.join("dirty.patch"), &forged).unwrap();
    let mut manifest = manifest(&archive);
    manifest["patch"]["sha256"] = Value::from(sha256_hex(forged.as_bytes()));
    manifest["patch"]["bytes"] = Value::from(forged.len());
    std::fs::write(
        archive.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let item = fixture.assessment("component");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(item["sections_stripped"], 1, "{item}");
    assert_eq!(item["sections_kept"], 1, "{item}");
    let before = snapshot(&archive);
    let output = fixture.strip_with(&["--apply", "--id", "component"], None);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(snapshot(&archive), before);
    assert_no_scratch(&archive);
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

fn section(path: &str) -> ScannedSection {
    ScannedSection {
        path: Some(path.as_bytes().to_vec()),
        new_file: true,
        tracked: false,
        signed_tag: false,
        patch_bytes: 10,
        content_bytes: 3,
    }
}

fn paths(paths: &[&str]) -> BTreeSet<Vec<u8>> {
    paths.iter().map(|path| path.as_bytes().to_vec()).collect()
}

/// The domain decides from Git's observations alone: repository content and kept paths stay,
/// and each stripped path is recorded under its outermost ignored directory that holds no kept
/// path, or as itself.
#[test]
fn ignored_output_is_recorded_per_outermost_directory_holding_no_kept_path() {
    let sections = [
        "target/a/x",
        "target/a/y",
        "target/keep/z",
        "target/b/w",
        "vendor/.git/config",
        "fixture/repo/HEAD",
        "fixture/repo/objects/ab/cd",
        "x.log",
        "notes/todo.md",
    ];
    let patch = ScannedPatch {
        sections: sections.iter().map(|path| section(path)).collect(),
        exclusion: ExcludeObservations {
            ignored: paths(&[
                "target/a/x",
                "target/a/y",
                "target/keep/z",
                "target/b/w",
                "vendor/.git/config",
                "fixture/repo/HEAD",
                "fixture/repo/objects/ab/cd",
                "x.log",
            ]),
            ignored_directories: paths(&[
                "target",
                "target/a",
                "target/keep",
                "target/b",
                "vendor",
                "fixture",
            ]),
            kept: paths(&["target/keep/z"]),
            keep_unreadable: false,
        },
        ..ScannedPatch::default()
    };
    let plan = plan_patch_strip(&patch);
    assert_eq!(
        plan.strip,
        [true, true, false, true, false, false, false, true, false]
    );
    let recorded = plan
        .targets
        .iter()
        .map(|target| {
            assert_eq!(target.origin, BuildOutputOrigin::Strip);
            (target.path.as_str(), target.files)
        })
        .collect::<Vec<_>>();
    assert_eq!(recorded, [("target/a", 2), ("target/b", 1), ("x.log", 1)]);

    // A keep file that cannot be read keeps everything.
    let mut unreadable = patch.clone();
    unreadable.exclusion.keep_unreadable = true;
    assert_eq!(plan_patch_strip(&unreadable).sections_stripped, 0);
}
