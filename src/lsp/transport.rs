use std::io::{self, BufRead, Write};

pub use lsp_server::{Message, Notification, Request, RequestId, Response};

pub struct Transport<R: BufRead, W: Write> {
    reader: R,
    writer: W,
    next_id: i32,
}

impl<R: BufRead, W: Write> Transport<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 0,
        }
    }

    pub fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> io::Result<RequestId> {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        let request = Request {
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        Message::Request(request).write(&mut self.writer)?;
        self.writer.flush()?;
        Ok(id)
    }

    pub fn send_notification(&mut self, method: &str, params: serde_json::Value) -> io::Result<()> {
        let notification = Notification {
            method: method.to_string(),
            params,
        };
        Message::Notification(notification).write(&mut self.writer)?;
        self.writer.flush()
    }

    pub fn send_response(&mut self, id: RequestId, result: serde_json::Value) -> io::Result<()> {
        let response = Response {
            id,
            result: Some(result),
            error: None,
        };
        Message::Response(response).write(&mut self.writer)?;
        self.writer.flush()
    }

    pub fn read(&mut self) -> io::Result<Option<Message>> {
        Message::read(&mut self.reader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    #[test]
    fn writes_a_request_with_incrementing_ids() {
        let input: Vec<u8> = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        let mut t = Transport::new(Cursor::new(input), &mut out);
        let first = t.send_request("initialize", serde_json::json!({})).unwrap();
        let second = t.send_request("shutdown", serde_json::json!(null)).unwrap();
        assert_ne!(first, second);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Content-Length:"));
        assert!(text.contains("\"method\":\"initialize\""));
    }

    #[test]
    fn reads_a_framed_response_message() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let mut t = Transport::new(Cursor::new(frame(body)), Vec::new());
        let msg = t.read().unwrap().expect("a message");
        match msg {
            Message::Response(r) => {
                assert_eq!(r.result.unwrap(), serde_json::json!({"ok": true}))
            }
            other => panic!("expected response, got {other:?}"),
        }
    }
}
