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
    let usage = value.get("usage")?.as_object()?;
    let prompt = usage.get("prompt_tokens").and_then(Value::as_i64);
    let completion = usage.get("completion_tokens").and_then(Value::as_i64);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    let details = usage.get("prompt_tokens_details");
    let cached = details
        .and_then(|value| value.get("cached_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| {
            details
                .and_then(|value| value.get("cache_read_input_tokens"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    let cache_write = details
        .and_then(|value| value.get("cache_write_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| {
            details
                .and_then(|value| value.get("cache_creation_input_tokens"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|value| value.get("reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(Usage {
        input_tokens: prompt.unwrap_or(0),
        cached_input_tokens: cached,
        cache_write_tokens: cache_write,
        output_tokens: completion.unwrap_or(0),
        reasoning_tokens: reasoning,
    })
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

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
        let mut consumed = 0;
        loop {
            let Some(offset) = self.buffer[consumed..]
                .iter()
                .position(|byte| *byte == b'\n')
            else {
                break;
            };
            let end = consumed + offset;
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
        match buffer.rsplit(|byte| *byte == b'\n').next() {
            Some(tail) if !tail.is_empty() => {
                if let Some(usage) = parse_line(tail) {
                    self.usage = Some(usage);
                }
            }
            _ => {}
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
