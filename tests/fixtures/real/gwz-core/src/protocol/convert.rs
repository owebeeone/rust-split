use crate::model;

use super::generated;

impl From<model::ErrorCode> for generated::GwzErrorCode {
    fn from(value: model::ErrorCode) -> Self {
        match value {
            model::ErrorCode::Ok => Self::Ok,
            model::ErrorCode::InvalidRequest => Self::InvalidRequest,
            model::ErrorCode::WorkspaceNotFound => Self::WorkspaceNotFound,
            model::ErrorCode::WorkspaceAlreadyExists => Self::WorkspaceAlreadyExists,
            model::ErrorCode::NestedWorkspace => Self::NestedWorkspace,
            model::ErrorCode::ManifestNotFound => Self::ManifestNotFound,
            model::ErrorCode::ManifestInvalid => Self::ManifestInvalid,
            model::ErrorCode::SchemaUnsupported => Self::SchemaUnsupported,
            model::ErrorCode::MemberNotFound => Self::MemberNotFound,
            model::ErrorCode::MemberInactive => Self::MemberInactive,
            model::ErrorCode::PathEscape => Self::PathEscape,
            model::ErrorCode::PathCollision => Self::PathCollision,
            model::ErrorCode::PathReserved => Self::PathReserved,
            model::ErrorCode::UnsupportedSourceKind => Self::UnsupportedSourceKind,
            model::ErrorCode::UnsupportedOperation => Self::UnsupportedOperation,
            model::ErrorCode::DirtyMember => Self::DirtyMember,
            model::ErrorCode::DivergedMember => Self::DivergedMember,
            model::ErrorCode::MissingRemote => Self::MissingRemote,
            model::ErrorCode::SnapshotNotFound => Self::SnapshotNotFound,
            model::ErrorCode::LockNotFound => Self::LockNotFound,
            model::ErrorCode::TagNotFound => Self::TagNotFound,
            model::ErrorCode::TagInvalid => Self::TagInvalid,
            model::ErrorCode::RemoteRejected => Self::RemoteRejected,
            model::ErrorCode::GitCommandFailed => Self::GitCommandFailed,
            model::ErrorCode::ExternalToolMissing => Self::ExternalToolMissing,
            model::ErrorCode::OperationNotFound => Self::OperationNotFound,
            model::ErrorCode::AttributionDenied => Self::AttributionDenied,
            model::ErrorCode::PermissionDenied => Self::PermissionDenied,
            model::ErrorCode::IoError => Self::IoError,
            model::ErrorCode::InternalError => Self::InternalError,
        }
    }
}

impl From<model::SourceKind> for generated::SourceKind {
    fn from(value: model::SourceKind) -> Self {
        match value {
            model::SourceKind::Git => Self::Git,
            model::SourceKind::Archive => Self::Archive,
            model::SourceKind::Package => Self::Package,
            model::SourceKind::Local => Self::Local,
            model::SourceKind::Generated => Self::Generated,
        }
    }
}

impl From<model::SyncBehavior> for generated::SyncBehavior {
    fn from(value: model::SyncBehavior) -> Self {
        match value {
            model::SyncBehavior::FetchOnly => Self::FetchOnly,
            model::SyncBehavior::FfOnly => Self::FfOnly,
            model::SyncBehavior::Merge => Self::Merge,
            model::SyncBehavior::Rebase => Self::Rebase,
            model::SyncBehavior::Reset => Self::Reset,
            model::SyncBehavior::DriverSelected => Self::DriverSelected,
        }
    }
}

impl From<model::PartialBehavior> for generated::PartialBehavior {
    fn from(value: model::PartialBehavior) -> Self {
        match value {
            model::PartialBehavior::Atomic => Self::Atomic,
            model::PartialBehavior::Partial => Self::Partial,
        }
    }
}

impl From<model::DestructiveBehavior> for generated::DestructiveBehavior {
    fn from(value: model::DestructiveBehavior) -> Self {
        match value {
            model::DestructiveBehavior::Refuse => Self::Refuse,
            model::DestructiveBehavior::Allow => Self::Allow,
        }
    }
}

