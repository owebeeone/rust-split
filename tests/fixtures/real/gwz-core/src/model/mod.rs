use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use crate::runtime::clock::TimestampMs;

pub type ModelResult<T> = Result<T, ModelError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    Ok,
    InvalidRequest,
    WorkspaceNotFound,
    WorkspaceAlreadyExists,
    NestedWorkspace,
    ManifestNotFound,
    ManifestInvalid,
    SchemaUnsupported,
    MemberNotFound,
    MemberInactive,
    PathEscape,
    PathCollision,
    PathReserved,
    UnsupportedSourceKind,
    UnsupportedOperation,
    DirtyMember,
    DivergedMember,
    MissingRemote,
    SnapshotNotFound,
    LockNotFound,
    TagNotFound,
    TagInvalid,
    RemoteRejected,
    GitCommandFailed,
    ExternalToolMissing,
    OperationNotFound,
    AttributionDenied,
    PermissionDenied,
    IoError,
    InternalError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelError {
    pub code: ErrorCode,
    pub message: String,
}

impl ModelError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ModelError {}

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            pub fn parse_str(value: &str) -> ModelResult<Self> {
                parse_id(Self::PREFIX, value).map(Self)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = ModelError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse_str(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(WorkspaceId, "ws_");
id_type!(SourceId, "src_");
id_type!(MemberId, "mem_");
id_type!(OperationId, "op_");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Git,
    Archive,
    Package,
    Local,
    Generated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSpec {
    pub id: WorkspaceId,
    pub sources: Vec<SourceSpec>,
    pub members: Vec<MemberSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSpec {
    pub id: SourceId,
    pub kind: SourceKind,
    pub remotes: Vec<RemoteSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberSpec {
    pub id: MemberId,
    pub path: String,
    pub source_id: SourceId,
    pub source_kind: SourceKind,
    pub active: bool,
    pub desired: Option<DesiredRef>,
    pub remotes: Vec<RemoteSpec>,
}

impl MemberSpec {
    pub fn new(
        id: MemberId,
        path: impl Into<String>,
        source_id: SourceId,
        source_kind: SourceKind,
        active: bool,
        desired: Option<DesiredRef>,
        remotes: Vec<RemoteSpec>,
    ) -> ModelResult<Self> {
        reject_duplicate_remote_names(&remotes)?;
        for remote in &remotes {
            remote.validate()?;
        }
        if let Some(desired) = &desired {
            desired.validate()?;
        }
        Ok(Self {
            id,
            path: path.into(),
            source_id,
            source_kind,
            active,
            desired,
            remotes,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteSpec {
    pub name: String,
    pub url: String,
    pub fetch: bool,
    pub push: bool,
}

impl RemoteSpec {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            fetch: true,
            push: true,
        }
    }

    pub fn validate(&self) -> ModelResult<()> {
        require_non_empty("remote.name", &self.name)?;
        require_non_empty("remote.url", &self.url)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DesiredRef {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub git_tag: Option<String>,
    pub local_only: Option<bool>,
}

impl DesiredRef {
    pub fn branch(branch: impl Into<String>) -> Self {
        Self {
            branch: Some(branch.into()),
            ..Self::default()
        }
    }

    pub fn git_tag(git_tag: impl Into<String>) -> Self {
        Self {
            git_tag: Some(git_tag.into()),
            ..Self::default()
        }
    }

    pub fn local_only() -> Self {
        Self {
            local_only: Some(true),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> ModelResult<()> {
        if self.local_only == Some(false) {
            return Err(ModelError::new(
                ErrorCode::InvalidRequest,
                "desired.local_only must be true when present",
            ));
        }

        let mut targets = 0;
        targets += validate_optional_target("desired.branch", &self.branch)?;
        targets += validate_optional_target("desired.commit", &self.commit)?;
        targets += validate_optional_target("desired.git_tag", &self.git_tag)?;
        if self.local_only == Some(true) {
            targets += 1;
        }

        if targets == 1 {
            Ok(())
        } else {
            Err(ModelError::new(
                ErrorCode::InvalidRequest,
                "desired ref must specify exactly one target",
            ))
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OperationActor {
    pub actor_id: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub authority: Option<String>,
}

impl OperationActor {
    pub fn new(actor_id: impl Into<String>) -> Self {
        Self {
            actor_id: actor_id.into(),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> ModelResult<()> {
        require_non_empty("actor.actor_id", &self.actor_id)?;
        validate_optional_text("actor.display_name", &self.display_name)?;
        validate_optional_text("actor.email", &self.email)?;
        validate_optional_text("actor.authority", &self.authority)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitObjectIdentity {
    pub name: String,
    pub email: String,
    pub time_ms: Option<TimestampMs>,
    pub timezone_offset_minutes: Option<i64>,
}

impl GitObjectIdentity {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> ModelResult<()> {
        require_non_empty("git_identity.name", &self.name)?;
        require_non_empty("git_identity.email", &self.email)?;
        if let Some(offset) = self.timezone_offset_minutes
            && !(-1_440..=1_440).contains(&offset)
        {
            return Err(ModelError::new(
                ErrorCode::InvalidRequest,
                "git identity timezone offset is out of range",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OperationAttribution {
    pub actor: Option<OperationActor>,
    pub git_author: Option<GitObjectIdentity>,
    pub git_committer: Option<GitObjectIdentity>,
    pub credential_ref: Option<String>,
}

impl OperationAttribution {
    pub fn validate(&self) -> ModelResult<()> {
        if let Some(actor) = &self.actor {
            actor.validate()?;
        }
        if let Some(author) = &self.git_author {
            author.validate()?;
        }
        if let Some(committer) = &self.git_committer {
            committer.validate()?;
        }
        validate_optional_text("credential_ref", &self.credential_ref)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    pub all: bool,
    pub member_ids: Vec<MemberId>,
    pub paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartialBehavior {
    Atomic,
    Partial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestructiveBehavior {
    Refuse,
    Allow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncBehavior {
    FetchOnly,
    FfOnly,
    Merge,
    Rebase,
    Reset,
    DriverSelected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedMemberBehavior {
    Fail,
    Skip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationPolicy {
    pub partial: PartialBehavior,
    pub destructive: DestructiveBehavior,
    pub sync: SyncBehavior,
    pub unsupported_member: UnsupportedMemberBehavior,
    pub remote: Option<String>,
    pub concurrency: Option<usize>,
}

impl OperationPolicy {
    pub fn builtin_default() -> Self {
        Self::default()
    }
}

impl Default for OperationPolicy {
    fn default() -> Self {
        Self {
            partial: PartialBehavior::Atomic,
            destructive: DestructiveBehavior::Refuse,
            sync: SyncBehavior::FfOnly,
            unsupported_member: UnsupportedMemberBehavior::Fail,
            remote: None,
            concurrency: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMemberState {
    pub member_id: MemberId,
    pub path: String,
    pub source_id: SourceId,
    pub source_kind: SourceKind,
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub upstream: Option<String>,
    pub dirty: bool,
    pub materialized: bool,
    pub remotes: Vec<RemoteSpec>,
}

fn parse_id(prefix: &str, value: &str) -> ModelResult<String> {
    let valid = value.starts_with(prefix)
        && value.len() > prefix.len()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    if valid {
        Ok(value.to_owned())
    } else {
        Err(ModelError::new(
            ErrorCode::InvalidRequest,
            format!("id must start with {prefix} and contain only portable characters"),
        ))
    }
}

fn reject_duplicate_remote_names(remotes: &[RemoteSpec]) -> ModelResult<()> {
    let mut names = BTreeSet::new();
    for remote in remotes {
        if !names.insert(remote.name.as_str()) {
            return Err(ModelError::new(
                ErrorCode::InvalidRequest,
                format!("duplicate remote name '{}'", remote.name),
            ));
        }
    }
    Ok(())
}

fn validate_optional_target(field: &str, value: &Option<String>) -> ModelResult<usize> {
    match value {
        Some(value) => {
            require_non_empty(field, value)?;
            Ok(1)
        }
        None => Ok(0),
    }
}

fn validate_optional_text(field: &str, value: &Option<String>) -> ModelResult<()> {
    match value {
        Some(value) => require_non_empty(field, value),
        None => Ok(()),
    }
}

fn require_non_empty(field: &str, value: &str) -> ModelResult<()> {
    if value.trim().is_empty() {
        Err(ModelError::new(
            ErrorCode::InvalidRequest,
            format!("{field} must not be empty"),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse_and_display_with_expected_prefixes() {
        let workspace: WorkspaceId = "ws_01".parse().expect("workspace id");
        let source: SourceId = "src_01".parse().expect("source id");
        let member: MemberId = "mem_01".parse().expect("member id");
        let operation: OperationId = "op_01".parse().expect("operation id");

        assert_eq!(workspace.to_string(), "ws_01");
        assert_eq!(source.to_string(), "src_01");
        assert_eq!(member.to_string(), "mem_01");
        assert_eq!(operation.to_string(), "op_01");
        assert_eq!(
            "bad id".parse::<WorkspaceId>().unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            "src_01".parse::<WorkspaceId>().unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn desired_ref_accepts_exactly_one_target() {
        assert!(DesiredRef::branch("main").validate().is_ok());
        assert!(DesiredRef::git_tag("v1.0.0").validate().is_ok());
        assert!(DesiredRef::local_only().validate().is_ok());

        let invalid = DesiredRef {
            branch: Some("main".to_owned()),
            commit: Some("abc123".to_owned()),
            ..DesiredRef::default()
        };
        assert_eq!(
            invalid.validate().unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            DesiredRef::default().validate().unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn attribution_requires_non_empty_actor_and_git_identity_fields() {
        let attribution = OperationAttribution {
            actor: Some(OperationActor::new("agent://local/session")),
            git_author: Some(GitObjectIdentity::new("Agent", "agent@example.invalid")),
            git_committer: Some(GitObjectIdentity::new("Bot", "bot@example.invalid")),
            credential_ref: Some("cred:test".to_owned()),
        };
        assert!(attribution.validate().is_ok());

        let invalid_actor = OperationAttribution {
            actor: Some(OperationActor::new("")),
            ..OperationAttribution::default()
        };
        assert_eq!(
            invalid_actor.validate().unwrap_err().code,
            ErrorCode::InvalidRequest
        );

        let invalid_git = OperationAttribution {
            git_author: Some(GitObjectIdentity::new("Agent", "")),
            ..OperationAttribution::default()
        };
        assert_eq!(
            invalid_git.validate().unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn member_spec_rejects_duplicate_remote_names() {
        let remotes = vec![
            RemoteSpec::new("origin", "git@example.invalid:one.git"),
            RemoteSpec::new("origin", "git@example.invalid:two.git"),
        ];

        let result = MemberSpec::new(
            MemberId::parse_str("mem_01").unwrap(),
            "repos/core",
            SourceId::parse_str("src_01").unwrap(),
            SourceKind::Git,
            true,
            None,
            remotes,
        );

        assert_eq!(result.unwrap_err().code, ErrorCode::InvalidRequest);
    }
}
