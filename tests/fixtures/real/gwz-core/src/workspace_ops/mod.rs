use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::artifact::{
    self, ArtifactSourceKind, CreatedByArtifact, DesiredRefArtifact, LockArtifact,
    ManifestArtifact, ManifestMember, RemoteArtifact, ResolvedMemberArtifact, WorkspaceHeader,
};
use crate::git::{Git2Backend, GitBackend, GitHeadState, GitStatus, git_host};
use crate::model::{ErrorCode, MemberId, ModelError, ModelResult, SourceId};
use crate::operation::{
    EventEmitter, EventSink, NullSink, OperationRequest, par_map_per_host, resolve_jobs,
    resolve_per_host,
};
use crate::workspace::{
    MemberPath, WORKSPACE_DIR, WORKSPACE_MANIFEST, discover_workspace_root,
    preflight_create_workspace, validate_member_path_set,
};

const GITIGNORE_GWZ_BEGIN: &str = "# BEGIN GWZ managed member repositories";
const GITIGNORE_GWZ_END: &str = "# END GWZ managed member repositories";

pub fn handle_create_workspace(
    request: crate::CreateWorkspaceRequest,
    operation_id: impl Into<String>,
) -> ModelResult<crate::CreateWorkspaceResponse> {
    let context =
        OperationRequest::CreateWorkspace(request.clone()).context(operation_id.into())?;
    let root = PathBuf::from(&request.workspace_root);
    preflight_create_workspace(&root)?;
    let workspace_id = request
        .workspace_id
        .clone()
        .unwrap_or_else(|| "ws_default".to_owned());
    crate::model::WorkspaceId::parse_str(&workspace_id)?;
    ensure_workspace_git_repo(&root)?;

    artifact::write_manifest(
        &root,
        &ManifestArtifact {
            schema: artifact::WORKSPACE_SCHEMA.to_owned(),
            workspace: WorkspaceHeader {
                id: workspace_id.clone(),
            },
            members: Vec::new(),
        },
    )?;
    artifact::write_lock(
        &root,
        &LockArtifact {
            schema: artifact::LOCK_SCHEMA.to_owned(),
            workspace_id,
            manifest_schema: artifact::WORKSPACE_SCHEMA.to_owned(),
            created_at: now_marker(),
            members: BTreeMap::new(),
        },
    )?;
    sync_workspace_git_metadata(&root, &[])?;

    Ok(crate::CreateWorkspaceResponse {
        response: response_envelope(context, crate::AggregateStatus::Ok, Vec::new()),
    })
}

pub fn handle_create_repo<B>(
    backend: &B,
    start: &Path,
    request: crate::CreateRepoRequest,
    operation_id: impl Into<String>,
) -> ModelResult<crate::CreateRepoResponse>
where
    B: GitBackend,
{
    let context = OperationRequest::CreateRepo(request.clone()).context(operation_id.into())?;
    if request
        .initial_branch
        .as_ref()
        .is_some_and(|branch| branch != "main")
    {
        return Err(ModelError::new(
            ErrorCode::UnsupportedOperation,
            "custom initial branches are not supported in v0",
        ));
    }

    let root = resolve_workspace_root(start, request.meta.workspace.as_ref())?;
    let mut manifest = artifact::read_manifest(&root)?;
    assert_workspace_id(&manifest, request.meta.workspace.as_ref())?;
    let member_path = MemberPath::parse(&request.member_path)?;
    reject_existing_member_path(&manifest, &member_path)?;
    let member_abs_path = root.join(member_path.as_str());
    ensure_member_target_available(&member_abs_path)?;

    let slug = path_slug(member_path.as_str())?;
    let member_id = request.member_id.unwrap_or_else(|| format!("mem_{slug}"));
    let source_id = request.source_id.unwrap_or_else(|| format!("src_{slug}"));
    MemberId::parse_str(&member_id)?;
    SourceId::parse_str(&source_id)?;
    reject_duplicate_member_id(&manifest, &member_id)?;

    backend.create_repo(&member_abs_path)?;
    let head = backend.head(&member_abs_path)?;
    let status = backend.status(&member_abs_path)?;
    let remotes = backend.remotes(&member_abs_path)?;

    let manifest_member = ManifestMember {
        id: member_id.clone(),
        path: member_path.as_str().to_owned(),
        source_kind: ArtifactSourceKind::Git,
        source_id: source_id.clone(),
        active: true,
        desired: Some(DesiredRefArtifact {
            local_only: Some(true),
            ..Default::default()
        }),
        remotes: remotes
            .iter()
            .map(|remote| RemoteArtifact {
                name: remote.name.clone(),
                url: remote.url.clone().unwrap_or_default(),
                fetch: true,
                push: true,
            })
            .collect(),
    };
    manifest.members.push(manifest_member.clone());
    let paths = manifest
        .members
        .iter()
        .map(|member| MemberPath::parse(&member.path))
        .collect::<ModelResult<Vec<_>>>()?;
    validate_member_path_set(&paths)?;
    artifact::write_manifest(&root, &manifest)?;

    let mut lock = read_lock_or_empty(&root, &manifest.workspace.id)?;
    let locked = resolved_member(&manifest_member, &head, &status);
    lock.members.insert(member_id.clone(), locked.clone());
    lock.created_at = now_marker();
    artifact::write_lock(&root, &lock)?;
    sync_workspace_git_metadata(&root, &manifest.members)?;

    Ok(crate::CreateRepoResponse {
        response: response_envelope(
            context,
            crate::AggregateStatus::Ok,
            vec![crate::MemberResponse {
                member_id,
                member_path: manifest_member.path.clone(),
                source_kind: crate::SourceKind::Git,
                status: crate::MemberStatus::Ok,
                error: None,
                planned: None,
                state: Some(protocol_state(&manifest_member, &locked)),
                git_status: None,
                lock_match: Some(crate::LockMatch::Matches),
            }],
        ),
    })
}

pub fn handle_add_existing_repo<B>(
    backend: &B,
    start: &Path,
    request: crate::AddExistingRepoRequest,
    operation_id: impl Into<String>,
) -> ModelResult<crate::AddExistingRepoResponse>
where
    B: GitBackend,
{
    let context =
        OperationRequest::AddExistingRepo(request.clone()).context(operation_id.into())?;
    let root = resolve_workspace_root(start, request.meta.workspace.as_ref())?;
    let mut manifest = artifact::read_manifest(&root)?;
    assert_workspace_id(&manifest, request.meta.workspace.as_ref())?;
    let repo_path = resolve_input_path(start, &request.repository_path);
    if !backend.is_repository(&repo_path)? {
        return Err(ModelError::new(
            ErrorCode::GitCommandFailed,
            "repository_path is not a git repository",
        ));
    }

    let member_path = existing_repo_member_path(&root, &repo_path, request.member_path.as_ref())?;
    reject_existing_member_path(&manifest, &member_path)?;
    let slug = path_slug(member_path.as_str())?;
    let member_id = request.member_id.unwrap_or_else(|| format!("mem_{slug}"));
    let source_id = request.source_id.unwrap_or_else(|| format!("src_{slug}"));
    MemberId::parse_str(&member_id)?;
    SourceId::parse_str(&source_id)?;
    reject_duplicate_member_id(&manifest, &member_id)?;

    let head = backend.head(&repo_path)?;
    let status = backend.status(&repo_path)?;
    let remotes = backend.remotes(&repo_path)?;
    let manifest_member = ManifestMember {
        id: member_id.clone(),
        path: member_path.as_str().to_owned(),
        source_kind: ArtifactSourceKind::Git,
        source_id: source_id.clone(),
        active: true,
        desired: Some(desired_from_head(&head)),
        remotes: remotes
            .iter()
            .map(|remote| RemoteArtifact {
                name: remote.name.clone(),
                url: remote.url.clone().unwrap_or_default(),
                fetch: true,
                push: true,
            })
            .collect(),
    };
    manifest.members.push(manifest_member.clone());
    let paths = manifest
        .members
        .iter()
        .map(|member| MemberPath::parse(&member.path))
        .collect::<ModelResult<Vec<_>>>()?;
    validate_member_path_set(&paths)?;
    artifact::write_manifest(&root, &manifest)?;

    let mut lock = read_lock_or_empty(&root, &manifest.workspace.id)?;
    let locked = resolved_member(&manifest_member, &head, &status);
    lock.members.insert(member_id.clone(), locked.clone());
    lock.created_at = now_marker();
    artifact::write_lock(&root, &lock)?;
    sync_workspace_git_metadata(&root, &manifest.members)?;

    Ok(crate::AddExistingRepoResponse {
        response: response_envelope(
            context,
            crate::AggregateStatus::Ok,
            vec![crate::MemberResponse {
                member_id,
                member_path: manifest_member.path.clone(),
                source_kind: crate::SourceKind::Git,
                status: crate::MemberStatus::Ok,
                error: None,
                planned: None,
                state: Some(protocol_state(&manifest_member, &locked)),
                git_status: None,
                lock_match: Some(crate::LockMatch::Matches),
            }],
        ),
    })
}

pub fn handle_init_from_sources<B>(
    backend: &B,
    start: &Path,
    request: crate::InitFromSourcesRequest,
    operation_id: impl Into<String>,
    events: &dyn EventSink,
) -> ModelResult<crate::InitFromSourcesResponse>
where
    B: GitBackend + Sync,
{
    let context =
        OperationRequest::InitFromSources(request.clone()).context(operation_id.into())?;
    let root = if request.workspace_root.trim().is_empty() {
        start.to_path_buf()
    } else {
        PathBuf::from(&request.workspace_root)
    };
    if request.sources.is_empty() {
        return Err(invalid("init from sources requires at least one source"));
    }
    assert_init_target_is_head(request.target.as_ref())?;

    if root.join(WORKSPACE_MANIFEST).exists() {
        let manifest = artifact::read_manifest(&root)?;
        if let Some(expected) = &request.workspace_id
            && expected != &manifest.workspace.id
        {
            return Err(ModelError::new(
                ErrorCode::WorkspaceNotFound,
                "workspace id does not match manifest",
            ));
        }
        let plans = init_source_plans(&manifest, &request.sources)?;
        return Ok(crate::InitFromSourcesResponse {
            response: response_envelope(
                context,
                crate::AggregateStatus::Accepted,
                plans.iter().map(InitSourcePlan::planned_response).collect(),
            ),
        });
    }

    preflight_create_workspace(&root)?;
    let workspace_id = request
        .workspace_id
        .clone()
        .unwrap_or_else(|| "ws_default".to_owned());
    crate::model::WorkspaceId::parse_str(&workspace_id)?;
    let mut manifest = ManifestArtifact {
        schema: artifact::WORKSPACE_SCHEMA.to_owned(),
        workspace: WorkspaceHeader {
            id: workspace_id.clone(),
        },
        members: Vec::new(),
    };
    let plans = init_source_plans(&manifest, &request.sources)?;
    preflight_init_execution_targets(&root, &plans)?;

    if request.meta.dry_run.unwrap_or(false) {
        return Ok(crate::InitFromSourcesResponse {
            response: response_envelope(
                context,
                crate::AggregateStatus::Accepted,
                plans.iter().map(InitSourcePlan::planned_response).collect(),
            ),
        });
    }

    ensure_workspace_git_repo(&root)?;
    let mut lock = LockArtifact {
        schema: artifact::LOCK_SCHEMA.to_owned(),
        workspace_id,
        manifest_schema: artifact::WORKSPACE_SCHEMA.to_owned(),
        created_at: now_marker(),
        members: BTreeMap::new(),
    };
    let progress_interval = request
        .meta
        .policy
        .as_ref()
        .and_then(|policy| policy.progress_min_interval_ms)
        .unwrap_or(0);
    let emitter = EventEmitter::new(&context, events, progress_interval);
    emitter.operation_started();
    type InitOutcome = (
        ManifestMember,
        ResolvedMemberArtifact,
        crate::MemberResponse,
    );
    let outcomes = par_map_per_host(
        plans,
        resolve_jobs(
            request
                .meta
                .policy
                .as_ref()
                .and_then(|policy| policy.concurrency),
        ),
        resolve_per_host(
            request
                .meta
                .policy
                .as_ref()
                .and_then(|policy| policy.max_connections_per_host),
        ),
        |plan| git_host(&plan.source.url),
        |plan| -> ModelResult<InitOutcome> {
            let member_root = root.join(plan.path.as_str());
            emitter.member_started(&plan.member_id, plan.path.as_str());
            backend.clone_repo_with_progress(&plan.source.url, &member_root, &|progress| {
                emitter.member_progress(&plan.member_id, plan.path.as_str(), progress)
            })?;
            let head = backend.head(&member_root)?;
            let status = backend.status(&member_root)?;
            emitter.member_finished(&plan.member_id, plan.path.as_str());
            let remotes = backend.remotes(&member_root)?;
            let manifest_member = ManifestMember {
                id: plan.member_id.clone(),
                path: plan.path.as_str().to_owned(),
                source_kind: ArtifactSourceKind::Git,
                source_id: plan.source_id.clone(),
                active: true,
                desired: Some(desired_from_head(&head)),
                remotes: remotes
                    .iter()
                    .map(|remote| RemoteArtifact {
                        name: remote.name.clone(),
                        url: remote.url.clone().unwrap_or_default(),
                        fetch: true,
                        push: true,
                    })
                    .collect(),
            };
            let locked = resolved_member(&manifest_member, &head, &status);
            let response = crate::MemberResponse {
                member_id: plan.member_id,
                member_path: manifest_member.path.clone(),
                source_kind: crate::SourceKind::Git,
                status: crate::MemberStatus::Ok,
                error: None,
                planned: None,
                state: Some(protocol_state(&manifest_member, &locked)),
                git_status: None,
                lock_match: Some(crate::LockMatch::Matches),
            };
            Ok((manifest_member, locked, response))
        },
    );
    let mut members = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        let (manifest_member, locked, response) = outcome?;
        lock.members.insert(manifest_member.id.clone(), locked);
        members.push(response);
        manifest.members.push(manifest_member);
    }
    artifact::write_manifest(&root, &manifest)?;
    lock.created_at = now_marker();
    artifact::write_lock(&root, &lock)?;
    sync_workspace_git_metadata(&root, &manifest.members)?;
    emitter.operation_finished();

    Ok(crate::InitFromSourcesResponse {
        response: response_envelope(context, crate::AggregateStatus::Ok, members),
    })
}

pub fn handle_snapshot(
    start: &Path,
    request: crate::SnapshotRequest,
    operation_id: impl Into<String>,
) -> ModelResult<crate::SnapshotResponse> {
    let context = OperationRequest::Snapshot(request.clone()).context(operation_id.into())?;
    let root = resolve_workspace_root(start, request.meta.workspace.as_ref())?;
    let manifest = artifact::read_manifest(&root)?;
    assert_workspace_id(&manifest, request.meta.workspace.as_ref())?;
    let lock = artifact::read_lock(&root)?;
    let selected = resolve_locked_selection(&manifest, &lock, request.meta.selection.as_ref())?;
    let members = selected_member_map(&lock, &selected)?;
    artifact::write_snapshot(
        &root,
        &artifact::SnapshotArtifact {
            schema: artifact::SNAPSHOT_SCHEMA.to_owned(),
            workspace_id: manifest.workspace.id.clone(),
            snapshot_id: request.snapshot_id,
            created_at: now_marker(),
            created_by: created_by(&context),
            selected_members: selected.clone(),
            members: members.clone(),
        },
    )?;

    Ok(crate::SnapshotResponse {
        response: response_envelope(
            context,
            crate::AggregateStatus::Ok,
            locked_member_responses(&manifest, &members),
        ),
    })
}

