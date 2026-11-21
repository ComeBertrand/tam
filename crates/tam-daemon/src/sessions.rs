//! Claude's session files, read for chats: a session's title and last
//! prompt, and the sessions of a repository's main checkout (its history,
//! what a chat can be made from).
//!
//! Claude writes a session to `<claude dir>/projects/<project>/<id>.jsonl`,
//! `<project>` being the directory it runs in, every character but ASCII
//! letters and digits made a `-` (cut at 200, plus a hash, when longer:
//! [`project_name`]). The format is Claude's own and undocumented; this
//! module is the only one that reads it, and a file it cannot read gives
//! nothing, never an error. A file is one JSON object per line; the lines
//! read here (as Claude 2.1 writes them):
//!
//! - `{"type":"ai-title","aiTitle":…,"sessionId":…}`: the title Claude
//!   generates, written again as the conversation moves;
//! - `{"type":"custom-title","customTitle":…,"sessionId":…}`: the name the
//!   user gave the session in Claude (`/rename`, or the desktop app's
//!   rename); an empty one takes it back;
//! - `{"type":"last-prompt","lastPrompt":…,"leafUuid":…,"sessionId":…}`:
//!   the latest prompt typed, on one line, cut at 200 characters.
//!
//! The latest line of each kind wins, and Claude shows the user's name
//! before its own title. A file can be tens of megabytes: Claude writes
//! these lines again at its end every 32 KiB it appends, and reads them
//! from the last 64 KiB, then the first 64 KiB. So does this module
//! ([`WINDOW`]).
//!
//! Known limit, as for transcripts elsewhere in the daemon: the files are
//! looked for under the daemon's own `HOME`/`CLAUDE_CONFIG_DIR`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use tam_proto::SessionInfo;

/// How much of a file's end (and, for what is not there, of its start) is
/// read: what Claude reads for its own session list.
pub const WINDOW: u64 = 64 * 1024;

/// Longest title or prompt kept, in characters (Claude's own cut for a
/// prompt).
const MAX_TEXT: usize = 200;

/// Longest project name before Claude cuts it and adds a hash.
const MAX_PROJECT_NAME: usize = 200;

/// Claude's data directory: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude")),
    }
}

/// The directory of Claude's sessions run in `dir`.
pub fn project_dir(claude_dir: &Path, dir: &Path) -> PathBuf {
    claude_dir.join("projects").join(project_name(dir))
}

/// How Claude names the project of `dir`: every UTF-16 unit but ASCII
/// letters and digits made a `-`; past 200, cut there and followed by `-`
/// and a hash of the path (base 36).
pub fn project_name(dir: &Path) -> String {
    let path = dir.to_string_lossy();
    let mut name = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else {
            name.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    if name.len() <= MAX_PROJECT_NAME {
        return name;
    }
    // ASCII only by now: bytes are characters.
    name.truncate(MAX_PROJECT_NAME);
    format!("{name}-{}", base36(path_hash(&path).unsigned_abs().into()))
}

/// The 32-bit string hash Claude takes of a long project path
/// (`h = h * 31 + unit`, over UTF-16 units).
fn path_hash(path: &str) -> i32 {
    path.encode_utf16()
        .fold(0i32, |h, unit| h.wrapping_mul(31).wrapping_add(unit.into()))
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII digits")
}

/// The file of session `session` run in `dir`, whether it exists or not.
/// `None` for what cannot be a session id (empty, a path).
pub fn session_file(claude_dir: &Path, dir: &Path, session: &str) -> Option<PathBuf> {
    if session.is_empty() || session.contains(['/', '\\']) || session.starts_with('.') {
        return None;
    }
    Some(project_dir(claude_dir, dir).join(format!("{session}.jsonl")))
}

/// What a session's file says of it: its title (the user's name for it,
/// else Claude's), its latest prompt, and when it was last written.
/// `None` when it cannot be read.
pub fn read(file: &Path) -> Option<SessionInfo> {
    let id = file
        .file_name()?
        .to_str()?
        .strip_suffix(".jsonl")
        .filter(|id| !id.is_empty() && !id.starts_with('.'))?;
    let mut f = File::open(file).ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified_at = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());

    let len = meta.len();
    let start = len.saturating_sub(WINDOW);
    let mut found = Found::default();
    found.scan(&read_at(&mut f, start, len - start)?, start > 0, id);
    // A file the end did not hold all of: what is missing may be at its
    // start, as Claude looks for it.
    if start > 0 && !found.complete() {
        let mut head = Found::default();
        head.scan(&read_at(&mut f, 0, WINDOW.min(start))?, false, id);
        found.fill_from(head);
    }
    Some(SessionInfo {
        id: id.to_string(),
        title: found.title(),
        last_prompt: found.last_prompt.as_deref().and_then(clean),
        modified_at,
    })
}

