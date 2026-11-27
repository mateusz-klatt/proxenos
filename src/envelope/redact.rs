//! Heuristic redaction of sign-in output and well-known secret formats in failure diagnostics.
//!
//! Redaction is a best-effort filter over failure messages, failure archives, diagnostics, and
//! session-save diagnostics, never a guarantee: unknown token formats pass through,
//! `reported.content` of successful turns is not filtered, files written on disk by provider CLIs
//! remain untouched, and widened context matching (such as device-code cues within a short
//! paragraph) can over-redact benign text such as `"error code EACCES"`.

use std::borrow::Cow;

use crate::providers::Report;

use super::{AUTHENTICATION_FAILURE_MESSAGE, Failure, FailureKind};

pub(super) const AUTHENTICATION_DIAGNOSTIC: &str = "The provider CLI is not signed in. Its sign-in output was omitted because it can contain one-time login URLs or codes.";
const REDACTED: &str = "[redacted]";
const DEVICE_CODE_CONTEXT_BYTES: usize = 160;

// Longest prefixes first so `sk-ant-` matches before `sk-`.
const SECRET_PREFIXES: &[&str] = &[
    "sk-ant-",
    "github_pat_",
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "xai-",
    "AIza",
];

pub(super) fn redact_report(report: &mut Report) -> bool {
    let mut redacted = report.diagnostic_redacted;
    if let Some(failure) = &mut report.failure {
        if failure.kind == FailureKind::Authentication {
            failure.message = AUTHENTICATION_FAILURE_MESSAGE.to_owned();
            report.diagnostic = Some(AUTHENTICATION_DIAGNOSTIC.to_owned());
            report.content = None;
            return true;
        }
        redacted |= filter_in_place(&mut failure.message);
        if let Some(reset_at) = &mut failure.reset_at {
            redacted |= filter_in_place(reset_at);
        }
    }
    if !report.diagnostic_redacted
        && let Some(diagnostic) = &mut report.diagnostic
    {
        redacted |= filter_in_place(diagnostic);
    }
    redacted
}

pub(super) fn redact_failure(failure: &mut Failure) -> bool {
    filter_in_place(&mut failure.message)
}

/// Redacts `input` before truncating to at most `max_bytes` on a UTF-8 char boundary.
/// Returns the bounded string and whether any replacement occurred within the kept prefix.
pub(crate) fn redact_excerpt(input: &str, max_bytes: usize) -> (String, bool) {
    match filter_text(input) {
        Some((mut filtered, first_redacted)) => {
            let kept = filtered.floor_char_boundary(max_bytes);
            filtered.truncate(kept);
            (filtered, first_redacted < kept)
        }
        None => {
            let kept = input.floor_char_boundary(max_bytes);
            (input[..kept].to_owned(), false)
        }
    }
}

/// Runs the heuristic redaction filter over `input` without truncation.
pub(crate) fn redact_text(input: &str) -> Cow<'_, str> {
    match filter_text(input) {
        Some((filtered, _)) => Cow::Owned(filtered),
        None => Cow::Borrowed(input),
    }
}

fn filter_in_place(text: &mut String) -> bool {
    let Some((filtered, _)) = filter_text(text) else {
        return false;
    };
    *text = filtered;
    true
}

