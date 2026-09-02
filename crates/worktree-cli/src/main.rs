//! Command-line composition root for the worktree lifecycle service.

use anyhow::{Context, Result, anyhow};
use b10x_worktree::{GitPort, RegistryPort, SystemClock, WorktreeManager};
use b10x_worktree_domain::{
    CLI_PROTOCOL_VERSION, CreateRequest, GitRevision, HOOK_PROTOCOL_VERSION,
    RECONCILIATION_VERSION, ReconciliationAction, Refusal, WorktreeId,
};
use b10x_worktree_git::ProcessGit;
use b10x_worktree_state::{
    ProfileTemplate, SqliteRegistry, config_path, load_config, registry_path, resolve_policy,
    save_config, state_home, upsert_profile,
};
use clap::{Args, Parser, Subcommand, error::ErrorKind};
use serde::Serialize;
use std::io::Write as _;
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
    /// Reconcile interrupted provisioning, adopted paths, and missing records.
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
    /// Exact registered id reviewed in a dry-run; repeat for multiple records.
    #[arg(long = "id")]
    ids: Vec<String>,
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
    /// Explicitly permit reviewed retirement of finished trees outside the managed root.
    #[arg(long, requires = "apply")]
    allow_external_retirement: bool,
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

#[derive(Serialize)]
struct SuccessEnvelope<T> {
    version: u32,
    ok: bool,
    #[serde(flatten)]
    payload: T,
}

#[derive(Serialize)]
struct PolicyPayload<'a, T: ?Sized> {
    policy: &'a T,
}

#[derive(Serialize)]
struct EvidencePayload<'a, T: ?Sized> {
    evidence: &'a T,
}

#[derive(Serialize)]
struct RecordsPayload<'a, T: ?Sized> {
    records: &'a T,
}

#[derive(Serialize)]
struct AssessmentsPayload<'a, T: ?Sized> {
    assessments: &'a T,
}

#[derive(Serialize)]
struct ItemsPayload<'a, T: ?Sized> {
    items: &'a T,
}