/// The sessions of `dir` (a repository's main checkout) that have a title
/// or a prompt, newest first. A session nobody typed in says nothing worth
/// listing (a new chat's, before its first turn).
pub fn history(claude_dir: &Path, dir: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(project_dir(claude_dir, dir)) else {
        return Vec::new();
    };
    let mut sessions: Vec<SessionInfo> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|p| read(&p))
        .filter(|s| s.title.is_some() || s.last_prompt.is_some())
        .collect();
    sessions.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    sessions
}

/// `len` bytes of `f` from `start` (fewer if it shrank meanwhile).
fn read_at(f: &mut File, start: u64, len: u64) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// The lines this module reads; any other field is left alone.
#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "lastPrompt")]
    last_prompt: Option<String>,
}

/// The latest of each kind of line in a part of a file, as written (an
/// empty custom title is found: it takes the name back).
#[derive(Default)]
struct Found {
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
}

impl Found {
    /// Read `bytes`, a part of the file of session `id`. `mid_line`: it
    /// starts inside a line, which is skipped. A line cut at the end does
    /// not parse, and is skipped too.
    fn scan(&mut self, bytes: &[u8], mid_line: bool, id: &str) {
        let mut lines = bytes.split(|&b| b == b'\n');
        if mid_line {
            lines.next();
        }
        for line in lines {
            // Most lines are not these: no JSON parsed for them.
            if !contains(line, b"-title") && !contains(line, b"last-prompt") {
                continue;
            }
            let Ok(line) = serde_json::from_slice::<Line>(line) else {
                continue;
            };
            if line.session_id.as_deref().is_some_and(|s| s != id) {
                continue;
            }
            match line.kind.as_str() {
                "custom-title" if line.custom_title.is_some() => {
                    self.custom_title = line.custom_title
                }
                "ai-title" if line.ai_title.is_some() => self.ai_title = line.ai_title,
                "last-prompt" if line.last_prompt.is_some() => self.last_prompt = line.last_prompt,
                _ => {}
            }
        }
    }

    fn complete(&self) -> bool {
        self.custom_title.is_some() && self.ai_title.is_some() && self.last_prompt.is_some()
    }

    /// Take from `earlier` (the start of the file) what this has not.
    fn fill_from(&mut self, earlier: Found) {
        self.custom_title = self.custom_title.take().or(earlier.custom_title);
        self.ai_title = self.ai_title.take().or(earlier.ai_title);
        self.last_prompt = self.last_prompt.take().or(earlier.last_prompt);
    }

