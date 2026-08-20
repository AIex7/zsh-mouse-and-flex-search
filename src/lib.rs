use pyo3::exceptions::{PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod syntax_highlighting;

const WORD_BOUNDARIES: &str = " _-/.:";
const MAX_DAEMON_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_DAEMON_QUERY_CACHE_ENTRIES: usize = 64;
const MAX_DAEMON_QUERY_CACHE_INDICES: usize = 1_000_000;

fn is_word_boundary_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'_' | b'-' | b'/' | b'.' | b':')
}

fn is_python_whitespace(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
}

fn python_trim(value: &str) -> &str {
    let start = value
        .char_indices()
        .find(|(_, character)| !is_python_whitespace(*character))
        .map_or(value.len(), |(index, _)| index);
    let end = value
        .char_indices()
        .rev()
        .find(|(_, character)| !is_python_whitespace(*character))
        .map_or(start, |(index, character)| index + character.len_utf8());
    &value[start..end]
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

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[inline(always)]
fn compute_char_mask_ascii(bytes: &[u8]) -> u128 {
    let mut mask = 0u128;
    for &b in bytes {
        mask |= 1u128 << (b & 0x7F);
    }
    mask
}

#[inline(always)]
fn compute_char_mask_str(text: &str) -> u128 {
    let mut mask = 0u128;
    for c in text.chars() {
        if c.is_ascii() {
            mask |= 1u128 << (c as u8 & 0x7F);
        } else {
            mask |= 1u128 << ((c as u32 % 128) as u8);
        }
    }
    mask
}

#[inline(always)]
fn compute_query_char_mask(query: &[char]) -> u128 {
    let mut mask = 0u128;
    for &c in query {
        if c.is_ascii() {
            mask |= 1u128 << (c as u8 & 0x7F);
        } else {
            mask |= 1u128 << ((c as u32 % 128) as u8);
        }
    }
    mask
}

#[inline(always)]
fn scan_char_masks(
    slice: &[u128],
    base_offset: usize,
    query_mask: u128,
    mut on_match: impl FnMut(usize),
) {
    if query_mask == 0 {
        for i in 0..slice.len() {
            on_match(base_offset + i);
        }
        return;
    }

    #[cfg(target_arch = "aarch64")]
    unsafe {
        let q_bytes = query_mask.to_ne_bytes();
        let q_vec = vld1q_u8(q_bytes.as_ptr());
        let chunks = slice.chunks_exact(4);
        let remainder = chunks.remainder();
        let mut base = base_offset;

        for chunk in chunks {
            let ptr = chunk.as_ptr();
            let m0 = vld1q_u8(ptr as *const u8);
            let m1 = vld1q_u8(ptr.add(1) as *const u8);
            let m2 = vld1q_u8(ptr.add(2) as *const u8);
            let m3 = vld1q_u8(ptr.add(3) as *const u8);

            let c0 = vceqq_u8(vandq_u8(m0, q_vec), q_vec);
            let c1 = vceqq_u8(vandq_u8(m1, q_vec), q_vec);
            let c2 = vceqq_u8(vandq_u8(m2, q_vec), q_vec);
            let c3 = vceqq_u8(vandq_u8(m3, q_vec), q_vec);

            if vminvq_u8(c0) == 0xFF {
                on_match(base);
            }
            if vminvq_u8(c1) == 0xFF {
                on_match(base + 1);
            }
            if vminvq_u8(c2) == 0xFF {
                on_match(base + 2);
            }
            if vminvq_u8(c3) == 0xFF {
                on_match(base + 3);
            }
            base += 4;
        }

        for &mask in remainder {
            let m = vld1q_u8((&mask as *const u128) as *const u8);
            let c = vceqq_u8(vandq_u8(m, q_vec), q_vec);
            if vminvq_u8(c) == 0xFF {
                on_match(base);
            }
            base += 1;
        }
    }

    #[cfg(all(target_arch = "x86_64", target_feature = "sse4.1"))]
    unsafe {
        let q_bytes = query_mask.to_ne_bytes();
        let q_vec = _mm_loadu_si128(q_bytes.as_ptr() as *const __m128i);
        let chunks = slice.chunks_exact(4);
        let remainder = chunks.remainder();
        let mut base = base_offset;

        for chunk in chunks {
            let ptr = chunk.as_ptr() as *const __m128i;
            let m0 = _mm_loadu_si128(ptr);
            let m1 = _mm_loadu_si128(ptr.add(1));
            let m2 = _mm_loadu_si128(ptr.add(2));
            let m3 = _mm_loadu_si128(ptr.add(3));

            if _mm_testc_si128(m0, q_vec) != 0 {
                on_match(base);
            }
            if _mm_testc_si128(m1, q_vec) != 0 {
                on_match(base + 1);
            }
            if _mm_testc_si128(m2, q_vec) != 0 {
                on_match(base + 2);
            }
            if _mm_testc_si128(m3, q_vec) != 0 {
                on_match(base + 3);
            }
            base += 4;
        }

        for &mask in remainder {
            let m = _mm_loadu_si128((&mask as *const u128) as *const __m128i);
            if _mm_testc_si128(m, q_vec) != 0 {
                on_match(base);
            }
            base += 1;
        }
    }

    #[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "sse4.1"))))]
    {
        for (i, &mask) in slice.iter().enumerate() {
            if (mask & query_mask) == query_mask {
                on_match(base_offset + i);
            }
        }
    }
}

struct NativeCandidate {
    text: String,
    text_lower: Option<Box<str>>,
    normalized_start: u32,
    normalized_end: u32,
    cwd: Option<Arc<str>>,
    words: Vec<String>,
    failed: bool,
    ascii: bool,
    boundary_characters: Vec<bool>,
    character_count: usize,
    char_mask: u128,
}

impl NativeCandidate {
    fn new(
        text: String,
        text_lower: String,
        cwd: Option<Arc<str>>,
        words: Vec<String>,
        failed: bool,
    ) -> Self {
        let text_lower = if text_lower == text {
            None
        } else {
            Some(text_lower.into_boxed_str())
        };
        let lower = text_lower.as_deref().unwrap_or(&text);
        let normalized = python_trim(lower);
        let normalized_start = u32::try_from(normalized.as_ptr() as usize - lower.as_ptr() as usize)
            .unwrap_or(u32::MAX);
        let normalized_end = u32::try_from(normalized_start as usize + normalized.len())
            .unwrap_or(u32::MAX);
        let ascii = text.is_ascii() && lower.is_ascii();
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
        let char_mask = if ascii {
            compute_char_mask_ascii(lower.as_bytes())
        } else {
            compute_char_mask_str(lower)
        };
        Self {
            text,
            text_lower,
            normalized_start,
            normalized_end,
            cwd,
            words,
            failed,
            ascii,
            boundary_characters,
            character_count,
            char_mask,
        }
    }

    fn text_lower(&self) -> &str {
        self.text_lower.as_deref().unwrap_or(&self.text)
    }

    fn normalized_text(&self) -> &str {
        self.text_lower()
            .get(self.normalized_start as usize..self.normalized_end as usize)
            .unwrap_or_else(|| python_trim(self.text_lower()))
    }

    fn match_flex_score(&self, query: &[char], query_ascii: Option<&[u8]>) -> Option<i64> {
        if self.ascii {
            if let Some(query_ascii) = query_ascii {
                return match_flex_ascii(
                    query_ascii,
                    self.text.as_bytes(),
                    self.text_lower().as_bytes(),
                );
            }
        }
        if query.is_empty() {
            return Some(0);
        }
        if query.len() == 1 {
            let position = self
                .text_lower()
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

        for (position, candidate_character) in self.text_lower().chars().enumerate() {
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
    char_masks: VecDeque<u128>,
    candidates: VecDeque<NativeCandidate>,
    cwd_interner: HashSet<Arc<str>>,
    daemon_query_cache: Mutex<DaemonQueryCache>,
}

#[derive(Default)]
struct DaemonQueryCache {
    entries: HashMap<String, Vec<usize>>,
    order: VecDeque<String>,
    total_indices: usize,
}

impl DaemonQueryCache {
    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.total_indices = 0;
    }

    fn candidates_for(&mut self, query: &str) -> Option<Vec<usize>> {
        let mut prefix_end = query.len();
        while prefix_end > 0 {
            prefix_end = query[..prefix_end]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index);
            if prefix_end == 0 {
                break;
            }
            let prefix = &query[..prefix_end];
            if let Some(indices) = self.entries.get(prefix) {
                let indices = indices.clone();
                if let Some(position) = self.order.iter().position(|key| key == prefix) {
                    self.order.remove(position);
                }
                self.order.push_back(prefix.to_owned());
                return Some(indices);
            }
        }
        None
    }

    fn insert(&mut self, query: &str, indices: Vec<usize>) {
        if query.is_empty() || indices.len() > MAX_DAEMON_QUERY_CACHE_INDICES {
            return;
        }
        if let Some(previous) = self.entries.remove(query) {
            self.total_indices -= previous.len();
            if let Some(position) = self.order.iter().position(|key| key == query) {
                self.order.remove(position);
            }
        }
        while !self.order.is_empty()
            && (self.order.len() >= MAX_DAEMON_QUERY_CACHE_ENTRIES
                || self.total_indices + indices.len() > MAX_DAEMON_QUERY_CACHE_INDICES)
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.total_indices -= removed.len();
            }
        }
        self.total_indices += indices.len();
        self.entries.insert(query.to_owned(), indices);
        self.order.push_back(query.to_owned());
    }
}

