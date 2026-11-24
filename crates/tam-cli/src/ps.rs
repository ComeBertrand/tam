//! `tam ps`'s text: the chats first, a row each, with their ids; then the
//! shells on main checkouts, with their ids; then a row per task, and below
//! a task with several members a row per member (its status, worktree and
//! context), and below a member (or a task of one) a row per shell in its
//! worktree, as in the dashboard's list; then what is wrong with worktrees.
//! `tam ps --json` is `script::ps`.

use crate::chat::{time_left, Chat, Closes};
use crate::health;
use crate::task::{Member, Task};
use crate::{format_age, shorten_home};
use tam_proto::ShellInfo;

const HEADER: [&str; 6] = ["STATUS", "TASK", "RUNS", "LAST", "DIR", "CTX"];
/// The columns aligned to the right: RUNS, LAST, CTX.
const RIGHT: [bool; 6] = [false, false, true, true, false, true];
const SPACING: &str = "  ";

/// What `tam ps` prints, line by line, the tasks in the order given, each
/// member's shells (of `shells`) below it.
pub fn lines(tasks: &[Task], shells: &[ShellInfo]) -> Vec<String> {
    if tasks.is_empty() {
        return vec!["No tasks.".into()];
    }
    let mut rows = vec![HEADER.map(String::from)];
    for task in tasks {
        let runs = task.run_count.to_string();
        let last = format_age(task.last_activity);
        if let [member] = &task.members[..] {
            let [status, dir, ctx] = member_cells(task, member);
            rows.push([status, task.name.clone(), runs, last, dir, ctx]);
            rows.extend(shell_rows(shells, member, ""));
            continue;
        }
        // The task's status is its most urgent member's; the rest is each
        // member's, on its row.
        let status = task.status().indicator().to_string();
        rows.push([
            status,
            task.name.clone(),
            runs,
            last,
            String::new(),
            String::new(),
        ]);
        for (i, member) in task.members.iter().enumerate() {
            let tree = if i + 1 == task.members.len() {
                "└─"
            } else {
                "├─"
            };
            let name = format!("{tree} {}", member.info.name);
            let [status, dir, ctx] = member_cells(task, member);
            rows.push([status, name, String::new(), String::new(), dir, ctx]);
            let indent = if i + 1 == task.members.len() {
                "   "
            } else {
                "│  "
            };
            rows.extend(shell_rows(shells, member, indent));
        }
    }

    let mut out = table(&rows, RIGHT);

    // What is wrong with the worktrees, and what to do about it.
    let notes: Vec<String> = tasks
        .iter()
        .flat_map(|t| {
            t.members
                .iter()
                .filter_map(|m| health::ps_note(&t.member_label(m), &t.name, &m.info))
        })
        .collect();
    if !notes.is_empty() {
        out.push(String::new());
        out.extend(notes);
    }
    out
}

/// The rows of a member's shells, by number, below its row (`indent`: the
/// tree drawn so far): `└─ Shell #2 (sh-…)`, its id being how to name it.
/// No status, time or context: a shell has none.
fn shell_rows(shells: &[ShellInfo], member: &Member, indent: &str) -> Vec<[String; 6]> {
    let mut mine: Vec<&ShellInfo> = shells
        .iter()
        .filter(|s| s.member.as_deref() == Some(member.id()))
        .collect();
    mine.sort_by_key(|s| s.number);
    let last = mine.len().saturating_sub(1);
    mine.iter()
        .enumerate()
        .map(|(i, shell)| {
            let tree = if i == last { "└─" } else { "├─" };
            let mut row: [String; 6] = Default::default();
            row[1] = format!("{indent}{tree} {} ({})", shell.name(), shell.id);
            row
        })
        .collect()
}

/// The columns of the table of the shells on main checkouts: named by
/// their ids, where they run as chats show it.
const SHELL_HEADER: [&str; 3] = ["SHELL", "ID", "DIR"];

/// The table of the shells on main checkouts (those in a task's worktree
/// are below its member), in the order given; nothing without them.
pub fn shell_lines(shells: &[ShellInfo]) -> Vec<String> {
    let mut rows = vec![SHELL_HEADER.map(String::from)];
    for shell in shells.iter().filter(|s| s.member.is_none()) {
        rows.push([
            shell.name(),
            shell.id.clone(),
            shorten_home(&shell.dir.display().to_string()),
        ]);
    }
    if rows.len() == 1 {
        return Vec::new();
    }
    table(&rows, [false; 3])
}