    /// The user's name for the session, else Claude's title.
    fn title(&self) -> Option<String> {
        self.custom_title
            .as_deref()
            .and_then(clean)
            .or_else(|| self.ai_title.as_deref().and_then(clean))
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `text` fit to show on one line: control characters made spaces, runs
/// of whitespace one space, trimmed, cut at [`MAX_TEXT`] characters (with
/// `…`). `None` when nothing is left.
fn clean(text: &str) -> Option<String> {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let line = words.join(" ");
    if line.is_empty() {
        return None;
    }
    match line.char_indices().nth(MAX_TEXT) {
        Some((cut, _)) => Some(format!("{}…", line[..cut].trim_end())),
        None => Some(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    const S1: &str = "0b7a8f9e-0000-4000-8000-000000000001";

    fn ai(session: &str, title: &str) -> String {
        serde_json::json!({"type": "ai-title", "aiTitle": title, "sessionId": session}).to_string()
    }

    fn custom(session: &str, title: &str) -> String {
        serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": session})
            .to_string()
    }

    fn prompt(session: &str, text: &str) -> String {
        serde_json::json!({
            "type": "last-prompt",
            "lastPrompt": text,
            "leafUuid": "00000000-0000-4000-8000-0000000000aa",
            "sessionId": session,
        })
        .to_string()
    }

    /// A conversation line, as large as wanted (a tool's output).
    fn filler(session: &str, bytes: usize) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": session,
            "message": {"role": "assistant", "content": "x".repeat(bytes)},
        })
        .to_string()
    }

    fn write(file: &Path, lines: &[String]) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut f = File::create(file).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    fn set_mtime(file: &Path, secs: u64) {
        File::options()
            .append(true)
            .open(file)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn session(dir: &Path, id: &str, lines: &[String]) -> SessionInfo {
        let file = dir.join(format!("{id}.jsonl"));
        write(&file, lines);
        read(&file).unwrap()
    }

    #[test]
    fn project_names_are_claudes() {
        assert_eq!(
            project_name(Path::new("/home/user/Workspace/tam")),
            "-home-user-Workspace-tam"
        );
        assert_eq!(project_name(Path::new("/")), "-");
        // Dots, underscores, spaces: all `-` (a hidden directory doubles it).
        assert_eq!(
            project_name(Path::new("/home/u/.local/a_b c/tam")),
            "-home-u--local-a-b-c-tam"
        );
        // One `-` per UTF-16 unit: two for a character outside the BMP.
        assert_eq!(project_name(Path::new("/w/é/🦀/x")), "-w------x");
        // Longer than 200: cut, then a hash of the path. The expected
        // values are computed as Claude's JavaScript computes them.
        let long = format!("/home/user/{}repo", "very-long-directory-name/".repeat(9));
        let name = project_name(Path::new(&long));
        assert_eq!(name.len(), 207);
        assert!(name.ends_with("-very-long-dire-lwzqcn"), "{name}");
        assert_eq!(path_hash(&long), -1325204519);
        let accented = format!("/srv/é/{}", "x".repeat(210));
        assert_eq!(path_hash(&accented), -1887427027);
        assert!(project_name(Path::new(&accented)).ends_with("xxxxx-v7q40j"));
        assert_eq!(base36(0), "0");
        assert_eq!(base36(i32::MIN.unsigned_abs().into()), "zik0zk");
    }

    #[test]
    fn session_files_are_in_the_project_of_their_directory() {
        let claude = Path::new("/c");
        assert_eq!(
            session_file(claude, Path::new("/src/api"), S1),
            Some(PathBuf::from(format!("/c/projects/-src-api/{S1}.jsonl")))
        );
        for bad in ["", "../x", "a/b", ".hidden", "a\\b"] {
            assert_eq!(session_file(claude, Path::new("/src/api"), bad), None);
        }
    }

    #[test]
    fn the_latest_title_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session(
            tmp.path(),
            S1,
            &[
                prompt(S1, "find the leak"),
                ai(S1, "Investigate a leak"),
                filler(S1, 100),
                prompt(S1, "and fix it"),
                ai(S1, "Fix the connection pool leak"),
            ],
        );
        assert_eq!(s.id, S1);
        assert_eq!(s.title.as_deref(), Some("Fix the connection pool leak"));
        assert_eq!(s.last_prompt.as_deref(), Some("and fix it"));
    }

    #[test]
    fn a_file_without_a_title_has_its_prompt_only() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session(tmp.path(), S1, &[prompt(S1, "hello"), filler(S1, 10)]);
        assert_eq!(s.title, None);
        assert_eq!(s.last_prompt.as_deref(), Some("hello"));
        // Nothing at all: nothing, not an error.
        let s = session(tmp.path(), "empty", &[filler("empty", 10)]);
        assert_eq!((s.title, s.last_prompt), (None, None));
        // A last-prompt line without its prompt (Claude writes some) keeps
        // the earlier one.
        let bare = serde_json::json!({"type": "last-prompt", "leafUuid": "x", "sessionId": S1});
        let s = session(tmp.path(), S1, &[prompt(S1, "first"), bare.to_string()]);
        assert_eq!(s.last_prompt.as_deref(), Some("first"));
    }

    #[test]
    fn the_users_name_wins_over_claudes_title_until_taken_back() {
        let tmp = tempfile::tempdir().unwrap();
        // Even an older name wins over a newer generated title.
        let named = [
            ai(S1, "Generated"),
            custom(S1, "deploy friday"),
            ai(S1, "Generated again"),
        ];
        let s = session(tmp.path(), S1, &named);
        assert_eq!(s.title.as_deref(), Some("deploy friday"));
        // An empty name takes it back: Claude's title shows again.
        let mut cleared = named.to_vec();
        cleared.push(custom(S1, ""));
        let s = session(tmp.path(), S1, &cleared);
        assert_eq!(s.title.as_deref(), Some("Generated again"));
    }

