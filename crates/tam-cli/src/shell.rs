//! Shells from the command line (#83): `tam shell` opens one, `tam attach`
//! and `tam stop` take a shell's id, `tam ps` lists them, and the commands
//! that act on agents or tasks say when a name is a shell's. The dashboard
//! shares the wording (where a shell runs, what a drop closes).
//!
//! A shell has no name a command takes but its id (`sh-…`): every hint tam
//! prints names it by its id.

use std::path::{Path, PathBuf};

use anyhow::Result;
use tam_proto::{ShellInfo, ShellPlace};

use crate::client::{Client, Target};
use crate::schema;
use crate::script::{self, ErrorKind};

/// Where a shell runs, as the user knows it: its member's label (the task's
/// name, or `<task>/<member>`), else its repository.
pub fn place(shell: &ShellInfo, member_label: Option<String>) -> String {
    member_label.unwrap_or_else(|| shell.repo.clone())
}

/// `Shell #3 of fix-api`.
pub fn named(shell: &ShellInfo, place: &str) -> String {
    format!("{} of {place}", shell.name())
}

/// `1 shell`, `3 shells`.
pub fn count(n: usize) -> String {
    match n {
        1 => "1 shell".into(),
        n => format!("{n} shells"),
    }
}

/// What a drop closes of a task's (or a member's) `n` shells, when it has
/// any: `Its shell closes with it`, `Its 3 shells close with it`.
pub fn closes_with_it(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("Its shell closes with it".into()),
        n => Some(format!("Its {n} shells close with it")),
    }
}

/// How many of `shells` run in the worktrees of `members` (member ids).
pub fn of_members(shells: &[ShellInfo], members: &[String]) -> usize {
    shells
        .iter()
        .filter(|s| s.member.as_ref().is_some_and(|m| members.contains(m)))
        .count()
}

/// The error for a shell's id given to a command that does not take one
/// (`not`: what the command takes, e.g. `a task`). "Not found" (exit 3),
/// as a chat named where a task is needed.
pub fn misnamed(shell: &ShellInfo, not: &str) -> anyhow::Error {
    let id = &shell.id;
    script::error(
        ErrorKind::NotFound,
        format!(
            "'{id}' is a shell ({}), not {not}: tam attach {id} types into it, tam stop {id} \
             closes it",
            shell.name()
        ),
    )
}

/// `Shell #3 of fix-api`, its member's label asked of the daemon.
pub async fn describe(client: &mut Client, shell: &ShellInfo) -> String {
    let label = match &shell.member {
        Some(member) => match client.resolve_member(Some(member), None).await {
            Ok((task, member, _)) => Some(task.member_label(&member)),
            Err(_) => None,
        },
        None => None,
    };
    named(shell, &place(shell, label))
}

/// What `tam shell` was asked for.
pub struct NewShell {
    pub target: Option<String>,
    pub repo: Option<String>,
    pub detach: bool,
    pub json: bool,
}

/// `tam shell`: open a shell on a task member's worktree or a main
/// checkout, in our environment and through `[spawn] wrapper` (as the
/// dashboard does: `Client::open_shell`), then attach; or say what opened.
pub async fn open(new: NewShell) -> Result<()> {
    let mut client = Client::connect().await?;
    let (on, member) = match (&new.target, &new.repo) {
        (_, Some(repo)) => (ShellPlace::Checkout(crate::repos::named(repo)?), None),
        (Some(target), None) => named_place(&mut client, target).await?,
        (None, None) => place_here(&mut client).await?,
    };
    let shell = client.open_shell(on).await?;
    let (task, label) = member.unzip();
    let what = named(&shell, &place(&shell, label));

    if script::attaching(new.detach || new.json) {
        let client = Client::connect().await?;
        return client
            .attach(&shell.id, &format!("{what} ({})", shell.id))
            .await;
    }
    if new.json {
        return print_json(&schema::ShellDoc::new(&shell, task));
    }
    // The id alone on stdout: a script takes it from there.
    eprintln!(
        "Opened {what} in {}. Attach with: tam attach {}",
        crate::shorten_home(&shell.dir.display().to_string()),
        shell.id
    );
    println!("{}", shell.id);
    Ok(())
}

