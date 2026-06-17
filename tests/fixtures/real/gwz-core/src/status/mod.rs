use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::artifact::{self, ArtifactSourceKind, LockArtifact, ManifestArtifact, ManifestMember};
use crate::git::{GitBackend, GitHeadState, GitStatus as BackendGitStatus};
use crate::model::{ErrorCode, MemberId, ModelError, ModelResult};
use crate::operation::{ActionKind, OperationRequest};
use crate::workspace::{MemberPath, discover_workspace_root};

pub fn handle_status<B>(
    backend: &B,
    start: &Path,
    request: crate::StatusRequest,
    operation_id: impl Into<String>,
) -> ModelResult<crate::StatusResponse>
where
    B: GitBackend,
{
    let context = OperationRequest::Status(request.clone()).context(operation_id.into())?;
    let workspace_root = resolve_workspace_root(start, request.meta.workspace.as_ref())?;
    let manifest = artifact::read_manifest(&workspace_root)?;
    if let Some(expected) = request
        .meta
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.workspace_id.as_ref())
        && expected != &manifest.workspace.id
    {
        return Err(ModelError::new(
            ErrorCode::WorkspaceNotFound,
            "workspace id does not match manifest",
        ));
    }

    let lock = read_lock_optional(&workspace_root)?;
    let selected = resolve_selection(&manifest, request.meta.selection.as_ref())?;
    let mut reports = Vec::with_capacity(selected.len());
    for member in selected {
        reports.push(status_member(
            backend,
            &workspace_root,
            member,
            lock.as_ref(),
        ));
    }
    let members = reports
        .iter()
        .map(|report| report.response.clone())
        .collect::<Vec<_>>();
    let root_report = root_status(backend, &workspace_root)?;
    let workspace_git_status = matches!(
        request.mode,
        Some(crate::StatusMode::Combined | crate::StatusMode::Summary)
    )
    .then(|| {
        workspace_git_status(
            root_report.as_ref(),
            &reports,
            request.include_file_changes.unwrap_or(true),
            request.include_branch_summary.unwrap_or(true),
            request
                .path_style
                .unwrap_or(crate::StatusPathStyle::WorkspaceRelative),
        )
    });

    Ok(crate::StatusResponse {
        response: crate::ResponseEnvelope {
            meta: crate::ResponseMeta {
                request_id: context.request_id,
                schema_version: context.schema_version,
                action: ActionKind::Status.into(),
                aggregate_status: aggregate_status(&members),
                operation_id: Some(context.operation_id),
                message: None,
                attribution: context.attribution.as_ref().map(Into::into),
            },
            members,
            errors: Vec::new(),
        },
        workspace_git_status,
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

fn read_lock_optional(root: &Path) -> ModelResult<Option<LockArtifact>> {
    let path = root.join(artifact::LOCK_PATH);
    if path.exists() {
        artifact::read_lock(root).map(Some)
    } else {
        Ok(None)
    }
}

fn resolve_selection<'a>(
    manifest: &'a ManifestArtifact,
    selection: Option<&crate::Selection>,
) -> ModelResult<Vec<&'a ManifestMember>> {
    match selection {
        None => Ok(manifest
            .members
            .iter()
            .filter(|member| member.active)
            .collect()),
        Some(selection) => resolve_explicit_selection(manifest, selection),
    }
}

fn resolve_explicit_selection<'a>(
    manifest: &'a ManifestArtifact,
    selection: &crate::Selection,
) -> ModelResult<Vec<&'a ManifestMember>> {
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
            .collect());
    }
    if !has_filters {
        return Err(invalid(
            "selection must include all=true, member_ids, or paths",
        ));
    }

    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for member_id in &selection.member_ids {
        MemberId::parse_str(member_id)?;
        let member = find_member_by_id(manifest, member_id)?;
        push_selected(member, &mut seen, &mut selected)?;
    }
    for path in &selection.paths {
        MemberPath::parse(path)?;
        let member = find_member_by_path(manifest, path)?;
        push_selected(member, &mut seen, &mut selected)?;
    }
    Ok(selected)
}

