//! Messages between task members (and from the user): what the daemon keeps
//! in memory to deliver them. Messages themselves live in the ledger.
//!
//! Delivery:
//! - The agent's inbox hook runs `tam inbox-wait` in the
//!   background (an `asyncRewake` hook armed at session start and after
//!   each turn). It holds a `WaitInbox` connection; the daemon hands it the
//!   queued messages, it prints them and exits 2, and Claude shows them to
//!   the model: at once when idle, else at its next step (Claude queues it
//!   like text typed while it works, and holds it while a permission dialog
//!   is open). A message handed over counts as delivered when it shows up in
//!   the prompt of the agent's next `UserPromptSubmit` hook; until then it is
//!   in flight, and goes back to the queue if the agent exits.
//! - A member with no running agent keeps it queued for its next agent.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use tam_proto::MessageInfo;
use tokio::sync::oneshot;

/// Longest message text, in characters (Claude caps a hook's output at
/// 10,000 characters).
pub const MAX_TEXT: usize = 8000;
/// Most agent-to-agent hops a message may follow (loop guard).
pub const MAX_HOPS: u32 = 6;
/// Most messages an agent may send within [`RATE_WINDOW`].
pub const RATE_LIMIT: usize = 20;
pub const RATE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// An agent's hop chain is forgotten when nothing reached it for this long
/// (a user's prompt, which its hooks report, forgets it sooner).
const CHAIN_TTL: Duration = Duration::from_secs(30 * 60);
/// A message only answers one sent to its sender within this window.
pub const REPLY_WINDOW_SECS: u64 = 60 * 60;
/// Most text handed to an inbox hook at once; further messages wait for the
/// next hand-over.
const MAX_BATCH: usize = 9000;
/// Replies kept for a `WaitReply` that comes after the reply.
const KEPT_REPLIES: usize = 64;

/// What the inbox hook shows the user for a delivery (`rewakeSummary`),
/// and the prefix of what the model sees (`rewakeMessage`).
pub const REWAKE_SUMMARY: &str = "tam message";
pub const REWAKE_PREFIX: &str = "[tam]";

/// One message as the recipient sees it.
pub fn format_message(m: &MessageInfo) -> String {
    let sender = match m.from {
        Some(_) => m.from_label.as_str(),
        None => "the user",
    };
    let mut out = format!("Message {} from {}:\n{}", m.id, sender, m.text.trim_end());
    if m.from.is_some() {
        out.push_str(&format!(
            "\nTo reply, run: tam msg {} \"<reply>\"",
            m.from_label
        ));
    }
    out
}

/// The messages to hand to an inbox hook at once: in order, as many as fit
/// in [`MAX_BATCH`] (at least one). Returns the text and the ids.
pub fn batch(pending: &[MessageInfo]) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut ids = Vec::new();
    for m in pending {
        let block = format_message(m);
        if !ids.is_empty() && text.len() + block.len() + 2 > MAX_BATCH {
            break;
        }
        if !ids.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&block);
        ids.push(m.id.clone());
    }
    (text, ids)
}

/// Message ids (`msg-` and 8 hex digits) found in `text`.
pub fn ids_in(text: &str, ids: &[String]) -> Vec<String> {
    ids.iter()
        .filter(|id| text.contains(id.as_str()))
        .cloned()
        .collect()
}

/// Whether a prompt the agent got was typed by its user, as opposed to a
/// background notification (a tam delivery, a finished background task…).
pub fn is_user_prompt(prompt: &str) -> bool {
    !prompt.trim_start().starts_with("<task-notification>")
}

/// What an inbox hook waiter gets.
#[derive(Debug)]
pub enum Handover {
    /// Messages to show the agent: the formatted text and their ids.
    Messages { text: String, ids: Vec<String> },
    /// A newer waiter took over, or the agent is gone: exit quietly.
    Released,
}

/// How a wait for a reply ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyOutcome {
    /// `message` is the reply message, or `None` for the recipient's last
    /// response.
    Reply {
        message: Option<MessageInfo>,
        text: String,
    },
    Failed(String),
}

struct Waiter {
    token: u64,
    tx: oneshot::Sender<Handover>,
}

