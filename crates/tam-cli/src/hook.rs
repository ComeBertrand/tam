//! `tam hook-notify`: forward an agent hook's JSON input to the daemon.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use serde_json::Value;

use crate::client::Client;

/// Payload fields not forwarded: bulky and unused by the daemon (a tool's
/// full output can be megabytes).
const DROPPED_FIELDS: &[&str] = &["tool_response"];

/// Turn a hook's raw stdin into `(event name, payload)`. `None` when it is
/// not a JSON object naming its event.
pub fn parse_payload(raw: &str) -> Option<(String, Value)> {
    let mut payload: Value = serde_json::from_str(raw).ok()?;
    let obj = payload.as_object_mut()?;
    let event = obj.get("hook_event_name")?.as_str()?.to_string();
    for field in DROPPED_FIELDS {
        obj.remove(*field);
    }
    Some((event, payload))
}

/// Run `tam hook-notify`. Always succeeds: a hook must never get in the
/// agent's way, and an agent outside tam (no agent id) has nothing to report.
pub async fn notify(agent: Option<String>, socket: Option<PathBuf>, legacy_event: Option<String>) {
    // Hooks from an older `tam init` (global Claude settings) still pass
    // --event; the hooks injected at spawn report the same events.
    if legacy_event.is_some() {
        return;
    }
    let Some(agent_id) = agent else {
        return;
    };
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return;
    }
    let mut raw = String::new();
    if stdin.lock().read_to_string(&mut raw).is_err() {
        return;
    }
    let Some((event, payload)) = parse_payload(&raw) else {
        return;
    };

    // Only the daemon that spawned the agent: never start one.
    let socket = socket.unwrap_or_else(tam_proto::default_socket_path);
    if let Ok(Some(mut client)) = Client::try_connect_at(&socket).await {
        let reply = client
            .send(tam_proto::Request::HookEvent {
                agent_id,
                event: event.clone(),
                payload: Some(payload),
            })
            .await;
        // Nothing else goes to stdout: Claude reads it (as context for some
        // events, as a decision for others). Not println!: it panics when
        // stdout is closed.
        if let Ok(tam_proto::Response::HookContext { context }) = reply {
            let _ = writeln!(
                std::io::stdout().lock(),
                "{}",
                hook_output(&event, &context)
            );
        }
    }
}

/// What a hook prints for Claude to add `context` to the session's
/// context.
pub fn hook_output(event: &str, context: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context,
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_and_drops_tool_response() {
        let raw = r#"{"session_id":"s","hook_event_name":"PostToolUse","tool_name":"Read","tool_response":{"content":"huge"}}"#;
        let (event, payload) = parse_payload(raw).unwrap();
        assert_eq!(event, "PostToolUse");
        assert_eq!(payload["session_id"], "s");
        assert_eq!(payload["tool_name"], "Read");
        assert!(payload.get("tool_response").is_none());
    }

    #[test]
    fn context_is_hook_specific_output() {
        let out: Value = serde_json::from_str(&hook_output("SessionStart", "a \"b\"\nc")).unwrap();
        assert_eq!(
            out,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": "a \"b\"\nc",
            }})
        );
    }

    #[test]
    fn rejects_payload_without_event() {
        assert!(parse_payload(r#"{"session_id":"s"}"#).is_none());
        assert!(parse_payload("[]").is_none());
        assert!(parse_payload("not json").is_none());
        assert!(parse_payload("").is_none());
    }
}
