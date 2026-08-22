use pyo3::exceptions::{PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

mod syntax_highlighting;

const MAX_DAEMON_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const FRAME_MAGIC: [u8; 4] = *b"ZFH\x01";
const FRAME_HEADER_BYTES: usize = 8;
const FRAME_PING_REQUEST: u8 = 1;
const FRAME_SEARCH_REQUEST: u8 = 2;
const FRAME_SEARCH_RESPONSE: u8 = 0x81;
const FRAME_PONG_RESPONSE: u8 = 0x82;
const FRAME_ERROR_RESPONSE: u8 = 0xff;
const MAX_DAEMON_QUERY_CACHE_ENTRIES: usize = 64;
const MAX_DAEMON_QUERY_CACHE_INDICES: usize = 1_000_000;

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
        .map(|character| {
            if character.is_ascii() {
                Some(*character as u8)
            } else {
                None
            }
        })
        .collect()
}

#[inline(always)]
fn find_byte_simd(slice: &[u8], target: u8) -> Option<usize> {
    if slice.len() < 16 {
        return slice.iter().position(|&b| b == target);
    }

    #[cfg(target_arch = "aarch64")]
    unsafe {
        let target_vec = vdupq_n_u8(target);
        let chunks = slice.chunks_exact(16);
        let remainder = chunks.remainder();
        let mut offset = 0;

        for chunk in chunks {
            let chunk_vec = vld1q_u8(chunk.as_ptr());
            let cmp = vceqq_u8(chunk_vec, target_vec);
            if vmaxvq_u8(cmp) != 0 {
                for (i, &b) in chunk.iter().enumerate() {
                    if b == target {
                        return Some(offset + i);
                    }
                }
            }
            offset += 16;
        }

        for (i, &b) in remainder.iter().enumerate() {
            if b == target {
                return Some(offset + i);
            }
        }
        None
    }

    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    unsafe {
        let target_vec = _mm_set1_epi8(target as i8);
        let chunks = slice.chunks_exact(16);
        let remainder = chunks.remainder();
        let mut offset = 0;

        for chunk in chunks {
            let chunk_vec = _mm_loadu_si128(chunk.as_ptr() as *const __m128i);
            let cmp = _mm_cmpeq_epi8(chunk_vec, target_vec);
            let mask = _mm_movemask_epi8(cmp);
            if mask != 0 {
                let bit = mask.trailing_zeros() as usize;
                return Some(offset + bit);
            }
            offset += 16;
        }

        for (i, &b) in remainder.iter().enumerate() {
            if b == target {
                return Some(offset + i);
            }
        }
        None
    }

    #[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "sse2"))))]
    {
        slice.iter().position(|&b| b == target)
    }
}

fn match_flex_ascii(query: &[u8], _candidate: &[u8], candidate_lower: &[u8]) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut search_from = 0;
    for &target in query {
        if search_from >= candidate_lower.len() {
            return None;
        }
        if candidate_lower[search_from] == target {
            search_from += 1;
        } else {
            search_from += find_byte_simd(&candidate_lower[search_from..], target)? + 1;
        }
    }
    Some(0)
}

/// Return whether query is a valid subsequence of candidate.
fn match_flex(query: &[char], _candidate: &str, candidate_lower: &str) -> Option<i64> {
    if candidate_lower.is_ascii() {
        if let Some(query_ascii) = ascii_query(query) {
            return match_flex_ascii(
                &query_ascii,
                candidate_lower.as_bytes(),
                candidate_lower.as_bytes(),
            );
        }
    }
    if query.is_empty() {
        return Some(0);
    }

    let mut query_index = 0;
    for candidate_character in candidate_lower.chars() {
        if candidate_character == query[query_index] {
            query_index += 1;
            if query_index == query.len() {
                return Some(0);
            }
        }
    }
    None
}

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
fn bigram_hash(b1: u32, b2: u32) -> u64 {
    let val = ((b1 as u64) << 16) | (b2 as u64);
    let h = val.wrapping_mul(0x9E3779B97F4A7C15);
    1u64 << ((h >> 58) & 63)
}

#[inline(always)]
fn compute_bigram_mask_ascii(bytes: &[u8]) -> u64 {
    if bytes.len() < 2 {
        return 0;
    }
    let mut mask = 0u64;
    for i in 0..bytes.len() - 1 {
        mask |= bigram_hash(bytes[i] as u32, bytes[i + 1] as u32);
    }
    mask
}

#[inline(always)]
fn compute_bigram_mask_str(text: &str) -> u64 {
    let mut mask = 0u64;
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if let Some(p) = prev {
            mask |= bigram_hash(p as u32, c as u32);
        }
        prev = Some(c);
    }
    mask
}

