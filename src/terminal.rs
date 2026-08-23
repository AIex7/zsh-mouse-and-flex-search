use std::os::unix::io::RawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const RESET: &str = "\x1b[0m";
pub const CLEAR_TO_END: &str = "\x1b[K";
pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";
pub const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub const DISABLE_MOUSE: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1006l";
pub const ENABLE_KITTY_KEYBOARD: &str = "\x1b[>1u";
pub const DISABLE_KITTY_KEYBOARD: &str = "\x1b[<u";

pub fn move_to(row: usize, col: usize) -> String {
    format!("\x1b[{};{}H", row.max(1), col.max(1))
}

pub fn term_write_raw(fd: RawFd, bytes: &[u8]) {
    let mut written = 0;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
            )
        };
        if n <= 0 {
            break;
        }
        written += n as usize;
    }
}

pub fn term_write(fd: RawFd, text: &str) {
    term_write_raw(fd, text.as_bytes());
}

pub fn tty_terminal_size(fd: RawFd, fallback: (usize, usize)) -> (usize, usize) {
    unsafe {
        let mut winsize: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) == 0 {
            let cols = if winsize.ws_col > 0 { winsize.ws_col as usize } else { fallback.0 };
            let rows = if winsize.ws_row > 0 { winsize.ws_row as usize } else { fallback.1 };
            return (cols, rows);
        }
    }
    fallback
}

pub fn supports_kitty_keyboard_protocol() -> bool {
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        return true;
    }
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    term.contains("kitty")
}

pub struct RawTerminal {
    fd: RawFd,
    old_termios: Option<libc::termios>,
    owned: bool,
}

impl RawTerminal {
    pub fn enter(fd: RawFd, owned: bool) -> Result<Self, std::io::Error> {
        let mut old: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut old) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut raw = old;
        unsafe {
            libc::cfmakeraw(&mut raw);
            let _ = libc::tcflush(fd, libc::TCIFLUSH);
            if libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        let term = Self {
            fd,
            old_termios: Some(old),
            owned,
        };

        term_write(fd, DISABLE_MOUSE);
        term_write(fd, HIDE_CURSOR);

        Ok(term)
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        term_write(self.fd, DISABLE_MOUSE);
        term_write(self.fd, SHOW_CURSOR);
        term_write(self.fd, RESET);

        if let Some(old) = self.old_termios.take() {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSADRAIN, &old);
            }
        }

        if self.owned && self.fd > 2 {
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

pub fn select_readable(fd: RawFd, timeout: Option<Duration>) -> bool {
    if fd < 0 || fd >= libc::FD_SETSIZE as RawFd {
        return false;
    }
    unsafe {
        let mut readfds: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut readfds);
        let mut tv = match timeout {
            Some(d) => libc::timeval {
                tv_sec: d.as_secs() as libc::time_t,
                tv_usec: d.subsec_micros() as libc::suseconds_t,
            },
            None => libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        };
        let tv_ptr = if timeout.is_some() {
            &mut tv as *mut libc::timeval
        } else {
            std::ptr::null_mut()
        };
        let res = libc::select(
            fd + 1,
            &mut readfds,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            tv_ptr,
        );
        res > 0 && libc::FD_ISSET(fd, &readfds)
    }
}

