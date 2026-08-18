use pyo3::prelude::*;
use std::collections::HashSet;

const WORD_BOUNDARIES: &str = " _-/.:";

fn compact_query(query_lower: &str) -> Vec<char> {
    query_lower
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// Return the existing Python flex match score and character positions.
///
/// All positions are Unicode character indexes, not UTF-8 byte indexes. This
/// deliberately matches Python's `str.find` and `str[index]` behavior.
fn match_flex(query: &[char], candidate: &str, candidate_lower: &str) -> Option<(i64, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }

    let lowered_characters: Vec<char> = candidate_lower.chars().collect();
    let candidate_characters: Vec<char> = candidate.chars().collect();
    let mut positions = Vec::with_capacity(query.len());
    let mut search_from = 0;

    for query_character in query {
        let relative_position = lowered_characters[search_from..]
            .iter()
            .position(|candidate_character| candidate_character == query_character)?;
        let position = search_from + relative_position;
        positions.push(position);
        search_from = position + 1;
    }

    let mut contiguous = 0_i64;
    let mut gap_penalty = 0_i64;
    let mut boundary_bonus = 0_i64;
    for (index, position) in positions.iter().copied().enumerate() {
        if index == 0 {
            if position == 0 {
                boundary_bonus += 12;
            } else if candidate_characters
                .get(position - 1)
                .is_some_and(|character| WORD_BOUNDARIES.contains(*character))
            {
                boundary_bonus += 8;
            }
            continue;
        }

        let previous = positions[index - 1];
        let gap = position - previous - 1;
        gap_penalty += (gap * 2) as i64;
        if gap == 0 {
            contiguous += 10;
        }
        if candidate_characters
            .get(position - 1)
            .is_some_and(|character| WORD_BOUNDARIES.contains(*character))
        {
            boundary_bonus += 6;
        }
    }

    let first = positions[0];
    let last = *positions.last()?;
    let span = last - first + 1;
    let start_bonus = (30_i64 - first as i64).max(0);
    let compact_bonus = (20_i64 - (span - positions.len()) as i64).max(0);
    let score = contiguous + boundary_bonus + start_bonus + compact_bonus
        - gap_penalty
        - (candidate_characters.len() / 8) as i64;
    Some((score, positions))
}

struct NativeCandidate {
    text: String,
    text_lower: String,
    normalized_text: String,
    cwd: Option<String>,
    words: Vec<String>,
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
    ) -> Self {
        let boundary_characters = text
            .chars()
            .map(|character| WORD_BOUNDARIES.contains(character))
            .collect();
        let character_count = text.chars().count();
        Self {
            text,
            text_lower,
            normalized_text,
            cwd,
            words,
            boundary_characters,
            character_count,
        }
    }

    fn match_flex(&self, query: &[char]) -> Option<(i64, Vec<usize>)> {
        if query.is_empty() {
            return Some((0, Vec::new()));
        }

        let mut positions = Vec::with_capacity(query.len());
        let mut query_index = 0;
        for (position, candidate_character) in self.text_lower.chars().enumerate() {
            if candidate_character != query[query_index] {
                continue;
            }
            positions.push(position);
            query_index += 1;
            if query_index == query.len() {
                break;
            }
        }
        if query_index != query.len() {
            return None;
        }

        let mut contiguous = 0_i64;
        let mut gap_penalty = 0_i64;
        let mut boundary_bonus = 0_i64;
        for (index, position) in positions.iter().copied().enumerate() {
            if index == 0 {
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
                continue;
            }

            let previous = positions[index - 1];
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

        let first = positions[0];
        let last = *positions.last()?;
        let span = last - first + 1;
        let start_bonus = (30_i64 - first as i64).max(0);
        let compact_bonus = (20_i64 - (span - positions.len()) as i64).max(0);
        let score = contiguous + boundary_bonus + start_bonus + compact_bonus
            - gap_penalty
            - (self.character_count / 8) as i64;
        Some((score, positions))
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

struct RankedMatch {
    index: usize,
    score: i64,
    positions: Vec<usize>,
    prefix_word_count: usize,
    words_in_order: bool,
    same_cwd: bool,
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

#[pymethods]
impl NativeHistory {
    #[new]
    fn new(candidates: Vec<(String, String, String, Option<String>, Vec<String>)>) -> Self {
        let candidates = candidates
            .into_iter()
            .map(|(text, text_lower, normalized_text, cwd, words)| {
                NativeCandidate::new(text, text_lower, normalized_text, cwd, words)
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
    ) -> Vec<(usize, i64, Vec<usize>)> {
        let query = compact_query(query_lower);
        py.allow_threads(|| {
            let mut matches = Vec::new();
            if let Some(indices) = candidate_indices.as_deref() {
                for &index in indices {
                    let Some(candidate) = self.candidates.get(index) else {
                        continue;
                    };
                    if let Some((score, positions)) = candidate.match_flex(&query) {
                        matches.push((index, score, positions));
                    }
                }
            } else {
                for (index, candidate) in self.candidates.iter().enumerate() {
                    if let Some((score, positions)) = candidate.match_flex(&query) {
                        matches.push((index, score, positions));
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
    ) -> (Vec<(usize, i64, Vec<usize>)>, Option<Vec<usize>>, usize) {
        let query = compact_query(query_lower);
        py.allow_threads(|| {
            let mut matches = Vec::new();
            let mut matched_indices = if query_lower.is_empty() {
                None
            } else {
                Some(Vec::new())
            };
            let mut matched_count = 0;
            let mut max_prefix_word_count = 0;

            let mut check_candidate = |index: usize| {
                let Some(candidate) = self.candidates.get(index) else {
                    return;
                };
                let matched = if query.is_empty() {
                    Some((0, Vec::new()))
                } else {
                    candidate.match_flex(&query)
                };
                let Some((score, positions)) = matched else {
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
                matches.push(RankedMatch {
                    index,
                    score,
                    positions,
                    prefix_word_count,
                    words_in_order: words_appear_in_order(
                        &ordered_query_words,
                        &candidate.text_lower,
                    ),
                    same_cwd: current_cwd
                        .as_deref()
                        .is_some_and(|cwd| candidate.cwd.as_deref() == Some(cwd)),
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
                        let matched_inner_bucket = match (matched.words_in_order, matched.same_cwd)
                        {
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
                        selected.push((matched.index, matched.score, matched.positions.clone()));
                        if selected.len() >= result_limit {
                            break 'buckets;
                        }
                    }
                }
            }
            (selected, matched_indices, matched_count)
        })
    }
}

#[pyfunction]
fn flex_match(
    query_lower: &str,
    candidate: &str,
    candidate_lower: &str,
) -> Option<(i64, Vec<usize>)> {
    match_flex(&compact_query(query_lower), candidate, candidate_lower)
}

#[pymodule]
fn _flex_match(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeHistory>()?;
    module.add_function(wrap_pyfunction!(flex_match, module)?)
}
