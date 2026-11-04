use serde_json::Value;

const MAX_BUFFER: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
}

pub fn extract(value: &Value) -> Option<Usage> {
    if let Some(usage) = value.get("usage").and_then(extract_usage) {
        return Some(usage);
    }
    value
        .get("response")
        .and_then(|response| response.get("usage"))
        .and_then(extract_usage)
}

fn extract_usage(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let prompt = first_int(usage, &["prompt_tokens", "input_tokens"]);
    let completion = first_int(usage, &["completion_tokens", "output_tokens"]);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    let input_details = usage
        .get("prompt_tokens_details")
        .or_else(|| usage.get("input_tokens_details"));
    let output_details = usage
        .get("completion_tokens_details")
        .or_else(|| usage.get("output_tokens_details"));
    let cached = input_details
        .and_then(|details| first_int(details, &["cached_tokens", "cache_read_input_tokens"]))
        .unwrap_or(0);
    let cache_write = input_details
        .and_then(|details| first_int(details, &["cache_write_tokens", "cache_creation_input_tokens"]))
        .unwrap_or(0);
    let reasoning = output_details
        .and_then(|details| first_int(details, &["reasoning_tokens"]))
        .unwrap_or(0);
    Some(Usage {
        input_tokens: prompt.unwrap_or(0),
        cached_input_tokens: cached,
        cache_write_tokens: cache_write,
        output_tokens: completion.unwrap_or(0),
        reasoning_tokens: reasoning,
    })
}

fn first_int(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_i64))
}

pub struct SseUsageScanner {
    buffer: Vec<u8>,
    usage: Option<Usage>,
}

impl SseUsageScanner {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            usage: None,
        }
    }

    /// Byte index of the next newline at or after `from`.
    fn line_end(&self, from: usize) -> Option<usize> {
        self.buffer[from..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| from + offset)
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
        let mut consumed = 0;
        while let Some(end) = self.line_end(consumed) {
            if let Some(usage) = parse_line(&self.buffer[consumed..end]) {
                self.usage = Some(usage);
            }
            consumed = end + 1;
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
        if self.buffer.len() > MAX_BUFFER {
            self.buffer.clear();
        }
    }

    pub fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let buffer = std::mem::take(&mut self.buffer);
        if let Some(tail) = buffer.rsplit(|byte| *byte == b'\n').next() {
            if let Some(usage) = parse_line(tail) {
                self.usage = Some(usage);
            }
        }
    }

    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }
}

fn parse_line(line: &[u8]) -> Option<Usage> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut rest = line.strip_prefix(b"data:")?;
    while let [b' ', tail @ ..] = rest {
        rest = tail;
    }
    if rest.is_empty() || rest == b"[DONE]" {
        return None;
    }
    let value: Value = serde_json::from_slice(rest).ok()?;
    extract(&value)
}
