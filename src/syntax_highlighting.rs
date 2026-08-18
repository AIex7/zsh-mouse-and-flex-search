use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyModule};

const DEFAULT: u8 = 0;
const COMMAND: u8 = 1;
const KEYWORD: u8 = 2;
const OPTION: u8 = 3;
const STRING: u8 = 4;
const VARIABLE: u8 = 5;
const OPERATOR: u8 = 6;
const COMMENT: u8 = 7;
const ASSIGNMENT: u8 = 8;
const ERROR: u8 = 9;

const OPERATORS: [&str; 17] = [
    "<<-", "&&", "||", ";;", "<<", ">>", "<&", ">&", "|", ";", "&", "(", ")", "{", "}", "<", ">",
];

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "do"
            | "done"
            | "in"
            | "case"
            | "esac"
            | "select"
            | "function"
            | "time"
            | "coproc"
            | "repeat"
            | "noglob"
            | "builtin"
            | "command"
            | "exec"
            | "eval"
            | "source"
            | "."
    )
}

fn is_assignment(word: &str) -> bool {
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

fn operator_at(text: &[char], index: usize) -> Option<&'static str> {
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

fn is_command_separator(operator: &str) -> bool {
    matches!(operator, "&&" | "||" | "|" | ";" | "&" | "(" | ")")
}

fn is_comment_start(text: &[char], index: usize) -> bool {
    if text.get(index) != Some(&'#') {
        return false;
    }
    if index == 0 {
        return true;
    }
    let previous = text[index - 1];
    previous.is_whitespace() || matches!(previous, ';' | '|' | '&' | '(' | ')' | '{' | '}')
}

fn scan_quoted(text: &[char], start: usize, quote: char) -> usize {
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

fn scan_braced(text: &[char], start: usize) -> usize {
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

fn scan_subshell(text: &[char], start: usize) -> usize {
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

fn scan_arithmetic(text: &[char], start: usize) -> usize {
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

fn scan_dollar_expression(text: &[char], start: usize) -> usize {
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

fn mark(tokens: &mut [u8], start: usize, end: usize, kind: u8) {
    let length = tokens.len();
    tokens[start.min(length)..end.min(length)].fill(kind);
}

fn classify_word(
    command_state: &Bound<'_, PyAny>,
    word: &str,
    expect_command: bool,
    word_complete: bool,
) -> PyResult<u8> {
    if word.is_empty() {
        return Ok(DEFAULT);
    }
    if is_keyword(word) {
        return Ok(KEYWORD);
    }
    if is_assignment(word) {
        return Ok(ASSIGNMENT);
    }
    if word.starts_with('-') && word.chars().count() > 1 {
        return Ok(OPTION);
    }
    if !expect_command {
        return Ok(DEFAULT);
    }
    let result = command_state.call1((word, word_complete))?;
    let state: &str = result.extract()?;
    Ok(match state {
        "valid" => COMMAND,
        "error" => ERROR,
        _ => DEFAULT,
    })
}

fn highlight_from(
    text: &[char],
    tokens: &mut [u8],
    states: &mut [Option<bool>],
    start: usize,
    mut expect_command: bool,
    command_state: &Bound<'_, PyAny>,
) -> PyResult<()> {
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
        let kind = classify_word(command_state, &word, expect_command, word_complete)?;
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
    Ok(())
}

fn relex_start(text: &[char], changed_at: usize) -> usize {
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

#[pyclass]
pub struct NativeIncrementalHighlighter {
    query: Vec<char>,
    tokens: Vec<u8>,
    states: Vec<Option<bool>>,
}

#[pymethods]
impl NativeIncrementalHighlighter {
    #[new]
    fn new() -> Self {
        Self {
            query: Vec::new(),
            tokens: Vec::new(),
            states: vec![Some(true)],
        }
    }

    fn highlight<'py>(
        &mut self,
        py: Python<'py>,
        query: &str,
        command_state: &Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let text: Vec<char> = query.chars().collect();
        if text == self.query {
            return Ok(PyBytes::new(py, &self.tokens));
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
        highlight_from(
            &text,
            &mut tokens,
            &mut states,
            start,
            expect_command.unwrap_or(true),
            command_state,
        )?;
        self.query = text;
        self.tokens = tokens;
        self.states = states;
        Ok(PyBytes::new(py, &self.tokens))
    }
}

pub fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeIncrementalHighlighter>()
}
