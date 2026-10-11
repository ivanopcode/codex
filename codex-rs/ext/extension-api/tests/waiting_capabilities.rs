use codex_extension_api::ToolName;
use codex_extension_api::WaitingCapabilities;
use pretty_assertions::assert_eq;
use pretty_assertions::assert_ne;

#[test]
fn none_disables_every_flag() {
    assert_eq!(
        WaitingCapabilities::none(),
        WaitingCapabilities {
            sleep: false,
            stdin: false,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        }
    );
}

#[test]
fn snapshot_compares_by_value() {
    let left = WaitingCapabilities {
        sleep: true,
        stdin: true,
        native_wait: Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
        exec_notify: true,
        native_mail_wait: true,
    };
    let right = WaitingCapabilities {
        sleep: true,
        stdin: true,
        native_wait: Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
        exec_notify: true,
        native_mail_wait: true,
    };
    assert_eq!(left, right);
    assert_ne!(left, WaitingCapabilities::none());
}

#[test]
fn snapshot_stays_small_and_bounded() {
    assert!(
        std::mem::size_of::<WaitingCapabilities>() <= 128,
        "waiting snapshot must stay a small bounded struct, got {} bytes",
        std::mem::size_of::<WaitingCapabilities>()
    );
}
