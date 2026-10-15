use super::ExecCommandHandler;
use super::ExecCommandHandlerOptions;
use crate::tools::registry::ToolExecutor;
use codex_extension_api::AsyncNotificationSupport;
use pretty_assertions::assert_eq;
use serde_json::json;

// Wire-contract pin against upstream 0462dcc: the server rejects reserved tool
// schema drift. Testing static values is intentional here; changes require a
// deliberate wire-contract decision, not a snapshot refresh. Exercise the
// one-shot handler's advertised spec, including its timeout transformation.
#[test]
fn one_shot_exec_command_reserved_parameters_match_upstream_wire_contract() {
    let handler = ExecCommandHandler::one_shot(ExecCommandHandlerOptions {
        allow_login_shell: true,
        allow_tty: true,
        exec_permission_approvals_enabled: false,
        include_environment_id: false,
        include_shell_parameter: true,
        include_windows_shell_guidance: false,
        async_notifications: AsyncNotificationSupport::Unavailable,
    });
    let tool =
        serde_json::to_value(handler.spec()).expect("one-shot exec_command should serialize");
    let parameters = &tool["parameters"];
    assert_eq!(
        parameters,
        &json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string", "description": "Shell command to execute."},
                "workdir": {
                    "type": "string",
                    "description": "Working directory for the command. Defaults to the turn cwd."
                },
                "shell": {
                    "type": "string",
                    "description": "Shell binary to launch. Defaults to the user's default shell."
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "Maximum command runtime. Defaults to 10000 ms."
                },
                "max_output_tokens": {
                    "type": "number",
                    "description": "Output token budget. Defaults to 10000 tokens; larger requests may be capped by policy."
                },
                "login": {
                    "type": "boolean",
                    "description": "True runs the shell with -l/-i semantics; false disables them. Defaults to true."
                },
                "sandbox_permissions": {
                    "type": "string",
                    "enum": ["use_default", "require_escalated"],
                    "description": "Per-command sandbox override. Defaults to `use_default`; use `require_escalated` for unsandboxed execution."
                },
                "justification": {
                    "type": "string",
                    "description": "User-facing approval question for `require_escalated`; omit otherwise."
                },
                "prefix_rule": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Reusable approval prefix for `cmd`, only with `sandbox_permissions: \"require_escalated\"`; for example [\"git\", \"pull\"]."
                }
            },
            "required": ["cmd"],
            "additionalProperties": false
        })
    );
    for parameter in ["timeout_ms", "max_output_tokens"] {
        assert_ne!(parameters["properties"][parameter]["type"], "integer");
    }
}