/// The columns of the chats' table: a chat is shown by its label (its
/// name, else Claude's title, else its id) and named by its id; CLOSES is
/// when autoclean closes it.
const CHAT_HEADER: [&str; 8] = [
    "STATUS", "CHAT", "ID", "RUNS", "LAST", "CLOSES", "DIR", "CTX",
];
const CHAT_RIGHT: [bool; 8] = [false, false, false, true, true, true, false, true];

/// The chats' table, in the order given; nothing without chats.
pub fn chat_lines(chats: &[Chat]) -> Vec<String> {
    if chats.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![CHAT_HEADER.map(String::from)];
    for chat in chats {
        let info = &chat.info;
        rows.push([
            chat.status().indicator().into(),
            info.label(),
            info.id.clone(),
            info.run_count.to_string(),
            format_age(Some(chat.last())),
            closes(&chat.info, crate::task::now()),
            shorten_home(&info.dir.display().to_string()),
            context(chat.agent.as_ref()),
        ]);
    }
    table(&rows, CHAT_RIGHT)
}

/// When autoclean closes the chat, from `now`: within how long, rounded
/// up (`in 3d` from a turn just now), `due` once that time is past (it
/// closes at the prompt with nobody attached), `kept`, or `-` when
/// autoclean is off.
fn closes(chat: &tam_proto::ChatInfo, now: u64) -> String {
    match Closes::of(chat, now) {
        Closes::Kept => "kept".into(),
        Closes::Off => "-".into(),
        Closes::Due => "due".into(),
        Closes::In(left) => format!("in {}", time_left(left)),
    }
}

/// Rows as lines, each column as wide as its widest cell.
fn table<const N: usize>(rows: &[[String; N]], right: [bool; N]) -> Vec<String> {
    let width = |c: usize| rows.iter().map(|r| r[c].chars().count()).max();
    let widths: Vec<usize> = (0..N).map(|c| width(c).unwrap_or(0)).collect();
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .zip(right)
                .map(|((cell, &w), right)| {
                    if right {
                        format!("{cell:>w$}")
                    } else {
                        format!("{cell:<w$}")
                    }
                })
                .collect();
            cells.join(SPACING).trim_end().to_string()
        })
        .collect()
}

/// An agent's context use, `-` without one.
fn context(agent: Option<&tam_proto::AgentInfo>) -> String {
    agent
        .and_then(|a| a.context_percent)
        .map_or_else(|| "-".into(), |p| format!("{p}%"))
}

