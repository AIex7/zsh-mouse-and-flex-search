use pyo3::exceptions::{PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

mod syntax_highlighting;

const WORD_BOUNDARIES: &str = " _-/.:";
const MAX_DAEMON_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

fn is_word_boundary_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'_' | b'-' | b'/' | b'.' | b':')
}

fn compact_query(query_lower: &str) -> Vec<char> {
    query_lower
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn ascii_query(query: &[char]) -> Option<Vec<u8>> {
    query
        .iter()
        .map(|character| u8::try_from(*character as u32).ok())
        .collect()
}

fn match_flex_ascii(query: &[u8], candidate: &[u8], candidate_lower: &[u8]) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    if query.len() == 1 {
        let position = candidate_lower
            .iter()
            .position(|character| *character == query[0])?;
        let boundary_bonus = if position == 0 {
            12
        } else if is_word_boundary_byte(candidate[position - 1]) {
            8
        } else {
            0
        };
        return Some(
            boundary_bonus + (30_i64 - position as i64).max(0) + 20 - (candidate.len() / 8) as i64,
        );
    }

    let mut query_index = 0;
    let mut first = 0;
    let mut previous = 0;
    let mut contiguous = 0_i64;
    let mut gap_penalty = 0_i64;
    let mut boundary_bonus = 0_i64;

    for (position, candidate_character) in candidate_lower.iter().copied().enumerate() {
        if candidate_character != query[query_index] {
            continue;
        }
        if query_index == 0 {
            first = position;
            if position == 0 {
                boundary_bonus += 12;
            } else if is_word_boundary_byte(candidate[position - 1]) {
                boundary_bonus += 8;
            }
        } else {
            let gap = position - previous - 1;
            gap_penalty += (gap * 2) as i64;
            if gap == 0 {
                contiguous += 10;
            }
            if is_word_boundary_byte(candidate[position - 1]) {
                boundary_bonus += 6;
            }
        }

        previous = position;
        query_index += 1;
        if query_index == query.len() {
            let span = previous - first + 1;
            let start_bonus = (30_i64 - first as i64).max(0);
            let compact_bonus = (20_i64 - (span - query.len()) as i64).max(0);
            return Some(
                contiguous + boundary_bonus + start_bonus + compact_bonus
                    - gap_penalty
                    - (candidate.len() / 8) as i64,
            );
        }
    }
    None
}

/// Return the existing Python flex match score without retaining match positions.
fn match_flex(query: &[char], candidate: &str, candidate_lower: &str) -> Option<i64> {
    if candidate.is_ascii() && candidate_lower.is_ascii() {
        if let Some(query_ascii) = ascii_query(query) {
            return match_flex_ascii(
                &query_ascii,
                candidate.as_bytes(),
                candidate_lower.as_bytes(),
            );
        }
    }
    if query.is_empty() {
        return Some(0);
    }

    let boundary_characters: Vec<bool> = candidate
        .chars()
        .map(|character| WORD_BOUNDARIES.contains(character))
        .collect();
    let character_count = boundary_characters.len();
    let mut query_index = 0;
    let mut first = 0;
    let mut previous = 0;
    let mut contiguous = 0_i64;
    let mut gap_penalty = 0_i64;
    let mut boundary_bonus = 0_i64;

    for (position, candidate_character) in candidate_lower.chars().enumerate() {
        if candidate_character != query[query_index] {
            continue;
        }
        if query_index == 0 {
            first = position;
            if position == 0 {
                boundary_bonus += 12;
            } else if boundary_characters
                .get(position - 1)
                .copied()
                .unwrap_or(false)
            {
                boundary_bonus += 8;
            }
        } else {
            let gap = position - previous - 1;
            gap_penalty += (gap * 2) as i64;
            if gap == 0 {
                contiguous += 10;
            }
            if boundary_characters
                .get(position - 1)
                .copied()
                .unwrap_or(false)
            {
                boundary_bonus += 6;
            }
        }

        previous = position;
        query_index += 1;
        if query_index == query.len() {
            let span = previous - first + 1;
            let start_bonus = (30_i64 - first as i64).max(0);
            let compact_bonus = (20_i64 - (span - query.len()) as i64).max(0);
            return Some(
                contiguous + boundary_bonus + start_bonus + compact_bonus
                    - gap_penalty
                    - (character_count / 8) as i64,
            );
        }
    }
    None
}

