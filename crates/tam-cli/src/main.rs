use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;

mod cli;
mod client;
mod config;
mod ledger;
mod sessions;
mod task;
mod tui;

use cli::{Cli, Commands};
use ledger::{Ledger, LedgerEvent};

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
            worktree,
            source,
        } => {
            let mut ledger = Ledger::load()?;

            if ledger.task_exists(&name) {
                anyhow::bail!("task '{}' already exists", name);
            }

            let worktree = worktree || source.is_some();

            if worktree {
                // Owned context: create a worktree
                let wt_config = tam_worktree::config::load_config()?;
                let cwd = std::fs::canonicalize(".")?;
                let wt_path =
                    tam_worktree::worktree::create(&name, source.as_deref(), &wt_config, &cwd)?;

                if wt_config.auto_init {
                    tam_worktree::init::run(&wt_path)?;
                }

                ledger.append(LedgerEvent::TaskCreated {
                    name: name.clone(),
                    dir: wt_path.clone(),
                    owned: true,
                    timestamp: ledger::now(),
                })?;

                println!("Created task '{}' at {}", name, wt_path.display());
            } else {
                // Borrowed context: bind to cwd
                let cwd = std::fs::canonicalize(".")?;

                if let Some(existing) = ledger.find_task_by_dir(&cwd) {
                    anyhow::bail!("directory already has an active task: '{}'", existing.name);
                }

                ledger.append(LedgerEvent::TaskCreated {
                    name: name.clone(),
                    dir: cwd.clone(),
                    owned: false,
                    timestamp: ledger::now(),
                })?;

                println!("Created task '{}' in {}", name, cwd.display());
            }
        }

        Commands::Run {
            name,
            new_session,
            agent,
            prompt,
            args,
        } => {
            let mut ledger = Ledger::load()?;
            let task = ledger
                .find_task(&name)
                .ok_or_else(|| anyhow::anyhow!("task '{}' not found", name))?;

            let agent = agent.unwrap_or_else(|| config.default_agent.clone());
            config::validate_provider(&agent)?;

            // Resolve session — cross-reference ledger runs with filesystem sessions
            let resume_session = if new_session || !std::io::stdin().is_terminal() {
                None
            } else {
                let runs = ledger.task_runs(&name);
                let found = sessions::list_sessions_for_task(&agent, &task.dir, &runs);
                if found.is_empty() {
                    None
                } else {
                    config::pick_session(&found)?
                }
            };

            let mut client = client::Client::connect().await?;
            let resp = client
                .send(tam_proto::Request::Spawn {
                    provider: agent.clone(),
                    dir: task.dir.clone(),
                    id: Some(name.clone()),
                    args,
                    resume_session: resume_session.clone(),
                    prompt,
                })
                .await?;

            match resp {
                tam_proto::Response::Spawned { id } => {
                    ledger.append(LedgerEvent::AgentRunStarted {
                        task: name.clone(),
                        provider: agent,
                        session_id: resume_session,
                        timestamp: ledger::now(),
                    })?;

                    // Attach immediately
                    let client = client::Client::connect().await?;
                    client.attach(&id).await?;
                }
                tam_proto::Response::Error { message } => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                _ => {}
            }
        }

        Commands::Stop { name } => {
            let mut ledger = Ledger::load()?;
            let name = resolve_task_name(name, &ledger)?;

            let mut client = client::Client::connect().await?;
            let resp = client
                .send(tam_proto::Request::Kill { id: name.clone() })
                .await?;
            match resp {
                tam_proto::Response::Ok => {
                    ledger.append(LedgerEvent::AgentRunEnded {
                        task: name.clone(),
                        exit_code: -1,
                        timestamp: ledger::now(),
                    })?;
                    println!("Stopped agent in task '{}'", name);
                }
                tam_proto::Response::Error { message } => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                _ => {}
            }
        }

        Commands::Attach { name } => {
            let ledger = Ledger::load()?;
            let name = resolve_task_name(name, &ledger)?;

            let client = client::Client::connect().await?;
            client.attach(&name).await?;
        }

        Commands::Drop { name, branch } => {
            let mut ledger = Ledger::load()?;
            let task = ledger
                .find_task(&name)
                .ok_or_else(|| anyhow::anyhow!("task '{}' not found", name))?;

            // Kill agent if running
            if let Ok(mut client) = client::Client::connect().await {
                let _ = client
                    .send(tam_proto::Request::Kill { id: name.clone() })
                    .await;
            }

            // Delete worktree if owned
            if task.owned {
                let wt_config = tam_worktree::config::load_config()?;
                // Find repo root from the worktree dir (or cwd as fallback)
                let cwd = if task.dir.exists() {
                    task.dir.clone()
                } else {
                    std::fs::canonicalize(".")?
                };
                if task.dir.exists() {
                    tam_worktree::worktree::delete(&name, branch, true, &wt_config, &cwd)?;
                }
                ledger.append(LedgerEvent::WorktreeDeleted {
                    task: name.clone(),
                    timestamp: ledger::now(),
                })?;
            }

            ledger.append(LedgerEvent::TaskDropped {
                task: name.clone(),
                timestamp: ledger::now(),
            })?;

            println!("Dropped task '{}'", name);
        }

        Commands::Gc { dry_run } => {
            let mut ledger = Ledger::load()?;
            let tasks = ledger.active_tasks();
            let mut dropped = Vec::new();

            for task in &tasks {
                if !task.owned {
                    continue;
                }
                if let Ok(root) = tam_worktree::git::repo_root(&task.dir) {
                    if let Ok(default) = tam_worktree::git::default_branch(&root) {
                        if tam_worktree::git::is_branch_merged(&root, &task.name, &default)
                            .unwrap_or(false)
                        {
                            dropped.push(task.name.clone());
                        }
                    }
                }
            }

            if dropped.is_empty() {
                println!("Nothing to clean up.");
            } else if dry_run {
                println!("Would drop:");
                for name in &dropped {
                    println!("  {}", name);
                }
            } else {
                for name in &dropped {
                    // Kill agent if running
                    if let Ok(mut client) = client::Client::connect().await {
                        let _ = client
                            .send(tam_proto::Request::Kill { id: name.clone() })
                            .await;
                    }
                    let task = ledger.find_task(name).unwrap();
                    if task.dir.exists() {
                        let wt_config = tam_worktree::config::load_config()?;
                        let _ =
                            tam_worktree::worktree::delete(name, true, true, &wt_config, &task.dir);
                    }
                    ledger.append(LedgerEvent::TaskDropped {
                        task: name.clone(),
                        timestamp: ledger::now(),
                    })?;
                    println!("Dropped '{}'", name);
                }
            }
        }

        Commands::Ps { json } => {
            let ledger = Ledger::load()?;
            let snapshots = ledger.active_tasks();

            // Get running agents from daemon
            let agents = if let Ok(mut client) = client::Client::connect().await {
                match client.send(tam_proto::Request::List).await {
                    Ok(tam_proto::Response::Agents { agents }) => agents,
                    _ => vec![],
                }
            } else {
                vec![]
            };

            let mut tasks: Vec<task::Task> = snapshots
                .into_iter()
                .map(|s| {
                    let agent_info = agents.iter().find(|a| a.id == s.name).cloned();
                    task::Task::from_snapshot(s, agent_info)
                })
                .collect();

            // Populate git branch status for owned tasks without a running agent
            for t in &mut tasks {
                if t.owned && t.agent_info.is_none() {
                    t.git_branch_status = task::check_git_branch_status(&t.name, &t.dir);
                }
            }

            tasks.sort_by_key(|t| (t.status().sort_priority(), t.name.clone()));

            if json {
                let entries: Vec<serde_json::Value> = tasks
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "status": t.status().to_string(),
                            "dir": t.dir,
                            "owned": t.owned,
                            "agent": t.agent_info.as_ref().map(|a| &a.provider),
                            "context_percent": t.agent_info.as_ref().and_then(|a| a.context_percent),
                            "run_count": t.run_count,
                            "last_activity": t.last_activity,
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
                    let dir = shorten_home(&t.dir.display().to_string());
                    let agent = t
                        .agent_info
                        .as_ref()
                        .map(|a| a.provider.as_str())
                        .unwrap_or("-");
                    let ctx = t
                        .agent_info
                        .as_ref()
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

        Commands::Ls { path, json, raw } => {
            let wt_config = tam_worktree::config::load_config()?;
            let root = path.unwrap_or_else(|| {
                dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
            });
            let ignore = tam_worktree::discovery::build_ignore_set(&wt_config.ignore)?;
            let paths = tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth)?;

            if json {
                let entries: Vec<serde_json::Value> = paths
                    .iter()
                    .map(|p| serde_json::json!({"path": p}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if raw {
                for p in &paths {
                    println!("{}", p.display());
                }
            } else {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                let lines = tam_worktree::pretty::build_tree_output(&entries);
                for line in &lines {
                    println!("{}", line);
                }
            }
        }

        Commands::Pick => {
            let wt_config = tam_worktree::config::load_config()?;
            let root = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            let ignore = tam_worktree::discovery::build_ignore_set(&wt_config.ignore)?;
            let paths = tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth)?;
            let entries = tam_worktree::pretty::build_pretty_names(&paths);

            // Pipe through fzf or configured finder
            use std::io::Write;
            use std::process::{Command, Stdio};

            let finder = config.finder.as_deref().unwrap_or("fzf");
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
            config::init_agent_hooks(&agent)?;
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
            let d = tam_daemon::daemon::Daemon::new(socket_path);
            return d.run().await;
        }

        Commands::HookNotify { agent, event } => {
            // Not running under tam — silently succeed so hooks don't block the agent
            let agent = match agent {
                Some(a) => a,
                None => {
                    // Also check ZINC_AGENT_ID for migration
                    match std::env::var("ZINC_AGENT_ID") {
                        Ok(a) => a,
                        Err(_) => return Ok(()),
                    }
                }
            };
            // Best-effort: don't block the agent
            if let Ok(mut client) = client::Client::connect().await {
                let _ = client
                    .send(tam_proto::Request::HookEvent {
                        agent_id: agent,
                        event,
                    })
                    .await;
            }
        }
    }

    Ok(())
}

/// Resolve a task name from explicit argument or from cwd.
fn resolve_task_name(name: Option<String>, ledger: &Ledger) -> Result<String> {
    if let Some(name) = name {
        return Ok(name);
    }
    let cwd = std::fs::canonicalize(".")?;
    match ledger.find_task_by_dir(&cwd) {
        Some(task) => Ok(task.name),
        None => anyhow::bail!("no task in current directory"),
    }
}

fn format_age(timestamp: Option<u64>) -> String {
    let Some(ts) = timestamp else {
        return "-".into();
    };
    let now = ledger::now();
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

    #[test]
    fn test_shorten_home() {
        assert_eq!(shorten_home("/other/path"), "/other/path");
        if let Ok(home) = std::env::var("HOME") {
            let input = format!("{}/projects/foo", home);
            assert_eq!(shorten_home(&input), "~/projects/foo");
        }
    }
}