/// Daemon-side messaging state that does not outlive the daemon.
#[derive(Default)]
pub struct Inbox {
    /// The inbox hook waiting for each agent's messages (the newest one).
    waiters: HashMap<String, Waiter>,
    next_token: u64,
    /// Message ids handed over to each member's agent, not yet confirmed.
    in_flight: HashMap<String, Vec<String>>,
    /// Message ids delivered to each member's agent in its current turn:
    /// their reply is the turn's last response.
    this_turn: HashMap<String, Vec<String>>,
    /// Highest hop delivered to each member since its user last prompted it.
    chains: HashMap<String, (u32, Instant)>,
    /// When each member sent its recent messages.
    sent: HashMap<String, VecDeque<Instant>>,
    reply_waiters: HashMap<String, Vec<oneshot::Sender<ReplyOutcome>>>,
    replies: VecDeque<(String, ReplyOutcome)>,
}

impl Inbox {
    /// Register an agent's inbox hook, releasing the one it replaces.
    pub fn register_waiter(&mut self, agent: &str) -> (u64, oneshot::Receiver<Handover>) {
        let (tx, rx) = oneshot::channel();
        self.next_token += 1;
        let token = self.next_token;
        if let Some(old) = self.waiters.insert(agent.to_string(), Waiter { token, tx }) {
            let _ = old.tx.send(Handover::Released);
        }
        (token, rx)
    }

    /// Forget a waiter that went away (if it is still the agent's).
    pub fn unregister_waiter(&mut self, agent: &str, token: u64) {
        if self.waiters.get(agent).is_some_and(|w| w.token == token) {
            self.waiters.remove(agent);
        }
    }

    pub fn has_waiter(&self, agent: &str) -> bool {
        self.waiters.get(agent).is_some_and(|w| !w.tx.is_closed())
    }

    /// Hand messages to the agent's waiter, which is used up. They are in
    /// flight until confirmed. `false` when no waiter took them.
    pub fn hand_over(&mut self, agent: &str, member: &str, text: String, ids: Vec<String>) -> bool {
        let Some(waiter) = self.waiters.remove(agent) else {
            return false;
        };
        let handed = ids.clone();
        if waiter.tx.send(Handover::Messages { text, ids }).is_err() {
            return false;
        }
        self.in_flight
            .entry(member.to_string())
            .or_default()
            .extend(handed);
        true
    }

