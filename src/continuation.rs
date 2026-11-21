//! Continuing a truncated answer instead of starting it again
//! (`SchedulerConfig::continue_truncated`).
//!
//! A worker answer that ends on `finish_reason = "length"` is cut at the
//! request's output budget. Without this the scheduler retries the whole
//! request with a doubled budget, so the model writes the same file again from
//! its first token. With it, the scheduler asks for the rest of the answer and
//! joins the parts; the joined answer then goes through the same checks as any
//! other answer.
//!
//! A client continues in one of two ways ([`ContinueMode`]):
//!
//! - [`ContinueMode::Prefix`]: the provider continues the same token stream
//!   from an assistant prefix (DeepSeek's chat prefix completion: a last
//!   assistant message with `"prefix": true` on the `/beta` endpoint). The
//!   answer is exactly the rest, so the parts are joined as they are.
//! - [`ContinueMode::Turn`] (the default of [`ModelClient`]): the request's
//!   user turn also carries what was written up to its last complete line and
//!   asks for the rest from the next line. The model may repeat a line or
//!   reopen the code fence, so [`join`] removes that overlap.

use crate::client::{Completion, CompletionRequest};

/// Continuations of one task's answer when `continue_truncated` is on and
/// the host sets no cap of its own.
pub const DEFAULT_CAP: u32 = 2;

/// How a continuation was asked for, which decides how its text is joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueMode {
    /// The provider continued the same stream: the text is exactly the rest.
    Prefix,
    /// A user turn asked for the rest from the line after the last complete
    /// line: the text may repeat lines (see [`join`]).
    Turn,
}

impl ContinueMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ContinueMode::Prefix => "prefix",
            ContinueMode::Turn => "turn",
        }
    }
}

/// One continuation's answer: the new text only, and how it was asked for.
#[derive(Debug, Clone)]
pub struct Continued {
    pub completion: Completion,
    pub mode: ContinueMode,
}

/// The part of `partial` that a [`ContinueMode::Turn`] request shows: up to
/// and including its last newline. The cut line is written again.
pub fn kept(partial: &str) -> &str {
    match partial.rfind('\n') {
        Some(at) => &partial[..=at],
        None => "",
    }
}

/// Start of the note a [`ContinueMode::Turn`] request appends to the user
/// turn.
pub const TURN_NOTE: &str =
    "# Continuation\nYour answer to this assignment was cut off at the output limit.";

/// The [`ContinueMode::Turn`] request for the rest of `partial`: `request`
/// with a note at the end of its user turn, so the system prompt, the shared
/// context and the assignment stay a byte-prefix of the first request.
pub fn turn_request(request: &CompletionRequest, partial: &str) -> CompletionRequest {
    let mut next = request.clone();
    next.user = format!(
        "{}\n\n{TURN_NOTE} Here is what you wrote, up to its last complete line, between the \
         markers:\n<<<WRITTEN\n{}WRITTEN>>>\nContinue from exactly the next line: output ONLY the \
         rest of the answer, starting with the line after the last line above. Do not repeat any \
         line above, do not start over and do not open a new code fence; end the answer as it \
         would have ended (close the open code fence).",
        request.user,
        kept(partial),
    );
    next
}

/// The chat prefix-completion endpoint for an OpenAI-compatible `base_url`,
/// for providers known to continue an assistant prefix: DeepSeek's
/// `{scheme}://{host}/beta/chat/completions`. `None` for any other host.
pub fn prefix_endpoint(base_url: &str) -> Option<String> {
    let (scheme, rest) = base_url.split_once("://")?;
    let host = rest.split('/').next().unwrap_or_default();
    host.ends_with("api.deepseek.com")
        .then(|| format!("{scheme}://{host}/beta/chat/completions"))
}

/// The last message of a prefix-completion request: the answer so far, which
/// the provider continues (`"prefix": true`).
pub fn prefix_message(partial: &str) -> serde_json::Value {
    serde_json::json!({"role": "assistant", "content": partial, "prefix": true})
}

