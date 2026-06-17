use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{ErrorCode, ModelError, ModelResult};
use crate::workspace::{MemberPath, WORKSPACE_MANIFEST};

pub const WORKSPACE_SCHEMA: &str = "gwz.workspace/v0";
pub const LOCK_SCHEMA: &str = "gwz.lock/v0";
pub const SNAPSHOT_SCHEMA: &str = "gwz.snapshot/v0";
pub const TAG_SCHEMA: &str = "gwz.tag/v0";
pub const LOCK_PATH: &str = "gwz.conf/gwz.lock.yml";
pub const SNAPSHOT_DIR: &str = "gwz.conf/snapshots";
pub const TAG_DIR: &str = "gwz.conf/tags";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManifestArtifact {
    pub schema: String,
    pub workspace: WorkspaceHeader,
    pub members: Vec<ManifestMember>,
}

impl ManifestArtifact {
    pub fn from_yaml(text: &str) -> ModelResult<Self> {
        let artifact: Self = parse_yaml(text)?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn to_yaml(&self) -> ModelResult<String> {
        self.validate()?;
        emit_yaml(self)
    }

    pub fn validate(&self) -> ModelResult<()> {
        require_schema(&self.schema, WORKSPACE_SCHEMA)?;
        parse_id("workspace.id", "ws_", &self.workspace.id)?;

        let mut paths = Vec::with_capacity(self.members.len());
        for member in &self.members {
            member.validate()?;
            paths.push(MemberPath::parse(&member.path)?);
        }
        crate::workspace::validate_member_path_set(&paths)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceHeader {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManifestMember {
    pub id: String,
    pub path: String,
    #[serde(rename = "type")]
    pub source_kind: ArtifactSourceKind,
    pub source_id: String,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired: Option<DesiredRefArtifact>,
    pub remotes: Vec<RemoteArtifact>,
}

impl ManifestMember {
    fn validate(&self) -> ModelResult<()> {
        parse_id("member.id", "mem_", &self.id)?;
        parse_id("member.source_id", "src_", &self.source_id)?;
        MemberPath::parse(&self.path)?;
        if let Some(desired) = &self.desired {
            desired.validate()?;
        }
        reject_duplicate_remote_names(&self.remotes)?;
        for remote in &self.remotes {
            remote.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DesiredRefArtifact {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_only: Option<bool>,
}

impl DesiredRefArtifact {
    fn validate(&self) -> ModelResult<()> {
        if self.local_only == Some(false) {
            return Err(invalid("desired.local_only must be true when present"));
        }

        let mut targets = 0;
        targets += optional_text_target("desired.branch", &self.branch)?;
        targets += optional_text_target("desired.commit", &self.commit)?;
        targets += optional_text_target("desired.git_tag", &self.git_tag)?;
        if self.local_only == Some(true) {
            targets += 1;
        }

        if targets == 1 {
            Ok(())
        } else {
            Err(invalid("desired ref must specify exactly one target"))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RemoteArtifact {
    pub name: String,
    pub url: String,
    pub fetch: bool,
    pub push: bool,
}

impl RemoteArtifact {
    fn validate(&self) -> ModelResult<()> {
        require_non_empty("remote.name", &self.name)?;
        require_non_empty("remote.url", &self.url)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LockArtifact {
    pub schema: String,
    pub workspace_id: String,
    pub manifest_schema: String,
    pub created_at: String,
    pub members: BTreeMap<String, ResolvedMemberArtifact>,
}

impl LockArtifact {
    pub fn from_yaml(text: &str) -> ModelResult<Self> {
        let artifact: Self = parse_yaml(text)?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn to_yaml(&self) -> ModelResult<String> {
        self.validate()?;
        emit_yaml(self)
    }

    pub fn validate(&self) -> ModelResult<()> {
        require_schema(&self.schema, LOCK_SCHEMA)?;
        require_schema(&self.manifest_schema, WORKSPACE_SCHEMA)?;
        parse_id("workspace_id", "ws_", &self.workspace_id)?;
        require_non_empty("created_at", &self.created_at)?;
        for (member_id, member) in &self.members {
            parse_id("member id", "mem_", member_id)?;
            member.validate(true)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedMemberArtifact {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub source_kind: ArtifactSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialized: Option<bool>,
}

impl ResolvedMemberArtifact {
    fn validate(&self, require_source_id: bool) -> ModelResult<()> {
        MemberPath::parse(&self.path)?;
        if require_source_id {
            let source_id = self
                .source_id
                .as_ref()
                .ok_or_else(|| invalid("resolved member source_id is required"))?;
            parse_id("member.source_id", "src_", source_id)?;
        } else if let Some(source_id) = &self.source_id {
            parse_id("member.source_id", "src_", source_id)?;
        }
        validate_optional_text("commit", &self.commit)?;
        validate_optional_text("branch", &self.branch)?;
        validate_optional_text("upstream", &self.upstream)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotArtifact {
    pub schema: String,
    pub workspace_id: String,
    pub snapshot_id: String,
    pub created_at: String,
    pub created_by: CreatedByArtifact,
    pub selected_members: Vec<String>,
    pub members: BTreeMap<String, ResolvedMemberArtifact>,
}

impl SnapshotArtifact {
    pub fn from_yaml(text: &str) -> ModelResult<Self> {
        let artifact: Self = parse_yaml(text)?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn to_yaml(&self) -> ModelResult<String> {
        self.validate()?;
        emit_yaml(self)
    }

    pub fn validate(&self) -> ModelResult<()> {
        require_schema(&self.schema, SNAPSHOT_SCHEMA)?;
        parse_id("workspace_id", "ws_", &self.workspace_id)?;
        require_slug("snapshot_id", &self.snapshot_id)?;
        validate_member_record(
            &self.created_at,
            &self.created_by,
            &self.selected_members,
            &self.members,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TagArtifact {
    pub schema: String,
    pub workspace_id: String,
    pub tag: String,
    pub created_at: String,
    pub created_by: CreatedByArtifact,
    pub selected_members: Vec<String>,
    pub members: BTreeMap<String, ResolvedMemberArtifact>,
}

impl TagArtifact {
    pub fn from_yaml(text: &str) -> ModelResult<Self> {
        let artifact: Self = parse_yaml(text)?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn to_yaml(&self) -> ModelResult<String> {
        self.validate()?;
        emit_yaml(self)
    }

    pub fn validate(&self) -> ModelResult<()> {
        require_schema(&self.schema, TAG_SCHEMA)?;
        parse_id("workspace_id", "ws_", &self.workspace_id)?;
        require_slug("tag", &self.tag)?;
        validate_member_record(
            &self.created_at,
            &self.created_by,
            &self.selected_members,
            &self.members,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreatedByArtifact {
    pub actor_id: String,
}

impl CreatedByArtifact {
    fn validate(&self) -> ModelResult<()> {
        require_non_empty("created_by.actor_id", &self.actor_id)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSourceKind {
    #[default]
    Git,
    Archive,
    Package,
    Local,
    Generated,
}

pub fn read_manifest(root: &Path) -> ModelResult<ManifestArtifact> {
    ManifestArtifact::from_yaml(&read_to_string(root.join(WORKSPACE_MANIFEST))?)
}

pub fn write_manifest(root: &Path, artifact: &ManifestArtifact) -> ModelResult<()> {
    write_atomic(&root.join(WORKSPACE_MANIFEST), artifact.to_yaml()?)
}

pub fn read_lock(root: &Path) -> ModelResult<LockArtifact> {
    LockArtifact::from_yaml(&read_to_string(root.join(LOCK_PATH))?)
}

pub fn write_lock(root: &Path, artifact: &LockArtifact) -> ModelResult<()> {
    write_atomic(&root.join(LOCK_PATH), artifact.to_yaml()?)
}

pub fn read_snapshot(root: &Path, snapshot_id: &str) -> ModelResult<SnapshotArtifact> {
    SnapshotArtifact::from_yaml(&read_to_string(snapshot_path(root, snapshot_id))?)
}

pub fn write_snapshot(root: &Path, artifact: &SnapshotArtifact) -> ModelResult<()> {
    write_atomic(
        &snapshot_path(root, &artifact.snapshot_id),
        artifact.to_yaml()?,
    )
}

pub fn read_tag(root: &Path, tag: &str) -> ModelResult<TagArtifact> {
    TagArtifact::from_yaml(&read_to_string(tag_path(root, tag))?)
}

pub fn write_tag(root: &Path, artifact: &TagArtifact) -> ModelResult<()> {
    write_atomic(&tag_path(root, &artifact.tag), artifact.to_yaml()?)
}

pub fn write_atomic(path: &Path, contents: impl AsRef<str>) -> ModelResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    let tmp_path = temp_path(path)?;
    fs::write(&tmp_path, contents.as_ref()).map_err(io_error)?;
    fs::rename(&tmp_path, path).map_err(io_error)?;
    Ok(())
}

fn parse_yaml<T>(text: &str) -> ModelResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_yaml::from_str(text).map_err(|err| {
        ModelError::new(
            ErrorCode::ManifestInvalid,
            format!("failed to parse artifact YAML: {err}"),
        )
    })
}

fn emit_yaml<T>(value: &T) -> ModelResult<String>
where
    T: Serialize,
{
    serde_yaml::to_string(value).map_err(|err| {
        ModelError::new(
            ErrorCode::InternalError,
            format!("failed to serialize artifact YAML: {err}"),
        )
    })
}

fn read_to_string(path: PathBuf) -> ModelResult<String> {
    fs::read_to_string(path).map_err(io_error)
}

fn snapshot_path(root: &Path, snapshot_id: &str) -> PathBuf {
    root.join(SNAPSHOT_DIR).join(format!("{snapshot_id}.yaml"))
}

fn tag_path(root: &Path, tag: &str) -> PathBuf {
    root.join(TAG_DIR).join(format!("{tag}.yml"))
}

fn temp_path(path: &Path) -> ModelResult<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("atomic write target must have a file name"))?;
    Ok(path.with_file_name(format!("{file_name}.tmp")))
}

fn require_schema(actual: &str, expected: &str) -> ModelResult<()> {
    let expected_major =
        schema_major(expected).ok_or_else(|| invalid("invalid expected schema"))?;
    match schema_major(actual) {
        Some(actual_major) if actual == expected && actual_major == expected_major => Ok(()),
        Some(_) => Err(ModelError::new(
            ErrorCode::SchemaUnsupported,
            format!("unsupported schema {actual}; expected {expected}"),
        )),
        None => Err(ModelError::new(
            ErrorCode::ManifestInvalid,
            format!("invalid schema {actual}"),
        )),
    }
}

fn schema_major(schema: &str) -> Option<u32> {
    let (_, major) = schema.rsplit_once("/v")?;
    major.parse().ok()
}

fn validate_member_record(
    created_at: &str,
    created_by: &CreatedByArtifact,
    selected_members: &[String],
    members: &BTreeMap<String, ResolvedMemberArtifact>,
) -> ModelResult<()> {
    require_non_empty("created_at", created_at)?;
    created_by.validate()?;
    for member_id in selected_members {
        parse_id("selected member", "mem_", member_id)?;
    }
    for (member_id, member) in members {
        parse_id("member id", "mem_", member_id)?;
        member.validate(false)?;
    }
    Ok(())
}

fn reject_duplicate_remote_names(remotes: &[RemoteArtifact]) -> ModelResult<()> {
    let mut names = BTreeSet::new();
    for remote in remotes {
        if !names.insert(remote.name.as_str()) {
            return Err(invalid(format!("duplicate remote name '{}'", remote.name)));
        }
    }
    Ok(())
}

fn parse_id(field: &str, prefix: &str, value: &str) -> ModelResult<()> {
    let valid = value.starts_with(prefix)
        && value.len() > prefix.len()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "{field} must start with {prefix} and contain only portable characters"
        )))
    }
}

fn require_slug(field: &str, value: &str) -> ModelResult<()> {
    require_non_empty(field, value)?;
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        Ok(())
    } else {
        Err(invalid(format!(
            "{field} must contain only portable slug characters"
        )))
    }
}

fn optional_text_target(field: &str, value: &Option<String>) -> ModelResult<usize> {
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
        Err(invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> ModelError {
    ModelError::new(ErrorCode::InvalidRequest, message)
}

fn io_error(err: io::Error) -> ModelError {
    ModelError::new(ErrorCode::IoError, err.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::model::ErrorCode;

    use super::*;

    const MANIFEST_GOLDEN: &str = "schema: gwz.workspace/v0\nworkspace:\n  id: ws_01\nmembers:\n- id: mem_01\n  path: repos/example\n  type: git\n  source_id: src_01\n  active: true\n  desired:\n    branch: main\n  remotes:\n  - name: origin\n    url: git@example.invalid:example.git\n    fetch: true\n    push: true\n";

    const LOCK_GOLDEN: &str = "schema: gwz.lock/v0\nworkspace_id: ws_01\nmanifest_schema: gwz.workspace/v0\ncreated_at: 2026-06-15T00:00:00Z\nmembers:\n  mem_01:\n    path: repos/example\n    source_id: src_01\n    source_kind: git\n    commit: abc123\n    branch: main\n    detached: false\n    upstream: origin/main\n    dirty: false\n    materialized: true\n";

    const SNAPSHOT_GOLDEN: &str = "schema: gwz.snapshot/v0\nworkspace_id: ws_01\nsnapshot_id: snap_demo\ncreated_at: 2026-06-15T00:00:00Z\ncreated_by:\n  actor_id: agent_01\nselected_members:\n- mem_01\nmembers:\n  mem_01:\n    path: repos/example\n    source_kind: git\n    commit: abc123\n";

    const TAG_GOLDEN: &str = "schema: gwz.tag/v0\nworkspace_id: ws_01\ntag: demo\ncreated_at: 2026-06-15T00:00:00Z\ncreated_by:\n  actor_id: agent_01\nselected_members:\n- mem_01\nmembers:\n  mem_01:\n    path: repos/example\n    source_kind: git\n    commit: abc123\n";

    #[test]
    fn manifest_round_trips_and_matches_golden_yaml() {
        let manifest = sample_manifest();

        assert_eq!(manifest.to_yaml().unwrap(), MANIFEST_GOLDEN);
        assert_eq!(
            ManifestArtifact::from_yaml(MANIFEST_GOLDEN).unwrap(),
            manifest
        );
    }

    #[test]
    fn lock_snapshot_and_tag_round_trip_and_match_golden_yaml() {
        assert_eq!(sample_lock().to_yaml().unwrap(), LOCK_GOLDEN);
        assert_eq!(LockArtifact::from_yaml(LOCK_GOLDEN).unwrap(), sample_lock());

        assert_eq!(sample_snapshot().to_yaml().unwrap(), SNAPSHOT_GOLDEN);
        assert_eq!(
            SnapshotArtifact::from_yaml(SNAPSHOT_GOLDEN).unwrap(),
            sample_snapshot()
        );

        assert_eq!(sample_tag().to_yaml().unwrap(), TAG_GOLDEN);
        assert_eq!(TagArtifact::from_yaml(TAG_GOLDEN).unwrap(), sample_tag());
    }

    #[test]
    fn unsupported_major_schema_versions_fail_with_typed_error() {
        let manifest = MANIFEST_GOLDEN.replace("gwz.workspace/v0", "gwz.workspace/v1");
        let lock = LOCK_GOLDEN.replacen("gwz.lock/v0", "gwz.lock/v1", 1);
        let snapshot = SNAPSHOT_GOLDEN.replace("gwz.snapshot/v0", "gwz.snapshot/v1");
        let tag = TAG_GOLDEN.replace("gwz.tag/v0", "gwz.tag/v1");

        assert_eq!(
            ManifestArtifact::from_yaml(&manifest).unwrap_err().code,
            ErrorCode::SchemaUnsupported
        );
        assert_eq!(
            LockArtifact::from_yaml(&lock).unwrap_err().code,
            ErrorCode::SchemaUnsupported
        );
        assert_eq!(
            SnapshotArtifact::from_yaml(&snapshot).unwrap_err().code,
            ErrorCode::SchemaUnsupported
        );
        assert_eq!(
            TagArtifact::from_yaml(&tag).unwrap_err().code,
            ErrorCode::SchemaUnsupported
        );
    }

    #[test]
    fn manifest_reader_rejects_duplicate_remote_names() {
        let yaml = MANIFEST_GOLDEN.replace(
            "  - name: origin\n    url: git@example.invalid:example.git\n    fetch: true\n    push: true\n",
            "  - name: origin\n    url: git@example.invalid:example.git\n    fetch: true\n    push: true\n  - name: origin\n    url: git@example.invalid:example-2.git\n    fetch: true\n    push: false\n",
        );

        assert_eq!(
            ManifestArtifact::from_yaml(&yaml).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn artifact_file_io_uses_workspace_paths() {
        let temp = TempDir::new("artifact-io");
        write_manifest(temp.path(), &sample_manifest()).unwrap();
        write_lock(temp.path(), &sample_lock()).unwrap();
        write_snapshot(temp.path(), &sample_snapshot()).unwrap();
        write_tag(temp.path(), &sample_tag()).unwrap();

        assert_eq!(read_manifest(temp.path()).unwrap(), sample_manifest());
        assert_eq!(read_lock(temp.path()).unwrap(), sample_lock());
        assert_eq!(
            read_snapshot(temp.path(), "snap_demo").unwrap(),
            sample_snapshot()
        );
        assert_eq!(read_tag(temp.path(), "demo").unwrap(), sample_tag());
    }

    #[test]
    fn atomic_write_replaces_existing_file_without_leftover_temp() {
        let temp = TempDir::new("atomic");
        let target = temp.path().join("nested/file.txt");

        write_atomic(&target, "old").unwrap();
        write_atomic(&target, "new").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(!temp.path().join("nested/file.txt.tmp").exists());
    }

    fn sample_manifest() -> ManifestArtifact {
        ManifestArtifact {
            schema: WORKSPACE_SCHEMA.to_owned(),
            workspace: WorkspaceHeader {
                id: "ws_01".to_owned(),
            },
            members: vec![ManifestMember {
                id: "mem_01".to_owned(),
                path: "repos/example".to_owned(),
                source_kind: ArtifactSourceKind::Git,
                source_id: "src_01".to_owned(),
                active: true,
                desired: Some(DesiredRefArtifact {
                    branch: Some("main".to_owned()),
                    ..DesiredRefArtifact::default()
                }),
                remotes: vec![RemoteArtifact {
                    name: "origin".to_owned(),
                    url: "git@example.invalid:example.git".to_owned(),
                    fetch: true,
                    push: true,
                }],
            }],
        }
    }

    fn sample_lock() -> LockArtifact {
        LockArtifact {
            schema: LOCK_SCHEMA.to_owned(),
            workspace_id: "ws_01".to_owned(),
            manifest_schema: WORKSPACE_SCHEMA.to_owned(),
            created_at: "2026-06-15T00:00:00Z".to_owned(),
            members: [("mem_01".to_owned(), sample_resolved_member())].into(),
        }
    }

    fn sample_snapshot() -> SnapshotArtifact {
        SnapshotArtifact {
            schema: SNAPSHOT_SCHEMA.to_owned(),
            workspace_id: "ws_01".to_owned(),
            snapshot_id: "snap_demo".to_owned(),
            created_at: "2026-06-15T00:00:00Z".to_owned(),
            created_by: CreatedByArtifact {
                actor_id: "agent_01".to_owned(),
            },
            selected_members: vec!["mem_01".to_owned()],
            members: [("mem_01".to_owned(), sample_short_member())].into(),
        }
    }

    fn sample_tag() -> TagArtifact {
        TagArtifact {
            schema: TAG_SCHEMA.to_owned(),
            workspace_id: "ws_01".to_owned(),
            tag: "demo".to_owned(),
            created_at: "2026-06-15T00:00:00Z".to_owned(),
            created_by: CreatedByArtifact {
                actor_id: "agent_01".to_owned(),
            },
            selected_members: vec!["mem_01".to_owned()],
            members: [("mem_01".to_owned(), sample_short_member())].into(),
        }
    }

    fn sample_resolved_member() -> ResolvedMemberArtifact {
        ResolvedMemberArtifact {
            path: "repos/example".to_owned(),
            source_id: Some("src_01".to_owned()),
            source_kind: ArtifactSourceKind::Git,
            commit: Some("abc123".to_owned()),
            branch: Some("main".to_owned()),
            detached: Some(false),
            upstream: Some("origin/main".to_owned()),
            dirty: Some(false),
            materialized: Some(true),
        }
    }

    fn sample_short_member() -> ResolvedMemberArtifact {
        ResolvedMemberArtifact {
            path: "repos/example".to_owned(),
            source_kind: ArtifactSourceKind::Git,
            commit: Some("abc123".to_owned()),
            ..ResolvedMemberArtifact::default()
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("gwz-core-{name}-{}-{unique}", std::process::id()));
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