struct NativeCandidate {
    text: String,
    text_lower: String,
    normalized_text: String,
    cwd: Option<String>,
    words: Vec<String>,
    failed: bool,
    ascii: bool,
    boundary_characters: Vec<bool>,
    character_count: usize,
}

impl NativeCandidate {
    fn new(
        text: String,
        text_lower: String,
        normalized_text: String,
        cwd: Option<String>,
        words: Vec<String>,
        failed: bool,
    ) -> Self {
        let ascii = text.is_ascii() && text_lower.is_ascii();
        let boundary_characters = if ascii {
            Vec::new()
        } else {
            text.chars()
                .map(|character| WORD_BOUNDARIES.contains(character))
                .collect()
        };
        let character_count = if ascii {
            text.len()
        } else {
            text.chars().count()
        };
        Self {
            text,
            text_lower,
            normalized_text,
            cwd,
            words,
            failed,
            ascii,
            boundary_characters,
            character_count,
        }
    }

    fn match_flex_score(&self, query: &[char], query_ascii: Option<&[u8]>) -> Option<i64> {
        if self.ascii {
            if let Some(query_ascii) = query_ascii {
                return match_flex_ascii(
                    query_ascii,
                    self.text.as_bytes(),
                    self.text_lower.as_bytes(),
                );
            }
        }
        if query.is_empty() {
            return Some(0);
        }
        if query.len() == 1 {
            let position = self
                .text_lower
                .chars()
                .position(|character| character == query[0])?;
            let boundary_bonus = if position == 0 {
                12
            } else if self
                .boundary_characters
                .get(position - 1)
                .copied()
                .unwrap_or(false)
            {
                8
            } else {
                0
            };
            return Some(
                boundary_bonus + (30_i64 - position as i64).max(0) + 20
                    - (self.character_count / 8) as i64,
            );
        }

        let mut query_index = 0;
        let mut first = 0;
        let mut previous = 0;
        let mut contiguous = 0_i64;
        let mut gap_penalty = 0_i64;
        let mut boundary_bonus = 0_i64;

        for (position, candidate_character) in self.text_lower.chars().enumerate() {
            if candidate_character != query[query_index] {
                continue;
            }

            if query_index == 0 {
                first = position;
                if position == 0 {
                    boundary_bonus += 12;
                } else if self
                    .boundary_characters
                    .get(position - 1)
                    .copied()
                    .unwrap_or(false)
                {
                    boundary_bonus += 8;
                }
            } else {
                let gap = position - previous - 1;
                gap_penalty += (gap * 2) as i64;
                if gap == 0 {
                    contiguous += 10;
                }
                if self
                    .boundary_characters
                    .get(position - 1)
                    .copied()
                    .unwrap_or(false)
                {
                    boundary_bonus += 6;
                }
            }

            previous = position;
            query_index += 1;
            if query_index == query.len() {
                let span = previous - first + 1;
                let start_bonus = (30_i64 - first as i64).max(0);
                let compact_bonus = (20_i64 - (span - query.len()) as i64).max(0);
                return Some(
                    contiguous + boundary_bonus + start_bonus + compact_bonus
                        - gap_penalty
                        - (self.character_count / 8) as i64,
                );
            }
        }
        None
    }
}

/// History text stored and scanned entirely in Rust.
///
/// Python constructs this once when the daemon loads or refreshes history.
/// Subsequent searches avoid extracting every Python tuple again.
#[pyclass]
struct NativeHistory {
    candidates: Vec<NativeCandidate>,
}

type CandidateInput = (String, String, String, Option<String>, Vec<String>, bool);

struct RankedMatch {
    index: usize,
    score: i64,
    scan_order: usize,
}

#[derive(Serialize)]
struct HistoryResultPayload<'a> {
    text: &'a str,
    score: i64,
    exact: bool,
    recency: i64,
    cwd: Option<&'a str>,
    failed: bool,
    words: &'a [String],
}

#[derive(Serialize)]
struct SearchResponsePayload<'a> {
    ok: bool,
    history_results: Vec<HistoryResultPayload<'a>>,
    matched_indices: Option<&'a [usize]>,
    matched_indices_omitted: bool,
    matched_count: usize,
}