fn main() {
    let json_requested = std::env::args_os().any(|argument| argument == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            error.exit()
        }
        Err(error) if json_requested => exit_with_json_error(
            CLI_PROTOCOL_VERSION,
            "invalid-arguments",
            &error.to_string(),
        ),
        Err(error) => error.exit(),
    };
    if let Err(error) = run(&cli) {
        if cli.json {
            let refusal = error.downcast_ref::<Refusal>();
            exit_with_json_error(
                command_protocol_version(&cli.command),
                refusal.map_or("operation-failed", |item| item.code.as_str()),
                &error.to_string(),
            );
        }
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn command_protocol_version(command: &Command) -> u32 {
    match command {
        Command::Hook { .. } => HOOK_PROTOCOL_VERSION,
        Command::Reconcile(_) => RECONCILIATION_VERSION,
        _ => CLI_PROTOCOL_VERSION,
    }
}

fn exit_with_json_error(version: u32, code: &str, message: &str) -> ! {
    let value = serde_json::json!({
        "version": version,
        "ok": false,
        "code": code,
        "message": message,
    });
    eprintln!(
        "{}",
        serde_json::to_string(&value).expect("error report contains only serializable values")
    );
    std::process::exit(1);
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Activate(args) => activate(args, cli.json),
        Command::Create(args) => create(args, cli.json),
        Command::Status => status(cli.json),
        Command::Finish { path } => {
            let evidence = manager()?.finish(path).map_err(anyhow::Error::new)?;
            emit_success(
                cli.json,
                CLI_PROTOCOL_VERSION,
                EvidencePayload {
                    evidence: &evidence,
                },
                || format!("finished {}", evidence.path.display()),
            )
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
        .resolve(&workspace, &state_home().map_err(anyhow::Error::new)?)
        .map_err(anyhow::Error::new)?;
    let path = config_path().map_err(anyhow::Error::new)?;
    let mut config = load_config(&path).map_err(anyhow::Error::new)?;
    upsert_profile(&mut config, policy.clone());
    save_config(&path, &config).map_err(anyhow::Error::new)?;
    if args.install_agent_guidance {
        install_agent_guidance()?;
    }
    emit_success(
        json,
        CLI_PROTOCOL_VERSION,
        PolicyPayload { policy: &policy },
        || {
            format!(
                "activated {}: {} -> {}",
                policy.name,
                policy.workspace_root.display(),
                policy.worktree_root.display()
            )
        },
    )
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
    let evidence = service.create(policy, &plan).map_err(anyhow::Error::new)?;
    emit_success(
        json,
        CLI_PROTOCOL_VERSION,
        EvidencePayload {
            evidence: &evidence,
        },
        || evidence.path.display().to_string(),
    )
}

fn status(json: bool) -> Result<()> {
    let records = manager()?.registry().list().map_err(anyhow::Error::new)?;
    emit_success(
        json,
        CLI_PROTOCOL_VERSION,
        RecordsPayload { records: &records },
        || {
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
        },
    )
}

fn gc(args: &GcArgs, json: bool) -> Result<()> {
    if args.apply && args.ids.is_empty() {
        return Err(anyhow!("gc --apply requires at least one reviewed --id"));
    }
    let repository = ProcessGit
        .repository_snapshot(&args.repo)
        .map_err(anyhow::Error::new)?;
    let config =
        load_config(&config_path().map_err(anyhow::Error::new)?).map_err(anyhow::Error::new)?;
    let policy = resolve_policy(&config, &repository.root).map_err(anyhow::Error::new)?;
    let ids = parse_ids(&args.ids)?;
    let assessments = manager()?
        .gc(policy, &ids, args.apply)
        .map_err(anyhow::Error::new)?;
    emit_success(
        json,
        CLI_PROTOCOL_VERSION,
        AssessmentsPayload {
            assessments: &assessments,
        },
        || {
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
        },
    )
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
    let ids = parse_ids(&args.ids)?;
    let assessments = manager()?
        .reconcile(policy, &ids, args.apply, args.allow_external_retirement)
        .map_err(anyhow::Error::new)?;
    emit_success(
        json,
        RECONCILIATION_VERSION,
        AssessmentsPayload {
            assessments: &assessments,
        },
        || {
            if assessments.is_empty() {
                "no reconciliation candidates".into()
            } else {
                assessments
                    .iter()
                    .map(|item| {
                        let action = match &item.action {
                            ReconciliationAction::RecoverProvisioning { path } => {
                                format!("recover provisioning {}", path.display())
                            }
                            ReconciliationAction::Migrate { from, to } => {
                                format!("migrate {} -> {}", from.display(), to.display())
                            }
                            ReconciliationAction::RetireExternal { path } => {
                                format!("retire external {}", path.display())
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
        },
    )
}

#[derive(Serialize)]
struct DoctorReport {
    git: bool,
    config: bool,
    registry: bool,
    profiles: usize,
    errors: Vec<String>,
}

fn doctor(check: bool, json: bool) -> Result<()> {
    let mut report = DoctorReport {
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
    if check && !healthy {
        return Err(anyhow!(
            "doctor checks failed: {}",
            report.errors.join("; ")
        ));
    }
    emit_success(json, CLI_PROTOCOL_VERSION, &report, || {
        format!(
            "git={} config={} registry={} profiles={}",
            report.git, report.config, report.registry, report.profiles
        )
    })?;
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
            emit_success(
                json,
                CLI_PROTOCOL_VERSION,
                ItemsPayload { items: &items },
                || {
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
                },
            )
        }
        RepoCommand::Adopt {
            repo,
            path,
            id,
            purpose,
            owner,
        } => {
            let repository = ProcessGit
                .repository_snapshot(repo)
                .map_err(anyhow::Error::new)?;
            let config = load_config(&config_path().map_err(anyhow::Error::new)?)
                .map_err(anyhow::Error::new)?;
            let policy = resolve_policy(&config, &repository.root).map_err(anyhow::Error::new)?;
            let evidence = manager()?
                .adopt(
                    policy,
                    &repository.root,
                    path,
                    WorktreeId::new(id.clone()).map_err(anyhow::Error::new)?,
                    purpose.clone(),
                    owner.clone(),
                )
                .map_err(anyhow::Error::new)?;
            emit_success(
                json,
                CLI_PROTOCOL_VERSION,
                EvidencePayload {
                    evidence: &evidence,
                },
                || format!("adopted {}", evidence.path.display()),
            )
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
        "version": HOOK_PROTOCOL_VERSION,
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
        "path": args.out,
        "check": args.check,
    });
    emit_success(json, CLI_PROTOCOL_VERSION, &value, || {
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

fn parse_ids(ids: &[String]) -> Result<Vec<WorktreeId>> {
    ids.iter()
        .map(|id| WorktreeId::new(id.clone()).map_err(anyhow::Error::new))
        .collect()
}

fn emit_success<T, F>(json: bool, version: u32, payload: T, human: F) -> Result<()>
where
    T: Serialize,
    F: FnOnce() -> String,
{
    emit(
        json,
        &SuccessEnvelope {
            version,
            ok: true,
            payload,
        },
        human,
    )
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
        "{GUIDANCE_BEGIN}\n## Managed worktrees\n\nFor repository changes, invoke `$worktree` and use the `worktree` CLI. Create isolated trees with `worktree create`, keep primary checkouts clean, and publish commits before `worktree finish`. Review cleanup with `worktree gc --dry-run`, then pass only exact reviewed ids to `worktree gc --apply --id <id>`. Use `worktree reconcile` for interrupted provisioning, adopted legacy paths, and already-missing records; external retirement additionally requires explicit `--allow-external-retirement`. Never force-remove or manually delete a managed tree.\n{GUIDANCE_END}\n"
    );
    update_managed_block(&home.join(".codex/AGENTS.md"), &block)?;
    update_managed_block(&home.join(".claude/CLAUDE.md"), &block)
}

fn update_managed_block(path: &Path, block: &str) -> Result<()> {
    let current = match std::fs::read_to_string(path) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let begins = current.match_indices(GUIDANCE_BEGIN).collect::<Vec<_>>();
    let ends = current.match_indices(GUIDANCE_END).collect::<Vec<_>>();
    let next = match (begins.as_slice(), ends.as_slice()) {
        ([], []) if current.trim().is_empty() => format!("{block}\n"),
        ([], []) => format!("{}\n\n{block}\n", current.trim_end()),
        ([(begin, _)], [(end, _)]) if begin < end => {
            let after = end + GUIDANCE_END.len();
            format!(
                "{}{}{}",
                &current[..*begin],
                block.trim_end(),
                &current[after..]
            )
        }
        _ => {
            return Err(anyhow!(
                "{} contains malformed or duplicate managed-guidance markers",
                path.display()
            ));
        }
    };
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary guidance beside {}", path.display()))?;
    if let Ok(metadata) = std::fs::metadata(path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .with_context(|| format!("preserve permissions for {}", path.display()))?;
    }
    temporary
        .write_all(next.as_bytes())
        .with_context(|| format!("write temporary guidance for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("sync temporary guidance for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn skill_markdown() -> String {
    r"---
name: worktree
description: Safely create, inspect, finish, reconcile, recover, and garbage-collect managed Git worktrees. Use whenever an agent needs an isolated checkout for repository changes, must hand off a worktree, or needs to audit, recover, or clean linked worktrees.
---

# Worktree

<!-- Generated by `worktree skill`; edit the generator, not this file. -->

Use the `worktree` CLI as the sole owner of linked-worktree lifecycle. It keeps trees outside primary checkout collections and refuses cleanup without current recovery proof.

## Start repository work

1. Invoke `$worktree`, then from a primary checkout run `worktree create --purpose <short-purpose>`. Add `--repo <path>`, `--base <revision>`, or `--id <stable-id>` when needed.
2. Treat the printed path as the task checkout and do all changes there.
3. If already inside a managed tree, reuse it; do not nest another worktree.
4. For automation, add `--json` and consume the versioned output.

## Maintain the lease

Hook integrations should run `worktree hook session-start --session <id>` on entry, `worktree hook heartbeat --session <id>` during long work, and `worktree hook session-end --session <id>` on exit. A live lease blocks cleanup.

## Finish and clean up

1. Commit and publish every wanted change. A local-only commit is deliberately not cleanup-safe.
2. In the managed tree, run `worktree finish`. It refuses dirty, locked, unmanaged, live, or mid-operation Git worktrees.
3. Run `worktree gc --repo <primary> --dry-run` and inspect every result.
4. Run `worktree gc --repo <primary> --apply --id <reviewed-id>` with repeated `--id` values only for the exact results intended for removal. The command refreshes remote advertisements, fetches required objects, and revalidates immediately before non-forced removal.

## Audit and recovery

- Run `worktree status` for durable lifecycle state.
- Run `worktree repo list --repo <path>` to distinguish managed, unmanaged, primary, and linked checkouts.
- Run `worktree reconcile --repo <path> --dry-run` to assess interrupted provisioning, adopted legacy paths, finished external trees, and missing records.
- Apply reconciliation only to ids copied from that immediately preceding dry-run with `worktree reconcile --repo <path> --apply --id <reviewed-id>` and repeated `--id` arguments when needed.
- If that dry-run explicitly proposes `retire-external`, confirm that destructive action separately by adding `--allow-external-retirement`; never add it for an unrelated migration or missing-record repair.
- If removal is interrupted while the path still exists, rerun GC dry-run and exact-id apply. If the path is already absent, use reconciliation dry-run and exact-id apply; its durable removal intent can safely finish the recorded transition.
- A missing Active record without matching durable removal intent must remain refused. Preserve and investigate its registry evidence; never manually tombstone it, delete related state, or fabricate recovery proof.
- Run `worktree doctor --check` for prerequisites and configuration.
- Only after a human explicitly decides an existing linked tree should become manager-owned, run `worktree repo adopt --repo <primary> --path <linked-tree> --id <stable-id> --purpose <purpose>`. Then review `reconcile --dry-run` and use exact-id apply only if migration is intended.

Never run `git worktree remove --force`, recursively delete a linked tree, place managed trees below the primary workspace, or clean up a tree merely because it looks old.
"
    .to_owned()
}

fn skill_interface() -> String {
    r#"# Generated by `worktree skill`; edit the generator, not this file.
interface:
  display_name: "Worktree"
  short_description: "Safely manage isolated Git worktree lifecycles"
  default_prompt: "Use $worktree and its CLI to create or maintain an isolated checkout, preserve recovery evidence, reconcile lifecycle state, and clean it up safely."
"#
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_report_uses_reconciliation_protocol_version() {
        let assessments = Vec::<b10x_worktree_domain::ReconciliationAssessment>::new();
        let value = serde_json::to_value(SuccessEnvelope {
            version: RECONCILIATION_VERSION,
            ok: true,
            payload: AssessmentsPayload {
                assessments: &assessments,
            },
        })
        .unwrap();

        assert_eq!(value["version"], RECONCILIATION_VERSION);
        assert_eq!(value["ok"], true);
        assert_eq!(value["assessments"], serde_json::json!([]));
    }

    #[test]
    fn ordinary_success_uses_cli_protocol_envelope() {
        let value = serde_json::to_value(SuccessEnvelope {
            version: CLI_PROTOCOL_VERSION,
            ok: true,
            payload: serde_json::json!({"records": []}),
        })
        .unwrap();

        assert_eq!(value["version"], CLI_PROTOCOL_VERSION);
        assert_eq!(value["ok"], true);
        assert_eq!(value["records"], serde_json::json!([]));
    }

    #[test]
    fn command_errors_use_their_protocol_versions() {
        let hook = Cli::try_parse_from(["worktree", "hook", "session-start", "--session", "test"])
            .unwrap();
        let reconcile = Cli::try_parse_from(["worktree", "reconcile", "--dry-run"]).unwrap();
        let status = Cli::try_parse_from(["worktree", "status"]).unwrap();

        assert_eq!(
            command_protocol_version(&hook.command),
            HOOK_PROTOCOL_VERSION
        );
        assert_eq!(
            command_protocol_version(&reconcile.command),
            RECONCILIATION_VERSION
        );
        assert_eq!(
            command_protocol_version(&status.command),
            CLI_PROTOCOL_VERSION
        );
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

    #[test]
    fn managed_guidance_refuses_malformed_markers_without_overwriting() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("AGENTS.md");
        let original = format!("{GUIDANCE_BEGIN}\nfirst\n{GUIDANCE_BEGIN}\nsecond\n");
        std::fs::write(&path, &original).unwrap();

        assert!(update_managed_block(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn managed_guidance_refuses_non_utf8_without_overwriting() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("AGENTS.md");
        let original = [0xff, 0xfe, b'\n'];
        std::fs::write(&path, original).unwrap();

        assert!(update_managed_block(&path, "replacement").is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn generated_guidance_requires_exact_reviewed_cleanup_ids() {
        let markdown = skill_markdown();
        let interface = skill_interface();

        assert!(markdown.contains("gc --repo <primary> --apply --id <reviewed-id>"));
        assert!(markdown.contains("interrupted provisioning"));
        assert!(markdown.contains("--allow-external-retirement"));
        assert!(interface.contains("$worktree"));
        assert!(interface.contains("Generated by `worktree skill`"));
    }

    #[test]
    fn external_retirement_confirmation_requires_apply() {
        assert!(
            Cli::try_parse_from(["worktree", "reconcile", "--allow-external-retirement",]).is_err()
        );
    }

    #[test]
    fn generated_skill_can_refresh_without_force() {
        let temporary = tempfile::tempdir().unwrap();
        let args = SkillArgs {
            out: temporary.path().join("worktree"),
            check: false,
            force: false,
        };

        render_skill(&args, false).unwrap();
        render_skill(&args, false).unwrap();
    }
}
