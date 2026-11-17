//! Fuzzy matching for the task filter and the project picker: the query's
//! characters in order in the text, not necessarily together. A match
//! scores more for runs of them and for starting words (`tam` in
//! `tam-cli`, `fe` in `fix-everything`); the best alignment is kept, with
//! where it matched, to highlight. Smart case: a query with a capital is
//! matched as typed, else case is ignored.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

const MATCH: i32 = 16;
/// The character starts a word: the text's first, after a separator, or a
/// capital after a lower-case letter.
const BOUNDARY: i32 = 10;
/// The character follows the previous match.
const CONSECUTIVE: i32 = 12;
const GAP_START: i32 = -3;
const GAP_EXTENSION: i32 = -1;

/// Where and how well a query matched a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    /// Indices of the matched characters in the text (characters, not
    /// bytes), increasing.
    pub positions: Vec<usize>,
}

/// How `query` matches `text`, if all its characters are in it in order.
/// An empty query matches anything (score 0).
pub fn fuzzy(query: &str, text: &str) -> Option<Match> {
    let exact = query.chars().any(char::is_uppercase);
    let fold = |c: char| {
        if exact {
            c
        } else {
            c.to_lowercase().next().unwrap_or(c)
        }
    };
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(fold)
        .collect();
    let original: Vec<char> = text.chars().collect();
    let t: Vec<char> = original.iter().copied().map(fold).collect();
    if q.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let (m, n) = (q.len(), t.len());
    if m > n {
        return None;
    }
    let bonus: Vec<i32> = (0..n)
        .map(|j| {
            let start = j == 0
                || matches!(
                    original[j - 1],
                    ' ' | '-' | '_' | '/' | '.' | ':' | '@' | ','
                )
                || (original[j - 1].is_lowercase() && original[j].is_uppercase());
            if start {
                BOUNDARY
            } else {
                0
            }
        })
        .collect();

    // score[i][j]: the best with q[..=i] matched, q[i] at t[j]; from[i][j]:
    // where q[i - 1] was then.
    const NONE: i32 = i32::MIN / 2;
    let mut score = vec![vec![NONE; n]; m];
    let mut from = vec![vec![0usize; n]; m];
    for j in 0..n {
        if t[j] == q[0] {
            score[0][j] = MATCH + bonus[j];
        }
    }
    for i in 1..m {
        // The best previous match two or more characters back, the gap
        // penalized, and where it was.
        let mut gap = NONE;
        let mut gap_at = 0;
        for j in 1..n {
            if j >= 2 {
                gap += GAP_EXTENSION;
                let candidate = score[i - 1][j - 2] + GAP_START;
                if candidate > gap {
                    gap = candidate;
                    gap_at = j - 2;
                }
            }
            if t[j] != q[i] {
                continue;
            }
            let next = score[i - 1][j - 1] + CONSECUTIVE;
            let (best, at) = if next >= gap {
                (next, j - 1)
            } else {
                (gap, gap_at)
            };
            if best > NONE / 2 {
                score[i][j] = best + MATCH + bonus[j];
                from[i][j] = at;
            }
        }
    }
    let (mut j, best) = score[m - 1]
        .iter()
        .copied()
        .enumerate()
        .filter(|&(_, s)| s > NONE / 2)
        .max_by_key(|&(j, s)| (s, std::cmp::Reverse(j)))?;
    let mut positions = vec![0; m];
    for i in (0..m).rev() {
        positions[i] = j;
        j = from[i][j];
    }
    Some(Match {
        score: best,
        positions,
    })
}

/// `text` with the characters at `positions` in `matched`'s style.
pub fn highlight(text: &str, positions: &[usize], base: Style, matched: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    for (i, c) in text.chars().enumerate() {
        let is_matched = positions.binary_search(&i).is_ok();
        if is_matched != run_matched && !run.is_empty() {
            let style = if run_matched { matched } else { base };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_matched = is_matched;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_matched { matched } else { base }));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positions(query: &str, text: &str) -> Option<Vec<usize>> {
        fuzzy(query, text).map(|m| m.positions)
    }

    fn score(query: &str, text: &str) -> i32 {
        fuzzy(query, text).map_or(i32::MIN, |m| m.score)
    }

    #[test]
    fn matches_the_characters_in_order() {
        assert_eq!(positions("fnav", "fix-nav"), Some(vec![0, 4, 5, 6]));
        assert_eq!(positions("", "anything"), Some(vec![]));
        assert_eq!(positions("vanf", "fix-nav"), None);
        assert_eq!(positions("abc", "ab"), None);
        // Spaces in the query are ignored.
        assert_eq!(positions("fix nav", "fix-nav").map(|p| p.len()), Some(6));
    }

    #[test]
    fn prefers_runs_and_word_starts() {
        // `tc` at the start of both words, not in `tam-cli`'s middle.
        assert_eq!(positions("tc", "tam-cli"), Some(vec![0, 4]));
        // A run beats scattered characters.
        assert!(score("nav", "fix-nav") > score("nav", "n-a-v"));
        // A word start beats the middle of a word.
        assert!(score("ui", "tam-ui") > score("ui", "build"));
        // The best alignment, not the first one found.
        assert_eq!(
            positions("cache", "c-a-c-h-e-cache"),
            Some(vec![10, 11, 12, 13, 14])
        );
    }

    #[test]
    fn smart_case() {
        assert!(fuzzy("tam", "TAM").is_some());
        assert!(fuzzy("Tam", "tam").is_none());
        assert!(fuzzy("Tam", "Tam").is_some());
    }

    #[test]
    fn highlighting() {
        let bold = Style::new().add_modifier(ratatui::style::Modifier::BOLD);
        let line = highlight("fix-nav", &[0, 4, 5, 6], Style::new(), bold);
        let parts: Vec<(&str, bool)> = line
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style == bold))
            .collect();
        assert_eq!(parts, [("f", true), ("ix-", false), ("nav", true)]);
    }
}
