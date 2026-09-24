//! The agent plugin ships with the binary at one version and carries only generated skills.

use std::path::{Path, PathBuf};

fn plugin_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/worktree")
}

fn json(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

#[test]
fn both_manifests_carry_the_binary_version_and_plugin_name() {
    for manifest in [".claude-plugin/plugin.json", ".codex-plugin/plugin.json"] {
        let value = json(&plugin_root().join(manifest));
        assert_eq!(value["name"], "worktree", "{manifest} name");
        assert_eq!(
            value["version"],
            env!("CARGO_PKG_VERSION"),
            "{manifest} version must equal the workspace package version"
        );
    }
}

#[test]
fn codex_marketplace_serves_the_plugin_directory() {
    let value = json(&plugin_root().join("../../.agents/plugins/marketplace.json"));
    assert_eq!(value["name"], "worktree");
    let plugins = value["plugins"].as_array().expect("plugins array");
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["name"], "worktree");
    assert_eq!(plugins[0]["source"]["path"], "./plugins/worktree");
}

#[test]
fn every_skill_folder_matches_its_frontmatter_name() {
    let skills = plugin_root().join("skills");
    let mut seen = 0;
    for entry in std::fs::read_dir(&skills).expect("skills directory") {
        let folder = entry.expect("skill entry").path();
        let text = std::fs::read_to_string(folder.join("SKILL.md")).expect("SKILL.md");
        let name = text
            .lines()
            .find_map(|line| line.strip_prefix("name: "))
            .expect("frontmatter name");
        assert_eq!(
            Some(name),
            folder.file_name().and_then(|name| name.to_str()),
            "{}",
            folder.display()
        );
        seen += 1;
    }
    assert_eq!(
        seen, 1,
        "the plugin ships exactly the generated worktree skill"
    );
}