/// Where a shell opens, and on a member, its task's name and the member's
/// label.
type Place = (ShellPlace, Option<(String, String)>);

/// Where `tam shell TARGET` opens: a task member's worktree (TARGET as
/// `tam run` takes it), or a chat's main checkout.
async fn named_place(client: &mut Client, target: &str) -> Result<Place> {
    match client.resolve_target(target).await? {
        // By id: the name may mean another member once resolved again.
        Target::Member(task, member) => {
            let label = task.member_label(&member);
            Ok((ShellPlace::Member(member.id), Some((task.name, label))))
        }
        Target::Chat(chat, _) => Ok((ShellPlace::Checkout(chat.dir), None)),
        Target::Shell(shell) => Err(misnamed(&shell, "a task member or a chat")),
    }
}

/// Where `tam shell` opens with neither TARGET nor `-r`: the member whose
/// worktree holds the current directory, else the main checkout holding
/// it. A linked worktree no task has is refused: the shell would open
/// somewhere else than where it was asked for.
async fn place_here(client: &mut Client) -> Result<Place> {
    let cwd = std::fs::canonicalize(".")?;
    match client.resolve_member(None, Some(&cwd)).await {
        Ok((task, member, _)) => {
            let label = task.member_label(&member);
            return Ok((ShellPlace::Member(member.id), Some((task.name, label))));
        }
        Err(e) if script::kind_of(&e) == ErrorKind::Daemon => return Err(e),
        Err(_) => {}
    }
    Ok((ShellPlace::Checkout(checkout_here(&cwd)?), None))
}

/// The main checkout holding `cwd`, when `cwd` is in one (not in a linked
/// worktree).
fn checkout_here(cwd: &Path) -> Result<PathBuf> {
    let none = || {
        script::error(
            ErrorKind::NotFound,
            "no task or repository in the current directory: run this from a task's worktree \
             or a project, or name one (tam shell TASK, tam shell -r REPO)",
        )
    };
    let root = crate::repos::main_checkout(cwd).map_err(|_| none())?;
    let top = tam_worktree::git::toplevel(cwd)
        .and_then(|top| Ok(top.canonicalize()?))
        .map_err(|_| none())?;
    if top != root {
        return Err(script::error(
            ErrorKind::NotFound,
            format!(
                "no task has the worktree {}: tam adopt makes it a task's, tam shell -r . \
                 opens a shell on its repository's main checkout",
                crate::shorten_home(&top.display().to_string())
            ),
        ));
    }
    Ok(root)
}

/// `tam stop SHELL`: close it (hung up, and waited for). Nothing on disk
/// changes.
pub async fn close(client: &mut Client, shell: &ShellInfo) -> Result<()> {
    let what = describe(client, shell).await;
    let closed = client.close_shell(&shell.id).await?;
    println!("Closed {what} ({})", closed.id);
    Ok(())
}

/// `tam attach SHELL`.
pub async fn attach(client: &mut Client, shell: &ShellInfo) -> Result<()> {
    let what = describe(client, shell).await;
    let client = Client::connect().await?;
    client
        .attach(&shell.id, &format!("{what} ({})", shell.id))
        .await
}

