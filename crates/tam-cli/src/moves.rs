//! `tam merge` and `tam split`: members move between tasks, with their
//! worktrees, runs, sessions, messages and running agents. The daemon does
//! it (`Request::MergeTasks`, `Request::SplitMember`); this prints what moved.

use anyhow::Result;
use tam_proto::AgentInfo;

use crate::client::{Client, Moved};
use crate::schema::{self, MovedMember, SCHEMA_VERSION};

/// `tam merge INTO FROM...`
pub async fn merge(into: &str, from: &[String], json: bool) -> Result<()> {
    let mut client = Client::connect().await?;
    // An unknown task exits 3, as elsewhere; the daemon checks again.
    for name in std::iter::once(into).chain(from.iter().map(String::as_str)) {
        client.get_task(name).await?;
    }
    let moved = client.merge_tasks(into, from).await?;
    let agents = client.list_agents().await.unwrap_or_default();
    let doc = document(&moved, from.to_vec(), &agents);
    if json {
        return print_json(&doc);
    }
    println!("Merged {} into {}:", from.join(", "), into);
    print_members(&doc);
    Ok(())
}

/// `tam split TASK/MEMBER [NAME]`
pub async fn split(target: &str, name: Option<&str>, json: bool) -> Result<()> {
    let mut client = Client::connect().await?;
    // An unknown member exits 3, as elsewhere.
    let (task, member, _) = client.resolve_member(Some(target), None).await?;
    let moved = client.split_member(&member.id, name).await?;
    let agents = client.list_agents().await.unwrap_or_default();
    let doc = document(&moved, vec![task.name.clone()], &agents);
    if json {
        return print_json(&doc);
    }
    println!(
        "Split {} out of {} into task {}:",
        task.member_label(&member),
        task.name,
        doc.task
    );
    print_members(&doc);
    Ok(())
}

/// The JSON document for a merge or split (`from`: the tasks the members
/// came from).
fn document(moved: &Moved, from: Vec<String>, agents: &[AgentInfo]) -> schema::Moved {
    let members = moved
        .tasks
        .iter()
        .flat_map(|task| {
            task.members.iter().map(move |m| MovedMember {
                id: m.id.clone(),
                task: task.name.clone(),
                name: m.name.clone(),
                label: task.member_label(m),
                previous_label: moved.previous.get(&m.id).cloned(),
                moved: moved.moved.contains(&m.id),
                repo: m.repo.clone(),
                branch: m.branch.clone(),
                dir: m.dir.clone(),
                // A member's agent runs under the member id.
                agent: agents
                    .iter()
                    .find(|a| a.id == m.id)
                    .map(|a| a.provider.clone()),
                notified: moved.notified.contains(&m.id),
            })
        })
        .collect();
    schema::Moved {
        schema_version: SCHEMA_VERSION,
        task: moved
            .tasks
            .first()
            .map(|t| t.name.clone())
            .unwrap_or_default(),
        from,
        members,
        error: None,
    }
}

/// Each member with the label it had before, and who its agent is told.
fn print_members(doc: &schema::Moved) {
    let width = doc.members.iter().map(|m| m.label.len()).max().unwrap_or(0);
    for m in &doc.members {
        match m.previous_label.as_deref().filter(|p| *p != m.label) {
            Some(previous) => println!("  {:<width$}  was {}", m.label, previous),
            None => println!("  {}", m.label),
        }
    }
    let labels = |pick: fn(&MovedMember) -> bool| -> Vec<&str> {
        doc.members
            .iter()
            .filter(|m| pick(m))
            .map(|m| m.label.as_str())
            .collect()
    };
    let told = labels(|m| m.notified);
    if !told.is_empty() {
        println!(
            "Running agents keep running; told at their next prompt or tool call: {}",
            told.join(", ")
        );
    }
    let untold = labels(|m| m.agent.is_some() && !m.notified);
    if !untold.is_empty() {
        println!(
            "Not told (agents without hooks; tam peers shows their peers): {}",
            untold.join(", ")
        );
    }
}

fn print_json(doc: &schema::Moved) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(doc)?);
    Ok(())
}
