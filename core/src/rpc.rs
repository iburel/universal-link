// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! JSON-RPC 2.0 grammar: responses, errors, params extraction.
//!
//! A deliberate copy of `server/src/rpc.rs` — a single protocol grammar in the
//! project (doc/architecture.md). To be extracted into a shared crate if a
//! third copy threatens. Divergence: `app` is a `String`, because the Core
//! relays the application errors received from the server as-is.

use serde_json::{Value, json};

pub struct RpcErr {
    pub code: i64,
    pub message: String,
    /// Application code, placed in `error.data.code`.
    pub app: Option<String>,
}

impl RpcErr {
    /// Application error: a generic JSON-RPC code, the business code in data.
    pub fn app(code: &str) -> RpcErr {
        RpcErr {
            code: -32000,
            message: code.replace('_', " ").to_lowercase(),
            app: Some(code.to_string()),
        }
    }

    /// Reconstructs a JSON-RPC error received from the server, to relay it.
    pub fn from_value(err: &Value) -> RpcErr {
        RpcErr {
            code: err["code"].as_i64().unwrap_or(-32000),
            message: err["message"]
                .as_str()
                .unwrap_or("server error")
                .to_string(),
            app: err
                .pointer("/data/code")
                .and_then(Value::as_str)
                .map(String::from),
        }
    }

    pub fn invalid_request() -> RpcErr {
        RpcErr {
            code: -32600,
            message: "invalid request".into(),
            app: None,
        }
    }

    pub fn method_not_found(method: &str) -> RpcErr {
        RpcErr {
            code: -32601,
            message: format!("method not found: {method}"),
            app: None,
        }
    }

    pub fn invalid_params(what: &str) -> RpcErr {
        RpcErr {
            code: -32602,
            message: format!("invalid params: {what}"),
            app: None,
        }
    }
}

pub fn response_ok(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

pub fn response_err(id: &Value, err: &RpcErr) -> String {
    let mut error = json!({ "code": err.code, "message": err.message });
    if let Some(app) = &err.app {
        error["data"] = json!({ "code": app });
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error }).to_string()
}

/// Invalid JSON: an error response with `id: null` (JSON-RPC 2.0, §5).
pub fn parse_error() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32700, "message": "parse error" },
    })
    .to_string()
}

/// A notification (without id) ready to write.
pub fn notification(method: &str, params: &Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

/// A required string param, otherwise -32602.
pub fn required_str(params: &Value, key: &str) -> Result<String, RpcErr> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RpcErr::invalid_params(key))
}

/// An optional param: absent or null → None; present but non-string → -32602.
pub fn optional_str(params: &Value, key: &str) -> Result<Option<String>, RpcErr> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(RpcErr::invalid_params(key)),
    }
}

/// The fields that are stored and later rebroadcast (name, version…) are
/// bounded at the input: bounds both the memory and the traffic.
pub fn required_str_max(params: &Value, key: &str, max: usize) -> Result<String, RpcErr> {
    let value = required_str(params, key)?;
    if value.len() > max {
        return Err(RpcErr::invalid_params(key));
    }
    Ok(value)
}

/// A required param: an array of strings, otherwise -32602.
pub fn required_str_array(params: &Value, key: &str) -> Result<Vec<String>, RpcErr> {
    params
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        })
        .and_then(|v| v)
        .ok_or_else(|| RpcErr::invalid_params(key))
}
