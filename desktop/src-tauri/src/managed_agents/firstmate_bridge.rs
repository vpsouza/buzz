use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

struct BridgeFile {
    relative_path: &'static str,
    contents: &'static [u8],
    executable: bool,
}

const BRIDGE_FILES: &[BridgeFile] = &[
    BridgeFile {
        relative_path: "bin/fm-buzz-lifecycle.sh",
        contents: include_bytes!("../../firstmate-bridge/bin/fm-buzz-lifecycle.sh"),
        executable: true,
    },
    BridgeFile {
        relative_path: "bin/fm-buzz-supervisor.sh",
        contents: include_bytes!("../../firstmate-bridge/bin/fm-buzz-supervisor.sh"),
        executable: true,
    },
    BridgeFile {
        relative_path: "bin/fm-buzz.sh",
        contents: include_bytes!("../../firstmate-bridge/bin/fm-buzz.sh"),
        executable: true,
    },
    BridgeFile {
        relative_path: "bin/fm-supervision-lib.sh",
        contents: include_bytes!("../../firstmate-bridge/bin/fm-supervision-lib.sh"),
        executable: false,
    },
    BridgeFile {
        relative_path: "bin/fm-session-lock-lib.sh",
        contents: include_bytes!("../../firstmate-bridge/bin/fm-session-lock-lib.sh"),
        executable: false,
    },
    BridgeFile {
        relative_path: "bin/fm-cursor-lib.sh",
        contents: include_bytes!("../../firstmate-bridge/bin/fm-cursor-lib.sh"),
        executable: true,
    },
];

/// Install the self-contained Buzz integration layer into an older FirstMate
/// home. Existing files are never replaced: a divergent bridge must be
/// reviewed explicitly instead of being overwritten by selecting a folder in
/// the UI.
pub(crate) fn ensure_firstmate_bridge(home: &Path) -> Result<Vec<String>, String> {
    let bin = home.join("bin");
    let metadata = std::fs::symlink_metadata(&bin)
        .map_err(|error| format!("cannot inspect FirstMate bin directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "FirstMate bin directory must be a real directory: {}",
            bin.display()
        ));
    }

    let mut installed = Vec::new();
    for bridge_file in BRIDGE_FILES {
        let target = home.join(bridge_file.relative_path);
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(format!(
                        "FirstMate Buzz bridge path is not a regular file: {}",
                        target.display()
                    ));
                }
                let existing = std::fs::read(&target).map_err(|error| {
                    format!(
                        "cannot read FirstMate Buzz bridge {}: {error}",
                        target.display()
                    )
                })?;
                if existing != bridge_file.contents {
                    return Err(format!(
                        "FirstMate Buzz bridge file differs from this Buzz build: {}. Back it up and update the bridge explicitly before starting this agent",
                        target.display()
                    ));
                }
                ensure_executable(&target, bridge_file.executable)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map_err(|error| {
                        format!(
                            "cannot install FirstMate Buzz bridge {}: {error}",
                            target.display()
                        )
                    })?;
                file.write_all(bridge_file.contents).map_err(|error| {
                    format!(
                        "cannot write FirstMate Buzz bridge {}: {error}",
                        target.display()
                    )
                })?;
                file.sync_all().map_err(|error| {
                    format!(
                        "cannot sync FirstMate Buzz bridge {}: {error}",
                        target.display()
                    )
                })?;
                ensure_executable(&target, bridge_file.executable)?;
                installed.push(bridge_file.relative_path.to_string());
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect FirstMate Buzz bridge {}: {error}",
                    target.display()
                ));
            }
        }
    }
    Ok(installed)
}

#[cfg(unix)]
fn ensure_executable(path: &Path, executable: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if !executable {
        return Ok(());
    }
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot inspect bridge permissions: {error}"))?;
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 {
        let mut permissions = metadata.permissions();
        permissions.set_mode(mode | 0o700);
        std::fs::set_permissions(path, permissions)
            .map_err(|error| format!("cannot make FirstMate Buzz bridge executable: {error}"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_executable(_path: &Path, _executable: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_home(name: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "buzz-firstmate-bridge-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        home
    }

    #[test]
    fn installs_missing_bridge_and_is_idempotent() {
        let home = test_home("install");
        let installed = ensure_firstmate_bridge(&home).unwrap();
        assert_eq!(installed.len(), BRIDGE_FILES.len());
        assert!(ensure_firstmate_bridge(&home).unwrap().is_empty());
        for bridge_file in BRIDGE_FILES {
            assert_eq!(
                std::fs::read(home.join(bridge_file.relative_path)).unwrap(),
                bridge_file.contents
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn refuses_to_overwrite_a_different_existing_bridge() {
        let home = test_home("different");
        let target = home.join(BRIDGE_FILES[0].relative_path);
        std::fs::write(&target, "user-owned bridge").unwrap();
        let error = ensure_firstmate_bridge(&home).unwrap_err();
        assert!(error.contains("differs from this Buzz build"));
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "user-owned bridge"
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
