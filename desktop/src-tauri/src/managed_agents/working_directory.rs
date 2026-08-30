//! Trusted local working-directory validation for managed ACP agents.

use std::path::{Path, PathBuf};

use super::ManagedAgentRecord;

/// Validate and canonicalize an explicitly selected managed-agent directory.
///
/// The original path and the canonical target must both be real directories.
/// Rejecting an input symlink makes the selected local authority visible to the
/// user; storing the canonical path then prevents equivalent spellings from
/// creating separate FirstMate homes.
pub(crate) fn validate_working_directory(path: Option<PathBuf>) -> Result<Option<PathBuf>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err("working directory must be an absolute path".to_string());
    }
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| format!("working directory does not exist: {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err("working directory must not be a symlink".to_string());
    }
    if !metadata.is_dir() {
        return Err("working directory must be a directory".to_string());
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|error| format!("failed to canonicalize working directory: {error}"))?;
    let canonical_metadata = std::fs::symlink_metadata(&canonical)
        .map_err(|error| format!("failed to inspect working directory: {error}"))?;
    if canonical_metadata.file_type().is_symlink() || !canonical_metadata.is_dir() {
        return Err("working directory must resolve to a real directory".to_string());
    }
    Ok(Some(canonical))
}

/// Validate the on-disk FirstMate integration contract.
pub(crate) fn validate_firstmate_home(path: &Path) -> Result<(), String> {
    for required in ["AGENTS.md", "bin", "state"] {
        let candidate = path.join(required);
        let metadata = std::fs::symlink_metadata(&candidate).map_err(|_| {
            format!(
                "FirstMate home is missing required {}: {}",
                if required == "AGENTS.md" {
                    "file"
                } else {
                    "directory"
                },
                candidate.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "FirstMate home required path must not be a symlink: {}",
                candidate.display()
            ));
        }
        let valid_type = if required == "AGENTS.md" {
            metadata.is_file()
        } else {
            metadata.is_dir()
        };
        if !valid_type {
            return Err("FirstMate home must contain AGENTS.md, bin/, and state/".to_string());
        }
    }
    Ok(())
}

pub(crate) fn validate_record_working_directory(
    record: &ManagedAgentRecord,
) -> Result<Option<PathBuf>, String> {
    let path = validate_working_directory(record.working_directory.clone())?;
    if record.firstmate {
        let home = path
            .as_deref()
            .ok_or_else(|| "FirstMate agents require a FirstMate working directory".to_string())?;
        validate_firstmate_home(home)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_missing_and_file_paths() {
        assert!(validate_working_directory(Some(PathBuf::from("relative"))).is_err());
        assert!(
            validate_working_directory(Some(PathBuf::from("/definitely/missing/buzz-home")))
                .is_err()
        );
        let file =
            std::env::temp_dir().join(format!("buzz-working-dir-file-{}", std::process::id()));
        std::fs::write(&file, "x").unwrap();
        assert!(validate_working_directory(Some(file.clone())).is_err());
        let _ = std::fs::remove_file(file);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_path() {
        use std::os::unix::fs::symlink;
        let root =
            std::env::temp_dir().join(format!("buzz-working-dir-link-{}", std::process::id()));
        let target = root.join("target");
        let link = root.join("link");
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();
        assert!(validate_working_directory(Some(link)).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_firstmate_contract_paths() {
        use std::os::unix::fs::symlink;

        for required in ["AGENTS.md", "bin", "state"] {
            let root = std::env::temp_dir().join(format!(
                "buzz-firstmate-contract-link-{}-{required}",
                std::process::id()
            ));
            let home = root.join("home");
            let external = root.join("external");
            std::fs::create_dir_all(home.join("bin")).unwrap();
            std::fs::create_dir_all(home.join("state")).unwrap();
            std::fs::write(home.join("AGENTS.md"), "test").unwrap();

            let target = external.join(required);
            if required == "AGENTS.md" {
                std::fs::create_dir_all(&external).unwrap();
                std::fs::write(&target, "external").unwrap();
                std::fs::remove_file(home.join(required)).unwrap();
            } else {
                std::fs::create_dir_all(&target).unwrap();
                std::fs::remove_dir(home.join(required)).unwrap();
            }
            symlink(&target, home.join(required)).unwrap();

            assert!(validate_firstmate_home(&home).is_err());
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