/// `partial` followed by a continuation's `text`.
///
/// [`ContinueMode::Prefix`] joins them as they are. [`ContinueMode::Turn`]
/// joins `text` to [`kept`]`(partial)` and drops what the model repeated:
/// - an answer that starts over (its first two lines are the partial's
///   first two) replaces the partial;
/// - a code fence it opens again while the partial's fence is open;
/// - the longest run of lines (at most [`MAX_OVERLAP_LINES`]) that ends the
///   kept part and starts the text, when those lines hold at least
///   [`MIN_OVERLAP_CHARS`] non-blank characters (closing lines such as
///   `</div>` twice, or a few short assertion rows, are not taken for a
///   repeat: code repeats those lines on its own).
pub fn join(mode: ContinueMode, partial: &str, text: &str) -> String {
    match mode {
        ContinueMode::Prefix => format!("{partial}{text}"),
        ContinueMode::Turn => join_turn(kept(partial), text),
    }
}

/// Lines of overlap [`join`] looks for.
pub const MAX_OVERLAP_LINES: usize = 60;
/// Non-blank characters an overlap must hold to be dropped. High enough that
/// short lines a file legitimately repeats (closing tags, braces, one-line
/// assertions) are never cut from a correct continuation.
pub const MIN_OVERLAP_CHARS: usize = 40;

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

