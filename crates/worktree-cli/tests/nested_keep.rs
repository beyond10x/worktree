//! A `.worktreekeep` in a subdirectory keeps paths under that directory, its patterns matched
//! relative to it as Git reads a nested `.gitignore`. `discard-cache`, `sweep` and `finish
//! --discard-cache --archive` keep what it names, `archive` images it, and `prune-archives
//! --strip-build-output` never strips it. A nested keep file reaches nothing outside its own
//! directory.
#![cfg(unix)]

mod keep {
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

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
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim_end().into()
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const IGNORE: &str = "/tool/\n/x\n/other/\n";

    struct Fixture {
        root: tempfile::TempDir,
        repository: PathBuf,
    }

    impl Fixture {
        fn new(expire: u32) -> Self {
            Self::committing(expire, &[])
        }

        /// A fixture whose commit also holds `files`, force-added past `.gitignore`.
        fn committing(expire: u32, files: &[(&str, &str)]) -> Self {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            let repository = workspace.join("one");
            std::fs::create_dir_all(&repository).unwrap();
            git(&repository, &["init", "-b", "main"]);
            write(&repository.join("source"), "original\n");
            write(&repository.join(".gitignore"), IGNORE);
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
                &format!(
                    "version = 1\nname = 'test'\nexpire_after_seconds = {expire}\nprotect_workspace_root = false\n"
                ),
            );
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

