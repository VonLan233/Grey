//! Incremental Server-Sent Events framing shared by provider adapters.

use anyhow::{bail, Context, Result};

const MAX_PENDING_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    pending: Vec<u8>,
}

impl SseDecoder {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.pending.extend_from_slice(bytes);
        let mut data_events = Vec::new();

        while let Some((boundary, delimiter_len)) = find_event_boundary(&self.pending) {
            if boundary > MAX_PENDING_EVENT_BYTES {
                bail!("SSE event exceeds {MAX_PENDING_EVENT_BYTES} bytes");
            }
            let raw_event = self.pending[..boundary].to_vec();
            self.pending.drain(..boundary + delimiter_len);
            if let Some(data) = parse_event(&raw_event)? {
                data_events.push(data);
            }
        }

        if self.pending.len() > MAX_PENDING_EVENT_BYTES {
            bail!("SSE event exceeds {MAX_PENDING_EVENT_BYTES} bytes");
        }
        Ok(data_events)
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.pending.iter().all(u8::is_ascii_whitespace) {
            self.pending.clear();
            Ok(())
        } else {
            bail!(
                "incomplete SSE event at end of stream ({} buffered bytes)",
                self.pending.len()
            )
        }
    }
}

fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|window| window == b"\n\n");
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(left), Some(right)) if left <= right => Some((left, 2)),
        (Some(_), Some(right)) => Some((right, 4)),
        (Some(left), None) => Some((left, 2)),
        (None, Some(right)) => Some((right, 4)),
        (None, None) => None,
    }
}

fn parse_event(raw: &[u8]) -> Result<Option<String>> {
    let text = std::str::from_utf8(raw).context("SSE event is not valid UTF-8")?;
    let mut data_lines = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if data_lines.is_empty() {
        Ok(None)
    } else {
        Ok(Some(data_lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_byte_by_byte_with_crlf_and_multiple_events() {
        let wire = b": keepalive\r\ndata: {\"one\":1}\r\n\r\ndata: {\"two\":2}\n\n";
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for byte in wire {
            events.extend(decoder.feed(std::slice::from_ref(byte)).unwrap());
        }
        decoder.finish().unwrap();
        assert_eq!(events, [r#"{"one":1}"#, r#"{"two":2}"#]);
    }

    #[test]
    fn joins_multiple_data_lines() {
        let mut decoder = SseDecoder::default();
        let events = decoder.feed(b"data: first\ndata: second\n\n").unwrap();
        assert_eq!(events, ["first\nsecond"]);
    }

    #[test]
    fn rejects_incomplete_and_invalid_utf8_events() {
        let mut incomplete = SseDecoder::default();
        incomplete.feed(b"data: {\"unfinished\":true}").unwrap();
        assert!(incomplete
            .finish()
            .unwrap_err()
            .to_string()
            .contains("incomplete"));

        let mut invalid = SseDecoder::default();
        let error = invalid.feed(b"data: \xff\n\n").unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }
}
