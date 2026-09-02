use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const MAX_DAEMON_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
pub const FRAME_MAGIC: [u8; 4] = *b"ZFH\x02";
pub const FRAME_HEADER_BYTES: usize = 8;
pub const FRAME_PING_REQUEST: u8 = 1;
pub const FRAME_SEARCH_REQUEST: u8 = 2;
pub const FRAME_PATH_COMMANDS_REQUEST: u8 = 3;
pub const FRAME_SEARCH_RESPONSE: u8 = 0x81;
pub const FRAME_PONG_RESPONSE: u8 = 0x82;
pub const FRAME_PATH_COMMANDS_RESPONSE: u8 = 0x83;
pub const FRAME_ERROR_RESPONSE: u8 = 0xff;

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedMatchItem {
    pub text: String,
    pub score: i64,
    pub exact: bool,
    pub recency: i64,
    pub cwd: Option<String>,
    pub failed: bool,
    pub words: Vec<String>,
}

pub type ParsedResponse = (Vec<ParsedMatchItem>, Option<Vec<usize>>);

pub struct FrameWriter {
    payload: Vec<u8>,
}

impl FrameWriter {
    pub fn new(kind: u8) -> Self {
        Self { payload: vec![kind] }
    }

    pub fn byte(&mut self, value: u8) {
        self.payload.push(value);
    }

    pub fn u32(&mut self, value: usize) -> Result<(), &'static str> {
        let value = u32::try_from(value).map_err(|_| "binary frame field is too large")?;
        self.payload.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    pub fn u64(&mut self, value: usize) -> Result<(), &'static str> {
        let value = u64::try_from(value).map_err(|_| "binary frame integer is too large")?;
        self.payload.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    pub fn i64(&mut self, value: i64) {
        self.payload.extend_from_slice(&value.to_le_bytes());
    }

    pub fn string(&mut self, value: &str) -> Result<(), &'static str> {
        self.u32(value.len())?;
        self.payload.extend_from_slice(value.as_bytes());
        Ok(())
    }

    pub fn optional_string(&mut self, value: Option<&str>) -> Result<(), &'static str> {
        self.byte(u8::from(value.is_some()));
        if let Some(value) = value {
            self.string(value)?;
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<u8>, &'static str> {
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

pub struct FrameReader<'a> {
    payload: &'a [u8],
    position: usize,
}

impl<'a> FrameReader<'a> {
    pub fn new(frame: &'a [u8], expected_kind: u8) -> Option<Self> {
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

    pub fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.position.checked_add(count)?;
        let value = self.payload.get(self.position..end)?;
        self.position = end;
        Some(value)
    }

    pub fn byte(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }

    pub fn bool(&mut self) -> Option<bool> {
        match self.byte()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    pub fn u32(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    pub fn u64(&mut self) -> Option<usize> {
        usize::try_from(u64::from_le_bytes(self.take(8)?.try_into().ok()?)).ok()
    }

    pub fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    pub fn string(&mut self) -> Option<String> {
        let length = self.u32()?;
        String::from_utf8(self.take(length)?.to_vec()).ok()
    }

    pub fn optional_string(&mut self) -> Option<Option<String>> {
        if self.bool()? {
            Some(Some(self.string()?))
        } else {
            Some(None)
        }
    }

    pub fn done(&self) -> bool {
        self.position == self.payload.len()
    }

    pub fn remaining(&self) -> usize {
        self.payload.len() - self.position
    }
}

pub fn parse_search_response_bytes(raw: &[u8]) -> Option<ParsedResponse> {
    let mut reader = FrameReader::new(raw, FRAME_SEARCH_RESPONSE)?;
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
        results.push(ParsedMatchItem {
            text,
            score,
            exact,
            recency,
            cwd,
            failed,
            words,
        });
    }
    reader.done().then_some((results, matched_indices))
}

pub fn parse_path_commands_response_bytes(frame: &[u8]) -> Option<Vec<String>> {
    let mut reader = FrameReader::new(frame, FRAME_PATH_COMMANDS_RESPONSE)?;
    let count = reader.u32()?;
    let mut commands = Vec::with_capacity(count);
    for _ in 0..count {
        commands.push(reader.string()?);
    }
    reader.done().then_some(commands)
}

pub fn serialize_search_request_bytes(
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

pub fn serialize_ping_request_bytes() -> Vec<u8> {
    FrameWriter::new(FRAME_PING_REQUEST)
        .finish()
        .expect("fixed ping frame fits")
}

pub fn parse_path_commands_request_bytes(frame: &[u8]) -> Option<(String, String)> {
    let mut reader = FrameReader::new(frame, FRAME_PATH_COMMANDS_REQUEST)?;
    if reader.done() {
        return Some((String::new(), String::new()));
    }
    let path_env = reader.string()?;
    let cwd = reader.string()?;
    reader.done().then_some((path_env, cwd))
}

pub fn serialize_path_commands_request_bytes(path_env: &str, cwd: &str) -> Result<Vec<u8>, &'static str> {
    let mut writer = FrameWriter::new(FRAME_PATH_COMMANDS_REQUEST);
    writer.string(path_env)?;
    writer.string(cwd)?;
    writer.finish()
}

pub fn serialize_path_commands_response_bytes(commands: &[String]) -> Result<Vec<u8>, &'static str> {
    let mut writer = FrameWriter::new(FRAME_PATH_COMMANDS_RESPONSE);
    writer.u32(commands.len())?;
    for cmd in commands {
        writer.string(cmd)?;
    }
    writer.finish()
}

pub fn read_daemon_message(stream: &mut UnixStream) -> Option<Vec<u8>> {
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

pub fn write_daemon_message(stream: &mut UnixStream, payload: &[u8]) -> bool {
    stream.write_all(payload).is_ok()
}

pub fn error_frame(message: &str) -> Vec<u8> {
    let mut writer = FrameWriter::new(FRAME_ERROR_RESPONSE);
    writer.string(message).expect("static error message fits");
    writer.finish().expect("static error frame fits")
}

pub fn daemon_exchange_bytes(
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
