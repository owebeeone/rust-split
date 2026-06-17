use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::model::{ErrorCode, ModelError, ModelResult};

pub const WORKSPACE_DIR: &str = "gwz.conf";
pub const WORKSPACE_MANIFEST: &str = "gwz.conf/gwz.yml";
pub const RUNTIME_DIR: &str = ".gwz";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MemberPath(String);

impl MemberPath {
    pub fn parse(value: &str) -> ModelResult<Self> {
        let path = Path::new(value);
        if value.trim().is_empty() || path.is_absolute() {
            return Err(path_escape("member path must be relative"));
        }

        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => {
                    let value = value.to_string_lossy();
                    if value.is_empty() {
                        return Err(path_escape("member path contains an empty component"));
                    }
                    parts.push(value.into_owned());
                }
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(path_escape("member path escapes the workspace"));
                }
                Component::CurDir => {
                    return Err(path_escape("member path must not contain '.' components"));
                }
            }
        }

        if matches!(
            parts.first().map(String::as_str),
            Some(WORKSPACE_DIR) | Some(RUNTIME_DIR)
        ) {
            return Err(ModelError::new(
                ErrorCode::PathReserved,
                "member path uses a reserved GWZ prefix",
            ));
        }

        Ok(Self(parts.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MemberPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn validate_member_path_set(paths: &[MemberPath]) -> ModelResult<()> {
    for (index, left) in paths.iter().enumerate() {
        for right in paths.iter().skip(index + 1) {
            if paths_collide(left.as_str(), right.as_str()) {
                return Err(ModelError::new(
                    ErrorCode::PathCollision,
                    format!("member paths '{}' and '{}' collide", left, right),
                ));
            }
        }
    }
    Ok(())
}

pub fn discover_workspace_root(start: &Path) -> ModelResult<PathBuf> {
    let mut current = if start.is_file() {
        start
            .parent()
            .ok_or_else(workspace_config_missing)?
            .to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        if current.join(WORKSPACE_MANIFEST).is_file() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(workspace_config_missing());
        }
    }
}

fn workspace_config_missing() -> ModelError {
    ModelError::new(
        ErrorCode::WorkspaceNotFound,
        format!("{WORKSPACE_MANIFEST} missing"),
    )
}

pub fn preflight_create_workspace(target: &Path) -> ModelResult<&Path> {
    if target.join(WORKSPACE_MANIFEST).exists() {
        return Err(ModelError::new(
            ErrorCode::WorkspaceAlreadyExists,
            "target already contains a GWZ workspace",
        ));
    }

    let mut current = target.parent();
    while let Some(parent) = current {
        if parent.join(WORKSPACE_MANIFEST).exists() {
            return Err(ModelError::new(
                ErrorCode::NestedWorkspace,
                "cannot create a GWZ workspace inside another GWZ workspace",
            ));
        }
        current = parent.parent();
    }

    Ok(target)
}

pub fn reject_nested_active_workspace(
    workspace_root: &Path,
    member_path: &MemberPath,
    active: bool,
) -> ModelResult<()> {
    if active
        && workspace_root
            .join(member_path.as_str())
            .join(WORKSPACE_MANIFEST)
            .exists()
    {
        Err(ModelError::new(
            ErrorCode::NestedWorkspace,
            "active member root contains a nested GWZ workspace",
        ))
    } else {
        Ok(())
    }
}

fn paths_collide(left: &str, right: &str) -> bool {
    left == right
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn path_escape(message: &str) -> ModelError {
    ModelError::new(ErrorCode::PathEscape, message)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::model::ErrorCode;

    use super::*;

    #[test]
    fn member_paths_reject_absolute_escape_and_reserved_prefixes() {
        assert_eq!(
            MemberPath::parse("/absolute").unwrap_err().code,
            ErrorCode::PathEscape
        );
        assert_eq!(
            MemberPath::parse("../escape").unwrap_err().code,
            ErrorCode::PathEscape
        );
        assert_eq!(
            MemberPath::parse("gwz.conf/meta").unwrap_err().code,
            ErrorCode::PathReserved
        );
        assert_eq!(
            MemberPath::parse(".gwz/runtime").unwrap_err().code,
            ErrorCode::PathReserved
        );
        assert_eq!(
            MemberPath::parse("repos/core").unwrap().as_str(),
            "repos/core"
        );
    }

    #[test]
    fn member_path_sets_reject_equal_and_nested_collisions() {
        let one = MemberPath::parse("repos/core").unwrap();
        let same = MemberPath::parse("repos/core").unwrap();
        let nested = MemberPath::parse("repos/core/tools").unwrap();

        assert_eq!(
            validate_member_path_set(&[one.clone(), same])
                .unwrap_err()
                .code,
            ErrorCode::PathCollision
        );
        assert_eq!(
            validate_member_path_set(&[one, nested]).unwrap_err().code,
            ErrorCode::PathCollision
        );
    }

    #[test]
    fn discovery_walks_up_to_workspace_manifest() {
        let temp = TempDir::new("discover");
        touch_workspace_manifest(temp.path());
        fs::create_dir_all(temp.path().join("repos/core/src")).unwrap();

        let root = discover_workspace_root(&temp.path().join("repos/core/src")).unwrap();

        assert_eq!(root, temp.path());
    }

    #[test]
    fn init_target_uses_requested_directory_without_discovering_upward() {
        let temp = TempDir::new("init");
        touch_workspace_manifest(temp.path());
        let child = temp.path().join("ordinary/child");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("README.md"), "not a gwz workspace").unwrap();

        assert_eq!(
            preflight_create_workspace(temp.path()).unwrap_err().code,
            ErrorCode::WorkspaceAlreadyExists
        );
        assert_eq!(
            preflight_create_workspace(&child).unwrap_err().code,
            ErrorCode::NestedWorkspace
        );

        let standalone = TempDir::new("standalone");
        fs::write(
            standalone.path().join("README.md"),
            "ordinary files are allowed",
        )
        .unwrap();
        assert_eq!(
            preflight_create_workspace(standalone.path()).unwrap(),
            standalone.path()
        );
    }

    #[test]
    fn active_member_roots_must_not_contain_their_own_workspace() {
        let temp = TempDir::new("nested-member");
        let member_path = MemberPath::parse("repos/core").unwrap();
        touch_workspace_manifest(&temp.path().join(member_path.as_str()));

        assert_eq!(
            reject_nested_active_workspace(temp.path(), &member_path, true)
                .unwrap_err()
                .code,
            ErrorCode::NestedWorkspace
        );
        assert!(reject_nested_active_workspace(temp.path(), &member_path, false).is_ok());
    }

    fn touch_workspace_manifest(root: &Path) {
        fs::create_dir_all(root.join(WORKSPACE_DIR)).unwrap();
        fs::write(root.join(WORKSPACE_MANIFEST), "schema: gwz.workspace/v0\n").unwrap();
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