/// A member's status, worktree and context.
fn member_cells(task: &Task, member: &Member) -> [String; 3] {
    [
        task.member_status(member).indicator().into(),
        shorten_home(&member.info.dir.display().to_string()),
        context(member.agent_info.as_ref()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tam_proto::{AgentDetail, AgentInfo, AgentState, MemberHealth, MemberInfo, TaskInfo};

    fn member(id: &str, name: &str, branch: &str) -> MemberInfo {
        MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: name.into(),
            repo_root: None,
            branch: branch.into(),
            dir: format!("/w/{name}--{branch}").into(),
            health: Some(MemberHealth::Ok {
                branch: Some(branch.into()),
            }),
        }
    }

    fn agent(id: &str, state: AgentState, ctx: Option<u8>) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            provider: "claude".into(),
            dir: "/w".into(),
            state,
            pid: Some(1),
            uptime_secs: 60,
            viewers: 0,
            context_percent: ctx,
            task: None,
            detail: Box::new(AgentDetail::default()),
            state_since: None,
        }
    }

    fn task(name: &str, members: Vec<MemberInfo>, runs: usize, agents: &[AgentInfo]) -> Task {
        let info = TaskInfo {
            name: name.into(),
            members,
            created_at: 0,
            run_count: runs,
            last_activity: Some(crate::task::now() - 300),
        };
        Task::from_info(info, agents)
    }

    /// Where `needle` starts in `line`, in characters.
    fn column(line: &str, needle: &str) -> usize {
        let at = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {line:?}"));
        line[..at].chars().count()
    }

    #[test]
    fn a_row_per_task_and_per_member_of_a_task_with_several() {
        let agents = [
            agent("m-fix", AgentState::Working, Some(45)),
            agent("m-api", AgentState::Idle, Some(12)),
            agent("m-web", AgentState::Ready, None),
        ];
        let tasks = [
            task(
                "api-and-web",
                vec![
                    member("m-api", "api", "api-and-web"),
                    member("m-web", "web", "api-and-web"),
                ],
                5,
                &agents,
            ),
            task(
                "fix-nav",
                vec![member("m-fix", "app", "fix-nav")],
                3,
                &agents,
            ),
        ];
        let lines = lines(&tasks, &[]);
        let shown = lines.join("\n");
        assert_eq!(lines.len(), 5, "{shown}");
        let [header, multi, api, web, single] = &lines[..] else {
            unreachable!()
        };

        // The task: its most urgent member's status, its runs and activity,
        // nothing that is a member's.
        assert!(multi.starts_with("◆ ready"), "{shown}");
        assert!(multi.ends_with("5    5m"), "{shown}");
        // Each member: its own status, worktree and context.
        assert!(api.starts_with("○ idle"), "{shown}");
        assert!(api.contains("├─ api"), "{shown}");
        assert!(api.ends_with("/w/api--api-and-web  12%"), "{shown}");
        assert!(web.starts_with("◆ ready"), "{shown}");
        assert!(web.contains("└─ web"), "{shown}");
        assert!(web.ends_with("/w/web--api-and-web    -"), "{shown}");
        // A single member is the task's row.
        assert!(single.starts_with("● run"), "{shown}");
        assert!(single.ends_with("/w/app--fix-nav      45%"), "{shown}");

        // Columns line up, long names included.
        assert_eq!(column(header, "TASK"), column(multi, "api-and-web"));
        assert_eq!(column(header, "TASK"), column(single, "fix-nav"));
        assert_eq!(column(header, "TASK"), column(api, "├─"));
        assert_eq!(column(header, "DIR"), column(api, "/w/"));
        assert_eq!(column(header, "DIR"), column(single, "/w/"));
    }

    #[test]
    fn notes_name_members_by_their_label() {
        let mut gone = member("m-web", "web", "both");
        gone.health = Some(MemberHealth::Gone {
            cannot_repair: None,
        });
        let tasks = [task(
            "both",
            vec![member("m-api", "api", "both"), gone],
            1,
            &[],
        )];
        let lines = lines(&tasks, &[]);
        let shown = lines.join("\n");
        assert!(lines[3].starts_with("∅ gone"), "{shown}");
        assert_eq!(lines[4], "", "{shown}");
        assert!(
            lines[5].starts_with("both/web: worktree /w/web--both is gone"),
            "{shown}"
        );
    }

    #[test]
    fn no_tasks() {
        assert_eq!(lines(&[], &[]), ["No tasks."]);
        assert!(chat_lines(&[]).is_empty());
        assert!(shell_lines(&[]).is_empty());
    }

    fn shell(id: &str, member: Option<&str>, number: u32, dir: &str) -> ShellInfo {
        ShellInfo {
            id: id.into(),
            member: member.map(str::to_string),
            dir: dir.into(),
            repo: "api".into(),
            number,
            pid: 1,
            opened_at: 0,
            viewers: 0,
        }
    }

    #[test]
    fn a_members_shells_are_below_it_named_by_their_ids() {
        let tasks = [
            task(
                "both",
                vec![
                    member("m-api", "api", "both"),
                    member("m-web", "web", "both"),
                ],
                1,
                &[],
            ),
            task("fix", vec![member("m-fix", "app", "fix")], 1, &[]),
        ];
        // Opened in any order; below their member by number.
        let shells = [
            shell("sh-0000fix3", Some("m-fix"), 3, "/w/app--fix"),
            shell("sh-0000api1", Some("m-api"), 1, "/w/api--both"),
            shell("sh-0000fix1", Some("m-fix"), 1, "/w/app--fix"),
            shell("sh-0000web1", Some("m-web"), 1, "/w/web--both"),
            // On a main checkout: in a table of its own.
            shell("sh-0000main", None, 1, "/src/api"),
        ];
        let lines = lines(&tasks, &shells);
        let shown = lines.join("\n");
        let [header, both, api, api_shell, web, web_shell, fix, fix1, fix3] = &lines[..] else {
            panic!("{shown}")
        };
        assert!(both.contains("both"), "{shown}");
        assert!(api.contains("├─ api"), "{shown}");
        assert!(web.contains("└─ web"), "{shown}");
        assert!(fix.contains("fix"), "{shown}");
        // No status, runs, time, directory or context: its name and id.
        assert_eq!(
            api_shell.trim_start(),
            "│  └─ Shell #1 (sh-0000api1)",
            "{shown}"
        );
        assert_eq!(
            web_shell.trim_start(),
            "└─ Shell #1 (sh-0000web1)",
            "{shown}"
        );
        assert_eq!(column(web_shell, "└─"), column(web, "└─") + 3, "{shown}");
        assert_eq!(fix1.trim_start(), "├─ Shell #1 (sh-0000fix1)", "{shown}");
        assert_eq!(fix3.trim_start(), "└─ Shell #3 (sh-0000fix3)", "{shown}");
        assert_eq!(column(header, "TASK"), column(fix1, "├─"), "{shown}");
        assert_eq!(column(header, "TASK"), column(api_shell, "│"), "{shown}");
        assert!(!shown.contains("sh-0000main"), "{shown}");

        let table = shell_lines(&shells);
        let shown = table.join("\n");
        let [header, main] = &table[..] else {
            panic!("{shown}")
        };
        assert_eq!(header, "SHELL     ID           DIR", "{shown}");
        assert_eq!(main, "Shell #1  sh-0000main  /src/api", "{shown}");
    }

    fn chat(id: &str, name: Option<&str>, agents: &[AgentInfo]) -> Chat {
        let info = tam_proto::ChatInfo {
            id: id.into(),
            name: name.map(str::to_string),
            repo: "api".into(),
            dir: "/src/api".into(),
            created_at: crate::task::now() - 2 * 86400,
            keep: false,
            session: None,
            last_turn_at: None,
            run_count: 1,
            title: None,
            last_prompt: None,
            closes_at: None,
        };
        Chat::new(info, agents)
    }

    #[test]
    fn closes_says_when_autoclean_closes_a_chat() {
        let mut info = chat("c-0000abcd", None, &[]).info;
        let now = 1_000_000;
        for (at, shown) in [
            (None, "-"),
            (Some(now - 1), "due"),
            (Some(now), "due"),
            (Some(now + 30), "in 1m"),
            (Some(now + 600), "in 10m"),
            (Some(now + 3599), "in 1h"),
            (Some(now + 5 * 3600), "in 5h"),
            (Some(now + 86399), "in 1d"),
            (Some(now + 3 * 86400), "in 3d"),
            (Some(now + 3 * 86400 - 1), "in 3d"),
            (Some(now + 2 * 86400 + 1), "in 3d"),
        ] {
            info.closes_at = at;
            assert_eq!(closes(&info, now), shown, "{at:?}");
        }
        info.keep = true;
        assert_eq!(closes(&info, now), "kept");
    }

    #[test]
    fn a_row_per_chat_shown_by_its_label_named_by_its_id() {
        let agents = [agent("c-0000abcd", AgentState::Ready, Some(12))];
        let now = crate::task::now();
        let mut named = chat("c-0000abcd", Some("login-bug"), &agents);
        named.info.last_turn_at = Some(now - 300);
        named.info.run_count = 2;
        named.info.closes_at = Some(now + 2 * 86400 + 3600);
        let mut unnamed = chat("c-1111beef", None, &agents);
        unnamed.info.keep = true;
        let mut titled = chat("c-2222cafe", None, &agents);
        titled.info.title = Some("Fix the login".into());
        let lines = chat_lines(&[named, unnamed, titled]);
        let shown = lines.join("\n");
        let [header, named, unnamed, titled] = &lines[..] else {
            panic!("{shown}")
        };
        // Unnamed, Claude's title shows; its id still names it.
        assert!(titled.contains("Fix the login  c-2222cafe"), "{shown}");
        assert!(header.starts_with("STATUS   CHAT"), "{shown}");
        assert!(named.starts_with("◆ ready  login-bug"), "{shown}");
        assert!(
            named.ends_with("c-0000abcd     2    5m   in 3d  /src/api  12%"),
            "{shown}"
        );
        // No agent: idle. Never prompted: its age. Kept: never closed.
        assert!(unnamed.starts_with("○ idle   c-1111beef "), "{shown}");
        assert_eq!(column(header, "ID"), column(titled, "c-2222cafe"));
        assert!(
            unnamed.ends_with("1    2d    kept  /src/api    -"),
            "{shown}"
        );
        // Autoclean off.
        assert!(
            titled.ends_with("1    2d       -  /src/api    -"),
            "{shown}"
        );
        assert_eq!(column(header, "ID"), column(named, "c-0000abcd"));
        assert_eq!(column(header, "DIR"), column(unnamed, "/src/api"));
    }
}
