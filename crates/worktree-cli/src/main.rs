//! Command-line composition root for the worktree lifecycle service.

use anyhow::{Context, Result, anyhow};
use b10x_worktree::{GitPort, RegistryPort, SystemClock, WorktreeManager};
use b10x_worktree_domain::{
    CreateRequest, GitRevision, ReconciliationAction, ReconciliationAssessment, Refusal,
    SURFACE_VERSION, WorktreeId,
};
use b10x_worktree_git::ProcessGit;
use b10x_worktree_state::{
    ProfileTemplate, SqliteRegistry, config_path, load_config, registry_path, resolve_policy,
    save_config, state_home, upsert_profile,
};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(version, about = "Safe, policy-driven Git worktree lifecycle")]
struct Cli {
    /// Emit stable JSON instead of human-readable output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Activate a portable workspace policy.
    Activate(ActivateArgs),
    /// Create a registered detached linked worktree.
    Create(CreateArgs),
    /// Show registered worktree lifecycle state.
    Status,
    /// Mark a clean, idle worktree finished.
    Finish {
        /// Managed worktree path; defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Assess or safely remove finished and expired worktrees.
    Gc(GcArgs),
    /// Reconcile adopted legacy paths and already-missing registry records.
    Reconcile(ReconcileArgs),
    /// Check configuration, registry and Git prerequisites.
    Doctor {
        /// Exit unsuccessfully if any check fails.
        #[arg(long)]
        check: bool,
    },
    /// Discover or explicitly adopt linked worktrees.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Lifecycle protocol used by agent hooks.
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Render agent guidance from this exact command surface.
    Skill(SkillArgs),
}

#[derive(Debug, Args)]
struct ActivateArgs {
    /// Committed portable profile template.
    #[arg(long)]
    profile: PathBuf,
    /// Absolute primary checkout collection governed by the profile.
    #[arg(long)]
    workspace: PathBuf,
    /// Install managed global guidance for Codex and Claude agents.
    #[arg(long, default_value_t = false)]
    install_agent_guidance: bool,
}

#[derive(Debug, Args)]
struct CreateArgs {
    /// Source repository or any path inside it.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Short purpose retained in the registry.
    #[arg(long)]
    purpose: String,
    /// Starting revision.
    #[arg(long, default_value = "HEAD")]
    base: String,
    /// Stable id; generated when omitted.
    #[arg(long)]
    id: Option<String>,
    /// Owner class recorded for cleanup delegation.
    #[arg(long, default_value = "agent")]
    owner: String,
}

#[derive(Debug, Args)]
struct GcArgs {
    /// Repository used to select its activated workspace policy.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Apply eligible removals. Without this flag, GC is a dry-run.
    #[arg(long, conflicts_with = "dry_run")]
    apply: bool,
    /// Explicitly document dry-run intent.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ReconcileArgs {
    /// Repository used to select its activated workspace policy.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Exact registered id to assess or apply; repeat for multiple records.
    #[arg(long = "id")]
    ids: Vec<String>,
    /// Apply reviewed reconciliation actions. Requires at least one id.
    #[arg(long, conflicts_with = "dry_run")]
    apply: bool,
    /// Explicitly document dry-run intent.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// List Git-discovered worktrees and their registration status.
    List {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Explicitly register one existing linked tree as manager-owned.
    Adopt {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        purpose: String,
        #[arg(long, default_value = "legacy-adoption")]
        owner: String,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Acquire a live session lease.
    SessionStart(SessionArgs),
    /// Refresh a live session lease.
    Heartbeat(SessionArgs),
    /// Release a session lease.
    SessionEnd(SessionArgs),
}

#[derive(Debug, Args)]
struct SessionArgs {
    #[arg(long, default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    session: String,
}

#[derive(Debug, Args)]
struct SkillArgs {
    /// Skill directory to create or verify.
    #[arg(long, default_value = ".agents/skills/worktree")]
    out: PathBuf,
    /// Compare generated files without modifying them.
    #[arg(long, conflicts_with = "force")]
    check: bool,
    /// Replace files even when they lack the generator marker.
    #[arg(long)]
    force: bool,
}

type Manager = WorktreeManager<ProcessGit, SqliteRegistry, SystemClock>;

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(error) = run(&cli) {
        if cli.json {
            let refusal = error.downcast_ref::<Refusal>();
            let value = serde_json::json!({
                "version": SURFACE_VERSION,
                "ok": false,
                "code": refusal.map_or("operation-failed", |item| item.code.as_str()),
                "message": error.to_string(),
            });
            eprintln!("{}", serde_json::to_string_pretty(&value)?);
        }
        return Err(error);
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Activate(args) => activate(args, cli.json),
        Command::Create(args) => create(args, cli.json),
        Command::Status => status(cli.json),
        Command::Finish { path } => {
            let evidence = manager()?.finish(path).map_err(anyhow::Error::new)?;
            emit(cli.json, &evidence, || {
                format!("finished {}", evidence.path.display())
            })
        }
        Command::Gc(args) => gc(args, cli.json),
        Command::Reconcile(args) => reconcile(args, cli.json),
        Command::Doctor { check } => doctor(*check, cli.json),
        Command::Repo { command } => repo(command, cli.json),
        Command::Hook { command } => hook(command, cli.json),
        Command::Skill(args) => render_skill(args, cli.json),
    }
}

fn manager() -> Result<Manager> {
    let registry = SqliteRegistry::open(&registry_path().map_err(anyhow::Error::new)?)
        .map_err(anyhow::Error::new)?;
    Ok(WorktreeManager::new(ProcessGit, registry, SystemClock))
}

fn activate(args: &ActivateArgs, json: bool) -> Result<()> {
    let source = std::fs::read_to_string(&args.profile)
        .with_context(|| format!("read profile {}", args.profile.display()))?;
    let template: ProfileTemplate = toml::from_str(&source).context("parse profile")?;
    let workspace = std::fs::canonicalize(&args.workspace)
        .with_context(|| format!("resolve workspace {}", args.workspace.display()))?;
    let policy = template
        .resolve(workspace, &state_home().map_err(anyhow::Error::new)?)
        .map_err(anyhow::Error::new)?;
    let path = config_path().map_err(anyhow::Error::new)?;
    let mut config = load_config(&path).map_err(anyhow::Error::new)?;
    upsert_profile(&mut config, policy.clone());
    save_config(&path, &config).map_err(anyhow::Error::new)?;
    if args.install_agent_guidance {
        install_agent_guidance()?;
    }
    emit(json, &policy, || {
        format!(
            "activated {}: {} -> {}",
            policy.name,
            policy.workspace_root.display(),
            policy.worktree_root.display()
        )
    })
}

fn create(args: &CreateArgs, json: bool) -> Result<()> {
    let service = manager()?;
    let repository = ProcessGit
        .repository_snapshot(&args.repo)
        .map_err(anyhow::Error::new)?;
    let config =
        load_config(&config_path().map_err(anyhow::Error::new)?).map_err(anyhow::Error::new)?;
    let policy = resolve_policy(&config, &repository.root).map_err(anyhow::Error::new)?;
    let id = args.id.clone().unwrap_or_else(generated_id);
    let request = CreateRequest {
        id: WorktreeId::new(id).map_err(anyhow::Error::new)?,
        repository: repository.root,
        purpose: args.purpose.clone(),
        base: GitRevision::new(args.base.clone()).map_err(anyhow::Error::new)?,
        owner: args.owner.clone(),
    };
    let plan = service
        .plan_create(policy, request)
        .map_err(anyhow::Error::new)?;
    let evidence = service.create(&plan).map_err(anyhow::Error::new)?;
    emit(json, &evidence, || evidence.path.display().to_string())
}

fn status(json: bool) -> Result<()> {
    let records = manager()?.registry().list().map_err(anyhow::Error::new)?;
    emit(json, &records, || {
        if records.is_empty() {
            "no registered worktrees".into()
        } else {
            records
                .iter()
                .map(|record| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        record.id,
                        record.lifecycle.as_str(),
                        record.repository_root.display(),
                        record.path.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    })
}

fn gc(args: &GcArgs, json: bool) -> Result<()> {
    let repository = ProcessGit
        .repository_snapshot(&args.repo)
        .map_err(anyhow::Error::new)?;
    let config =
        load_config(&config_path().map_err(anyhow::Error::new)?).map_err(anyhow::Error::new)?;
    let policy = resolve_policy(&config, &repository.root).map_err(anyhow::Error::new)?;
    let assessments = manager()?
        .gc(policy, args.apply)
        .map_err(anyhow::Error::new)?;
    emit(json, &assessments, || {
        if assessments.is_empty() {
            "no cleanup candidates".into()
        } else {
            assessments
                .iter()
                .map(|item| {
                    let outcome = item.evidence.as_ref().map_or_else(
                        || {
                            item.refusal.as_ref().map_or_else(
                                || "eligible".into(),
                                |reason| format!("retained: {reason}"),
                            )
                        },
                        |_| "removed".into(),
                    );
                    format!(
                        "{}\t{outcome}\t{}",
                        item.record.id,
                        item.record.path.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    })
}

#[derive(Serialize)]
struct ReconciliationReport<'a> {
    version: u32,
    assessments: &'a [ReconciliationAssessment],
}

fn reconcile(args: &ReconcileArgs, json: bool) -> Result<()> {
    if args.apply && args.ids.is_empty() {
        return Err(anyhow!(
            "reconcile --apply requires at least one reviewed --id"
        ));
    }
    let repository = ProcessGit
        .repository_snapshot(&args.repo)
        .map_err(anyhow::Error::new)?;
    let config =
        load_config(&config_path().map_err(anyhow::Error::new)?).map_err(anyhow::Error::new)?;
    let policy = resolve_policy(&config, &repository.root).map_err(anyhow::Error::new)?;
    let ids = args
        .ids
        .iter()
        .map(|id| WorktreeId::new(id.clone()).map_err(anyhow::Error::new))
        .collect::<Result<Vec<_>>>()?;
    let assessments = manager()?
        .reconcile(policy, &ids, args.apply)
        .map_err(anyhow::Error::new)?;
    let report = ReconciliationReport {
        version: SURFACE_VERSION,
        assessments: &assessments,
    };
    emit(json, &report, || {
        if assessments.is_empty() {
            "no reconciliation candidates".into()
        } else {
            assessments
                .iter()
                .map(|item| {
                    let action = match &item.action {
                        ReconciliationAction::Migrate { from, to } => {
                            format!("migrate {} -> {}", from.display(), to.display())
                        }
                        ReconciliationAction::TombstoneMissing { path } => {
                            format!("tombstone missing {}", path.display())
                        }
                    };
                    let outcome = item.evidence.as_ref().map_or_else(
                        || {
                            item.refusal.as_ref().map_or_else(
                                || "eligible".into(),
                                |reason| format!("retained: {reason}"),
                            )
                        },
                        |_| "applied".into(),
                    );
                    format!("{}\t{outcome}\t{action}", item.record.id)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    })
}

#[derive(Serialize)]
struct DoctorReport {
    version: u32,
    git: bool,
    config: bool,
    registry: bool,
    profiles: usize,
    errors: Vec<String>,
}

fn doctor(check: bool, json: bool) -> Result<()> {
    let mut report = DoctorReport {
        version: SURFACE_VERSION,
        git: std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()),
        config: false,
        registry: false,
        profiles: 0,
        errors: Vec::new(),
    };
    match config_path()
        .map_err(anyhow::Error::new)
        .and_then(|path| load_config(&path).map_err(anyhow::Error::new))
    {
        Ok(config) => {
            report.config = true;
            report.profiles = config.profiles.len();
        }
        Err(error) => report.errors.push(error.to_string()),
    }
    match manager() {
        Ok(_) => report.registry = true,
        Err(error) => report.errors.push(error.to_string()),
    }
    if !report.git {
        report.errors.push("git is unavailable".into());
    }
    let healthy = report.git && report.config && report.registry;
    emit(json, &report, || {
        format!(
            "git={} config={} registry={} profiles={}",
            report.git, report.config, report.registry, report.profiles
        )
    })?;
    if check && !healthy {
        return Err(anyhow!("doctor checks failed"));
    }
    Ok(())
}

#[derive(Serialize)]
struct RepoItem {
    path: PathBuf,
    head: Option<String>,
    primary: bool,
    locked: bool,
    registered: bool,
}

fn repo(command: &RepoCommand, json: bool) -> Result<()> {
    match command {
        RepoCommand::List { repo } => {
            let manager = manager()?;
            let records = manager.registry().list().map_err(anyhow::Error::new)?;
            let discovered = ProcessGit
                .list_worktrees(repo)
                .map_err(anyhow::Error::new)?;
            let items = discovered
                .into_iter()
                .map(|item| RepoItem {
                    registered: records.iter().any(|record| record.path == item.path),
                    path: item.path,
                    head: item.head,
                    primary: item.primary,
                    locked: item.locked,
                })
                .collect::<Vec<_>>();
            emit(json, &items, || {
                items
                    .iter()
                    .map(|item| {
                        format!(
                            "{}\t{}\t{}",
                            if item.registered {
                                "managed"
                            } else {
                                "unmanaged"
                            },
                            if item.primary { "primary" } else { "linked" },
                            item.path.display()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        RepoCommand::Adopt {
            repo,
            path,
            id,
            purpose,
            owner,
        } => {
            let evidence = manager()?
                .adopt(
                    repo,
                    path,
                    WorktreeId::new(id.clone()).map_err(anyhow::Error::new)?,
                    purpose.clone(),
                    owner.clone(),
                )
                .map_err(anyhow::Error::new)?;
            emit(json, &evidence, || {
                format!("adopted {}", evidence.path.display())
            })
        }
    }
}

fn hook(command: &HookCommand, json: bool) -> Result<()> {
    let service = manager()?;
    let (operation, path, session) = match command {
        HookCommand::SessionStart(args) => {
            service
                .session_start(&args.path, &args.session)
                .map_err(anyhow::Error::new)?;
            ("session-start", &args.path, &args.session)
        }
        HookCommand::Heartbeat(args) => {
            service
                .session_start(&args.path, &args.session)
                .map_err(anyhow::Error::new)?;
            ("heartbeat", &args.path, &args.session)
        }
        HookCommand::SessionEnd(args) => {
            service
                .session_end(&args.path, &args.session)
                .map_err(anyhow::Error::new)?;
            ("session-end", &args.path, &args.session)
        }
    };
    let value = serde_json::json!({
        "version": SURFACE_VERSION,
        "operation": operation,
        "path": path,
        "session": session,
    });
    emit(json, &value, || format!("{operation} {}", path.display()))
}

fn render_skill(args: &SkillArgs, json: bool) -> Result<()> {
    let files = [
        (args.out.join("SKILL.md"), skill_markdown()),
        (args.out.join("agents/openai.yaml"), skill_interface()),
    ];
    if args.check {
        let stale = files
            .iter()
            .filter(|(path, expected)| {
                std::fs::read_to_string(path).ok().as_ref() != Some(expected)
            })
            .map(|(path, _)| path.display().to_string())
            .collect::<Vec<_>>();
        if !stale.is_empty() {
            return Err(anyhow!("generated skill is stale: {}", stale.join(", ")));
        }
    } else {
        for (path, source) in &files {
            if path.exists() && !args.force {
                let current = std::fs::read_to_string(path)?;
                if !current.contains("Generated by `worktree skill`") {
                    return Err(anyhow!(
                        "{} is not generator-owned; pass --force once",
                        path.display()
                    ));
                }
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, source)?;
        }
    }
    let value = serde_json::json!({
        "version": SURFACE_VERSION,
        "path": args.out,
        "check": args.check,
    });
    emit(json, &value, || {
        if args.check {
            format!("skill is current: {}", args.out.display())
        } else {
            format!("rendered skill: {}", args.out.display())
        }
    })
}

fn generated_id() -> String {
    let compact = Uuid::new_v4().simple().to_string();
    format!("wt-{}", &compact[..12])
}

fn emit<T, F>(json: bool, value: &T, human: F) -> Result<()>
where
    T: Serialize,
    F: FnOnce() -> String,
{
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", human());
    }
    Ok(())
}

const GUIDANCE_BEGIN: &str = "<!-- b10x-worktree:begin -->";
const GUIDANCE_END: &str = "<!-- b10x-worktree:end -->";

fn install_agent_guidance() -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let block = format!(
        "{GUIDANCE_BEGIN}\n## Managed worktrees\n\nFor repository changes, use the `worktree` CLI and its installed worktree skill. Create isolated trees with `worktree create`, keep primary checkouts clean, publish commits before `worktree finish`, and use `worktree gc --dry-run` before `--apply`. Never force-remove or manually delete a managed tree.\n{GUIDANCE_END}\n"
    );
    update_managed_block(&home.join(".codex/AGENTS.md"), &block)?;
    update_managed_block(&home.join(".claude/CLAUDE.md"), &block)
}

fn update_managed_block(path: &Path, block: &str) -> Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let next = if let (Some(begin), Some(end)) =
        (current.find(GUIDANCE_BEGIN), current.find(GUIDANCE_END))
    {
        let after = end + GUIDANCE_END.len();
        format!(
            "{}{}{}",
            &current[..begin],
            block.trim_end(),
            &current[after..]
        )
    } else if current.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{block}\n", current.trim_end())
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, next).with_context(|| format!("write {}", path.display()))
}

fn skill_markdown() -> String {
    r"---
name: worktree
description: Safely create, inspect, finish, and garbage-collect managed Git worktrees. Use whenever an agent needs an isolated checkout for repository changes, must hand off a worktree, or needs to audit and clean linked worktrees.
---

# Worktree

<!-- Generated by `worktree skill`; edit the generator, not this file. -->

Use the `worktree` CLI as the sole owner of linked-worktree lifecycle. It keeps trees outside primary checkout collections and refuses cleanup without current recovery proof.

## Start repository work

1. From a primary checkout, run `worktree create --purpose <short-purpose>`. Add `--repo <path>`, `--base <revision>`, or `--id <stable-id>` when needed.
2. Treat the printed path as the task checkout and do all changes there.
3. If already inside a managed tree, reuse it; do not nest another worktree.
4. For automation, add `--json` and consume the versioned output.

## Maintain the lease

Hook integrations should run `worktree hook session-start --session <id>` on entry, `worktree hook heartbeat --session <id>` during long work, and `worktree hook session-end --session <id>` on exit. A live lease blocks cleanup.

## Finish and clean up

1. Commit and publish every wanted change. A local-only commit is deliberately not cleanup-safe.
2. In the managed tree, run `worktree finish`. It refuses dirty, locked, unmanaged, or live worktrees.
3. Run `worktree gc --repo <primary> --dry-run` and inspect every result.
4. Run `worktree gc --repo <primary> --apply` only when removal is intended. The command fetches and revalidates immediately before non-forced removal.

## Audit and recovery

- Run `worktree status` for durable lifecycle state.
- Run `worktree repo list --repo <path>` to distinguish managed, unmanaged, primary, and linked checkouts.
- Run `worktree reconcile --repo <path> --dry-run` to assess adopted legacy paths and missing records. Apply only reviewed ids with repeated `--id` arguments.
- Run `worktree doctor --check` for prerequisites and configuration.
- Use `worktree repo adopt ...` only after a human explicitly decides an existing tree should become manager-owned.

Never run `git worktree remove --force`, recursively delete a linked tree, place managed trees below the primary workspace, or clean up a tree merely because it looks old.
"
    .to_owned()
}

fn skill_interface() -> String {
    r#"interface:
  display_name: "Worktree"
  short_description: "Safely manage isolated Git worktree lifecycles"
  default_prompt: "Use the worktree CLI to create or maintain an isolated checkout, preserve recovery evidence, and clean it up safely."
"#
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_report_uses_standard_version_key() {
        let assessments = Vec::<ReconciliationAssessment>::new();
        let value = serde_json::to_value(ReconciliationReport {
            version: SURFACE_VERSION,
            assessments: &assessments,
        })
        .unwrap();

        assert_eq!(value["version"], SURFACE_VERSION);
        assert!(value.get("reconciliation_version").is_none());
    }

    #[test]
    fn managed_guidance_is_replaced_idempotently() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("AGENTS.md");
        update_managed_block(
            &path,
            "<!-- b10x-worktree:begin -->\none\n<!-- b10x-worktree:end -->\n",
        )
        .unwrap();
        update_managed_block(
            &path,
            "<!-- b10x-worktree:begin -->\ntwo\n<!-- b10x-worktree:end -->\n",
        )
        .unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert!(!content.contains("one"));
        assert_eq!(content.matches(GUIDANCE_BEGIN).count(), 1);
    }
}
