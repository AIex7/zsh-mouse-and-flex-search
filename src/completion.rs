use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::layout::*;
use crate::render::MatchResult;
use crate::search::{
    match_flex_metrics, normalized_flex_admission_score, pairwise_majority_scores,
    word_starts_with_ignoring_separators, words_appear_in_order,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryListingEntry {
    pub name: String,
    pub is_dir: bool,
}

pub struct DirectoryListingCache {
    entries: HashMap<PathBuf, Vec<DirectoryListingEntry>>,
    order: VecDeque<PathBuf>,
    limit: usize,
}

impl Default for DirectoryListingCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            limit: 128,
        }
    }
}

static DIRECTORY_CACHE: Mutex<Option<DirectoryListingCache>> = Mutex::new(None);

pub fn cached_directory_listing(directory: &Path) -> Option<Vec<DirectoryListingEntry>> {
    let resolved = match directory.canonicalize() {
        Ok(p) => p,
        Err(_) => directory.to_path_buf(),
    };

    let mut lock = DIRECTORY_CACHE.lock().unwrap();
    let cache = lock.get_or_insert_with(DirectoryListingCache::default);
    if let Some(cached) = cache.entries.get(&resolved) {
        return Some(cached.clone());
    }

    let mut entries = Vec::new();
    if let Ok(read_dir) = fs::read_dir(&resolved) {
        for entry in read_dir.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                || fs::metadata(entry.path()).map(|m| m.is_dir()).unwrap_or(false);
            entries.push(DirectoryListingEntry { name, is_dir });
        }
    } else {
        return None;
    }

    if cache.order.len() >= cache.limit {
        if let Some(oldest) = cache.order.pop_front() {
            cache.entries.remove(&oldest);
        }
    }
    cache.order.push_back(resolved.clone());
    cache.entries.insert(resolved, entries.clone());
    Some(entries)
}

pub fn top_ranked_directory_entries(
    query: &str,
    entries: &[DirectoryListingEntry],
) -> Vec<DirectoryListingEntry> {
    top_ranked_directory_entries_bounded(query, entries, usize::MAX)
}

fn top_ranked_directory_entries_bounded(
    query: &str,
    entries: &[DirectoryListingEntry],
    flex_pool_limit: usize,
) -> Vec<DirectoryListingEntry> {
    let query_lower = query.to_lowercase();
    let query_chars: Vec<char> = query_lower.chars().filter(|c| !c.is_whitespace()).collect();
    let query_words: Vec<String> = query_lower.split_whitespace().map(str::to_string).collect();
    let mut ranked = Vec::new();

    for entry in entries {
        let entry_lower = entry.name.to_lowercase();
        if let Some(metrics) = match_flex_metrics(&query_chars, &entry_lower) {
            let entry_words: Vec<&str> = entry_lower.split_whitespace().collect();
            let prefix_match = !query_words.is_empty()
                && query_words.len() <= entry_words.len()
                && query_words
                    .iter()
                    .zip(&entry_words)
                    .all(|(query_word, entry_word)| {
                        word_starts_with_ignoring_separators(entry_word, query_word)
                    });
            let words_in_order =
                !query_words.is_empty() && words_appear_in_order(&query_words, &entry_lower);
            let rank_group = if prefix_match {
                0
            } else if words_in_order {
                1
            } else {
                2
            };
            ranked.push((
                entry.clone(),
                metrics,
                rank_group,
                entry_lower.chars().count(),
                entry_lower,
                0_i64,
            ));
        }
    }

    if flex_pool_limit < usize::MAX {
        let mut flex_matches = Vec::new();
        ranked.retain(|item| {
            if item.2 == 2 {
                flex_matches.push(item.clone());
                false
            } else {
                true
            }
        });
        flex_matches.sort_by(|left, right| {
            normalized_flex_admission_score(right.1, query_chars.len(), None)
                .cmp(&normalized_flex_admission_score(
                    left.1,
                    query_chars.len(),
                    None,
                ))
                .then_with(|| left.4.cmp(&right.4))
        });
        flex_matches.truncate(flex_pool_limit);
        ranked.extend(flex_matches);
    }

    let flex_positions: Vec<usize> = ranked
        .iter()
        .enumerate()
        .filter_map(|(index, item)| (item.2 == 2).then_some(index))
        .collect();
    let flex_metrics: Vec<_> = flex_positions.iter().map(|&index| ranked[index].1).collect();
    for (&index, score) in flex_positions
        .iter()
        .zip(pairwise_majority_scores(&flex_metrics, None))
    {
        ranked[index].5 = score;
    }

    ranked.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| {
                if a.2 == 2 {
                    b.5.cmp(&a.5)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| a.3.cmp(&b.3))
            .then_with(|| a.4.cmp(&b.4))
            .then_with(|| a.0.name.cmp(&b.0.name))
    });
    ranked
        .into_iter()
        .map(|(entry, _, _, _, _, _)| entry)
        .collect()
}

