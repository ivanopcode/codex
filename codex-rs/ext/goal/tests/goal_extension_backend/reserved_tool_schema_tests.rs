use super::installed_tools;
use super::test_runtime;
use super::test_thread_id;
use super::tool_by_name;
use pretty_assertions::assert_eq;
use serde_json::json;

// Wire-contract pin against upstream 0462dcc: the server rejects reserved tool
// schema drift. Testing static values is intentional here; changes require a
// deliberate wire-contract decision, not a snapshot refresh. Unlike waiting
// tools, create_goal already advertised token_budget as integer upstream.
#[tokio::test]
async fn create_goal_reserved_parameters_match_upstream_wire_contract() -> anyhow::Result<()> {
    let runtime = test_runtime().await?;
    let tools = installed_tools(runtime, test_thread_id()?).await;
    let tool = serde_json::to_value(tool_by_name(&tools, "create_goal").spec())?;
    let parameters = &tool["parameters"];
    assert_eq!(
        parameters,
        &json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "Required. The concrete objective to start pursuing. This starts a new active goal when no goal exists or replaces the current goal when it is complete."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Positive token budget for the new goal. Omit unless explicitly requested."
                }
            },
            "required": ["objective"],
            "additionalProperties": false
        })
    );
    assert_ne!(parameters["properties"]["token_budget"]["type"], "number");
    Ok(())
}
