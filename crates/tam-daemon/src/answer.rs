//! Answering an agent's permission requests from a client (the dashboard)
//! instead of in the agent's terminal.
//!
//! A Claude agent's `PermissionRequest` hook (`tam hook-notify`) reports the
//! request (`HookEvent`), then waits for its answer (`WaitAnswer`). The
//! daemon keeps the hook's connection with the request ([`Asks`]) and shows
//! the request to clients as the agent's `AgentDetail::ask`. Claude shows
//! its own dialog meanwhile, and takes whichever answer comes first.
//!
//! A client's `Answer` names the request by its id: the daemon turns it
//! into the hook's output ([`decision`], built from the request as the hook
//! sent it, never from the client's copy) and writes it to that request's
//! connection only, so an answer never reaches another request.
//!
//! A request is over without an answer from tam, and its hook released
//! (it prints nothing: Claude keeps to its own dialog), when:
//! - the hook goes away: the user said no in the terminal (Claude kills the
//!   hook), or it timed out;
//! - its tool ran (`PostToolUse`): the user said yes in the terminal, which
//!   leaves the hook running;
//! - a newer request comes, or a turn ends or starts;
//! - the agent exits (its hooks outlive it).

use std::collections::HashMap;

use serde_json::{json, Value};
use tam_proto::{Ask, AskAnswer, AskKind, AskOption, AskQuestion, QuestionAnswer};
use tokio::sync::oneshot;

/// Longest input or plan an `Ask` carries, in characters (every
/// `AgentInfo` of the agent carries it).
pub const MAX_ASK_TEXT: usize = 16_000;

const ASK_USER_QUESTION: &str = "AskUserQuestion";
const EXIT_PLAN_MODE: &str = "ExitPlanMode";

/// Input fields shown first, in this order (the others follow, sorted).
const FIRST_FIELDS: &[&str] = &[
    "command",
    "file_path",
    "notebook_path",
    "url",
    "pattern",
    "query",
    "old_string",
    "new_string",
    "content",
];

/// An answer handed to the hook waiting for it: what it prints, and where
/// to say whether it got it.
pub struct Delivery {
    pub output: String,
    pub written: oneshot::Sender<bool>,
}

/// A request a hook waits on.
struct Waiting {
    ask: Ask,
    /// Tells this registration from a later one of the same agent.
    token: u64,
    /// The tool's input, as the hook sent it.
    input: Value,
    tx: oneshot::Sender<Delivery>,
}

/// The requests agents' hooks wait on, by agent id: one per agent (Claude
/// asks one at a time).
#[derive(Default)]
pub struct Asks {
    waiting: HashMap<String, Waiting>,
    next_token: u64,
}

impl Asks {
    /// Register the request of a `PermissionRequest` hook's `payload` for
    /// `agent`, releasing an older one. Returns the request as clients see
    /// it, the registration's token, and where its answer comes; `None` when
    /// it is not one a client can answer (no tool, unreadable questions).
    pub fn register(
        &mut self,
        agent: &str,
        payload: &Value,
    ) -> Option<(Ask, u64, oneshot::Receiver<Delivery>)> {
        let tool = payload.get("tool_name")?.as_str()?.to_string();
        let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
        let kind = ask_kind(&tool, &input)?;
        let id = loop {
            let id = format!("ask-{:08x}", crate::ledger::random_u32());
            if !self.waiting.values().any(|w| w.ask.id == id) {
                break id;
            }
        };
        let ask = Ask { id, tool, kind };
        self.next_token += 1;
        let token = self.next_token;
        let (tx, rx) = oneshot::channel();
        // Dropping the older one's sender releases its hook.
        self.waiting.insert(
            agent.to_string(),
            Waiting {
                ask: ask.clone(),
                token,
                input,
                tx,
            },
        );
        Some((ask, token, rx))
    }

    /// The hook of `agent`'s request registered as `token` went away:
    /// forget the request. Returns `false` when a newer one replaced it.
    pub fn unregister(&mut self, agent: &str, token: u64) -> bool {
        if self.waiting.get(agent).is_some_and(|w| w.token == token) {
            self.waiting.remove(agent);
            return true;
        }
        false
    }

    /// Release the hook of `agent`'s request, if any (it prints nothing).
    /// Returns whether there was one.
    pub fn release(&mut self, agent: &str) -> bool {
        self.waiting.remove(agent).is_some()
    }

