//! A task's brief from the command line: `tam brief`, `tam new --brief`,
//! and the brief `tam peers` shows.
//!
//! The daemon keeps the brief (see `tam-daemon/src/brief.rs`): a reference
//! the user gave, else the local file `brief.md` in the task's directory,
//! next to the members' scratch directory. Here a reference the user types
//! is expanded by the config's `[[brief.patterns]]` (a ticket id into its
//! URL, and the start prompt `tam new` gives the agents), or made absolute
//! when it is a path. tam never fetches or reads it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tam_proto::TaskBrief;

use crate::client::Client;
use crate::script::{self, ErrorKind};

/// Version of the `--json` output.
const SCHEMA_VERSION: u32 = 1;

/// One `[[brief.patterns]]` entry of `config.toml`, as written.
#[derive(Debug, Clone, Deserialize)]
struct PatternFile {
    /// Regular expression the brief is matched against.
    #[serde(rename = "match")]
    pattern: String,
    /// What the brief becomes.
    reference: Option<String>,
    /// The start prompt of the agents of `tam new`, unless given one.
    prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct BriefConfig {
    #[serde(default)]
    patterns: Vec<PatternFile>,
}

/// The part of `config.toml` read here.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    brief: Option<BriefConfig>,
}

/// A pattern that expands a brief: in `reference` and `prompt`, `{0}` is
/// the match, `{1}`… its groups, `{brief}` the brief (as given in
/// `reference`, as expanded in `prompt`).
#[derive(Debug)]
pub struct Pattern {
    regex: Regex,
    reference: Option<String>,
    prompt: Option<String>,
}

/// A brief as the user typed it, expanded.
#[derive(Debug, PartialEq, Eq)]
pub struct Expanded {
    /// What tam records and tells the members.
    pub reference: String,
    /// The start prompt a pattern gives.
    pub prompt: Option<String>,
}

