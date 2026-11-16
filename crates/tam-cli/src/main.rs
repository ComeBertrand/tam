use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;

mod cli;
mod client;
mod config;
mod drop_report;
mod hook;
mod task;
mod tui;

use cli::{Cli, Commands};
use tam_proto::{MemberInfo, RunInfo, SessionChoice, TaskInfo};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::load_config()?;

    let command = match cli.command {
        Some(cmd) => cmd,
        None => return tui::run().await,
    };

    match command {
        Commands::New {
            name,
            source,
            no_start,
        } => {
            let mut client = client::Client::connect().await?;
            if client.list_tasks().await?.iter().any(|t| t.name == name) {
                anyhow::bail!("task '{}' already exists", name);
            }

            // A task owns its worktree: branch NAME of the current repository.
            let cwd = std::fs::canonicalize(".")?;
            let wt_config = tam_worktree::config::load_config()?;
            let member = task::create_worktree(&name, source.as_deref(), &wt_config, &cwd)?;
            if wt_config.auto_init {
                tam_worktree::init::run(&member.dir)?;
            }

            let task = client.create_task(&name, vec![member]).await?;
            let member = task.members[0].clone();
            println!("Created task '{}' at {}", name, member.dir.display());

            if !no_start {
                let start = Start {
                    agent: None,
                    session: SessionChoice::New,
                    prompt: None,
                    args: vec![],
                };
                run_and_attach(&mut client, &task, &member, &config, start).await?;
            }
        }

        Commands::Run {
            name,
            new_session,
            session,
            agent,
            prompt,
            args,
        } => {
            let mut client = client::Client::connect().await?;
            let (task, member, _) = resolve_member(&mut client, name.as_deref()).await?;
            config::validate_provider(agent.as_ref().unwrap_or(&config.default_agent))?;

            // The daemon resumes the member's current session by default.
            let session = match session {
                Some(id) => SessionChoice::Id(id),
                None if new_session => SessionChoice::New,
                None => SessionChoice::Current,
            };
            let start = Start {
                agent,
                session,
                prompt,
                args,
            };
            run_and_attach(&mut client, &task, &member, &config, start).await?;
        }

        Commands::Stop { name } => {
            let mut client = client::Client::connect().await?;
            // A task name stops all its members; TASK/MEMBER or the current
            // directory, one member.
            let task = match name.as_deref() {
                Some(name) => client.get_task(name).await.ok().map(|(task, _)| task),
                None => None,
            };
            let (label, task, members) = match task {
                Some(task) => (task.name.clone(), task.clone(), task.members),
                None => {
                    let (task, member, _) = resolve_member(&mut client, name.as_deref()).await?;
                    (task.member_label(&member), task, vec![member])
                }
            };

            let running = client.list_agents().await?;
            let mut stopped = 0;
            for member in members
                .iter()
                .filter(|m| running.iter().any(|a| a.id == m.id))
            {
                // The daemon records the end of the run.
                let resp = client
                    .send(tam_proto::Request::Kill {
                        id: member.id.clone(),
                    })
                    .await?;
                match resp {
                    tam_proto::Response::Ok => {
                        println!("Stopped agent in task '{}'", task.member_label(member));
                        stopped += 1;
                    }
                    tam_proto::Response::Error { message } => eprintln!("Error: {}", message),
                    _ => {}
                }
            }
            if stopped == 0 {
                eprintln!("Error: no agent running in task '{}'", label);
                std::process::exit(1);
            }
        }

        Commands::Attach { name } => {
            let mut client = client::Client::connect().await?;
            let (task, member, _) = resolve_member(&mut client, name.as_deref()).await?;
            let label = task.member_label(&member);
            if !client
                .list_agents()
                .await?
                .iter()
                .any(|a| a.id == member.id)
            {
                anyhow::bail!("no agent running in task '{}'", label);
            }
            client.attach(&member.id, &label).await?;
        }

        Commands::Drop {
            name,
            branch,
            force,
        } => {
            let mut client = client::Client::connect().await?;
            let mut outcome = client.drop_task(&name, force, branch).await?;
            if let client::DropOutcome::Refused { worktrees } = &outcome {
                eprintln!("Task '{name}' not dropped:");
                for line in drop_report::lines(worktrees) {
                    eprintln!("  {line}");
                }
                if !confirm("Drop anyway? [y/N] ")? {
                    eprintln!("Task '{name}' kept (pass --force to drop anyway).");
                    std::process::exit(1);
                }
                outcome = client.drop_task(&name, true, branch).await?;
            }
            match outcome {
                client::DropOutcome::Dropped { warnings } => {
                    for warning in warnings {
                        eprintln!("warning: {warning}");
                    }
                    println!("Dropped task '{name}'");
                }
                client::DropOutcome::Refused { .. } => {
                    anyhow::bail!("the daemon refused a forced drop")
                }
            }
        }

        Commands::Ps { json } => {
            let mut client = client::Client::connect().await?;
            let infos = client.list_tasks().await?;
            let agents = client.list_agents().await.unwrap_or_default();

            let mut tasks: Vec<task::Task> = infos
                .into_iter()
                .map(|info| task::Task::from_info(info, &agents))
                .collect();

            // Populate git branch status for members without a running agent
            for t in &mut tasks {
                t.refresh_git_status();
            }

            tasks.sort_by_key(|t| (t.status().sort_priority(), t.name.clone()));

            if json {
                let entries: Vec<serde_json::Value> = tasks
                    .iter()
                    .map(|t| {
                        let members: Vec<serde_json::Value> = t
                            .members
                            .iter()
                            .map(|m| {
                                serde_json::json!({
                                    "id": m.info.id,
                                    "name": m.info.name,
                                    "repo": m.info.repo,
                                    "branch": m.info.branch,
                                    "dir": m.info.dir,
                                    "status": t.member_status(m).to_string(),
                                    "agent": m.agent_info.as_ref().map(|a| &a.provider),
                                })
                            })
                            .collect();
                        serde_json::json!({
                            "name": t.name,
                            "status": t.status().to_string(),
                            "dir": t.dir(),
                            "agent": t.agent_info().map(|a| &a.provider),
                            "context_percent": t.agent_info().and_then(|a| a.context_percent),
                            "run_count": t.run_count,
                            "last_activity": t.last_activity,
                            "members": members,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if tasks.is_empty() {
                println!("No tasks.");
            } else {
                println!(
                    "{:<12} {:<15} {:<10} {:>5} {:>5} {:<30} {:>5}",
                    "STATUS", "TASK", "AGENT", "RUNS", "LAST", "DIR", "CTX"
                );
                for t in &tasks {
                    let dir = t
                        .dir()
                        .map(|d| shorten_home(&d.display().to_string()))
                        .unwrap_or_else(|| "-".into());
                    let agent = t.agent_info().map(|a| a.provider.as_str()).unwrap_or("-");
                    let ctx = t
                        .agent_info()
                        .and_then(|a| a.context_percent)
                        .map(|p| format!("{}%", p))
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{:<12} {:<15} {:<10} {:>5} {:>5} {:<30} {:>5}",
                        t.status().indicator(),
                        t.name,
                        agent,
                        t.run_count,
                        format_age(t.last_activity),
                        dir,
                        ctx,
                    );
                }
            }
        }

        Commands::Ls {
            path,
            json,
            raw,
            porcelain,
        } => {
            let wt_config = tam_worktree::config::load_config()?;
            let root = path.unwrap_or_else(|| {
                dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
            });
            let ignore = tam_worktree::discovery::build_ignore_set(&wt_config.ignore)?;
            let paths = tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth)?;

            if json {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                let json_entries: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| serde_json::json!({"path": e.path, "pretty": e.display_name}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_entries)?);
            } else if raw {
                for p in &paths {
                    println!("{}", p.display());
                }
            } else if porcelain {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                for entry in &entries {
                    println!("{}", entry.display_name);
                }
            } else {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                let lines = tam_worktree::pretty::build_tree_output(&entries);
                for line in &lines {
                    println!("{}", line);
                }
            }
        }

        Commands::Pick { finder } => {
            let wt_config = tam_worktree::config::load_config()?;
            let root = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            let ignore = tam_worktree::discovery::build_ignore_set(&wt_config.ignore)?;
            let paths = tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth)?;
            let entries = tam_worktree::pretty::build_pretty_names(&paths);

            // Pipe through fzf or configured finder
            use std::io::Write;
            use std::process::{Command, Stdio};

            let finder = finder
                .as_deref()
                .or(config.finder.as_deref())
                .unwrap_or("fzf");
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(finder)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|_| anyhow::anyhow!("finder '{}' not found", finder))?;

            let mut stdin = child.stdin.take().unwrap();
            for entry in &entries {
                writeln!(stdin, "{}", entry.display_name)?;
            }
            drop(stdin);

            let output = child.wait_with_output()?;
            if !output.status.success() {
                std::process::exit(1);
            }
            let choice = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Ok(path) = tam_worktree::pretty::resolve(&choice, &paths) {
                println!("{}", path.display());
            }
        }

        Commands::Init { agent } => {
            config::validate_provider(&agent)?;
            config::remove_legacy_hooks(&agent)?;
        }

        Commands::Shutdown => {
            let mut client = client::Client::connect().await?;
            let resp = client.send(tam_proto::Request::Shutdown).await?;
            match resp {
                tam_proto::Response::Ok => println!("Daemon shutting down."),
                tam_proto::Response::Error { message } => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                _ => {}
            }
        }

        Commands::Status => match client::Client::try_connect().await? {
            Some(_) => println!("Daemon is running."),
            None => {
                println!("Daemon is not running.");
                std::process::exit(1);
            }
        },

        Commands::Daemon => {
            use tracing_subscriber::EnvFilter;
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .init();
            #[cfg(unix)]
            {
                let _ = nix::unistd::setsid();
            }
            let socket_path = tam_proto::default_socket_path();
            let ledger_path = tam_daemon::ledger::default_path();
            let d = tam_daemon::daemon::Daemon::new(socket_path, ledger_path);
            return d.run().await;
        }

        Commands::HookNotify {
            agent,
            socket,
            event,
        } => hook::notify(agent, socket, event).await,
    }

    Ok(())
}

