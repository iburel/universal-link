// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! JSON-RPC 2.0 grammar: responses, errors, param extraction.

use serde_json::{Value, json};

pub struct RpcErr {
    pub code: i64,
    pub message: String,
    /// Application code, placed in `error.data.code`.
    pub app: Option<&'static str>,
}

impl RpcErr {
    /// Application error: generic JSON-RPC code, business code in data.
    pub fn app(code: &'static str) -> RpcErr {
        RpcErr {
            code: -32000,
            message: code.replace('_', " ").to_lowercase(),
            app: Some(code),
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
    if let Some(app) = err.app {
        error["data"] = json!({ "code": app });
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error }).to_string()
}

/// Invalid JSON: error response with `id: null` (JSON-RPC 2.0, §5).
pub fn parse_error() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32700, "message": "parse error" },
    })
    .to_string()
}

/// Required string param, otherwise -32602.
pub fn required_str(params: &Value, key: &str) -> Result<String, RpcErr> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RpcErr::invalid_params(key))
}

/// Optional param: absent or null → None; present but not a string → -32602.
pub fn optional_str(params: &Value, key: &str) -> Result<Option<String>, RpcErr> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(RpcErr::invalid_params(key)),
    }
}

// The fields kept in the directory are rebroadcast in every device record:
// bounding them at the entry point also bounds memory and traffic.

pub fn required_str_max(params: &Value, key: &str, max: usize) -> Result<String, RpcErr> {
    let value = required_str(params, key)?;
    if value.len() > max {
        return Err(RpcErr::invalid_params(key));
    }
    Ok(value)
}

pub fn optional_str_max(params: &Value, key: &str, max: usize) -> Result<Option<String>, RpcErr> {
    let value = optional_str(params, key)?;
    if value.as_ref().is_some_and(|v| v.len() > max) {
        return Err(RpcErr::invalid_params(key));
    }
    Ok(value)
}
