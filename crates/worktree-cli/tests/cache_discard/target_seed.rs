//! `create --seed-target` gives a new tree the dependency artifacts of a built checkout of the
//! same repository: copies, never links to the seed, of the `debug` profile's dependency units
//! only, from the checkout built last that the tree's rustc built and no Cargo is building in.
//! Without the option `create` is what it was.

use super::{Fixture, TAG, discarded, git, write};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

/// The variable that turns the option on for every `create`.
const VARIABLE: &str = "WORKTREE_SEED_TARGET";

/// A unit of the dependency: its library.
const DEP_LIB: &str = "1111111111111111";
/// A unit of the dependency: its build script, compiled.
const DEP_SCRIPT: &str = "2222222222222222";
/// A unit of the dependency: its build script, run.
const DEP_RUN: &str = "3333333333333333";
/// The unit of a dependency whose output holds a link that leaves its directory.
const ESCAPE: &str = "4444444444444444";
/// A member known only by its build script's dep-info.
const TOOL_SCRIPT: &str = "5555555555555555";
/// A member's binary.
const APP_BIN: &str = "aaaaaaaaaaaaaaaa";
/// The same member's build script run, which has no dep-info file.
const APP_RUN: &str = "bbbbbbbbbbbbbbbb";

/// The two layouts of a profile: Cargo before 1.100 and Cargo 1.100.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Stores,
    Units,
}

const LAYOUTS: [Layout; 2] = [Layout::Stores, Layout::Units];

fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000 + seconds)
}

fn set_modified(path: &Path, time: SystemTime) {
    File::open(path).unwrap().set_modified(time).unwrap();
}

