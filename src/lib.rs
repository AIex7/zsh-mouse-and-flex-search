use pyo3::prelude::*;
use pyo3::types::{PyAny, PyList, PyTuple};

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

#[pyfunction]
fn flex_match(
    query_lower: &str,
    candidate: &str,
    candidate_lower: &str,
) -> Option<(i64, Vec<usize>)> {
    match_flex(&compact_query(query_lower), candidate, candidate_lower)
}

/// Match an entire history candidate list in one Python-to-Rust call.
///
/// `candidates` contains `(original_text, lowercased_text)` pairs. Optional
/// indexes retain the caller's existing incremental-search candidate order.
#[pyfunction]
fn flex_match_many(
    query_lower: &str,
    candidates: &Bound<'_, PyList>,
    candidate_indices: Option<&Bound<'_, PyAny>>,
) -> PyResult<Vec<(usize, i64, Vec<usize>)>> {
    let query = compact_query(query_lower);
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let mut matches = Vec::new();
    let mut check_candidate = |index: usize| -> PyResult<()> {
        let item = candidates.get_item(index)?;
        let pair = item.downcast::<PyTuple>()?;
        let candidate_object = pair.get_item(0)?;
        let candidate_lower_object = pair.get_item(1)?;
        let candidate: &str = candidate_object.extract()?;
        let candidate_lower: &str = candidate_lower_object.extract()?;
        if let Some((score, positions)) = match_flex(&query, candidate, candidate_lower) {
            matches.push((index, score, positions));
        }
        Ok(())
    };

    if let Some(indices) = candidate_indices {
        for raw_index in indices.try_iter()? {
            let index: usize = raw_index?.extract()?;
            if index < candidates.len() {
                check_candidate(index)?;
            }
        }
    } else {
        for index in 0..candidates.len() {
            check_candidate(index)?;
        }
    }
    Ok(matches)
}

#[pymodule]
fn _flex_match(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(flex_match, module)?)?;
    module.add_function(wrap_pyfunction!(flex_match_many, module)?)
}