    #[test]
    fn a_large_file_is_read_from_its_end_then_its_start() {
        let tmp = tempfile::tempdir().unwrap();
        let big = (2 * WINDOW) as usize;
        // Claude wrote its lines again near the end: the end's win.
        let s = session(
            tmp.path(),
            S1,
            &[
                custom(S1, "old name"),
                ai(S1, "Old title"),
                prompt(S1, "old prompt"),
                filler(S1, big),
                filler(S1, big),
                custom(S1, "new name"),
                ai(S1, "New title"),
                prompt(S1, "new prompt"),
                filler(S1, 1000),
            ],
        );
        assert_eq!(s.title.as_deref(), Some("new name"));
        assert_eq!(s.last_prompt.as_deref(), Some("new prompt"));
        // Not at the end: found at the start, as Claude finds it. A name
        // at the start still wins over a title at the end.
        let s = session(
            tmp.path(),
            S1,
            &[
                custom(S1, "named early"),
                prompt(S1, "early prompt"),
                filler(S1, big),
                ai(S1, "Late title"),
            ],
        );
        assert_eq!(s.title.as_deref(), Some("named early"));
        assert_eq!(s.last_prompt.as_deref(), Some("early prompt"));
        // In the middle only: not read (Claude would not show it either).
        let s = session(
            tmp.path(),
            S1,
            &[
                filler(S1, big),
                ai(S1, "Lost in the middle"),
                filler(S1, big),
            ],
        );
        assert_eq!(s.title, None);
    }

    #[test]
    fn a_window_starting_inside_a_line_skips_it() {
        let tmp = tempfile::tempdir().unwrap();
        // A line ending in what reads as a title line, the window starting
        // right where it does: it is no line of its own.
        let inner = ai(S1, "Not a title");
        let junk = format!("{}{inner}", "y".repeat(100));
        // Lines after it fill the window up from there.
        let overhead = filler(S1, 0).len();
        let rest = filler(S1, WINDOW as usize - inner.len() - 2 - overhead);
        let lines = [ai(S1, "Real"), junk, rest];
        let file = tmp.path().join(format!("{S1}.jsonl"));
        write(&file, &lines);
        let len = std::fs::metadata(&file).unwrap().len();
        let start = (len - WINDOW) as usize;
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            bytes[start..].starts_with(inner.as_bytes()),
            "window misplaced"
        );
        assert_eq!(read(&file).unwrap().title.as_deref(), Some("Real"));
    }

    #[test]
    fn lines_of_another_session_and_broken_lines_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session(
            tmp.path(),
            S1,
            &[
                ai(S1, "Mine"),
                ai("other-session", "Not mine"),
                "{\"type\":\"ai-title\",\"aiTitle\":".into(),
                "{\"type\":\"ai-title\",\"aiTitle\":42}".into(),
                "not json -title".into(),
            ],
        );
        assert_eq!(s.title.as_deref(), Some("Mine"));
        assert_eq!(read(&tmp.path().join("missing.jsonl")), None);
        std::fs::create_dir(tmp.path().join("dir.jsonl")).unwrap();
        assert_eq!(read(&tmp.path().join("dir.jsonl")), None);
    }

    #[test]
    fn text_is_made_one_short_line() {
        assert_eq!(
            clean("  a\n\tb\u{1b}[31m  c\u{7} "),
            Some("a b [31m c".into())
        );
        assert_eq!(clean(" \n "), None);
        let long = "é".repeat(MAX_TEXT + 5);
        let cut = clean(&long).unwrap();
        assert_eq!(cut.chars().count(), MAX_TEXT + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(
            clean(&"é".repeat(MAX_TEXT)).unwrap().chars().count(),
            MAX_TEXT
        );
    }

    #[test]
    fn history_lists_a_directorys_sessions_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path();
        let repo = Path::new("/src/api");
        let project = project_dir(claude, repo);
        let file = |id: &str| project.join(format!("{id}.jsonl"));
        write(
            &file("old"),
            &[prompt("old", "first try"), ai("old", "Old")],
        );
        write(&file("new"), &[prompt("new", "second try")]);
        write(&file("named"), &[custom("named", "mine")]);
        // Nobody typed in it: not listed.
        write(&file("blank"), &[filler("blank", 10)]);
        // Not sessions.
        write(&project.join("notes.txt"), &[ai("notes", "x")]);
        std::fs::create_dir_all(project.join("old").join("subagents")).unwrap();
        write(
            &project.join("old").join("subagents").join("agent-1.jsonl"),
            &[ai("old", "Subagent")],
        );
        // Another directory's.
        write(
            &project_dir(claude, Path::new("/src/web")).join("web.jsonl"),
            &[ai("web", "Web")],
        );
        set_mtime(&file("old"), 1_000);
        set_mtime(&file("new"), 3_000);
        set_mtime(&file("named"), 2_000);
        set_mtime(&file("blank"), 4_000);

        let history = history(claude, repo);
        let ids: Vec<&str> = history.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new", "named", "old"]);
        assert_eq!(
            history[2],
            SessionInfo {
                id: "old".into(),
                title: Some("Old".into()),
                last_prompt: Some("first try".into()),
                modified_at: 1_000,
            }
        );
        assert!(super::history(claude, Path::new("/src/none")).is_empty());
    }
}
