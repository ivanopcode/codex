use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_extension_api::ContentItemKind;
use codex_extension_api::PromptFragment;
use codex_extension_api::ToolName;
use codex_extension_api::WaitingCapabilities;
use codex_prompts::without_update_plan_instructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadGoal;
use codex_utils_template::Template;
use std::sync::LazyLock;

static CONTINUATION_PROMPT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_embedded_template(
        include_str!("../templates/goals/continuation.md"),
        "goals/continuation.md",
    )
});

static CONTINUATION_PROMPT_WITHOUT_UPDATE_PLAN: LazyLock<Template> = LazyLock::new(|| {
    parse_embedded_template(
        &without_update_plan_instructions(include_str!("../templates/goals/continuation.md")),
        "goals/continuation.md",
    )
});

static BUDGET_LIMIT_PROMPT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_embedded_template(
        include_str!("../templates/goals/budget_limit.md"),
        "goals/budget_limit.md",
    )
});

static OBJECTIVE_UPDATED_PROMPT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_embedded_template(
        include_str!("../templates/goals/objective_updated.md"),
        "goals/objective_updated.md",
    )
});

fn parse_embedded_template(source: &str, template_name: &str) -> Template {
    match Template::parse(source) {
        Ok(template) => template,
        Err(err) => panic!("embedded template {template_name} is invalid: {err}"),
    }
}

pub(crate) fn budget_limit_steering_item(goal: &ThreadGoal) -> ResponseItem {
    goal_context_input_item(budget_limit_prompt(goal))
}

pub(crate) fn objective_updated_steering_item(goal: &ThreadGoal) -> ResponseItem {
    goal_context_input_item(objective_updated_prompt(goal))
}

pub(crate) fn continuation_steering_item(
    goal: &ThreadGoal,
    update_plan_enabled: bool,
) -> ResponseItem {
    goal_context_input_item(continuation_prompt(goal, update_plan_enabled))
}

/// Byte-exact waiting-guidance sentences from final-plan §9.
///
/// Rust selects these static strings from the sampling-step
/// [`WaitingCapabilities`] snapshot; no new template syntax is involved. The
/// §9 "none of the positive flags" no-wait/end-turn paragraph is NOT emitted
/// here: plan §10 defers it until P4b pacing is active.
pub(crate) const WAITING_EXEC_NOTIFY: &str = "exec_command can acknowledge notify_on_exit subscriptions. For finite background work, request a notification; after the tool acknowledges it, do independent work and then end the turn. Its completion will be delivered by the runtime. Use write_stdin only for needed intermediate output or interaction. Use exec_notification to read retained terminal output or release a mistaken subscription without terminating the process.";
pub(crate) const WAITING_NO_EXEC_NOTIFY: &str = "This session cannot promise an exec completion wake. A yielded exec session by itself is not a reason to expect automatic resumption.";
pub(crate) const WAITING_SLEEP: &str = "clock.sleep is available as a direct tool. Use a substantial input-interruptible wait when a notification subscription is unavailable. Do not place clock.sleep inside a code-mode exec cell.";
pub(crate) const WAITING_STDIN: &str = "An empty write_stdin with a long yield_time_ms can wait for one known live exec session. Read again early only when intermediate output is needed to decide the next action.";
pub(crate) const WAITING_NATIVE_WAIT: &str = "The native agent wait tool is available for Codex-managed children. Pass multiple child identifiers where that tool supports them to wait for whichever changes first. It cannot observe arbitrary shell or remote jobs.";
pub(crate) const WAITING_NATIVE_MAIL_WAIT: &str = "When this active goal yields with owned native child work still running, the runtime can wait for child mailbox results. Do not repeatedly inspect child logs merely to keep the goal alive.";

/// Hard cap on the rendered native wait tool name, in characters.
///
/// The name is the only variable datum in the fragment; everything else is
/// static §9 text. Truncation appends `…` so a capped name is recognizable.
pub(crate) const MAX_NATIVE_WAIT_TOOL_NAME_CHARS: usize = 128;