pub fn drain_input(fd: RawFd) {
    while select_readable(fd, Some(Duration::from_millis(0))) {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

pub fn parse_cursor_position_response(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut pos = 0;
    let mut last_match = None;
    while pos < bytes.len() {
        if bytes[pos..].starts_with(b"\x1b[") {
            let start = pos + 2;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == b'R' && end > start {
                if let Ok(coord_str) = std::str::from_utf8(&bytes[start..end]) {
                    if let Some((r_str, c_str)) = coord_str.split_once(';') {
                        if let (Ok(r), Ok(c)) = (r_str.parse::<usize>(), c_str.parse::<usize>()) {
                            last_match = Some((r, c));
                        }
                    }
                }
                pos = end + 1;
                continue;
            }
        }
        pos += 1;
    }
    last_match
}

pub fn query_cursor_position(fd: RawFd) -> Option<(usize, usize)> {
    drain_input(fd);
    term_write(fd, "\x1b[6n");

    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        if !select_readable(fd, Some(Duration::from_millis(20))) {
            continue;
        }
        let mut chunk = [0u8; 64];
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if let Some(pos) = parse_cursor_position_response(&buf) {
            return Some(pos);
        }
    }
    parse_cursor_position_response(&buf)
}

fn scale_hex_component(component: &str) -> Option<u8> {
    if component.is_empty() {
        return None;
    }
    let value = u64::from_str_radix(component, 16).ok()?;
    let max_val = 16u64.checked_pow(component.len() as u32)?.saturating_sub(1);
    if max_val == 0 {
        return Some(0);
    }
    Some(((value as f64 / max_val as f64) * 255.0).round() as u8)
}

fn parse_osc_color_response(bytes: &[u8], osc_number: u8) -> Option<String> {
    let mut pos = 0;
    let prefix = format!("\x1b]{osc_number};rgb:");
    let prefix = prefix.as_bytes();
    while pos < bytes.len() {
        if bytes[pos..].starts_with(prefix) {
            let start = pos + prefix.len();
            let mut end = start;
            while end < bytes.len() && bytes[end] != 0x07 && !bytes[end..].starts_with(b"\x1b\\") {
                end += 1;
            }
            if end <= bytes.len() {
                if let Ok(s) = std::str::from_utf8(&bytes[start..end]) {
                    let parts: Vec<&str> = s.split('/').collect();
                    if parts.len() == 3 {
                        if let (Some(r), Some(g), Some(b)) = (
                            scale_hex_component(parts[0]),
                            scale_hex_component(parts[1]),
                            scale_hex_component(parts[2]),
                        ) {
                            return Some(format!("#{:02x}{:02x}{:02x}", r, g, b));
                        }
                    }
                }
            }
        }
        pos += 1;
    }
    None
}

pub fn parse_cursor_color_response(bytes: &[u8]) -> Option<String> {
    parse_osc_color_response(bytes, 12)
}

pub fn parse_background_color_response(bytes: &[u8]) -> Option<String> {
    parse_osc_color_response(bytes, 11)
}

pub fn parse_selection_background_color_response(bytes: &[u8]) -> Option<String> {
    parse_osc_color_response(bytes, 17)
}

pub fn parse_selection_foreground_color_response(bytes: &[u8]) -> Option<String> {
    parse_osc_color_response(bytes, 19)
}

fn configured_terminal_color(name: &str) -> (Option<String>, bool) {
    if let Ok(env_val) = std::env::var(name) {
        let val = env_val.trim().to_lowercase();
        if matches!(val.as_str(), "" | "none" | "disabled" | "unsupported" | "off" | "0") {
            return (None, false);
        }
        if val.starts_with('#') && val.len() == 7 && val[1..].chars().all(|c| c.is_ascii_hexdigit()) {
            return (Some(val), false);
        }
    }
    (None, true)
}

pub fn query_terminal_colors(
    fd: RawFd,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let (mut cursor_color, query_cursor) =
        configured_terminal_color("ZSH_FLEX_HISTORY_CURSOR_COLOR");
    let (mut background_color, query_background) =
        configured_terminal_color("ZSH_FLEX_HISTORY_BACKGROUND_COLOR");
    let (mut selection_background_color, query_selection_background) =
        configured_terminal_color("ZSH_FLEX_HISTORY_SELECTION_BACKGROUND_COLOR");
    let (mut selection_foreground_color, query_selection_foreground) =
        configured_terminal_color("ZSH_FLEX_HISTORY_SELECTION_FOREGROUND_COLOR");

    if !query_cursor
        && !query_background
        && !query_selection_background
        && !query_selection_foreground
    {
        return (
            cursor_color,
            background_color,
            selection_background_color,
            selection_foreground_color,
        );
    }

    drain_input(fd);
    let mut request = String::new();
    if query_background {
        request.push_str("\x1b]11;?\x07");
    }
    if query_cursor {
        request.push_str("\x1b]12;?\x07");
    }
    if query_selection_background {
        request.push_str("\x1b]17;?\x07");
    }
    if query_selection_foreground {
        request.push_str("\x1b]19;?\x07");
    }
    term_write(fd, &request);

    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(25);
    while Instant::now() < deadline {
        if !select_readable(fd, Some(Duration::from_millis(2))) {
            continue;
        }
        let mut chunk = [0u8; 128];
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if query_background && background_color.is_none() {
            background_color = parse_background_color_response(&buf);
        }
        if query_cursor && cursor_color.is_none() {
            cursor_color = parse_cursor_color_response(&buf);
        }
        if query_selection_background && selection_background_color.is_none() {
            selection_background_color = parse_selection_background_color_response(&buf);
        }
        if query_selection_foreground && selection_foreground_color.is_none() {
            selection_foreground_color = parse_selection_foreground_color_response(&buf);
        }
        if (!query_background || background_color.is_some())
            && (!query_cursor || cursor_color.is_some())
            && (!query_selection_background || selection_background_color.is_some())
            && (!query_selection_foreground || selection_foreground_color.is_some())
        {
            break;
        }
    }

    if query_cursor && cursor_color.is_none() {
        std::env::set_var("ZSH_FLEX_HISTORY_CURSOR_COLOR", "none");
    } else if query_cursor {
        let color = cursor_color.as_deref().unwrap_or_default();
        std::env::set_var("ZSH_FLEX_HISTORY_CURSOR_COLOR", color);
    }
    if query_background && background_color.is_none() {
        std::env::set_var("ZSH_FLEX_HISTORY_BACKGROUND_COLOR", "none");
    } else if query_background {
        let color = background_color.as_deref().unwrap_or_default();
        std::env::set_var("ZSH_FLEX_HISTORY_BACKGROUND_COLOR", color);
    }
    if query_selection_background && selection_background_color.is_none() {
        std::env::set_var("ZSH_FLEX_HISTORY_SELECTION_BACKGROUND_COLOR", "none");
    } else if query_selection_background {
        let color = selection_background_color.as_deref().unwrap_or_default();
        std::env::set_var("ZSH_FLEX_HISTORY_SELECTION_BACKGROUND_COLOR", color);
    }
    if query_selection_foreground && selection_foreground_color.is_none() {
        std::env::set_var("ZSH_FLEX_HISTORY_SELECTION_FOREGROUND_COLOR", "none");
    } else if query_selection_foreground {
        let color = selection_foreground_color.as_deref().unwrap_or_default();
        std::env::set_var("ZSH_FLEX_HISTORY_SELECTION_FOREGROUND_COLOR", color);
    }
    (
        cursor_color,
        background_color,
        selection_background_color,
        selection_foreground_color,
    )
}

pub fn query_cursor_color(fd: RawFd) -> Option<String> {
    query_terminal_colors(fd).0
}

pub fn query_background_color(fd: RawFd) -> Option<String> {
    query_terminal_colors(fd).1
}

pub fn query_selection_background_color(fd: RawFd) -> Option<String> {
    query_terminal_colors(fd).2
}

pub fn query_selection_foreground_color(fd: RawFd) -> Option<String> {
    query_terminal_colors(fd).3
}

pub fn write_clipboard(text: &str) -> bool {
    let mut child = match Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|status| status.success()).unwrap_or(false)
}