pub fn handle_tag(
    start: &Path,
    request: crate::TagRequest,
    operation_id: impl Into<String>,
) -> ModelResult<crate::TagResponse> {
    let context = OperationRequest::Tag(request.clone()).context(operation_id.into())?;
    let root = resolve_workspace_root(start, request.meta.workspace.as_ref())?;
    let tag_path = root
        .join(artifact::TAG_DIR)
        .join(format!("{}.yml", request.tag_name));
    if tag_path.exists() {
        return Err(ModelError::new(
            ErrorCode::TagInvalid,
            "GWZ tag already exists",
        ));
    }

    let manifest = artifact::read_manifest(&root)?;
    assert_workspace_id(&manifest, request.meta.workspace.as_ref())?;
    let lock = artifact::read_lock(&root)?;
    let selected = resolve_locked_selection(&manifest, &lock, request.meta.selection.as_ref())?;
    let members = selected_member_map(&lock, &selected)?;
    let tag = artifact::TagArtifact {
        schema: artifact::TAG_SCHEMA.to_owned(),
        workspace_id: manifest.workspace.id.clone(),
        tag: request.tag_name,
        created_at: now_marker(),
        created_by: created_by(&context),
        selected_members: selected.clone(),
        members: members.clone(),
    };
    artifact::write_tag(&root, &tag).map_err(tag_error)?;

    Ok(crate::TagResponse {
        response: response_envelope(
            context,
            crate::AggregateStatus::Ok,
            locked_member_responses(&manifest, &members),
        ),
    })
}

pub fn load_snapshot_target(
    root: &Path,
    snapshot_id: &str,
) -> ModelResult<BTreeMap<String, ResolvedMemberArtifact>> {
    Ok(artifact::read_snapshot(root, snapshot_id)?.members)
}

pub fn load_tag_target(
    root: &Path,
    tag_name: &str,
) -> ModelResult<BTreeMap<String, ResolvedMemberArtifact>> {
    Ok(artifact::read_tag(root, tag_name)?.members)
}

pub fn handle_materialize<B>(
    backend: &B,
    start: &Path,
    request: crate::MaterializeRequest,
    operation_id: impl Into<String>,
    events: &dyn EventSink,
) -> ModelResult<crate::MaterializeResponse>
where
    B: GitBackend + Sync,
{
    let context = OperationRequest::Materialize(request.clone()).context(operation_id.into())?;
    let root = resolve_workspace_root(start, request.meta.workspace.as_ref())?;
    let manifest = artifact::read_manifest(&root)?;
    assert_workspace_id(&manifest, request.meta.workspace.as_ref())?;
    let (target_members, rewrite_lock) = materialize_target_members(&root, &request.target)?;
    let target_lock = LockArtifact {
        schema: artifact::LOCK_SCHEMA.to_owned(),
        workspace_id: manifest.workspace.id.clone(),
        manifest_schema: artifact::WORKSPACE_SCHEMA.to_owned(),
        created_at: now_marker(),
        members: target_members,
    };
    let selected =
        resolve_locked_selection(&manifest, &target_lock, request.meta.selection.as_ref())?;
    let dry_run = request.meta.dry_run.unwrap_or(false);
    let destructive_allowed = request
        .meta
        .policy
        .as_ref()
        .and_then(|policy| policy.destructive)
        == Some(crate::DestructiveBehavior::Allow);

    let plans = materialize_preflight(
        backend,
        &root,
        &manifest,
        &target_lock,
        &selected,
        destructive_allowed,
    )?;
    if dry_run {
        return Ok(crate::MaterializeResponse {
            response: response_envelope(
                context,
                crate::AggregateStatus::Accepted,
                plans.into_iter().map(|plan| plan.response).collect(),
            ),
        });
    }

    let progress_interval = request
        .meta
        .policy
        .as_ref()
        .and_then(|policy| policy.progress_min_interval_ms)
        .unwrap_or(0);
    let emitter = EventEmitter::new(&context, events, progress_interval);
    emitter.operation_started();
    let outcomes = par_map_per_host(
        plans,
        resolve_jobs(
            request
                .meta
                .policy
                .as_ref()
                .and_then(|policy| policy.concurrency),
        ),
        resolve_per_host(
            request
                .meta
                .policy
                .as_ref()
                .and_then(|policy| policy.max_connections_per_host),
        ),
        |plan| plan.clone_url.as_deref().and_then(git_host),
        |plan| -> ModelResult<crate::MemberResponse> {
            emitter.member_started(&plan.member_id, &plan.state.path);
            if let Some(url) = plan.clone_url.as_deref() {
                backend.clone_repo_with_progress(
                    url,
                    &root.join(&plan.state.path),
                    &|progress| {
                        emitter.member_progress(&plan.member_id, &plan.state.path, progress)
                    },
                )?;
            }
            if let Some(commit) = &plan.state.commit {
                backend.checkout_commit(&root.join(&plan.state.path), commit)?;
            }
            emitter.member_finished(&plan.member_id, &plan.state.path);
            Ok(materialized_response(
                &manifest,
                &plan.member_id,
                &plan.state,
            ))
        },
    );
    let mut responses = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        responses.push(outcome?);
    }
    emitter.operation_finished();

    if rewrite_lock {
        let mut lock = read_lock_or_empty(&root, &manifest.workspace.id)?;
        for member_id in &selected {
            if let Some(state) = target_lock.members.get(member_id) {
                lock.members.insert(member_id.clone(), state.clone());
            }
        }
        lock.created_at = now_marker();
        artifact::write_lock(&root, &lock)?;
    }

    Ok(crate::MaterializeResponse {
        response: response_envelope(context, crate::AggregateStatus::Ok, responses),
    })
}

/// Clone a workspace from its root repository URL and complete it.
///
/// This is the one-shot form of `git clone <url> <target>` followed by
/// `gwz materialize --lock`: it clones the workspace root (the git repository
/// that owns `gwz.conf/`), verifies it is a GWZ workspace, then materializes
/// every member to the committed lock — cloning missing member repositories and
/// checking out their locked commits. The recorded operation is a lock
/// materialization; no new wire request type is introduced.
pub fn handle_clone_workspace<B>(
    backend: &B,
    meta: crate::RequestMeta,
    url: &str,
    target: &str,
    operation_id: impl Into<String>,
    events: &dyn EventSink,
) -> ModelResult<crate::MaterializeResponse>
where
    B: GitBackend + Sync,
{
    let target_path = PathBuf::from(target);
    // Refuse to clone over an existing workspace rather than corrupt it.
    if target_path.join(WORKSPACE_MANIFEST).exists() {
        return Err(ModelError::new(
            ErrorCode::WorkspaceAlreadyExists,
            "clone target already contains a GWZ workspace",
        ));
    }
    // Clone the workspace root repository — the step the CLI cannot perform.
    backend.clone_repo(url, &target_path)?;
    // Verify the cloned repository really is a GWZ workspace before mutating it.
    if !target_path.join(WORKSPACE_MANIFEST).is_file() {
        return Err(ModelError::new(
            ErrorCode::WorkspaceNotFound,
            format!("cloned repository is not a GWZ workspace: {WORKSPACE_MANIFEST} missing"),
        ));
    }
    // Complete the clone: materialize members to the committed lock.
    let workspace_id = meta
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.workspace_id.clone());
    let materialize = crate::MaterializeRequest {
        meta: crate::RequestMeta {
            workspace: Some(crate::WorkspaceRef {
                root: Some(target_path.to_string_lossy().into_owned()),
                workspace_id,
            }),
            ..meta
        },
        target: crate::MaterializeTarget {
            kind: crate::MaterializeTargetKind::Lock,
            name: None,
            commit: None,
        },
    };
    handle_materialize(backend, &target_path, materialize, operation_id, events)
}

pub fn handle_pull_snapshot<B>(
    backend: &B,
    start: &Path,
    request: crate::PullSnapshotRequest,
    operation_id: impl Into<String>,
    events: &dyn EventSink,
) -> ModelResult<crate::PullSnapshotResponse>
where
    B: GitBackend + Sync,
{
    let context = OperationRequest::PullSnapshot(request.clone()).context(operation_id.into())?;
    let materialize = crate::MaterializeRequest {
        meta: request.meta,
        target: crate::MaterializeTarget {
            kind: crate::MaterializeTargetKind::Snapshot,
            name: Some(request.snapshot_id),
            commit: None,
        },
    };
    let mut response = handle_materialize(
        backend,
        start,
        materialize,
        context.operation_id.clone(),
        events,
    )?
    .response;
    response.meta = crate::ResponseMeta {
        request_id: context.request_id,
        schema_version: context.schema_version,
        action: context.action.into(),
        aggregate_status: response.meta.aggregate_status,
        operation_id: Some(context.operation_id),
        message: response.meta.message,
        attribution: context.attribution.as_ref().map(Into::into),
    };
    Ok(crate::PullSnapshotResponse { response })
}

pub fn handle_pull_head<B>(
    backend: &B,
    start: &Path,
    request: crate::PullHeadRequest,
    operation_id: impl Into<String>,
) -> ModelResult<crate::PullHeadResponse>
where
    B: GitBackend + Sync,
{
    handle_pull_head_with_events(backend, start, request, operation_id, &NullSink)
}

pub fn handle_pull_head_with_events<B>(
    backend: &B,
    start: &Path,
    request: crate::PullHeadRequest,
    operation_id: impl Into<String>,
    events: &dyn EventSink,
) -> ModelResult<crate::PullHeadResponse>
where
    B: GitBackend + Sync,
{
    let context = OperationRequest::PullHead(request.clone()).context(operation_id.into())?;
    let root = resolve_workspace_root(start, request.meta.workspace.as_ref())?;
    let manifest = artifact::read_manifest(&root)?;
    assert_workspace_id(&manifest, request.meta.workspace.as_ref())?;
    let mut lock = artifact::read_lock(&root)?;
    let selected = resolve_locked_selection(&manifest, &lock, request.meta.selection.as_ref())?;
    if request.meta.dry_run.unwrap_or(false) {
        let plans = pull_head_preflight(
            backend,
            &root,
            &manifest,
            &lock,
            &selected,
            request.meta.policy.as_ref(),
            None,
        )?;
        return Ok(crate::PullHeadResponse {
            response: response_envelope(
                context,
                pull_aggregate_status(&plans),
                plans.iter().map(PullHeadPlan::planned_response).collect(),
            ),
        });
    }

    let progress_interval = request
        .meta
        .policy
        .as_ref()
        .and_then(|policy| policy.progress_min_interval_ms)
        .unwrap_or(0);
    let emitter = EventEmitter::new(&context, events, progress_interval);
    emitter.operation_started();
    let plans = pull_head_preflight(
        backend,
        &root,
        &manifest,
        &lock,
        &selected,
        request.meta.policy.as_ref(),
        Some(&emitter),
    )?;
    let mut responses = Vec::with_capacity(plans.len());
    for plan in plans {
        if let PullHeadAction::FastForward { remote_ref } = &plan.action {
            backend.fast_forward(&root.join(&plan.state.path), &plan.branch, remote_ref)?;
        }
        let member = manifest
            .members
            .iter()
            .find(|member| member.id == plan.member_id)
            .ok_or_else(|| ModelError::new(ErrorCode::MemberNotFound, "member not found"))?;
        let member_root = root.join(&plan.state.path);
        let state = if backend.is_repository(&member_root)? {
            let head = backend.head(&member_root)?;
            let status = backend.status(&member_root)?;
            resolved_member(member, &head, &status)
        } else {
            plan.state.clone()
        };
        lock.members.insert(plan.member_id.clone(), state.clone());
        responses.push(pull_result_response(member, &state, &plan.action));
    }
    lock.created_at = now_marker();
    artifact::write_lock(&root, &lock)?;
    emitter.operation_finished();

    Ok(crate::PullHeadResponse {
        response: response_envelope(context, pull_response_aggregate(&responses), responses),
    })
}

pub fn handle_push<B>(
    backend: &B,
    start: &Path,
    request: crate::PushRequest,
    operation_id: impl Into<String>,
) -> ModelResult<crate::PushResponse>
where
    B: GitBackend + Sync,
{
    handle_push_with_events(backend, start, request, operation_id, &NullSink)
}

pub fn handle_push_with_events<B>(
    backend: &B,
    start: &Path,
    request: crate::PushRequest,
    operation_id: impl Into<String>,
    events: &dyn EventSink,
) -> ModelResult<crate::PushResponse>
where
    B: GitBackend + Sync,
{
    let context = OperationRequest::Push(request.clone()).context(operation_id.into())?;
    let root = resolve_workspace_root(start, request.meta.workspace.as_ref())?;
    let manifest = artifact::read_manifest(&root)?;
    assert_workspace_id(&manifest, request.meta.workspace.as_ref())?;
    let selected = resolve_manifest_selection(&manifest, request.meta.selection.as_ref())?;
    if request.meta.dry_run.unwrap_or(false) {
        let responses = selected
            .iter()
            .map(|member_id| {
                let member = manifest
                    .members
                    .iter()
                    .find(|member| &member.id == member_id)
                    .ok_or_else(|| {
                        ModelError::new(ErrorCode::MemberNotFound, "member not found")
                    })?;
                Ok(push_member(backend, &root, member, &request, true))
            })
            .collect::<ModelResult<Vec<_>>>()?;

        return Ok(crate::PushResponse {
            response: response_envelope(context, push_aggregate_status(&responses), responses),
        });
    }

    let progress_interval = request
        .meta
        .policy
        .as_ref()
        .and_then(|policy| policy.progress_min_interval_ms)
        .unwrap_or(0);
    let emitter = EventEmitter::new(&context, events, progress_interval);
    emitter.operation_started();
    let responses = par_map_per_host(
        selected,
        resolve_jobs(
            request
                .meta
                .policy
                .as_ref()
                .and_then(|policy| policy.concurrency),
        ),
        resolve_per_host(
            request
                .meta
                .policy
                .as_ref()
                .and_then(|policy| policy.max_connections_per_host),
        ),
        |member_id| {
            manifest
                .members
                .iter()
                .find(|member| member.id == *member_id)
                .and_then(|member| push_remote_host(member, &request))
        },
        |member_id| {
            let member = manifest
                .members
                .iter()
                .find(|member| member.id == member_id)
                .ok_or_else(|| ModelError::new(ErrorCode::MemberNotFound, "member not found"))?;
            emitter.member_started(&member.id, &member.path);
            let response = push_member(backend, &root, member, &request, false);
            emitter.member_finished(&member.id, &member.path);
            Ok(response)
        },
    )
    .into_iter()
    .collect::<ModelResult<Vec<_>>>()?;
    emitter.operation_finished();

    Ok(crate::PushResponse {
        response: response_envelope(context, push_aggregate_status(&responses), responses),
    })
}

