use pretty_assertions::assert_eq;
use serde_json::json;

use super::CommandExecOutcome;
use super::SandboxCommandExecParams;

#[test]
fn command_exec_deserializes_local_additional_permissions_extension() {
    let params = serde_json::from_value::<SandboxCommandExecParams>(json!({
        "command": ["sh", "-c", "printf ok"],
        "processId": "permission-test",
        "additionalPermissions": {
            "network": null,
            "fileSystem": {
                "read": null,
                "write": ["/opt/example"]
            }
        }
    }))
    .expect("deserialize command/exec params");

    assert_eq!(
        params.command_exec.command,
        vec!["sh".to_string(), "-c".to_string(), "printf ok".to_string()]
    );
    assert_eq!(
        serde_json::to_value(params.additional_permissions)
            .expect("serialize additional permissions"),
        json!({
            "network": null,
            "fileSystem": {
                "read": null,
                "write": ["/opt/example"],
            }
        })
    );
}

#[test]
fn command_exec_outcomes_share_the_same_payload_shape() {
    let completed = serde_json::to_value(CommandExecOutcome::Completed {
        exit_code: 0,
        stdout: "ok".to_string(),
        stderr: String::new(),
    })
    .expect("serialize completed outcome");
    let denied = serde_json::to_value(CommandExecOutcome::SandboxDenied {
        exit_code: 1,
        stdout: String::new(),
        stderr: "permission denied".to_string(),
    })
    .expect("serialize denied outcome");

    assert_eq!(
        completed,
        json!({
            "type": "completed",
            "exitCode": 0,
            "stdout": "ok",
            "stderr": "",
        })
    );
    assert_eq!(
        denied,
        json!({
            "type": "sandboxDenied",
            "exitCode": 1,
            "stdout": "",
            "stderr": "permission denied",
        })
    );
}