#[inline(always)]
fn compute_words_bigram_mask(words: &[String]) -> u64 {
    let mut mask = 0u64;
    for word in words {
        if word.len() >= 2 {
            mask |= compute_bigram_mask_str(word);
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
    mut on_match: impl FnMut(usize) -> bool,
) {
    if query_mask == 0 {
        for i in 0..slice.len() {
            if !on_match(base_offset + i) {
                break;
            }
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
        let mut keep_matching = true;

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

            if keep_matching {
                if vminvq_u8(c0) == 0xFF && !on_match(base) {
                    keep_matching = false;
                }
                if keep_matching && vminvq_u8(c1) == 0xFF && !on_match(base + 1) {
                    keep_matching = false;
                }
                if keep_matching && vminvq_u8(c2) == 0xFF && !on_match(base + 2) {
                    keep_matching = false;
                }
                if keep_matching && vminvq_u8(c3) == 0xFF && !on_match(base + 3) {
                    keep_matching = false;
                }
            } else {
                if vminvq_u8(c0) == 0xFF {
                    let _ = on_match(base);
                }
                if vminvq_u8(c1) == 0xFF {
                    let _ = on_match(base + 1);
                }
                if vminvq_u8(c2) == 0xFF {
                    let _ = on_match(base + 2);
                }
                if vminvq_u8(c3) == 0xFF {
                    let _ = on_match(base + 3);
                }
            }
            base += 4;
        }

        for &mask in remainder {
            let m = vld1q_u8((&mask as *const u128) as *const u8);
            let c = vceqq_u8(vandq_u8(m, q_vec), q_vec);
            if vminvq_u8(c) == 0xFF && !on_match(base) {
                // finished
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
        let mut keep_matching = true;

        for chunk in chunks {
            let ptr = chunk.as_ptr() as *const __m128i;
            let m0 = _mm_loadu_si128(ptr);
            let m1 = _mm_loadu_si128(ptr.add(1));
            let m2 = _mm_loadu_si128(ptr.add(2));
            let m3 = _mm_loadu_si128(ptr.add(3));

            if keep_matching {
                if _mm_testc_si128(m0, q_vec) != 0 && !on_match(base) {
                    keep_matching = false;
                }
                if keep_matching && _mm_testc_si128(m1, q_vec) != 0 && !on_match(base + 1) {
                    keep_matching = false;
                }
                if keep_matching && _mm_testc_si128(m2, q_vec) != 0 && !on_match(base + 2) {
                    keep_matching = false;
                }
                if keep_matching && _mm_testc_si128(m3, q_vec) != 0 && !on_match(base + 3) {
                    keep_matching = false;
                }
            } else {
                if _mm_testc_si128(m0, q_vec) != 0 {
                    let _ = on_match(base);
                }
                if _mm_testc_si128(m1, q_vec) != 0 {
                    let _ = on_match(base + 1);
                }
                if _mm_testc_si128(m2, q_vec) != 0 {
                    let _ = on_match(base + 2);
                }
                if _mm_testc_si128(m3, q_vec) != 0 {
                    let _ = on_match(base + 3);
                }
            }
            base += 4;
        }

        for &mask in remainder {
            let m = _mm_loadu_si128((&mask as *const u128) as *const __m128i);
            if _mm_testc_si128(m, q_vec) != 0 && !on_match(base) {
                // finished
            }
            base += 1;
        }
    }

    #[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "sse4.1"))))]
    {
        for (i, &mask) in slice.iter().enumerate() {
            if (mask & query_mask) == query_mask && !on_match(base_offset + i) {
                // finished
            }
        }
    }
}

const WORD_STORAGE_HEADER_BYTES: usize = 5;
const WORD_STORAGE_PACKED_SOURCE: u8 = 0x80;
const WORD_STORAGE_U16: u8 = 0;
const WORD_STORAGE_U32: u8 = 1;
const WORD_STORAGE_U64: u8 = 2;

/// One allocation containing word spans and, only when required, fallback text.
/// Normal candidates store each `(u16, u16)` span in four bytes.
#[repr(transparent)]
struct CompactWords(Box<[u8]>);

impl CompactWords {
    fn new(source: &str, words: Vec<String>) -> Self {
        let (offsets, packed_source) = if let Some(offsets) = find_word_offsets(source, &words) {
            (offsets, None)
        } else {
            let packed_capacity = words.iter().map(String::len).sum();
            let mut packed = String::with_capacity(packed_capacity);
            let mut offsets = Vec::with_capacity(words.len());
            for word in words {
                let start = packed.len();
                packed.push_str(&word);
                offsets.push((start, packed.len()));
            }
            (offsets, Some(packed))
        };

        let width_format = if offsets.iter().all(|&(_, end)| end <= u16::MAX as usize) {
            WORD_STORAGE_U16
        } else if offsets.iter().all(|&(_, end)| end <= u32::MAX as usize) {
            WORD_STORAGE_U32
        } else {
            WORD_STORAGE_U64
        };
        let span_bytes = match width_format {
            WORD_STORAGE_U16 => 4,
            WORD_STORAGE_U32 => 8,
            _ => 16,
        };
        let packed_bytes = packed_source.as_ref().map_or(0, String::len);
        let mut storage = Vec::with_capacity(
            WORD_STORAGE_HEADER_BYTES + offsets.len() * span_bytes + packed_bytes,
        );
        storage.push(
            width_format
                | if packed_source.is_some() {
                    WORD_STORAGE_PACKED_SOURCE
                } else {
                    0
                },
        );
        storage.extend_from_slice(&(offsets.len() as u32).to_ne_bytes());
        for (start, end) in offsets {
            match width_format {
                WORD_STORAGE_U16 => {
                    storage.extend_from_slice(&(start as u16).to_ne_bytes());
                    storage.extend_from_slice(&(end as u16).to_ne_bytes());
                }
                WORD_STORAGE_U32 => {
                    storage.extend_from_slice(&(start as u32).to_ne_bytes());
                    storage.extend_from_slice(&(end as u32).to_ne_bytes());
                }
                _ => {
                    storage.extend_from_slice(&(start as u64).to_ne_bytes());
                    storage.extend_from_slice(&(end as u64).to_ne_bytes());
                }
            }
        }
        if let Some(packed_source) = packed_source {
            storage.extend_from_slice(packed_source.as_bytes());
        }
        Self(storage.into_boxed_slice())
    }

    fn format(&self) -> u8 {
        self.0.first().copied().unwrap_or(WORD_STORAGE_U16)
    }

    fn len(&self) -> usize {
        self.0
            .get(1..WORD_STORAGE_HEADER_BYTES)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_ne_bytes)
            .unwrap_or(0) as usize
    }

    fn span_bytes(&self) -> usize {
        match self.format() & !WORD_STORAGE_PACKED_SOURCE {
            WORD_STORAGE_U16 => 4,
            WORD_STORAGE_U32 => 8,
            _ => 16,
        }
    }

    fn spans_end(&self) -> usize {
        WORD_STORAGE_HEADER_BYTES + self.len() * self.span_bytes()
    }

    fn get(&self, index: usize) -> Option<(usize, usize)> {
        if index >= self.len() {
            return None;
        }
        let start = WORD_STORAGE_HEADER_BYTES + index * self.span_bytes();
        match self.format() & !WORD_STORAGE_PACKED_SOURCE {
            WORD_STORAGE_U16 => Some((
                u16::from_ne_bytes(self.0.get(start..start + 2)?.try_into().ok()?) as usize,
                u16::from_ne_bytes(self.0.get(start + 2..start + 4)?.try_into().ok()?) as usize,
            )),
            WORD_STORAGE_U32 => Some((
                u32::from_ne_bytes(self.0.get(start..start + 4)?.try_into().ok()?) as usize,
                u32::from_ne_bytes(self.0.get(start + 4..start + 8)?.try_into().ok()?) as usize,
            )),
            _ => Some((
                u64::from_ne_bytes(self.0.get(start..start + 8)?.try_into().ok()?) as usize,
                u64::from_ne_bytes(self.0.get(start + 8..start + 16)?.try_into().ok()?) as usize,
            )),
        }
    }

    fn is_compact(&self) -> bool {
        self.format() & !WORD_STORAGE_PACKED_SOURCE == WORD_STORAGE_U16
    }

    fn is_wide(&self) -> bool {
        !self.is_compact()
    }

    fn is_packed(&self) -> bool {
        self.format() & WORD_STORAGE_PACKED_SOURCE != 0
    }

    fn packed_source(&self) -> Option<&str> {
        if !self.is_packed() {
            return None;
        }
        std::str::from_utf8(self.0.get(self.spans_end()..)?).ok()
    }

    fn packed_bytes(&self) -> usize {
        self.packed_source().map_or(0, str::len)
    }
}

fn find_word_offsets(source: &str, words: &[String]) -> Option<Vec<(usize, usize)>> {
    let mut offsets = Vec::with_capacity(words.len());
    let mut cursor = 0;
    for word in words {
        let relative_start = source.get(cursor..)?.find(word)?;
        let start = cursor + relative_start;
        let end = start.checked_add(word.len())?;
        offsets.push((start, end));
        cursor = end;
    }
    Some(offsets)
}

struct NativeCandidate {
    text: String,
    text_lower: Option<Box<str>>,
    normalized_start: u32,
    normalized_end: u32,
    cwd: Option<Arc<str>>,
    words: CompactWords,
    failed: bool,
    ascii: bool,
}

impl NativeCandidate {
    fn new(
        text: String,
        text_lower: String,
        cwd: Option<Arc<str>>,
        words: Vec<String>,
        failed: bool,
    ) -> (Self, u128, u64) {
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
        let char_mask = if ascii {
            compute_char_mask_ascii(lower.as_bytes())
        } else {
            compute_char_mask_str(lower)
        };
        let bigram_mask = if ascii {
            compute_bigram_mask_ascii(lower.as_bytes())
        } else {
            compute_bigram_mask_str(lower)
        };
        let words = CompactWords::new(lower, words);
        let candidate = Self {
            text,
            text_lower,
            normalized_start,
            normalized_end,
            cwd,
            words,
            failed,
            ascii,
        };
        (candidate, char_mask, bigram_mask)
    }

    fn text_lower(&self) -> &str {
        self.text_lower.as_deref().unwrap_or(&self.text)
    }

    fn normalized_text(&self) -> &str {
        self.text_lower()
            .get(self.normalized_start as usize..self.normalized_end as usize)
            .unwrap_or_else(|| python_trim(self.text_lower()))
    }

    fn word_source(&self) -> &str {
        self.words
            .packed_source()
            .unwrap_or_else(|| self.text_lower())
    }

    fn word_count(&self) -> usize {
        self.words.len()
    }

    fn word(&self, index: usize) -> Option<&str> {
        let (start, end) = self.words.get(index)?;
        self.word_source().get(start..end)
    }

    fn first_word(&self) -> Option<&str> {
        self.word(0)
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
        let mut query_index = 0;
        for candidate_character in self.text_lower().chars() {
            if candidate_character == query[query_index] {
                query_index += 1;
                if query_index == query.len() {
                    return Some(0);
                }
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
    bigram_masks: VecDeque<u64>,
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

type ParsedResult = (String, i64, bool, i64, Option<String>, bool, Vec<String>);
type ParsedResponse = (Vec<ParsedResult>, Option<Vec<usize>>, usize);

struct FrameWriter {
    payload: Vec<u8>,
}

impl FrameWriter {
    fn new(kind: u8) -> Self {
        Self { payload: vec![kind] }
    }

    fn byte(&mut self, value: u8) {
        self.payload.push(value);
    }

    fn u32(&mut self, value: usize) -> Result<(), &'static str> {
        let value = u32::try_from(value).map_err(|_| "binary frame field is too large")?;
        self.payload.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn u64(&mut self, value: usize) -> Result<(), &'static str> {
        let value = u64::try_from(value).map_err(|_| "binary frame integer is too large")?;
        self.payload.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn i64(&mut self, value: i64) {
        self.payload.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) -> Result<(), &'static str> {
        self.u32(value.len())?;
        self.payload.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn optional_string(&mut self, value: Option<&str>) -> Result<(), &'static str> {
        self.byte(u8::from(value.is_some()));
        if let Some(value) = value {
            self.string(value)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, &'static str> {
        if self.payload.len() > MAX_DAEMON_MESSAGE_BYTES {
            return Err("binary frame exceeds daemon message limit");
        }
        let payload_len = u32::try_from(self.payload.len())
            .map_err(|_| "binary frame exceeds its length field")?;
        let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + self.payload.len());
        frame.extend_from_slice(&FRAME_MAGIC);
        frame.extend_from_slice(&payload_len.to_le_bytes());
        frame.extend_from_slice(&self.payload);
        Ok(frame)
    }
}

struct FrameReader<'a> {
    payload: &'a [u8],
    position: usize,
}

impl<'a> FrameReader<'a> {
    fn new(frame: &'a [u8], expected_kind: u8) -> Option<Self> {
        if frame.len() < FRAME_HEADER_BYTES || frame[..4] != FRAME_MAGIC {
            return None;
        }
        let payload_len = u32::from_le_bytes(frame[4..8].try_into().ok()?) as usize;
        if payload_len == 0
            || payload_len > MAX_DAEMON_MESSAGE_BYTES
            || frame.len() != FRAME_HEADER_BYTES + payload_len
            || frame[FRAME_HEADER_BYTES] != expected_kind
        {
            return None;
        }
        Some(Self {
            payload: &frame[FRAME_HEADER_BYTES..],
            position: 1,
        })
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.position.checked_add(count)?;
        let value = self.payload.get(self.position..end)?;
        self.position = end;
        Some(value)
    }

    fn byte(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }

    fn bool(&mut self) -> Option<bool> {
        match self.byte()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn u32(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    fn u64(&mut self) -> Option<usize> {
        usize::try_from(u64::from_le_bytes(self.take(8)?.try_into().ok()?)).ok()
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let length = self.u32()?;
        String::from_utf8(self.take(length)?.to_vec()).ok()
    }

    fn optional_string(&mut self) -> Option<Option<String>> {
        if self.bool()? {
            Some(Some(self.string()?))
        } else {
            Some(None)
        }
    }

    fn done(&self) -> bool {
        self.position == self.payload.len()
    }

    fn remaining(&self) -> usize {
        self.payload.len() - self.position
    }
}

fn parse_search_response_bytes(raw: &[u8]) -> Option<ParsedResponse> {
    let mut reader = FrameReader::new(raw, FRAME_SEARCH_RESPONSE)?;
    let matched_count = reader.u64()?;
    let matched_indices = if reader.bool()? {
        let count = reader.u32()?;
        if count > reader.remaining() / 8 {
            return None;
        }
        let mut indices = Vec::with_capacity(count);
        for _ in 0..count {
            indices.push(reader.u64()?);
        }
        Some(indices)
    } else {
        None
    };
    let result_count = reader.u32()?;
    if result_count > reader.remaining() / 27 {
        return None;
    }
    let mut results = Vec::with_capacity(result_count);
    for _ in 0..result_count {
        let text = reader.string()?;
        let score = reader.i64()?;
        let exact = reader.bool()?;
        let recency = reader.i64()?;
        let cwd = reader.optional_string()?;
        let failed = reader.bool()?;
        let word_count = reader.u32()?;
        if word_count > reader.remaining() / 4 {
            return None;
        }
        let mut words = Vec::with_capacity(word_count);
        for _ in 0..word_count {
            words.push(reader.string()?);
        }
        results.push((text, score, exact, recency, cwd, failed, words));
    }
    reader.done().then_some((results, matched_indices, matched_count))
}

fn serialize_search_request_bytes(
    query: &str,
    candidate_indices: Option<&[usize]>,
    limit: Option<i64>,
    cwd: Option<&str>,
) -> Result<Vec<u8>, &'static str> {
    let mut writer = FrameWriter::new(FRAME_SEARCH_REQUEST);
    writer.string(query)?;
    writer.byte(u8::from(candidate_indices.is_some()));
    if let Some(indices) = candidate_indices {
        writer.u32(indices.len())?;
        for &index in indices {
            writer.u64(index)?;
        }
    }
    writer.byte(u8::from(limit.is_some()));
    if let Some(limit) = limit {
        writer.i64(limit);
    }
    writer.optional_string(cwd)?;
    writer.finish()
}

fn serialize_ping_request_bytes() -> Vec<u8> {
    FrameWriter::new(FRAME_PING_REQUEST)
        .finish()
        .expect("fixed ping frame fits")
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

    read_daemon_message(&mut stream)
}

fn read_daemon_message(stream: &mut UnixStream) -> Option<Vec<u8>> {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    stream.read_exact(&mut header).ok()?;
    if header[..4] != FRAME_MAGIC {
        return None;
    }
    let payload_len = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
    if payload_len == 0 || payload_len > MAX_DAEMON_MESSAGE_BYTES {
        return None;
    }
    let mut message = Vec::with_capacity(FRAME_HEADER_BYTES + payload_len);
    message.extend_from_slice(&header);
    message.resize(FRAME_HEADER_BYTES + payload_len, 0);
    stream.read_exact(&mut message[FRAME_HEADER_BYTES..]).ok()?;
    Some(message)
}

fn write_daemon_message(stream: &mut UnixStream, payload: &[u8]) -> bool {
    stream.write_all(payload).is_ok()
}

fn error_frame(message: &str) -> Vec<u8> {
    let mut writer = FrameWriter::new(FRAME_ERROR_RESPONSE);
    writer.string(message).expect("static error message fits");
    writer.finish().expect("static error frame fits")
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

    fn respond_frame(&mut self, payload: &[u8]) -> bool {
        self.stream
            .take()
            .is_some_and(|mut stream| write_daemon_message(&mut stream, payload))
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
                write_daemon_message(&mut stream, &error_frame("invalid request"));
                continue;
            };
            let Some(&kind) = raw.get(FRAME_HEADER_BYTES) else {
                write_daemon_message(&mut stream, &error_frame("invalid request"));
                continue;
            };
            if kind == FRAME_PING_REQUEST {
                let valid = FrameReader::new(&raw, FRAME_PING_REQUEST).is_some_and(|reader| reader.done());
                if valid {
                    let pong = FrameWriter::new(FRAME_PONG_RESPONSE)
                        .finish()
                        .expect("fixed pong frame fits");
                    write_daemon_message(&mut stream, &pong);
                } else {
                    write_daemon_message(&mut stream, &error_frame("invalid request"));
                }
                continue;
            }
            if kind != FRAME_SEARCH_REQUEST {
                write_daemon_message(&mut stream, &error_frame("unknown frame type"));
                continue;
            }

            let Some(mut reader) = FrameReader::new(&raw, FRAME_SEARCH_REQUEST) else {
                write_daemon_message(&mut stream, &error_frame("invalid request"));
                continue;
            };
            let parsed = (|| {
                let query = reader.string()?;
                let candidate_indices = if reader.bool()? {
                    let count = reader.u32()?;
                    if count > reader.remaining() / 8 {
                        return None;
                    }
                    let mut indices = Vec::with_capacity(count);
                    for _ in 0..count {
                        indices.push(reader.u64()?);
                    }
                    Some(indices)
                } else {
                    None
                };
                let limit = if reader.bool()? {
                    Some(reader.i64()?)
                } else {
                    None
                };
                let cwd = reader.optional_string()?;
                reader.done().then_some((query, candidate_indices, limit, cwd))
            })();
            let Some((query, candidate_indices, limit, cwd)) = parsed else {
                write_daemon_message(&mut stream, &error_frame("invalid request"));
                continue;
            };
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
) -> (NativeCandidate, u128, u64) {
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
    ) -> Result<Vec<u8>, &'static str> {
        let mut writer = FrameWriter::new(FRAME_SEARCH_RESPONSE);
        writer.u64(matched_count)?;
        writer.byte(u8::from(matched_indices.is_some()));
        if let Some(indices) = matched_indices {
            writer.u32(indices.len())?;
            for &index in indices {
                writer.u64(index)?;
            }
        }
        writer.u32(selected.len())?;
        for &(index, score) in selected {
            let candidate = &self.candidates[index];
            writer.string(&candidate.text)?;
            writer.i64(score);
            writer.byte(0); // exact
            writer.i64(-(index as i64));
            writer.optional_string(candidate.cwd.as_deref())?;
            writer.byte(u8::from(candidate.failed));
            writer.u32(candidate.word_count())?;
            for word_index in 0..candidate.word_count() {
                writer.string(candidate.word(word_index).unwrap_or_default())?;
            }
        }
        writer.finish()
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
        let mut bigram_masks = VecDeque::with_capacity(candidates_input.len());
        for input in candidates_input {
            let (candidate, char_mask, bigram_mask) =
                candidate_from_input(input, &mut cwd_interner);
            char_masks.push_back(char_mask);
            bigram_masks.push_back(bigram_mask);
            candidates.push_back(candidate);
        }
        Self {
            char_masks,
            bigram_masks,
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

    /// Report flattened word storage for regression tests and diagnostics.
    /// (words, compact-span candidates, wide-span candidates,
    /// packed fallback candidates, packed fallback bytes)
    fn candidate_word_storage_stats(&self) -> (usize, usize, usize, usize, usize) {
        let word_count = self
            .candidates
            .iter()
            .map(NativeCandidate::word_count)
            .sum();
        let compact_candidates = self
            .candidates
            .iter()
            .filter(|candidate| candidate.words.is_compact())
            .count();
        let wide_candidates = self
            .candidates
            .iter()
            .filter(|candidate| candidate.words.is_wide())
            .count();
        let packed_candidates = self
            .candidates
            .iter()
            .filter(|candidate| candidate.words.is_packed())
            .count();
        let packed_bytes = self
            .candidates
            .iter()
            .map(|candidate| candidate.words.packed_bytes())
            .sum();
        (
            word_count,
            compact_candidates,
            wide_candidates,
            packed_candidates,
            packed_bytes,
        )
    }

    /// Append a batch while initially loading a large history.
    fn extend(&mut self, candidates: Vec<CandidateInput>) {
        self.daemon_query_cache
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        for input in candidates {
            let (candidate, char_mask, bigram_mask) =
                candidate_from_input(input, &mut self.cwd_interner);
            self.char_masks.push_back(char_mask);
            self.bigram_masks.push_back(bigram_mask);
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
        self.bigram_masks.truncate(length);
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
                    self.bigram_masks.remove(index);
                    self.candidates.remove(index);
                } else {
                    index += 1;
                }
            }
        }
        for input in candidates.into_iter().rev() {
            let (candidate, char_mask, bigram_mask) =
                candidate_from_input(input, &mut self.cwd_interner);
            self.char_masks.push_front(char_mask);
            self.bigram_masks.push_front(bigram_mask);
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
            if let Some(indices) = candidate_indices {
                for index in indices {
                    let Some(&mask) = self.char_masks.get(index) else {
                        continue;
                    };
                    if (mask & query_mask) != query_mask {
                        continue;
                    }
                    if let Some(candidate) = self.candidates.get(index) {
                        if let Some(score) =
                            candidate.match_flex_score(&query, query_ascii.as_deref())
                        {
                            matches.push((index, score));
                        }
                    }
                }
            } else {
                let (slice1, slice2) = self.char_masks.as_slices();
                let mut check = |index: usize| -> bool {
                    if let Some(candidate) = self.candidates.get(index) {
                        if let Some(score) =
                            candidate.match_flex_score(&query, query_ascii.as_deref())
                        {
                            matches.push((index, score));
                        }
                    }
                    true
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
        let query_words_bigram_mask = if ordered_query_words.len() > 1 {
            compute_words_bigram_mask(&ordered_query_words)
        } else {
            0
        };
        let prefix_bigram_mask =
            if prefix_query_words.len() == 1 && prefix_query_words[0].len() >= 2 {
                compute_bigram_mask_str(&prefix_query_words[0])
            } else {
                0
            };

        let result_limit = limit.unwrap_or(usize::MAX);
        let single_char_query = query.len() == 1;
        let single_ascii_prefix_byte = if prefix_query_words.len() == 1
            && prefix_query_words[0].len() == 1
            && prefix_query_words[0].is_ascii()
        {
            Some(prefix_query_words[0].as_bytes()[0])
        } else {
            None
        };

        py.allow_threads(|| {
            let mut buckets_by_prefix: Vec<[Vec<RankedMatch>; 4]> = Vec::new();
            let mut matched_indices = Some(Vec::new());
            let mut matched_count = 0;
            let mut preferred_collected = 0;
            let mut scoring_completed = false;
            let mut seen_preferred = HashSet::new();

            let mut check_candidate = |scan_order: usize, index: usize| -> bool {
                if scoring_completed {
                    matched_count += 1;
                    if let Some(indices) = matched_indices.as_mut() {
                        if indices.len() < max_returned_indices {
                            indices.push(index);
                            return true;
                        } else {
                            matched_indices = None;
                        }
                    }
                    return false;
                }

                let Some(candidate) = self.candidates.get(index) else {
                    return true;
                };
                let is_single_ascii = single_char_query && query_ascii.is_some();
                let score = if is_single_ascii {
                    0
                } else {
                    let Some(score) = candidate.match_flex_score(&query, query_ascii.as_deref()) else {
                        return true;
                    };
                    score
                };
                if !normalized_query.is_empty() && candidate.normalized_text() == normalized_query {
                    return true;
                }

                matched_count += 1;
                if let Some(indices) = matched_indices.as_mut() {
                    if indices.len() < max_returned_indices {
                        indices.push(index);
                    } else {
                        matched_indices = None;
                    }
                }

                let prefix_word_count = if let Some(prefix_byte) = single_ascii_prefix_byte {
                    let lower_bytes = candidate.text_lower().as_bytes();
                    if lower_bytes.first().copied() == Some(prefix_byte) {
                        1
                    } else if candidate
                        .first_word()
                        .is_some_and(|word| word.starts_with(prefix_query_words[0].as_str()))
                    {
                        1
                    } else {
                        0
                    }
                } else if prefix_query_words.len() == 1 {
                    let prefix = prefix_query_words[0].as_str();
                    let has_prefix_match = if prefix_bigram_mask != 0 {
                        self.bigram_masks
                            .get(index)
                            .map_or(true, |&bm| (bm & prefix_bigram_mask) == prefix_bigram_mask)
                    } else {
                        true
                    };
                    if has_prefix_match
                        && candidate
                            .first_word()
                            .is_some_and(|word| word.starts_with(prefix))
                    {
                        1
                    } else {
                        0
                    }
                } else if prefix_query_words.is_empty() {
                    0
                } else {
                    prefix_query_words
                        .iter()
                        .enumerate()
                        .take_while(|(index, query_word)| {
                            candidate
                                .word(*index)
                                .is_some_and(|word| word.starts_with(query_word.as_str()))
                        })
                        .count()
                };
                let words_in_order = if ordered_query_words.len() <= 1 {
                    true
                } else if query_words_bigram_mask != 0 {
                    let has_bigram_match = self
                        .bigram_masks
                        .get(index)
                        .map_or(true, |&bm| (bm & query_words_bigram_mask) == query_words_bigram_mask);
                    if has_bigram_match {
                        words_appear_in_order(&ordered_query_words, candidate.text_lower())
                    } else {
                        false
                    }
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
                if buckets_by_prefix[prefix_word_count][inner_bucket].len() < result_limit * 2 {
                    buckets_by_prefix[prefix_word_count][inner_bucket].push(RankedMatch {
                        index,
                        score,
                        scan_order,
                    });
                }

                if single_char_query && prefix_word_count == 1 {
                    if current_cwd.is_none() {
                        if seen_preferred.insert(candidate.text.as_str()) {
                            preferred_collected += 1;
                            if preferred_collected >= result_limit {
                                scoring_completed = true;
                            }
                        }
                    } else if inner_bucket == 0 {
                        if seen_preferred.insert(candidate.text.as_str()) {
                            preferred_collected += 1;
                            if preferred_collected >= result_limit {
                                scoring_completed = true;
                            }
                        }
                    }
                }
                true
            };

            if let Some(indices) = candidate_indices.as_deref() {
                for (scan_order, &index) in indices.iter().enumerate() {
                    let Some(&mask) = self.char_masks.get(index) else {
                        continue;
                    };
                    if (mask & query_mask) != query_mask {
                        continue;
                    }
                    if !check_candidate(scan_order, index) {
                        break;
                    }
                }
            } else {
                let (slice1, slice2) = self.char_masks.as_slices();
                scan_char_masks(slice1, 0, query_mask, |index| {
                    check_candidate(index, index)
                });
                scan_char_masks(slice2, slice1.len(), query_mask, |index| {
                    check_candidate(index, index)
                });
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
    fn search_response_frame<'py>(
        &self,
        py: Python<'py>,
        query_lower: &str,
        normalized_query: &str,
        prefix_query_words: Vec<String>,
        ordered_query_words: Vec<String>,
        current_cwd: Option<String>,
        candidate_indices: Option<Vec<usize>>,
        limit: Option<usize>,
        max_returned_indices: usize,
    ) -> PyResult<Bound<'py, PyBytes>> {
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
        let frame = self
            .serialize_ranked_response(&selected, matched_indices.as_deref(), matched_count)
            .map_err(PyValueError::new_err)?;
        Ok(PyBytes::new(py, &frame))
    }

    /// Cache the complete match set in-process and omit it from the serialized
    /// daemon response sent to the client.
    #[allow(clippy::too_many_arguments)]
    fn search_response_frame_for_daemon<'py>(
        &self,
        py: Python<'py>,
        query_lower: &str,
        normalized_query: &str,
        prefix_query_words: Vec<String>,
        ordered_query_words: Vec<String>,
        current_cwd: Option<String>,
        candidate_indices: Option<Vec<usize>>,
        limit: Option<usize>,
    ) -> PyResult<Bound<'py, PyBytes>> {
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
        let frame = self
            .serialize_ranked_response(&selected, None, matched_count)
            .map_err(PyValueError::new_err)?;
        Ok(PyBytes::new(py, &frame))
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
        .map_err(PyValueError::new_err)?;
    Ok(PyBytes::new(py, &serialized))
}

#[pyfunction]
#[pyo3(signature = (socket_path, timeout_seconds=0.5))]
fn ping_daemon(py: Python<'_>, socket_path: &str, timeout_seconds: f64) -> bool {
    let Ok(timeout) = Duration::try_from_secs_f64(timeout_seconds) else {
        return false;
    };
    let request = serialize_ping_request_bytes();
    let response = py.allow_threads(|| daemon_exchange_bytes(socket_path, &request, timeout));
    response.is_some_and(|response| {
        FrameReader::new(&response, FRAME_PONG_RESPONSE).is_some_and(|reader| reader.done())
    })
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
    module.add_function(wrap_pyfunction!(ping_daemon, module)?)?;
    module.add_function(wrap_pyfunction!(search_daemon, module)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_search_request_is_versioned_and_length_prefixed() {
        let frame = serialize_search_request_bytes(
            "git st",
            Some(&[1, 3, 8]),
            Some(100),
            Some("/repo"),
        )
        .unwrap();
        assert_eq!(&frame[..4], &FRAME_MAGIC);
        assert_eq!(
            u32::from_le_bytes(frame[4..8].try_into().unwrap()) as usize,
            frame.len() - FRAME_HEADER_BYTES
        );

        let mut reader = FrameReader::new(&frame, FRAME_SEARCH_REQUEST).unwrap();
        assert_eq!(reader.string().as_deref(), Some("git st"));
        assert_eq!(reader.bool(), Some(true));
        assert_eq!(reader.u32(), Some(3));
        assert_eq!(reader.u64(), Some(1));
        assert_eq!(reader.u64(), Some(3));
        assert_eq!(reader.u64(), Some(8));
        assert_eq!(reader.bool(), Some(true));
        assert_eq!(reader.i64(), Some(100));
        assert_eq!(reader.optional_string().unwrap().as_deref(), Some("/repo"));
        assert!(reader.done());
    }

    #[test]
    fn binary_search_response_round_trips() {
        let mut writer = FrameWriter::new(FRAME_SEARCH_RESPONSE);
        writer.u64(2).unwrap();
        writer.byte(1);
        writer.u32(2).unwrap();
        writer.u64(3).unwrap();
        writer.u64(8).unwrap();
        writer.u32(1).unwrap();
        writer.string("git status --short").unwrap();
        writer.i64(72);
        writer.byte(0);
        writer.i64(-3);
        writer.optional_string(Some("/repo")).unwrap();
        writer.byte(0);
        writer.u32(3).unwrap();
        writer.string("git").unwrap();
        writer.string("status").unwrap();
        writer.string("--short").unwrap();

        assert_eq!(
            parse_search_response_bytes(&writer.finish().unwrap()),
            Some((
                vec![(
                    "git status --short".to_owned(),
                    72,
                    false,
                    -3,
                    Some("/repo".to_owned()),
                    false,
                    vec!["git".to_owned(), "status".to_owned(), "--short".to_owned()],
                )],
                Some(vec![3, 8]),
                2,
            ))
        );
    }

    #[test]
    fn binary_response_parser_rejects_truncation_and_trailing_bytes() {
        let mut frame = FrameWriter::new(FRAME_SEARCH_RESPONSE);
        frame.u64(0).unwrap();
        frame.byte(0);
        frame.u32(0).unwrap();
        let frame = frame.finish().unwrap();
        assert!(parse_search_response_bytes(&frame[..frame.len() - 1]).is_none());
        let mut trailing = frame;
        trailing.push(0);
        assert!(parse_search_response_bytes(&trailing).is_none());
    }

    #[test]
    fn compact_word_field_does_not_enlarge_native_candidate_layout() {
        assert_eq!(
            std::mem::size_of::<CompactWords>(),
            std::mem::size_of::<Box<[Box<str>]>>()
        );
    }

    #[test]
    fn compact_words_borrow_the_candidate_text() {
        let source = "git status --short";
        let words = CompactWords::new(
            source,
            vec!["git".to_owned(), "status".to_owned(), "--short".to_owned()],
        );
        assert!(words.is_compact());
        assert!(!words.is_packed());
        assert_eq!(words.get(1), Some((4, 10)));
    }

    #[test]
    fn shell_transformed_words_share_one_packed_fallback() {
        let words = CompactWords::new(
            r"printf hello\ world",
            vec!["printf".to_owned(), "hello world".to_owned()],
        );
        assert!(words.is_compact());
        assert_eq!(words.packed_source(), Some("printfhello world"));
        assert_eq!(words.get(1), Some((6, 17)));
    }

    #[test]
    fn oversized_candidates_use_wide_offsets_without_truncation() {
        let source = format!("{}word", "x".repeat(u16::MAX as usize + 1));
        let words = CompactWords::new(&source, vec!["word".to_owned()]);
        assert!(words.is_wide());
        assert!(!words.is_packed());
        assert_eq!(
            words.get(0),
            Some((u16::MAX as usize + 1, u16::MAX as usize + 5))
        );
    }
}