/// The brief patterns of `config.toml` (`$XDG_CONFIG_HOME/tam`).
pub fn load_patterns() -> Result<Vec<Pattern>> {
    let path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join("tam")
        .join("config.toml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    parse_patterns(&content).with_context(|| format!("in {}", path.display()))
}

fn parse_patterns(toml_str: &str) -> Result<Vec<Pattern>> {
    let file: ConfigFile = toml::from_str(toml_str)?;
    let patterns = file.brief.unwrap_or_default().patterns;
    patterns
        .into_iter()
        .map(|p| {
            let regex =
                Regex::new(&p.pattern).with_context(|| format!("brief pattern '{}'", p.pattern))?;
            Ok(Pattern {
                regex,
                reference: p.reference,
                prompt: p.prompt,
            })
        })
        .collect()
}

/// Expand `value` with the first pattern it matches. Matching none, an
/// existing path is made absolute (members run elsewhere); anything else is
/// kept as given.
pub fn expand(patterns: &[Pattern], value: &str) -> Expanded {
    let value = value.trim();
    for pattern in patterns {
        let Some(captures) = pattern.regex.captures(value) else {
            continue;
        };
        let group = |key: &str| -> Option<String> {
            let n: usize = key.parse().ok()?;
            (n < 10).then(|| {
                captures
                    .get(n)
                    .map_or(String::new(), |m| m.as_str().to_string())
            })
        };
        let reference = match &pattern.reference {
            Some(template) => fill(template, |key| match key {
                "brief" => Some(value.to_string()),
                key => group(key),
            }),
            None => value.to_string(),
        };
        let prompt = pattern.prompt.as_ref().map(|template| {
            fill(template, |key| match key {
                "brief" => Some(reference.clone()),
                key => group(key),
            })
        });
        return Expanded { reference, prompt };
    }
    let reference = match std::fs::canonicalize(value) {
        Ok(path) if !value.is_empty() => path.display().to_string(),
        _ => value.to_string(),
    };
    Expanded {
        reference,
        prompt: None,
    }
}

/// `template` with each `{key}` that `value` knows replaced; other braces
/// are kept.
fn fill(template: &str, value: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after
            .find('}')
            .and_then(|close| Some((close, value(&after[..close])?)))
        {
            Some((close, text)) => {
                out.push_str(&text);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A brief in `--json` output (`tam brief`, `tam peers`).
#[derive(Debug, Serialize)]
pub struct BriefView {
    pub task: String,
    /// The reference given; `null` for the local file.
    pub reference: Option<String>,
    /// The local brief, while there is no reference.
    pub file: Option<PathBuf>,
    pub scratch: PathBuf,
}

impl BriefView {
    pub fn new(task: &str, brief: &TaskBrief) -> Self {
        Self {
            task: task.to_string(),
            reference: brief.reference.clone(),
            file: brief.file(),
            scratch: brief.scratch(),
        }
    }
}

#[derive(Serialize)]
struct BriefDoc {
    schema_version: u32,
    #[serde(flatten)]
    brief: BriefView,
}

/// The lines `tam brief` and `tam peers` print.
pub fn lines(task: &str, brief: &TaskBrief) -> [String; 2] {
    [
        format!("Brief of task {task}: {}", brief.location()),
        format!("Scratch: {}", brief.scratch().display()),
    ]
}

/// `tam brief`: show the brief of a task (`name`, else the caller's), or
/// change it (`set`, or back to the local file with `local`). `from` is the
/// caller's agent id.
pub async fn brief(
    name: Option<String>,
    set: Option<String>,
    local: bool,
    json: bool,
    from: Option<String>,
) -> Result<()> {
    let set = match set {
        Some(value) => Some(expand(&load_patterns()?, &value).reference),
        None => None,
    };
    let mut client = Client::connect().await?;
    let target = match name {
        Some(name) => name,
        None => caller_task(&mut client, from.as_deref()).await?,
    };
    // Found first: what fails then is the change, not the name.
    let (task, mut brief) = client.get_brief(&target).await?;
    if set.is_some() || local {
        (_, brief) = client.set_brief(&task, set).await?;
    }
    if json {
        let doc = BriefDoc {
            schema_version: SCHEMA_VERSION,
            brief: BriefView::new(&task, &brief),
        };
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        for line in lines(&task, &brief) {
            println!("{line}");
        }
    }
    Ok(())
}

/// The task of the caller: the member of its agent id, else the one whose
/// worktree holds the current directory.
async fn caller_task(client: &mut Client, from: Option<&str>) -> Result<String> {
    if let Some(id) = from.filter(|id| !id.is_empty()) {
        if let Ok((task, _, _)) = client.resolve_member(Some(id), None).await {
            return Ok(task.name);
        }
    }
    let cwd = std::fs::canonicalize(".")?;
    match client.resolve_member(None, Some(&cwd)).await {
        Ok((task, _, _)) => Ok(task.name),
        Err(e) if script::kind_of(&e) == ErrorKind::Daemon => Err(e),
        Err(_) => Err(script::error(
            ErrorKind::NotFound,
            "no task in the current directory: name one",
        )),
    }
}

/// Set the brief of a task `tam new` just created, before its agents start.
pub async fn set_new(client: &mut Client, task: &str, reference: &str) -> Result<()> {
    client
        .set_brief(task, Some(reference.to_string()))
        .await
        .map(|_| ())
        .with_context(|| {
            format!(
                "task '{task}' was created, but not its brief: set it with tam brief {} --set {}",
                crate::config::shell_quote(task),
                crate::config::shell_quote(reference)
            )
        })
}

impl Client {
    /// A task's brief: `task` is a task name, or a member as
    /// `ResolveMember` takes it. Returns the task's name too. An error is
    /// the name's (not found, or a task name where a member is needed).
    pub async fn get_brief(&mut self, task: &str) -> Result<(String, TaskBrief)> {
        let request = tam_proto::Request::GetBrief { task: task.into() };
        brief_response(self.send(request).await?, ErrorKind::NotFound)
    }

    /// Change a task's brief (`None`: back to the local file).
    pub async fn set_brief(
        &mut self,
        task: &str,
        brief: Option<String>,
    ) -> Result<(String, TaskBrief)> {
        let request = tam_proto::Request::SetBrief {
            task: task.into(),
            brief,
        };
        brief_response(self.send(request).await?, ErrorKind::Failed)
    }
}

/// The task and brief of a `Brief` reply; a daemon error as `kind`.
fn brief_response(response: tam_proto::Response, kind: ErrorKind) -> Result<(String, TaskBrief)> {
    match response {
        tam_proto::Response::Brief { task, brief } => Ok((task, brief)),
        tam_proto::Response::Error { message } => Err(script::error(kind, message)),
        other => Err(script::error(
            ErrorKind::Daemon,
            format!("unexpected response: {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns() -> Vec<Pattern> {
        parse_patterns(
            r#"
            [spawn]
            default_agent = "claude"

            [[brief.patterns]]
            match = '^CS-(\d+)$'
            reference = "https://tickets.example/CS-{1}"
            prompt = "/implement-task {brief}"

            [[brief.patterns]]
            match = '^PR-\d+$'
            prompt = "review {0} {unknown} {"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn patterns_expand_a_reference_and_a_prompt() {
        let patterns = patterns();
        assert_eq!(
            expand(&patterns, " CS-42 "),
            Expanded {
                reference: "https://tickets.example/CS-42".into(),
                prompt: Some("/implement-task https://tickets.example/CS-42".into()),
            }
        );
        // No reference: the brief stays as given; unknown braces stay too.
        assert_eq!(
            expand(&patterns, "PR-7"),
            Expanded {
                reference: "PR-7".into(),
                prompt: Some("review PR-7 {unknown} {".into()),
            }
        );
        // No match: as given.
        assert_eq!(
            expand(&patterns, "https://notion.so/x"),
            Expanded {
                reference: "https://notion.so/x".into(),
                prompt: None,
            }
        );
        assert!(parse_patterns("").unwrap().is_empty());
        let err = parse_patterns("[[brief.patterns]]\nmatch = '('\n").unwrap_err();
        assert!(format!("{err:#}").contains("brief pattern '('"), "{err:#}");
    }

    #[test]
    fn a_path_is_made_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("spec.md");
        std::fs::write(&file, "").unwrap();
        let expanded = expand(&[], file.to_str().unwrap());
        assert_eq!(
            expanded.reference,
            std::fs::canonicalize(&file).unwrap().display().to_string()
        );
        assert_eq!(expand(&[], "no/such/file").reference, "no/such/file");
    }

    #[test]
    fn fill_replaces_known_keys_only() {
        let value = |key: &str| (key == "a").then(|| "A".to_string());
        assert_eq!(fill("x{a}y{b}z{a", value), "xAy{b}z{a");
        assert_eq!(fill("", value), "");
    }

    #[test]
    fn views_say_where_the_brief_is() {
        let local = TaskBrief {
            reference: None,
            dir: PathBuf::from("/d/tasks/feat"),
        };
        assert_eq!(
            lines("feat", &local),
            [
                "Brief of task feat: /d/tasks/feat/brief.md",
                "Scratch: /d/tasks/feat/scratch",
            ]
        );
        let view = serde_json::to_value(BriefView::new("feat", &local)).unwrap();
        assert_eq!(
            view,
            serde_json::json!({
                "task": "feat",
                "reference": null,
                "file": "/d/tasks/feat/brief.md",
                "scratch": "/d/tasks/feat/scratch",
            })
        );
    }
}
