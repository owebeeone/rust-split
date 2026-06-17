use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use gwz_core::{
    ActionKind, AggregateStatus, EventKind, GitBranchDifference, GitBranchGroup, GitFileChange,
    GitMemberBranchStatus, GitObjectIdentity, GwzError, GwzErrorCode, MemberResponse, MemberStatus,
    OperationActor, OperationAttribution, OperationEvent, RequestMeta, ResponseEnvelope,
    ResponseMeta, Severity, SourceKind, StatusMode, StatusPathStyle, StatusRequest, StatusResponse,
    WorkspaceGitStatus, WorkspaceRootFileChange, WorkspaceRootGitStatus, decode, encode,
};

fn round_trip<T>(
    value: &T,
    to_cbor: impl Fn(&T) -> gwz_core::Cbor,
    from_cbor: impl Fn(&gwz_core::Cbor) -> T,
) -> T {
    from_cbor(&decode(&encode(&to_cbor(value))))
}

#[test]
fn status_request_round_trips() {
    let request = StatusRequest {
        meta: RequestMeta {
            request_id: "req-1".to_owned(),
            schema_version: "gwz.v0".to_owned(),
            attribution: Some(attribution()),
            ..RequestMeta::default()
        },
        mode: Some(StatusMode::Combined),
        include_file_changes: Some(true),
        include_branch_summary: Some(true),
        path_style: Some(StatusPathStyle::WorkspaceRelative),
    };

    assert_eq!(
        round_trip(&request, StatusRequest::to_cbor, StatusRequest::from_cbor),
        request
    );
}

#[test]
fn status_response_round_trips_combined_workspace_status() {
    let response = StatusResponse {
        response: ResponseEnvelope {
            meta: ResponseMeta {
                request_id: "req-1".to_owned(),
                schema_version: "gwz.v0".to_owned(),
                action: ActionKind::Status,
                aggregate_status: AggregateStatus::Ok,
                ..ResponseMeta::default()
            },
            members: Vec::new(),
            errors: Vec::new(),
        },
        workspace_git_status: Some(WorkspaceGitStatus {
            clean: false,
            root_status: Some(WorkspaceRootGitStatus {
                branch: Some("main".to_owned()),
                detached: false,
                head: Some("def456".to_owned()),
                staged: 2,
                unstaged: 0,
                untracked: 0,
                dirty: true,
                unborn: false,
            }),
            root_file_changes: vec![WorkspaceRootFileChange {
                repo_path: "gwz.conf/gwz.yml".to_owned(),
                workspace_path: "gwz.conf/gwz.yml".to_owned(),
                index_status: "A".to_owned(),
                worktree_status: " ".to_owned(),
                original_repo_path: None,
            }],
            file_changes: vec![GitFileChange {
                member_id: "mem_core".to_owned(),
                member_path: "repos/core".to_owned(),
                repo_path: "src/lib.rs".to_owned(),
                workspace_path: "repos/core/src/lib.rs".to_owned(),
                index_status: " ".to_owned(),
                worktree_status: "M".to_owned(),
                original_repo_path: None,
            }],
            branches: vec![GitMemberBranchStatus {
                member_id: "mem_core".to_owned(),
                member_path: "repos/core".to_owned(),
                label: "main".to_owned(),
                branch: Some("main".to_owned()),
                detached: false,
                unborn: false,
                head: Some("abc123".to_owned()),
                upstream: Some("origin/main".to_owned()),
                ahead: Some(1),
                behind: Some(0),
            }],
            branch_groups: vec![GitBranchGroup {
                label: "main".to_owned(),
                member_ids: vec!["mem_core".to_owned()],
                member_paths: vec!["repos/core".to_owned()],
            }],
            branch_differences: vec![GitBranchDifference {
                label: "feature/app".to_owned(),
                majority_label: Some("main".to_owned()),
                member_ids: vec!["mem_app".to_owned()],
                member_paths: vec!["repos/app".to_owned()],
                message: Some("repos/app differs from majority branch main".to_owned()),
            }],
        }),
    };

    assert_eq!(
        round_trip(
            &response,
            StatusResponse::to_cbor,
            StatusResponse::from_cbor
        ),
        response
    );
}

#[test]
fn response_envelope_round_trips_with_member_error() {
    let response = ResponseEnvelope {
        meta: ResponseMeta {
            request_id: "req-1".to_owned(),
            schema_version: "gwz.v0".to_owned(),
            action: ActionKind::Status,
            aggregate_status: AggregateStatus::Rejected,
            message: Some("workspace has errors".to_owned()),
            attribution: Some(attribution()),
            ..ResponseMeta::default()
        },
        members: vec![MemberResponse {
            member_id: "core".to_owned(),
            member_path: "libs/core".to_owned(),
            source_kind: SourceKind::Git,
            status: MemberStatus::Rejected,
            error: Some(member_error()),
            ..MemberResponse::default()
        }],
        errors: vec![member_error()],
    };

    assert_eq!(
        round_trip(
            &response,
            ResponseEnvelope::to_cbor,
            ResponseEnvelope::from_cbor
        ),
        response
    );
}

#[test]
fn operation_event_round_trips_with_attribution() {
    let event = OperationEvent {
        operation_id: "op-1".to_owned(),
        request_id: "req-1".to_owned(),
        sequence: 42,
        timestamp_ms: 1_727_000_000_000,
        kind: EventKind::MemberFinished,
        severity: Severity::Warn,
        member_id: Some("core".to_owned()),
        member_path: Some("libs/core".to_owned()),
        message: Some("member rejected".to_owned()),
        error: Some(member_error()),
        attribution: Some(attribution()),
        ..OperationEvent::default()
    };

    assert_eq!(
        round_trip(&event, OperationEvent::to_cbor, OperationEvent::from_cbor),
        event
    );
}