type CandidateInput = (String, String, Option<String>, Vec<String>, bool);

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

fn intern_cwd(cwd_interner: &mut HashSet<Arc<str>>, cwd: Option<String>) -> Option<Arc<str>> {
    let cwd = cwd?;
    if let Some(interned) = cwd_interner.get(cwd.as_str()) {
        return Some(Arc::clone(interned));
    }
    let interned: Arc<str> = Arc::from(cwd);
    cwd_interner.insert(Arc::clone(&interned));
    Some(interned)
}

fn candidate_from_input(
    input: CandidateInput,
    cwd_interner: &mut HashSet<Arc<str>>,
) -> NativeCandidate {
    let (text, text_lower, cwd, words, failed) = input;
    NativeCandidate::new(
        text,
        text_lower,
        intern_cwd(cwd_interner, cwd),
        words,
        failed,
    )
}

impl NativeHistory {
    fn prune_cwd_interner(&mut self) {
        self.cwd_interner
            .retain(|cwd| Arc::strong_count(cwd) > 1);
    }

    fn serialize_ranked_response(
        &self,
        selected: &[(usize, i64)],
        matched_indices: Option<&[usize]>,
        matched_count: usize,
    ) -> PyResult<String> {
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
            matched_indices,
            matched_indices_omitted: matched_indices.is_none(),
            matched_count,
        };
        serde_json::to_string(&response).map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Select an empty-query result page without constructing one ranked item
    /// per history entry. Empty queries only rank by cwd and recency.
    fn search_empty_ranked(
        &self,
        candidate_indices: Option<&[usize]>,
        limit: Option<usize>,
        current_cwd: Option<&Arc<str>>,
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
                    let same_cwd = current_cwd
                        .is_some_and(|cwd| candidate.cwd.as_ref().is_some_and(|candidate_cwd| {
                            Arc::ptr_eq(candidate_cwd, cwd)
                        }));
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
    fn new(candidates_input: Vec<CandidateInput>) -> Self {
        let mut cwd_interner = HashSet::new();
        let mut candidates = VecDeque::with_capacity(candidates_input.len());
        let mut char_masks = VecDeque::with_capacity(candidates_input.len());
        for input in candidates_input {
            let candidate = candidate_from_input(input, &mut cwd_interner);
            char_masks.push_back(candidate.char_mask);
            candidates.push_back(candidate);
        }
        Self {
            char_masks,
            candidates,
            cwd_interner,
            daemon_query_cache: Mutex::new(DaemonQueryCache::default()),
        }
    }

    fn __len__(&self) -> usize {
        self.candidates.len()
    }

    fn clear_daemon_query_cache(&self) {
        self.daemon_query_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    fn daemon_query_cache_stats(&self) -> (usize, usize, usize, usize) {
        let cache = self
            .daemon_query_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            cache.entries.len(),
            cache.total_indices,
            MAX_DAEMON_QUERY_CACHE_ENTRIES,
            MAX_DAEMON_QUERY_CACHE_INDICES,
        )
    }

    /// Report compact-storage details for regression tests and diagnostics.
    fn candidate_storage_stats(&self) -> (usize, usize, usize, usize, usize) {
        let lowercase_allocations = self
            .candidates
            .iter()
            .filter(|candidate| candidate.text_lower.is_some())
            .count();
        let lowercase_bytes = self
            .candidates
            .iter()
            .filter_map(|candidate| candidate.text_lower.as_deref())
            .map(str::len)
            .sum();
        let cwd_references = self
            .candidates
            .iter()
            .filter(|candidate| candidate.cwd.is_some())
            .count();
        (
            self.candidates.len(),
            lowercase_allocations,
            lowercase_bytes,
            self.cwd_interner.len(),
            cwd_references,
        )
    }

    /// Append a batch while initially loading a large history.
    fn extend(&mut self, candidates: Vec<CandidateInput>) {
        self.daemon_query_cache
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        for input in candidates {
            let candidate = candidate_from_input(input, &mut self.cwd_interner);
            self.char_masks.push_back(candidate.char_mask);
            self.candidates.push_back(candidate);
        }
    }

    fn truncate(&mut self, length: usize) {
        if self.candidates.len() <= length {
            return;
        }
        self.daemon_query_cache
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.char_masks.truncate(length);
        self.candidates.truncate(length);
        self.prune_cwd_interner();
    }

    /// Update metadata for one SQLite row without rebuilding or reordering history.
    fn update_failed_at(&mut self, index: usize, failed: bool) -> bool {
        let Some(candidate) = self.candidates.get_mut(index) else {
            return false;
        };
        candidate.failed = failed;
        true
    }

    /// Prepend newly loaded SQLite rows and discard older rows they replace.
    ///
    /// Retained candidates keep their owned strings and precomputed metadata;
    /// only the small new prefix is constructed.
    fn prepend_replacing(&mut self, candidates: Vec<CandidateInput>) {
        if candidates.is_empty() {
            return;
        }
        self.daemon_query_cache
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        {
            let replaced_pairs: HashSet<(&str, Option<&str>)> = candidates
                .iter()
                .map(|candidate| (candidate.0.as_str(), candidate.2.as_deref()))
                .collect();
            let mut index = 0;
            while index < self.candidates.len() {
                let candidate = &self.candidates[index];
                if replaced_pairs
                    .contains(&(candidate.text.as_str(), candidate.cwd.as_deref()))
                {
                    self.char_masks.remove(index);
                    self.candidates.remove(index);
                } else {
                    index += 1;
                }
            }
        }
        for input in candidates.into_iter().rev() {
            let candidate = candidate_from_input(input, &mut self.cwd_interner);
            self.char_masks.push_front(candidate.char_mask);
            self.candidates.push_front(candidate);
        }
        self.prune_cwd_interner();
    }

    fn flex_match_many(
        &self,
        py: Python<'_>,
        query_lower: &str,
        candidate_indices: Option<Vec<usize>>,
    ) -> Vec<(usize, i64)> {
        let query = compact_query(query_lower);
        let query_ascii = ascii_query(&query);
        let query_mask = compute_query_char_mask(&query);
        py.allow_threads(|| {
            let mut matches = Vec::new();
            if let Some(indices) = candidate_indices.as_deref() {
                for &index in indices {
                    let Some(&mask) = self.char_masks.get(index) else {
                        continue;
                    };
                    if (mask & query_mask) != query_mask {
                        continue;
                    }
                    let Some(candidate) = self.candidates.get(index) else {
                        continue;
                    };
                    if let Some(score) = candidate.match_flex_score(&query, query_ascii.as_deref())
                    {
                        matches.push((index, score));
                    }
                }
            } else {
                let (slice1, slice2) = self.char_masks.as_slices();
                let mut check = |index: usize| {
                    if let Some(candidate) = self.candidates.get(index) {
                        if let Some(score) =
                            candidate.match_flex_score(&query, query_ascii.as_deref())
                        {
                            matches.push((index, score));
                        }
                    }
                };
                scan_char_masks(slice1, 0, query_mask, &mut check);
                scan_char_masks(slice2, slice1.len(), query_mask, &mut check);
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
        let current_cwd = current_cwd
            .as_deref()
            .and_then(|cwd| self.cwd_interner.get(cwd).cloned());
        if query_lower.is_empty() {
            return py.allow_threads(|| {
                self.search_empty_ranked(
                    candidate_indices.as_deref(),
                    limit,
                    current_cwd.as_ref(),
                )
            });
        }

        let query = compact_query(query_lower);
        let query_ascii = ascii_query(&query);
        let query_mask = compute_query_char_mask(&query);
        py.allow_threads(|| {
            let mut buckets_by_prefix: Vec<[Vec<RankedMatch>; 4]> = Vec::new();
            let mut matched_indices = Some(Vec::new());
            let mut matched_count = 0;

            // Phase 1: Fast SIMD Bitmask Index Pre-Filtering
            let matching_indices = if let Some(indices) = candidate_indices {
                let mut filtered = Vec::with_capacity(indices.len());
                for &index in &indices {
                    let Some(&mask) = self.char_masks.get(index) else {
                        continue;
                    };
                    if (mask & query_mask) == query_mask {
                        filtered.push(index);
                    }
                }
                filtered
            } else {
                let mut collected = Vec::with_capacity(16_384);
                let (slice1, slice2) = self.char_masks.as_slices();
                scan_char_masks(slice1, 0, query_mask, |index| {
                    collected.push(index);
                });
                scan_char_masks(slice2, slice1.len(), query_mask, |index| {
                    collected.push(index);
                });
                collected
            };

            let mut check_candidate = |scan_order: usize, index: usize| {
                let Some(candidate) = self.candidates.get(index) else {
                    return;
                };
                let Some(score) = candidate.match_flex_score(&query, query_ascii.as_deref()) else {
                    return;
                };
                if !normalized_query.is_empty() && candidate.normalized_text() == normalized_query {
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

                let prefix_word_count = if prefix_query_words.len() == 1 {
                    if candidate.words.first().is_some_and(|w| w.starts_with(prefix_query_words[0].as_str())) {
                        1
                    } else {
                        0
                    }
                } else if prefix_query_words.is_empty() {
                    0
                } else {
                    prefix_query_words
                        .iter()
                        .zip(&candidate.words)
                        .take_while(|(query_word, candidate_word)| {
                            candidate_word.starts_with(query_word.as_str())
                        })
                        .count()
                };
                let words_in_order = if ordered_query_words.len() <= 1 {
                    true
                } else {
                    words_appear_in_order(&ordered_query_words, candidate.text_lower())
                };
                let same_cwd = current_cwd.as_ref().is_some_and(|cwd| {
                    candidate
                        .cwd
                        .as_ref()
                        .is_some_and(|candidate_cwd| Arc::ptr_eq(candidate_cwd, cwd))
                });
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

            // Phase 2: Candidate Scoring
            for (scan_order, &index) in matching_indices.iter().enumerate() {
                check_candidate(scan_order, index);
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

            let max_prefix_word_count = buckets_by_prefix.len().saturating_sub(1);
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
        self.serialize_ranked_response(
            &selected,
            matched_indices.as_deref(),
            matched_count,
        )
    }

    /// Cache the complete match set in-process and omit it from the serialized
    /// daemon response sent to the client.
    #[allow(clippy::too_many_arguments)]
    fn search_response_json_for_daemon(
        &self,
        py: Python<'_>,
        query_lower: &str,
        normalized_query: &str,
        prefix_query_words: Vec<String>,
        ordered_query_words: Vec<String>,
        current_cwd: Option<String>,
        candidate_indices: Option<Vec<usize>>,
        limit: Option<usize>,
    ) -> PyResult<String> {
        let cache_enabled = candidate_indices.is_none() && !query_lower.is_empty();
        let cached_indices = if cache_enabled {
            self.daemon_query_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .candidates_for(query_lower)
        } else {
            None
        };
        let (selected, matched_indices, matched_count) = self.search_ranked(
            py,
            query_lower,
            normalized_query,
            prefix_query_words,
            ordered_query_words,
            current_cwd,
            candidate_indices.or(cached_indices),
            limit,
            usize::MAX,
        );
        if cache_enabled {
            if let Some(indices) = matched_indices {
                self.daemon_query_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(query_lower, indices);
            }
        }
        self.serialize_ranked_response(&selected, None, matched_count)
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
