use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const WORD_BOUNDARIES: &str = " _-/.:";

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
    prefix_word_count: u32,
    ranking_flags: u8,
}

const WORDS_IN_ORDER: u8 = 1;
const SAME_CWD: u8 = 2;

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
        let query = compact_query(query_lower);
        let query_ascii = ascii_query(&query);
        py.allow_threads(|| {
            let mut matches = Vec::new();
            let mut matched_indices = if query_lower.is_empty() {
                None
            } else {
                Some(Vec::new())
            };
            let mut matched_count = 0;
            let mut max_prefix_word_count = 0_u32;

            let mut check_candidate = |index: usize| {
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
                    .count()
                    .min(u32::MAX as usize) as u32;
                max_prefix_word_count = max_prefix_word_count.max(prefix_word_count);
                let words_in_order =
                    words_appear_in_order(&ordered_query_words, &candidate.text_lower);
                let same_cwd = current_cwd
                    .as_deref()
                    .is_some_and(|cwd| candidate.cwd.as_deref() == Some(cwd));
                matches.push(RankedMatch {
                    index,
                    score,
                    prefix_word_count,
                    ranking_flags: (u8::from(words_in_order) * WORDS_IN_ORDER)
                        | (u8::from(same_cwd) * SAME_CWD),
                });
            };

            if let Some(indices) = candidate_indices.as_deref() {
                for &index in indices {
                    check_candidate(index);
                }
            } else {
                for index in 0..self.candidates.len() {
                    check_candidate(index);
                }
            }

            let prefix_tiers = if max_prefix_word_count > 0 { 2 } else { 1 };
            let result_limit = limit.unwrap_or(usize::MAX);
            let mut selected = Vec::with_capacity(result_limit.min(matches.len()));
            let mut seen = HashSet::new();
            'buckets: for prefix_tier in 0..prefix_tiers {
                for inner_bucket in 0..4 {
                    for matched in &matches {
                        let matched_prefix_tier = if max_prefix_word_count > 0
                            && matched.prefix_word_count == max_prefix_word_count
                        {
                            0
                        } else if max_prefix_word_count > 0 {
                            1
                        } else {
                            0
                        };
                        let matched_inner_bucket = match (
                            matched.ranking_flags & WORDS_IN_ORDER != 0,
                            matched.ranking_flags & SAME_CWD != 0,
                        ) {
                            (true, true) => 0,
                            (true, false) => 1,
                            (false, true) => 2,
                            (false, false) => 3,
                        };
                        if matched_prefix_tier != prefix_tier
                            || matched_inner_bucket != inner_bucket
                        {
                            continue;
                        }
                        let text = self.candidates[matched.index].text.as_str();
                        if !seen.insert(text) {
                            continue;
                        }
                        selected.push((matched.index, matched.score));
                        if selected.len() >= result_limit {
                            break 'buckets;
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

#[pymodule]
fn _flex_match(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeHistory>()?;
    module.add_function(wrap_pyfunction!(flex_match, module)?)?;
    module.add_function(wrap_pyfunction!(parse_search_response, module)?)
}