/// What `rustc -vV` prints in `directory`, asked as `create` asks it.
fn rustc_verbose_version(directory: &Path) -> String {
    let rustc = std::env::var_os("RUSTC")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("-vV")
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

/// A `.rustc_info.json` as Cargo writes one, holding `verbose_version` as the output of
/// `rustc -vV` beside the output of another call.
fn rustc_info(verbose_version: &str) -> String {
    let output = |stdout: &str| {
        serde_json::json!({
            "success": true, "status": "", "code": 0, "stdout": stdout, "stderr": "",
        })
    };
    serde_json::json!({
        "rustc_fingerprint": 1_u64,
        "outputs": {"17": output("___\nlib___.rlib\n"), "23": output(verbose_version)},
        "successes": {},
    })
    .to_string()
}

const OTHER_RUSTC: &str = "rustc 1.0.0 (a59de37e9 2015-05-13)\nbinary: rustc\n\
    commit-hash: a59de37e99060162a2674e3ff45409ac73595c0e\ncommit-date: 2015-05-13\n\
    host: x86_64-unknown-linux-gnu\nrelease: 1.0.0\n";

/// Every entry below `root`, relative to it, sorted; a directory before what it holds. No
/// symbolic link is followed.
fn entries(root: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, relative: &Path, found: &mut Vec<PathBuf>) {
        let mut names: Vec<_> = std::fs::read_dir(root.join(relative))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        for name in names {
            let path = relative.join(name);
            let directory = std::fs::symlink_metadata(root.join(&path))
                .unwrap()
                .is_dir();
            found.push(path.clone());
            if directory {
                walk(root, &path, found);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, Path::new(""), &mut found);
    found
}

/// The files and symbolic links below `root`, as strings.
fn files(root: &Path) -> Vec<String> {
    entries(root)
        .into_iter()
        .filter(|path| !std::fs::symlink_metadata(root.join(path)).unwrap().is_dir())
        .map(|path| path.to_str().unwrap().to_owned())
        .collect()
}

/// Everything a `create` could change in a checkout's target: per entry its kind, size, times
/// of modification and of status change, inode, link count and content.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    entries(root)
        .into_iter()
        .map(|path| {
            let full = &root.join(&path);
            let metadata = std::fs::symlink_metadata(full).unwrap();
            let content = if metadata.is_file() {
                format!("{:?}", std::fs::read(full).unwrap())
            } else if metadata.is_symlink() {
                format!("-> {}", std::fs::read_link(full).unwrap().display())
            } else {
                "directory".to_owned()
            };
            let state = format!(
                "mode {:o} size {} mtime {}.{} ctime {}.{} inode {} links {} {content}",
                metadata.mode(),
                metadata.len(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.ino(),
                metadata.nlink(),
            );
            (path, state)
        })
        .collect()
}

/// Give every entry below `target` its own modification time in the past, as a build that
/// ended long ago leaves them, and the directories Cargo creates unit directories in the time
/// `built`.
fn age(target: &Path, built: SystemTime) {
    for (index, path) in entries(target).iter().enumerate() {
        let full = &target.join(path);
        if !std::fs::symlink_metadata(full).unwrap().is_symlink() {
            set_modified(full, at(u64::try_from(index).unwrap() * 10));
        }
    }
    set_built(target, built);
}

/// Set the time the profile last gained a unit directory: the modification time of
/// `.fingerprint/`, `build/` and each directory in `build/`.
fn set_built(target: &Path, built: SystemTime) {
    let units = &target.join("debug/build");
    let mut directories = vec![target.join("debug/.fingerprint"), units.clone()];
    if let Ok(packages) = std::fs::read_dir(units) {
        directories.extend(packages.map(|entry| entry.unwrap().path()));
    }
    for directory in directories {
        if directory.is_dir() {
            set_modified(&directory, built);
        }
    }
}

/// A built `target/` in `checkout`, in `layout`, whose `.rustc_info.json` names the rustc a
/// tree of this repository runs: the tag, a `debug` profile with the units of one dependency
/// (library, build script and its run), of a dependency whose output holds a link out of its
/// directory, and of two members; lock files, an uplifted binary, `incremental/`, `examples/`,
/// a `release` profile and `tmp/`. Built at `built`.
#[allow(clippy::too_many_lines)]
fn build_seed(checkout: &Path, layout: Layout, built: SystemTime) {
    let target = checkout.join("target");
    let put = |relative: &str, contents: &str| write(&target.join(relative), contents);
    put("CACHEDIR.TAG", TAG);
    put(
        ".rustc_info.json",
        &rustc_info(&rustc_verbose_version(checkout)),
    );
    put("tmp/scratch", "test scratch\n");
    put("debug/.cargo-lock", "");
    if layout == Layout::Units {
        put("debug/.cargo-build-lock", "");
        put("debug/.cargo-artifact-lock", "");
    }
    put("debug/app.d", "debug/app: app/src/main.rs\n");
    put(
        "debug/incremental/app-1x/s-1/dep-graph.bin",
        "incremental\n",
    );
    std::fs::create_dir_all(target.join("debug/examples")).unwrap();
    let registry = "/registry/dep-0.1.0/src/lib.rs";
    match layout {
        Layout::Stores => {
            put(
                "release/.fingerprint/dep-9999999999999999/lib-dep",
                "release\n",
            );
            put("release/deps/libdep-9999999999999999.rlib", "release\n");
            let fingerprint = |unit: &str, name: &str| {
                put(
                    &format!("debug/.fingerprint/{unit}/{name}"),
                    "fingerprint\n",
                );
            };
            fingerprint(&format!("dep-{DEP_LIB}"), "lib-dep");
            fingerprint(&format!("dep-{DEP_LIB}"), "dep-lib-dep");
            fingerprint(&format!("dep-{DEP_LIB}"), "invoked.timestamp");
            fingerprint(
                &format!("dep-{DEP_SCRIPT}"),
                "build-script-build-script-build",
            );
            fingerprint(
                &format!("dep-{DEP_RUN}"),
                "run-build-script-build-script-build",
            );
            fingerprint(&format!("escape-{ESCAPE}"), "lib-escape");
            fingerprint(
                &format!("tool-{TOOL_SCRIPT}"),
                "build-script-build-script-build",
            );
            fingerprint(&format!("app-{APP_BIN}"), "bin-app");
            fingerprint(
                &format!("app-{APP_RUN}"),
                "run-build-script-build-script-build",
            );
            put(
                &format!("debug/deps/libdep-{DEP_LIB}.rlib"),
                &"r".repeat(5000),
            );
            put(&format!("debug/deps/libdep-{DEP_LIB}.rmeta"), "metadata\n");
            put(
                &format!("debug/deps/dep-{DEP_LIB}.d"),
                &format!(
                    "{0}/debug/deps/libdep-{DEP_LIB}.rmeta: {registry} \
                     {0}/debug/build/dep-{DEP_RUN}/out/generated.rs\n",
                    target.display()
                ),
            );
            put(
                &format!("debug/deps/app-{APP_BIN}"),
                "the member's binary\n",
            );
            put(
                &format!("debug/deps/app-{APP_BIN}.d"),
                &format!(
                    "{}/debug/deps/app-{APP_BIN}: app/src/main.rs\n",
                    target.display()
                ),
            );
            let script = &format!("debug/build/dep-{DEP_SCRIPT}");
            put(
                &format!("{script}/build-script-build"),
                "the build script\n",
            );
            std::fs::hard_link(
                target.join(script).join("build-script-build"),
                target
                    .join(script)
                    .join(format!("build_script_build-{DEP_SCRIPT}")),
            )
            .unwrap();
            put(
                &format!("{script}/build_script_build-{DEP_SCRIPT}.d"),
                &format!(
                    "{}/{script}/build_script_build-{DEP_SCRIPT}: /registry/dep-0.1.0/build.rs\n",
                    target.display()
                ),
            );
            let run = &format!("debug/build/dep-{DEP_RUN}");
            for name in ["output", "root-output", "stderr", "invoked.timestamp"] {
                put(&format!("{run}/{name}"), "run\n");
            }
            put(
                &format!("{run}/out/generated.rs"),
                "pub const GENERATED: u32 = 7;\n",
            );
            put(&format!("{run}/out/libnative.so.1"), "native\n");
            std::os::unix::fs::symlink("libnative.so.1", target.join(run).join("out/libnative.so"))
                .unwrap();
            put(&format!("debug/build/escape-{ESCAPE}/out/data"), "data\n");
            std::os::unix::fs::symlink(
                target.join("tmp/scratch"),
                target.join(format!("debug/build/escape-{ESCAPE}/out/link")),
            )
            .unwrap();
            put(
                &format!("debug/build/tool-{TOOL_SCRIPT}/build_script_build-{TOOL_SCRIPT}.d"),
                &format!(
                    "{}/debug/build/tool-{TOOL_SCRIPT}/build_script_build-{TOOL_SCRIPT}: \
                     tool/build.rs\n",
                    target.display()
                ),
            );
            put(&format!("debug/build/app-{APP_RUN}/output"), "run\n");
            put(
                &format!("debug/build/app-{APP_RUN}/out/table.rs"),
                "generated\n",
            );
            // Cargo links a final artifact to the top of the profile, and rustc links a
            // library's metadata into `incremental/`: second names that are not copied.
            std::fs::hard_link(
                target.join(format!("debug/deps/app-{APP_BIN}")),
                target.join("debug/app"),
            )
            .unwrap();
            put("debug/incremental/dep-2x/s-1/keep", "incremental\n");
            std::fs::hard_link(
                target.join(format!("debug/deps/libdep-{DEP_LIB}.rmeta")),
                target.join("debug/incremental/dep-2x/s-1/metadata.rmeta"),
            )
            .unwrap();
        }
        Layout::Units => {
            put(
                "release/build/dep/9999999999999999/fingerprint/lib-dep",
                "release\n",
            );
            put(
                "release/build/dep/9999999999999999/out/libdep.rlib",
                "release\n",
            );
            let unit = |package: &str, unit: &str| format!("debug/build/{package}/{unit}");
            let library = &unit("dep", DEP_LIB);
            for name in [
                "lib-dep",
                "lib-dep.json",
                "dep-lib-dep",
                "invoked.timestamp",
            ] {
                put(&format!("{library}/fingerprint/{name}"), "fingerprint\n");
            }
            put(
                &format!("{library}/out/libdep-{DEP_LIB}.rlib"),
                &"r".repeat(5000),
            );
            put(
                &format!("{library}/out/libdep-{DEP_LIB}.rmeta"),
                "metadata\n",
            );
            put(
                &format!("{library}/out/dep-{DEP_LIB}.d"),
                &format!(
                    "{0}/{library}/out/libdep-{DEP_LIB}.rmeta: {registry} \
                     {0}/debug/build/dep/{DEP_RUN}/out/generated.rs\n",
                    target.display()
                ),
            );
            let script = &unit("dep", DEP_SCRIPT);
            put(
                &format!("{script}/fingerprint/build-script-build-script-build"),
                "fingerprint\n",
            );
            put(
                &format!("{script}/out/build_script_build"),
                "the build script\n",
            );
            put(
                &format!("{script}/out/build_script_build.d"),
                &format!(
                    "{}/{script}/out/build_script_build: /registry/dep-0.1.0/build.rs\n",
                    target.display()
                ),
            );
            let run = &unit("dep", DEP_RUN);
            put(
                &format!("{run}/fingerprint/run-build-script-build-script-build"),
                "fingerprint\n",
            );
            for name in ["stdout", "stderr", "root-output", "invoked.timestamp"] {
                put(&format!("{run}/run/{name}"), "run\n");
            }
            put(
                &format!("{run}/out/generated.rs"),
                "pub const GENERATED: u32 = 7;\n",
            );
            put(&format!("{run}/out/libnative.so.1"), "native\n");
            std::os::unix::fs::symlink("libnative.so.1", target.join(run).join("out/libnative.so"))
                .unwrap();
            // Two names of one file inside the dependency's own unit.
            std::fs::hard_link(
                target.join(run).join("out/libnative.so.1"),
                target.join(run).join("out/libnative.so.1.0"),
            )
            .unwrap();
            let escape = &unit("escape", ESCAPE);
            put(&format!("{escape}/fingerprint/lib-escape"), "fingerprint\n");
            put(&format!("{escape}/out/data"), "data\n");
            std::os::unix::fs::symlink(
                target.join("tmp/scratch"),
                target.join(escape).join("out/link"),
            )
            .unwrap();
            let tool = &unit("tool", TOOL_SCRIPT);
            put(
                &format!("{tool}/fingerprint/build-script-build-script-build"),
                "fingerprint\n",
            );
            put(
                &format!("{tool}/out/build_script_build.d"),
                &format!(
                    "{}/{tool}/out/build_script_build: tool/build.rs\n",
                    target.display()
                ),
            );
            let app = &unit("app", APP_BIN);
            put(&format!("{app}/fingerprint/bin-app"), "fingerprint\n");
            put(&format!("{app}/out/app"), "the member's binary\n");
            put(
                &format!("{app}/out/app.d"),
                &format!("{}/{app}/out/app.d: app/src/main.rs\n", target.display()),
            );
            let app_run = &unit("app", APP_RUN);
            put(
                &format!("{app_run}/fingerprint/run-build-script-build-script-build"),
                "fingerprint\n",
            );
            put(&format!("{app_run}/run/stdout"), "run\n");
            put(&format!("{app_run}/out/table.rs"), "generated\n");
            std::fs::hard_link(target.join(app).join("out/app"), target.join("debug/app")).unwrap();
            put("debug/incremental/dep-2x/s-1/keep", "incremental\n");
            std::fs::hard_link(
                target
                    .join(library)
                    .join(format!("out/libdep-{DEP_LIB}.rmeta")),
                target.join("debug/incremental/dep-2x/s-1/metadata.rmeta"),
            )
            .unwrap();
        }
    }
    age(&target, built);
}

/// The files a seed built by [`build_seed`] gives a new tree, relative to `target/`: the tag,
/// `.rustc_info.json` and the dependency's three units, with both names of the file that has
/// two.
fn expected_files(layout: Layout) -> Vec<String> {
    let mut expected: Vec<String> = match layout {
        Layout::Stores => vec![
            ".rustc_info.json".to_owned(),
            "CACHEDIR.TAG".to_owned(),
            format!("debug/.fingerprint/dep-{DEP_LIB}/dep-lib-dep"),
            format!("debug/.fingerprint/dep-{DEP_LIB}/invoked.timestamp"),
            format!("debug/.fingerprint/dep-{DEP_LIB}/lib-dep"),
            format!("debug/.fingerprint/dep-{DEP_SCRIPT}/build-script-build-script-build"),
            format!("debug/.fingerprint/dep-{DEP_RUN}/run-build-script-build-script-build"),
            format!("debug/build/dep-{DEP_SCRIPT}/build-script-build"),
            format!("debug/build/dep-{DEP_SCRIPT}/build_script_build-{DEP_SCRIPT}"),
            format!("debug/build/dep-{DEP_SCRIPT}/build_script_build-{DEP_SCRIPT}.d"),
            format!("debug/build/dep-{DEP_RUN}/invoked.timestamp"),
            format!("debug/build/dep-{DEP_RUN}/out/generated.rs"),
            format!("debug/build/dep-{DEP_RUN}/out/libnative.so"),
            format!("debug/build/dep-{DEP_RUN}/out/libnative.so.1"),
            format!("debug/build/dep-{DEP_RUN}/output"),
            format!("debug/build/dep-{DEP_RUN}/root-output"),
            format!("debug/build/dep-{DEP_RUN}/stderr"),
            format!("debug/deps/dep-{DEP_LIB}.d"),
            format!("debug/deps/libdep-{DEP_LIB}.rlib"),
            format!("debug/deps/libdep-{DEP_LIB}.rmeta"),
        ],
        Layout::Units => vec![
            ".rustc_info.json".to_owned(),
            "CACHEDIR.TAG".to_owned(),
            format!("debug/build/dep/{DEP_LIB}/fingerprint/dep-lib-dep"),
            format!("debug/build/dep/{DEP_LIB}/fingerprint/invoked.timestamp"),
            format!("debug/build/dep/{DEP_LIB}/fingerprint/lib-dep"),
            format!("debug/build/dep/{DEP_LIB}/fingerprint/lib-dep.json"),
            format!("debug/build/dep/{DEP_LIB}/out/dep-{DEP_LIB}.d"),
            format!("debug/build/dep/{DEP_LIB}/out/libdep-{DEP_LIB}.rlib"),
            format!("debug/build/dep/{DEP_LIB}/out/libdep-{DEP_LIB}.rmeta"),
            format!("debug/build/dep/{DEP_SCRIPT}/fingerprint/build-script-build-script-build"),
            format!("debug/build/dep/{DEP_SCRIPT}/out/build_script_build"),
            format!("debug/build/dep/{DEP_SCRIPT}/out/build_script_build.d"),
            format!("debug/build/dep/{DEP_RUN}/fingerprint/run-build-script-build-script-build"),
            format!("debug/build/dep/{DEP_RUN}/out/generated.rs"),
            format!("debug/build/dep/{DEP_RUN}/out/libnative.so"),
            format!("debug/build/dep/{DEP_RUN}/out/libnative.so.1"),
            format!("debug/build/dep/{DEP_RUN}/out/libnative.so.1.0"),
            format!("debug/build/dep/{DEP_RUN}/run/invoked.timestamp"),
            format!("debug/build/dep/{DEP_RUN}/run/root-output"),
            format!("debug/build/dep/{DEP_RUN}/run/stderr"),
            format!("debug/build/dep/{DEP_RUN}/run/stdout"),
        ],
    };
    expected.sort();
    expected
}

/// The two names of one file among [`expected_files`].
fn linked_pair(layout: Layout) -> [String; 2] {
    match layout {
        Layout::Stores => [
            format!("debug/build/dep-{DEP_SCRIPT}/build-script-build"),
            format!("debug/build/dep-{DEP_SCRIPT}/build_script_build-{DEP_SCRIPT}"),
        ],
        Layout::Units => [
            format!("debug/build/dep/{DEP_RUN}/out/libnative.so.1"),
            format!("debug/build/dep/{DEP_RUN}/out/libnative.so.1.0"),
        ],
    }
}

/// `worktree` in the fixture's state, with the option's variable unset whatever the caller's
/// environment holds.
fn worktree(fixture: &Fixture, json: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_worktree"));
    if json {
        command.arg("--json");
    }
    command
        .env("XDG_CONFIG_HOME", fixture.root.path().join("config"))
        .env("XDG_STATE_HOME", fixture.root.path().join("state"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove(VARIABLE);
    command
}

fn create_command(fixture: &Fixture, json: bool, id: &str, extra: &[&str]) -> Command {
    let mut command = worktree(fixture, json);
    command
        .args(["create", "--repo", fixture.repository.to_str().unwrap()])
        .args(["--id", id, "--purpose", "target seed"])
        .args(extra);
    command
}

fn succeeded(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// `create --seed-target --id <id>`, as JSON.
fn create_seeded(fixture: &Fixture, id: &str) -> Value {
    succeeded(
        &create_command(fixture, true, id, &["--seed-target"])
            .output()
            .unwrap(),
    )
}

fn tree_of(created: &Value) -> PathBuf {
    PathBuf::from(created["evidence"]["path"].as_str().unwrap())
}

/// The record of the tree `id`, as `status` reports it.
fn record(fixture: &Fixture, id: &str) -> Value {
    let status = succeeded(&worktree(fixture, true).arg("status").output().unwrap());
    status["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == id)
        .unwrap()
        .clone()
}

/// A create that copied nothing: the tree exists and is active, has no `target`, and the JSON
/// and the record carry the reason's code with one line. Returns the line.
fn skipped(fixture: &Fixture, created: &Value, id: &str, code: &str) -> String {
    let tree = tree_of(created);
    assert!(tree.join("source").is_file(), "{created}");
    let seed = &created["seed"];
    assert_eq!(seed["skipped"], code, "{created}");
    assert_eq!(seed["files"], 0, "{created}");
    assert_eq!(seed["bytes"], 0, "{created}");
    assert!(seed.get("seed").is_none(), "{created}");
    let reason = seed["reason"].as_str().unwrap().to_owned();
    assert!(!reason.is_empty() && !reason.contains('\n'), "{reason:?}");
    let record = record(fixture, id);
    assert_eq!(record["lifecycle"], "active");
    assert_eq!(&record["seed"], seed);
    reason
}

#[test]
fn a_seed_gives_the_tag_the_rustc_info_and_the_dependency_units_of_debug_only() {
    for layout in LAYOUTS {
        let fixture = Fixture::new();
        build_seed(&fixture.repository, layout, at(100_000));

        let created = create_seeded(&fixture, "seeded");

        let tree = tree_of(&created);
        // Neither `incremental/`, `examples/`, a lock file, the binary at the top of the
        // profile, `release/`, `tmp/`, a member's unit (`app`, `tool`) nor the unit whose link
        // leaves its directory (`escape`) is among them.
        assert_eq!(
            files(&tree.join("target")),
            expected_files(layout),
            "{layout:?}"
        );
        assert_eq!(
            created["seed"]["seed"],
            fixture.repository.to_str().unwrap(),
            "{created}"
        );
        for name in ["debug/examples", "debug/incremental", "release", "tmp"] {
            assert!(
                !tree.join("target").join(name).exists(),
                "{layout:?} {name}"
            );
        }
        // The seed did not make the new tree differ from HEAD: `target/` is ignored.
        assert_eq!(git(&tree, &["status", "--porcelain"]), "", "{layout:?}");
    }
}

#[test]
fn copied_files_keep_their_times_and_share_no_file_with_the_seed() {
    for layout in LAYOUTS {
        let fixture = Fixture::new();
        build_seed(&fixture.repository, layout, at(100_000));
        let seed = fixture.repository.join("target");
        let seed_inodes: BTreeSet<(u64, u64)> = entries(&seed)
            .iter()
            .map(|path| std::fs::symlink_metadata(seed.join(path)).unwrap())
            .map(|metadata| (metadata.dev(), metadata.ino()))
            .collect();

        let created = create_seeded(&fixture, "seeded");

        let target = tree_of(&created).join("target");
        let pair = linked_pair(layout);
        for path in &entries(&target) {
            let copy = std::fs::symlink_metadata(target.join(path)).unwrap();
            let original = std::fs::symlink_metadata(seed.join(path)).unwrap();
            let name = path.to_str().unwrap();
            assert_eq!(copy.file_type(), original.file_type(), "{layout:?} {name}");
            if copy.is_symlink() {
                assert_eq!(
                    std::fs::read_link(target.join(path)).unwrap(),
                    Path::new("libnative.so.1"),
                    "{layout:?} {name}"
                );
                continue;
            }
            // Cargo compares modification times; `debug` is the outermost directory that
            // keeps the seed's.
            if name != "debug" || copy.is_file() {
                assert_eq!(
                    copy.modified().unwrap(),
                    original.modified().unwrap(),
                    "{layout:?} {name}"
                );
            }
            // No name in the new tree is a hard link to a file of the seed.
            assert!(
                !seed_inodes.contains(&(copy.dev(), copy.ino())),
                "{layout:?} {name} is the seed's own file"
            );
            if copy.is_file() {
                assert_eq!(
                    std::fs::read(target.join(path)).unwrap(),
                    std::fs::read(seed.join(path)).unwrap(),
                    "{layout:?} {name}"
                );
                assert_eq!(copy.mode(), original.mode(), "{layout:?} {name}");
                // Two names of one file in the seed are one file here; a file whose second
                // name was not copied (the metadata rustc links into `incremental/`) has one.
                let links = if pair.iter().any(|linked| linked == name) {
                    2
                } else {
                    1
                };
                assert_eq!(copy.nlink(), links, "{layout:?} {name}");
            }
        }
        let [first, second] = pair.map(|name| std::fs::metadata(target.join(name)).unwrap());
        assert_eq!(first.ino(), second.ino(), "{layout:?}");
        // The seed's file with a second name outside the copy set did have two names.
        let metadata_file = expected_files(layout)
            .into_iter()
            .find(|name| {
                Path::new(name)
                    .extension()
                    .is_some_and(|kind| kind == "rmeta")
            })
            .unwrap();
        assert_eq!(
            std::fs::metadata(seed.join(metadata_file)).unwrap().nlink(),
            2
        );
    }
}

#[test]
fn the_record_and_the_json_name_the_seed_its_files_and_its_bytes() {
    for layout in LAYOUTS {
        let fixture = Fixture::new();
        build_seed(&fixture.repository, layout, at(100_000));

        let created = create_seeded(&fixture, "seeded");

        let target = tree_of(&created).join("target");
        let names = files(&target);
        // A second name of a file already copied adds no bytes; a symbolic link has none.
        let bytes: u64 = names
            .iter()
            .map(|name| std::fs::symlink_metadata(target.join(name)).unwrap())
            .filter(std::fs::Metadata::is_file)
            .map(|metadata| (metadata.ino(), metadata.len()))
            .collect::<BTreeMap<u64, u64>>()
            .values()
            .sum();
        let expected = serde_json::json!({
            "seed": fixture.repository.to_str().unwrap(),
            "files": names.len(),
            "bytes": bytes,
        });
        assert_eq!(created["seed"], expected, "{layout:?}");
        assert_eq!(record(&fixture, "seeded")["seed"], expected, "{layout:?}");
        // The tree made at fixture start, without the option, has no seed in its record.
        assert!(record(&fixture, "cache").get("seed").is_none());

        // Without `--json` the path stays alone on standard output and the seed is one line
        // on standard error.
        let plain = create_command(&fixture, false, "plain", &["--seed-target"])
            .output()
            .unwrap();
        assert!(plain.status.success());
        let path = String::from_utf8(plain.stdout).unwrap();
        assert!(
            Path::new(path.trim_end())
                .join("target/CACHEDIR.TAG")
                .is_file()
        );
        assert_eq!(path.lines().count(), 1);
        assert_eq!(
            String::from_utf8(plain.stderr).unwrap(),
            format!(
                "target seeded from {}: {} files, {bytes} bytes\n",
                fixture.repository.display(),
                names.len()
            ),
            "{layout:?}"
        );
    }
}

#[test]
fn a_create_leaves_the_seed_as_it_was() {
    for layout in LAYOUTS {
        let fixture = Fixture::new();
        build_seed(&fixture.repository, layout, at(100_000));
        let before = snapshot(&fixture.repository.join("target"));

        let created = create_seeded(&fixture, "seeded");

        assert_eq!(
            created["seed"]["seed"],
            fixture.repository.to_str().unwrap()
        );
        let after = snapshot(&fixture.repository.join("target"));
        // No entry appeared (a lock file Cargo had not written is not created), none went,
        // and none changed in size, content, link count or time of modification or of status
        // change.
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>(),
            "{layout:?}"
        );
        for (path, state) in &before {
            assert_eq!(state, &after[path], "{layout:?} {}", path.display());
        }
        if layout == Layout::Stores {
            assert!(
                !fixture
                    .repository
                    .join("target/debug/.cargo-build-lock")
                    .exists()
            );
        }
    }
}

#[test]
fn discard_cache_names_a_seeded_target_a_cargo_target() {
    for layout in LAYOUTS {
        let fixture = Fixture::new();
        build_seed(&fixture.repository, layout, at(100_000));
        let created = create_seeded(&fixture, "seeded");
        let tree = tree_of(&created);

        let review = fixture.ok(&["discard-cache", tree.to_str().unwrap(), "--dry-run"]);

        assert_eq!(
            discarded(&review["cache"]),
            [("target".to_owned(), "cargo-target".to_owned())],
            "{layout:?} {review}"
        );
        assert!(tree.join("target/CACHEDIR.TAG").is_file());
    }
}

#[test]
fn without_the_option_create_copies_nothing_and_prints_nothing_new() {
    let fixture = Fixture::new();
    build_seed(&fixture.repository, Layout::Units, at(100_000));

    // As JSON: the envelope `create` always printed, and nothing on standard error.
    let output = create_command(&fixture, true, "plain", &[])
        .output()
        .unwrap();
    let created = succeeded(&output);
    assert_eq!(output.stderr, b"");
    let mut keys: Vec<&String> = created.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["evidence", "ok", "version"]);
    assert_eq!(created["version"], 4);
    let mut evidence: Vec<&String> = created["evidence"].as_object().unwrap().keys().collect();
    evidence.sort();
    assert_eq!(
        evidence,
        ["head", "id", "operation", "path", "recorded_at", "recovery"]
    );
    assert!(!tree_of(&created).join("target").exists());
    assert!(record(&fixture, "plain").get("seed").is_none());

    // Without `--json`: the path, and nothing on standard error.
    let output = create_command(&fixture, false, "human", &[])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stderr, b"");
    let path = String::from_utf8(output.stdout).unwrap();
    assert_eq!(path.lines().count(), 1);
    assert!(Path::new(path.trim_end()).join("source").is_file());
    assert!(!Path::new(path.trim_end()).join("target").exists());

    // The variable switches the option on only as `1` or `true`.
    for (value, id, seeded) in [
        ("0", "zero", false),
        ("", "empty", false),
        ("yes", "yes", false),
        ("1", "one", true),
        ("true", "true", true),
    ] {
        let output = create_command(&fixture, true, id, &[])
            .env(VARIABLE, value)
            .output()
            .unwrap();
        let created = succeeded(&output);
        assert_eq!(output.stderr, b"", "{VARIABLE}={value}");
        assert_eq!(
            created.get("seed").is_some(),
            seeded,
            "{VARIABLE}={value} {created}"
        );
        assert_eq!(
            tree_of(&created).join("target/CACHEDIR.TAG").is_file(),
            seeded,
            "{VARIABLE}={value}"
        );
    }
}

#[test]
fn the_seed_is_the_checkout_built_last_that_passes_the_checks() {
    let fixture = Fixture::new();
    let primary = fixture.repository.clone();
    let managed = fixture.tree.clone();
    build_seed(&primary, Layout::Stores, at(200_000));
    build_seed(&managed, Layout::Units, at(300_000));
    // The seed of a new tree; the tree's own copy is removed, so that the two checkouts built
    // above stay the only built ones.
    let seed_of = |id: &str| {
        let created = create_seeded(&fixture, id);
        std::fs::remove_dir_all(tree_of(&created).join("target")).unwrap();
        PathBuf::from(created["seed"]["seed"].as_str().unwrap())
    };

    // The managed tree's profile gained a unit directory last.
    assert_eq!(seed_of("first"), managed);

    // Once the primary checkout gains a unit directory later, it is the seed: the time of one
    // directory Cargo creates unit directories in decides.
    set_modified(&primary.join("target/debug/.fingerprint"), at(400_000));
    assert_eq!(seed_of("second"), primary);

    // A newest checkout that fails a check is passed over for the next one: built by another
    // rustc, then with its profile lock held by a build.
    write(
        &primary.join("target/.rustc_info.json"),
        &rustc_info(OTHER_RUSTC),
    );
    assert_eq!(seed_of("third"), managed);
    write(
        &primary.join("target/.rustc_info.json"),
        &rustc_info(&rustc_verbose_version(&primary)),
    );
    assert_eq!(seed_of("fourth"), primary);
    let lock = File::open(primary.join("target/debug/.cargo-lock")).unwrap();
    lock.lock().unwrap();
    assert_eq!(seed_of("fifth"), managed);
    drop(lock);
    assert_eq!(seed_of("sixth"), primary);

    // Checkouts built at the same time: the primary checkout before a managed tree.
    set_built(&managed.join("target"), at(400_000));
    assert_eq!(seed_of("seventh"), primary);

    // A tree seeded earlier is a checkout like any other once it was built later.
    let created = create_seeded(&fixture, "eighth");
    let eighth = tree_of(&created);
    set_built(&eighth.join("target"), at(500_000));
    assert_eq!(seed_of("ninth"), eighth);
    // Which records are checkouts of the repository (active and finished trees, not removed
    // ones or another repository's) is the facade's own test.
}

#[test]
fn with_no_built_checkout_create_says_so_in_one_line() {
    let fixture = Fixture::new();
    // A `target/` Cargo did not build: a tag, and a profile holding `.fingerprint/` alone.
    write(&fixture.repository.join("target/CACHEDIR.TAG"), TAG);
    write(
        &fixture
            .repository
            .join("target/debug/.fingerprint/one/lib-one"),
        "fingerprint\n",
    );
    // A built profile below a `target/` without Cargo's tag.
    fixture.profile("target/debug");

    let created = create_seeded(&fixture, "seeded");

    let reason = skipped(&fixture, &created, "seeded", "no-built-checkout");
    assert!(
        reason.starts_with("no built checkout: none of 2 checkouts"),
        "{reason}"
    );
    assert!(!tree_of(&created).join("target").exists());

    let plain = create_command(&fixture, false, "plain", &["--seed-target"])
        .output()
        .unwrap();
    assert!(plain.status.success());
    assert_eq!(String::from_utf8(plain.stdout).unwrap().lines().count(), 1);
    assert_eq!(
        String::from_utf8(plain.stderr).unwrap(),
        // The tree `seeded` is a third checkout by now.
        format!(
            "target not seeded: {}\n",
            reason.replace("none of 2", "none of 3")
        )
    );
}

#[test]
fn a_seed_without_rustc_info_or_built_by_another_rustc_is_not_used() {
    let fixture = Fixture::new();
    build_seed(&fixture.repository, Layout::Units, at(100_000));
    let info = fixture.repository.join("target/.rustc_info.json");

    // Cargo writes no `.rustc_info.json` when it runs with an empty `RUSTC_WRAPPER`: such a
    // checkout is no seed, and the line says so.
    std::fs::remove_file(&info).unwrap();
    let created = create_seeded(&fixture, "missing");
    let reason = skipped(&fixture, &created, "missing", "rustc-info-missing");
    assert!(reason.starts_with(info.to_str().unwrap()), "{reason}");
    assert!(reason.contains("RUSTC_WRAPPER is empty"), "{reason}");
    assert!(!tree_of(&created).join("target").exists());

    write(&info, &rustc_info(OTHER_RUSTC));
    let created = create_seeded(&fixture, "other");
    let reason = skipped(&fixture, &created, "other", "rustc-mismatch");
    let running = rustc_verbose_version(&fixture.repository);
    assert_eq!(
        reason,
        format!(
            "{} names rustc 1.0.0 (a59de37e9 2015-05-13), the new tree runs {}",
            info.display(),
            running.lines().next().unwrap()
        )
    );
    assert!(!tree_of(&created).join("target").exists());

    // A file that is no such document names no rustc.
    write(&info, "{}");
    let created = create_seeded(&fixture, "empty");
    let reason = skipped(&fixture, &created, "empty", "rustc-mismatch");
    assert!(reason.contains("names no rustc version"), "{reason}");

    // The new tree's own rustc cannot be asked: nothing is copied from a seed that matches.
    write(&info, &rustc_info(&running));
    let output = create_command(&fixture, true, "norustc", &["--seed-target"])
        .env("RUSTC", fixture.root.path().join("no-such-rustc"))
        .output()
        .unwrap();
    let created = succeeded(&output);
    let reason = skipped(&fixture, &created, "norustc", "rustc-unavailable");
    assert!(
        reason.starts_with("rustc -vV could not be run in the new tree"),
        "{reason}"
    );
    assert!(!tree_of(&created).join("target").exists());
}

#[test]
fn a_seed_whose_profile_lock_a_build_holds_is_not_used() {
    for (layout, name) in [
        (Layout::Stores, ".cargo-lock"),
        (Layout::Units, ".cargo-lock"),
        (Layout::Units, ".cargo-build-lock"),
        (Layout::Units, ".cargo-artifact-lock"),
    ] {
        let fixture = Fixture::new();
        build_seed(&fixture.repository, layout, at(100_000));
        let path = fixture.repository.join("target/debug").join(name);
        let before = snapshot(&fixture.repository.join("target"));
        // What a building Cargo holds: an exclusive lock on the open file.
        let lock = File::open(&path).unwrap();
        lock.lock().unwrap();

        let created = create_seeded(&fixture, "locked");

        let reason = skipped(&fixture, &created, "locked", "lock-held");
        assert_eq!(
            reason,
            format!("{} is locked: a Cargo is building there", path.display())
        );
        assert!(!tree_of(&created).join("target").exists());
        assert_eq!(before, snapshot(&fixture.repository.join("target")));

        // A shared lock, as another `create` reading the same seed holds one, refuses nothing.
        drop(lock);
        let shared = File::open(&path).unwrap();
        shared.lock_shared().unwrap();
        let created = create_seeded(&fixture, "shared");
        assert_eq!(
            created["seed"]["seed"],
            fixture.repository.to_str().unwrap()
        );
        drop(shared);

        // Once the build ended the same checkout is the seed, and `create` kept no lock.
        let created = create_seeded(&fixture, "free");
        assert_eq!(
            created["seed"]["seed"],
            fixture.repository.to_str().unwrap()
        );
        File::open(path).unwrap().try_lock().unwrap();
    }
}

#[test]
fn a_tree_that_holds_a_target_or_does_not_ignore_it_is_not_seeded() {
    // HEAD tracks a file below `target/`, so the new tree already has the directory.
    let fixture = Fixture::tracking(&[("target/records/note.md", "a tracked record\n")]);
    build_seed(&fixture.repository, Layout::Units, at(100_000));
    let created = create_seeded(&fixture, "tracked");
    let reason = skipped(&fixture, &created, "tracked", "target-exists");
    assert_eq!(reason, "the new tree already holds target");
    assert_eq!(
        files(&tree_of(&created).join("target")),
        ["records/note.md"],
        "{created}"
    );

    // HEAD no longer ignores `target/`: a seed would leave the tree with untracked files.
    let fixture = Fixture::new();
    write(&fixture.repository.join(".gitignore"), "/build\n");
    git(
        &fixture.repository,
        &["commit", "-am", "no longer ignore target"],
    );
    build_seed(&fixture.repository, Layout::Units, at(100_000));
    let created = create_seeded(&fixture, "unignored");
    let reason = skipped(&fixture, &created, "unignored", "target-not-ignored");
    assert_eq!(reason, "Git does not ignore target/ in the new tree");
    let tree = tree_of(&created);
    assert!(!tree.join("target").exists());
    assert_eq!(git(&tree, &["status", "--porcelain"]), "");
}

#[test]
fn a_seed_holding_only_the_workspaces_own_units_is_not_used() {
    for layout in LAYOUTS {
        let fixture = Fixture::new();
        build_seed(&fixture.repository, layout, at(100_000));
        let debug = fixture.repository.join("target/debug");
        // Remove every unit but the members': what a workspace without dependencies builds.
        match layout {
            Layout::Stores => {
                for store in [".fingerprint", "build", "deps"] {
                    for entry in std::fs::read_dir(debug.join(store)).unwrap() {
                        let entry = entry.unwrap();
                        let name = entry.file_name().into_string().unwrap();
                        if name.contains("dep-") || name.contains("escape-") {
                            if entry.file_type().unwrap().is_dir() {
                                std::fs::remove_dir_all(entry.path()).unwrap();
                            } else {
                                std::fs::remove_file(entry.path()).unwrap();
                            }
                        }
                    }
                }
            }
            Layout::Units => {
                std::fs::remove_dir_all(debug.join("build/dep")).unwrap();
                std::fs::remove_dir_all(debug.join("build/escape")).unwrap();
            }
        }

        let created = create_seeded(&fixture, "seeded");

        let reason = skipped(&fixture, &created, "seeded", "no-dependency-units");
        assert_eq!(
            reason,
            format!(
                "{} holds no dependency unit in its debug profile",
                fixture.repository.join("target").display()
            ),
            "{layout:?}"
        );
        assert!(!tree_of(&created).join("target").exists(), "{layout:?}");
    }
}

/// The dependency: a crate with a build script in a directory source outside every checkout,
/// so that its package id names the registry and never a checkout.
fn vendor_dependency(vendor: &Path) {
    let dep = vendor.join("dep");
    write(
        &dep.join("Cargo.toml"),
        "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
    );
    write(
        &dep.join("build.rs"),
        "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
         std::fs::write(format!(\"{out}/generated.rs\"), \"pub const GENERATED: u32 = 7;\\n\")\
         .unwrap();\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n",
    );
    write(
        &dep.join("src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n\
         pub fn dep() -> u32 {\n    GENERATED\n}\n",
    );
    write(
        &dep.join(".cargo-checksum.json"),
        "{\"files\":{},\"package\":null}",
    );
}

/// A workspace of two members, `app` and `core-lib`, whose one dependency comes from the
/// directory source `vendor`.
fn cargo_workspace(checkout: &Path, vendor: &Path) {
    write(
        &checkout.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"core\"]\n",
    );
    write(
        &checkout.join(".cargo/config.toml"),
        &format!(
            "[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\n\
             directory = {:?}\n",
            vendor.to_str().unwrap()
        ),
    );
    write(
        &checkout.join("core/Cargo.toml"),
        "[package]\nname = \"core-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ndep = \"0.1\"\n",
    );
    write(
        &checkout.join("core/src/lib.rs"),
        "pub fn core() -> u32 {\n    dep::dep() + 1\n}\n",
    );
    write(
        &checkout.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ncore-lib = { path = \"../core\" }\n",
    );
    write(
        &checkout.join("app/src/main.rs"),
        "fn main() {\n    println!(\"{}\", core_lib::core());\n}\n",
    );
}

/// The Cargo that runs this test, offline, in `directory`, with the environment the other
/// tests of this crate give a Cargo they run on a fixture of their own.
fn cargo(directory: &Path, args: &[&str]) -> Output {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(args)
        .arg("--offline")
        .current_dir(directory)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_BUILD_BUILD_DIR")
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// What one `cargo build -v` did: the packages per status word (`Fresh`, `Compiling`, `Dirty`)
/// and the crates rustc ran for, with `only_rustc` false once Cargo ran anything but rustc.
struct Build {
    packages: BTreeMap<String, BTreeSet<String>>,
    compiled: BTreeSet<String>,
    only_rustc: bool,
}

impl Build {
    fn of(directory: &Path) -> Self {
        let output = cargo(directory, &["build", "-v"]);
        let mut build = Self {
            packages: BTreeMap::new(),
            compiled: BTreeSet::new(),
            only_rustc: true,
        };
        for line in String::from_utf8(output.stderr).unwrap().lines() {
            let Some((status, rest)) = line.trim_start().split_once(' ') else {
                continue;
            };
            match status {
                "Fresh" | "Compiling" | "Dirty" => {
                    let package = rest.split(' ').next().unwrap().to_owned();
                    build
                        .packages
                        .entry(status.to_owned())
                        .or_default()
                        .insert(package);
                }
                "Running" => {
                    if let Some((_, arguments)) = rest.split_once("--crate-name ") {
                        let name = arguments.split(' ').next().unwrap().to_owned();
                        build.compiled.insert(name);
                    } else {
                        build.only_rustc = false;
                    }
                }
                _ => {}
            }
        }
        build
    }

    fn packages(&self, status: &str) -> Vec<&str> {
        self.packages
            .get(status)
            .map(|packages| packages.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

const NONE: [&str; 0] = [];

/// With the Cargo of the pinned toolchain: the first build in a seeded tree compiles the
/// workspace's members and nothing else, and a member whose source differs from what the seed
/// built is built from the tree's own source, however old its files are.
#[test]
fn the_first_build_in_a_seeded_tree_compiles_the_members_only() {
    let fixture = Fixture::new();
    let vendor = fixture.root.path().join("vendor");
    vendor_dependency(&vendor);
    let seed = fixture.repository.clone();
    cargo_workspace(&seed, &vendor);
    cargo(&seed, &["generate-lockfile"]);
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "a cargo workspace"]);

    let built = Build::of(&seed);
    assert_eq!(built.packages("Compiling"), ["app", "core-lib", "dep"]);
    // `cargo` here runs with an empty `RUSTC_WRAPPER`, as every Cargo this crate's tests run
    // on a fixture, and such a Cargo writes no `.rustc_info.json`. The fixture writes the file
    // a Cargo with a wrapper, or with none configured, leaves beside its build.
    let info = seed.join("target/.rustc_info.json");
    if !info.exists() {
        write(&info, &rustc_info(&rustc_verbose_version(&seed)));
    }
    assert_eq!(git(&seed, &["status", "--porcelain"]), "");
    let before = snapshot(&seed.join("target"));

    // The tree holds the dependency's units and none of a member's.
    let created = create_seeded(&fixture, "fresh");
    assert_eq!(created["seed"]["seed"], seed.to_str().unwrap(), "{created}");
    let tree = tree_of(&created);
    let copied = files(&tree.join("target"));
    let named = |prefixes: &[&str]| {
        copied
            .iter()
            .filter(|path| {
                path.split('/')
                    .any(|part| prefixes.iter().any(|prefix| part.starts_with(prefix)))
            })
            .count()
    };
    assert!(named(&["dep", "libdep"]) > 0, "{copied:?}");
    assert_eq!(
        named(&["app", "core-lib", "core_lib", "libcore_lib", "incremental"]),
        0,
        "{copied:?}"
    );

    let first = Build::of(&tree);
    assert_eq!(first.packages("Fresh"), ["dep"]);
    assert_eq!(first.packages("Compiling"), ["app", "core-lib"]);
    assert_eq!(first.packages("Dirty"), NONE);
    assert_eq!(
        first
            .compiled
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["app", "core_lib"]
    );
    assert!(first.only_rustc, "the dependency's build script ran again");
    let run = |tree: &Path| {
        let output = Command::new(tree.join("target/debug/app"))
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(run(&tree), "8\n");
    let second = Build::of(&tree);
    assert_eq!(second.packages("Fresh"), ["app", "core-lib", "dep"]);
    assert_eq!(second.packages("Compiling"), NONE);

    // A member's source that differs from the seed's, in files older than every artifact
    // copied: the member is built from the tree's source. The seed is the tree built above,
    // whose profile gained the members' unit directories last; none of them is copied.
    let built_tree = snapshot(&tree.join("target"));
    let created = create_seeded(&fixture, "changed");
    assert_eq!(created["seed"]["seed"], tree.to_str().unwrap(), "{created}");
    assert_eq!(created["seed"]["files"], copied.len(), "{created}");
    let changed = tree_of(&created);
    assert_eq!(files(&changed.join("target")), copied);
    write(
        &changed.join("core/src/lib.rs"),
        "pub fn core() -> u32 {\n    dep::dep() + 2\n}\n",
    );
    for file in [
        "core/src/lib.rs",
        "core/Cargo.toml",
        "app/src/main.rs",
        "app/Cargo.toml",
    ] {
        set_modified(&changed.join(file), at(0));
    }
    let rebuilt = Build::of(&changed);
    assert_eq!(rebuilt.packages("Fresh"), ["dep"]);
    assert_eq!(rebuilt.packages("Compiling"), ["app", "core-lib"]);
    assert_eq!(run(&changed), "9\n");

    // Neither create nor the builds in the two trees wrote into either seed.
    assert_eq!(before, snapshot(&seed.join("target")));
    assert_eq!(built_tree, snapshot(&tree.join("target")));
}