fn filter_text(input: &str) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    let mut out: Option<(String, usize)> = None;
    let mut copied = 0;
    let mut url_reject_until = 0;
    let mut i = 0;
    while i < bytes.len() {
        let matched = match bytes[i] {
            b's' | b'g' | b'x' => match_prefixed_secret_at(bytes, i),
            b'A' => match_prefixed_secret_at(bytes, i).or_else(|| match_device_code_at(bytes, i)),
            b'b' => match_bearer_at(bytes, i),
            b'B' => match_bearer_at(bytes, i).or_else(|| match_device_code_at(bytes, i)),
            b'h' => match_auth_url_at(bytes, i, &mut url_reject_until),
            b'H' => match_auth_url_at(bytes, i, &mut url_reject_until)
                .or_else(|| match_device_code_at(bytes, i)),
            b'C'..=b'Z' | b'0'..=b'9' => match_device_code_at(bytes, i),
            _ => None,
        };
        if let Some(end) = matched {
            let (buf, _) = out.get_or_insert_with(|| (String::with_capacity(input.len()), i));
            buf.push_str(&input[copied..i]);
            buf.push_str(REDACTED);
            copied = end;
            i = end;
        } else {
            i += 1;
        }
    }
    out.map(|(mut buf, first_redacted)| {
        buf.push_str(&input[copied..]);
        (buf, first_redacted)
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
    let is_long_opaque = token.len() >= 16;
    (token.len() >= 4
        && token.iter().any(u8::is_ascii_alphanumeric)
        && (is_long_opaque || has_digit_or_punct || has_non_title_mixed_case || is_long_upper))
        .then_some(end)
}

fn is_url_byte(b: u8) -> bool {
    b > 0x20
        && b != 0x7f
        && !matches!(
            b,
            b'<' | b'>' | b'"' | b'\'' | b'`' | b'{' | b'}' | b'|' | b'\\' | b'^'
        )
}

fn has_auth_marker(path_and_query: &[u8]) -> bool {
    for (idx, &b) in path_and_query.iter().enumerate() {
        if b == b'?' {
            return true;
        }
        let rest = &path_and_query[idx..];
        let matched = match b.to_ascii_lowercase() {
            b'a' => starts_with_ignore_ascii_case(rest, b"auth"),
            b'l' => starts_with_ignore_ascii_case(rest, b"login"),
            b'o' => starts_with_ignore_ascii_case(rest, b"oauth"),
            b'd' => starts_with_ignore_ascii_case(rest, b"device"),
            b't' => starts_with_ignore_ascii_case(rest, b"token"),
            b'c' => starts_with_ignore_ascii_case(rest, b"code"),
            b's' => starts_with_ignore_ascii_case(rest, b"session"),
            b'v' => starts_with_ignore_ascii_case(rest, b"verify"),
            _ => false,
        };
        if matched {
            return true;
        }
    }
    false
}

fn match_auth_url_at(bytes: &[u8], i: usize, reject_until: &mut usize) -> Option<usize> {
    if i < *reject_until || !is_token_start(bytes, i) {
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
    if scheme_end >= bytes.len()
        || !is_url_byte(bytes[scheme_end])
        || matches!(bytes[scheme_end], b'/' | b'?' | b'#')
    {
        return None;
    }
    let mut end = scheme_end;
    let mut auth_start = None;
    let mut hash_pos = None;
    let mut parens = (0usize, 0usize);
    let mut brackets = (0usize, 0usize);
    while end < bytes.len() && is_url_byte(bytes[end]) {
        let b = bytes[end];
        if b == b'#' && hash_pos.is_none() {
            hash_pos = Some(end);
            if !auth_start.is_some_and(|start| has_auth_marker(&bytes[start..end])) {
                *reject_until = end;
                return None;
            }
        }
        if auth_start.is_none() && matches!(b, b'/' | b'?') {
            auth_start = Some(end);
        }
        match b {
            b'(' => parens.0 += 1,
            b')' => parens.1 += 1,
            b'[' => brackets.0 += 1,
            b']' => brackets.1 += 1,
            _ => {}
        }
        end += 1;
    }
    let raw_end = end;
    while end > scheme_end {
        match bytes[end - 1] {
            b'.' | b',' | b';' | b':' | b'!' | b'?' => end -= 1,
            b')' if parens.1 > parens.0 => {
                parens.1 -= 1;
                end -= 1;
            }
            b']' if brackets.1 > brackets.0 => {
                brackets.1 -= 1;
                end -= 1;
            }
            _ => break,
        }
    }
    let is_auth = hash_pos.is_some()
        || auth_start.is_some_and(|start| start < end && has_auth_marker(&bytes[start..end]));
    if is_auth {
        Some(end)
    } else {
        *reject_until = raw_end;
        None
    }
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

fn is_blank_line(line: &[u8]) -> bool {
    line.iter().all(|b| matches!(b, b' ' | b'\t' | b'\r'))
}

fn contains_code_or_enter_word(window: &[u8]) -> bool {
    window
        .split(|b| !b.is_ascii_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case(b"code") || word.eq_ignore_ascii_case(b"enter"))
}

fn has_keyword_before_blank<'a>(mut lines: impl Iterator<Item = &'a [u8]>) -> bool {
    if lines.next().is_some_and(contains_code_or_enter_word) {
        return true;
    }
    for line in lines {
        if is_blank_line(line) {
            return false;
        }
        if contains_code_or_enter_word(line) {
            return true;
        }
    }
    false
}

fn has_adjacent_keyword(bytes: &[u8], start: usize, end: usize) -> bool {
    let mut before_min = start.saturating_sub(DEVICE_CODE_CONTEXT_BYTES);
    if before_min > 0 && bytes[before_min - 1].is_ascii_alphanumeric() {
        while before_min < start && bytes[before_min].is_ascii_alphanumeric() {
            before_min += 1;
        }
    }
    let mut after_max = (end + DEVICE_CODE_CONTEXT_BYTES).min(bytes.len());
    if after_max < bytes.len() && bytes[after_max].is_ascii_alphanumeric() {
        while after_max > end && bytes[after_max - 1].is_ascii_alphanumeric() {
            after_max -= 1;
        }
    }
    has_keyword_before_blank(bytes[before_min..start].split(|&b| b == b'\n').rev())
        || has_keyword_before_blank(bytes[end..after_max].split(|&b| b == b'\n'))
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