fn join_turn(kept: &str, text: &str) -> String {
    let kept_lines: Vec<&str> = kept.split_inclusive('\n').collect();
    let mut rest: Vec<&str> = text.split_inclusive('\n').collect();
    let same = |a: &str, b: &str| a.trim_end() == b.trim_end();
    // Started over: the whole answer again.
    if kept_lines.len() >= 2
        && rest.len() >= 2
        && !kept_lines[0].trim().is_empty()
        && same(kept_lines[0], rest[0])
        && same(kept_lines[1], rest[1])
    {
        return text.to_string();
    }
    // A fence opened again inside the open fence: a language tag, or a bare
    // fence that another fence closes later (a bare fence alone closes).
    let open = kept_lines.iter().filter(|line| is_fence(line)).count() % 2 == 1;
    if open {
        if let Some(first) = rest.first().copied().filter(|line| is_fence(line)) {
            let tagged = !first.trim().trim_start_matches('`').is_empty();
            if tagged || rest[1..].iter().any(|line| is_fence(line)) {
                rest.remove(0);
            }
        }
    }
    // Lines repeated from the end of the kept part.
    let most = kept_lines.len().min(rest.len()).min(MAX_OVERLAP_LINES);
    let overlap = (1..=most)
        .rev()
        .find(|&lines| {
            let tail = &kept_lines[kept_lines.len() - lines..];
            let head = &rest[..lines];
            tail.iter().zip(head).all(|(a, b)| same(a, b))
                && tail.iter().map(|line| line.trim().len()).sum::<usize>() >= MIN_OVERLAP_CHARS
        })
        .unwrap_or(0);
    let mut joined = kept.to_string();
    for line in &rest[overlap..] {
        joined.push_str(line);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_joins_as_is() {
        assert_eq!(
            join(ContinueMode::Prefix, "```ts\nconst a = fo", "o;\n```\n"),
            "```ts\nconst a = foo;\n```\n"
        );
    }

    #[test]
    fn turn_rewrites_the_cut_line_and_drops_repeats() {
        let partial = "```ts\nexport function add(a: number, b: number) {\n  return a + b;\n}\nexport const x = fo";
        // The model repeats the last complete line, then writes the rest.
        let text = "}\nexport const x = foo();\n```\n";
        // `}` alone is too short to count as a repeat: kept.
        assert_eq!(
            join(ContinueMode::Turn, partial, text),
            "```ts\nexport function add(a: number, b: number) {\n  return a + b;\n}\n}\nexport const x = foo();\n```\n"
        );
        let text = "export function add(a: number, b: number) {\n  return a + b;\n}\nexport const x = foo();\n```\n";
        assert_eq!(
            join(ContinueMode::Turn, partial, text),
            "```ts\nexport function add(a: number, b: number) {\n  return a + b;\n}\nexport const x = foo();\n```\n"
        );
        // Two short lines are not enough to call it a repeat.
        let text = "  return a + b;\n}\nexport const x = foo();\n```\n";
        assert!(join(ContinueMode::Turn, partial, text).contains("}\n  return a + b;\n}\n"));
    }

    #[test]
    fn turn_keeps_lines_that_code_repeats_on_its_own() {
        // Closing tags that the continuation legitimately starts with.
        let partial = "```tsx\n<div><div>\n<div><div>\n<p>x</p>\n</div>\n</div>\n</d";
        let text = "</div>\n</div>\n```\n";
        assert_eq!(
            join(ContinueMode::Turn, partial, text),
            "```tsx\n<div><div>\n<div><div>\n<p>x</p>\n</div>\n</div>\n</div>\n</div>\n```\n"
        );
        // Repeated assertion rows.
        let partial = "```ts\nexpect(x).toBe(1);\nexpect(x).toBe(1);\nexpe";
        let text = "expect(x).toBe(1);\nexpect(y).toBe(2);\n```\n";
        assert_eq!(
            join(ContinueMode::Turn, partial, text),
            "```ts\nexpect(x).toBe(1);\nexpect(x).toBe(1);\nexpect(x).toBe(1);\nexpect(y).toBe(2);\n```\n"
        );
    }

    #[test]
    fn turn_drops_a_reopened_fence() {
        let partial = "```tsx\nconst a = 1;\nconst b";
        assert_eq!(
            join(ContinueMode::Turn, partial, "```tsx\nconst b = 2;\n```\n"),
            "```tsx\nconst a = 1;\nconst b = 2;\n```\n"
        );
        // A bare fence alone closes the open fence: kept.
        assert_eq!(
            join(ContinueMode::Turn, "```tsx\nconst a = 1;\n", "```\n"),
            "```tsx\nconst a = 1;\n```\n"
        );
    }

    #[test]
    fn turn_takes_an_answer_that_starts_over() {
        let partial = "```ts\nconst a = 1;\nconst b = 2;\nconst c";
        let text = "```ts\nconst a = 1;\nconst b = 2;\nconst c = 3;\n```\n";
        assert_eq!(join(ContinueMode::Turn, partial, text), text);
    }

    #[test]
    fn prefix_endpoint_only_for_deepseek() {
        assert_eq!(
            prefix_endpoint("https://api.deepseek.com/v1").as_deref(),
            Some("https://api.deepseek.com/beta/chat/completions")
        );
        assert_eq!(
            prefix_endpoint("https://api.deepseek.com").as_deref(),
            Some("https://api.deepseek.com/beta/chat/completions")
        );
        assert_eq!(prefix_endpoint("http://127.0.0.1:8080/v1"), None);
        assert_eq!(prefix_endpoint("api.deepseek.com"), None);
        assert_eq!(prefix_message("ab")["prefix"], true);
    }

    #[test]
    fn turn_request_keeps_the_prefix() {
        let request = CompletionRequest {
            system: "s".into(),
            shared_context: Some("ctx".into()),
            user: "Title: a.ts\nwrite it".into(),
            max_tokens: 64,
            temperature: 0.2,
            reasoning_effort: None,
            prompt_cache_key: None,
            allow_empty: false,
            response_format: None,
        };
        let next = turn_request(&request, "```ts\nline one\nline t");
        assert!(next
            .user
            .starts_with("Title: a.ts\nwrite it\n\n# Continuation"));
        assert!(next
            .user
            .contains("<<<WRITTEN\n```ts\nline one\nWRITTEN>>>"));
        assert!(!next.user.contains("line t\n"));
        assert_eq!(next.system, request.system);
        assert_eq!(next.max_tokens, request.max_tokens);
    }
}
