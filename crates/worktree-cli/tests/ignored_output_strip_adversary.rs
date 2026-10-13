//! Adversarial cases for `prune-archives --strip-build-output` stripping ignored output.
//!
//! The archive is the only copy of a removed tree's uncommitted state. A section the strip
//! removes is gone, so a path the tree's own Git did not call ignored must stay, and a keep file
//! that is not a regular text file keeps every path (AGENTS.md, CHANGELOG.md).
#![cfg(unix)]

use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeSet;
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
            "-c",
            "core.excludesFile=/dev/null",
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

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
}

impl Fixture {
    fn new(committed: &[(&str, &[u8])], links: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("site");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        write(&repository.join("source"), "original\n");
        for (path, contents) in committed {
            write(&repository.join(path), contents);
        }
        for (path, target) in links {
            let link = repository.join(path);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, link).unwrap();
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

    fn archived(&self, id: &str, work: &dyn Fn(&Path)) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository.to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "ignored output strip adversary",
        ]);
        let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        write(&tree.join("work.txt"), format!("{id}\n"));
        git(&tree, &["add", "work.txt"]);
        git(&tree, &["commit", "--quiet", "-m", id]);
        let branch = format!("HEAD:refs/heads/{id}");
        git(&tree, &["push", "--quiet", "origin", &branch]);
        work(&tree);
        let archive = self.ok(&["archive", id])["archive"].clone();
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
        let output = self.run(&args);
        assert!(output.status.success(), "{extra:?}: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn assessment(&self, directory: &str) -> Value {
        let report = self.strip(&[]);
        report["archives"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["archive_directory"] == directory)
            .unwrap_or_else(|| panic!("{report}"))
            .clone()
    }
}

fn manifest(archive: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap()
}

/// The paths the index holds after applying the archive's patch over its HEAD, as raw bytes.
fn applied_paths(repository: &Path, archive: &Path) -> BTreeSet<Vec<u8>> {
    let manifest = manifest(archive);
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
    ok(&["ls-files", "-z"])
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Whether the tree's own Git, with no user excludes, calls `path` ignored.
fn ignored_in_tree(tree: &Path, path: &str) -> bool {
    let output = git_output(tree, &["check-ignore", "-q", path], None);
    match output.status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => panic!("{output:?}"),
    }
}

const IGNORE: &str = "node_modules/\n/target/\n";

/// The patch drops `/target/` from the root `.gitignore`: in the tree, Git listed
/// `target/review-1/a.md` as an untracked file it does not ignore. HEAD's rules still ignore it,
/// and the archive is its only copy, so it must stay.
#[test]
fn a_path_the_trees_modified_gitignore_no_longer_ignores_stays() {
    let fixture = Fixture::new(&[(".gitignore", IGNORE.as_bytes())], &[]);
    fixture.archived("unignored", &|tree| {
        write(&tree.join(".gitignore"), "node_modules/\n");
        write(&tree.join("target/review-1/a.md"), "# review 1\n");
        assert!(!ignored_in_tree(tree, "target/review-1/a.md"));
    });

    let item = fixture.assessment("unignored");
    assert_eq!(item["sections_stripped"], 0, "{item}");
}

/// The patch adds `logs/.gitignore` re-including `keep.log`: in the tree Git did not ignore
/// `logs/keep.log`. HEAD's root rule `*.log` does, and the strip must not take it.
#[test]
fn a_path_a_gitignore_the_patch_adds_re_includes_stays() {
    let fixture = Fixture::new(&[(".gitignore", b"*.log\n")], &[]);
    fixture.archived("reincluded", &|tree| {
        write(&tree.join("logs/.gitignore"), "!keep.log\n");
        write(&tree.join("logs/keep.log"), "the only copy\n");
        assert!(!ignored_in_tree(tree, "logs/keep.log"));
    });

    let item = fixture.assessment("reincluded");
    assert_eq!(item["sections_stripped"], 0, "{item}");
}

/// A `.worktreekeep` the patch adds as a symbolic link is not a regular file: AGENTS.md says it
/// keeps every path. Its link target must not be read as patterns.
#[test]
fn a_keep_file_the_patch_adds_as_a_symlink_keeps_every_path() {
    let fixture = Fixture::new(&[(".gitignore", IGNORE.as_bytes())], &[]);
    let archive = fixture.archived("linked-keep", &|tree| {
        write(&tree.join("website/node_modules/x/index.js"), "x\n");
        write(&tree.join("target/review-1/a.md"), "# review 1\n");
        write(&tree.join("shared/keep"), "target/\nnode_modules/\n");
        std::os::unix::fs::symlink("shared/keep", tree.join(".worktreekeep")).unwrap();
    });
    let patch = std::fs::read(archive.join("dirty.patch")).unwrap();
    assert!(
        patch
            .windows(b"new file mode 120000".len())
            .any(|window| window == b"new file mode 120000"),
        "the patch adds the keep file as a symbolic link"
    );

    let item = fixture.assessment("linked-keep");
    assert_eq!(item["sections_stripped"], 0, "{item}");
}

