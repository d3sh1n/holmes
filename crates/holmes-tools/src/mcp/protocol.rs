use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse {
    pub id: u64,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        }
    }

    pub fn initialize(id: u64) -> Self {
        Self::new(
            id,
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "apeiron", "version": "3.0.0" }
            })),
        )
    }

    pub fn tools_list(id: u64) -> Self {
        Self::new(id, "tools/list", None)
    }

    pub fn tools_call(id: u64, name: &str, arguments: Value) -> Self {
        Self::new(
            id,
            "tools/call",
            Some(serde_json::json!({
                "name": name,
                "arguments": arguments,
            })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_request_format() {
        let req = JsonRpcRequest::initialize(1);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "initialize");
        assert_eq!(json["id"], 1);
    }

    #[test]
    fn tools_call_request_format() {
        let req =
            JsonRpcRequest::tools_call(2, "nmap_scan", serde_json::json!({"target": "10.0.0.1"}));
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["method"], "tools/call");
        assert_eq!(json["params"]["name"], "nmap_scan");
    }

    #[test]
    fn parse_success_response() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn parse_error_response() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }
}
