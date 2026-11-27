//! Heuristic redaction of sign-in output and well-known secret formats in failure diagnostics.
//!
//! Redaction is a best-effort filter over failure messages and diagnostics, never a guarantee:
//! unknown token formats pass through, `reported.content` is not filtered, and files written on
//! disk by provider CLIs remain untouched.

use crate::providers::Report;

use super::{AUTHENTICATION_FAILURE_MESSAGE, Failure, FailureKind};

pub(super) const AUTHENTICATION_DIAGNOSTIC: &str = "The provider CLI is not signed in. Its sign-in output was omitted because it can contain one-time login URLs or codes.";
const REDACTED: &str = "[redacted]";

const AUTH_URL_WORDS: &[&str] = &[
    "auth", "login", "oauth", "device", "token", "code", "session", "verify",
];

// Longest prefixes first so `sk-ant-` matches before `sk-`.
const SECRET_PREFIXES: &[&str] = &[
    "sk-ant-",
    "github_pat_",
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "xai-",
    "AIza",
];

pub(super) fn redact_report(report: &mut Report) -> bool {
    let mut redacted = false;
    if let Some(failure) = &mut report.failure {
        if failure.kind == FailureKind::Authentication {
            failure.message = AUTHENTICATION_FAILURE_MESSAGE.to_owned();
            report.diagnostic = Some(AUTHENTICATION_DIAGNOSTIC.to_owned());
            return true;
        }
        redacted |= filter_in_place(&mut failure.message);
        if let Some(reset_at) = &mut failure.reset_at {
            redacted |= filter_in_place(reset_at);
        }
    }
    if let Some(diagnostic) = &mut report.diagnostic {
        redacted |= filter_in_place(diagnostic);
    }
    redacted
}

pub(super) fn redact_failure(failure: &mut Failure) -> bool {
    filter_in_place(&mut failure.message)
}

fn filter_in_place(text: &mut String) -> bool {
    let Some(filtered) = filter_text(text) else {
        return false;
    };
    *text = filtered;
    true
}

fn filter_text(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = match_secret_at(bytes, i)
            .or_else(|| match_auth_url_at(bytes, i))
            .or_else(|| match_device_code_at(bytes, i))
        {
            let buf = out.get_or_insert_with(|| String::with_capacity(input.len()));
            buf.push_str(&input[copied..i]);
            buf.push_str(REDACTED);
            copied = end;
            i = end;
        } else {
            i += 1;
        }
    }
    out.map(|mut buf| {
        buf.push_str(&input[copied..]);
        buf
    })
}

fn is_token_start(bytes: &[u8], i: usize) -> bool {
    i == 0 || !bytes[i - 1].is_ascii_alphanumeric()
}

fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .get(..needle.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle))
}

fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn match_secret_at(bytes: &[u8], i: usize) -> Option<usize> {
    match_prefixed_secret_at(bytes, i).or_else(|| match_bearer_at(bytes, i))
}

fn match_prefixed_secret_at(bytes: &[u8], i: usize) -> Option<usize> {
    if !is_token_start(bytes, i) {
        return None;
    }
    let rest = &bytes[i..];
    let prefix = SECRET_PREFIXES
        .iter()
        .find(|prefix| rest.starts_with(prefix.as_bytes()))?;
    let mut end = i + prefix.len();
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'-' | b'_'))
    {
        end += 1;
    }
    (end > i + prefix.len()).then_some(end)
}

fn match_bearer_at(bytes: &[u8], i: usize) -> Option<usize> {
    if !is_token_start(bytes, i) || !starts_with_ignore_ascii_case(&bytes[i..], b"bearer") {
        return None;
    }
    let mut ws = i + 6;
    while ws < bytes.len() && matches!(bytes[ws], b' ' | b'\t') {
        ws += 1;
    }
    if ws == i + 6 {
        return None;
    }
    let mut end = ws;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric()
            || matches!(bytes[end], b'-' | b'_' | b'.' | b'+' | b'/' | b'~'))
    {
        end += 1;
    }
    while end < bytes.len() && bytes[end] == b'=' {
        end += 1;
    }
    // Avoid matching `Bearer realm="example"` key=value challenges.
    if end > ws
        && bytes[end - 1] == b'='
        && bytes
            .get(end)
            .is_some_and(|&b| matches!(b, b'"' | b'\'') || b.is_ascii_alphanumeric())
    {
        return None;
    }
    while end > ws && bytes[end - 1] == b'.' {
        end -= 1;
    }
    let token = &bytes[ws..end];
    let has_digit_or_punct = token.iter().any(|&b| {
        b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'/' | b'=' | b'~')
    });
    let has_non_title_mixed_case = token.len() >= 4
        && token[1..].iter().any(u8::is_ascii_uppercase)
        && token.iter().any(u8::is_ascii_lowercase);
    let is_long_upper = token.len() >= 6 && token.iter().all(u8::is_ascii_uppercase);
    (token.len() >= 4
        && token.iter().any(u8::is_ascii_alphanumeric)
        && (has_digit_or_punct || has_non_title_mixed_case || is_long_upper))
        .then_some(end)
}