/// Goal-specific waiting guidance for this sampling step.
///
/// Renders exactly one [`PromptFragment`] in the developer-capabilities slot
/// from this step's [`WaitingCapabilities`] snapshot, in fixed §9 order:
/// exactly one of the exec sentences, then the sleep, stdin, native wait
/// (with the snapshot's actual tool name), and native mail wait sentences
/// for each set flag. The caller (goal's `ContextContributor`) emits nothing
/// unless the thread's goal is active, so this renderer always returns one
/// fragment.
pub(crate) fn waiting_capability_fragments(
    capabilities: &WaitingCapabilities,
) -> Vec<PromptFragment> {
    let mut sentences = Vec::with_capacity(5);
    sentences.push(WAITING_EXEC_NOTIFY.to_string());
    if capabilities.sleep {
        sentences.push(WAITING_SLEEP.to_string());
    }
    if capabilities.stdin {
        sentences.push(WAITING_STDIN.to_string());
    }
    if let Some(native_wait) = capabilities.native_wait.as_ref() {
        sentences.push(format!(
            "{WAITING_NATIVE_WAIT} (Tool name: {}.)",
            render_native_wait_tool_name(native_wait)
        ));
    }
    if capabilities.native_mail_wait {
        sentences.push(WAITING_NATIVE_MAIL_WAIT.to_string());
    }
    vec![PromptFragment::developer_capability(
        sentences.join("\n"),
        ContentItemKind("goal.waiting_capabilities".to_string()),
    )]
}

/// Renders the snapshot's actual native wait tool name, bounded by
/// [`MAX_NATIVE_WAIT_TOOL_NAME_CHARS`].
///
/// Namespaced tools use the dotted `namespace.name` form; default-namespace
/// tools render as the bare name. Overlong names truncate at a character
/// boundary with a trailing `…`.
fn render_native_wait_tool_name(name: &ToolName) -> String {
    let tool = &name.name;
    let display = match name.namespace.as_deref() {
        Some(namespace) if !name.is_default_namespace() => format!("{namespace}.{tool}"),
        _ => tool.clone(),
    };
    if display.chars().count() <= MAX_NATIVE_WAIT_TOOL_NAME_CHARS {
        return display;
    }
    let truncated: String = display
        .chars()
        .take(MAX_NATIVE_WAIT_TOOL_NAME_CHARS)
        .collect();
    format!("{truncated}…")
}

fn goal_context_input_item(prompt: String) -> ResponseItem {
    ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("goal"),
        prompt,
    ))
}

fn continuation_prompt(goal: &ThreadGoal, update_plan_enabled: bool) -> String {
    let objective = escape_xml_text(&goal.objective);
    let tokens_used = goal.tokens_used.to_string();
    let token_budget = goal
        .token_budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "none".to_string());
    let remaining_tokens = goal
        .token_budget
        .map(|budget| (budget - goal.tokens_used).max(0).to_string())
        .unwrap_or_else(|| "unbounded".to_string());

    let template = if update_plan_enabled {
        &*CONTINUATION_PROMPT_TEMPLATE
    } else {
        &*CONTINUATION_PROMPT_WITHOUT_UPDATE_PLAN
    };
    template
        .render([
            ("objective", objective.as_str()),
            ("tokens_used", tokens_used.as_str()),
            ("token_budget", token_budget.as_str()),
            ("remaining_tokens", remaining_tokens.as_str()),
        ])
        .unwrap_or_else(|err| {
            panic!("embedded goals/continuation.md template failed to render: {err}")
        })
}

fn budget_limit_prompt(goal: &ThreadGoal) -> String {
    let objective = escape_xml_text(&goal.objective);
    let time_used_seconds = goal.time_used_seconds.to_string();
    let tokens_used = goal.tokens_used.to_string();
    let token_budget = goal
        .token_budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "none".to_string());

    BUDGET_LIMIT_PROMPT_TEMPLATE
        .render([
            ("objective", objective.as_str()),
            ("time_used_seconds", time_used_seconds.as_str()),
            ("tokens_used", tokens_used.as_str()),
            ("token_budget", token_budget.as_str()),
        ])
        .unwrap_or_else(|err| {
            panic!("embedded goals/budget_limit.md template failed to render: {err}")
        })
}

fn objective_updated_prompt(goal: &ThreadGoal) -> String {
    let objective = escape_xml_text(&goal.objective);
    let tokens_used = goal.tokens_used.to_string();
    let (token_budget, remaining_tokens) = match goal.token_budget {
        Some(token_budget) => (
            token_budget.to_string(),
            (token_budget - goal.tokens_used).max(0).to_string(),
        ),
        None => ("none".to_string(), "unknown".to_string()),
    };

    OBJECTIVE_UPDATED_PROMPT_TEMPLATE
        .render([
            ("objective", objective.as_str()),
            ("tokens_used", tokens_used.as_str()),
            ("token_budget", token_budget.as_str()),
            ("remaining_tokens", remaining_tokens.as_str()),
        ])
        .unwrap_or_else(|err| {
            panic!("embedded goals/objective_updated.md template failed to render: {err}")
        })
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