    /// Whether hook `event` (with its `payload`) of `agent` tells that the
    /// request it waits on is over: its tool ran, another request came, a
    /// turn ended or started.
    pub fn ended_by(&self, agent: &str, event: &str, payload: Option<&Value>) -> bool {
        let Some(waiting) = self.waiting.get(agent) else {
            return false;
        };
        match event {
            "PostToolUse" | "PostToolUseFailure" => payload.is_some_and(|p| {
                p.get("tool_name").and_then(Value::as_str) == Some(waiting.ask.tool.as_str())
                    && p.get("tool_input")
                        .is_some_and(|input| same_input(input, &waiting.input))
            }),
            "PermissionRequest" | "UserPromptSubmit" | "Stop" | "StopFailure" | "SessionStart"
            | "SessionEnd" => true,
            _ => false,
        }
    }

    /// Answer `agent`'s request `id`: hand its hook the decision. Returns
    /// where the hook's connection tells whether it got it; an error when
    /// `id` is not the request the agent waits on (answered or changed
    /// meanwhile: nothing is sent) or the answer does not fit it.
    pub fn answer(
        &mut self,
        agent: &str,
        id: &str,
        answer: &AskAnswer,
    ) -> Result<oneshot::Receiver<bool>, String> {
        let gone = || "the request was answered or changed meanwhile: nothing was sent".to_string();
        let waiting = self
            .waiting
            .get(agent)
            .filter(|w| w.ask.id == id && !w.tx.is_closed())
            .ok_or_else(gone)?;
        let output = decision(&waiting.ask, &waiting.input, answer)?;
        let waiting = self.waiting.remove(agent).expect("just found");
        let (written, rx) = oneshot::channel();
        waiting
            .tx
            .send(Delivery { output, written })
            .map_err(|_| gone())?;
        Ok(rx)
    }
}

/// What a request asks for, from its tool and input: `None` for questions
/// the dashboard could not show.
fn ask_kind(tool: &str, input: &Value) -> Option<AskKind> {
    Some(match tool {
        ASK_USER_QUESTION => AskKind::Questions {
            questions: questions(input)?,
        },
        EXIT_PLAN_MODE => {
            let plan = match input.get("plan").and_then(Value::as_str) {
                Some(plan) => plan.to_string(),
                None => input_text(input),
            };
            let (plan, cut) = cap(plan);
            AskKind::Plan { plan, cut }
        }
        _ => {
            let (input, cut) = cap(input_text(input));
            AskKind::Permission { input, cut }
        }
    })
}

