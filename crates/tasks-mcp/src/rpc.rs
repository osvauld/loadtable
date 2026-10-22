//! JSON-RPC 2.0 messages as MCP frames them: one JSON object per line.

use serde_json::{Value, json};

/// Parse error: the line is not JSON.
pub const PARSE_ERROR: i64 = -32700;
/// Invalid request: JSON, but not a JSON-RPC message.
pub const INVALID_REQUEST: i64 = -32600;
/// Unknown method.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Invalid parameters (including an unknown tool name).
pub const INVALID_PARAMS: i64 = -32602;
/// Internal error.
pub const INTERNAL_ERROR: i64 = -32603;
/// Server busy: too many requests in flight.
pub const SERVER_BUSY: i64 = -32000;

/// A decoded incoming line.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A request that wants a response.
    Request {
        /// The id, a string or a number.
        id: Value,
        /// The method.
        method: String,
        /// The parameters (`Null` when absent).
        params: Value,
    },
    /// A notification: never answered.
    Notification {
        /// The method.
        method: String,
        /// The parameters (`Null` when absent).
        params: Value,
    },
    /// Not a usable message; answer with this error.
    Invalid {
        /// The id when one could be read, else `Null`.
        id: Value,
        /// The JSON-RPC error code.
        code: i64,
        /// The text.
        message: String,
    },
}

/// Decodes one line.
pub fn parse_line(line: &[u8]) -> Incoming {
    let invalid = |id: Value, code: i64, message: &str| Incoming::Invalid {
        id,
        code,
        message: message.to_string(),
    };
    let value: Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => return invalid(Value::Null, PARSE_ERROR, &format!("parse error: {e}")),
    };
    let Value::Object(mut obj) = value else {
        let msg = if value.is_array() {
            "batches are not supported; send one JSON-RPC message per line"
        } else {
            "a JSON-RPC message is a JSON object"
        };
        return invalid(Value::Null, INVALID_REQUEST, msg);
    };
    let id = obj.remove("id");
    let id_ok = id
        .as_ref()
        .is_none_or(|i| i.is_string() || i.is_number() || i.is_null());
    let id_for_error = id.clone().filter(|_| id_ok).unwrap_or(Value::Null);
    if !id_ok {
        return invalid(
            Value::Null,
            INVALID_REQUEST,
            "id must be a string or a number",
        );
    }
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return invalid(id_for_error, INVALID_REQUEST, "jsonrpc must be \"2.0\"");
    }
    let Some(method) = obj
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        // a response from the client (we never send requests) or garbage
        return invalid(id_for_error, INVALID_REQUEST, "missing method");
    };
    let params = obj.remove("params").unwrap_or(Value::Null);
    match id {
        Some(Value::Null) => invalid(Value::Null, INVALID_REQUEST, "id must not be null"),
        Some(id) => Incoming::Request { id, method, params },
        None => Incoming::Notification { method, params },
    }
}

/// A success response line.
pub fn result_response(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// An error response line.
pub fn error_response(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_notifications_and_garbage_are_told_apart() {
        let r = parse_line(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        assert!(matches!(r, Incoming::Request { ref method, .. } if method == "ping"));
        let n = parse_line(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert!(matches!(n, Incoming::Notification { .. }));
        for (line, code) in [
            (&b"not json"[..], PARSE_ERROR),
            (b"[]", INVALID_REQUEST),
            (b"3", INVALID_REQUEST),
            (br#"{"id":1,"method":"x"}"#, INVALID_REQUEST),
            (br#"{"jsonrpc":"2.0","id":1}"#, INVALID_REQUEST),
            (
                br#"{"jsonrpc":"2.0","id":{"a":1},"method":"x"}"#,
                INVALID_REQUEST,
            ),
            (
                br#"{"jsonrpc":"2.0","id":null,"method":"x"}"#,
                INVALID_REQUEST,
            ),
        ] {
            assert!(
                matches!(parse_line(line), Incoming::Invalid { code: c, .. } if c == code),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn an_invalid_request_keeps_its_id_when_readable() {
        let r = parse_line(br#"{"jsonrpc":"1.0","id":7,"method":"x"}"#);
        assert!(matches!(r, Incoming::Invalid { id, .. } if id == json!(7)));
    }
}