impl From<model::UnsupportedMemberBehavior> for generated::UnsupportedMemberBehavior {
    fn from(value: model::UnsupportedMemberBehavior) -> Self {
        match value {
            model::UnsupportedMemberBehavior::Fail => Self::Fail,
            model::UnsupportedMemberBehavior::Skip => Self::Skip,
        }
    }
}

impl From<&model::OperationActor> for generated::OperationActor {
    fn from(value: &model::OperationActor) -> Self {
        Self {
            actor_id: value.actor_id.clone(),
            display_name: value.display_name.clone(),
            email: value.email.clone(),
            authority: value.authority.clone(),
        }
    }
}

impl From<&model::GitObjectIdentity> for generated::GitObjectIdentity {
    fn from(value: &model::GitObjectIdentity) -> Self {
        Self {
            name: value.name.clone(),
            email: value.email.clone(),
            time_ms: value.time_ms.map(|time| time.0),
            timezone_offset_minutes: value.timezone_offset_minutes,
        }
    }
}

impl From<&model::OperationAttribution> for generated::OperationAttribution {
    fn from(value: &model::OperationAttribution) -> Self {
        Self {
            actor: value.actor.as_ref().map(Into::into),
            git_author: value.git_author.as_ref().map(Into::into),
            git_committer: value.git_committer.as_ref().map(Into::into),
            credential_ref: value.credential_ref.clone(),
        }
    }
}

impl From<&model::Selection> for generated::Selection {
    fn from(value: &model::Selection) -> Self {
        Self {
            all: Some(value.all),
            member_ids: value.member_ids.iter().map(ToString::to_string).collect(),
            paths: value.paths.clone(),
        }
    }
}

impl From<&model::OperationPolicy> for generated::OperationPolicy {
    fn from(value: &model::OperationPolicy) -> Self {
        Self {
            partial: Some(value.partial.into()),
            destructive: Some(value.destructive.into()),
            sync: Some(value.sync.into()),
            unsupported_member: Some(value.unsupported_member.into()),
            remote: value.remote.clone(),
            concurrency: value.concurrency.map(|value| value as i64),
            // Progress coalescing and per-host limits are read directly from the
            // wire policy by handlers; the internal model does not carry them.
            progress_min_interval_ms: None,
            max_connections_per_host: None,
        }
    }
}

impl From<&model::ModelError> for generated::GwzError {
    fn from(value: &model::ModelError) -> Self {
        Self {
            code: value.code.into(),
            message: value.message.clone(),
            member_id: None,
            member_path: None,
            detail: None,
        }
    }
}

impl From<&model::RemoteSpec> for generated::RemoteSpec {
    fn from(value: &model::RemoteSpec) -> Self {
        Self {
            name: value.name.clone(),
            url: value.url.clone(),
            fetch: Some(value.fetch),
            push: Some(value.push),
        }
    }
}

impl From<&model::DesiredRef> for generated::DesiredRef {
    fn from(value: &model::DesiredRef) -> Self {
        Self {
            branch: value.branch.clone(),
            commit: value.commit.clone(),
            git_tag: value.git_tag.clone(),
            local_only: value.local_only,
        }
    }
}

impl From<&model::MemberSpec> for generated::MemberSpec {
    fn from(value: &model::MemberSpec) -> Self {
        Self {
            member_id: value.id.to_string(),
            path: value.path.clone(),
            source_id: value.source_id.to_string(),
            source_kind: value.source_kind.into(),
            active: value.active,
            desired: value.desired.as_ref().map(Into::into),
            remotes: value.remotes.iter().map(Into::into).collect(),
        }
    }
}

impl From<&model::ResolvedMemberState> for generated::ResolvedMemberState {
    fn from(value: &model::ResolvedMemberState) -> Self {
        Self {
            member_id: value.member_id.to_string(),
            path: value.path.clone(),
            source_id: value.source_id.to_string(),
            source_kind: value.source_kind.into(),
            commit: value.commit.clone(),
            branch: value.branch.clone(),
            detached: Some(value.detached),
            upstream: value.upstream.clone(),
            dirty: Some(value.dirty),
            materialized: value.materialized,
            remotes: value.remotes.iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_error_codes_convert_to_protocol_codes() {
        let code: generated::GwzErrorCode = model::ErrorCode::DivergedMember.into();
        assert_eq!(code, generated::GwzErrorCode::DivergedMember);
        assert_eq!(code.wire(), 16);
    }
}
