

fn error_json(status: u16, message: &str, err_type: Option<String>) -> serde_json::Value {
    let err_type = err_type.unwrap_or_else(|| "invalid_request_error".to_string());
    serde_json::json!({"error": {"message": message, "type": err_type, "code": status}})
}