fn resolve_workspace_root(
    start: &Path,
    workspace: Option<&crate::WorkspaceRef>,
) -> ModelResult<PathBuf> {
    if let Some(root) = workspace.and_then(|workspace| workspace.root.as_ref()) {
        Ok(PathBuf::from(root))
    } else {
        discover_workspace_root(start)
    }
}

fn assert_workspace_id(
    manifest: &ManifestArtifact,
    workspace: Option<&crate::WorkspaceRef>,
) -> ModelResult<()> {
    if let Some(expected) = workspace.and_then(|workspace| workspace.workspace_id.as_ref())
        && expected != &manifest.workspace.id
    {
        return Err(ModelError::new(
            ErrorCode::WorkspaceNotFound,
            "workspace id does not match manifest",
        ));
    }
    Ok(())
}

fn resolve_locked_selection(
    manifest: &ManifestArtifact,
    lock: &LockArtifact,
    selection: Option<&crate::Selection>,
) -> ModelResult<Vec<String>> {
    let selected = resolve_manifest_selection(manifest, selection)?;
    for member_id in &selected {
        if !lock.members.contains_key(member_id) {
            return Err(ModelError::new(
                ErrorCode::LockNotFound,
                format!("lock record missing for member '{member_id}'"),
            ));
        }
    }
    Ok(selected)
}

fn resolve_manifest_selection(
    manifest: &ManifestArtifact,
    selection: Option<&crate::Selection>,
) -> ModelResult<Vec<String>> {
    match selection {
        None => Ok(manifest
            .members
            .iter()
            .filter(|member| member.active)
            .map(|member| member.id.clone())
            .collect::<Vec<_>>()),
        Some(selection) => resolve_explicit_locked_selection(manifest, selection),
    }
}

fn resolve_explicit_locked_selection(
    manifest: &ManifestArtifact,
    selection: &crate::Selection,
) -> ModelResult<Vec<String>> {
    let has_filters = !selection.member_ids.is_empty() || !selection.paths.is_empty();
    if selection.all == Some(true) {
        if has_filters {
            return Err(invalid(
                "selection cannot combine all=true with member filters",
            ));
        }
        return Ok(manifest
            .members
            .iter()
            .filter(|member| member.active)
            .map(|member| member.id.clone())
            .collect());
    }
    if !has_filters {
        return Err(invalid(
            "selection must include all=true, member_ids, or paths",
        ));
    }

    let mut selected = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for member_id in &selection.member_ids {
        MemberId::parse_str(member_id)?;
        let member = find_active_member_by_id(manifest, member_id)?;
        if !seen.insert(member.id.clone()) {
            return Err(invalid("selection resolves the same member more than once"));
        }
        selected.push(member.id.clone());
    }
    for path in &selection.paths {
        MemberPath::parse(path)?;
        let member = find_active_member_by_path(manifest, path)?;
        if !seen.insert(member.id.clone()) {
            return Err(invalid("selection resolves the same member more than once"));
        }
        selected.push(member.id.clone());
    }
    Ok(selected)
}

fn find_active_member_by_id<'a>(
    manifest: &'a ManifestArtifact,
    member_id: &str,
) -> ModelResult<&'a ManifestMember> {
    let member = manifest
        .members
        .iter()
        .find(|member| member.id == member_id)
        .ok_or_else(|| ModelError::new(ErrorCode::MemberNotFound, "member id not found"))?;
    if member.active {
        Ok(member)
    } else {
        Err(ModelError::new(
            ErrorCode::MemberInactive,
            "selected member is inactive",
        ))
    }
}

fn find_active_member_by_path<'a>(
    manifest: &'a ManifestArtifact,
    path: &str,
) -> ModelResult<&'a ManifestMember> {
    let member = manifest
        .members
        .iter()
        .find(|member| member.path == path)
        .ok_or_else(|| ModelError::new(ErrorCode::MemberNotFound, "member path not found"))?;
    if member.active {
        Ok(member)
    } else {
        Err(ModelError::new(
            ErrorCode::MemberInactive,
            "selected member is inactive",
        ))
    }
}

fn selected_member_map(
    lock: &LockArtifact,
    selected: &[String],
) -> ModelResult<BTreeMap<String, ResolvedMemberArtifact>> {
    let mut members = BTreeMap::new();
    for member_id in selected {
        let member = lock.members.get(member_id).ok_or_else(|| {
            ModelError::new(
                ErrorCode::LockNotFound,
                format!("lock record missing for member '{member_id}'"),
            )
        })?;
        members.insert(member_id.clone(), member.clone());
    }
    Ok(members)
}

fn locked_member_responses(
    manifest: &ManifestArtifact,
    members: &BTreeMap<String, ResolvedMemberArtifact>,
) -> Vec<crate::MemberResponse> {
    members
        .iter()
        .map(|(member_id, state)| {
            let manifest_member = manifest
                .members
                .iter()
                .find(|member| &member.id == member_id);
            crate::MemberResponse {
                member_id: member_id.clone(),
                member_path: state.path.clone(),
                source_kind: crate::SourceKind::Git,
                status: crate::MemberStatus::Ok,
                error: None,
                planned: None,
                state: manifest_member.map(|member| protocol_state(member, state)),
                git_status: None,
                lock_match: Some(crate::LockMatch::Matches),
            }
        })
        .collect()
}

fn created_by(context: &crate::operation::OperationContext) -> CreatedByArtifact {
    CreatedByArtifact {
        actor_id: context
            .attribution
            .as_ref()
            .and_then(|attribution| attribution.actor.as_ref())
            .map(|actor| actor.actor_id.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
    }
}

fn reject_existing_member_path(manifest: &ManifestArtifact, path: &MemberPath) -> ModelResult<()> {
    if manifest
        .members
        .iter()
        .any(|member| member.path == path.as_str())
    {
        Err(ModelError::new(
            ErrorCode::PathCollision,
            "member path is already registered",
        ))
    } else {
        Ok(())
    }
}

fn reject_duplicate_member_id(manifest: &ManifestArtifact, member_id: &str) -> ModelResult<()> {
    if manifest.members.iter().any(|member| member.id == member_id) {
        Err(ModelError::new(
            ErrorCode::InvalidRequest,
            "member id is already registered",
        ))
    } else {
        Ok(())
    }
}

fn existing_repo_member_path(
    root: &Path,
    repo_path: &Path,
    requested: Option<&String>,
) -> ModelResult<MemberPath> {
    let root = normalize_path(root);
    let repo_path = normalize_path(repo_path);
    let member_path = if let Some(path) = requested {
        MemberPath::parse(path)?
    } else {
        let relative = repo_path.strip_prefix(&root).map_err(|_| {
            ModelError::new(
                ErrorCode::PathEscape,
                "repository_path must be inside the workspace when member_path is omitted",
            )
        })?;
        MemberPath::parse(&relative.to_string_lossy())?
    };
    if normalize_path(&root.join(member_path.as_str())) != repo_path {
        return Err(ModelError::new(
            ErrorCode::PathEscape,
            "member_path must point at repository_path",
        ));
    }
    Ok(member_path)
}

fn resolve_input_path(start: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&start_dir(start).join(path))
    }
}