#[derive(Serialize)]
struct SearchRequestPayload<'a> {
    action: &'static str,
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_indices: Option<&'a [usize]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
}

#[derive(Deserialize)]
struct ParsedHistoryResult {
    text: String,
    score: i64,
    #[serde(default)]
    exact: bool,
    #[serde(default)]
    recency: i64,
    cwd: Option<String>,
    #[serde(default)]
    failed: bool,
    #[serde(default)]
    words: Vec<String>,
}

#[derive(Deserialize)]
struct ParsedSearchResponse {
    #[serde(default)]
    ok: bool,
    history_results: Vec<ParsedHistoryResult>,
    matched_indices: Option<Vec<usize>>,
    matched_count: Option<usize>,
}

type ParsedResult = (String, i64, bool, i64, Option<String>, bool, Vec<String>);
type ParsedResponse = (Vec<ParsedResult>, Option<Vec<usize>>, usize);

fn parse_search_response_bytes(raw: &[u8]) -> Option<ParsedResponse> {
    let response: ParsedSearchResponse = serde_json::from_slice(raw).ok()?;
    if !response.ok {
        return None;
    }
    let matched_count = response
        .matched_count
        .unwrap_or_else(|| response.matched_indices.as_ref().map_or(0, Vec::len));
    let results = response
        .history_results
        .into_iter()
        .map(|result| {
            (
                result.text,
                result.score,
                result.exact,
                result.recency,
                result.cwd,
                result.failed,
                result.words,
            )
        })
        .collect();
    Some((results, response.matched_indices, matched_count))
}

fn serialize_search_request_bytes(
    query: &str,
    candidate_indices: Option<&[usize]>,
    limit: Option<i64>,
    cwd: Option<&str>,
) -> Result<Vec<u8>, serde_json::Error> {
    let payload = SearchRequestPayload {
        action: "search_history",
        query,
        candidate_indices,
        limit,
        cwd,
    };
    let mut serialized = serde_json::to_vec(&payload)?;
    serialized.push(b'\n');
    Ok(serialized)
}

fn daemon_exchange_bytes(
    socket_path: &str,
    request: &[u8],
    timeout: Duration,
) -> Option<Vec<u8>> {
    let mut stream = UnixStream::connect(socket_path).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    stream.write_all(request).ok()?;

    let mut response = Vec::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            if response.len() + newline > MAX_DAEMON_MESSAGE_BYTES {
                return None;
            }
            response.extend_from_slice(&chunk[..newline]);
            break;
        }
        response.extend_from_slice(chunk);
        if response.len() > MAX_DAEMON_MESSAGE_BYTES {
            return None;
        }
    }

    let start = response
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())?;
    let end = response
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())?
        + 1;
    if start > 0 {
        response.drain(..start);
    }
    response.truncate(end - start);
    Some(response)
}

fn read_daemon_message(stream: &mut UnixStream) -> Option<Vec<u8>> {
    let mut message = Vec::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            if message.len() + newline > MAX_DAEMON_MESSAGE_BYTES {
                return None;
            }
            message.extend_from_slice(&chunk[..newline]);
            break;
        }
        message.extend_from_slice(chunk);
        if message.len() > MAX_DAEMON_MESSAGE_BYTES {
            return None;
        }
    }
    let start = message
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())?;
    let end = message
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())?
        + 1;
    Some(message[start..end].to_vec())
}

fn write_daemon_message(stream: &mut UnixStream, payload: &[u8]) -> bool {
    stream.write_all(payload).is_ok() && stream.write_all(b"\n").is_ok()
}

fn json_value_as_python_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "None".to_owned(),
        serde_json::Value::Bool(true) => "True".to_owned(),
        serde_json::Value::Bool(false) => "False".to_owned(),
        serde_json::Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

struct AcceptedSearchRequest {
    stream: UnixStream,
    query: String,
    candidate_indices: Option<Vec<usize>>,
    limit: Option<i64>,
    cwd: Option<String>,
}

#[pyclass]
struct NativeSearchRequest {
    stream: Option<UnixStream>,
    query: String,
    candidate_indices: Option<Vec<usize>>,
    limit: Option<i64>,
    cwd: Option<String>,
}