fn find_member_by_id<'a>(
    manifest: &'a ManifestArtifact,
    member_id: &str,
) -> ModelResult<&'a ManifestMember> {
    let mut matches = manifest
        .members
        .iter()
        .filter(|member| member.id == member_id);
    let member = matches
        .next()
        .ok_or_else(|| ModelError::new(ErrorCode::MemberNotFound, "member id not found"))?;
    if matches.next().is_some() {
        return Err(invalid("member id selection is ambiguous"));
    }
    require_active(member)?;
    Ok(member)
}

fn find_member_by_path<'a>(
    manifest: &'a ManifestArtifact,
    path: &str,
) -> ModelResult<&'a ManifestMember> {
    let mut matches = manifest.members.iter().filter(|member| member.path == path);
    let member = matches
        .next()
        .ok_or_else(|| ModelError::new(ErrorCode::MemberNotFound, "member path not found"))?;
    if matches.next().is_some() {
        return Err(invalid("member path selection is ambiguous"));
    }
    require_active(member)?;
    Ok(member)
}

fn push_selected<'a>(
    member: &'a ManifestMember,
    seen: &mut BTreeSet<&'a str>,
    selected: &mut Vec<&'a ManifestMember>,
) -> ModelResult<()> {
    if !seen.insert(member.id.as_str()) {
        return Err(invalid("selection resolves the same member more than once"));
    }
    selected.push(member);
    Ok(())
}

fn require_active(member: &ManifestMember) -> ModelResult<()> {
    if member.active {
        Ok(())
    } else {
        Err(ModelError::new(
            ErrorCode::MemberInactive,
            "selected member is inactive",
        ))
    }
}

#[derive(Clone, Debug)]
struct StatusMemberReport {
    response: crate::MemberResponse,
    head: Option<GitHeadState>,
    status: Option<BackendGitStatus>,
}

#[derive(Clone, Debug)]
struct RootStatusReport {
    head: GitHeadState,
    status: BackendGitStatus,
}

fn root_status<B>(backend: &B, workspace_root: &Path) -> ModelResult<Option<RootStatusReport>>
where
    B: GitBackend,
{
    if !backend.is_repository(workspace_root)? {
        return Ok(None);
    }

    Ok(Some(RootStatusReport {
        head: backend.head(workspace_root)?,
        status: backend.status(workspace_root)?,
    }))
}

fn status_member<B>(
    backend: &B,
    workspace_root: &Path,
    member: &ManifestMember,
    lock: Option<&LockArtifact>,
) -> StatusMemberReport
where
    B: GitBackend,
{
    let source_kind = protocol_source_kind(member.source_kind);
    if member.source_kind != ArtifactSourceKind::Git {
        return StatusMemberReport {
            response: member_error(
                member,
                source_kind,
                ModelError::new(
                    ErrorCode::UnsupportedSourceKind,
                    "status supports git members only",
                ),
                crate::MemberStatus::Rejected,
            ),
            head: None,
            status: None,
        };
    }

    let member_root = workspace_root.join(&member.path);
    match backend.is_repository(&member_root) {
        // The member is declared in gwz.conf but its working tree was never
        // cloned (e.g. right after a bare `git clone` of the workspace root).
        // That is an expected, recoverable state, not a git failure.
        Ok(false) => {
            return StatusMemberReport {
                response: member_not_materialized(member, source_kind, lock),
                head: None,
                status: None,
            };
        }
        Err(error) => {
            return StatusMemberReport {
                response: member_error(member, source_kind, error, crate::MemberStatus::Failed),
                head: None,
                status: None,
            };
        }
        Ok(true) => {}
    }
    let head = match backend.head(&member_root) {
        Ok(head) => head,
        Err(error) => {
            return StatusMemberReport {
                response: member_error(member, source_kind, error, crate::MemberStatus::Failed),
                head: None,
                status: None,
            };
        }
    };
    let status = match backend.status(&member_root) {
        Ok(status) => status,
        Err(error) => {
            return StatusMemberReport {
                response: member_error(member, source_kind, error, crate::MemberStatus::Failed),
                head: None,
                status: None,
            };
        }
    };

    let response = crate::MemberResponse {
        member_id: member.id.clone(),
        member_path: member.path.clone(),
        source_kind,
        status: crate::MemberStatus::Ok,
        error: None,
        planned: None,
        state: None,
        git_status: Some(protocol_git_status(member, &head, &status)),
        lock_match: Some(lock_match(lock, member, &head, &status)),
    };
    StatusMemberReport {
        response,
        head: Some(head),
        status: Some(status),
    }
}