fn start_dir(start: &Path) -> &Path {
    if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut normalized = PathBuf::new();
    for component in canonical.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn desired_from_head(head: &GitHeadState) -> DesiredRefArtifact {
    if let Some(branch) = &head.branch {
        DesiredRefArtifact {
            branch: Some(branch.clone()),
            ..Default::default()
        }
    } else if let Some(commit) = &head.commit {
        DesiredRefArtifact {
            commit: Some(commit.clone()),
            ..Default::default()
        }
    } else {
        DesiredRefArtifact {
            local_only: Some(true),
            ..Default::default()
        }
    }
}

fn ensure_member_target_available(path: &Path) -> ModelResult<()> {
    if !path.exists() {
        return Ok(());
    }
    if !path.is_dir() {
        return Err(ModelError::new(
            ErrorCode::PathCollision,
            "member path exists and is not a directory",
        ));
    }
    if fs::read_dir(path)
        .map_err(io_error)?
        .next()
        .transpose()
        .map_err(io_error)?
        .is_some()
    {
        return Err(ModelError::new(
            ErrorCode::PathCollision,
            "member path is not empty",
        ));
    }
    Ok(())
}

fn read_lock_or_empty(root: &Path, workspace_id: &str) -> ModelResult<LockArtifact> {
    if root.join(artifact::LOCK_PATH).exists() {
        artifact::read_lock(root)
    } else {
        Ok(LockArtifact {
            schema: artifact::LOCK_SCHEMA.to_owned(),
            workspace_id: workspace_id.to_owned(),
            manifest_schema: artifact::WORKSPACE_SCHEMA.to_owned(),
            created_at: now_marker(),
            members: BTreeMap::new(),
        })
    }
}

struct InitSourcePlan {
    source: crate::SourceUrl,
    path: MemberPath,
    member_id: String,
    source_id: String,
}

impl InitSourcePlan {
    fn planned_response(&self) -> crate::MemberResponse {
        crate::MemberResponse {
            member_id: self.member_id.clone(),
            member_path: self.path.as_str().to_owned(),
            source_kind: crate::SourceKind::Git,
            status: crate::MemberStatus::Planned,
            error: None,
            planned: Some(crate::PlannedChange {
                action: crate::PlannedAction::Clone,
                from_ref: None,
                to_ref: self.source.branch.clone(),
                message: Some(format!(
                    "clone {} as {}",
                    self.source.url,
                    self.source.remote_name.as_deref().unwrap_or("origin")
                )),
            }),
            state: None,
            git_status: None,
            lock_match: None,
        }
    }
}

fn init_source_plans(
    manifest: &ManifestArtifact,
    sources: &[crate::SourceUrl],
) -> ModelResult<Vec<InitSourcePlan>> {
    let mut paths = Vec::with_capacity(manifest.members.len() + sources.len());
    let mut member_ids = manifest
        .members
        .iter()
        .map(|member| member.id.clone())
        .collect::<BTreeSet<_>>();
    let mut source_ids = manifest
        .members
        .iter()
        .map(|member| member.source_id.clone())
        .collect::<BTreeSet<_>>();
    for member in &manifest.members {
        paths.push(MemberPath::parse(&member.path)?);
    }

    let mut plans = Vec::with_capacity(sources.len());
    for source in sources {
        let path = source
            .path
            .clone()
            .unwrap_or_else(|| repo_name_from_url(&source.url));
        let member_path = MemberPath::parse(&path)?;
        paths.push(member_path.clone());
        let slug = path_slug(member_path.as_str())?;
        let member_id = format!("mem_{slug}");
        let source_id = format!("src_{slug}");
        MemberId::parse_str(&member_id)?;
        SourceId::parse_str(&source_id)?;
        plans.push(InitSourcePlan {
            source: source.clone(),
            path: member_path,
            member_id,
            source_id,
        });
    }
    validate_member_path_set(&paths)?;

    for plan in &plans {
        if !member_ids.insert(plan.member_id.clone()) {
            return Err(ModelError::new(
                ErrorCode::InvalidRequest,
                "member id is already registered",
            ));
        }
        if !source_ids.insert(plan.source_id.clone()) {
            return Err(ModelError::new(
                ErrorCode::InvalidRequest,
                "source id is already registered",
            ));
        }
    }
    Ok(plans)
}

fn assert_init_target_is_head(target: Option<&crate::MaterializeTarget>) -> ModelResult<()> {
    match target {
        None => Ok(()),
        Some(target)
            if target.kind == crate::MaterializeTargetKind::Head
                && target.name.is_none()
                && target.commit.is_none() =>
        {
            Ok(())
        }
        Some(_) => Err(ModelError::new(
            ErrorCode::UnsupportedOperation,
            "init from sources only supports the default head target in v0",
        )),
    }
}

fn preflight_init_execution_targets(root: &Path, plans: &[InitSourcePlan]) -> ModelResult<()> {
    for plan in plans {
        if plan.source.branch.is_some() {
            return Err(ModelError::new(
                ErrorCode::UnsupportedOperation,
                "fresh init branch selection is not supported in v0",
            ));
        }
        if plan
            .source
            .remote_name
            .as_ref()
            .is_some_and(|name| name != "origin")
        {
            return Err(ModelError::new(
                ErrorCode::UnsupportedOperation,
                "fresh init custom remote names are not supported in v0",
            ));
        }
        ensure_member_target_available(&root.join(plan.path.as_str()))?;
    }
    Ok(())
}

fn materialize_target_members(
    root: &Path,
    target: &crate::MaterializeTarget,
) -> ModelResult<(BTreeMap<String, ResolvedMemberArtifact>, bool)> {
    match target.kind {
        crate::MaterializeTargetKind::Lock => {
            if !root.join(artifact::LOCK_PATH).exists() {
                return Err(ModelError::new(ErrorCode::LockNotFound, "lock not found"));
            }
            Ok((artifact::read_lock(root)?.members, false))
        }
        crate::MaterializeTargetKind::Snapshot => {
            let name = target
                .name
                .as_ref()
                .ok_or_else(|| invalid("snapshot target requires a name"))?;
            if !root
                .join(artifact::SNAPSHOT_DIR)
                .join(format!("{name}.yaml"))
                .exists()
            {
                return Err(ModelError::new(
                    ErrorCode::SnapshotNotFound,
                    "snapshot not found",
                ));
            }
            Ok((load_snapshot_target(root, name)?, true))
        }
        crate::MaterializeTargetKind::Tag => {
            let name = target
                .name
                .as_ref()
                .ok_or_else(|| invalid("tag target requires a name"))?;
            if !root
                .join(artifact::TAG_DIR)
                .join(format!("{name}.yml"))
                .exists()
            {
                return Err(ModelError::new(ErrorCode::TagNotFound, "tag not found"));
            }
            Ok((load_tag_target(root, name)?, true))
        }
        crate::MaterializeTargetKind::Commit | crate::MaterializeTargetKind::Head => {
            Err(ModelError::new(
                ErrorCode::UnsupportedOperation,
                "target is not supported here",
            ))
        }
    }
}

struct MaterializePlan {
    member_id: String,
    state: ResolvedMemberArtifact,
    clone_url: Option<String>,
    response: crate::MemberResponse,
}

fn materialize_preflight<B>(
    backend: &B,
    root: &Path,
    manifest: &ManifestArtifact,
    target_lock: &LockArtifact,
    selected: &[String],
    destructive_allowed: bool,
) -> ModelResult<Vec<MaterializePlan>>
where
    B: GitBackend,
{
    let mut plans = Vec::with_capacity(selected.len());
    for member_id in selected {
        let member = manifest
            .members
            .iter()
            .find(|member| &member.id == member_id)
            .ok_or_else(|| ModelError::new(ErrorCode::MemberNotFound, "member not found"))?;
        let state = target_lock.members.get(member_id).cloned().ok_or_else(|| {
            ModelError::new(
                ErrorCode::LockNotFound,
                format!("target state missing for member '{member_id}'"),
            )
        })?;
        let member_root = root.join(&state.path);
        let is_repo = member_root.exists() && backend.is_repository(&member_root)?;
        let clone_url = if is_repo {
            let status = backend.status(&member_root)?;
            if status.is_dirty && !destructive_allowed {
                return Err(ModelError::new(
                    ErrorCode::DirtyMember,
                    format!("member '{member_id}' has uncommitted changes"),
                ));
            }
            None
        } else {
            Some(first_remote_url(member)?)
        };
        let action = if clone_url.is_some() {
            crate::PlannedAction::Clone
        } else if state.commit.is_some() {
            crate::PlannedAction::Checkout
        } else {
            crate::PlannedAction::Noop
        };
        plans.push(MaterializePlan {
            member_id: member_id.clone(),
            state: state.clone(),
            clone_url,
            response: crate::MemberResponse {
                member_id: member_id.clone(),
                member_path: state.path.clone(),
                source_kind: crate::SourceKind::Git,
                status: crate::MemberStatus::Planned,
                error: None,
                planned: Some(crate::PlannedChange {
                    action,
                    from_ref: None,
                    to_ref: state.commit.clone(),
                    message: None,
                }),
                state: Some(protocol_state(member, &state)),
                git_status: None,
                lock_match: Some(crate::LockMatch::Differs),
            },
        });
    }
    Ok(plans)
}

fn first_remote_url(member: &ManifestMember) -> ModelResult<String> {
    member
        .remotes
        .iter()
        .find(|remote| remote.fetch)
        .map(|remote| remote.url.clone())
        .ok_or_else(|| ModelError::new(ErrorCode::MissingRemote, "member has no fetch remote"))
}

fn materialized_response(
    manifest: &ManifestArtifact,
    member_id: &str,
    state: &ResolvedMemberArtifact,
) -> crate::MemberResponse {
    let member = manifest
        .members
        .iter()
        .find(|member| member.id == member_id);
    crate::MemberResponse {
        member_id: member_id.to_owned(),
        member_path: state.path.clone(),
        source_kind: crate::SourceKind::Git,
        status: crate::MemberStatus::Ok,
        error: None,
        planned: None,
        state: member.map(|member| protocol_state(member, state)),
        git_status: None,
        lock_match: Some(crate::LockMatch::Matches),
    }
}

const NO_FETCH_REMOTE_PULL_MESSAGE: &str = "no fetch remote configured; skipping pull";

enum PullHeadAction {
    Noop,
    SkipNoFetchRemote,
    FastForward { remote_ref: String },
}

impl PullHeadAction {
    fn is_noop(&self) -> bool {
        matches!(self, Self::Noop | Self::SkipNoFetchRemote)
    }

    fn planned_message(&self) -> Option<String> {
        match self {
            Self::SkipNoFetchRemote => Some(NO_FETCH_REMOTE_PULL_MESSAGE.to_owned()),
            Self::Noop | Self::FastForward { .. } => None,
        }
    }
}

struct PullHeadPlan {
    member_id: String,
    branch: String,
    state: ResolvedMemberArtifact,
    action: PullHeadAction,
}

impl PullHeadPlan {
    fn planned_response(&self) -> crate::MemberResponse {
        crate::MemberResponse {
            member_id: self.member_id.clone(),
            member_path: self.state.path.clone(),
            source_kind: crate::SourceKind::Git,
            status: match self.action {
                PullHeadAction::Noop | PullHeadAction::SkipNoFetchRemote => {
                    crate::MemberStatus::Noop
                }
                PullHeadAction::FastForward { .. } => crate::MemberStatus::Planned,
            },
            error: None,
            planned: Some(crate::PlannedChange {
                action: match self.action {
                    PullHeadAction::Noop | PullHeadAction::SkipNoFetchRemote => {
                        crate::PlannedAction::Noop
                    }
                    PullHeadAction::FastForward { .. } => crate::PlannedAction::FastForward,
                },
                from_ref: self.state.commit.clone(),
                to_ref: None,
                message: self.action.planned_message(),
            }),
            state: None,
            git_status: None,
            lock_match: None,
        }
    }
}

fn pull_head_preflight<B>(
    backend: &B,
    root: &Path,
    manifest: &ManifestArtifact,
    lock: &LockArtifact,
    selected: &[String],
    policy: Option<&crate::OperationPolicy>,
    emitter: Option<&EventEmitter<'_>>,
) -> ModelResult<Vec<PullHeadPlan>>
where
    B: GitBackend + Sync,
{
    par_map_per_host(
        selected.to_vec(),
        resolve_jobs(policy.and_then(|policy| policy.concurrency)),
        resolve_per_host(policy.and_then(|policy| policy.max_connections_per_host)),
        |member_id| {
            manifest
                .members
                .iter()
                .find(|member| member.id == *member_id)
                .and_then(|member| pull_remote_host(member, policy))
        },
        |member_id| {
            pull_head_member_preflight(backend, root, manifest, lock, member_id, policy, emitter)
        },
    )
    .into_iter()
    .collect()
}

fn pull_head_member_preflight<B>(
    backend: &B,
    root: &Path,
    manifest: &ManifestArtifact,
    lock: &LockArtifact,
    member_id: String,
    policy: Option<&crate::OperationPolicy>,
    emitter: Option<&EventEmitter<'_>>,
) -> ModelResult<PullHeadPlan>
where
    B: GitBackend,
{
    let member = manifest
        .members
        .iter()
        .find(|member| member.id == member_id)
        .ok_or_else(|| ModelError::new(ErrorCode::MemberNotFound, "member not found"))?;
    let state = lock.members.get(&member_id).cloned().ok_or_else(|| {
        ModelError::new(
            ErrorCode::LockNotFound,
            format!("lock record missing for member '{member_id}'"),
        )
    })?;
    let branch = state
        .branch
        .clone()
        .or_else(|| {
            member
                .desired
                .as_ref()
                .and_then(|desired| desired.branch.clone())
        })
        .unwrap_or_else(|| "main".to_owned());
    if let Some(emitter) = emitter {
        emitter.member_started(&member.id, &state.path);
    }
    if member
        .desired
        .as_ref()
        .and_then(|desired| desired.local_only)
        == Some(true)
    {
        if let Some(emitter) = emitter {
            emitter.member_finished(&member.id, &state.path);
        }
        return Ok(PullHeadPlan {
            member_id,
            branch,
            state,
            action: PullHeadAction::Noop,
        });
    }

    let member_root = root.join(&state.path);
    if !backend.is_repository(&member_root)? {
        return Err(ModelError::new(
            ErrorCode::MemberNotFound,
            format!("member '{member_id}' is not materialized"),
        ));
    }
    let status = backend.status(&member_root)?;
    if status.is_dirty {
        return Err(ModelError::new(
            ErrorCode::DirtyMember,
            format!("member '{member_id}' has uncommitted changes"),
        ));
    }
    let Some(remote) = pull_fetch_remote_name(member, policy) else {
        if let Some(emitter) = emitter {
            emitter.member_finished(&member.id, &state.path);
        }
        return Ok(PullHeadPlan {
            member_id,
            branch,
            state,
            action: PullHeadAction::SkipNoFetchRemote,
        });
    };
    backend.fetch(&member_root, &remote)?;
    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    let remote_commit = backend
        .read_ref(&member_root, &remote_ref)?
        .ok_or_else(|| ModelError::new(ErrorCode::MissingRemote, "remote branch not found"))?;
    let head = backend.head(&member_root)?;
    let Some(local_commit) = head.commit.clone() else {
        return Err(ModelError::new(
            ErrorCode::DivergedMember,
            "cannot fast-forward unborn member",
        ));
    };
    let action = if local_commit == remote_commit {
        PullHeadAction::Noop
    } else if backend.is_ancestor(&member_root, &local_commit, &remote_commit)? {
        PullHeadAction::FastForward { remote_ref }
    } else {
        return Err(ModelError::new(
            ErrorCode::DivergedMember,
            format!("member '{member_id}' has diverged from remote"),
        ));
    };
    if let Some(emitter) = emitter {
        emitter.member_finished(&member.id, &state.path);
    }
    Ok(PullHeadPlan {
        member_id,
        branch,
        state,
        action,
    })
}

fn pull_fetch_remote_name(
    member: &ManifestMember,
    policy: Option<&crate::OperationPolicy>,
) -> Option<String> {
    policy
        .and_then(|policy| policy.remote.as_ref())
        .cloned()
        .or_else(|| {
            member
                .remotes
                .iter()
                .find(|remote| remote.fetch)
                .map(|remote| remote.name.clone())
        })
}

fn pull_remote_host(
    member: &ManifestMember,
    policy: Option<&crate::OperationPolicy>,
) -> Option<String> {
    let remote = pull_fetch_remote_name(member, policy)?;
    member
        .remotes
        .iter()
        .find(|candidate| candidate.name == remote)
        .and_then(|candidate| git_host(&candidate.url))
}

fn push_remote_host(member: &ManifestMember, request: &crate::PushRequest) -> Option<String> {
    let remote = resolve_push_remote(member, request).ok()?;
    member
        .remotes
        .iter()
        .find(|candidate| candidate.name == remote)
        .and_then(|candidate| git_host(&candidate.url))
}

fn pull_result_response(
    member: &ManifestMember,
    state: &ResolvedMemberArtifact,
    action: &PullHeadAction,
) -> crate::MemberResponse {
    crate::MemberResponse {
        member_id: member.id.clone(),
        member_path: state.path.clone(),
        source_kind: crate::SourceKind::Git,
        status: match action {
            PullHeadAction::Noop | PullHeadAction::SkipNoFetchRemote => crate::MemberStatus::Noop,
            PullHeadAction::FastForward { .. } => crate::MemberStatus::Ok,
        },
        error: None,
        planned: action
            .planned_message()
            .map(|message| crate::PlannedChange {
                action: crate::PlannedAction::Noop,
                from_ref: state.commit.clone(),
                to_ref: None,
                message: Some(message),
            }),
        state: Some(protocol_state(member, state)),
        git_status: None,
        lock_match: Some(crate::LockMatch::Matches),
    }
}

fn pull_aggregate_status(plans: &[PullHeadPlan]) -> crate::AggregateStatus {
    if plans.iter().all(|plan| plan.action.is_noop()) {
        crate::AggregateStatus::Noop
    } else {
        crate::AggregateStatus::Accepted
    }
}

fn pull_response_aggregate(responses: &[crate::MemberResponse]) -> crate::AggregateStatus {
    if responses
        .iter()
        .all(|response| response.status == crate::MemberStatus::Noop)
    {
        crate::AggregateStatus::Noop
    } else {
        crate::AggregateStatus::Ok
    }
}

fn push_member<B>(
    backend: &B,
    root: &Path,
    member: &ManifestMember,
    request: &crate::PushRequest,
    dry_run: bool,
) -> crate::MemberResponse
where
    B: GitBackend,
{
    let source_kind = artifact_source_kind_to_protocol(member.source_kind);
    if member.source_kind != ArtifactSourceKind::Git {
        return push_policy_member_error(
            member,
            source_kind,
            request,
            ModelError::new(
                ErrorCode::UnsupportedSourceKind,
                "push supports git members only",
            ),
        );
    }

    let remote = match resolve_push_remote(member, request) {
        Ok(remote) => remote,
        Err(error) => return push_policy_member_error(member, source_kind, request, error),
    };
    let member_root = root.join(&member.path);
    let is_repo = match backend.is_repository(&member_root) {
        Ok(is_repo) => is_repo,
        Err(error) => {
            return push_member_error(member, source_kind, error, crate::MemberStatus::Failed);
        }
    };
    if !is_repo {
        return push_member_error(
            member,
            source_kind,
            ModelError::new(ErrorCode::MemberNotFound, "member is not materialized"),
            crate::MemberStatus::Rejected,
        );
    }

    let head = match backend.head(&member_root) {
        Ok(head) => head,
        Err(error) => {
            return push_member_error(member, source_kind, error, crate::MemberStatus::Failed);
        }
    };
    let refspec = match resolve_push_refspec(&head, request) {
        Ok(refspec) => refspec,
        Err(error) => return push_policy_member_error(member, source_kind, request, error),
    };
    if dry_run {
        return crate::MemberResponse {
            member_id: member.id.clone(),
            member_path: member.path.clone(),
            source_kind,
            status: crate::MemberStatus::Planned,
            error: None,
            planned: Some(crate::PlannedChange {
                action: crate::PlannedAction::Push,
                from_ref: head.commit.clone(),
                to_ref: Some(refspec),
                message: Some(format!("push to {remote}")),
            }),
            state: None,
            git_status: None,
            lock_match: None,
        };
    }

    match backend.push(&member_root, &remote, &refspec) {
        Ok(_) => crate::MemberResponse {
            member_id: member.id.clone(),
            member_path: member.path.clone(),
            source_kind,
            status: crate::MemberStatus::Ok,
            error: None,
            planned: None,
            state: None,
            git_status: None,
            lock_match: None,
        },
        Err(error) if error.code == ErrorCode::MissingRemote => {
            push_member_error(member, source_kind, error, crate::MemberStatus::Failed)
        }
        Err(error) => push_member_error(
            member,
            source_kind,
            ModelError::new(ErrorCode::RemoteRejected, error.message),
            crate::MemberStatus::Failed,
        ),
    }
}

fn resolve_push_remote(
    member: &ManifestMember,
    request: &crate::PushRequest,
) -> ModelResult<String> {
    request
        .remote
        .clone()
        .or_else(|| {
            request
                .meta
                .policy
                .as_ref()
                .and_then(|policy| policy.remote.clone())
        })
        .or_else(|| {
            member
                .remotes
                .iter()
                .find(|remote| remote.push)
                .map(|remote| remote.name.clone())
        })
        .ok_or_else(|| ModelError::new(ErrorCode::MissingRemote, "member has no push remote"))
}

fn resolve_push_refspec(head: &GitHeadState, request: &crate::PushRequest) -> ModelResult<String> {
    if let Some(refspec) = &request.refspec {
        return Ok(refspec.clone());
    }
    let branch = head.branch.as_ref().ok_or_else(|| {
        ModelError::new(
            ErrorCode::InvalidRequest,
            "push refspec is required for detached members",
        )
    })?;
    if head.commit.is_none() {
        return Err(ModelError::new(
            ErrorCode::InvalidRequest,
            "cannot push a branch without commits",
        ));
    }
    Ok(format!("refs/heads/{branch}:refs/heads/{branch}"))
}

fn push_policy_member_error(
    member: &ManifestMember,
    source_kind: crate::SourceKind,
    request: &crate::PushRequest,
    error: ModelError,
) -> crate::MemberResponse {
    if request
        .meta
        .policy
        .as_ref()
        .and_then(|policy| policy.unsupported_member)
        == Some(crate::UnsupportedMemberBehavior::Skip)
    {
        push_member_error(member, source_kind, error, crate::MemberStatus::Skipped)
    } else {
        push_member_error(member, source_kind, error, crate::MemberStatus::Rejected)
    }
}

fn push_member_error(
    member: &ManifestMember,
    source_kind: crate::SourceKind,
    error: ModelError,
    status: crate::MemberStatus,
) -> crate::MemberResponse {
    crate::MemberResponse {
        member_id: member.id.clone(),
        member_path: member.path.clone(),
        source_kind,
        status,
        error: Some(crate::GwzError {
            code: error.code.into(),
            message: error.message,
            member_id: Some(member.id.clone()),
            member_path: Some(member.path.clone()),
            detail: None,
        }),
        planned: None,
        state: None,
        git_status: None,
        lock_match: None,
    }
}

fn push_aggregate_status(responses: &[crate::MemberResponse]) -> crate::AggregateStatus {
    let has_ok = responses
        .iter()
        .any(|response| response.status == crate::MemberStatus::Ok);
    let has_failed = responses
        .iter()
        .any(|response| response.status == crate::MemberStatus::Failed);
    let has_rejected = responses
        .iter()
        .any(|response| response.status == crate::MemberStatus::Rejected);
    let has_skipped = responses
        .iter()
        .any(|response| response.status == crate::MemberStatus::Skipped);
    if has_ok && (has_failed || has_rejected || has_skipped) {
        crate::AggregateStatus::Partial
    } else if has_failed {
        crate::AggregateStatus::Failed
    } else if has_rejected {
        crate::AggregateStatus::Rejected
    } else if has_skipped
        || responses
            .iter()
            .all(|response| response.status == crate::MemberStatus::Noop)
    {
        crate::AggregateStatus::Noop
    } else {
        crate::AggregateStatus::Ok
    }
}

fn artifact_source_kind_to_protocol(source_kind: ArtifactSourceKind) -> crate::SourceKind {
    match source_kind {
        ArtifactSourceKind::Git => crate::SourceKind::Git,
        ArtifactSourceKind::Archive => crate::SourceKind::Archive,
        ArtifactSourceKind::Package => crate::SourceKind::Package,
        ArtifactSourceKind::Local => crate::SourceKind::Local,
        ArtifactSourceKind::Generated => crate::SourceKind::Generated,
    }
}

fn resolved_member(
    member: &ManifestMember,
    head: &GitHeadState,
    status: &GitStatus,
) -> ResolvedMemberArtifact {
    ResolvedMemberArtifact {
        path: member.path.clone(),
        source_id: Some(member.source_id.clone()),
        source_kind: ArtifactSourceKind::Git,
        commit: head.commit.clone(),
        branch: head.branch.clone(),
        detached: Some(head.is_detached),
        upstream: None,
        dirty: Some(status.is_dirty),
        materialized: Some(true),
    }
}

fn protocol_state(
    member: &ManifestMember,
    state: &ResolvedMemberArtifact,
) -> crate::ResolvedMemberState {
    crate::ResolvedMemberState {
        member_id: member.id.clone(),
        path: state.path.clone(),
        source_id: member.source_id.clone(),
        source_kind: crate::SourceKind::Git,
        commit: state.commit.clone(),
        branch: state.branch.clone(),
        detached: state.detached,
        upstream: state.upstream.clone(),
        dirty: state.dirty,
        materialized: state.materialized.unwrap_or(false),
        remotes: member
            .remotes
            .iter()
            .map(|remote| crate::RemoteSpec {
                name: remote.name.clone(),
                url: remote.url.clone(),
                fetch: Some(remote.fetch),
                push: Some(remote.push),
            })
            .collect(),
    }
}

fn response_envelope(
    context: crate::operation::OperationContext,
    aggregate_status: crate::AggregateStatus,
    members: Vec<crate::MemberResponse>,
) -> crate::ResponseEnvelope {
    crate::ResponseEnvelope {
        meta: crate::ResponseMeta {
            request_id: context.request_id,
            schema_version: context.schema_version,
            action: context.action.into(),
            aggregate_status,
            operation_id: Some(context.operation_id),
            message: None,
            attribution: context.attribution.as_ref().map(Into::into),
        },
        members,
        errors: Vec::new(),
    }
}

fn path_slug(path: &str) -> ModelResult<String> {
    let leaf = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid("member path must have a final component"))?;
    let slug = leaf
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned();
    if slug.is_empty() {
        Err(invalid("member path does not contain a usable id slug"))
    } else {
        Ok(slug)
    }
}