#[pymethods]
impl NativeSearchRequest {
    #[getter]
    fn query(&self) -> &str {
        &self.query
    }

    #[getter]
    fn candidate_indices(&self) -> Option<Vec<usize>> {
        self.candidate_indices.clone()
    }

    #[getter]
    fn limit(&self) -> Option<i64> {
        self.limit
    }

    #[getter]
    fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    fn respond_serialized(&mut self, payload: &str) -> bool {
        self.stream
            .take()
            .is_some_and(|mut stream| write_daemon_message(&mut stream, payload.as_bytes()))
    }
}

#[pyclass]
struct NativeDaemonServer {
    listener: UnixListener,
}

impl NativeDaemonServer {
    fn accept_search_request(&self) -> std::io::Result<AcceptedSearchRequest> {
        loop {
            let (mut stream, _) = self.listener.accept()?;
            let Some(raw) = read_daemon_message(&mut stream) else {
                write_daemon_message(
                    &mut stream,
                    br#"{"ok":false,"error":"invalid request"}"#,
                );
                continue;
            };
            let Ok(serde_json::Value::Object(request)) = serde_json::from_slice(&raw) else {
                write_daemon_message(
                    &mut stream,
                    br#"{"ok":false,"error":"invalid request"}"#,
                );
                continue;
            };
            let action = request.get("action").and_then(serde_json::Value::as_str);
            if action == Some("ping") {
                write_daemon_message(&mut stream, br#"{"ok":true}"#);
                continue;
            }
            if action != Some("search_history") {
                write_daemon_message(
                    &mut stream,
                    br#"{"ok":false,"error":"unknown action"}"#,
                );
                continue;
            }

            let query = request
                .get("query")
                .map_or_else(String::new, json_value_as_python_string);
            let candidate_indices = request.get("candidate_indices").and_then(|value| {
                value.as_array().map(|values| {
                    values
                        .iter()
                        .filter_map(|value| match value {
                            serde_json::Value::Bool(value) => Some(usize::from(*value)),
                            _ => value
                                .as_i64()
                                .and_then(|index| usize::try_from(index).ok()),
                        })
                        .collect()
                })
            });
            let limit = request.get("limit").and_then(|value| match value {
                serde_json::Value::Bool(value) => Some(i64::from(*value)),
                _ => value.as_i64(),
            });
            let cwd = request
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            return Ok(AcceptedSearchRequest {
                stream,
                query,
                candidate_indices,
                limit,
                cwd,
            });
        }
    }
}

#[pymethods]
impl NativeDaemonServer {
    #[new]
    fn new(socket_path: &str) -> PyResult<Self> {
        let listener = UnixListener::bind(socket_path).map_err(PyOSError::new_err)?;
        if let Err(error) = fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600)) {
            drop(listener);
            let _ = fs::remove_file(socket_path);
            return Err(PyOSError::new_err(error));
        }
        Ok(Self { listener })
    }

    fn accept_search(&self, py: Python<'_>) -> PyResult<Py<NativeSearchRequest>> {
        let request = py
            .allow_threads(|| self.accept_search_request())
            .map_err(PyOSError::new_err)?;
        Py::new(
            py,
            NativeSearchRequest {
                stream: Some(request.stream),
                query: request.query,
                candidate_indices: request.candidate_indices,
                limit: request.limit,
                cwd: request.cwd,
            },
        )
    }
}

fn words_appear_in_order(words: &[String], text_lower: &str) -> bool {
    if words.is_empty() {
        return false;
    }
    let mut remaining = text_lower;
    for word in words {
        let Some(position) = remaining.find(word) else {
            return false;
        };
        remaining = &remaining[position + word.len()..];
    }
    true
}

