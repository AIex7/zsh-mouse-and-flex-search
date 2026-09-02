use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const DEFAULT: u8 = 0;
pub const COMMAND: u8 = 1;
pub const KEYWORD: u8 = 2;
pub const OPTION: u8 = 3;
pub const STRING: u8 = 4;
pub const VARIABLE: u8 = 5;
pub const OPERATOR: u8 = 6;
pub const COMMENT: u8 = 7;
pub const ASSIGNMENT: u8 = 8;
pub const ERROR: u8 = 9;

pub const TOKEN_NAMES: [&str; 10] = [
    "default",
    "command",
    "keyword",
    "option",
    "string",
    "variable",
    "operator",
    "comment",
    "assignment",
    "error",
];

pub const ANSI_STYLE_BY_TOKEN_ID: [&str; 10] = [
    "",             // default
    "\x1b[32m",     // command (green)
    "\x1b[34m",     // keyword (blue)
    "\x1b[36m",     // option (cyan)
    "\x1b[33m",     // string (yellow)
    "\x1b[35m",     // variable (magenta)
    "",             // operator (default)
    "\x1b[90m",     // comment (bright black)
    "",             // assignment (default)
    "",             // error (default)
];

pub fn ansi_for_token(token_id: u8) -> &'static str {
    if (token_id as usize) < ANSI_STYLE_BY_TOKEN_ID.len() {
        ANSI_STYLE_BY_TOKEN_ID[token_id as usize]
    } else {
        ""
    }
}

pub const OPERATORS: [&str; 17] = [
    "<<-", "&&", "||", ";;", "<<", ">>", "<&", ">&", "|", ";", "&", "(", ")", "{", "}", "<", ">",
];

pub const KEYWORDS: [&str; 25] = [
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "in", "case",
    "esac", "select", "function", "time", "coproc", "repeat", "noglob", "builtin", "command",
    "exec", "eval", "source", ".",
];

pub const BUILTINS: [&str; 69] = [
    "alias", "autoload", "bg", "bindkey", "break", "builtin", "bye", "cd", "chdir", "command",
    "compgen", "complete", "continue", "declare", "dirs", "disable", "disown", "echo", "echotc",
    "emulate", "enable", "eval", "exec", "exit", "export", "false", "fc", "fg", "functions",
    "getopts", "hash", "history", "jobs", "kill", "let", "limit", "local", "logout", "popd",
    "print", "printf", "pushd", "pwd", "read", "readonly", "rehash", "return", "set", "setopt",
    "shift", "source", "suspend", "test", "times", "trap", "true", "type", "typeset", "ulimit",
    "umask", "unalias", "unfunction", "unset", "unsetopt", "wait", "whence", "where", "which",
    "zmodload",
];

pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(&word)
}

pub fn is_builtin(word: &str) -> bool {
    BUILTINS.contains(&word)
}

pub fn is_assignment(word: &str) -> bool {
    let mut characters = word.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    for character in characters {
        if character == '=' {
            return true;
        }
        if !(character.is_ascii_alphanumeric() || character == '_') {
            return false;
        }
    }
    false
}

pub fn is_ambiguous_command(word: &str) -> bool {
    word.chars()
        .any(|c| matches!(c, '\\' | '$' | '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '\'' | '"' | '`' | '='))
}

pub fn operator_at(text: &[char], index: usize) -> Option<&'static str> {
    OPERATORS.iter().copied().find(|operator| {
        let mut position = index;
        for character in operator.chars() {
            if text.get(position) != Some(&character) {
                return false;
            }
            position += 1;
        }
        true
    })
}

pub fn is_command_separator(operator: &str) -> bool {
    matches!(operator, "&&" | "||" | "|" | ";" | "&" | "(" | ")")
}

pub fn is_comment_start(text: &[char], index: usize) -> bool {
    if text.get(index) != Some(&'#') {
        return false;
    }
    if index == 0 {
        return true;
    }
    let previous = text[index - 1];
    previous.is_whitespace() || matches!(previous, ';' | '|' | '&' | '(' | ')' | '{' | '}')
}