/// A keep file HEAD holds that is not text keeps every path (AGENTS.md: "one that is not a
/// regular file or text, keeps every path"); the same file added by the patch already does.
#[test]
fn a_binary_keep_file_in_head_keeps_every_path() {
    let fixture = Fixture::new(
        &[
            (".gitignore", IGNORE.as_bytes()),
            (".worktreekeep", b"\x00\x01\x02binary\n"),
        ],
        &[],
    );
    fixture.archived("binary-keep", &|tree| {
        write(&tree.join("website/node_modules/x/index.js"), "x\n");
        write(&tree.join("target/review-1/a.md"), "# review 1\n");
    });

    let item = fixture.assessment("binary-keep");
    assert_eq!(item["sections_stripped"], 0, "{item}");
}

/// Fidelity against Git itself: the set of paths the strip removes is exactly the set the tree's
/// own Git listed as ignored (no user excludes), across nested `.gitignore` files, negation,
/// `**`, directory-only and anchored patterns, a symlinked `.gitignore` Git does not follow, the
/// repository's `info/exclude`, and names Git C-quotes. A path with a leading `:` may stay. A
/// path only `info/exclude` ignores must stay: it may have changed since the archive was written.
#[test]
fn the_strip_removes_exactly_what_the_trees_git_ignored() {
    let fixture = Fixture::new(
        &[
            (
                ".gitignore",
                b"*.log\n!important.log\n/build/\ndocs/**/generated/\ncache/\ntmp\n**/out/*.bin\n",
            ),
            ("pkg/.gitignore", b"!special.log\n*.tmp\n"),
            ("deep/a/.gitignore", b"/local\n"),
            ("other.gitignore", b"*.txt\n"),
        ],
        &[("nest/.gitignore", "../other.gitignore")],
    );
    write(
        &fixture.repository.join(".git/info/exclude"),
        "/excluded-by-info/\n",
    );
    let names: &[&[u8]] = &[
        b"a.log",
        b"important.log",
        b"pkg/special.log",
        b"pkg/x.tmp",
        b"build/x",
        b"sub/build/x",
        b"docs/v1/generated/x.html",
        b"docs/generated/y",
        b"cache/x",
        b"sub/cache/y",
        b"tmp",
        b"sub/tmp/z",
        b"deep/a/local",
        b"deep/a/b/local",
        b"q/out/x.bin",
        b"q/out/deeper/x.bin",
        b"nest/x.txt",
        b"excluded-by-info/f",
        // A name holding a newline is refused by `archive` itself, so none is asked here.
        b"tab\there.log",
        b"quote\"d.log",
        b"glob[1].log",
        b"star*.txt",
        b"back\\slash.log",
        b"caf\xe9.log",
        b"plain\xff.txt",
        b":colon.log",
        b"sp ace/x.log",
    ];
    let truth: RefCell<BTreeSet<Vec<u8>>> = RefCell::new(BTreeSet::new());
    let archive = fixture.archived("fidelity", &|tree| {
        for name in names {
            let path = tree.join(std::ffi::OsStr::from_bytes(name));
            write(&path, b"content\n");
        }
        let output = git_output(
            tree,
            &[
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
            ],
            None,
        );
        assert!(output.status.success(), "{output:?}");
        *truth.borrow_mut() = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
    });
    let truth = truth.into_inner();
    assert!(truth.contains(b"a.log".as_slice()), "{truth:?}");
    assert!(!truth.contains(b"nest/x.txt".as_slice()), "{truth:?}");

    let before = applied_paths(&fixture.repository, &archive);
    let report = fixture.strip(&["--apply", "--id", "fidelity"]);
    assert_eq!(report["archives"][0]["applied"], true, "{report}");
    let after = applied_paths(&fixture.repository, &archive);
    let stripped = before.difference(&after).cloned().collect::<BTreeSet<_>>();
    let show = |set: &BTreeSet<Vec<u8>>| {
        set.iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect::<Vec<_>>()
    };
    let wrongly = stripped
        .difference(&truth)
        .cloned()
        .collect::<BTreeSet<_>>();
    assert!(
        wrongly.is_empty(),
        "stripped, not ignored: {:?}",
        show(&wrongly)
    );
    assert!(
        !stripped.contains(b"excluded-by-info/f".as_slice()),
        "stripped a path only info/exclude ignores"
    );
    let missed = truth
        .difference(&stripped)
        .filter(|path| !path.starts_with(b":") && !path.starts_with(b"excluded-by-info/"))
        .cloned()
        .collect::<BTreeSet<_>>();
    assert!(
        missed.is_empty(),
        "ignored, not stripped: {:?}",
        show(&missed)
    );
}

/// AGENTS.md: "A dry-run, the default, writes nothing." With the repository holding HEAD, the
/// dry-run before this unit read only; it now creates scratch work trees beside the archive, so a
/// read-only archive root turns a dry-run assessment into a refusal.
#[test]
fn a_dry_run_writes_nothing_beside_the_archive() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new(&[(".gitignore", IGNORE.as_bytes())], &[]);
    let archive = fixture.archived("read-only", &|tree| {
        write(&tree.join("website/node_modules/x/index.js"), "x\n");
    });
    let parent = archive.parent().unwrap().to_path_buf();
    let mode = std::fs::metadata(&parent).unwrap().permissions().mode();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let output = fixture.run(&[
        "prune-archives",
        "--strip-build-output",
        "--repo",
        fixture.repository.to_str().unwrap(),
    ]);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let item = report["archives"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["archive_directory"] == "read-only")
        .unwrap_or_else(|| panic!("{report}"))
        .clone();
    assert_eq!(item["verdict"], "strippable", "{item}");
}