impl NativeHistory {
    /// Select an empty-query result page without constructing one ranked item
    /// per history entry. Empty queries only rank by cwd and recency.
    fn search_empty_ranked(
        &self,
        candidate_indices: Option<&[usize]>,
        limit: Option<usize>,
        current_cwd: Option<&str>,
    ) -> (Vec<(usize, i64)>, Option<Vec<usize>>, usize) {
        let matched_count = candidate_indices.map_or(self.candidates.len(), |indices| {
            indices
                .iter()
                .filter(|&&index| index < self.candidates.len())
                .count()
        });
        let result_limit = limit.unwrap_or(usize::MAX);
        let capacity = result_limit.min(matched_count);
        let mut selected = Vec::with_capacity(capacity);
        let mut seen = HashSet::with_capacity(capacity);

        let mut collect_pass = |same_cwd_required: Option<bool>| -> bool {
            let mut collect_candidate = |index: usize| -> bool {
                let Some(candidate) = self.candidates.get(index) else {
                    return false;
                };
                if let Some(required) = same_cwd_required {
                    let same_cwd = candidate.cwd.as_deref() == current_cwd;
                    if same_cwd != required {
                        return false;
                    }
                }
                if selected.len() < result_limit && seen.insert(candidate.text.as_str()) {
                    selected.push((index, 0));
                }
                selected.len() >= result_limit
            };

            if let Some(indices) = candidate_indices {
                for &index in indices {
                    if collect_candidate(index) {
                        return true;
                    }
                }
            } else {
                for index in 0..self.candidates.len() {
                    if collect_candidate(index) {
                        return true;
                    }
                }
            }
            false
        };

        if current_cwd.is_some() {
            if !collect_pass(Some(true)) {
                collect_pass(Some(false));
            }
        } else {
            collect_pass(None);
        }
        (selected, None, matched_count)
    }
}

#[pymethods]
impl NativeHistory {
    #[new]
    fn new(candidates: Vec<CandidateInput>) -> Self {
        let candidates = candidates
            .into_iter()
            .map(|(text, text_lower, normalized_text, cwd, words, failed)| {
                NativeCandidate::new(text, text_lower, normalized_text, cwd, words, failed)
            })
            .collect();
        Self { candidates }
    }

    fn __len__(&self) -> usize {
        self.candidates.len()
    }

    /// Append a batch while initially loading a large history.
    fn extend(&mut self, candidates: Vec<CandidateInput>) {
        self.candidates.extend(candidates.into_iter().map(
            |(text, text_lower, normalized_text, cwd, words, failed)| {
                NativeCandidate::new(text, text_lower, normalized_text, cwd, words, failed)
            },
        ));
    }

    /// Prepend newly loaded SQLite rows and discard older rows they replace.
    ///
    /// Retained candidates keep their owned strings and precomputed metadata;
    /// only the small new prefix is constructed.
    fn prepend_replacing(&mut self, candidates: Vec<CandidateInput>) {
        if candidates.is_empty() {
            return;
        }
        {
            let replaced_pairs: HashSet<(&str, Option<&str>)> = candidates
                .iter()
                .map(|candidate| (candidate.0.as_str(), candidate.3.as_deref()))
                .collect();
            self.candidates.retain(|candidate| {
                !replaced_pairs.contains(&(candidate.text.as_str(), candidate.cwd.as_deref()))
            });
        }
        let additions = candidates.into_iter().map(
            |(text, text_lower, normalized_text, cwd, words, failed)| {
                NativeCandidate::new(text, text_lower, normalized_text, cwd, words, failed)
            },
        );
        self.candidates.splice(0..0, additions);
    }