pub fn read_clipboard() -> String {
    let output = match Command::new("pbpaste")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return String::new(),
    };
    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout);
        raw.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        String::new()
    }
}

pub fn normalize_pasted_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n").replace('\0', "");
    let mut out = String::with_capacity(normalized.len());
    let mut chars = normalized.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&c) = chars.peek() {
                if (0x40..=0x7E).contains(&(c as u32)) {
                    chars.next();
                    break;
                }
                chars.next();
            }
            continue;
        }
        out.push(ch);
    }
    out.replace("200~", "")
        .replace("201~", "")
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn parse_cursor_pos_handles_other_escapes() {
        let raw = b"\x1b[?25l\x1b[?1000l\x1b[14;52R";
        assert_eq!(parse_cursor_position_response(raw), Some((14, 52)));
    }

    #[test]
    fn parse_cursor_color_extracts_rgb() {
        let raw = b"\x1b]12;rgb:2020/5757/9898\x07";
        assert_eq!(parse_cursor_color_response(raw), Some("#205798".to_string()));
    }

    #[test]
    fn parses_background_and_cursor_colors_from_one_response() {
        let raw = b"\x1b]11;rgb:ffff/f0f0/e5e5\x07\x1b]12;rgb:2020/5757/9898\x1b\\";
        assert_eq!(
            parse_background_color_response(raw),
            Some("#fff0e5".to_string())
        );
        assert_eq!(parse_cursor_color_response(raw), Some("#205798".to_string()));
    }

    #[test]
    fn parses_all_terminal_colors_from_one_response() {
        let raw = b"\x1b]11;rgb:ffff/f0f0/e5e5\x07\x1b]12;rgb:2020/5757/9898\x1b\\\x1b]17;rgb:3030/6060/9090\x07\x1b]19;rgb:eeee/dddd/cccc\x1b\\";
        assert_eq!(
            parse_background_color_response(raw),
            Some("#fff0e5".to_string())
        );
        assert_eq!(parse_cursor_color_response(raw), Some("#205798".to_string()));
        assert_eq!(
            parse_selection_background_color_response(raw),
            Some("#306090".to_string())
        );
        assert_eq!(
            parse_selection_foreground_color_response(raw),
            Some("#eeddcc".to_string())
        );
    }
}