/// The questions of an `AskUserQuestion` input, each with its options.
fn questions(input: &Value) -> Option<Vec<AskQuestion>> {
    let str_of = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let questions = input
        .get("questions")?
        .as_array()?
        .iter()
        .map(|q| {
            let options = q
                .get("options")?
                .as_array()?
                .iter()
                .map(|o| {
                    Some(AskOption {
                        label: str_of(o, "label")?,
                        description: str_of(o, "description").filter(|d| !d.is_empty()),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(AskQuestion {
                question: str_of(q, "question")?,
                header: str_of(q, "header").filter(|h| !h.is_empty()),
                options,
                multi_select: q.get("multiSelect").and_then(Value::as_bool) == Some(true),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!questions.is_empty()).then_some(questions)
}

/// A tool's input as text: a line per field (`command: ls`), the most
/// telling first; a text of several lines below its name, indented.
fn input_text(input: &Value) -> String {
    let Some(fields) = input.as_object() else {
        return serde_json::to_string_pretty(input).unwrap_or_default();
    };
    let mut keys: Vec<&String> = fields.keys().collect();
    let rank = |k: &str| {
        FIRST_FIELDS
            .iter()
            .position(|f| *f == k)
            .unwrap_or(usize::MAX)
    };
    keys.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.cmp(b)));
    let mut lines = Vec::new();
    for key in keys {
        match &fields[key] {
            Value::String(s) if s.contains('\n') => {
                lines.push(format!("{key}:"));
                lines.extend(s.lines().map(|l| format!("  {l}")));
            }
            Value::String(s) => lines.push(format!("{key}: {s}")),
            other => lines.push(format!("{key}: {other}")),
        }
    }
    lines.join("\n")
}

/// `text` cut to [`MAX_ASK_TEXT`] characters, and whether it was.
fn cap(text: String) -> (String, bool) {
    match text.char_indices().nth(MAX_ASK_TEXT) {
        Some((at, _)) => (text[..at].to_string(), true),
        None => (text, false),
    }
}

/// Whether a tool's input at `PostToolUse` (`ran`) is the request's
/// (`asked`): the fields both have are the same. Claude adds some after
/// the request (a question's answers and annotations) and leaves some out
/// (a plan's text, which only the request carries). Two calls of a tool
/// at once differ in a field both have (a command, a path).
fn same_input(ran: &Value, asked: &Value) -> bool {
    match (ran.as_object(), asked.as_object()) {
        (Some(ran), Some(asked)) => asked.iter().all(|(k, v)| ran.get(k).is_none_or(|r| r == v)),
        _ => ran == asked,
    }
}

/// What the `PermissionRequest` hook prints for `answer` to `ask`, whose
/// tool's input is `input`: Claude's decision. Plan approval leaves plan
/// mode for the default mode, where edits ask again (Claude would restore
/// the mode from before plan mode, maybe accepting edits). No permission
/// rule is ever added.
pub fn decision(ask: &Ask, input: &Value, answer: &AskAnswer) -> Result<String, String> {
    let decision = match (&ask.kind, answer) {
        (AskKind::Questions { questions }, AskAnswer::Answers { answers }) => {
            let answers = answer_map(questions, answers)?;
            let mut updated = input.clone();
            let Some(fields) = updated.as_object_mut() else {
                return Err("the question's input is not an object".into());
            };
            fields.insert("answers".into(), Value::Object(answers));
            json!({ "behavior": "allow", "updatedInput": updated })
        }
        (AskKind::Questions { .. }, AskAnswer::Allow) => {
            return Err("these are questions: answer them".into());
        }
        (_, AskAnswer::Answers { .. }) => {
            return Err(format!("{} asks no questions", ask.tool));
        }
        // What was not shown is not allowed.
        (
            AskKind::Permission { cut: true, .. } | AskKind::Plan { cut: true, .. },
            AskAnswer::Allow,
        ) => {
            return Err(
                "the request is too long to show in full: allow it in the agent's terminal".into(),
            );
        }
        (AskKind::Plan { .. }, AskAnswer::Allow) => json!({
            "behavior": "allow",
            "updatedInput": input,
            "updatedPermissions": [
                { "type": "setMode", "mode": "default", "destination": "session" }
            ],
        }),
        (AskKind::Permission { .. }, AskAnswer::Allow) => json!({ "behavior": "allow" }),
        (kind, AskAnswer::Deny { reason }) => {
            let reason = reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
            let what = match kind {
                AskKind::Plan { .. } => "rejected this plan",
                AskKind::Questions { .. } => "declined to answer",
                AskKind::Permission { .. } => "denied this",
            };
            // Never `interrupt`: an answer that comes after the user said
            // yes in the terminal (the hook still runs until the tool ends)
            // would stop the turn they let go on. Claude ignores a late
            // deny without it.
            let message = match reason {
                Some(reason) => {
                    format!("The user {what} from the tam dashboard, with this reason: {reason}")
                }
                None => format!(
                    "The user {what} from the tam dashboard, without a reason. \
                     Stop here and wait for their instructions."
                ),
            };
            json!({ "behavior": "deny", "message": message })
        }
    };
    Ok(json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": decision,
        }
    })
    .to_string())
}

/// Claude's `answers`: each question's text to its answer, as Claude's own
/// dialog gives it (the options chosen, then the typed answer, joined by
/// `, `).
fn answer_map(
    questions: &[AskQuestion],
    answers: &[QuestionAnswer],
) -> Result<serde_json::Map<String, Value>, String> {
    if answers.len() != questions.len() {
        return Err(format!(
            "{} answers to {} questions",
            answers.len(),
            questions.len()
        ));
    }
    let mut map = serde_json::Map::new();
    for (q, a) in questions.iter().zip(answers) {
        if let Some(label) = a
            .chosen
            .iter()
            .find(|l| !q.options.iter().any(|o| &o.label == *l))
        {
            return Err(format!("'{label}' is not an option of '{}'", q.question));
        }
        let other = a.other.as_deref().map(str::trim).filter(|o| !o.is_empty());
        let parts: Vec<&str> = a.chosen.iter().map(String::as_str).chain(other).collect();
        match parts.len() {
            0 => return Err(format!("'{}' has no answer", q.question)),
            n if n > 1 && !q.multi_select => {
                return Err(format!("'{}' takes one answer", q.question));
            }
            _ => {}
        }
        map.insert(q.question.clone(), parts.join(", ").into());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(tool: &str, input: Value) -> Value {
        json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "s",
            "tool_name": tool,
            "tool_input": input,
            "permission_suggestions": [{ "type": "addRules", "rules": [] }],
        })
    }

    fn questions_input() -> Value {
        json!({ "questions": [
            {
                "question": "Which colors?",
                "header": "Colors",
                "multiSelect": true,
                "options": [
                    { "label": "Red", "description": "Red" },
                    { "label": "Blue", "description": "" },
                ],
            },
            {
                "question": "Which size?",
                "header": "Size",
                "multiSelect": false,
                "options": [{ "label": "Small" }, { "label": "Large" }],
            },
        ]})
    }

    fn decision_of(output: &str) -> Value {
        let out: Value = serde_json::from_str(output).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        out["hookSpecificOutput"]["decision"].clone()
    }

    fn answered(chosen: &[&str], other: Option<&str>) -> QuestionAnswer {
        QuestionAnswer {
            chosen: chosen.iter().map(|c| c.to_string()).collect(),
            other: other.map(str::to_string),
        }
    }

    #[test]
    fn a_permission_shows_the_whole_input_most_telling_field_first() {
        let input = json!({
            "replace_all": false,
            "old_string": "a\nb",
            "new_string": "c",
            "file_path": "/w/src/main.rs",
        });
        let kind = ask_kind("Edit", &input).unwrap();
        assert_eq!(
            kind,
            AskKind::Permission {
                input: "file_path: /w/src/main.rs\nold_string:\n  a\n  b\nnew_string: c\nreplace_all: false"
                    .into(),
                cut: false,
            }
        );
        let AskKind::Permission { input, .. } =
            ask_kind("Bash", &json!({"description": "List", "command": "ls -la"})).unwrap()
        else {
            panic!()
        };
        assert_eq!(input, "command: ls -la\ndescription: List");
    }

    #[test]
    fn long_inputs_and_plans_are_cut() {
        let big = "x".repeat(MAX_ASK_TEXT + 10);
        let write = json!({ "content": big });
        let Some(AskKind::Permission { input, cut }) = ask_kind("Write", &write) else {
            panic!()
        };
        assert!(cut);
        assert_eq!(input.chars().count(), MAX_ASK_TEXT);
        // What was not shown is not allowed; a no is fine.
        let ask = Ask {
            id: "ask-1".into(),
            tool: "Write".into(),
            kind: ask_kind("Write", &write).unwrap(),
        };
        let refused = decision(&ask, &write, &AskAnswer::Allow).unwrap_err();
        assert!(refused.contains("too long"), "{refused}");
        assert!(decision(&ask, &write, &AskAnswer::Deny { reason: None }).is_ok());
        let Some(AskKind::Plan { plan, cut }) =
            ask_kind("ExitPlanMode", &json!({ "plan": "# Plan\n- do it" }))
        else {
            panic!()
        };
        assert_eq!((plan.as_str(), cut), ("# Plan\n- do it", false));
    }

    #[test]
    fn questions_keep_their_options() {
        let Some(AskKind::Questions { questions }) =
            ask_kind(ASK_USER_QUESTION, &questions_input())
        else {
            panic!()
        };
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].question, "Which colors?");
        assert_eq!(questions[0].header.as_deref(), Some("Colors"));
        assert!(questions[0].multi_select && !questions[1].multi_select);
        assert_eq!(questions[0].options[0].description.as_deref(), Some("Red"));
        assert_eq!(questions[0].options[1].description, None);
        assert_eq!(questions[1].options[1].label, "Large");
        // Questions the dashboard cannot show are left to the terminal.
        assert!(ask_kind(ASK_USER_QUESTION, &json!({})).is_none());
        assert!(ask_kind(
            ASK_USER_QUESTION,
            &json!({"questions": [{"question": "?"}]})
        )
        .is_none());
    }

    #[test]
    fn allow_once_adds_no_rule() {
        let input = json!({"command": "ls"});
        let ask = Ask {
            id: "ask-1".into(),
            tool: "Bash".into(),
            kind: ask_kind("Bash", &input).unwrap(),
        };
        let d = decision_of(&decision(&ask, &input, &AskAnswer::Allow).unwrap());
        assert_eq!(d, json!({ "behavior": "allow" }));
    }

    #[test]
    fn plan_approval_passes_the_plan_back_and_keeps_edits_asking() {
        let input = json!({"plan": "# P", "planFilePath": "/p.md"});
        let ask = Ask {
            id: "ask-1".into(),
            tool: EXIT_PLAN_MODE.into(),
            kind: ask_kind(EXIT_PLAN_MODE, &input).unwrap(),
        };
        let d = decision_of(&decision(&ask, &input, &AskAnswer::Allow).unwrap());
        assert_eq!(d["behavior"], "allow");
        assert_eq!(d["updatedInput"], input);
        assert_eq!(
            d["updatedPermissions"],
            json!([{ "type": "setMode", "mode": "default", "destination": "session" }])
        );
    }

    #[test]
    fn deny_tells_the_agent_who_and_why() {
        let input = json!({"command": "rm -rf build"});
        let ask = Ask {
            id: "ask-1".into(),
            tool: "Bash".into(),
            kind: ask_kind("Bash", &input).unwrap(),
        };
        let deny = |reason: Option<&str>| {
            let answer = AskAnswer::Deny {
                reason: reason.map(str::to_string),
            };
            decision_of(&decision(&ask, &input, &answer).unwrap())
        };
        assert_eq!(
            deny(Some(" use make clean ")),
            json!({
                "behavior": "deny",
                "message": "The user denied this from the tam dashboard, with this reason: use make clean",
            })
        );
        // No reason: the agent is asked to stop (never `interrupt`, which
        // Claude applies even to a request already answered).
        for reason in [None, Some("  ")] {
            assert_eq!(
                deny(reason),
                json!({
                    "behavior": "deny",
                    "message": "The user denied this from the tam dashboard, without a reason. \
                                Stop here and wait for their instructions.",
                })
            );
        }
    }

    #[test]
    fn answers_are_given_as_claude_gives_them() {
        let input = questions_input();
        let ask = Ask {
            id: "ask-1".into(),
            tool: ASK_USER_QUESTION.into(),
            kind: ask_kind(ASK_USER_QUESTION, &input).unwrap(),
        };
        let answer = AskAnswer::Answers {
            answers: vec![
                answered(&["Red", "Blue"], Some("Teal")),
                answered(&[], Some(" Medium ")),
            ],
        };
        let d = decision_of(&decision(&ask, &input, &answer).unwrap());
        assert_eq!(d["behavior"], "allow");
        assert_eq!(d["updatedInput"]["questions"], input["questions"]);
        assert_eq!(
            d["updatedInput"]["answers"],
            json!({ "Which colors?": "Red, Blue, Teal", "Which size?": "Medium" })
        );

        let refused = |answers: Vec<QuestionAnswer>| {
            decision(&ask, &input, &AskAnswer::Answers { answers }).unwrap_err()
        };
        assert_eq!(
            refused(vec![answered(&["Red"], None)]),
            "1 answers to 2 questions"
        );
        assert_eq!(
            refused(vec![
                answered(&["Red"], None),
                answered(&["Small", "Large"], None)
            ]),
            "'Which size?' takes one answer"
        );
        assert_eq!(
            refused(vec![answered(&["Red"], None), answered(&["Huge"], None)]),
            "'Huge' is not an option of 'Which size?'"
        );
        assert_eq!(
            refused(vec![answered(&[], Some(" ")), answered(&["Small"], None)]),
            "'Which colors?' has no answer"
        );
        assert!(decision(&ask, &input, &AskAnswer::Allow).is_err());
        let bash = Ask {
            id: "ask-2".into(),
            tool: "Bash".into(),
            kind: ask_kind("Bash", &json!({})).unwrap(),
        };
        assert!(decision(&bash, &json!({}), &answer).is_err());
    }

    #[test]
    fn an_answer_reaches_the_request_it_names_only() {
        let mut asks = Asks::default();
        let (first, _, mut first_rx) = asks
            .register("m-1", &request("Bash", json!({"command": "ls"})))
            .unwrap();
        // A newer request releases the older one's hook.
        let (second, token, mut second_rx) = asks
            .register("m-1", &request("Bash", json!({"command": "rm x"})))
            .unwrap();
        assert_ne!(first.id, second.id);
        assert!(matches!(
            first_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        let stale = asks
            .answer("m-1", &first.id, &AskAnswer::Allow)
            .unwrap_err();
        assert!(stale.contains("answered or changed meanwhile"), "{stale}");
        assert!(asks.answer("m-2", &second.id, &AskAnswer::Allow).is_err());
        // An answer that does not fit leaves the request waiting.
        let wrong = AskAnswer::Answers { answers: vec![] };
        assert!(asks.answer("m-1", &second.id, &wrong).is_err());

        let _written = asks.answer("m-1", &second.id, &AskAnswer::Allow).unwrap();
        let delivery = second_rx.try_recv().unwrap();
        assert_eq!(
            decision_of(&delivery.output),
            json!({ "behavior": "allow" })
        );
        // Answered: there is nothing left to answer.
        assert!(asks.answer("m-1", &second.id, &AskAnswer::Allow).is_err());
        assert!(!asks.unregister("m-1", token));
    }

    #[test]
    fn a_hook_gone_forgets_its_request_not_a_newer_one() {
        let mut asks = Asks::default();
        let (_, old, _old_rx) = asks.register("m-1", &request("Bash", json!({}))).unwrap();
        let (ask, new, _rx) = asks.register("m-1", &request("Bash", json!({}))).unwrap();
        assert!(!asks.unregister("m-1", old));
        assert!(asks.ended_by("m-1", "Stop", None));
        assert!(asks.unregister("m-1", new));
        let gone = asks.answer("m-1", &ask.id, &AskAnswer::Allow).unwrap_err();
        assert!(gone.contains("nothing was sent"), "{gone}");
        assert!(!asks.release("m-1"));
    }

    #[test]
    fn the_request_ends_when_its_tool_runs_or_the_turn_moves_on() {
        let mut asks = Asks::default();
        let input = questions_input();
        asks.register("m-1", &request(ASK_USER_QUESTION, input.clone()))
            .unwrap();
        let post = |tool: &str, input: Value| json!({ "tool_name": tool, "tool_input": input });
        // Another tool, or the same tool with another input: still waiting.
        assert!(!asks.ended_by("m-1", "PostToolUse", Some(&post("Read", input.clone()))));
        assert!(!asks.ended_by(
            "m-1",
            "PostToolUse",
            Some(&post(ASK_USER_QUESTION, json!({"questions": []})))
        ));
        assert!(!asks.ended_by("m-1", "Notification", None));
        assert!(!asks.ended_by("m-2", "Stop", None));
        // Answered in the terminal: the tool ran, with the answers (and
        // whatever else) added.
        let mut ran = input.clone();
        ran["answers"] = json!({ "Which colors?": "Red" });
        ran["annotations"] = json!({});
        assert!(asks.ended_by("m-1", "PostToolUse", Some(&post(ASK_USER_QUESTION, ran))));
        assert!(asks.ended_by(
            "m-1",
            "PostToolUseFailure",
            Some(&post(ASK_USER_QUESTION, input))
        ));
        // A plan approved in the terminal: its PostToolUse input has no
        // plan.
        let plan = json!({"plan": "# P", "planFilePath": "/p.md"});
        asks.register("m-1", &request(EXIT_PLAN_MODE, plan))
            .unwrap();
        assert!(asks.ended_by("m-1", "PostToolUse", Some(&post(EXIT_PLAN_MODE, json!({})))));
        // Another command of the same tool (a parallel call) is not it.
        asks.register(
            "m-1",
            &request("Bash", json!({"command": "make", "description": "Build"})),
        )
        .unwrap();
        let other = post("Bash", json!({"command": "ls", "description": "Build"}));
        assert!(!asks.ended_by("m-1", "PostToolUse", Some(&other)));
        let ran = post(
            "Bash",
            json!({"command": "make", "description": "Build", "timeout": 1}),
        );
        assert!(asks.ended_by("m-1", "PostToolUse", Some(&ran)));
        for event in [
            "PermissionRequest",
            "UserPromptSubmit",
            "Stop",
            "SessionEnd",
        ] {
            assert!(asks.ended_by("m-1", event, None), "{event}");
        }
        assert!(asks.release("m-1"));
        assert!(!asks.ended_by("m-1", "Stop", None));
    }
}
