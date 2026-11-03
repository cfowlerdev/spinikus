//! The messages of a terminal's WebSocket, which the API, the dashboard and the CLI share.
//! Binary messages are the terminal's bytes: keystrokes from the client, output from the
//! server. Text messages are these, as JSON with a `type`.

use serde::{Deserialize, Serialize};

/// A text message from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    /// The window changed size, in characters.
    Resize { rows: u16, columns: u16 },
}

/// A text message from the server; the last one before it closes the socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The command exited with this code.
    Exit { code: i64 },
    /// The terminal could not be opened, or could not go on.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_tagged_json() {
        let resize: ClientMessage = serde_json::from_str(r#"{"type":"resize","rows":40,"columns":120}"#).unwrap();
        assert_eq!(resize, ClientMessage::Resize { rows: 40, columns: 120 });
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"resize","rows":-1,"columns":1}"#).is_err());
        let exit = serde_json::to_string(&ServerMessage::Exit { code: 3 }).unwrap();
        assert_eq!(exit, r#"{"type":"exit","code":3}"#);
        let error = serde_json::to_string(&ServerMessage::Error { message: "gone".to_string() }).unwrap();
        assert_eq!(error, r#"{"type":"error","message":"gone"}"#);
    }
}
