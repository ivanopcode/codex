use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

// Wire-contract pin against upstream 0462dcc: the server rejects reserved tool
// schema drift. Testing static values is intentional here; changes require a
// deliberate wire-contract decision, not a snapshot refresh.
#[test]
fn sleep_reserved_parameters_match_upstream_wire_contract() {
    let tool = serde_json::to_value(SleepHandler.spec()).expect("clock.sleep should serialize");
    let parameters = &tool["tools"][0]["parameters"];
    assert_eq!(
        parameters,
        &json!({
            "type": "object",
            "properties": {
                "duration_ms": {
                    "type": "number",
                    "description": "How long to sleep in milliseconds. Must be between 1 and 43200000."
                }
            },
            "required": ["duration_ms"],
            "additionalProperties": false
        })
    );
    assert_ne!(parameters["properties"]["duration_ms"]["type"], "integer");
}