fn print_json<T: serde::Serialize>(doc: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(doc)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;

    fn shell(member: Option<&str>, number: u32) -> ShellInfo {
        ShellInfo {
            id: format!("sh-0000000{number}"),
            member: member.map(str::to_string),
            dir: "/w/api--feat".into(),
            repo: "api".into(),
            number,
            pid: 7,
            opened_at: 10,
            viewers: 0,
        }
    }

    #[test]
    fn shells_are_named_by_where_they_run() {
        let on_member = shell(Some("m-1"), 3);
        assert_eq!(
            named(&on_member, &place(&on_member, Some("feat/api".into()))),
            "Shell #3 of feat/api"
        );
        let on_checkout = shell(None, 1);
        assert_eq!(
            named(&on_checkout, &place(&on_checkout, None)),
            "Shell #1 of api"
        );
    }

    #[test]
    fn what_a_drop_closes() {
        assert_eq!(count(1), "1 shell");
        assert_eq!(count(2), "2 shells");
        assert_eq!(closes_with_it(0), None);
        assert_eq!(closes_with_it(1).unwrap(), "Its shell closes with it");
        assert_eq!(closes_with_it(3).unwrap(), "Its 3 shells close with it");
        let shells = [
            shell(Some("m-1"), 1),
            shell(Some("m-1"), 2),
            shell(Some("m-2"), 1),
            shell(None, 1),
        ];
        assert_eq!(of_members(&shells, &["m-1".into()]), 2);
        assert_eq!(of_members(&shells, &["m-1".into(), "m-2".into()]), 3);
        assert_eq!(of_members(&shells, &[]), 0);
    }

    #[test]
    fn a_shells_id_where_it_does_not_belong_is_not_found() {
        let err = misnamed(&shell(None, 2), "a task");
        assert_eq!(script::kind_of(&err), ErrorKind::NotFound);
        assert_eq!(
            err.to_string(),
            "'sh-00000002' is a shell (Shell #2), not a task: tam attach sh-00000002 types \
             into it, tam stop sh-00000002 closes it"
        );
    }

    #[test]
    fn ps_lists_shells_with_their_task_every_key_present() {
        let task = tam_proto::TaskInfo {
            name: "feat".into(),
            members: vec![tam_proto::MemberInfo {
                id: "m-1".into(),
                name: "api".into(),
                repo: "api".into(),
                repo_root: None,
                branch: "feat".into(),
                dir: "/w/api--feat".into(),
                health: None,
            }],
            created_at: 0,
            run_count: 0,
            last_activity: None,
        };
        let doc = serde_json::to_value(schema::Ps::new(
            vec![(task, vec![])],
            vec![],
            vec![shell(Some("m-1"), 2), shell(None, 1)],
            &[],
        ))
        .unwrap();
        let shells = doc["shells"].as_array().unwrap();
        // In the order given (opened).
        assert_eq!(shells[0]["id"], "sh-00000002");
        assert_eq!(shells[0]["name"], "Shell #2");
        assert_eq!(shells[0]["number"], 2);
        assert_eq!(shells[0]["task"], "feat");
        assert_eq!(shells[0]["member"], "m-1");
        assert_eq!(shells[0]["dir"], "/w/api--feat");
        assert!(shells[1]["task"].is_null() && shells[1]["member"].is_null());
        let keys = |v: &serde_json::Value| -> Vec<String> {
            v.as_object().unwrap().keys().cloned().collect()
        };
        assert_eq!(keys(&shells[0]), keys(&shells[1]));

        let doc = serde_json::to_value(schema::ShellDoc::new(&shell(None, 1), None)).unwrap();
        assert_eq!(doc["schema_version"], 1);
        assert_eq!(doc["shell"], "sh-00000001");
        assert!(doc["task"].is_null() && doc["member"].is_null());
        assert_eq!(doc["repo"], "api");
    }

    #[test]
    fn the_shell_command_parses() {
        let parse = |args: &[&str]| Cli::try_parse_from(["tam"].iter().chain(args));
        match parse(&["shell", "feat/api", "-d", "--json"])
            .unwrap()
            .command
        {
            Some(Commands::Shell {
                target,
                repo,
                detach,
                json,
            }) => {
                assert_eq!(target.as_deref(), Some("feat/api"));
                assert!(repo.is_none() && detach && json);
            }
            _ => panic!("not a shell"),
        }
        match parse(&["shell", "-r", "api"]).unwrap().command {
            Some(Commands::Shell { target, repo, .. }) => {
                assert!(target.is_none());
                assert_eq!(repo.as_deref(), Some("api"));
            }
            _ => panic!("not a shell"),
        }
        // A target or a repository, not both.
        assert!(parse(&["shell", "feat", "-r", "api"]).is_err());
    }
}
