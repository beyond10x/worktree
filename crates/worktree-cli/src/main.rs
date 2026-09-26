//! Command-line composition root for the worktree lifecycle service.

use anyhow::{Context, Result, anyhow};
use b10x_worktree::{
    GitPort, ReadinessFailure, ReadinessObservation, RegistryPort, SystemClock, WorktreeManager,
    readiness_failures,
};
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
    /// Inspect actual Git state, storage, leases, and reasons a checkout is retained.
    Inspect(InspectArgs),
    /// Archive a tree's local-only commits and uncommitted state as local recovery proof.
    Archive(ArchiveArgs),
    /// Mark an idle worktree finished; it must be clean or exactly match its archive.
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
struct ArchiveArgs {
    /// Managed worktree path; defaults to the current directory.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Move an existing archive aside (never deleted) and write a new one.
    #[arg(long)]
    replace: bool,
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
struct InspectArgs {
    /// Inspect only this repository by default (unlike gc's profile-wide scope).
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Explicitly include every repository in the selected activated workspace.
    #[arg(long)]
    workspace: bool,
    /// Limit inspection to exact registered ids; repeat for multiple records.
    #[arg(long = "id")]
    ids: Vec<String>,
    /// Refresh remote-ref evidence; may fetch missing objects, never updates lifecycle.
    #[arg(long)]
    refresh: bool,
    /// Maximum filesystem entries examined per tree; partial sizes are reported explicitly.
    #[arg(long, default_value_t = 250_000, value_parser = clap::value_parser!(u64).range(1..))]
    max_entries: u64,
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
    /// Assert that this exact recorded commit is unrecoverable, abandoning the missing record
    /// that carries it; repeat per reviewed record. Refused while any ref still contains it.
    #[arg(
        long = "acknowledge-unrecoverable",
        value_name = "COMMIT",
        requires = "apply",
        conflicts_with = "dry_run"
    )]
    unrecoverable: Vec<String>,
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
        Command::Inspect(args) => inspect(args, cli.json),
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
        Command::Archive(args) => archive(args, cli.json),
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
    Ok(WorktreeManager::new(ProcessGit, registry, SystemClock).with_archive_root(archive_root()?))
}

/// Archives live beside the registry, under `<state home>/worktree/archives`.
fn archive_root() -> Result<PathBuf> {
    Ok(state_home()
        .map_err(anyhow::Error::new)?
        .join("worktree")
        .join("archives"))
}

#[derive(Serialize)]
struct ArchivePayload<'a> {
    archive: &'a b10x_worktree_domain::ArchiveEvidence,
}

fn archive(args: &ArchiveArgs, json: bool) -> Result<()> {
    let evidence = manager()?
        .archive(&args.path, args.replace)
        .map_err(anyhow::Error::new)?;
    emit_success(
        json,
        CLI_PROTOCOL_VERSION,
        ArchivePayload { archive: &evidence },
        || {
            let manifest = &evidence.manifest;
            let mut lines = vec![format!(
                "archived {} at {} to {}",
                manifest.id,
                manifest.head,
                evidence.path.display()
            )];
            lines.push(format!(
                "  {} local-only commit(s){}",
                manifest.unique_commits.len(),
                if manifest.bundle.is_some() {
                    " in commits.bundle"
                } else {
                    ""
                }
            ));
            if manifest.patch.is_some() {
                lines.push("  on-disk content that differs from HEAD in dirty.patch".into());
            }
            if let Some(blocker) = &evidence.blocker {
                lines.push(format!("  gc will still refuse this tree: {blocker}"));
            }
            if let Some(aside) = &evidence.superseded {
                lines.push(format!("  previous archive moved to {}", aside.display()));
            }
            lines.join("\n")
        },
    )
}

