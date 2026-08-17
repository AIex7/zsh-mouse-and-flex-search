use pyo3::prelude::*;

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
    text_lower: String,
    boundary_characters: Vec<bool>,
    character_count: usize,
}

impl NativeCandidate {
    fn new(text: &str, text_lower: String) -> Self {
        let boundary_characters = text
            .chars()
            .map(|character| WORD_BOUNDARIES.contains(character))
            .collect();
        let character_count = text.chars().count();
        Self {
            text_lower,
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

#[pymethods]
impl NativeHistory {
    #[new]
    fn new(candidates: Vec<(String, String)>) -> Self {
        let candidates = candidates
            .into_iter()
            .map(|(text, text_lower)| NativeCandidate::new(&text, text_lower))
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
        if query.is_empty() {
            return Vec::new();
        }
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