#[test]
fn attribution_round_trips_actor_and_git_identities_separately() {
    let attribution = attribution();

    assert_eq!(
        round_trip(
            &attribution,
            OperationAttribution::to_cbor,
            OperationAttribution::from_cbor
        ),
        attribution
    );
}

#[test]
fn error_code_wire_values_are_pinned() {
    assert_eq!(GwzErrorCode::Ok.wire(), 0);
    assert_eq!(GwzErrorCode::InvalidRequest.wire(), 1);
    assert_eq!(GwzErrorCode::DivergedMember.wire(), 16);
    assert_eq!(GwzErrorCode::AttributionDenied.wire(), 26);
    assert_eq!(GwzErrorCode::InternalError.wire(), 29);
}

#[test]
fn generated_protocol_is_current() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("gwz-taut-gen-{}", std::process::id()));
    let _ = fs::remove_dir_all(&out_dir);

    let status = taut_command(&root)
        .args([
            "gen",
            "protocol/gwz.taut.py",
            "-o",
            out_dir.to_str().expect("temp path is not utf-8"),
            "-l",
            "rust",
            "--api-only",
            "--with-runtime",
        ])
        .status()
        .expect("failed to run taut generator");
    assert!(status.success(), "taut generator failed");

    assert_same(
        &root.join("src/protocol/generated.rs"),
        &out_dir.join("rust/api.rs"),
    );
    assert_same(&root.join("src/cbor.rs"), &out_dir.join("rust/cbor.rs"));

    let status = taut_command(&root)
        .args([
            "corpus",
            "protocol/gwz.taut.py",
            "-o",
            "protocol/corpus",
            "-l",
            "rust",
            "--check",
        ])
        .status()
        .expect("failed to run taut corpus check");
    assert!(status.success(), "taut corpus is stale");

    fs::remove_dir_all(&out_dir).expect("failed to clean generated protocol temp dir");
}

#[test]
fn protocol_schema_uses_keyword_message_dsl() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let schema = fs::read_to_string(root.join("protocol/gwz.taut.py"))
        .expect("failed to read protocol/gwz.taut.py");

    assert!(
        !schema.contains("Msg(\""),
        "protocol/gwz.taut.py should name messages with schema keywords"
    );
    assert!(
        !schema.contains("Enum(\""),
        "protocol/gwz.taut.py should name enums with schema keywords"
    );
    assert!(
        !schema.contains("F(\""),
        "protocol/gwz.taut.py should name fields with Msg keywords"
    );
    assert!(
        !schema.contains("Ref(\""),
        "protocol/gwz.taut.py should reference messages and enums with Ref attributes"
    );
    assert!(
        !schema.contains("params=[("),
        "protocol/gwz.taut.py should name method params with Params keywords"
    );
}

#[test]
fn taut_command_can_use_configured_python_executable() {
    let command = taut_command_for_python(Path::new("/tmp/gwz-core"), "python");

    assert_eq!(command.get_program().to_string_lossy(), "python");
}

#[test]
fn taut_command_forces_utf8_for_generated_source_files() {
    let command = taut_command_for_python(Path::new("/tmp/gwz-core"), "python");

    assert_command_env(&command, "PYTHONUTF8", "1");
    assert_command_env(&command, "PYTHONIOENCODING", "utf-8");
}

fn attribution() -> OperationAttribution {
    OperationAttribution {
        actor: Some(OperationActor {
            actor_id: "agent://gryth/dev".to_owned(),
            display_name: Some("Gryth Agent".to_owned()),
            email: Some("agent@example.invalid".to_owned()),
            authority: Some("local-test".to_owned()),
        }),
        git_author: Some(GitObjectIdentity {
            name: "AI Agent".to_owned(),
            email: "agent@example.invalid".to_owned(),
            time_ms: Some(1_727_000_000_000),
            timezone_offset_minutes: Some(600),
        }),
        git_committer: Some(GitObjectIdentity {
            name: "Workspace Bot".to_owned(),
            email: "workspace@example.invalid".to_owned(),
            time_ms: Some(1_727_000_000_100),
            timezone_offset_minutes: Some(600),
        }),
        credential_ref: Some("cred:test".to_owned()),
    }
}

fn member_error() -> GwzError {
    GwzError {
        code: GwzErrorCode::DivergedMember,
        message: "member diverged".to_owned(),
        member_id: Some("core".to_owned()),
        member_path: Some("libs/core".to_owned()),
        detail: Some("HEAD and upstream have distinct commits".to_owned()),
    }
}

fn taut_command(root: &Path) -> Command {
    let python = std::env::var("TAUT_PYTHON").unwrap_or_else(|_| "python3".to_owned());
    taut_command_for_python(root, &python)
}

fn taut_command_for_python(root: &Path, python: &str) -> Command {
    let mut command = Command::new(python);
    let taut_src = root
        .parent()
        .expect("gwz-core should have a parent")
        .join("taut/src");
    command
        .current_dir(root)
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONPATH", taut_src)
        .args(["-m", "taut.cli"]);
    command
}

fn assert_command_env(command: &Command, key: &str, expected: &str) {
    let actual = command
        .get_envs()
        .find_map(|(name, value)| (name.to_string_lossy() == key).then_some(value))
        .flatten()
        .map(|value| value.to_string_lossy().into_owned());

    assert_eq!(actual.as_deref(), Some(expected));
}

fn assert_same(committed: &Path, generated: &Path) {
    let committed_text = fs::read_to_string(committed)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", committed.display()));
    let generated_text = fs::read_to_string(generated)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", generated.display()));
    assert_eq!(
        committed_text,
        generated_text,
        "{} is stale",
        committed.display()
    );
}
