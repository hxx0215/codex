use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_sandboxing::policy_transforms::effective_permission_profile;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

#[test]
fn effective_profile_preserves_base_restrictions_and_adds_write() {
    let readable = AbsolutePathBuf::from_absolute_path("/workspace").expect("absolute read path");
    let writable = AbsolutePathBuf::from_absolute_path("/opt/output").expect("absolute write path");
    let base_entries = vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: readable },
            access: FileSystemAccessMode::Read,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern {
                pattern: "**/.env".to_string(),
            },
            access: FileSystemAccessMode::Deny,
        },
    ];
    let base_policy = FileSystemSandboxPolicy::restricted(base_entries.clone());
    let base =
        PermissionProfile::from_runtime_permissions(&base_policy, NetworkSandboxPolicy::Restricted);
    let write_entry = FileSystemSandboxEntry {
        path: FileSystemPath::Path { path: writable },
        access: FileSystemAccessMode::Write,
    };
    let additional_permissions = AdditionalPermissionProfile {
        network: None,
        file_system: Some(FileSystemPermissions {
            entries: vec![write_entry.clone()],
            glob_scan_max_depth: None,
        }),
    };

    let effective = effective_permission_profile(&base, Some(&additional_permissions));
    let (file_system, network) = effective.to_runtime_permissions();
    let mut expected_entries = base_entries;
    expected_entries.push(write_entry);
    assert_eq!(
        (file_system, network),
        (
            FileSystemSandboxPolicy::restricted(expected_entries),
            NetworkSandboxPolicy::Restricted,
        )
    );
}