    fn flex_match_many(
        &self,
        py: Python<'_>,
        query_lower: &str,
        candidate_indices: Option<Vec<usize>>,
    ) -> Vec<(usize, i64)> {
        let query = compact_query(query_lower);
        let query_ascii = ascii_query(&query);
        py.allow_threads(|| {
            let mut matches = Vec::new();
            if let Some(indices) = candidate_indices.as_deref() {
                for &index in indices {
                    let Some(candidate) = self.candidates.get(index) else {
                        continue;
                    };
                    if let Some(score) = candidate.match_flex_score(&query, query_ascii.as_deref())
                    {
                        matches.push((index, score));
                    }
                }
            } else {
                for (index, candidate) in self.candidates.iter().enumerate() {
                    if let Some(score) = candidate.match_flex_score(&query, query_ascii.as_deref())
                    {
                        matches.push((index, score));
                    }
                }
            }
            matches
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn search_ranked(
        &self,
        py: Python<'_>,
        query_lower: &str,
        normalized_query: &str,
        prefix_query_words: Vec<String>,
        ordered_query_words: Vec<String>,
        current_cwd: Option<String>,
        candidate_indices: Option<Vec<usize>>,
        limit: Option<usize>,
        max_returned_indices: usize,
    ) -> (Vec<(usize, i64)>, Option<Vec<usize>>, usize) {
        if query_lower.is_empty() {
            return py.allow_threads(|| {
                self.search_empty_ranked(
                    candidate_indices.as_deref(),
                    limit,
                    current_cwd.as_deref(),
                )
            });
        }

        let query = compact_query(query_lower);
        let query_ascii = ascii_query(&query);
        py.allow_threads(|| {
            let mut buckets_by_prefix: Vec<[Vec<RankedMatch>; 4]> = Vec::new();
            let mut matched_indices = Some(Vec::new());
            let mut matched_count = 0;
            let mut max_prefix_word_count = 0_usize;

            let mut check_candidate = |scan_order: usize, index: usize| {
                let Some(candidate) = self.candidates.get(index) else {
                    return;
                };
                let Some(score) = candidate.match_flex_score(&query, query_ascii.as_deref()) else {
                    return;
                };
                if !normalized_query.is_empty() && candidate.normalized_text == normalized_query {
                    return;
                }

                matched_count += 1;
                if let Some(indices) = matched_indices.as_mut() {
                    if indices.len() < max_returned_indices {
                        indices.push(index);
                    } else {
                        matched_indices = None;
                    }
                }

                let prefix_word_count = prefix_query_words
                    .iter()
                    .zip(&candidate.words)
                    .take_while(|(query_word, candidate_word)| {
                        candidate_word.starts_with(query_word.as_str())
                    })
                    .count();
                max_prefix_word_count = max_prefix_word_count.max(prefix_word_count);
                let words_in_order =
                    words_appear_in_order(&ordered_query_words, &candidate.text_lower);
                let same_cwd = current_cwd
                    .as_deref()
                    .is_some_and(|cwd| candidate.cwd.as_deref() == Some(cwd));
                let inner_bucket = match (words_in_order, same_cwd) {
                    (true, true) => 0,
                    (true, false) => 1,
                    (false, true) => 2,
                    (false, false) => 3,
                };
                if buckets_by_prefix.len() <= prefix_word_count {
                    buckets_by_prefix.resize_with(prefix_word_count + 1, || {
                        std::array::from_fn(|_| Vec::new())
                    });
                }
                buckets_by_prefix[prefix_word_count][inner_bucket].push(RankedMatch {
                    index,
                    score,
                    scan_order,
                });
            };

            if let Some(indices) = candidate_indices.as_deref() {
                for (scan_order, &index) in indices.iter().enumerate() {
                    check_candidate(scan_order, index);
                }
            } else {
                for index in 0..self.candidates.len() {
                    check_candidate(index, index);
                }
            }

            let result_limit = limit.unwrap_or(usize::MAX);
            let mut selected = Vec::with_capacity(result_limit.min(matched_count));
            let mut seen = HashSet::new();
            let mut select_match = |matched: &RankedMatch| -> bool {
                let text = self.candidates[matched.index].text.as_str();
                if selected.len() < result_limit && seen.insert(text) {
                    selected.push((matched.index, matched.score));
                }
                selected.len() >= result_limit
            };

            let mut selection_complete = result_limit == 0;
            if !selection_complete {
                let prefix_buckets = buckets_by_prefix.get(max_prefix_word_count);
                if let Some(prefix_buckets) = prefix_buckets {
                    'preferred: for bucket in prefix_buckets {
                        for matched in bucket {
                            if select_match(matched) {
                                selection_complete = true;
                                break 'preferred;
                            }
                        }
                    }
                }
            }

            if !selection_complete && max_prefix_word_count > 0 {
                'remaining: for inner_bucket in 0..4 {
                    let mut heap = BinaryHeap::new();
                    for (prefix_count, prefix_buckets) in buckets_by_prefix
                        .iter()
                        .enumerate()
                        .take(max_prefix_word_count)
                    {
                        if let Some(matched) = prefix_buckets[inner_bucket].first() {
                            heap.push(Reverse((matched.scan_order, prefix_count, 0_usize)));
                        }
                    }

                    while let Some(Reverse((_, prefix_count, position))) = heap.pop() {
                        let bucket = &buckets_by_prefix[prefix_count][inner_bucket];
                        let matched = &bucket[position];
                        if select_match(matched) {
                            break 'remaining;
                        }
                        let next_position = position + 1;
                        if let Some(next) = bucket.get(next_position) {
                            heap.push(Reverse((
                                next.scan_order,
                                prefix_count,
                                next_position,
                            )));
                        }
                    }
                }
            }
            (selected, matched_indices, matched_count)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn search_response_json(
        &self,
        py: Python<'_>,
        query_lower: &str,
        normalized_query: &str,
        prefix_query_words: Vec<String>,
        ordered_query_words: Vec<String>,
        current_cwd: Option<String>,
        candidate_indices: Option<Vec<usize>>,
        limit: Option<usize>,
        max_returned_indices: usize,
    ) -> PyResult<String> {
        let (selected, matched_indices, matched_count) = self.search_ranked(
            py,
            query_lower,
            normalized_query,
            prefix_query_words,
            ordered_query_words,
            current_cwd,
            candidate_indices,
            limit,
            max_returned_indices,
        );
        let history_results = selected
            .iter()
            .map(|(index, score)| {
                let candidate = &self.candidates[*index];
                HistoryResultPayload {
                    text: &candidate.text,
                    score: *score,
                    exact: false,
                    recency: -(*index as i64),
                    cwd: candidate.cwd.as_deref(),
                    failed: candidate.failed,
                    words: &candidate.words,
                }
            })
            .collect();
        let response = SearchResponsePayload {
            ok: true,
            history_results,
            matched_indices: matched_indices.as_deref(),
            matched_indices_omitted: matched_indices.is_none(),
            matched_count,
        };
        serde_json::to_string(&response).map_err(|error| PyValueError::new_err(error.to_string()))
    }
}

#[pyfunction]
fn flex_match(query_lower: &str, candidate: &str, candidate_lower: &str) -> Option<i64> {
    match_flex(&compact_query(query_lower), candidate, candidate_lower)
}

#[pyfunction]
fn parse_search_response(raw: &[u8]) -> Option<ParsedResponse> {
    parse_search_response_bytes(raw)
}

#[pyfunction]
#[pyo3(signature = (query, candidate_indices=None, limit=None, cwd=None))]
fn serialize_search_request<'py>(
    py: Python<'py>,
    query: &str,
    candidate_indices: Option<Vec<usize>>,
    limit: Option<i64>,
    cwd: Option<&str>,
) -> PyResult<Bound<'py, PyBytes>> {
    let serialized = serialize_search_request_bytes(query, candidate_indices.as_deref(), limit, cwd)
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(PyBytes::new(py, &serialized))
}

#[pyfunction]
#[pyo3(signature = (socket_path, query, candidate_indices=None, limit=None, cwd=None, timeout_seconds=0.5))]
fn search_daemon(
    py: Python<'_>,
    socket_path: &str,
    query: &str,
    candidate_indices: Option<Vec<usize>>,
    limit: Option<i64>,
    cwd: Option<&str>,
    timeout_seconds: f64,
) -> (bool, Option<ParsedResponse>) {
    let Ok(timeout) = Duration::try_from_secs_f64(timeout_seconds) else {
        return (false, None);
    };
    let Ok(request) =
        serialize_search_request_bytes(query, candidate_indices.as_deref(), limit, cwd)
    else {
        return (false, None);
    };
    let response = py.allow_threads(|| daemon_exchange_bytes(socket_path, &request, timeout));
    let Some(response) = response else {
        return (false, None);
    };
    (true, parse_search_response_bytes(&response))
}

#[pymodule]
fn _flex_match(module: &Bound<'_, PyModule>) -> PyResult<()> {
    syntax_highlighting::register(module)?;
    module.add_class::<NativeHistory>()?;
    module.add_class::<NativeDaemonServer>()?;
    module.add_class::<NativeSearchRequest>()?;
    module.add_function(wrap_pyfunction!(flex_match, module)?)?;
    module.add_function(wrap_pyfunction!(parse_search_response, module)?)?;
    module.add_function(wrap_pyfunction!(serialize_search_request, module)?)?;
    module.add_function(wrap_pyfunction!(search_daemon, module)?)
}