pub fn scan_quoted(text: &[char], start: usize, quote: char) -> usize {
    let mut index = start + 1;
    while index < text.len() {
        let character = text[index];
        if quote != '\'' && character == '\\' && index + 1 < text.len() {
            index += 2;
            continue;
        }
        if character == quote {
            return index + 1;
        }
        index += 1;
    }
    text.len()
}

pub fn scan_braced(text: &[char], start: usize) -> usize {
    let mut depth = 1;
    let mut index = start;
    while index < text.len() {
        let character = text[index];
        if character == '\\' && index + 1 < text.len() {
            index += 2;
            continue;
        }
        if character == '{' {
            depth += 1;
        } else if character == '}' {
            depth -= 1;
            if depth == 0 {
                return index + 1;
            }
        }
        index += 1;
    }
    text.len()
}

pub fn scan_subshell(text: &[char], start: usize) -> usize {
    let mut depth = 1;
    let mut index = start;
    while index < text.len() {
        let character = text[index];
        if matches!(character, '\'' | '"' | '`') {
            index = scan_quoted(text, index, character);
            continue;
        }
        if character == '\\' && index + 1 < text.len() {
            index += 2;
            continue;
        }
        if character == '(' {
            depth += 1;
        } else if character == ')' {
            depth -= 1;
            if depth == 0 {
                return index + 1;
            }
        }
        index += 1;
    }
    text.len()
}

pub fn scan_arithmetic(text: &[char], start: usize) -> usize {
    let mut depth = 1;
    let mut index = start;
    while index < text.len() {
        if text.get(index) == Some(&'(') && text.get(index + 1) == Some(&'(') {
            depth += 1;
            index += 2;
            continue;
        }
        if text.get(index) == Some(&')') && text.get(index + 1) == Some(&')') {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return index;
            }
            continue;
        }
        if text[index] == '\\' && index + 1 < text.len() {
            index += 2;
            continue;
        }
        index += 1;
    }
    text.len()
}

pub fn scan_dollar_expression(text: &[char], start: usize) -> usize {
    if start + 1 >= text.len() {
        return start + 1;
    }
    let next = text[start + 1];
    if next == '{' {
        return scan_braced(text, start + 2);
    }
    if next == '(' {
        if text.get(start + 2) == Some(&'(') {
            return scan_arithmetic(text, start + 3);
        }
        return scan_subshell(text, start + 2);
    }
    if next.is_ascii_digit() || "@*#?$!-_".contains(next) {
        return start + 2;
    }
    if next.is_alphabetic() || next == '_' {
        let mut index = start + 2;
        while index < text.len() && (text[index].is_alphanumeric() || text[index] == '_') {
            index += 1;
        }
        return index;
    }
    start + 1
}

pub fn mark(tokens: &mut [u8], start: usize, end: usize, kind: u8) {
    let length = tokens.len();
    tokens[start.min(length)..end.min(length)].fill(kind);
}

#[derive(Default)]
pub struct PathCommandCache {
    cached_path_env: String,
    cached_cwd: String,
    sorted_executables: Vec<String>,
}

pub fn resolve_path_dir(dir: &str, cwd: &str) -> PathBuf {
    if dir.is_empty() {
        if cwd.is_empty() {
            PathBuf::from(".")
        } else {
            PathBuf::from(cwd)
        }
    } else if Path::new(dir).is_relative() {
        if cwd.is_empty() {
            PathBuf::from(dir)
        } else {
            Path::new(cwd).join(dir)
        }
    } else {
        PathBuf::from(dir)
    }
}