pub const PATH_COMPLETION_ENV_VARS: [&str; 8] = [
    "HOME",
    "PWD",
    "OLDPWD",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "TMPDIR",
];

pub fn expand_path_completion_environment(token: &str) -> Option<(String, String, String)> {
    if !token.starts_with('$') {
        return None;
    }
    let after_dollar = &token[1..];
    let (var_name, suffix) = if after_dollar.starts_with('{') {
        let close = after_dollar.find('}')?;
        let name = &after_dollar[1..close];
        let rest = &after_dollar[close + 1..];
        (name, rest)
    } else {
        let split_pos = after_dollar
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(after_dollar.len());
        let name = &after_dollar[..split_pos];
        let rest = &after_dollar[split_pos..];
        (name, rest)
    };

    if !PATH_COMPLETION_ENV_VARS.contains(&var_name) || (!suffix.is_empty() && !suffix.starts_with('/')) {
        return None;
    }
    let value = std::env::var(var_name).ok()?;
    if value.is_empty() {
        return None;
    }
    let variable_text = &token[..token.len() - suffix.len()];
    Some((format!("{}{}", value, suffix), variable_text.to_string(), suffix.to_string()))
}

pub fn runtime_completion_matches(
    query: &str,
    cursor_pos: usize,
    startup_entries: Option<&[DirectoryListingEntry]>,
    cwd: &Path,
    limit: usize,
) -> Vec<MatchResult> {
    if limit == 0 || query.trim().is_empty() {
        return Vec::new();
    }

    let (start, end) = token_bounds(query, cursor_pos);
    let raw_token = query_char_slice(query, start, end);
    let (quote, _) = enclosing_quote(&raw_token);
    let stripped = strip_enclosing_quotes(&raw_token);
    if stripped.is_empty() {
        return Vec::new();
    }

    let incomplete_escape = stripped.ends_with('\\');
    let token_prefix = shell_unescape_fragment(if incomplete_escape {
        &stripped[..stripped.len() - 1]
    } else {
        stripped
    });

    let environment_path = if quote != Some('\'') && !stripped.starts_with("\\$") {
        expand_path_completion_environment(&token_prefix)
    } else {
        None
    };

    let lookup_prefix = match &environment_path {
        Some((exp, _, _)) => exp.as_str(),
        None => token_prefix.as_str(),
    };

    let chosen_entries: Vec<DirectoryListingEntry>;
    let mut completed_prefix = String::new();

    if lookup_prefix.contains('/') {
        let (parent_part, name_prefix) = if lookup_prefix.ends_with('/') {
            (&lookup_prefix[..lookup_prefix.len() - 1], "")
        } else if let Some(last_slash) = lookup_prefix.rfind('/') {
            let p = if last_slash == 0 { "/" } else { &lookup_prefix[..last_slash] };
            let n = &lookup_prefix[last_slash + 1..];
            (p, n)
        } else {
            ("", lookup_prefix)
        };

        let mut display_prefix = parent_part.to_string();
        let base_dir = if let Some((_, var_text, suffix)) = &environment_path {
            if suffix.ends_with('/') {
                display_prefix = format!("{}{}", var_text, &suffix[..suffix.len() - 1]);
            } else if let Some(last_slash) = suffix.rfind('/') {
                display_prefix = format!("{}{}", var_text, &suffix[..last_slash]);
            }
            if parent_part.is_empty() { PathBuf::from("/") } else { PathBuf::from(parent_part) }
        } else if lookup_prefix.starts_with('/') {
            display_prefix = if parent_part.is_empty() { "/".to_string() } else { parent_part.to_string() };
            if parent_part.is_empty() { PathBuf::from("/") } else { PathBuf::from(parent_part) }
        } else if lookup_prefix.starts_with('~') {
            let expanded = if let Ok(home) = std::env::var("HOME") {
                if parent_part == "~" || parent_part.is_empty() {
                    PathBuf::from(home)
                } else {
                    PathBuf::from(home).join(&parent_part[1..].trim_start_matches('/'))
                }
            } else {
                PathBuf::from(parent_part)
            };
            display_prefix = if parent_part.is_empty() { "~".to_string() } else { parent_part.to_string() };
            expanded
        } else {
            let rel = if parent_part.is_empty() { Path::new(".") } else { Path::new(parent_part) };
            cwd.join(rel)
        };

        let cached_entries = match cached_directory_listing(&base_dir) {
            Some(entries) => entries,
            None => return Vec::new(),
        };

        let visible: Vec<DirectoryListingEntry> = cached_entries
            .into_iter()
            .filter(|e| !e.name.starts_with('.') || name_prefix.starts_with('.'))
            .collect();

        chosen_entries = top_ranked_directory_entries_bounded(
            name_prefix,
            &visible,
            limit.saturating_mul(2),
        );

        if display_prefix.is_empty() {
            completed_prefix.clear();
        } else if display_prefix == "." {
            completed_prefix = "./".to_string();
        } else if display_prefix == "/" {
            completed_prefix = "/".to_string();
        } else if display_prefix == "~" {
            completed_prefix = "~/".to_string();
        } else {
            completed_prefix = format!("{}/", display_prefix.trim_end_matches('/'));
        }
    } else {
        if token_prefix.len() <= 2 {
            return Vec::new();
        }
        let entries = match startup_entries {
            Some(e) => e.to_vec(),
            None => match cached_directory_listing(cwd) {
                Some(e) => e,
                None => return Vec::new(),
            },
        };
        let token_prefix_lower = token_prefix.to_lowercase();
        let mut matches: Vec<DirectoryListingEntry> = entries
            .into_iter()
            .filter(|e| (!e.name.starts_with('.') || token_prefix.starts_with('.')) && e.name.to_lowercase().starts_with(&token_prefix_lower))
            .collect();
        matches.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.name.cmp(&b.name)));
        chosen_entries = matches;
    }

    let mut runtime_matches = Vec::new();
    for chosen in chosen_entries {
        let completed_value = format!("{}{}{}", completed_prefix, chosen.name, if chosen.is_dir { "/" } else { "" });
        let completed_token = if let Some((_, var_text, _)) = &environment_path {
            let suffix = &completed_value[var_text.len()..];
            if let Some(q) = quote {
                format!("{}{}{}{}", q, var_text, shell_escape_quoted_fragment(suffix, q), q)
            } else {
                format!("{}{}", var_text, shell_escape_fragment(suffix))
            }
        } else if let Some(q) = quote {
            format!("{}{}{}", q, shell_escape_quoted_fragment(&completed_value, q), q)
        } else {
            shell_escape_fragment(&completed_value)
        };

        let completed_query = replace_query_token(query, cursor_pos, &completed_token);
        if completed_query == query {
            continue;
        }
        let encoded_name = if let Some(q) = quote {
            shell_escape_quoted_fragment(&chosen.name, q)
        } else {
            shell_escape_fragment(&chosen.name)
        };
        let completion_end = start + completed_token.chars().count()
            - usize::from(quote.is_some());
        let completion_start = completion_end
            - encoded_name.chars().count()
            - usize::from(chosen.is_dir);

        let completed_query_lower = completed_query.to_lowercase();
        runtime_matches.push(MatchResult {
            text: completed_query,
            score: 1_000_000_000,
            exact: false,
            recency: 0,
            cwd: None,
            text_lower: Some(completed_query_lower),
            runtime_completion: true,
            history_match: false,
            runtime_completion_span: Some((completion_start, completion_end)),
            failed: false,
            words: Vec::new(),
        });
        if runtime_matches.len() >= limit {
            break;
        }
    }

    runtime_matches
}