    pub fn in_flight(&self, member: &str) -> &[String] {
        self.in_flight
            .get(member)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Put messages in flight back in the queue (or drop them from flight
    /// once delivered).
    pub fn land(&mut self, member: &str, ids: &[String]) {
        if let Some(flying) = self.in_flight.get_mut(member) {
            flying.retain(|id| !ids.contains(id));
            if flying.is_empty() {
                self.in_flight.remove(member);
            }
        }
    }

    /// The member's agent is gone: release its waiter; what was in flight is
    /// queued again, for its next agent.
    pub fn agent_gone(&mut self, agent: &str, member: Option<&str>) {
        if let Some(waiter) = self.waiters.remove(agent) {
            let _ = waiter.tx.send(Handover::Released);
        }
        if let Some(member) = member {
            self.in_flight.remove(member);
            self.this_turn.remove(member);
        }
    }

    /// A message reached `member` in the turn it is running.
    pub fn delivered_this_turn(&mut self, member: &str, id: &str) {
        self.this_turn
            .entry(member.to_string())
            .or_default()
            .push(id.to_string());
    }

    /// The member's turn ended: the messages it got during that turn.
    pub fn turn_ended(&mut self, member: &str) -> Vec<String> {
        self.this_turn.remove(member).unwrap_or_default()
    }

    /// Hop of a message `member` would send now.
    pub fn next_hop(&self, member: &str) -> u32 {
        match self.chains.get(member) {
            Some((hop, at)) if at.elapsed() < CHAIN_TTL => hop + 1,
            _ => 1,
        }
    }

    /// A message with `hop` reached `member` (hop 0, from the user, resets
    /// its chain like a prompt the user types).
    pub fn record_hop(&mut self, member: &str, hop: u32) {
        if hop == 0 {
            self.chains.remove(member);
            return;
        }
        let current = match self.chains.get(member) {
            Some((h, at)) if at.elapsed() < CHAIN_TTL => *h,
            _ => 0,
        };
        self.chains
            .insert(member.to_string(), (current.max(hop), Instant::now()));
    }

    /// The member's user prompted it: its exchanges start over.
    pub fn reset_chain(&mut self, member: &str) {
        self.chains.remove(member);
    }

    /// Count a message `member` sends now, unless it is over the rate limit.
    pub fn check_rate(&mut self, member: &str) -> Result<(), String> {
        let sent = self.sent.entry(member.to_string()).or_default();
        while sent.front().is_some_and(|t| t.elapsed() >= RATE_WINDOW) {
            sent.pop_front();
        }
        if sent.len() >= RATE_LIMIT {
            return Err(format!(
                "rate limit: at most {RATE_LIMIT} messages per {} minutes",
                RATE_WINDOW.as_secs() / 60
            ));
        }
        sent.push_back(Instant::now());
        Ok(())
    }

    /// Wait for the reply to message `id`: `Ok` when it came already.
    pub fn wait_reply(
        &mut self,
        id: &str,
    ) -> Result<ReplyOutcome, oneshot::Receiver<ReplyOutcome>> {
        if let Some((_, outcome)) = self.replies.iter().find(|(r, _)| r == id) {
            return Ok(outcome.clone());
        }
        let (tx, rx) = oneshot::channel();
        self.reply_waiters
            .entry(id.to_string())
            .or_default()
            .push(tx);
        Err(rx)
    }

    /// Forget the reply waiters of `id` that gave up.
    pub fn prune_reply_waiters(&mut self, id: &str) {
        if let Some(waiters) = self.reply_waiters.get_mut(id) {
            waiters.retain(|tx| !tx.is_closed());
            if waiters.is_empty() {
                self.reply_waiters.remove(id);
            }
        }
    }

    /// The reply to message `id` came (only the first one counts). Returns
    /// how many waiters got it.
    pub fn replied(&mut self, id: &str, outcome: ReplyOutcome) -> usize {
        if self.replies.iter().any(|(r, _)| r == id) {
            return 0;
        }
        let served = self
            .reply_waiters
            .remove(id)
            .unwrap_or_default()
            .into_iter()
            .filter(|tx| !tx.is_closed())
            .filter_map(|tx| tx.send(outcome.clone()).ok())
            .count();
        self.replies.push_back((id.to_string(), outcome));
        if self.replies.len() > KEPT_REPLIES {
            self.replies.pop_front();
        }
        served
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, from: Option<&str>, text: &str) -> MessageInfo {
        MessageInfo {
            id: id.into(),
            from: from.map(str::to_string),
            from_label: from.map(|_| "feat/api").unwrap_or("user").into(),
            to: "m-2".into(),
            to_label: "feat/web".into(),
            text: text.into(),
            hop: 1,
            reply_to: None,
            sent_at: 0,
            delivered_at: None,
        }
    }

    #[test]
    fn messages_name_their_sender_and_how_to_reply() {
        assert_eq!(
            format_message(&msg("msg-1", Some("m-1"), "tests pass\n")),
            "Message msg-1 from feat/api:\ntests pass\nTo reply, run: tam msg feat/api \"<reply>\""
        );
        // The user cannot be answered with a message.
        assert_eq!(
            format_message(&msg("msg-2", None, "stop")),
            "Message msg-2 from the user:\nstop"
        );
    }

    #[test]
    fn batches_are_capped_but_never_empty() {
        let big = "x".repeat(5000);
        let pending = vec![
            msg("msg-1", None, &big),
            msg("msg-2", None, &big),
            msg("msg-3", None, "small"),
        ];
        let (text, ids) = batch(&pending);
        assert_eq!(ids, ["msg-1"]);
        assert!(text.starts_with("Message msg-1"));
        let (text, ids) = batch(&pending[2..]);
        assert_eq!(ids, ["msg-3"]);
        assert!(text.ends_with("small"));
        let small = vec![msg("msg-1", None, "a"), msg("msg-2", None, "b")];
        let (text, ids) = batch(&small);
        assert_eq!(ids, ["msg-1", "msg-2"]);
        assert!(text.contains("a\n\nMessage msg-2"));
        // A single oversized message still goes.
        let (_, ids) = batch(&[msg("msg-9", None, &"y".repeat(MAX_BATCH + 10))]);
        assert_eq!(ids, ["msg-9"]);
    }

    #[test]
    fn delivered_ids_are_found_in_the_prompt() {
        let ids = vec!["msg-0000000a".to_string(), "msg-0000000b".to_string()];
        let prompt = "<task-notification>…</task-notification>\n<system-reminder>\n[tam] Message msg-0000000b from the user:\nhi\n</system-reminder>";
        assert_eq!(ids_in(prompt, &ids), ["msg-0000000b"]);
        assert!(!is_user_prompt(prompt));
        assert!(is_user_prompt("fix the build"));
    }

    #[test]
    fn a_newer_waiter_releases_the_older_one() {
        let mut inbox = Inbox::default();
        let (t1, mut rx1) = inbox.register_waiter("m-1");
        let (_t2, mut rx2) = inbox.register_waiter("m-1");
        assert!(matches!(rx1.try_recv(), Ok(Handover::Released)));
        // The old token no longer unregisters the new waiter.
        inbox.unregister_waiter("m-1", t1);
        assert!(inbox.has_waiter("m-1"));

        assert!(inbox.hand_over("m-1", "m-1", "text".into(), vec!["msg-1".into()]));
        assert!(matches!(
            rx2.try_recv(),
            Ok(Handover::Messages { ids, .. }) if ids == ["msg-1"]
        ));
        assert_eq!(inbox.in_flight("m-1"), ["msg-1"]);
        // The waiter is used up.
        assert!(!inbox.has_waiter("m-1"));
        assert!(!inbox.hand_over("m-1", "m-1", "more".into(), vec!["msg-2".into()]));

        inbox.land("m-1", &["msg-1".into()]);
        assert!(inbox.in_flight("m-1").is_empty());
    }

    #[test]
    fn a_dead_waiter_takes_nothing() {
        let mut inbox = Inbox::default();
        let (_, rx) = inbox.register_waiter("m-1");
        drop(rx);
        assert!(!inbox.has_waiter("m-1"));
        assert!(!inbox.hand_over("m-1", "m-1", "t".into(), vec!["msg-1".into()]));
        assert!(inbox.in_flight("m-1").is_empty());
    }

    #[test]
    fn agent_exit_requeues_what_was_in_flight() {
        let mut inbox = Inbox::default();
        let (_, mut rx) = inbox.register_waiter("m-1");
        assert!(inbox.hand_over("m-1", "m-1", "t".into(), vec!["msg-1".into()]));
        assert!(matches!(rx.try_recv(), Ok(Handover::Messages { .. })));
        let (_, mut rx) = inbox.register_waiter("m-1");
        inbox.delivered_this_turn("m-1", "msg-0");
        inbox.agent_gone("m-1", Some("m-1"));
        assert!(matches!(rx.try_recv(), Ok(Handover::Released)));
        assert!(inbox.in_flight("m-1").is_empty());
        assert!(inbox.turn_ended("m-1").is_empty());
    }

    #[test]
    fn hop_chains_grow_and_reset() {
        let mut inbox = Inbox::default();
        assert_eq!(inbox.next_hop("a"), 1);
        inbox.record_hop("a", 3);
        inbox.record_hop("a", 2);
        assert_eq!(inbox.next_hop("a"), 4);
        inbox.reset_chain("a");
        assert_eq!(inbox.next_hop("a"), 1);
        inbox.record_hop("a", 5);
        // A message from the user starts over.
        inbox.record_hop("a", 0);
        assert_eq!(inbox.next_hop("a"), 1);
    }

    #[test]
    fn rate_limit_per_sender() {
        let mut inbox = Inbox::default();
        for _ in 0..RATE_LIMIT {
            inbox.check_rate("a").unwrap();
        }
        assert!(inbox.check_rate("a").unwrap_err().contains("rate limit"));
        inbox.check_rate("b").unwrap();
    }

    #[test]
    fn first_reply_wins_and_is_kept_for_late_waiters() {
        let mut inbox = Inbox::default();
        let mut rx = inbox.wait_reply("msg-1").unwrap_err();
        let first = ReplyOutcome::Reply {
            message: None,
            text: "done".into(),
        };
        assert_eq!(inbox.replied("msg-1", first.clone()), 1);
        assert_eq!(
            inbox.replied("msg-1", ReplyOutcome::Failed("late".into())),
            0
        );
        assert_eq!(rx.try_recv().unwrap(), first);
        assert_eq!(inbox.wait_reply("msg-1").unwrap(), first);

        let rx = inbox.wait_reply("msg-2").unwrap_err();
        drop(rx);
        inbox.prune_reply_waiters("msg-2");
        assert!(inbox.reply_waiters.is_empty());
    }
}
