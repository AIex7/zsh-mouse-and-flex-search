use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use crate::terminal::select_readable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Interrupt,
    Escape,
    Enter,
    Tab,
    Backspace,
    BackspaceWord,
    KillToStart,
    KillToEnd,
    Delete,
    Left,
    Right,
    ShiftLeft,
    ShiftRight,
    Home,
    ShiftHome,
    End,
    ShiftEnd,
    WordLeft,
    WordRight,
    SelectAll,
    Up,
    Down,
    PgUp,
    PgDn,
    Char(char),
    Copy,
    Paste,
    PasteText(String),
    Mouse {
        bstate: u32,
        x: usize,
        y: usize,
        action: char,
    },
    Timeout,
}

fn read_exact_one_byte(fd: RawFd) -> Option<u8> {
    let mut b = [0u8; 1];
    let n = unsafe { libc::read(fd, b.as_mut_ptr() as *mut libc::c_void, 1) };
    if n == 1 {
        Some(b[0])
    } else {
        None
    }
}

fn read_escape_tail(fd: RawFd) -> Vec<u8> {
    let mut seq = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(50);
    while Instant::now() < deadline {
        if !select_readable(fd, Some(Duration::from_millis(10))) {
            if !seq.is_empty() {
                break;
            }
            continue;
        }
        let Some(b) = read_exact_one_byte(fd) else {
            break;
        };
        seq.push(b);
        if seq.starts_with(b"[") && seq.len() >= 2 && (0x40..=0x7E).contains(&seq[seq.len() - 1]) {
            break;
        }
        if seq.starts_with(b"O") && seq.len() >= 2 {
            break;
        }
        if !seq.starts_with(b"[") && !seq.starts_with(b"O") && !seq.is_empty() {
            break;
        }
    }
    seq
}

fn parse_csi_key(full: &[u8]) -> Option<InputEvent> {
    let s = std::str::from_utf8(full).ok()?;

    // Match \x1b[(\d+)(?:;(\d+))?u
    if s.starts_with("\x1b[") && s.ends_with('u') {
        let body = &s[2..s.len() - 1];
        let (codepoint_s, mod_s) = body.split_once(';').unwrap_or((body, "1"));
        if let (Ok(codepoint), Ok(modifier)) = (codepoint_s.parse::<u32>(), mod_s.parse::<u32>()) {
            let ctrl = (modifier.saturating_sub(1)) & 4 != 0;
            let alt = (modifier.saturating_sub(1)) & 2 != 0;
            let super_key = (modifier.saturating_sub(1)) & 8 != 0;

            if codepoint == 13 {
                return Some(InputEvent::Enter);
            }
            if codepoint == 9 {
                return Some(InputEvent::Tab);
            }
            if codepoint == 8 || codepoint == 127 {
                if ctrl {
                    return Some(InputEvent::BackspaceWord);
                }
                return Some(InputEvent::Backspace);
            }
            if codepoint == 27 {
                return Some(InputEvent::Escape);
            }
            if (codepoint == 67 || codepoint == 99) && ctrl {
                return Some(InputEvent::Copy);
            }
            if (codepoint == 86 || codepoint == 118) && ctrl {
                return Some(InputEvent::Paste);
            }
            if codepoint == 1 && ctrl {
                return Some(InputEvent::Home);
            }
            if codepoint == 5 && ctrl {
                return Some(InputEvent::End);
            }
            if codepoint == 11 && ctrl {
                return Some(InputEvent::KillToEnd);
            }
            if codepoint == 21 && ctrl {
                return Some(InputEvent::KillToStart);
            }
            if codepoint == 23 && ctrl {
                return Some(InputEvent::BackspaceWord);
            }
            if codepoint == 98 && alt {
                return Some(InputEvent::WordLeft);
            }
            if codepoint == 102 && alt {
                return Some(InputEvent::WordRight);
            }
            if (codepoint == 65 || codepoint == 97) && alt {
                return Some(InputEvent::SelectAll);
            }
            if (codepoint == 67 || codepoint == 99) && (alt || super_key) {
                return Some(InputEvent::Copy);
            }
            if (codepoint == 86 || codepoint == 118) && (alt || super_key) {
                return Some(InputEvent::Paste);
            }
            if (32..127).contains(&codepoint) {
                if let Some(ch) = char::from_u32(codepoint) {
                    return Some(InputEvent::Char(ch));
                }
            }
        }
    }

    // Match \x1b[(?:1;)?(\d+)([ABCDHF])
    if s.starts_with("\x1b[") && s.len() >= 4 {
        let tail = &s[2..];
        let key_char = tail.chars().last()?;
        if matches!(key_char, 'A' | 'B' | 'C' | 'D' | 'H' | 'F') {
            let mod_part = &tail[..tail.len() - 1].trim_start_matches("1;");
            let modifier = mod_part.parse::<u32>().unwrap_or(1);
            if modifier == 1 {
                match key_char {
                    'D' => return Some(InputEvent::Left),
                    'C' => return Some(InputEvent::Right),
                    'H' => return Some(InputEvent::Home),
                    'F' => return Some(InputEvent::End),
                    _ => {}
                }
            } else if modifier == 2 {
                match key_char {
                    'D' => return Some(InputEvent::ShiftLeft),
                    'C' => return Some(InputEvent::ShiftRight),
                    'H' => return Some(InputEvent::ShiftHome),
                    'F' => return Some(InputEvent::ShiftEnd),
                    _ => {}
                }
            } else if modifier == 5 {
                match key_char {
                    'D' => return Some(InputEvent::WordLeft),
                    'C' => return Some(InputEvent::WordRight),
                    _ => {}
                }
            }
        }
    }

    match full {
        b"\x1b[1;5D" | b"\x1b[5D" => Some(InputEvent::WordLeft),
        b"\x1b[1;5C" | b"\x1b[5C" => Some(InputEvent::WordRight),
        b"\x1b[1;2D" => Some(InputEvent::ShiftLeft),
        b"\x1b[1;2C" => Some(InputEvent::ShiftRight),
        b"\x1b[1;2H" => Some(InputEvent::ShiftHome),
        b"\x1b[1;2F" => Some(InputEvent::ShiftEnd),
        _ => None,
    }
}

