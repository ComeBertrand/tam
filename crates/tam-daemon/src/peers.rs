//! What the agent of a task member is told about its task and the other
//! members (its peers): the note the daemon replies to the agent's
//! `SessionStart` hook, which Claude adds to the session's context.
//!
//! The note is written when it is asked for, from the ledger as it is then:
//! at every session start, resume, `/clear` and compaction. It holds what
//! an agent needs before its first step and what rarely changes: its task,
//! its name, its peers' names, ids, repositories and worktrees, and how to
//! reach them. What changes by the second (what the peers' agents are
//! doing) and membership changes in between are for `tam peers`.

use tam_proto::{MemberInfo, TaskInfo};

use crate::ledger::Ledger;

/// Most peers the note lists; `tam peers` lists them all.
const MAX_LISTED: usize = 8;

/// The note for the agent of member `id`, as the ledger is now. `None` for
/// an unknown member.
pub fn session_note(ledger: &Ledger, id: &str) -> Option<String> {
    let (task, member) = ledger.member(id)?;
    let peers: Vec<(String, &MemberInfo)> = task
        .members
        .iter()
        .filter(|m| m.id != id)
        .map(|m| (ledger.address(id, &task, m), m))
        .collect();
    Some(note(&task, &member, &peers))
}

/// The note for `member` of `task`; `peers` are the other members, each
/// with the name the member's agent reaches it by.
fn note(task: &TaskInfo, member: &MemberInfo, peers: &[(String, &MemberInfo)]) -> String {
    let Some((example, _)) = peers.first() else {
        return format!(
            "[tam] This session is the agent of task {} (member {}), its only member for now. \
             If other members join (each a worktree of one repository with its own agent), \
             tam peers lists them and tam msg reaches them.",
            task.name, member.id
        );
    };
    let mut out = format!(
        "[tam] This session is the agent of {} (member {}) in task {}. Each member of a task \
         is a worktree of one repository with its own agent. The other members now:",
        member.name, member.id, task.name
    );
    for (address, peer) in peers.iter().take(MAX_LISTED) {
        out.push_str(&format!(
            "\n- {} ({}): repo {}, branch {}, worktree {}",
            address,
            peer.id,
            peer.repo,
            peer.branch,
            peer.dir.display()
        ));
    }
    if peers.len() > MAX_LISTED {
        out.push_str(&format!(
            "\n- and {} more (tam peers lists them all)",
            peers.len() - MAX_LISTED
        ));
    }
    out.push_str(&format!(
        "\nTo message one: tam msg {example} \"<text>\" (add --wait to wait for its reply). \
         Members can join or leave later: tam peers lists the current ones and what their \
         agents are doing."
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn member(id: &str, name: &str, repo: &str) -> MemberInfo {
        MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: repo.into(),
            repo_root: None,
            branch: "feat".into(),
            dir: PathBuf::from("/w").join(format!("{repo}--feat")),
            health: None,
        }
    }

    fn task(members: Vec<MemberInfo>) -> TaskInfo {
        TaskInfo {
            name: "feat".into(),
            members,
            created_at: 0,
            run_count: 0,
            last_activity: None,
        }
    }

    #[test]
    fn a_member_learns_its_task_and_peers() {
        let api = member("m-1", "api", "backend");
        let web = member("m-2", "web", "frontend");
        let feat = task(vec![api.clone(), web.clone()]);
        assert_eq!(
            note(&feat, &api, &[("web".into(), &web)]),
            "[tam] This session is the agent of api (member m-1) in task feat. Each member of \
             a task is a worktree of one repository with its own agent. The other members now:\n\
             - web (m-2): repo frontend, branch feat, worktree /w/frontend--feat\n\
             To message one: tam msg web \"<text>\" (add --wait to wait for its reply). Members \
             can join or leave later: tam peers lists the current ones and what their agents \
             are doing."
        );
    }

    #[test]
    fn an_only_member_learns_how_to_find_later_ones() {
        let api = member("m-1", "api", "backend");
        let note = note(&task(vec![api.clone()]), &api, &[]);
        assert!(note.starts_with("[tam] This session is the agent of task feat (member m-1), its only member for now."), "{note}");
        assert!(note.contains("tam peers"), "{note}");
    }

    #[test]
    fn long_peer_lists_are_cut() {
        let members: Vec<MemberInfo> = (0..12)
            .map(|i| member(&format!("m-{i}"), &format!("r{i}"), &format!("r{i}")))
            .collect();
        let peers: Vec<(String, &MemberInfo)> =
            members[1..].iter().map(|m| (m.name.clone(), m)).collect();
        let note = note(&task(members.clone()), &members[0], &peers);
        assert_eq!(
            note.lines().filter(|l| l.starts_with("- r")).count(),
            MAX_LISTED
        );
        assert!(
            note.contains("- and 3 more (tam peers lists them all)"),
            "{note}"
        );
        assert!(note.contains("tam msg r1 "), "{note}");
    }
}
