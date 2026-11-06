//! Incremental SSE line splitting that tolerates arbitrary byte boundaries.

/// Remove and return every complete line currently in `buffer`.
///
/// The buffer may end in the middle of a UTF-8 character; that partial bytes stay
/// in the buffer until the next chunk arrives.
pub fn drain_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
        let mut raw: Vec<u8> = buffer.drain(..=position).collect();
        raw.pop();
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
        lines.push(String::from_utf8_lossy(&raw).into_owned());
    }
    lines
}

/// Extract the payload of an SSE `data:` line, ignoring other fields and comments.
pub fn data_payload(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data:")?;
    Some(rest.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_newlines_and_keeps_partial() {
        let mut buffer = b"data: one\n\ndata: tw".to_vec();
        let lines = drain_lines(&mut buffer);
        assert_eq!(lines, vec!["data: one", ""]);
        assert_eq!(buffer, b"data: tw");
    }

    #[test]
    fn keeps_multibyte_character_whole() {
        let text = "data: 你好\n".as_bytes().to_vec();
        let mut buffer = Vec::new();
        let mut lines = Vec::new();
        for chunk in text.chunks(1) {
            buffer.extend_from_slice(chunk);
            lines.extend(drain_lines(&mut buffer));
        }
        assert_eq!(lines, vec!["data: 你好"]);
    }

    #[test]
    fn strips_carriage_return() {
        let mut buffer = b"data: x\r\n".to_vec();
        assert_eq!(drain_lines(&mut buffer), vec!["data: x"]);
    }

    #[test]
    fn ignores_non_data_fields() {
        assert_eq!(data_payload("event: message"), None);
        assert_eq!(data_payload("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(data_payload("data:   "), Some(""));
    }
}
