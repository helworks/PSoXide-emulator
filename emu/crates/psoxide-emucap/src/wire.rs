//! emucap's NDJSON control wire, as the producer side sees it.
//!
//! One JSON object per line in each direction. The Control MCP sends
//! `{"v":1,"id":N,"method":"...","params":{...}}`; the producer answers
//! `{"id":N,"ok":true,"result":{...}}` or `{"id":N,"ok":false,"error":{"kind","message"}}`.
//! A request that is still executing is kept alive with `{"status":"working"}`
//! results under the same id (emucap `src/live/protocol.rs`, `src/live/reconnect.rs`).

use std::io::{self, BufRead};

use serde::Serialize;
use serde_json::{json, Map, Value};

/// Wire protocol version this host speaks (emucap `PROTOCOL_VERSION`).
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest NDJSON payload either side may send, excluding the newline. Matches
/// emucap's `MAX_NDJSON_FRAME_BYTES`; a longer frame poisons the connection.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// One parsed control request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Request id, echoed in every response frame for it.
    pub id: u64,
    /// Method name.
    pub method: String,
    /// Method parameters; always an object (`null` is read as `{}`).
    pub params: Map<String, Value>,
}

impl Request {
    /// Parse one request line with the producer-side rules emucap's native hosts
    /// apply (`EmucapControl::ReadRequest`): exactly `v`, `id`, `method`, `params`,
    /// `v == 1`, and `params` an object or `null`. On failure the error carries
    /// the id when one could be recovered, so the reply can still be correlated.
    pub fn parse(line: &str) -> Result<Self, (u64, String)> {
        let value: Value =
            serde_json::from_str(line).map_err(|error| (0, format!("invalid JSON: {error}")))?;
        let object = value
            .as_object()
            .ok_or_else(|| (0, "request must be a JSON object".to_string()))?;
        let id = object.get("id").and_then(Value::as_u64).unwrap_or(0);
        if object.len() != 4
            || !["v", "id", "method", "params"]
                .iter()
                .all(|key| object.contains_key(*key))
        {
            return Err((
                id,
                "request must carry exactly v, id, method and params".into(),
            ));
        }
        if object.get("v").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
            return Err((
                id,
                format!("unsupported protocol version (expected {PROTOCOL_VERSION})"),
            ));
        }
        if object.get("id").and_then(Value::as_u64).is_none() {
            return Err((0, "id must be an unsigned integer".into()));
        }
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 128
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
            .ok_or_else(|| (id, "method must be a non-empty identifier".to_string()))?
            .to_string();
        let params = match object.get("params") {
            Some(Value::Object(params)) => params.clone(),
            Some(Value::Null) => Map::new(),
            _ => return Err((id, "params must be an object or null".into())),
        };
        Ok(Self { id, method, params })
    }
}

/// One response frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Response {
    /// Id of the request this answers.
    pub id: u64,
    /// Whether `result` (true) or `error` (false) is present.
    pub ok: bool,
    /// Success payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

/// Machine-readable failure, `{"kind","message"}` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireError {
    /// Stable category (`bad_params`, `bad_state`, `unsupported`, `busy`,
    /// `emulator_error`, `io_error`, `protocol_error`).
    pub kind: String,
    /// Operator-facing detail.
    pub message: String,
}

impl Response {
    /// Successful terminal response.
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// Failed terminal response.
    pub fn err(id: u64, kind: &str, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(WireError {
                kind: kind.to_string(),
                message: message.into(),
            }),
        }
    }

    /// Non-terminal keepalive for a request that is still executing.
    pub fn working(id: u64) -> Self {
        Self::ok(id, json!({ "status": "working" }))
    }

    /// The frame as one newline-terminated line.
    pub fn to_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).expect("responses always serialize");
        line.push(b'\n');
        line
    }
}

/// Read one newline-terminated UTF-8 frame, keeping bytes already received across
/// read timeouts in `pending` (port of emucap's `read_ndjson_frame`). `Ok(None)`
/// is a clean EOF between frames; an oversized, non-UTF-8 or EOF-truncated frame
/// is an error and the connection must be dropped.
pub fn read_frame<R: BufRead>(reader: &mut R, pending: &mut Vec<u8>) -> io::Result<Option<String>> {
    read_frame_with_limit(reader, pending, MAX_FRAME_BYTES)
}

pub(crate) fn read_frame_with_limit<R: BufRead>(
    reader: &mut R,
    pending: &mut Vec<u8>,
    limit: usize,
) -> io::Result<Option<String>> {
    loop {
        let (consumed, complete) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if pending.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated NDJSON frame before newline",
                ));
            }
            let newline = available.iter().position(|&byte| byte == b'\n');
            let payload = newline.unwrap_or(available.len());
            if pending.len().saturating_add(payload) > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("NDJSON frame exceeds {limit} byte payload limit"),
                ));
            }
            let consumed = newline.map_or(available.len(), |index| index + 1);
            pending.extend_from_slice(&available[..consumed]);
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if complete {
            let bytes = std::mem::take(pending);
            return String::from_utf8(bytes).map(Some).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("NDJSON frame is not UTF-8: {error}"),
                )
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_request() {
        let request = Request::parse(r#"{"v":1,"id":7,"method":"status","params":null}"#).unwrap();
        assert_eq!(request.id, 7);
        assert_eq!(request.method, "status");
        assert!(request.params.is_empty());
    }

    #[test]
    fn rejects_extra_keys_wrong_version_and_bad_params() {
        let extra = Request::parse(r#"{"v":1,"id":3,"method":"x","params":{},"y":0}"#);
        assert_eq!(extra.unwrap_err().0, 3);
        assert!(Request::parse(r#"{"v":2,"id":3,"method":"x","params":{}}"#).is_err());
        assert!(Request::parse(r#"{"v":1,"id":3,"method":"x","params":[1]}"#).is_err());
        assert!(Request::parse(r#"{"v":1,"id":3,"method":"a b","params":{}}"#).is_err());
        assert_eq!(Request::parse("not json").unwrap_err().0, 0);
    }

    #[test]
    fn response_lines_match_the_wire_shape() {
        let ok = String::from_utf8(Response::ok(1, json!({"a":1})).to_line()).unwrap();
        assert_eq!(ok, "{\"id\":1,\"ok\":true,\"result\":{\"a\":1}}\n");
        let err = String::from_utf8(Response::err(2, "bad_params", "no").to_line()).unwrap();
        assert_eq!(
            err,
            "{\"id\":2,\"ok\":false,\"error\":{\"kind\":\"bad_params\",\"message\":\"no\"}}\n"
        );
    }

    #[test]
    fn frames_survive_split_reads_and_enforce_the_limit() {
        let mut pending = Vec::new();
        let mut first = io::BufReader::with_capacity(4, &b"{\"a\":1}\n{\"b\""[..]);
        assert_eq!(
            read_frame(&mut first, &mut pending).unwrap().as_deref(),
            Some("{\"a\":1}\n")
        );
        // The second frame is truncated by EOF: an error, never a partial frame.
        assert!(read_frame(&mut first, &mut pending).is_err());

        let mut pending = Vec::new();
        let mut long = io::BufReader::new(&b"0123456789\n"[..]);
        assert!(read_frame_with_limit(&mut long, &mut pending, 4).is_err());
    }
}
