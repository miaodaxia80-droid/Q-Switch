//! LSP frame parser/encoder
//!
//! Implements the Language Server Protocol frame format used by Qoder's
//! Agent Control Protocol over WebSocket:
//!
//! ```text
//! Content-Length: <byte-count>\r\n
//! \r\n
//! <JSON-RPC payload>
//! ```

use std::io;

/// Incremental LSP frame parser.
///
/// Feed raw bytes via [`LspFramer::append`]; it buffers internally and
/// returns zero or more complete frame payloads.
pub struct LspFramer {
    buffer: Vec<u8>,
}

const SEPARATOR: &[u8] = b"\r\n\r\n";

impl LspFramer {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Append raw bytes and extract any complete frames.
    ///
    /// Returns an error if the header is malformed (missing or invalid
    /// `Content-Length`).
    pub fn append(&mut self, data: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();

        while let Some(sep_pos) = find_subsequence(&self.buffer, SEPARATOR) {
            let header_bytes = &self.buffer[..sep_pos];
            let header_str = String::from_utf8_lossy(header_bytes);
            let content_length = match parse_content_length(&header_str) {
                Some(n) => n,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "LSP frame missing valid Content-Length header",
                    ));
                }
            };

            let body_start = sep_pos + SEPARATOR.len();
            if self.buffer.len() - body_start < content_length {
                // Not enough bytes yet — wait for more.
                break;
            }

            let body_end = body_start + content_length;
            let body = self.buffer[body_start..body_end].to_vec();
            frames.push(body);

            // Remove consumed bytes from the buffer.
            self.buffer.drain(..body_end);
        }

        Ok(frames)
    }

    /// Encode a JSON payload as an LSP frame.
    pub fn encode(body: &[u8]) -> Vec<u8> {
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut framed = Vec::with_capacity(header.len() + body.len());
        framed.extend_from_slice(header.as_bytes());
        framed.extend_from_slice(body);
        framed
    }
}

impl Default for LspFramer {
    fn default() -> Self {
        Self::new()
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_content_length(header: &str) -> Option<usize> {
    for line in header.split("\r\n") {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("Content-Length") {
                return value.trim().parse().ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_roundtrip() {
        let payload = br#"{"jsonrpc":"2.0","method":"ping","id":1}"#;
        let framed = LspFramer::encode(payload);

        let mut parser = LspFramer::new();
        let frames = parser.append(&framed).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], payload);
    }

    #[test]
    fn split_across_chunks() {
        let payload = br#"{"jsonrpc":"2.0","method":"ping"}"#;
        let framed = LspFramer::encode(payload);

        let mut parser = LspFramer::new();
        let mid = framed.len() / 2;

        // First half — no complete frame yet.
        let f1 = parser.append(&framed[..mid]).unwrap();
        assert!(f1.is_empty());

        // Second half — frame completes.
        let f2 = parser.append(&framed[mid..]).unwrap();
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0], payload);
    }

    #[test]
    fn multiple_frames_in_one_buffer() {
        let p1 = br#"{"id":1}"#;
        let p2 = br#"{"id":2}"#;
        let mut combined = LspFramer::encode(p1);
        combined.extend_from_slice(&LspFramer::encode(p2));

        let mut parser = LspFramer::new();
        let frames = parser.append(&combined).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], p1);
        assert_eq!(frames[1], p2);
    }

    #[test]
    fn missing_content_length_errors() {
        let bad = b"Content-Type: text\r\n\r\n{}";
        let mut parser = LspFramer::new();
        assert!(parser.append(bad).is_err());
    }
}