fn is_url_byte(b: u8) -> bool {
    b > 0x20
        && b < 0x7f
        && !matches!(
            b,
            b'<' | b'>' | b'"' | b'\'' | b'`' | b'{' | b'}' | b'|' | b'\\' | b'^'
        )
}

fn unbalanced(slice: &[u8], open: u8, close: u8) -> bool {
    let opens = slice.iter().filter(|&&b| b == open).count();
    let closes = slice.iter().filter(|&&b| b == close).count();
    closes > opens
}

fn is_trailing_url_punct(body: &[u8], last: u8) -> bool {
    matches!(last, b'.' | b',' | b';' | b':' | b'!' | b'?')
        || (last == b')' && unbalanced(body, b'(', b')'))
        || (last == b']' && unbalanced(body, b'[', b']'))
}

fn match_auth_url_at(bytes: &[u8], i: usize) -> Option<usize> {
    if !is_token_start(bytes, i) {
        return None;
    }
    let rest = &bytes[i..];
    let scheme_len = if starts_with_ignore_ascii_case(rest, b"https://") {
        8
    } else if starts_with_ignore_ascii_case(rest, b"http://") {
        7
    } else {
        return None;
    };
    let scheme_end = i + scheme_len;
    let mut end = scheme_end;
    while end < bytes.len() && is_url_byte(bytes[end]) {
        end += 1;
    }
    while end > scheme_end && is_trailing_url_punct(&bytes[scheme_end..end], bytes[end - 1]) {
        end -= 1;
    }
    let body = &bytes[scheme_end..end];
    let auth_end = body
        .iter()
        .position(|&b| matches!(b, b'/' | b'?' | b'#'))
        .unwrap_or(body.len());
    if auth_end == 0 {
        return None;
    }
    let path_and_query = body[auth_end..].split(|&b| b == b'#').next().unwrap_or(&[]);
    let is_auth_url = path_and_query.contains(&b'?')
        || AUTH_URL_WORDS
            .iter()
            .any(|word| contains_ignore_ascii_case(path_and_query, word.as_bytes()));
    is_auth_url.then_some(end)
}

fn is_code_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')
}

fn is_upper_alnum(b: &u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

fn is_device_code_shape(tok: &[u8]) -> bool {
    let is_four_four = tok.len() == 9
        && tok[4] == b'-'
        && tok[..4].iter().all(is_upper_alnum)
        && tok[5..].iter().all(is_upper_alnum);
    let is_six_to_nine = (6..=9).contains(&tok.len()) && tok.iter().all(is_upper_alnum);
    is_four_four || is_six_to_nine
}

fn is_sentence_break(bytes: &[u8], idx: usize) -> bool {
    matches!(bytes[idx], b';' | b'!' | b'?')
        || (bytes[idx] == b'.' && bytes.get(idx + 1).is_none_or(u8::is_ascii_whitespace))
}

fn contains_code_or_enter_word(window: &[u8]) -> bool {
    window
        .split(|b| !b.is_ascii_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case(b"code") || word.eq_ignore_ascii_case(b"enter"))
}

fn has_adjacent_keyword(bytes: &[u8], start: usize, end: usize) -> bool {
    let before_min = start.saturating_sub(36);
    let mut before_start = start;
    while before_start > before_min && !is_sentence_break(bytes, before_start - 1) {
        before_start -= 1;
    }
    let after_max = (end + 28).min(bytes.len());
    let mut after_end = end;
    while after_end < after_max && !is_sentence_break(bytes, after_end) {
        after_end += 1;
    }
    contains_code_or_enter_word(&bytes[before_start..start])
        || contains_code_or_enter_word(&bytes[end..after_end])
}

fn match_device_code_at(bytes: &[u8], i: usize) -> Option<usize> {
    if i > 0 && is_code_word_char(bytes[i - 1]) {
        return None;
    }
    let mut end = i;
    while end < bytes.len() && is_code_word_char(bytes[end]) {
        end += 1;
    }
    let tok = &bytes[i..end];
    if !is_device_code_shape(tok) || !has_adjacent_keyword(bytes, i, end) {
        return None;
    }
    Some(end)
}