fn inspect(args: &InspectArgs, json: bool) -> Result<()> {
    let service = manager()?;
    let repository = ProcessGit
        .repository_snapshot(&args.repo)
        .map_err(anyhow::Error::new)?;
    let config =
        load_config(&config_path().map_err(anyhow::Error::new)?).map_err(anyhow::Error::new)?;
    let policy = resolve_policy(&config, &repository.root).map_err(anyhow::Error::new)?;
    let ids = args
        .ids
        .iter()
        .map(|id| WorktreeId::new(id.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::new)?;
    let report = service
        .inspect(
            policy,
            &repository.root,
            args.workspace,
            &ids,
            args.refresh,
            args.max_entries,
        )
        .map_err(anyhow::Error::new)?;
    emit_success(json, CLI_PROTOCOL_VERSION, &report, || {
        let mut lines = vec![format!("Inspection: {} ({})", repository.root.display(), if args.workspace { "workspace" } else { "repository" }),
            "Storage is observed allocation, not guaranteed reclaimable space. No lease does not prove abandonment. Work-item ownership/completion is unknown in the current registry.".into()];
        for item in &report.inspections {
            let size = item.details.as_ref().map_or_else(
                || "unknown bytes".into(),
                |details| {
                    format!(
                        "{} bytes{}",
                        details
                            .storage
                            .allocated_bytes
                            .unwrap_or(details.storage.logical_bytes),
                        if details.storage.complete {
                            ""
                        } else {
                            " (partial)"
                        }
                    )
                },
            );
            lines.push(format!(
                "{}\t{}\t{:?}\t{}",
                item.record.id,
                size,
                item.record.lifecycle,
                item.record.path.display()
            ));
            lines.push(format!(
                "  owner={} purpose={}",
                item.record.owner, item.record.purpose
            ));
            if let Some(details) = &item.details {
                lines.push(format!("  HEAD={} branch={} recorded-head-differs={:?} tracked={} untracked={} ignored={} live-leases={:?}", details.snapshot.head, if details.branch.is_empty() { "(detached)" } else { &details.branch }, item.recorded_head_differs, details.tracked_changes, details.untracked_entries, details.ignored_entries, item.live_leases));
                for child in details.storage.children.iter().take(5) {
                    lines.push(format!(
                        "  storage {}: {} bytes",
                        child.path.display(),
                        child.allocated_bytes.unwrap_or(child.logical_bytes)
                    ));
                }
            }
            for blocker in &item.blockers {
                lines.push(format!("  retained: {blocker}"));
            }
            if item.blockers.is_empty() {
                lines.push(
                    "  no observed blocker; review gc --dry-run before exact-id apply".into(),
                );
            }
        }
        lines.join("\n")
    })
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
                        let outcome = match &item.archive {
                            Some(archive) => {
                                format!("{outcome} (archive {})", archive.display())
                            }
                            None => outcome,
                        };
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
    let unrecoverable = parse_revisions(&args.unrecoverable)?;
    let assessments = manager()?
        .reconcile(
            policy,
            &ids,
            args.apply,
            args.allow_external_retirement,
            &unrecoverable,
        )
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
    let failures = readiness_failures(ReadinessObservation {
        git: report.git,
        config: report.config,
        registry: report.registry,
        profiles: report.profiles,
    });
    if check && !failures.is_empty() {
        let mut reasons = report.errors.clone();
        if failures.contains(&ReadinessFailure::NoActiveProfile) {
            reasons.push(ReadinessFailure::NoActiveProfile.to_string());
        }
        return Err(anyhow!("doctor checks failed: {}", reasons.join("; ")));
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

fn parse_revisions(revisions: &[String]) -> Result<Vec<GitRevision>> {
    revisions
        .iter()
        .map(|revision| GitRevision::new(revision.clone()).map_err(anyhow::Error::new))
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
        "{GUIDANCE_BEGIN}\n## Managed worktrees\n\nFor repository changes, invoke `$worktree` and use the `worktree` CLI. Create isolated trees with `worktree create`, keep primary checkouts clean, and publish commits before `worktree finish`. Review cleanup with `worktree gc --dry-run`, then pass only exact reviewed ids to `worktree gc --apply --id <id>`. Use `worktree reconcile` for interrupted provisioning, adopted legacy paths, and already-missing records; external retirement additionally requires explicit `--allow-external-retirement`, and abandoning a missing record whose recorded commit you have established is gone for good additionally requires `--acknowledge-unrecoverable <recorded-commit>`. Never force-remove or manually delete a managed tree.\n{GUIDANCE_END}\n"
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

Acquire a lease before changing the tree: run `worktree hook session-start --path <tree> --session <session-id>`. Use a stable id unique to this session. If the host does not demonstrably run lifecycle hooks, run these commands yourself; loading this skill does not install hooks.

Run `worktree hook heartbeat --path <tree> --session <session-id>` periodically during long work, before the configured lease expiry, including while builds or external checks are running. Check each result. A live lease blocks cleanup; a missing or expired lease does not prove that another session has stopped.

Release only your own lease with `worktree hook session-end --path <tree> --session <session-id>` when leaving or immediately before finish. Never clear another session's lease to make cleanup pass.

## Bound disposable storage

Before a large build, inspect free space and `worktree inspect --repo <primary> --id <id>`. Keep compiler caches and dependencies separate from source and retained evidence. Prefer a repository-supported cache location and bounded build settings; do not force a shared target directory across incompatible build configurations.

After verification, preserve the small logs, reports, or deliverables needed for review in their intended durable location. Remove only exact build or dependency directories known to be reproducible, owned by this task, and unused by any running process. Ignored files can contain valuable work: never blanket-delete them or use `git clean -fdx`. A worktree saves duplicate Git history; its build output still consumes disk and is not automatically reclaimed.

## Finish and clean up

1. Commit and publish every wanted change. A local-only commit is deliberately not cleanup-safe. Work merged as rebased or cherry-picked copies also qualifies when an advertised ref carries every unique commit's exact patch; GC reports that proof as `patch-equivalent`.
   When work must not be published, run `worktree archive <tree>` instead. It never modifies the tree; it writes `commits.bundle` (every commit no advertised ref holds), `dirty.patch` (tracked, untracked and ignored changes over HEAD) and a `worktree.archive/1` `manifest.json` below the state directory's `worktree/archives/<repository>/<id>/`, and verifies them. GC then accepts that archive as `archive` proof while HEAD and every file still match it exactly; any later commit or edit is refused as `archive-stale` until `worktree archive --replace <tree>` writes a new one. `--replace` moves the old archive aside and never deletes it.
2. Preserve required evidence and remove this task's disposable output as described above. Release your own lease, then run `worktree finish <tree>`. It refuses locked, unmanaged, live, or mid-operation Git worktrees, and dirty ones unless their archive holds exactly the current state.
3. Run `worktree gc --repo <primary> --dry-run --id <id>` and inspect every result. Without exact ids, `--repo` selects the activated workspace profile, not just the repository: the assessment covers records under that profile's `workspace_root`, including other repositories.
4. Run `worktree gc --repo <primary> --apply --id <reviewed-id>` with repeated `--id` values only for the exact results intended for removal. The command refreshes remote advertisements, fetches required objects, and revalidates immediately before non-forced removal. Check the result before reporting storage reclaimed.
5. End with either verified cleanup or an explicit handoff: tree id and path, published branch/commit, related work-item references, retained evidence, remaining blockers, next owner and next action. Never leave a tree silently active or label work complete merely from its age or Git state.

## Audit and recovery

- Run `worktree inspect --repo <path>` for actual Git state, separate ignored-file counts, storage, leases, and retention blockers. It defaults to that repository; add `--workspace` to expand to its profile and repeat `--id` to narrow the selection. Sizes are bounded observations, not promised reclaimable bytes. Add `--refresh` for fresh remote recovery evidence (which may fetch objects). Inspection never changes lifecycle or infers owner abandonment or story completion; review GC separately before removal.
- Run `worktree status` for durable lifecycle state. It accepts no filter and reports every record in every profile, so read `repository_root` on each one before acting.
- Run `worktree repo list --repo <path>` to distinguish managed, unmanaged, primary, and linked checkouts.
- Run `worktree reconcile --repo <path> --dry-run` to assess interrupted provisioning, adopted legacy paths, finished external trees, and missing records.
- Apply reconciliation only to ids copied from that immediately preceding dry-run with `worktree reconcile --repo <path> --apply --id <reviewed-id>` and repeated `--id` arguments when needed.
- If that dry-run explicitly proposes `retire-external`, confirm that destructive action separately by adding `--allow-external-retirement`; never add it for an unrelated migration or missing-record repair.
- A finished external legacy tree may supersede a stale migration intent only when the dry-run itself proposes `retire-external`; never reinterpret or bypass a cross-device or ambiguous-relocation refusal.
- If removal is interrupted while the path still exists, rerun GC dry-run and exact-id apply. If the path is already absent, use reconciliation dry-run and exact-id apply; its durable removal intent can safely finish the recorded transition.
- A missing Active record without matching durable removal intent stays refused while its work may still exist. Preserve and investigate its registry evidence; never edit the registry by hand, delete related state, or fabricate recovery proof. If its recorded commit still exists anywhere, publish it and rerun the dry-run.
- Only once you have established that such a record's recorded commit is gone for good, abandon it with `worktree reconcile --repo <path> --apply --id <reviewed-id> --acknowledge-unrecoverable <recorded-commit>`. That acknowledgement asserts one exact commit named by the immediately preceding dry-run; the command still checks it and refuses while any local branch, tag, remote-tracking ref, or remote advertisement contains it. It deletes nothing from disk or from Git, and records the tombstone with no recovery proof, because there is none to record.
- An archive outlives the tree it retired. Restore it from a `--no-checkout` clone that has the advertised refs: first write `* -text -eol -filter -ident -working-tree-encoding` to `.git/info/attributes` so that attributes cannot rewrite the archived bytes, then `git fetch <archive>/commits.bundle refs/worktree-archive/head:refs/heads/<name>`, and run both `switch <name>` and, when the archive has one, `apply --binary --whitespace=nowarn <archive>/dirty.patch` as `git -c core.autocrlf=false -c core.fileMode=true -c core.symlinks=true …`. Never use `--attr-source` for this: Git 2.55 `apply` crashes with it. Never delete an archive to make GC pass; `archive-digest-mismatch` and `archive-incomplete` mean it no longer proves recovery.
- `worktree-hidden-state` (assume-unchanged or skip-worktree entries, staged content only the index holds, a nested `.git`) and `worktree-local-refs` (refs under `refs/worktree/`, `refs/bisect/`, `refs/rewritten/`) retain a tree whether or not it is archived, because Git status does not show that state and removal would destroy it. Resolve the named state yourself; never clear it just to make GC pass.
- Run `worktree doctor --check` for prerequisites and configuration. It exits non-zero and names each failure, including `no active profile` when no workspace profile is activated.
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
        assert!(markdown.contains("--acknowledge-unrecoverable <recorded-commit>"));
        assert!(markdown.contains("gone for good"));
        assert!(markdown.contains("worktree archive <tree>"));
        assert!(markdown.contains("worktree archive --replace <tree>"));
        assert!(markdown.contains("archive-stale"));
        assert!(markdown.contains(".git/info/attributes"));
        assert!(markdown.contains("worktree-hidden-state"));
        assert!(markdown.contains("worktree-local-refs"));
        assert!(interface.contains("$worktree"));
        assert!(interface.contains("Generated by `worktree skill`"));
    }

    #[test]
    fn archive_defaults_to_the_current_tree_and_replaces_only_on_request() {
        let plain = Cli::try_parse_from(["worktree", "archive"]).unwrap();
        let Command::Archive(args) = &plain.command else {
            panic!("archive command");
        };
        assert_eq!(args.path, PathBuf::from("."));
        assert!(!args.replace);
        assert_eq!(
            command_protocol_version(&plain.command),
            CLI_PROTOCOL_VERSION
        );

        let replace = Cli::try_parse_from(["worktree", "archive", "--replace", "/tree"]).unwrap();
        let Command::Archive(args) = replace.command else {
            panic!("archive command");
        };
        assert_eq!(args.path, PathBuf::from("/tree"));
        assert!(args.replace);
    }

    #[test]
    fn external_retirement_confirmation_requires_apply() {
        assert!(
            Cli::try_parse_from(["worktree", "reconcile", "--allow-external-retirement",]).is_err()
        );
    }

    #[test]
    fn unrecoverable_acknowledgement_requires_a_reviewed_apply() {
        // Without --apply there is no reviewed dry-run behind the assertion.
        assert!(
            Cli::try_parse_from([
                "worktree",
                "reconcile",
                "--acknowledge-unrecoverable",
                "abc",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "worktree",
                "reconcile",
                "--dry-run",
                "--acknowledge-unrecoverable",
                "abc",
            ])
            .is_err()
        );
        let reviewed = Cli::try_parse_from([
            "worktree",
            "reconcile",
            "--apply",
            "--id",
            "missing-active",
            "--acknowledge-unrecoverable",
            "abc",
        ])
        .unwrap();
        let Command::Reconcile(args) = reviewed.command else {
            panic!("reconcile command");
        };
        assert_eq!(args.unrecoverable, vec!["abc".to_owned()]);
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