pub fn insert_runtime_completions(
    results: Vec<MatchResult>,
    runtime_completions: Vec<MatchResult>,
    featured_count: usize,
) -> Vec<MatchResult> {
    if runtime_completions.is_empty() {
        return results;
    }
    let mut merged = results;
    let runtime_spans: HashMap<String, Option<(usize, usize)>> = runtime_completions
        .iter()
        .map(|item| (item.text.clone(), item.runtime_completion_span))
        .collect();
    for item in &mut merged {
        if let Some(span) = runtime_spans.get(&item.text) {
            item.runtime_completion = true;
            item.runtime_completion_span = *span;
        }
    }
    let mut merged_texts: HashSet<String> = merged.iter().map(|item| item.text.clone()).collect();
    let mut insertion_index = 0;
    for runtime_completion in runtime_completions.iter().take(featured_count) {
        if let Some(existing_index) = merged
            .iter()
            .position(|item| item.text == runtime_completion.text)
        {
            let mut existing = merged.remove(existing_index);
            existing.runtime_completion = true;
            existing.runtime_completion_span = runtime_completion.runtime_completion_span;
            merged.insert(insertion_index, existing);
        } else {
            merged.insert(insertion_index, runtime_completion.clone());
            merged_texts.insert(runtime_completion.text.clone());
        }
        insertion_index += 1;
    }
    for runtime_completion in runtime_completions.iter().skip(featured_count) {
        if merged_texts.contains(&runtime_completion.text) {
            continue;
        }
        merged.push(runtime_completion.clone());
        merged_texts.insert(runtime_completion.text.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_completion_handles_non_ascii_token_boundaries() {
        let entries = vec![DirectoryListingEntry {
            name: "café-file".to_string(),
            is_dir: true,
        }];
        let query = "echo café";

        let matches = runtime_completion_matches(
            query,
            query.chars().count(),
            Some(&entries),
            Path::new("."),
            10,
        );

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "echo caf\\é-file/");
        let (start, end) = matches[0].runtime_completion_span.unwrap();
        assert_eq!(query_char_slice(&matches[0].text, start, end), "caf\\é-file/");
    }
}