fn repo_name_from_url(url: &str) -> String {
    let trimmed = url.trim_end_matches(['/', '\\']);
    let tail = trimmed.rsplit(['/', '\\', ':']).next().unwrap_or(trimmed);
    tail.strip_suffix(".git").unwrap_or(tail).to_owned()
}

fn now_marker() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("unix-ms:{millis}")
}

fn ensure_workspace_git_repo(root: &Path) -> ModelResult<()> {
    if root.join(".git").exists() {
        Ok(())
    } else {
        Git2Backend::new().create_repo(root).map(|_| ())
    }
}

fn sync_workspace_git_metadata(root: &Path, members: &[ManifestMember]) -> ModelResult<()> {
    update_workspace_gitignore(root, members)?;
    stage_workspace_git_metadata(root)
}

fn update_workspace_gitignore(root: &Path, members: &[ManifestMember]) -> ModelResult<()> {
    let managed = managed_gitignore_block(members);
    let path = root.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io_error(error)),
    };
    let updated = replace_managed_gitignore_block(&existing, &managed);
    if updated != existing {
        fs::write(path, updated).map_err(io_error)?;
    }
    Ok(())
}

fn managed_gitignore_block(members: &[ManifestMember]) -> String {
    let mut paths = members
        .iter()
        .map(|member| member.path.as_str())
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();

    let mut lines = vec![
        GITIGNORE_GWZ_BEGIN.to_owned(),
        format!("/{WORKSPACE_DIR}/.tmp/"),
    ];
    lines.extend(paths.into_iter().map(|path| format!("/{path}/")));
    lines.push(GITIGNORE_GWZ_END.to_owned());
    lines.push(String::new());
    lines.join("\n")
}