pub fn scan_single_directory_executables(dir: &Path, names: &mut HashSet<String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_file() || file_type.is_symlink() {
                    if let Ok(metadata) = entry.metadata() {
                        if metadata.permissions().mode() & 0o111 != 0 {
                            if let Ok(name) = entry.file_name().into_string() {
                                names.insert(name);
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn scan_path_executables(path_env: &str, cwd: &str) -> Vec<String> {
    let mut names = HashSet::new();
    let mut scanned = HashSet::new();
    for dir in path_env.split(':') {
        let resolved = resolve_path_dir(dir, cwd);
        if !scanned.insert(resolved.clone()) {
            continue;
        }
        scan_single_directory_executables(&resolved, &mut names);
    }
    let mut sorted: Vec<String> = names.into_iter().collect();
    sorted.sort();
    sorted
}

impl PathCommandCache {
    pub fn from_sorted_executables(sorted_executables: Vec<String>) -> Self {
        Self {
            cached_path_env: String::new(),
            cached_cwd: String::new(),
            sorted_executables,
        }
    }

    pub fn refresh_if_needed(&mut self, path_env: &str, cwd: &str) {
        if self.cached_path_env == path_env && self.cached_cwd == cwd && !self.sorted_executables.is_empty() {
            return;
        }
        self.sorted_executables = scan_path_executables(path_env, cwd);
        self.cached_path_env = path_env.to_string();
        self.cached_cwd = cwd.to_string();
    }

    pub fn contains(&self, word: &str) -> bool {
        self.sorted_executables.binary_search_by(|e| e.as_str().cmp(word)).is_ok()
    }
}

pub fn is_executable_file(path_str: &str) -> bool {
    let path = if path_str.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(&path_str[1..].trim_start_matches('/'))
        } else {
            PathBuf::from(path_str)
        }
    } else {
        PathBuf::from(path_str)
    };
    if let Ok(metadata) = fs::metadata(&path) {
        metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0)
    } else {
        false
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommandWordState {
    Valid,
    Pending,
    Error,
}

pub fn command_state(word: &str, _word_complete: bool, path_cache: &PathCommandCache) -> CommandWordState {
    if word.is_empty() || is_ambiguous_command(word) {
        return CommandWordState::Pending;
    }
    if is_keyword(word) || is_builtin(word) {
        return CommandWordState::Valid;
    }
    if word.contains('/') {
        if is_executable_file(word) {
            return CommandWordState::Valid;
        }
    } else if path_cache.contains(word) {
        return CommandWordState::Valid;
    }

    CommandWordState::Error
}

pub fn classify_word(
    word: &str,
    expect_command: bool,
    word_complete: bool,
    path_cache: &PathCommandCache,
) -> u8 {
    if word.is_empty() {
        return DEFAULT;
    }
    if is_keyword(word) {
        return KEYWORD;
    }
    if is_assignment(word) {
        return ASSIGNMENT;
    }
    if word.starts_with('-') && word.chars().count() > 1 {
        return OPTION;
    }
    if !expect_command {
        return DEFAULT;
    }
    match command_state(word, word_complete, path_cache) {
        CommandWordState::Valid => COMMAND,
        CommandWordState::Error => ERROR,
        CommandWordState::Pending => DEFAULT,
    }
}

pub fn highlight_from(
    text: &[char],
    tokens: &mut [u8],
    states: &mut [Option<bool>],
    start: usize,
    mut expect_command: bool,
    path_cache: &PathCommandCache,
) {
    let mut index = start.min(text.len());
    while index < text.len() {
        states[index] = Some(expect_command);
        let character = text[index];
        if character.is_whitespace() {
            index += 1;
            continue;
        }
        if is_comment_start(text, index) {
            mark(tokens, index, text.len(), COMMENT);
            break;
        }
        if let Some(operator) = operator_at(text, index) {
            let end = index + operator.chars().count();
            mark(tokens, index, end, OPERATOR);
            if is_command_separator(operator) {
                expect_command = true;
            }
            index = end;
            if index < states.len() {
                states[index] = Some(expect_command);
            }
            continue;
        }
        if matches!(character, '\'' | '"' | '`') {
            let end = scan_quoted(text, index, character);
            mark(tokens, index, end, STRING);
            index = end;
            expect_command = false;
            if index < states.len() {
                states[index] = Some(expect_command);
            }
            continue;
        }
        if character == '$' {
            let end = scan_dollar_expression(text, index);
            mark(tokens, index, end, VARIABLE);
            index = end;
            expect_command = false;
            if index < states.len() {
                states[index] = Some(expect_command);
            }
            continue;
        }

        let word_start = index;
        while index < text.len() {
            if text[index].is_whitespace() || operator_at(text, index).is_some() {
                break;
            }
            if matches!(text[index], '\'' | '"' | '`') {
                index = scan_quoted(text, index, text[index]);
                continue;
            }
            if text[index] == '$' {
                index = scan_dollar_expression(text, index);
                continue;
            }
            if text[index] == '\\' && index + 1 < text.len() {
                index += 2;
                continue;
            }
            index += 1;
        }
        let word: String = text[word_start..index].iter().collect();
        let word_complete = index < text.len()
            && (text[index].is_whitespace()
                || operator_at(text, index).is_some()
                || is_comment_start(text, index));
        let kind = classify_word(&word, expect_command, word_complete, path_cache);
        mark(tokens, word_start, index, kind);
        if kind == ASSIGNMENT {
            expect_command = true;
        } else if kind == KEYWORD {
            expect_command = matches!(word.as_str(), "then" | "do" | "else" | "elif" | "time");
        } else {
            expect_command = false;
        }
        if index < states.len() {
            states[index] = Some(expect_command);
        }
    }
}

pub fn relex_start(text: &[char], changed_at: usize) -> usize {
    let mut segment_start = 0;
    let mut quote = None;
    let mut index = 0;
    while index < changed_at {
        let character = text[index];
        if let Some(active_quote) = quote {
            if active_quote != '\'' && character == '\\' && index + 1 < changed_at {
                index += 2;
                continue;
            }
            if character == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if character == '\\' && index + 1 < changed_at {
            index += 2;
            continue;
        }
        if matches!(character, '\'' | '"' | '`') {
            quote = Some(character);
            index += 1;
            continue;
        }
        if let Some(operator) = operator_at(text, index) {
            let operator_length = operator.chars().count();
            if index + operator_length <= changed_at {
                if is_command_separator(operator) {
                    segment_start = index + operator_length;
                }
                index += operator_length;
                continue;
            }
        }
        index += 1;
    }
    if quote.is_some() {
        return segment_start;
    }
    if text[..changed_at]
        .iter()
        .any(|character| matches!(character, '$' | '\\' | '#'))
    {
        return 0;
    }
    if changed_at > 0 && matches!(text[changed_at - 1], ';' | '|' | '&' | '<' | '>') {
        return 0;
    }
    let mut boundary = changed_at;
    while boundary > segment_start {
        let previous = text[boundary - 1];
        if previous.is_whitespace() {
            return boundary;
        }
        if matches!(
            previous,
            ';' | '|' | '&' | '(' | ')' | '{' | '}' | '<' | '>'
        ) {
            return boundary;
        }
        boundary -= 1;
    }
    segment_start
}

pub struct IncrementalHighlighter {
    query: Vec<char>,
    tokens: Vec<u8>,
    states: Vec<Option<bool>>,
    path_cache: Mutex<PathCommandCache>,
}

impl Default for IncrementalHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalHighlighter {
    pub fn new() -> Self {
        Self::with_commands(Vec::new())
    }

    pub fn with_commands(commands: Vec<String>) -> Self {
        let path_cache = if !commands.is_empty() {
            PathCommandCache::from_sorted_executables(commands)
        } else {
            let mut cache = PathCommandCache::default();
            let path_env = std::env::var("PATH").unwrap_or_default();
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            cache.refresh_if_needed(&path_env, &cwd);
            cache
        };
        Self {
            query: Vec::new(),
            tokens: Vec::new(),
            states: vec![Some(true)],
            path_cache: Mutex::new(path_cache),
        }
    }

    pub fn highlight(&mut self, query: &str) -> &[u8] {
        let text: Vec<char> = query.chars().collect();
        if text == self.query {
            return &self.tokens;
        }
        let common = self
            .query
            .iter()
            .zip(&text)
            .take_while(|(previous, current)| previous == current)
            .count();
        let mut start = relex_start(&text, common);
        let mut expect_command = self.states.get(start).copied().flatten();
        if expect_command.is_none() {
            start = 0;
            expect_command = Some(true);
        }
        let mut tokens = self.tokens[..start].to_vec();
        tokens.resize(text.len(), DEFAULT);
        let mut states = vec![None; text.len() + 1];
        states[..start].copy_from_slice(&self.states[..start]);

        let path_cache = self.path_cache.lock().unwrap();
        highlight_from(
            &text,
            &mut tokens,
            &mut states,
            start,
            expect_command.unwrap_or(true),
            &path_cache,
        );
        self.query = text;
        self.tokens = tokens;
        self.states = states;
        &self.tokens
    }
}