/// How to start a member's agent.
struct Start {
    /// Unset: the member's last agent, else the configured default.
    agent: Option<String>,
    session: SessionChoice,
    prompt: Option<String>,
    args: Vec<String>,
}

/// Start an agent for a task member (the daemon picks the session, runs it
/// in the member's worktree and records the run), then attach.
async fn run_and_attach(
    client: &mut client::Client,
    task: &TaskInfo,
    member: &MemberInfo,
    config: &config::Config,
    start: Start,
) -> Result<()> {
    let (id, _resumed) = client
        .run_agent(
            &member.id,
            start.agent,
            &config.default_agent,
            start.session,
            start.prompt,
            start.args,
        )
        .await?;
    let client = client::Client::connect().await?;
    client.attach(&id, &task.member_label(member)).await
}

/// Ask a yes/no question on the terminal; `false` without one.
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

/// Find the member a command acts on: named (a task, `<task>/<member>` or a
/// member id), else the one whose worktree contains the current directory.
async fn resolve_member(
    client: &mut client::Client,
    name: Option<&str>,
) -> Result<(TaskInfo, MemberInfo, Vec<RunInfo>)> {
    match name {
        Some(name) => client.resolve_member(Some(name), None).await,
        None => {
            let cwd = std::fs::canonicalize(".")?;
            client
                .resolve_member(None, Some(&cwd))
                .await
                .map_err(|_| anyhow::anyhow!("no task in current directory"))
        }
    }
}