fn replace_managed_gitignore_block(existing: &str, managed: &str) -> String {
    if let Some(begin) = existing.find(GITIGNORE_GWZ_BEGIN)
        && let Some(relative_end) = existing[begin..].find(GITIGNORE_GWZ_END)
    {
        let end = begin + relative_end + GITIGNORE_GWZ_END.len();
        let after_end = if existing[end..].starts_with("\r\n") {
            end + 2
        } else if existing[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        let mut out = String::with_capacity(existing.len() + managed.len());
        out.push_str(&existing[..begin]);
        out.push_str(managed);
        out.push_str(&existing[after_end..]);
        return out;
    }

    if existing.trim().is_empty() {
        managed.to_owned()
    } else if existing.ends_with('\n') {
        format!("{existing}\n{managed}")
    } else {
        format!("{existing}\n\n{managed}")
    }
}

fn stage_workspace_git_metadata(root: &Path) -> ModelResult<()> {
    let repo = git2::Repository::open(root).map_err(git_command_error)?;
    let mut index = repo.index().map_err(git_command_error)?;
    index
        .add_all([WORKSPACE_DIR], git2::IndexAddOption::DEFAULT, None)
        .map_err(git_command_error)?;
    if root.join(".gitignore").is_file() {
        index
            .add_path(Path::new(".gitignore"))
            .map_err(git_command_error)?;
    }
    index.write().map_err(git_command_error)?;
    Ok(())
}

fn git_command_error(error: git2::Error) -> ModelError {
    ModelError::new(ErrorCode::GitCommandFailed, error.message())
}

fn invalid(message: impl Into<String>) -> ModelError {
    ModelError::new(ErrorCode::InvalidRequest, message)
}

fn io_error(error: std::io::Error) -> ModelError {
    ModelError::new(ErrorCode::IoError, error.to_string())
}

fn tag_error(error: ModelError) -> ModelError {
    if matches!(
        error.code,
        ErrorCode::InvalidRequest | ErrorCode::TagInvalid
    ) {
        ModelError::new(ErrorCode::TagInvalid, error.message)
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::artifact::{read_lock, read_manifest, read_snapshot, read_tag};
    use crate::git::{Git2Backend, GitBackend};
    use crate::model::ErrorCode;
    use crate::operation::NullSink;

    use super::*;

    #[derive(Default)]
    struct CollectingSink {
        events: std::sync::Mutex<Vec<crate::OperationEvent>>,
    }

    impl EventSink for CollectingSink {
        fn deliver(&self, event: crate::OperationEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl CollectingSink {
        fn take(&self) -> Vec<crate::OperationEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    const TEST_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone)]
    struct TrackingBackend {
        fetch: Arc<OverlapTracker>,
        push: Arc<OverlapTracker>,
    }

    impl TrackingBackend {
        fn new(expected_overlap: usize) -> Self {
            Self {
                fetch: Arc::new(OverlapTracker::new(expected_overlap)),
                push: Arc::new(OverlapTracker::new(expected_overlap)),
            }
        }

        fn fetch_peak(&self) -> usize {
            self.fetch.peak()
        }

        fn push_peak(&self) -> usize {
            self.push.peak()
        }
    }

    struct OverlapTracker {
        expected_overlap: usize,
        active: AtomicUsize,
        peak: AtomicUsize,
        entered: Mutex<usize>,
        all_entered: Condvar,
    }

    impl OverlapTracker {
        fn new(expected_overlap: usize) -> Self {
            Self {
                expected_overlap,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                entered: Mutex::new(0),
                all_entered: Condvar::new(),
            }
        }

        fn run(&self) {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.record_peak(active);
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut entered = self.entered.lock().unwrap();
            *entered += 1;
            self.all_entered.notify_all();
            while *entered < self.expected_overlap {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                let (next, timeout) = self.all_entered.wait_timeout(entered, remaining).unwrap();
                entered = next;
                if timeout.timed_out() {
                    break;
                }
            }
            drop(entered);
            self.active.fetch_sub(1, Ordering::SeqCst);
        }

        fn record_peak(&self, active: usize) {
            let mut observed = self.peak.load(Ordering::SeqCst);
            while active > observed {
                match self.peak.compare_exchange(
                    observed,
                    active,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    impl GitBackend for TrackingBackend {
        fn is_repository(&self, _path: &Path) -> ModelResult<bool> {
            Ok(true)
        }

        fn create_repo(&self, path: &Path) -> ModelResult<crate::git::GitCreateResult> {
            Ok(crate::git::GitCreateResult {
                path: path.to_path_buf(),
            })
        }

        fn clone_repo(&self, url: &str, path: &Path) -> ModelResult<crate::git::GitCloneResult> {
            let _ = url;
            Ok(crate::git::GitCloneResult {
                path: path.to_path_buf(),
                head: self.head(path)?,
            })
        }

        fn fetch(&self, _path: &Path, remote: &str) -> ModelResult<crate::git::GitFetchResult> {
            self.fetch.run();
            Ok(crate::git::GitFetchResult {
                remote: remote.to_owned(),
            })
        }

        fn fast_forward(
            &self,
            _path: &Path,
            _branch: &str,
            _upstream_ref: &str,
        ) -> ModelResult<crate::git::GitUpdateResult> {
            Ok(crate::git::GitUpdateResult {
                updated: false,
                commit: Some(TEST_COMMIT.to_owned()),
            })
        }

        fn checkout_commit(
            &self,
            _path: &Path,
            commit: &str,
        ) -> ModelResult<crate::git::GitUpdateResult> {
            Ok(crate::git::GitUpdateResult {
                updated: true,
                commit: Some(commit.to_owned()),
            })
        }

        fn status(&self, _path: &Path) -> ModelResult<crate::git::GitStatus> {
            Ok(crate::git::GitStatus::clean())
        }

        fn head(&self, _path: &Path) -> ModelResult<crate::git::GitHeadState> {
            Ok(crate::git::GitHeadState {
                branch: Some("main".to_owned()),
                commit: Some(TEST_COMMIT.to_owned()),
                is_detached: false,
            })
        }

        fn remotes(&self, _path: &Path) -> ModelResult<Vec<crate::git::GitRemote>> {
            Ok(Vec::new())
        }

        fn add_remote(
            &self,
            _path: &Path,
            name: &str,
            url: &str,
        ) -> ModelResult<crate::git::GitRemoteResult> {
            Ok(crate::git::GitRemoteResult {
                remote: crate::git::GitRemote {
                    name: name.to_owned(),
                    url: Some(url.to_owned()),
                    push_url: None,
                },
            })
        }

        fn push(
            &self,
            _path: &Path,
            remote: &str,
            refspec: &str,
        ) -> ModelResult<crate::git::GitPushResult> {
            self.push.run();
            Ok(crate::git::GitPushResult {
                remote: remote.to_owned(),
                refspec: refspec.to_owned(),
            })
        }

        fn read_ref(&self, _path: &Path, _ref_spec: &str) -> ModelResult<Option<String>> {
            Ok(Some(TEST_COMMIT.to_owned()))
        }

        fn is_ancestor(
            &self,
            _path: &Path,
            _ancestor: &str,
            _descendant: &str,
        ) -> ModelResult<bool> {
            Ok(true)
        }
    }

    #[test]
    fn create_workspace_writes_empty_manifest_and_lock() {
        let temp = TempDir::new("create-workspace");
        let backend = Git2Backend::new();
        let response =
            handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Ok
        );
        assert!(response.response.members.is_empty());
        assert!(backend.is_repository(temp.path()).unwrap());
        assert!(temp.path().join("gwz.conf/gwz.yml").is_file());
        assert!(temp.path().join("gwz.conf/gwz.lock.yml").is_file());
        assert!(!temp.path().join("workspace").exists());
        let root_status = backend.status(temp.path()).unwrap();
        assert_eq!(root_status.untracked, 0);
        assert!(
            root_status
                .files
                .iter()
                .any(|file| { file.path == "gwz.conf/gwz.yml" && file.index_status == "A" })
        );
        assert!(
            root_status
                .files
                .iter()
                .any(|file| { file.path == "gwz.conf/gwz.lock.yml" && file.index_status == "A" })
        );
        assert_eq!(read_manifest(temp.path()).unwrap().members.len(), 0);
        assert_eq!(read_lock(temp.path()).unwrap().members.len(), 0);
    }

    #[test]
    fn create_workspace_rejects_existing_and_nested_workspaces() {
        let temp = TempDir::new("reject-workspace");
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();

        assert_eq!(
            handle_create_workspace(create_workspace_request(temp.path()), "op_create")
                .unwrap_err()
                .code,
            ErrorCode::WorkspaceAlreadyExists
        );

        let child = temp.path().join("repos/child");
        fs::create_dir_all(&child).unwrap();
        assert_eq!(
            handle_create_workspace(create_workspace_request(&child), "op_create")
                .unwrap_err()
                .code,
            ErrorCode::NestedWorkspace
        );
    }

    #[test]
    fn create_repo_writes_manifest_lock_and_empty_git_repo() {
        let temp = TempDir::new("create-repo");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();

        let response = handle_create_repo(
            &backend,
            temp.path(),
            create_repo_request("repos/app", None, None),
            "op_repo",
        )
        .unwrap();

        let member = response.response.members.single();
        assert_eq!(member.status, crate::MemberStatus::Ok);
        assert_eq!(member.state.as_ref().unwrap().member_id, "mem_app");
        assert_eq!(member.state.as_ref().unwrap().commit, None);
        assert_eq!(
            member.state.as_ref().unwrap().branch,
            Some("main".to_owned())
        );
        assert!(
            backend
                .is_repository(&temp.path().join("repos/app"))
                .unwrap()
        );

        let manifest = read_manifest(temp.path()).unwrap();
        assert_eq!(manifest.members.len(), 1);
        assert_eq!(manifest.members[0].id, "mem_app");
        assert_eq!(manifest.members[0].source_id, "src_app");
        assert_eq!(
            manifest.members[0]
                .desired
                .as_ref()
                .and_then(|desired| desired.local_only),
            Some(true)
        );

        let lock = read_lock(temp.path()).unwrap();
        let locked = lock.members.get("mem_app").unwrap();
        assert_eq!(locked.commit, None);
        assert_eq!(locked.branch, Some("main".to_owned()));
        assert_eq!(locked.dirty, Some(false));
        assert_eq!(locked.materialized, Some(true));
    }

    #[test]
    fn add_existing_repo_records_current_git_state_and_remotes_without_reclone() {
        let temp = TempDir::new("add-existing");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let repo_path = temp.path().join("repos/existing");
        backend.create_repo(&repo_path).unwrap();
        let commit = commit_file(&repo_path, "README.md", "one", "initial", &[]).unwrap();
        backend
            .add_remote(&repo_path, "origin", "file:///tmp/existing.git")
            .unwrap();
        fs::write(repo_path.join("README.md"), "dirty").unwrap();

        let response = handle_add_existing_repo(
            &backend,
            temp.path(),
            crate::AddExistingRepoRequest {
                meta: request_meta_with_workspace(),
                repository_path: repo_path.to_string_lossy().into_owned(),
                member_path: None,
                member_id: None,
                source_id: None,
            },
            "op_add",
        )
        .unwrap();

        let member = response.response.members.single();
        assert_eq!(member.member_path, "repos/existing");
        assert_eq!(member.state.as_ref().unwrap().commit, Some(commit.clone()));
        assert_eq!(member.state.as_ref().unwrap().dirty, Some(true));
        assert!(repo_path.join(".git").is_dir());

        let manifest = read_manifest(temp.path()).unwrap();
        assert_eq!(
            manifest.members[0].remotes[0].url,
            "file:///tmp/existing.git"
        );
        let locked = read_lock(temp.path())
            .unwrap()
            .members
            .get("mem_existing")
            .cloned()
            .unwrap();
        assert_eq!(locked.commit, Some(commit));
        assert_eq!(locked.dirty, Some(true));
    }

    #[test]
    fn add_existing_repo_accepts_relative_path_inside_workspace() {
        let temp = TempDir::new("add-existing-relative");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let repo_path = temp.path().join("local-repo");
        backend.create_repo(&repo_path).unwrap();
        commit_file(&repo_path, "README.md", "one", "initial", &[]).unwrap();
        let start = temp.path().join("gwz.conf");

        let response = handle_add_existing_repo(
            &backend,
            &start,
            crate::AddExistingRepoRequest {
                meta: request_meta_with_workspace(),
                repository_path: "../local-repo".to_owned(),
                member_path: None,
                member_id: None,
                source_id: None,
            },
            "op_add",
        )
        .unwrap();

        assert_eq!(response.response.members.single().member_path, "local-repo");
        let manifest = read_manifest(temp.path()).unwrap();
        assert_eq!(manifest.members[0].path, "local-repo");
    }

    #[test]
    fn init_from_sources_derives_default_paths_and_rejects_collisions() {
        let temp = TempDir::new("init-sources");
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();

        let backend = Git2Backend::new();
        let response = handle_init_from_sources(
            &backend,
            temp.path(),
            crate::InitFromSourcesRequest {
                meta: request_meta(),
                workspace_root: temp.path().to_string_lossy().into_owned(),
                sources: vec![
                    crate::SourceUrl {
                        url: "git@github.com:org/repo-a.git".to_owned(),
                        path: None,
                        remote_name: None,
                        branch: None,
                    },
                    crate::SourceUrl {
                        url: "https://github.com/org/repo-b".to_owned(),
                        path: None,
                        remote_name: Some("github".to_owned()),
                        branch: Some("main".to_owned()),
                    },
                ],
                target: None,
                workspace_id: Some("ws_ops".to_owned()),
            },
            "op_init",
            &NullSink,
        )
        .unwrap();

        assert_eq!(response.response.members[0].member_path, "repo-a");
        assert_eq!(response.response.members[1].member_path, "repo-b");
        assert_eq!(
            response.response.members[0]
                .planned
                .as_ref()
                .unwrap()
                .action,
            crate::PlannedAction::Clone
        );

        let collision = handle_init_from_sources(
            &backend,
            temp.path(),
            crate::InitFromSourcesRequest {
                meta: request_meta(),
                workspace_root: temp.path().to_string_lossy().into_owned(),
                sources: vec![
                    crate::SourceUrl {
                        url: "https://example.invalid/dup.git".to_owned(),
                        path: None,
                        remote_name: None,
                        branch: None,
                    },
                    crate::SourceUrl {
                        url: "ssh://example.invalid/dup".to_owned(),
                        path: None,
                        remote_name: None,
                        branch: None,
                    },
                ],
                target: None,
                workspace_id: Some("ws_ops".to_owned()),
            },
            "op_init",
            &NullSink,
        )
        .unwrap_err();
        assert_eq!(collision.code, ErrorCode::PathCollision);
    }

    #[test]
    fn init_from_sources_derives_default_paths_from_windows_local_paths() {
        let manifest = crate::artifact::ManifestArtifact {
            schema: crate::artifact::WORKSPACE_SCHEMA.to_owned(),
            workspace: crate::artifact::WorkspaceHeader {
                id: "ws_ops".to_owned(),
            },
            members: Vec::new(),
        };

        let plans = init_source_plans(
            &manifest,
            &[crate::SourceUrl {
                url: r"C:\Users\runneradmin\AppData\Local\Temp\remote.git".to_owned(),
                path: None,
                remote_name: None,
                branch: None,
            }],
        )
        .unwrap();

        assert_eq!(plans[0].path.as_str(), "remote");
        assert_eq!(plans[0].member_id, "mem_remote");
        assert_eq!(plans[0].source_id, "src_remote");
    }

    #[test]
    fn init_from_sources_can_create_workspace_clone_local_urls_and_write_lock() {
        let temp = TempDir::new("init-exec");
        let backend = Git2Backend::new();
        let fixture = RemoteFixture::new("init-exec-source");
        let commit = fixture.commit_and_push("README.md", "one", "initial", &backend);

        let events = CollectingSink::default();
        let response = handle_init_from_sources(
            &backend,
            temp.path(),
            crate::InitFromSourcesRequest {
                meta: request_meta(),
                workspace_root: temp.path().to_string_lossy().into_owned(),
                sources: vec![crate::SourceUrl {
                    url: fixture.remote_url().to_owned(),
                    path: None,
                    remote_name: None,
                    branch: None,
                }],
                target: None,
                workspace_id: Some("ws_ops".to_owned()),
            },
            "op_init",
            &events,
        )
        .unwrap();

        // init emits the per-member lifecycle, bracketed by operation events.
        let kinds: Vec<_> = events.take().into_iter().map(|event| event.kind).collect();
        assert_eq!(kinds.first(), Some(&crate::EventKind::OperationStarted));
        assert_eq!(kinds.last(), Some(&crate::EventKind::OperationFinished));
        assert!(kinds.contains(&crate::EventKind::MemberStarted));
        assert!(kinds.contains(&crate::EventKind::MemberFinished));

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Ok
        );
        assert!(backend.is_repository(temp.path()).unwrap());
        assert!(temp.path().join("gwz.conf/gwz.yml").is_file());
        assert!(temp.path().join("gwz.conf/gwz.lock.yml").is_file());
        assert!(!temp.path().join("workspace").exists());
        let ignore = fs::read_to_string(temp.path().join(".gitignore")).unwrap();
        assert!(ignore.contains("/remote/"));
        let root_status = backend.status(temp.path()).unwrap();
        assert_eq!(root_status.untracked, 0);
        assert!(
            root_status
                .files
                .iter()
                .any(|file| { file.path == ".gitignore" && file.index_status == "A" })
        );
        assert!(
            root_status
                .files
                .iter()
                .any(|file| { file.path == "gwz.conf/gwz.yml" && file.index_status == "A" })
        );
        assert_eq!(
            backend.head(&temp.path().join("remote")).unwrap().commit,
            Some(commit.clone())
        );
        let manifest = read_manifest(temp.path()).unwrap();
        assert_eq!(manifest.members[0].path, "remote");
        assert_eq!(manifest.members[0].remotes[0].name, "origin");
        assert_eq!(
            read_lock(temp.path()).unwrap().members["mem_remote"].commit,
            Some(commit)
        );
    }

    #[test]
    fn snapshot_and_tag_write_selected_member_records_with_attribution() {
        let temp = TempDir::new("snapshot-tag");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        handle_create_repo(
            &backend,
            temp.path(),
            create_repo_request("repos/app", None, None),
            "op_repo",
        )
        .unwrap();
        let lock_before = read_lock(temp.path()).unwrap();

        let snapshot_response = handle_snapshot(
            temp.path(),
            crate::SnapshotRequest {
                meta: request_meta_with_actor_selection("agent://tester", &["mem_app"]),
                snapshot_id: "snap_one".to_owned(),
            },
            "op_snapshot",
        )
        .unwrap();
        let tag_response = handle_tag(
            temp.path(),
            crate::TagRequest {
                meta: request_meta_with_actor_selection("agent://tester", &["mem_app"]),
                tag_name: "release-one".to_owned(),
            },
            "op_tag",
        )
        .unwrap();

        assert_eq!(
            snapshot_response.response.members.single().member_id,
            "mem_app"
        );
        assert_eq!(tag_response.response.members.single().member_id, "mem_app");
        let snapshot = read_snapshot(temp.path(), "snap_one").unwrap();
        assert_eq!(snapshot.created_by.actor_id, "agent://tester");
        assert_eq!(snapshot.selected_members, vec!["mem_app"]);
        assert!(snapshot.members.contains_key("mem_app"));
        let tag = read_tag(temp.path(), "release-one").unwrap();
        assert_eq!(tag.created_by.actor_id, "agent://tester");
        assert!(tag.members.contains_key("mem_app"));
        assert_eq!(read_lock(temp.path()).unwrap(), lock_before);
    }

    #[test]
    fn duplicate_and_invalid_gwz_tags_fail_cleanly() {
        let temp = TempDir::new("tag-errors");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        handle_create_repo(
            &backend,
            temp.path(),
            create_repo_request("repos/app", None, None),
            "op_repo",
        )
        .unwrap();
        let request = crate::TagRequest {
            meta: request_meta_with_actor_selection("agent://tester", &["mem_app"]),
            tag_name: "release-one".to_owned(),
        };
        handle_tag(temp.path(), request.clone(), "op_tag").unwrap();

        assert_eq!(
            handle_tag(temp.path(), request, "op_tag").unwrap_err().code,
            ErrorCode::TagInvalid
        );
        assert_eq!(
            handle_tag(
                temp.path(),
                crate::TagRequest {
                    meta: request_meta_with_actor_selection("agent://tester", &["mem_app"]),
                    tag_name: "bad/name".to_owned(),
                },
                "op_tag",
            )
            .unwrap_err()
            .code,
            ErrorCode::TagInvalid
        );
    }

    #[test]
    fn materialize_lock_clones_missing_member_and_checks_out_recorded_commit() {
        let temp = TempDir::new("materialize-clone");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let fixture = RemoteFixture::new("clone-source");
        let commit = fixture.commit_and_push("README.md", "one", "initial", &backend);
        write_materialize_fixture(temp.path(), fixture.remote_url(), &commit);

        let events = CollectingSink::default();
        let response = handle_materialize(
            &backend,
            temp.path(),
            materialize_lock_request(false),
            "op_materialize",
            &events,
        )
        .unwrap();

        assert_eq!(
            response.response.members.single().status,
            crate::MemberStatus::Ok
        );
        assert_eq!(
            backend.head(&temp.path().join("repos/app")).unwrap().commit,
            Some(commit)
        );

        // The clone-missing materialize emits a per-member lifecycle bracketed
        // by operation_started/finished. Transfer-progress volume depends on
        // libgit2's local-clone behavior, so assert the deterministic envelope
        // and that any emitted progress is well-formed for this member.
        let collected = events.take();
        let kinds: Vec<_> = collected.iter().map(|event| event.kind).collect();
        assert_eq!(kinds.first(), Some(&crate::EventKind::OperationStarted));
        assert_eq!(kinds.last(), Some(&crate::EventKind::OperationFinished));
        let started = collected
            .iter()
            .position(|event| event.kind == crate::EventKind::MemberStarted)
            .expect("member_started emitted");
        let finished = collected
            .iter()
            .position(|event| event.kind == crate::EventKind::MemberFinished)
            .expect("member_finished emitted");
        assert!(
            started < finished,
            "member_started precedes member_finished"
        );
        assert_eq!(collected[started].member_path.as_deref(), Some("repos/app"));
        for event in &collected {
            if event.kind == crate::EventKind::MemberProgress {
                assert_eq!(event.member_path.as_deref(), Some("repos/app"));
                let progress = event.progress.as_ref().expect("progress payload present");
                assert!(matches!(
                    progress.phase,
                    crate::GitProgressPhase::Receiving | crate::GitProgressPhase::Resolving
                ));
            }
        }
    }

    #[test]
    fn clone_workspace_clones_root_and_materializes_missing_members() {
        let temp = TempDir::new("clone-workspace");
        let backend = Git2Backend::new();
        // Build a source workspace whose root repo commits gwz.conf, with a
        // member that lives at a remote and is absent from the root tree.
        let source_ws = temp.path().join("origin");
        fs::create_dir_all(&source_ws).unwrap();
        handle_create_workspace(create_workspace_request(&source_ws), "op_create").unwrap();
        let fixture = RemoteFixture::new("clone-workspace-member");
        let commit = fixture.commit_and_push("README.md", "one", "initial", &backend);
        write_materialize_fixture(&source_ws, fixture.remote_url(), &commit);
        commit_workspace_root(&source_ws);

        // Clone the workspace from its root URL into a fresh target.
        let target = temp.path().join("clone");
        let response = handle_clone_workspace(
            &backend,
            request_meta(),
            source_ws.to_str().unwrap(),
            target.to_str().unwrap(),
            "op_clone",
            &NullSink,
        )
        .unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Ok
        );
        assert_eq!(
            response.response.members.single().status,
            crate::MemberStatus::Ok
        );
        // gwz.conf came over with the clone, and the member was materialized.
        assert!(target.join(crate::artifact::LOCK_PATH).is_file());
        assert_eq!(
            backend.head(&target.join("repos/app")).unwrap().commit,
            Some(commit)
        );
    }

    #[test]
    fn clone_workspace_rejects_url_that_is_not_a_workspace() {
        let temp = TempDir::new("clone-non-workspace");
        let backend = Git2Backend::new();
        let fixture = RemoteFixture::new("clone-non-workspace-source");
        fixture.commit_and_push("README.md", "one", "initial", &backend);
        let target = temp.path().join("clone");

        let err = handle_clone_workspace(
            &backend,
            request_meta(),
            fixture.remote_url(),
            target.to_str().unwrap(),
            "op_clone",
            &NullSink,
        )
        .unwrap_err();

        assert_eq!(err.code, ErrorCode::WorkspaceNotFound);
    }

    #[test]
    fn materialize_lock_blocks_dirty_member_by_default() {
        let temp = TempDir::new("materialize-dirty");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let fixture = RemoteFixture::new("dirty-source");
        let first = fixture.commit_and_push("README.md", "one", "initial", &backend);
        let second = fixture.commit_and_push("README.md", "two", "second", &backend);
        write_materialize_fixture(temp.path(), fixture.remote_url(), &first);
        backend
            .clone_repo(fixture.remote_url(), &temp.path().join("repos/app"))
            .unwrap();
        fs::write(temp.path().join("repos/app/README.md"), "dirty").unwrap();

        let err = handle_materialize(
            &backend,
            temp.path(),
            materialize_lock_request(false),
            "op_materialize",
            &NullSink,
        )
        .unwrap_err();

        assert_eq!(err.code, ErrorCode::DirtyMember);
        assert_eq!(
            backend.head(&temp.path().join("repos/app")).unwrap().commit,
            Some(second)
        );
    }

    #[test]
    fn materialize_lock_moves_clean_member_and_dry_run_does_not_mutate() {
        let temp = TempDir::new("materialize-clean");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let fixture = RemoteFixture::new("clean-source");
        let first = fixture.commit_and_push("README.md", "one", "initial", &backend);
        let second = fixture.commit_and_push("README.md", "two", "second", &backend);
        write_materialize_fixture(temp.path(), fixture.remote_url(), &first);
        backend
            .clone_repo(fixture.remote_url(), &temp.path().join("repos/app"))
            .unwrap();

        let dry_run = handle_materialize(
            &backend,
            temp.path(),
            materialize_lock_request(true),
            "op_materialize",
            &NullSink,
        )
        .unwrap();
        assert_eq!(
            dry_run
                .response
                .members
                .single()
                .planned
                .as_ref()
                .unwrap()
                .action,
            crate::PlannedAction::Checkout
        );
        assert_eq!(
            backend.head(&temp.path().join("repos/app")).unwrap().commit,
            Some(second)
        );

        handle_materialize(
            &backend,
            temp.path(),
            materialize_lock_request(false),
            "op_materialize",
            &NullSink,
        )
        .unwrap();
        assert_eq!(
            backend.head(&temp.path().join("repos/app")).unwrap().commit,
            Some(first)
        );
    }

    #[test]
    fn materialize_snapshot_and_tag_rewrite_lock_after_success() {
        let temp = TempDir::new("materialize-snapshot-tag");
        let backend = Git2Backend::new();
        let fixture = materialize_snapshot_fixture(temp.path(), &backend);

        handle_materialize(
            &backend,
            temp.path(),
            materialize_named_request(crate::MaterializeTargetKind::Snapshot, "snap_first"),
            "op_materialize",
            &NullSink,
        )
        .unwrap();
        assert_eq!(
            read_lock(temp.path()).unwrap().members["mem_app"].commit,
            Some(fixture.first.clone())
        );

        write_materialize_fixture(temp.path(), fixture.remote_url(), &fixture.second);
        handle_materialize(
            &backend,
            temp.path(),
            materialize_named_request(crate::MaterializeTargetKind::Tag, "tag_first"),
            "op_materialize",
            &NullSink,
        )
        .unwrap();
        assert_eq!(
            read_lock(temp.path()).unwrap().members["mem_app"].commit,
            Some(fixture.first)
        );
    }

    #[test]
    fn pull_snapshot_rewrites_lock_after_success() {
        let temp = TempDir::new("pull-snapshot");
        let backend = Git2Backend::new();
        let fixture = materialize_snapshot_fixture(temp.path(), &backend);

        handle_pull_snapshot(
            &backend,
            temp.path(),
            crate::PullSnapshotRequest {
                meta: request_meta_with_workspace(),
                snapshot_id: "snap_first".to_owned(),
            },
            "op_pull_snapshot",
            &NullSink,
        )
        .unwrap();

        assert_eq!(
            read_lock(temp.path()).unwrap().members["mem_app"].commit,
            Some(fixture.first)
        );
    }

    #[test]
    fn missing_snapshot_or_tag_fails_before_mutation() {
        let temp = TempDir::new("missing-target");
        let backend = Git2Backend::new();
        let fixture = materialize_snapshot_fixture(temp.path(), &backend);

        assert_eq!(
            handle_materialize(
                &backend,
                temp.path(),
                materialize_named_request(crate::MaterializeTargetKind::Snapshot, "missing"),
                "op_materialize",
                &NullSink,
            )
            .unwrap_err()
            .code,
            ErrorCode::SnapshotNotFound
        );
        assert_eq!(
            handle_materialize(
                &backend,
                temp.path(),
                materialize_named_request(crate::MaterializeTargetKind::Tag, "missing"),
                "op_materialize",
                &NullSink,
            )
            .unwrap_err()
            .code,
            ErrorCode::TagNotFound
        );
        assert_eq!(
            backend.head(&temp.path().join("repos/app")).unwrap().commit,
            Some(fixture.second)
        );
    }

    #[test]
    fn pull_head_returns_noop_for_local_only_member() {
        let temp = TempDir::new("pull-local-only");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        handle_create_repo(
            &backend,
            temp.path(),
            create_repo_request("repos/app", None, None),
            "op_repo",
        )
        .unwrap();

        let response =
            handle_pull_head(&backend, temp.path(), pull_head_request(), "op_pull").unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Noop
        );
        assert_eq!(
            response.response.members.single().status,
            crate::MemberStatus::Noop
        );
    }

    #[test]
    fn pull_head_noops_member_without_fetch_remote_and_continues() {
        let temp = TempDir::new("pull-no-fetch-remote");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();

        let local_path = temp.path().join("local-repo");
        backend.create_repo(&local_path).unwrap();
        commit_file(&local_path, "README.md", "one", "initial", &[]).unwrap();
        handle_add_existing_repo(
            &backend,
            temp.path(),
            crate::AddExistingRepoRequest {
                meta: request_meta_with_workspace(),
                repository_path: local_path.to_string_lossy().into_owned(),
                member_path: None,
                member_id: None,
                source_id: None,
            },
            "op_add_local",
        )
        .unwrap();

        let fixture = RemoteFixture::new("pull-no-fetch-source");
        fixture.commit_and_push("README.md", "one", "initial", &backend);
        let remote_path = temp.path().join("remote");
        backend
            .clone_repo(fixture.remote_url(), &remote_path)
            .unwrap();
        handle_add_existing_repo(
            &backend,
            temp.path(),
            crate::AddExistingRepoRequest {
                meta: request_meta_with_workspace(),
                repository_path: remote_path.to_string_lossy().into_owned(),
                member_path: None,
                member_id: None,
                source_id: None,
            },
            "op_add_remote",
        )
        .unwrap();

        let response =
            handle_pull_head(&backend, temp.path(), pull_head_request(), "op_pull").unwrap();

        assert_eq!(response.response.members.len(), 2);
        let local = response
            .response
            .members
            .iter()
            .find(|member| member.member_path == "local-repo")
            .unwrap();
        assert_eq!(local.status, crate::MemberStatus::Noop);
        assert_eq!(
            local
                .planned
                .as_ref()
                .and_then(|planned| planned.message.as_deref()),
            Some("no fetch remote configured; skipping pull")
        );
        let remote = response
            .response
            .members
            .iter()
            .find(|member| member.member_path == "remote")
            .unwrap();
        assert_eq!(remote.status, crate::MemberStatus::Noop);
    }

    #[test]
    fn pull_head_fast_forwards_clean_member_and_rewrites_lock() {
        let temp = TempDir::new("pull-ff");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let fixture = RemoteFixture::new("pull-ff-source");
        let first = fixture.commit_and_push("README.md", "one", "initial", &backend);
        backend
            .clone_repo(fixture.remote_url(), &temp.path().join("repos/app"))
            .unwrap();
        let second = fixture.commit_and_push("README.md", "two", "second", &backend);
        write_pull_fixture(
            temp.path(),
            vec![("mem_app", "repos/app", fixture.remote_url(), &first)],
        );

        let response =
            handle_pull_head(&backend, temp.path(), pull_head_request(), "op_pull").unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Ok
        );
        assert_eq!(
            backend.head(&temp.path().join("repos/app")).unwrap().commit,
            Some(second.clone())
        );
        assert_eq!(
            read_lock(temp.path()).unwrap().members["mem_app"].commit,
            Some(second)
        );
    }

    #[test]
    fn pull_head_fetches_selected_members_in_parallel() {
        let temp = TempDir::new("pull-parallel");
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let backend = TrackingBackend::new(2);
        write_pull_fixture(
            temp.path(),
            vec![
                (
                    "mem_app",
                    "repos/app",
                    "ssh://one.invalid/app.git",
                    TEST_COMMIT,
                ),
                (
                    "mem_lib",
                    "repos/lib",
                    "ssh://two.invalid/lib.git",
                    TEST_COMMIT,
                ),
            ],
        );

        let response = handle_pull_head_with_events(
            &backend,
            temp.path(),
            pull_head_request(),
            "op_pull",
            &NullSink,
        )
        .unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Noop
        );
        assert_eq!(backend.fetch_peak(), 2);
    }

    #[test]
    fn pull_head_dirty_member_blocks_all_selected_members_before_mutation() {
        let temp = TempDir::new("pull-dirty");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();

        let good = RemoteFixture::new("pull-dirty-good");
        let good_first = good.commit_and_push("README.md", "one", "initial", &backend);
        backend
            .clone_repo(good.remote_url(), &temp.path().join("repos/good"))
            .unwrap();
        let good_second = good.commit_and_push("README.md", "two", "second", &backend);

        let dirty = RemoteFixture::new("pull-dirty-bad");
        let dirty_first = dirty.commit_and_push("README.md", "one", "initial", &backend);
        backend
            .clone_repo(dirty.remote_url(), &temp.path().join("repos/dirty"))
            .unwrap();
        fs::write(temp.path().join("repos/dirty/README.md"), "dirty").unwrap();

        write_pull_fixture(
            temp.path(),
            vec![
                ("mem_good", "repos/good", good.remote_url(), &good_first),
                ("mem_dirty", "repos/dirty", dirty.remote_url(), &dirty_first),
            ],
        );
        let lock_before = read_lock(temp.path()).unwrap();

        let err =
            handle_pull_head(&backend, temp.path(), pull_head_request(), "op_pull").unwrap_err();

        assert_eq!(err.code, ErrorCode::DirtyMember);
        assert_eq!(
            backend
                .head(&temp.path().join("repos/good"))
                .unwrap()
                .commit,
            Some(good_first)
        );
        assert_ne!(
            backend
                .head(&temp.path().join("repos/good"))
                .unwrap()
                .commit,
            Some(good_second)
        );
        assert_eq!(read_lock(temp.path()).unwrap(), lock_before);
    }

    #[test]
    fn pull_head_divergence_blocks_all_selected_members_before_branch_mutation() {
        let temp = TempDir::new("pull-atomic");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();

        let good = RemoteFixture::new("pull-good");
        let good_first = good.commit_and_push("README.md", "one", "initial", &backend);
        backend
            .clone_repo(good.remote_url(), &temp.path().join("repos/good"))
            .unwrap();
        let good_second = good.commit_and_push("README.md", "two", "second", &backend);

        let bad = RemoteFixture::new("pull-bad");
        let bad_first = bad.commit_and_push("README.md", "one", "initial", &backend);
        backend
            .clone_repo(bad.remote_url(), &temp.path().join("repos/bad"))
            .unwrap();
        let bad_parent = git2::Oid::from_str(&bad_first).unwrap();
        let bad_local = commit_file(
            &temp.path().join("repos/bad"),
            "README.md",
            "local",
            "local",
            &[bad_parent],
        )
        .unwrap();
        bad.commit_and_push("README.md", "remote", "remote", &backend);

        write_pull_fixture(
            temp.path(),
            vec![
                ("mem_good", "repos/good", good.remote_url(), &good_first),
                ("mem_bad", "repos/bad", bad.remote_url(), &bad_local),
            ],
        );

        let err =
            handle_pull_head(&backend, temp.path(), pull_head_request(), "op_pull").unwrap_err();

        assert_eq!(err.code, ErrorCode::DivergedMember);
        assert_eq!(
            backend
                .head(&temp.path().join("repos/good"))
                .unwrap()
                .commit,
            Some(good_first)
        );
        assert_ne!(
            backend
                .head(&temp.path().join("repos/good"))
                .unwrap()
                .commit,
            Some(good_second)
        );
        assert_eq!(
            backend.head(&temp.path().join("repos/bad")).unwrap().commit,
            Some(bad_local)
        );
    }

    #[test]
    fn push_selected_member_to_local_bare_remote_succeeds() {
        let temp = TempDir::new("push-success");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let remote = temp.path().join("remote.git");
        init_bare_main(&remote);
        let repo_path = temp.path().join("repos/app");
        backend.create_repo(&repo_path).unwrap();
        backend
            .add_remote(&repo_path, "origin", remote.to_str().unwrap())
            .unwrap();
        let commit = commit_file(&repo_path, "README.md", "one", "initial", &[]).unwrap();
        write_pull_fixture(
            temp.path(),
            vec![("mem_app", "repos/app", remote.to_str().unwrap(), &commit)],
        );

        let response =
            handle_push(&backend, temp.path(), push_request(None, None), "op_push").unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Ok
        );
        assert_eq!(
            response.response.members.single().status,
            crate::MemberStatus::Ok
        );
        assert_eq!(read_repo_ref(&remote, "refs/heads/main"), Some(commit));
    }

    #[test]
    fn push_runs_selected_members_in_parallel() {
        let temp = TempDir::new("push-parallel");
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let backend = TrackingBackend::new(2);
        write_pull_fixture(
            temp.path(),
            vec![
                (
                    "mem_app",
                    "repos/app",
                    "ssh://one.invalid/app.git",
                    TEST_COMMIT,
                ),
                (
                    "mem_lib",
                    "repos/lib",
                    "ssh://two.invalid/lib.git",
                    TEST_COMMIT,
                ),
            ],
        );

        let response = handle_push_with_events(
            &backend,
            temp.path(),
            push_request(None, None),
            "op_push",
            &NullSink,
        )
        .unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Ok
        );
        assert_eq!(backend.push_peak(), 2);
    }

    #[test]
    fn push_honors_request_remote_and_refspec() {
        let temp = TempDir::new("push-refspec");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let remote = temp.path().join("publish.git");
        init_bare_main(&remote);
        let repo_path = temp.path().join("repos/app");
        backend.create_repo(&repo_path).unwrap();
        backend
            .add_remote(&repo_path, "publish", remote.to_str().unwrap())
            .unwrap();
        let commit = commit_file(&repo_path, "README.md", "one", "initial", &[]).unwrap();
        write_pull_fixture(
            temp.path(),
            vec![("mem_app", "repos/app", remote.to_str().unwrap(), &commit)],
        );

        let response = handle_push(
            &backend,
            temp.path(),
            push_request_explicit(
                Some("publish"),
                Some("refs/heads/main:refs/heads/published"),
            ),
            "op_push",
        )
        .unwrap();

        assert_eq!(
            response.response.members.single().status,
            crate::MemberStatus::Ok
        );
        assert_eq!(read_repo_ref(&remote, "refs/heads/main"), None);
        assert_eq!(read_repo_ref(&remote, "refs/heads/published"), Some(commit));
    }

    #[test]
    fn push_local_only_member_without_remote_fails_or_skips_by_policy() {
        let temp = TempDir::new("push-local-only");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        handle_create_repo(
            &backend,
            temp.path(),
            create_repo_request("repos/app", None, None),
            "op_repo",
        )
        .unwrap();

        let failed =
            handle_push(&backend, temp.path(), push_request(None, None), "op_push").unwrap();
        assert_eq!(
            failed.response.meta.aggregate_status,
            crate::AggregateStatus::Rejected
        );
        assert_eq!(
            failed.response.members.single().status,
            crate::MemberStatus::Rejected
        );
        assert_eq!(
            failed
                .response
                .members
                .single()
                .error
                .as_ref()
                .unwrap()
                .code,
            crate::GwzErrorCode::MissingRemote
        );

        let skipped = handle_push(
            &backend,
            temp.path(),
            push_request(Some(crate::UnsupportedMemberBehavior::Skip), None),
            "op_push",
        )
        .unwrap();
        assert_eq!(
            skipped.response.meta.aggregate_status,
            crate::AggregateStatus::Noop
        );
        assert_eq!(
            skipped.response.members.single().status,
            crate::MemberStatus::Skipped
        );
    }

    #[test]
    fn push_remote_rejection_is_reported_per_member() {
        let temp = TempDir::new("push-reject");
        let backend = Git2Backend::new();
        handle_create_workspace(create_workspace_request(temp.path()), "op_create").unwrap();
        let fixture = RemoteFixture::new("push-reject-source");
        let first = fixture.commit_and_push("README.md", "one", "initial", &backend);
        backend
            .clone_repo(fixture.remote_url(), &temp.path().join("repos/app"))
            .unwrap();
        let remote_second = fixture.commit_and_push("README.md", "two", "second", &backend);
        let first_oid = git2::Oid::from_str(&first).unwrap();
        let local = commit_file(
            &temp.path().join("repos/app"),
            "README.md",
            "local",
            "local",
            &[first_oid],
        )
        .unwrap();
        write_pull_fixture(
            temp.path(),
            vec![("mem_app", "repos/app", fixture.remote_url(), &local)],
        );

        let response =
            handle_push(&backend, temp.path(), push_request(None, None), "op_push").unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Failed
        );
        let member = response.response.members.single();
        assert_eq!(member.status, crate::MemberStatus::Failed);
        assert_eq!(
            member.error.as_ref().unwrap().code,
            crate::GwzErrorCode::RemoteRejected
        );
        assert_eq!(
            read_repo_ref(Path::new(fixture.remote_url()), "refs/heads/main"),
            Some(remote_second)
        );
    }

    fn create_workspace_request(root: &Path) -> crate::CreateWorkspaceRequest {
        crate::CreateWorkspaceRequest {
            meta: request_meta(),
            workspace_root: root.to_string_lossy().into_owned(),
            workspace_id: Some("ws_ops".to_owned()),
        }
    }

    fn create_repo_request(
        member_path: &str,
        member_id: Option<&str>,
        source_id: Option<&str>,
    ) -> crate::CreateRepoRequest {
        crate::CreateRepoRequest {
            meta: request_meta_with_workspace(),
            member_path: member_path.to_owned(),
            initial_branch: None,
            member_id: member_id.map(ToOwned::to_owned),
            source_id: source_id.map(ToOwned::to_owned),
        }
    }

    fn commit_file(
        repo_path: &Path,
        relative_path: &str,
        content: &str,
        message: &str,
        parents: &[git2::Oid],
    ) -> Result<String, git2::Error> {
        fs::write(repo_path.join(relative_path), content).unwrap();
        let repo = git2::Repository::open(repo_path)?;
        let mut index = repo.index()?;
        index.add_path(Path::new(relative_path))?;
        index.write()?;
        let tree_id = index.write_tree()?;
        let tree = repo.find_tree(tree_id)?;
        let signature = git2::Signature::now("GWZ Test", "gwz@example.invalid")?;
        let parent_commits = parents
            .iter()
            .map(|id| repo.find_commit(*id))
            .collect::<Result<Vec<_>, _>>()?;
        let parent_refs = parent_commits.iter().collect::<Vec<_>>();
        Ok(repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parent_refs,
            )?
            .to_string())
    }

    fn commit_workspace_root(root: &Path) {
        let repo = git2::Repository::open(root).unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["."], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("GWZ Test", "gwz@example.invalid").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "init workspace",
            &tree,
            &[],
        )
        .unwrap();
    }

    fn request_meta() -> crate::RequestMeta {
        crate::RequestMeta {
            request_id: "req_ops".to_owned(),
            schema_version: "gwz.protocol/v0".to_owned(),
            ..Default::default()
        }
    }

    fn request_meta_with_workspace() -> crate::RequestMeta {
        crate::RequestMeta {
            workspace: Some(crate::WorkspaceRef {
                root: None,
                workspace_id: Some("ws_ops".to_owned()),
            }),
            ..request_meta()
        }
    }

    fn request_meta_with_actor_selection(
        actor_id: &str,
        member_ids: &[&str],
    ) -> crate::RequestMeta {
        crate::RequestMeta {
            selection: Some(crate::Selection {
                all: Some(false),
                member_ids: member_ids.iter().map(|value| (*value).to_owned()).collect(),
                paths: Vec::new(),
            }),
            attribution: Some(crate::OperationAttribution {
                actor: Some(crate::OperationActor {
                    actor_id: actor_id.to_owned(),
                    display_name: None,
                    email: None,
                    authority: None,
                }),
                ..Default::default()
            }),
            ..request_meta_with_workspace()
        }
    }

    fn materialize_lock_request(dry_run: bool) -> crate::MaterializeRequest {
        crate::MaterializeRequest {
            meta: crate::RequestMeta {
                dry_run: Some(dry_run),
                ..request_meta_with_workspace()
            },
            target: crate::MaterializeTarget {
                kind: crate::MaterializeTargetKind::Lock,
                name: None,
                commit: None,
            },
        }
    }

    fn pull_head_request() -> crate::PullHeadRequest {
        crate::PullHeadRequest {
            meta: request_meta_with_workspace(),
        }
    }

    fn push_request(
        unsupported_member: Option<crate::UnsupportedMemberBehavior>,
        remote: Option<&str>,
    ) -> crate::PushRequest {
        crate::PushRequest {
            meta: crate::RequestMeta {
                policy: Some(crate::OperationPolicy {
                    unsupported_member,
                    remote: remote.map(ToOwned::to_owned),
                    ..Default::default()
                }),
                ..request_meta_with_workspace()
            },
            remote: None,
            refspec: None,
        }
    }

    fn push_request_explicit(remote: Option<&str>, refspec: Option<&str>) -> crate::PushRequest {
        crate::PushRequest {
            meta: request_meta_with_workspace(),
            remote: remote.map(ToOwned::to_owned),
            refspec: refspec.map(ToOwned::to_owned),
        }
    }

    fn read_repo_ref(repo_path: &Path, ref_name: &str) -> Option<String> {
        let repo = git2::Repository::open(repo_path).unwrap();
        repo.find_reference(ref_name)
            .ok()
            .and_then(|reference| reference.target())
            .map(|target| target.to_string())
    }

    fn write_pull_fixture(root: &Path, members: Vec<(&str, &str, &str, &str)>) {
        crate::artifact::write_manifest(
            root,
            &crate::artifact::ManifestArtifact {
                schema: crate::artifact::WORKSPACE_SCHEMA.to_owned(),
                workspace: crate::artifact::WorkspaceHeader {
                    id: "ws_ops".to_owned(),
                },
                members: members
                    .iter()
                    .map(
                        |(member_id, path, remote_url, _)| crate::artifact::ManifestMember {
                            id: (*member_id).to_owned(),
                            path: (*path).to_owned(),
                            source_kind: crate::artifact::ArtifactSourceKind::Git,
                            source_id: format!("src_{}", member_id.trim_start_matches("mem_")),
                            active: true,
                            desired: Some(crate::artifact::DesiredRefArtifact {
                                branch: Some("main".to_owned()),
                                ..Default::default()
                            }),
                            remotes: vec![crate::artifact::RemoteArtifact {
                                name: "origin".to_owned(),
                                url: (*remote_url).to_owned(),
                                fetch: true,
                                push: true,
                            }],
                        },
                    )
                    .collect(),
            },
        )
        .unwrap();
        crate::artifact::write_lock(
            root,
            &crate::artifact::LockArtifact {
                schema: crate::artifact::LOCK_SCHEMA.to_owned(),
                workspace_id: "ws_ops".to_owned(),
                manifest_schema: crate::artifact::WORKSPACE_SCHEMA.to_owned(),
                created_at: "2026-06-15T00:00:00Z".to_owned(),
                members: members
                    .into_iter()
                    .map(|(member_id, path, _, commit)| {
                        (
                            member_id.to_owned(),
                            test_member_state(path, Some(commit.to_owned()), false),
                        )
                    })
                    .collect(),
            },
        )
        .unwrap();
    }

    fn materialize_named_request(
        kind: crate::MaterializeTargetKind,
        name: &str,
    ) -> crate::MaterializeRequest {
        crate::MaterializeRequest {
            meta: request_meta_with_workspace(),
            target: crate::MaterializeTarget {
                kind,
                name: Some(name.to_owned()),
                commit: None,
            },
        }
    }

    struct SnapshotFixture {
        remote: String,
        first: String,
        second: String,
    }

    impl SnapshotFixture {
        fn remote_url(&self) -> &str {
            &self.remote
        }
    }

    fn materialize_snapshot_fixture(root: &Path, backend: &Git2Backend) -> SnapshotFixture {
        handle_create_workspace(create_workspace_request(root), "op_create").unwrap();
        let fixture = RemoteFixture::new("snapshot-source");
        let first = fixture.commit_and_push("README.md", "one", "initial", backend);
        let second = fixture.commit_and_push("README.md", "two", "second", backend);
        write_materialize_fixture(root, fixture.remote_url(), &second);
        backend
            .clone_repo(fixture.remote_url(), &root.join("repos/app"))
            .unwrap();
        let snapshot_members = std::collections::BTreeMap::from([(
            "mem_app".to_owned(),
            test_member_state("repos/app", Some(first.clone()), false),
        )]);
        crate::artifact::write_snapshot(
            root,
            &crate::artifact::SnapshotArtifact {
                schema: crate::artifact::SNAPSHOT_SCHEMA.to_owned(),
                workspace_id: "ws_ops".to_owned(),
                snapshot_id: "snap_first".to_owned(),
                created_at: "2026-06-15T00:00:00Z".to_owned(),
                created_by: crate::artifact::CreatedByArtifact {
                    actor_id: "agent://tester".to_owned(),
                },
                selected_members: vec!["mem_app".to_owned()],
                members: snapshot_members.clone(),
            },
        )
        .unwrap();
        crate::artifact::write_tag(
            root,
            &crate::artifact::TagArtifact {
                schema: crate::artifact::TAG_SCHEMA.to_owned(),
                workspace_id: "ws_ops".to_owned(),
                tag: "tag_first".to_owned(),
                created_at: "2026-06-15T00:00:00Z".to_owned(),
                created_by: crate::artifact::CreatedByArtifact {
                    actor_id: "agent://tester".to_owned(),
                },
                selected_members: vec!["mem_app".to_owned()],
                members: snapshot_members,
            },
        )
        .unwrap();
        SnapshotFixture {
            remote: fixture.remote_url().to_owned(),
            first,
            second,
        }
    }

    fn write_materialize_fixture(root: &Path, remote_url: &str, commit: &str) {
        crate::artifact::write_manifest(
            root,
            &crate::artifact::ManifestArtifact {
                schema: crate::artifact::WORKSPACE_SCHEMA.to_owned(),
                workspace: crate::artifact::WorkspaceHeader {
                    id: "ws_ops".to_owned(),
                },
                members: vec![crate::artifact::ManifestMember {
                    id: "mem_app".to_owned(),
                    path: "repos/app".to_owned(),
                    source_kind: crate::artifact::ArtifactSourceKind::Git,
                    source_id: "src_app".to_owned(),
                    active: true,
                    desired: Some(crate::artifact::DesiredRefArtifact {
                        branch: Some("main".to_owned()),
                        ..Default::default()
                    }),
                    remotes: vec![crate::artifact::RemoteArtifact {
                        name: "origin".to_owned(),
                        url: remote_url.to_owned(),
                        fetch: true,
                        push: true,
                    }],
                }],
            },
        )
        .unwrap();
        crate::artifact::write_lock(
            root,
            &test_lock("mem_app", "repos/app", Some(commit.to_owned()), false),
        )
        .unwrap();
    }

    fn test_lock(
        member_id: &str,
        path: &str,
        commit: Option<String>,
        dirty: bool,
    ) -> crate::artifact::LockArtifact {
        crate::artifact::LockArtifact {
            schema: crate::artifact::LOCK_SCHEMA.to_owned(),
            workspace_id: "ws_ops".to_owned(),
            manifest_schema: crate::artifact::WORKSPACE_SCHEMA.to_owned(),
            created_at: "2026-06-15T00:00:00Z".to_owned(),
            members: std::collections::BTreeMap::from([(
                member_id.to_owned(),
                test_member_state(path, commit, dirty),
            )]),
        }
    }

    fn test_member_state(
        path: &str,
        commit: Option<String>,
        dirty: bool,
    ) -> crate::artifact::ResolvedMemberArtifact {
        crate::artifact::ResolvedMemberArtifact {
            path: path.to_owned(),
            source_id: Some("src_app".to_owned()),
            source_kind: crate::artifact::ArtifactSourceKind::Git,
            commit,
            branch: Some("main".to_owned()),
            detached: Some(false),
            upstream: None,
            dirty: Some(dirty),
            materialized: Some(true),
        }
    }

    fn init_bare_main(path: &Path) {
        let repo = git2::Repository::init_bare(path).unwrap();
        repo.set_head("refs/heads/main").unwrap();
    }

    struct RemoteFixture {
        _temp: TempDir,
        source: PathBuf,
        remote: PathBuf,
    }

    impl RemoteFixture {
        fn new(prefix: &str) -> Self {
            let temp = TempDir::new(prefix);
            let source = temp.path().join("source");
            let remote = temp.path().join("remote.git");
            Git2Backend::new().create_repo(&source).unwrap();
            init_bare_main(&remote);
            Git2Backend::new()
                .add_remote(&source, "origin", remote.to_str().unwrap())
                .unwrap();
            Self {
                _temp: temp,
                source,
                remote,
            }
        }

        fn remote_url(&self) -> &str {
            self.remote.to_str().unwrap()
        }

        fn commit_and_push(
            &self,
            relative_path: &str,
            content: &str,
            message: &str,
            backend: &Git2Backend,
        ) -> String {
            let parent = backend
                .head(&self.source)
                .unwrap()
                .commit
                .and_then(|commit| git2::Oid::from_str(&commit).ok());
            let parents = parent.into_iter().collect::<Vec<_>>();
            let commit =
                commit_file(&self.source, relative_path, content, message, &parents).unwrap();
            backend
                .push(&self.source, "origin", "refs/heads/main:refs/heads/main")
                .unwrap();
            commit
        }
    }

    trait Single<T> {
        fn single(&self) -> &T;
    }

    impl<T> Single<T> for Vec<T> {
        fn single(&self) -> &T {
            assert_eq!(self.len(), 1);
            &self[0]
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "gwz-core-ops-{prefix}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
