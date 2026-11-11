//! JSON salvage for schema-off providers.
//!
//! Providers that reject or ignore `response_format` (deepseek-flash rejects
//! strict json_schema; see `ProviderPreset::architect_schema`) answer
//! structured requests as free text, so a fraction of outputs fail JSON
//! parsing. The legacy fallback regenerates the WHOLE request at the next
//! ladder step. Salvage instead sends ONE small call carrying only the failed
//! output and asking for syntax repair; every salvaged candidate still passes
//! the caller's own parse (and validate) gate, so accepted output is
//! unchanged by construction.

use crate::client::{CompletionRequest, ModelClient};

/// Extract the JSON-object slice: first `{` through last `}`.
pub(crate) fn extract_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then(|| &text[start..=end])
}

/// Drop commas that directly precede a closing `}` or `]`, string-aware so a
/// comma inside a string value survives. Trailing commas are the most common
/// malformation free-text models add to JSON.
pub(crate) fn strip_trailing_commas(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let mut chars = json.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            ',' => {
                let closes = chars
                    .clone()
                    .find(|c: &char| !c.is_whitespace())
                    .is_some_and(|c| c == '}' || c == ']');
                if !closes {
                    out.push(ch);
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Deterministic repair passes over an extracted JSON-object slice, most
/// conservative first: literal control characters inside strings become
/// spaces, then trailing commas drop, then Python-style single quotes flip
/// to double. Returns the first candidate that parses as any JSON value.
pub(crate) fn repair(extract: &str) -> Option<String> {
    let flattened = extract.replace(['\n', '\r', '\t'], " ");
    let de_commaed = strip_trailing_commas(&flattened);
    let quote_flipped = de_commaed.replace('\'', "\"");
    for candidate in [&flattened, &de_commaed, &quote_flipped] {
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.clone());
        }
    }
    None
}

/// One small call asking the model to repair a malformed JSON object and
/// return the repaired object only. `effort` must be a value the target
/// provider accepts ("minimal" on Muse, "none" on deepseek-flash); hosts
/// running other providers pass what their preset uses.
pub(crate) async fn salvage(
    client: &dyn ModelClient,
    raw: &str,
    max_tokens: u32,
    effort: &str,
    response_format: Option<serde_json::Value>,
) -> anyhow::Result<String> {
    let done = client
        .complete(CompletionRequest {
            system: "You repair malformed JSON. The user message contains one \
JSON object that failed to parse. Output only the repaired JSON object: keep \
every key and value exactly, fix syntax only (unescaped control characters, \
trailing commas, stray quotes). No Markdown fences, no commentary."
                .to_string(),
            shared_context: None,
            user: raw.to_string(),
            max_tokens,
            temperature: 0.0,
            reasoning_effort: Some(effort.to_string()),
            prompt_cache_key: None,
            allow_empty: false,
            response_format,
        })
        .await?;
    Ok(done.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_object_slices_first_brace_through_last() {
        assert_eq!(extract_object("pre {\"a\":1} post"), Some("{\"a\":1}"));
        assert_eq!(extract_object("no braces"), None);
        // Closing brace before opening: no object slice.
        assert_eq!(extract_object("} only closing {"), None);
    }

    #[test]
    fn strip_trailing_commas_drops_comma_before_closer_only() {
        assert_eq!(strip_trailing_commas(r#"{"a":1,}"#), r#"{"a":1}"#);
        assert_eq!(
            strip_trailing_commas(r#"{"a":[1,2,],"b":{"c":2,},}"#),
            r#"{"a":[1,2],"b":{"c":2}}"#
        );
        // Comma inside a string value (and an escaped quote) survive.
        assert_eq!(
            strip_trailing_commas(r##"{"a":"x,}","b":"q\"}",}"##),
            r##"{"a":"x,}","b":"q\"}"}"##
        );
        // Interior commas survive.
        assert_eq!(
            strip_trailing_commas(r#"{"a":1,"b":2}"#),
            r#"{"a":1,"b":2}"#
        );
    }

    #[test]
    fn repair_passes_run_most_conservative_first() {
        // Trailing comma, a class the old passes missed.
        let fixed = repair(r#"{"a":[1,2,],"b":true}"#).unwrap();
        assert_eq!(fixed, r#"{"a":[1,2],"b":true}"#);
        // Literal newline inside a string.
        let newline = "{\"a\":\"line one\nline two\"}";
        assert!(repair(newline).is_some());
        // Python-style single quotes.
        assert!(repair("{'a': 1}").is_some());
        // Unsalvageable junk stays None.
        assert!(repair("not json at all").is_none());
    }
}
