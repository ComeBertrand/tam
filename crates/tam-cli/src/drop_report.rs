//! Text for a refused drop: per worktree, what it would lose or why it cannot
//! be checked.

use tam_proto::WorktreeReport;

/// Items shown per list before eliding the rest.
const MAX_ITEMS: usize = 8;

/// The report as lines of text (indented, without trailing newlines).
pub fn lines(worktrees: &[WorktreeReport]) -> Vec<String> {
    let mut lines = Vec::new();
    for wt in worktrees {
        let path = crate::shorten_home(&wt.path.display().to_string());
        if let Some(problem) = &wt.problem {
            lines.push(path);
            lines.push(format!("  cannot be checked: {problem}"));
            lines.push("  dropping anyway leaves the directory in place".into());
            continue;
        }
        let branch = match &wt.branch {
            Some(branch) => format!("branch {branch}"),
            None => "detached HEAD".into(),
        };
        lines.push(format!("{path} ({branch})"));
        list(
            &mut lines,
            plural(wt.uncommitted.len(), "uncommitted change"),
            &wt.uncommitted,
        );
        let commits = plural(wt.unmerged.len(), "commit");
        let header = if wt.branch.is_some() {
            format!("{commits} on no other branch")
        } else {
            format!("{commits} on no branch")
        };
        list(&mut lines, header, &wt.unmerged);
    }
    lines
}

fn list(lines: &mut Vec<String>, header: String, items: &[String]) {
    if items.is_empty() {
        return;
    }
    lines.push(format!("  {header}:"));
    lines.extend(items.iter().take(MAX_ITEMS).map(|i| format!("    {i}")));
    if items.len() > MAX_ITEMS {
        lines.push(format!("    … and {} more", items.len() - MAX_ITEMS));
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn report_lists_what_is_lost() {
        let report = WorktreeReport {
            path: PathBuf::from("/wt/app--feat"),
            branch: Some("feat".into()),
            uncommitted: vec![" M src/lib.rs".into()],
            unmerged: (1..=10).map(|i| format!("abc{i} commit {i}")).collect(),
            problem: None,
        };
        let lines = lines(&[report]);
        assert_eq!(lines[0], "/wt/app--feat (branch feat)");
        assert_eq!(lines[1], "  1 uncommitted change:");
        assert_eq!(lines[2], "     M src/lib.rs");
        assert_eq!(lines[3], "  10 commits on no other branch:");
        assert_eq!(lines[4], "    abc1 commit 1");
        assert_eq!(lines.last().unwrap(), "    … and 2 more");
        assert_eq!(lines.len(), 4 + MAX_ITEMS + 1);
    }

    #[test]
    fn report_detached_head_without_changes() {
        let report = WorktreeReport {
            path: PathBuf::from("/wt/app--feat"),
            branch: None,
            uncommitted: vec![],
            unmerged: vec!["abc1 wip".into()],
            problem: None,
        };
        assert_eq!(
            lines(&[report]),
            vec![
                "/wt/app--feat (detached HEAD)",
                "  1 commit on no branch:",
                "    abc1 wip",
            ]
        );
    }

    #[test]
    fn report_directory_that_cannot_be_checked() {
        let report = WorktreeReport {
            path: PathBuf::from("/wt/plain"),
            branch: None,
            uncommitted: vec![],
            unmerged: vec![],
            problem: Some("not a git repository".into()),
        };
        assert_eq!(
            lines(&[report]),
            vec![
                "/wt/plain",
                "  cannot be checked: not a git repository",
                "  dropping anyway leaves the directory in place",
            ]
        );
    }
}