fn workspace_git_status(
    root: Option<&RootStatusReport>,
    reports: &[StatusMemberReport],
    include_file_changes: bool,
    include_branch_summary: bool,
    path_style: crate::StatusPathStyle,
) -> crate::WorkspaceGitStatus {
    let root_clean = root.is_none_or(|report| !report.status.is_dirty);
    let members_clean = reports.iter().all(|report| {
        report.response.status == crate::MemberStatus::Ok
            && report.status.as_ref().is_none_or(|status| !status.is_dirty)
    });
    let clean = root_clean && members_clean;
    let root_file_changes = if include_file_changes {
        root.map(root_file_changes).unwrap_or_default()
    } else {
        Vec::new()
    };
    let file_changes = if include_file_changes {
        reports
            .iter()
            .flat_map(|report| report_file_changes(report, path_style))
            .collect()
    } else {
        Vec::new()
    };
    let branches = if include_branch_summary {
        reports
            .iter()
            .filter_map(report_branch_status)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let (branch_groups, branch_differences) = if include_branch_summary {
        branch_groups_and_differences(&branches)
    } else {
        (Vec::new(), Vec::new())
    };

    crate::WorkspaceGitStatus {
        clean,
        root_status: root.map(protocol_root_git_status),
        root_file_changes,
        file_changes,
        branches,
        branch_groups,
        branch_differences,
    }
}

fn root_file_changes(report: &RootStatusReport) -> Vec<crate::WorkspaceRootFileChange> {
    report
        .status
        .files
        .iter()
        .map(|file| crate::WorkspaceRootFileChange {
            repo_path: file.path.clone(),
            workspace_path: file.path.clone(),
            index_status: file.index_status.clone(),
            worktree_status: file.worktree_status.clone(),
            original_repo_path: file.original_path.clone(),
        })
        .collect()
}

fn report_file_changes(
    report: &StatusMemberReport,
    path_style: crate::StatusPathStyle,
) -> Vec<crate::GitFileChange> {
    let Some(status) = &report.status else {
        return Vec::new();
    };
    status
        .files
        .iter()
        .map(|file| {
            let workspace_path = match path_style {
                crate::StatusPathStyle::WorkspaceRelative => {
                    workspace_path(&report.response.member_path, &file.path)
                }
                crate::StatusPathStyle::MemberRelative => file.path.clone(),
            };
            crate::GitFileChange {
                member_id: report.response.member_id.clone(),
                member_path: report.response.member_path.clone(),
                repo_path: file.path.clone(),
                workspace_path,
                index_status: file.index_status.clone(),
                worktree_status: file.worktree_status.clone(),
                original_repo_path: file.original_path.clone(),
            }
        })
        .collect()
}

fn report_branch_status(report: &StatusMemberReport) -> Option<crate::GitMemberBranchStatus> {
    let head = report.head.as_ref()?;
    let label = branch_label(head);
    Some(crate::GitMemberBranchStatus {
        member_id: report.response.member_id.clone(),
        member_path: report.response.member_path.clone(),
        label,
        branch: head.branch.clone(),
        detached: head.is_detached,
        unborn: head.commit.is_none() && !head.is_detached,
        head: head.commit.clone(),
        upstream: None,
        ahead: None,
        behind: None,
    })
}

fn workspace_path(member_path: &str, repo_path: &str) -> String {
    if repo_path.is_empty() {
        member_path.to_owned()
    } else {
        format!("{member_path}/{repo_path}")
    }
}

fn branch_label(head: &GitHeadState) -> String {
    if let Some(branch) = &head.branch {
        branch.clone()
    } else if let Some(commit) = &head.commit {
        format!("detached@{}", commit.chars().take(12).collect::<String>())
    } else {
        "unborn".to_owned()
    }
}

fn branch_groups_and_differences(
    branches: &[crate::GitMemberBranchStatus],
) -> (Vec<crate::GitBranchGroup>, Vec<crate::GitBranchDifference>) {
    let mut by_label: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for branch in branches {
        let entry = by_label
            .entry(branch.label.clone())
            .or_insert_with(|| (Vec::new(), Vec::new()));
        entry.0.push(branch.member_id.clone());
        entry.1.push(branch.member_path.clone());
    }

    let groups = by_label
        .iter()
        .map(
            |(label, (member_ids, member_paths))| crate::GitBranchGroup {
                label: label.clone(),
                member_ids: member_ids.clone(),
                member_paths: member_paths.clone(),
            },
        )
        .collect::<Vec<_>>();
    let Some(majority) = groups.iter().max_by_key(|group| {
        (
            group.member_ids.len(),
            std::cmp::Reverse(group.label.clone()),
        )
    }) else {
        return (groups, Vec::new());
    };
    if groups.len() <= 1 {
        return (groups, Vec::new());
    }

    let majority_label = majority.label.clone();
    let differences = groups
        .iter()
        .filter(|group| group.label != majority_label)
        .map(|group| crate::GitBranchDifference {
            label: group.label.clone(),
            majority_label: Some(majority_label.clone()),
            member_ids: group.member_ids.clone(),
            member_paths: group.member_paths.clone(),
            message: Some(format!(
                "{} differs from majority branch {}",
                group.member_paths.join(", "),
                majority_label
            )),
        })
        .collect();

    (groups, differences)
}

fn protocol_git_status(
    member: &ManifestMember,
    head: &GitHeadState,
    status: &BackendGitStatus,
) -> crate::GitStatus {
    crate::GitStatus {
        member_id: member.id.clone(),
        branch: head.branch.clone(),
        detached: head.is_detached,
        head: head.commit.clone(),
        upstream: None,
        ahead: None,
        behind: None,
        staged: status.staged as i64,
        unstaged: status.unstaged as i64,
        untracked: status.untracked as i64,
        dirty: status.is_dirty,
    }
}

fn protocol_root_git_status(report: &RootStatusReport) -> crate::WorkspaceRootGitStatus {
    crate::WorkspaceRootGitStatus {
        branch: report.head.branch.clone(),
        detached: report.head.is_detached,
        head: report.head.commit.clone(),
        staged: report.status.staged as i64,
        unstaged: report.status.unstaged as i64,
        untracked: report.status.untracked as i64,
        dirty: report.status.is_dirty,
        unborn: report.head.commit.is_none() && !report.head.is_detached,
    }
}

fn lock_match(
    lock: Option<&LockArtifact>,
    member: &ManifestMember,
    head: &GitHeadState,
    status: &BackendGitStatus,
) -> crate::LockMatch {
    let Some(lock) = lock else {
        return crate::LockMatch::Missing;
    };
    let Some(locked) = lock.members.get(&member.id) else {
        return crate::LockMatch::Missing;
    };
    if locked.commit == head.commit && locked.dirty.unwrap_or(false) == status.is_dirty {
        crate::LockMatch::Matches
    } else {
        crate::LockMatch::Differs
    }
}

fn member_not_materialized(
    member: &ManifestMember,
    source_kind: crate::SourceKind,
    lock: Option<&LockArtifact>,
) -> crate::MemberResponse {
    let locked = lock.and_then(|lock| lock.members.get(&member.id));
    crate::MemberResponse {
        member_id: member.id.clone(),
        member_path: member.path.clone(),
        source_kind,
        status: crate::MemberStatus::Noop,
        error: None,
        planned: None,
        state: Some(crate::ResolvedMemberState {
            member_id: member.id.clone(),
            path: locked
                .map(|state| state.path.clone())
                .unwrap_or_else(|| member.path.clone()),
            source_id: member.source_id.clone(),
            source_kind,
            commit: locked.and_then(|state| state.commit.clone()),
            branch: locked.and_then(|state| state.branch.clone()),
            detached: locked.and_then(|state| state.detached),
            upstream: locked.and_then(|state| state.upstream.clone()),
            dirty: None,
            materialized: false,
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
        }),
        git_status: None,
        lock_match: Some(crate::LockMatch::Missing),
    }
}

fn member_error(
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

fn aggregate_status(members: &[crate::MemberResponse]) -> crate::AggregateStatus {
    if members
        .iter()
        .any(|member| member.status == crate::MemberStatus::Failed)
    {
        crate::AggregateStatus::Failed
    } else if members
        .iter()
        .any(|member| member.status == crate::MemberStatus::Rejected)
    {
        crate::AggregateStatus::Rejected
    } else {
        crate::AggregateStatus::Ok
    }
}

fn protocol_source_kind(source_kind: ArtifactSourceKind) -> crate::SourceKind {
    match source_kind {
        ArtifactSourceKind::Git => crate::SourceKind::Git,
        ArtifactSourceKind::Archive => crate::SourceKind::Archive,
        ArtifactSourceKind::Package => crate::SourceKind::Package,
        ArtifactSourceKind::Local => crate::SourceKind::Local,
        ArtifactSourceKind::Generated => crate::SourceKind::Generated,
    }
}

fn invalid(message: impl Into<String>) -> ModelError {
    ModelError::new(ErrorCode::InvalidRequest, message)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::artifact::{
        ArtifactSourceKind, LockArtifact, ManifestArtifact, ManifestMember, RemoteArtifact,
        ResolvedMemberArtifact, WorkspaceHeader, write_lock, write_manifest,
    };
    use crate::git::{Git2Backend, GitBackend};
    use crate::model::ErrorCode;

    use super::*;

    #[test]
    fn status_on_empty_workspace_succeeds() {
        let temp = TempDir::new("empty");
        write_manifest(temp.path(), &manifest(vec![])).unwrap();

        let response = handle_status(
            &Git2Backend::new(),
            temp.path(),
            status_request(None),
            "op_status",
        )
        .unwrap();

        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Ok
        );
        assert!(response.response.members.is_empty());
    }

    #[test]
    fn status_on_clean_member_reports_git_status_and_lock_match() {
        let temp = TempDir::new("clean");
        let backend = Git2Backend::new();
        let repo_path = temp.path().join("repos/app");
        backend.create_repo(&repo_path).unwrap();
        let commit = commit_file(&repo_path, "README.md", "clean", "initial", &[]).unwrap();
        write_manifest(
            temp.path(),
            &manifest(vec![member("mem_app", "repos/app", true)]),
        )
        .unwrap();
        write_lock(
            temp.path(),
            &lock("mem_app", "repos/app", Some(commit.clone()), false),
        )
        .unwrap();

        let response =
            handle_status(&backend, &repo_path, status_request(None), "op_status").unwrap();
        let member = response.response.members.single();
        let git_status = member.git_status.as_ref().unwrap();

        assert_eq!(member.member_id, "mem_app");
        assert_eq!(member.status, crate::MemberStatus::Ok);
        assert_eq!(member.lock_match, Some(crate::LockMatch::Matches));
        assert_eq!(git_status.head, Some(commit));
        assert_eq!(git_status.branch, Some("main".to_owned()));
        assert!(!git_status.dirty);
    }

    #[test]
    fn status_on_dirty_member_reports_dirty_counts_and_lock_difference() {
        let temp = TempDir::new("dirty");
        let backend = Git2Backend::new();
        let repo_path = temp.path().join("repos/app");
        backend.create_repo(&repo_path).unwrap();
        let commit = commit_file(&repo_path, "README.md", "clean", "initial", &[]).unwrap();
        fs::write(repo_path.join("README.md"), "dirty").unwrap();
        fs::write(repo_path.join("new.txt"), "new").unwrap();
        write_manifest(
            temp.path(),
            &manifest(vec![member("mem_app", "repos/app", true)]),
        )
        .unwrap();
        write_lock(
            temp.path(),
            &lock("mem_app", "repos/app", Some(commit), false),
        )
        .unwrap();

        let response =
            handle_status(&backend, temp.path(), status_request(None), "op_status").unwrap();
        let git_status = response
            .response
            .members
            .single()
            .git_status
            .clone()
            .unwrap();

        assert!(git_status.dirty);
        assert_eq!(git_status.unstaged, 1);
        assert_eq!(git_status.untracked, 1);
        assert_eq!(
            response.response.members.single().lock_match,
            Some(crate::LockMatch::Differs)
        );
    }

    #[test]
    fn status_on_unmaterialized_member_reports_missing_not_failure() {
        // Right after a bare `git clone` of a workspace root, members are
        // declared in gwz.conf but their working trees were never cloned. That
        // is an expected, recoverable state, not a git failure.
        let temp = TempDir::new("unmaterialized");
        let backend = Git2Backend::new();
        let commit = "0".repeat(40);
        write_manifest(
            temp.path(),
            &manifest(vec![member("mem_app", "repos/app", true)]),
        )
        .unwrap();
        write_lock(
            temp.path(),
            &lock("mem_app", "repos/app", Some(commit.clone()), false),
        )
        .unwrap();

        let mut request = status_request(None);
        request.mode = Some(crate::StatusMode::Combined);
        let response = handle_status(&backend, temp.path(), request, "op_status").unwrap();
        let member = response.response.members.single();

        assert_eq!(member.status, crate::MemberStatus::Noop);
        assert!(member.error.is_none());
        assert!(member.git_status.is_none());
        assert_eq!(member.lock_match, Some(crate::LockMatch::Missing));
        let state = member.state.as_ref().expect("member state present");
        assert!(!state.materialized);
        assert_eq!(state.commit.as_deref(), Some(commit.as_str()));
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert_eq!(
            response.response.meta.aggregate_status,
            crate::AggregateStatus::Ok
        );
        // The absent member must not appear as a phantom branch group.
        let workspace_status = response.workspace_git_status.as_ref().unwrap();
        assert!(workspace_status.branch_groups.is_empty());
    }

    #[test]
    fn combined_status_reports_workspace_file_changes_and_branches() {
        let temp = TempDir::new("combined");
        let backend = Git2Backend::new();
        backend.create_repo(temp.path()).unwrap();
        let repo_path = temp.path().join("repos/app");
        backend.create_repo(&repo_path).unwrap();
        let commit = commit_file(&repo_path, "README.md", "clean", "initial", &[]).unwrap();
        fs::write(repo_path.join("README.md"), "dirty").unwrap();
        fs::write(repo_path.join("new.txt"), "new").unwrap();
        write_manifest(
            temp.path(),
            &manifest(vec![member("mem_app", "repos/app", true)]),
        )
        .unwrap();
        write_lock(
            temp.path(),
            &lock("mem_app", "repos/app", Some(commit), false),
        )
        .unwrap();
        let mut request = status_request(None);
        request.mode = Some(crate::StatusMode::Combined);
        request.include_file_changes = Some(true);
        request.include_branch_summary = Some(true);
        request.path_style = Some(crate::StatusPathStyle::WorkspaceRelative);

        let response = handle_status(&backend, temp.path(), request, "op_status").unwrap();
        let workspace_status = response.workspace_git_status.as_ref().unwrap();

        assert!(!workspace_status.clean);
        let root_status = workspace_status.root_status.as_ref().unwrap();
        assert_eq!(root_status.branch, Some("main".to_owned()));
        assert!(root_status.dirty);
        assert!(!workspace_status.root_file_changes.is_empty());
        assert!(workspace_status.root_file_changes.iter().any(|change| {
            change.repo_path == "gwz.conf/gwz.yml"
                && change.workspace_path == "gwz.conf/gwz.yml"
                && change.worktree_status == "?"
        }));
        assert_eq!(workspace_status.file_changes.len(), 2);
        assert!(workspace_status.file_changes.iter().any(|change| {
            change.member_id == "mem_app"
                && change.repo_path == "README.md"
                && change.workspace_path == "repos/app/README.md"
                && change.worktree_status == "M"
        }));
        assert!(workspace_status.file_changes.iter().any(|change| {
            change.member_id == "mem_app"
                && change.repo_path == "new.txt"
                && change.workspace_path == "repos/app/new.txt"
                && change.worktree_status == "?"
        }));
        assert_eq!(workspace_status.branches.len(), 1);
        assert_eq!(workspace_status.branches[0].label, "main");
        assert_eq!(workspace_status.branch_groups.len(), 1);
        assert!(workspace_status.branch_differences.is_empty());
    }

    #[test]
    fn unknown_inactive_and_ambiguous_selection_fail_before_member_work() {
        let temp = TempDir::new("selection");
        write_manifest(
            temp.path(),
            &manifest(vec![
                member("mem_active", "repos/active", true),
                member("mem_inactive", "repos/inactive", false),
            ]),
        )
        .unwrap();
        let backend = Git2Backend::new();

        assert_eq!(
            handle_status(
                &backend,
                temp.path(),
                status_request(Some(selection(false, &["mem_missing"], &[]))),
                "op_status",
            )
            .unwrap_err()
            .code,
            ErrorCode::MemberNotFound
        );
        assert_eq!(
            handle_status(
                &backend,
                temp.path(),
                status_request(Some(selection(false, &["mem_inactive"], &[]))),
                "op_status",
            )
            .unwrap_err()
            .code,
            ErrorCode::MemberInactive
        );
        assert_eq!(
            handle_status(
                &backend,
                temp.path(),
                status_request(Some(selection(false, &["mem_active"], &["repos/active"]))),
                "op_status",
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
        );
    }

    fn status_request(selection: Option<crate::Selection>) -> crate::StatusRequest {
        crate::StatusRequest {
            meta: crate::RequestMeta {
                request_id: "req_status".to_owned(),
                schema_version: "gwz.protocol/v0".to_owned(),
                selection,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn selection(all: bool, member_ids: &[&str], paths: &[&str]) -> crate::Selection {
        crate::Selection {
            all: Some(all),
            member_ids: member_ids.iter().map(|value| (*value).to_owned()).collect(),
            paths: paths.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    fn manifest(members: Vec<ManifestMember>) -> ManifestArtifact {
        ManifestArtifact {
            schema: crate::artifact::WORKSPACE_SCHEMA.to_owned(),
            workspace: WorkspaceHeader {
                id: "ws_status".to_owned(),
            },
            members,
        }
    }

    fn member(id: &str, path: &str, active: bool) -> ManifestMember {
        ManifestMember {
            id: id.to_owned(),
            path: path.to_owned(),
            source_kind: ArtifactSourceKind::Git,
            source_id: "src_status".to_owned(),
            active,
            desired: None,
            remotes: vec![RemoteArtifact {
                name: "origin".to_owned(),
                url: "file:///tmp/origin.git".to_owned(),
                fetch: true,
                push: true,
            }],
        }
    }

    fn lock(member_id: &str, path: &str, commit: Option<String>, dirty: bool) -> LockArtifact {
        LockArtifact {
            schema: crate::artifact::LOCK_SCHEMA.to_owned(),
            workspace_id: "ws_status".to_owned(),
            manifest_schema: crate::artifact::WORKSPACE_SCHEMA.to_owned(),
            created_at: "2026-06-15T00:00:00Z".to_owned(),
            members: BTreeMap::from([(
                member_id.to_owned(),
                ResolvedMemberArtifact {
                    path: path.to_owned(),
                    source_id: Some("src_status".to_owned()),
                    source_kind: ArtifactSourceKind::Git,
                    commit,
                    branch: Some("main".to_owned()),
                    detached: Some(false),
                    upstream: None,
                    dirty: Some(dirty),
                    materialized: Some(true),
                },
            )]),
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
                "gwz-core-status-{prefix}-{}-{unique}",
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