        fn run(&self, json: bool, args: &[&str]) -> Output {
            let mut command = Command::new(env!("CARGO_BIN_EXE_worktree"));
            if json {
                command.arg("--json");
            }
            command
                .args(args)
                .env("XDG_CONFIG_HOME", self.root.path().join("config"))
                .env("XDG_STATE_HOME", self.root.path().join("state"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap()
        }

        fn ok(&self, args: &[&str]) -> Value {
            let output = self.run(true, args);
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        }

        fn refused(&self, args: &[&str]) -> Value {
            let output = self.run(true, args);
            assert!(
                !output.status.success(),
                "expected a refusal from {args:?}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            serde_json::from_slice(&output.stderr).unwrap()
        }

        fn tree(&self, id: &str) -> PathBuf {
            let created = self.ok(&[
                "create",
                "--repo",
                self.repository.to_str().unwrap(),
                "--id",
                id,
                "--purpose",
                "nested keep",
            ]);
            PathBuf::from(created["evidence"]["path"].as_str().unwrap())
        }

        /// The ignored `tool/` of the story: a keep file naming `cases/`, a record and a cache.
        fn tool(&self, id: &str, keep: &str) -> PathBuf {
            let tree = self.tree(id);
            write(&tree.join("tool/.worktreekeep"), keep);
            write(&tree.join("tool/cases/a"), "kept case record\n");
            write(&tree.join("tool/cache/b"), "cache\n");
            tree
        }

        fn lifecycle(&self, id: &str) -> String {
            self.ok(&["status"])["records"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["id"] == id)
                .unwrap()["lifecycle"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.as_str().unwrap().to_owned())
            .collect()
    }

    fn discarded(cache: &Value) -> Vec<String> {
        cache["discarded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn a_nested_keep_file_keeps_its_directory() {
        let fixture = Fixture::new(604_800);
        let tree = fixture.tool("keeps", "cases/\n");
        let tree_str = tree.to_str().unwrap();

        let text = fixture.run(false, &["discard-cache", "--dry-run", tree_str]);
        let text = String::from_utf8(text.stdout).unwrap();
        assert!(text.contains("kept tool/cases/a"), "{text}");
        assert!(text.contains("kept tool/.worktreekeep"), "{text}");

        let report = fixture.ok(&["discard-cache", tree_str]);
        let cache = &report["cache"];
        assert_eq!(discarded(cache), ["tool/cache"], "{cache}");
        let retained = strings(&cache["retained_ignored"]);
        assert!(retained.contains(&"tool/cases/a".to_owned()), "{cache}");
        assert!(
            retained.contains(&"tool/.worktreekeep".to_owned()),
            "{cache}"
        );
        assert!(!tree.join("tool/cache").exists());
        assert_eq!(
            std::fs::read_to_string(tree.join("tool/cases/a")).unwrap(),
            "kept case record\n"
        );
        assert!(tree.join("tool/.worktreekeep").exists());
    }

    #[test]
    fn sweep_honours_a_nested_keep_file() {
        let fixture = Fixture::new(1);
        let tree = fixture.tool("swept", "cases/\n");
        std::thread::sleep(std::time::Duration::from_secs(2));

        let swept = fixture.ok(&[
            "sweep",
            "--repo",
            fixture.repository.to_str().unwrap(),
            "--idle-days",
            "0",
        ]);
        let item = swept["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["record"]["id"] == "swept")
            .unwrap_or_else(|| panic!("{swept}"));
        assert!(item.get("refusal").is_none(), "{item}");
        assert_eq!(discarded(&item["cache"]), ["tool/cache"], "{item}");
        assert!(!tree.join("tool/cache").exists());
        assert_eq!(
            std::fs::read_to_string(tree.join("tool/cases/a")).unwrap(),
            "kept case record\n"
        );
        let archive = PathBuf::from(item["archive"]["path"].as_str().unwrap());
        let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap();
        assert!(patch.contains("tool/cases/a"), "{patch}");
    }

    #[test]
    fn finish_archives_what_a_nested_keep_file_keeps() {
        let fixture = Fixture::new(604_800);
        let tree = fixture.tool("finished", "cases/\n");

        let finished = fixture.ok(&[
            "finish",
            "--discard-cache",
            "--archive",
            tree.to_str().unwrap(),
        ]);
        assert_eq!(discarded(&finished["cache"]), ["tool/cache"], "{finished}");
        let archive = PathBuf::from(finished["archive"]["path"].as_str().unwrap());
        let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap();
        assert!(
            patch.contains("diff --git a/tool/cases/a b/tool/cases/a\n"),
            "{patch}"
        );
        assert!(patch.contains("\n+kept case record\n"), "{patch}");
        assert!(!patch.contains("tool/cache/b"), "{patch}");
        assert_eq!(fixture.lifecycle("finished"), "finished");

        assert!(archive.join("manifest.json").exists());
    }

    #[test]
    fn an_invalid_nested_keep_file_refuses() {
        for shape in ["symlink", "directory", "unreadable"] {
            let fixture = Fixture::new(604_800);
            let tree = fixture.tool(shape, "cases/\n");
            let keep = tree.join("tool/.worktreekeep");
            std::fs::remove_file(&keep).unwrap();
            match shape {
                "symlink" => {
                    write(&tree.join("tool/elsewhere"), "cases/\n");
                    std::os::unix::fs::symlink(tree.join("tool/elsewhere"), &keep).unwrap();
                }
                "directory" => std::fs::create_dir(&keep).unwrap(),
                _ => {
                    write(&keep, "cases/\n");
                    std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o000))
                        .unwrap();
                    if std::fs::File::open(&keep).is_ok() {
                        // Running as a user that reads everything: nothing to refuse.
                        continue;
                    }
                }
            }
            let refusal = fixture.refused(&["discard-cache", tree.to_str().unwrap()]);
            assert_eq!(refusal["code"], "keep-file-invalid", "{shape}: {refusal}");
            assert!(
                refusal["message"]
                    .as_str()
                    .unwrap()
                    .contains("tool/.worktreekeep"),
                "{shape}: {refusal}"
            );
            assert!(tree.join("tool/cache/b").exists(), "{shape}");
            assert!(tree.join("tool/cases/a").exists(), "{shape}");

            let finish = fixture.refused(&["finish", "--discard-cache", tree.to_str().unwrap()]);
            assert_eq!(finish["code"], "keep-file-invalid", "{shape}: {finish}");
            assert!(tree.join("tool/cache/b").exists(), "{shape}");
        }
    }

    /// A tool that ignores its whole directory with a `.gitignore` of its own (`*`) and marks its
    /// records with a keep file in that directory loses only what is not marked.
    #[test]
    fn a_self_ignoring_directory_keeps_the_records_its_keep_file_names() {
        let fixture = Fixture::new(604_800);
        let tree = fixture.tree("self");
        write(&tree.join(".takt/.gitignore"), "*\n");
        write(&tree.join(".takt/.worktreekeep"), "/cases/\n");
        write(&tree.join(".takt/cases/c1/record.json"), "{}\n");
        write(&tree.join(".takt/scratch/tmp"), "scratch\n");

        let finished = fixture.ok(&[
            "finish",
            "--discard-cache",
            "--archive",
            tree.to_str().unwrap(),
        ]);
        let gone = discarded(&finished["cache"]);
        assert!(gone.contains(&".takt/scratch".to_owned()), "{finished}");
        assert!(
            !gone.iter().any(|path| path.contains("cases")),
            "{finished}"
        );
        assert!(!gone.iter().any(|path| path.contains(".worktreekeep")));
        assert_eq!(
            std::fs::read_to_string(tree.join(".takt/cases/c1/record.json")).unwrap(),
            "{}\n"
        );
        assert!(tree.join(".takt/.worktreekeep").exists());
        let archive = PathBuf::from(finished["archive"]["path"].as_str().unwrap());
        let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap();
        assert!(patch.contains(".takt/cases/c1/record.json"), "{patch}");
    }

    #[test]
    fn a_nested_pattern_cannot_reach_outside_its_directory() {
        let fixture = Fixture::new(604_800);
        let tree = fixture.tree("escape");
        write(
            &tree.join("tool/.worktreekeep"),
            "../x\n/../x\n../other/\n../other/x\n/x\n/other/x\nlink\n",
        );
        write(&tree.join("tool/x"), "inside\n");
        write(&tree.join("tool/y"), "disposable\n");
        write(&tree.join("x"), "outside\n");
        write(&tree.join("other/x"), "outside\n");
        std::os::unix::fs::symlink("../other", tree.join("tool/link")).unwrap();

        let report = fixture.ok(&["discard-cache", tree.to_str().unwrap()]);
        let cache = &report["cache"];
        let gone = discarded(cache);
        assert!(gone.contains(&"x".to_owned()), "{cache}");
        assert!(gone.contains(&"other".to_owned()), "{cache}");
        assert!(gone.contains(&"tool/y".to_owned()), "{cache}");
        assert!(!tree.join("x").exists());
        assert!(!tree.join("other").exists());
        assert!(!tree.join("tool/y").exists());
        assert!(tree.join("tool/x").exists());
        assert!(
            tree.join("tool/link").symlink_metadata().is_ok(),
            "the kept link"
        );
    }

    /// Archive `id`, retire its tree, and return the archive's directory.
    fn archived(fixture: &Fixture, id: &str) -> PathBuf {
        let archived = fixture.ok(&["archive", id])["archive"].clone();
        let archive = PathBuf::from(archived["path"].as_str().unwrap());
        fixture.ok(&["finish", id]);
        fixture.ok(&[
            "gc",
            "--repo",
            fixture.repository.to_str().unwrap(),
            "--apply",
            "--id",
            id,
        ]);
        archive
    }

    fn patch_paths(archive: &Path) -> Vec<String> {
        let patch = std::fs::read(archive.join("dirty.patch")).unwrap();
        String::from_utf8_lossy(&patch)
            .lines()
            .filter_map(|line| line.strip_prefix("diff --git a/"))
            .map(|rest| rest.split_once(" b/").unwrap().1.to_owned())
            .collect()
    }

    /// The dry-run assessment of `archive`, and then `--apply` of it.
    fn strip(fixture: &Fixture, archive: &Path) -> Value {
        let repository = fixture.repository.to_str().unwrap();
        let directory = archive.file_name().unwrap().to_str().unwrap();
        let dry = fixture.ok(&[
            "prune-archives",
            "--strip-build-output",
            "--repo",
            repository,
        ]);
        let item = dry["archives"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["archive_directory"] == directory)
            .unwrap_or_else(|| panic!("{dry}"))
            .clone();
        if item["verdict"] == "strippable" {
            fixture.ok(&[
                "prune-archives",
                "--strip-build-output",
                "--repo",
                repository,
                "--apply",
                "--id",
                directory,
            ]);
        }
        item
    }

    #[test]
    fn strip_build_output_keeps_nested_kept_paths() {
        let fixture = Fixture::new(604_800);
        fixture.tool("stripped", "cases/\n");
        let archive = archived(&fixture, "stripped");
        assert_eq!(
            patch_paths(&archive),
            ["tool/.worktreekeep", "tool/cache/b", "tool/cases/a"]
        );

        let item = strip(&fixture, &archive);
        assert_eq!(item["verdict"], "strippable", "{item}");
        assert_eq!(
            patch_paths(&archive),
            ["tool/.worktreekeep", "tool/cases/a"]
        );
        let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap();
        assert!(patch.contains("\n+kept case record\n"), "{patch}");
    }

    #[test]
    fn strip_build_output_reads_a_nested_keep_file_head_holds() {
        let fixture = Fixture::committing(604_800, &[("tool/.worktreekeep", "cases/\n")]);
        let tree = fixture.tree("held");
        write(&tree.join("tool/cases/a"), "kept case record\n");
        write(&tree.join("tool/cache/b"), "cache\n");
        let archive = archived(&fixture, "held");
        assert_eq!(patch_paths(&archive), ["tool/cache/b", "tool/cases/a"]);

        let item = strip(&fixture, &archive);
        assert_eq!(item["verdict"], "strippable", "{item}");
        assert_eq!(patch_paths(&archive), ["tool/cases/a"]);
    }

    #[test]
    fn strip_build_output_keeps_everything_when_a_nested_keep_file_is_a_link() {
        let fixture = Fixture::new(604_800);
        let tree = fixture.tree("linked");
        write(&tree.join("tool/elsewhere"), "cases/\n");
        std::os::unix::fs::symlink("elsewhere", tree.join("tool/.worktreekeep")).unwrap();
        write(&tree.join("tool/cases/a"), "kept case record\n");
        write(&tree.join("tool/cache/b"), "cache\n");
        let archive = archived(&fixture, "linked");
        let before = patch_paths(&archive);

        let item = strip(&fixture, &archive);
        assert_ne!(item["verdict"], "strippable", "{item}");
        assert_eq!(patch_paths(&archive), before);
    }

    #[test]
    fn a_keep_file_does_not_reach_a_sibling_directory_in_a_strip() {
        let fixture = Fixture::new(604_800);
        let tree = fixture.tool("sibling", "../other/\n/../other/\n");
        write(&tree.join("other/x"), "outside\n");
        let archive = archived(&fixture, "sibling");

        let item = strip(&fixture, &archive);
        assert_eq!(item["verdict"], "strippable", "{item}");
        assert_eq!(patch_paths(&archive), ["tool/.worktreekeep"]);
    }
}
