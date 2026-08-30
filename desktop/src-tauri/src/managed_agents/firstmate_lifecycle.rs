//! FirstMate-owned stop handoff for managed FirstMate primaries.
//!
//! Buzz deliberately does not inspect FirstMate task state. It invokes the
//! home-local lifecycle command and accepts only its narrow machine-readable
//! result before terminating the ACP process.

use std::{fs, path::Path, process::Command};

use super::{validate_record_working_directory, ManagedAgentRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstMateStopDisposition {
    Safe,
    SupervisionTransferred,
}

/// Parse the first line emitted by `fm-buzz-lifecycle.sh prepare-stop`.
///
/// The FirstMate command owns the policy. Anything unknown, malformed, or
/// explicitly refused is rejected so a Desktop stop can never silently bypass
/// uncertain crew supervision.
pub(crate) fn parse_firstmate_stop_report(
    output: &str,
) -> Result<FirstMateStopDisposition, String> {
    let first_line = output.lines().next().unwrap_or_default().trim();
    match first_line {
        "safe" => Ok(FirstMateStopDisposition::Safe),
        "supervision-transferred" => Ok(FirstMateStopDisposition::SupervisionTransferred),
        report if report.starts_with("refused:") => Err(format!(
            "FirstMate refused ordinary stop: {}",
            report.trim_start_matches("refused:").trim()
        )),
        _ => Err(format!(
            "FirstMate lifecycle returned an unrecognized stop report: {first_line:?}"
        )),
    }
}

/// Ask a FirstMate home to transfer or refuse supervision before ordinary
/// managed-agent termination. Channel-scoped agents retain the existing stop
/// behavior unchanged.
fn supervisor_attestation(home: &Path, harness_pids: &[u32]) -> Option<(u32, u64)> {
    let lease = home.join("state/.buzz-supervisor/lease");
    let metadata = lease.symlink_metadata().ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let contents = fs::read_to_string(lease).ok()?;
    let mut version = None;
    let mut lease_home = None;
    let mut pid = None;
    let mut generation = None;
    for line in contents.lines() {
        let (key, value) = line.split_once('=')?;
        let slot = match key {
            "version" => &mut version,
            "home" => &mut lease_home,
            "pid" => &mut pid,
            "generation" => &mut generation,
            _ => return None,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    if version != Some("1") || lease_home != home.to_str() {
        return None;
    }
    let pid = pid?.parse::<u32>().ok()?;
    let generation = generation?.parse::<u64>().ok()?;
    harness_pids.contains(&pid).then_some((pid, generation))
}

pub(crate) fn prepare_firstmate_stop(
    record: &ManagedAgentRecord,
    harness_pids: &[u32],
) -> Result<(), String> {
    if !record.firstmate {
        return Ok(());
    }

    let home = validate_record_working_directory(record)?
        .ok_or("FirstMate managed agent is missing its working directory")?;
    let lifecycle = home.join("bin/fm-buzz-lifecycle.sh");
    let metadata = lifecycle
        .symlink_metadata()
        .map_err(|error| format!("FirstMate lifecycle command is unavailable: {error}"))?;
    if !metadata.is_file() {
        return Err(format!(
            "FirstMate lifecycle command is not a regular file: {}",
            lifecycle.display()
        ));
    }

    let mut command = Command::new(&lifecycle);
    command
        .arg("prepare-stop")
        .current_dir(&home)
        .env("FM_HOME", &home)
        .env_remove("BUZZ_FIRSTMATE_HARNESS_PID")
        .env_remove("BUZZ_FIRSTMATE_SUPERVISOR_GENERATION");
    if let Some((pid, generation)) = supervisor_attestation(&home, harness_pids) {
        command
            .env("BUZZ_FIRSTMATE_HARNESS_PID", pid.to_string())
            .env(
                "BUZZ_FIRSTMATE_SUPERVISOR_GENERATION",
                generation.to_string(),
            );
    }
    let output = command
        .output()
        .map_err(|error| format!("failed to run FirstMate lifecycle command: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "FirstMate lifecycle command failed ({}){}",
            output.status,
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        ));
    }
    let _disposition = parse_firstmate_stop_report(&stdout)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_safe_and_supervision_transfer_only() {
        assert_eq!(
            parse_firstmate_stop_report("safe\n"),
            Ok(FirstMateStopDisposition::Safe)
        );
        assert_eq!(
            parse_firstmate_stop_report("supervision-transferred\n"),
            Ok(FirstMateStopDisposition::SupervisionTransferred)
        );
    }

    #[test]
    fn fails_closed_on_refusal_or_unknown_report() {
        assert!(parse_firstmate_stop_report("refused: active work\n").is_err());
        assert!(parse_firstmate_stop_report("maybe\n").is_err());
        assert!(parse_firstmate_stop_report("").is_err());
    }

    #[test]
    fn accepts_only_a_home_bound_lease_for_a_tracked_harness() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state/.buzz-supervisor");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("lease"),
            format!(
                "version=1\nhome={}\npid=42\ngeneration=7\n",
                dir.path().display()
            ),
        )
        .unwrap();
        assert_eq!(supervisor_attestation(dir.path(), &[41, 42]), Some((42, 7)));
        assert_eq!(supervisor_attestation(dir.path(), &[41]), None);
        fs::write(
            root.join("lease"),
            "version=1\nhome=/tmp/foreign\npid=42\ngeneration=7\n",
        )
        .unwrap();
        assert_eq!(supervisor_attestation(dir.path(), &[42]), None);
    }
}