fn parse_sgr_mouse(full: &[u8]) -> Option<InputEvent> {
    let s = std::str::from_utf8(full).ok()?;
    if !s.starts_with("\x1b[<") || !(s.ends_with('M') || s.ends_with('m')) {
        return None;
    }
    let action = s.chars().last().unwrap();
    let body = &s[3..s.len() - 1];
    let parts: Vec<&str> = body.split(';').collect();
    if parts.len() != 3 {
        return None;
    }
    let bstate = parts[0].parse::<u32>().ok()?;
    let x = parts[1].parse::<usize>().ok()?;
    let y = parts[2].parse::<usize>().ok()?;
    Some(InputEvent::Mouse { bstate, x, y, action })
}

fn read_pending_burst(fd: RawFd, initial: &[u8]) -> String {
    let mut buf = Vec::from(initial);
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        if !select_readable(fd, Some(Duration::from_millis(15))) {
            break;
        }
        let mut chunk = [0u8; 4096];
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if buf.len() >= 1_000_000 {
            break;
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

fn read_utf8_char(fd: RawFd, first_byte: u8) -> char {
    if first_byte < 0x80 {
        return first_byte as char;
    }
    let need = if (first_byte & 0xE0) == 0xC0 {
        2
    } else if (first_byte & 0xF0) == 0xE0 {
        3
    } else if (first_byte & 0xF8) == 0xF0 {
        4
    } else {
        0
    };
    if need == 0 {
        return char::REPLACEMENT_CHARACTER;
    }
    let mut buf = vec![first_byte];
    let deadline = Instant::now() + Duration::from_millis(30);
    while buf.len() < need && Instant::now() < deadline {
        if !select_readable(fd, Some(Duration::from_millis(5))) {
            break;
        }
        let Some(b) = read_exact_one_byte(fd) else {
            break;
        };
        buf.push(b);
    }
    if let Ok(s) = std::str::from_utf8(&buf) {
        s.chars().next().unwrap_or(char::REPLACEMENT_CHARACTER)
    } else {
        char::REPLACEMENT_CHARACTER
    }
}

pub fn read_key(fd: RawFd, timeout: Option<Duration>) -> InputEvent {
    loop {
        if !select_readable(fd, timeout) {
            return InputEvent::Timeout;
        }
        let Some(ch) = read_exact_one_byte(fd) else {
            continue;
        };

        if ch == 3 {
            return InputEvent::Copy;
        }
        if ch == 22 {
            return InputEvent::Paste;
        }
        if ch == 1 {
            return InputEvent::Home;
        }
        if ch == 5 {
            return InputEvent::End;
        }
        if ch == 10 || ch == 13 {
            if select_readable(fd, Some(Duration::from_millis(0))) {
                let burst = read_pending_burst(fd, b"\n");
                return InputEvent::PasteText(burst);
            }
            return InputEvent::Enter;
        }
        if ch == 9 {
            return InputEvent::Tab;
        }
        if ch == 8 || ch == 127 {
            return InputEvent::Backspace;
        }
        if ch == 23 {
            return InputEvent::BackspaceWord;
        }
        if ch == 21 {
            return InputEvent::KillToStart;
        }
        if ch == 11 {
            return InputEvent::KillToEnd;
        }
        if ch == 27 {
            let seq = read_escape_tail(fd);
            let mut full = vec![0x1b];
            full.extend_from_slice(&seq);

            if full == b"\x1b" {
                return InputEvent::Escape;
            }
            if full == b"\x1b[A" {
                return InputEvent::Up;
            }
            if full == b"\x1b[B" {
                return InputEvent::Down;
            }
            if full == b"\x1b[C" {
                return InputEvent::Right;
            }
            if full == b"\x1b[D" {
                return InputEvent::Left;
            }
            if full == b"\x1b[H" || full == b"\x1b[1~" || full == b"\x1bOH" {
                return InputEvent::Home;
            }
            if full == b"\x1b[F" || full == b"\x1b[4~" || full == b"\x1bOF" {
                return InputEvent::End;
            }
            if full == b"\x1b[3~" {
                return InputEvent::Delete;
            }
            if full == b"\x1b[5~" {
                return InputEvent::PgUp;
            }
            if full == b"\x1b[6~" {
                return InputEvent::PgDn;
            }
            if let Some(event) = parse_csi_key(&full) {
                return event;
            }
            if let Some(event) = parse_sgr_mouse(&full) {
                return event;
            }
            if full == b"\x1bb" || full == b"\x1b[1;3D" {
                return InputEvent::WordLeft;
            }
            if full == b"\x1bf" || full == b"\x1b[1;3C" {
                return InputEvent::WordRight;
            }
            if full == b"\x1ba" || full == b"\x1bA" {
                return InputEvent::SelectAll;
            }
            if full == b"\x1bc" || full == b"\x1bC" {
                return InputEvent::Copy;
            }
            if full == b"\x1bv" || full == b"\x1bV" {
                return InputEvent::Paste;
            }
            continue;
        }
        if ch >= 32 {
            if select_readable(fd, Some(Duration::from_millis(0))) {
                let burst = read_pending_burst(fd, &[ch]);
                if burst.len() > 1 || burst.contains('\n') {
                    return InputEvent::PasteText(burst);
                }
                return InputEvent::Char(burst.chars().next().unwrap_or(ch as char));
            }
            return InputEvent::Char(read_utf8_char(fd, ch));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_encoded_key(encoded: &[u8]) -> InputEvent {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        assert_eq!(
            unsafe {
                libc::write(
                    fds[1],
                    encoded.as_ptr() as *const libc::c_void,
                    encoded.len(),
                )
            },
            encoded.len() as isize
        );
        unsafe { libc::close(fds[1]) };
        let event = read_key(fds[0], Some(Duration::from_millis(100)));
        unsafe { libc::close(fds[0]) };
        event
    }

    #[test]
    fn ctrl_c_and_ctrl_v_are_clipboard_shortcuts() {
        assert_eq!(read_encoded_key(b"\x03"), InputEvent::Copy);
        assert_eq!(read_encoded_key(b"\x16"), InputEvent::Paste);
        assert_eq!(read_encoded_key(b"\x1b[99;5u"), InputEvent::Copy);
        assert_eq!(read_encoded_key(b"\x1b[118;5u"), InputEvent::Paste);
    }

    #[test]
    fn escape_still_exits() {
        assert_eq!(read_encoded_key(b"\x1b"), InputEvent::Escape);
        assert_eq!(read_encoded_key(b"\x1b[27u"), InputEvent::Escape);
    }
}