fn format_age(timestamp: Option<u64>) -> String {
    let Some(ts) = timestamp else {
        return "-".into();
    };
    let now = task::now();
    let elapsed = now.saturating_sub(ts);
    if elapsed < 60 {
        "now".into()
    } else if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h", elapsed / 3600)
    } else {
        format!("{}d", elapsed / 86400)
    }
}

fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_shorten_home() {
        assert_eq!(shorten_home("/other/path"), "/other/path");
        if let Ok(home) = std::env::var("HOME") {
            let input = format!("{}/projects/foo", home);
            assert_eq!(shorten_home(&input), "~/projects/foo");
        }
    }

    #[test]
    fn test_format_age_none() {
        assert_eq!(format_age(None), "-");
    }

    #[test]
    fn test_format_age_recent() {
        let ts = task::now();
        assert_eq!(format_age(Some(ts)), "now");
    }

    #[test]
    fn test_format_age_minutes() {
        let ts = task::now() - 300; // 5 minutes ago
        assert_eq!(format_age(Some(ts)), "5m");
    }

    #[test]
    fn test_format_age_hours() {
        let ts = task::now() - 7200; // 2 hours ago
        assert_eq!(format_age(Some(ts)), "2h");
    }

    #[test]
    fn test_format_age_days() {
        let ts = task::now() - 172800; // 2 days ago
        assert_eq!(format_age(Some(ts)), "2d");
    }

    /// Helper: create a git repo with an initial commit.
    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .unwrap()
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(path.join("README.md"), "# test").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
    }

    #[test]
    fn test_worktree_flow() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        init_git_repo(&repo);

        // Create a worktree via tam-worktree
        let wt_config = tam_worktree::config::Config {
            max_depth: 3,
            ignore: vec![],
            worktree_root: tmp.path().to_path_buf(),
            auto_init: false,
        };
        let wt_path = tam_worktree::worktree::create("test-feat", None, &wt_config, &repo).unwrap();
        assert!(wt_path.exists());

        // Delete worktree
        tam_worktree::worktree::remove(&repo, &wt_path, false).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_task_status_with_git() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        init_git_repo(&repo);

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap()
        };

        // Create a worktree so we have a real owned task dir
        let wt_config = tam_worktree::config::Config {
            max_depth: 3,
            ignore: vec![],
            worktree_root: tmp.path().to_path_buf(),
            auto_init: false,
        };
        let wt_path =
            tam_worktree::worktree::create("test-branch", None, &wt_config, &repo).unwrap();

        // check_git_branch_status uses repo_root which resolves from the worktree.
        let root = tam_worktree::git::repo_root(&wt_path).unwrap();
        assert!(
            root.join(".git").exists(),
            "repo_root should find main repo"
        );

        // Fresh branch should be Active
        assert!(tam_worktree::git::local_branch_exists(&root, "test-branch").unwrap());
        let status = task::check_git_branch_status("test-branch", &wt_path);
        assert_eq!(
            status,
            task::GitBranchStatus::Active,
            "new branch should be active"
        );

        // Make a commit on the worktree branch — still active
        std::fs::write(wt_path.join("new.txt"), "content").unwrap();
        let git_wt = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&wt_path)
                .output()
                .unwrap()
        };
        git_wt(&["add", "."]);
        git_wt(&["commit", "-m", "diverge"]);

        let status = task::check_git_branch_status("test-branch", &wt_path);
        assert_eq!(status, task::GitBranchStatus::Active);

        // Merge it back — branch still exists, still Active
        git(&["merge", "--no-ff", "test-branch", "-m", "Merge test-branch"]);
        let status = task::check_git_branch_status("test-branch", &wt_path);
        assert_eq!(status, task::GitBranchStatus::Active);

        // Remove worktree and delete branch → Gone
        tam_worktree::worktree::remove(&repo, &wt_path, false).unwrap();
        git(&["branch", "-d", "test-branch"]);
        let status = task::check_git_branch_status("test-branch", &repo);
        assert_eq!(status, task::GitBranchStatus::Gone);
    }
}
