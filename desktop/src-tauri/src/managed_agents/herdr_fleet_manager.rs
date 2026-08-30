//! Owner-exact fleet coordination for FirstMate instances running in Herdr.
//!
//! A Herdr server is a shared resource, but a FirstMate home is not. This
//! module deliberately keeps those two lifecycles separate: an agent owns an
//! exact workspace/root-pane endpoint and holds a lease on the shared server;
//! releasing one endpoint never stops the server while another Buzz endpoint
//! remains. The concrete socket/CLI implementation lives behind
//! [`HerdrFleetBroker`] so this policy can be tested without touching a user's
//! Herdr session.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, MutexGuard, OnceLock},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Dedicated session used by Buzz. Never silently adopt the user's default
/// Herdr server/session.
pub(crate) const DEFAULT_HERDR_SESSION: &str = "buzz-firstmate";

/// A newly-created pane can briefly report its launch-directory before the
/// shell has entered the requested FirstMate home. Retrying is allowed only
/// before a fresh endpoint is persisted; recovered snapshots stay fail-closed.
const FRESH_ENDPOINT_PREFLIGHT_ATTEMPTS: usize = 3;
const FRESH_ENDPOINT_PREFLIGHT_BACKOFF_MS: u64 = 40;

const HERDR_INHERITED_ENV: &[&str] = &[
    "HERDR_ENV",
    "HERDR_SOCKET_PATH",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
];

const HERDR_CONTRACT_ENV: &[&str] = &[
    "BUZZ_FIRSTMATE_HERDR_CONTRACT_VERSION",
    "BUZZ_FIRSTMATE_HERDR_BINARY",
    "BUZZ_FIRSTMATE_HERDR_SESSION",
    "BUZZ_FIRSTMATE_HERDR_SOCKET_PATH",
    "BUZZ_FIRSTMATE_HERDR_WORKSPACE_ID",
    "BUZZ_FIRSTMATE_HERDR_ROOT_TAB_ID",
    "BUZZ_FIRSTMATE_HERDR_ROOT_PANE_ID",
];

// Desktop's single-instance contract makes this process-local lock sufficient
// for production. It serializes the complete load → broker mutation → atomic
// save transition used by concurrent launch restore tasks; a poisoned lock
// fails closed rather than guessing which endpoint is authoritative.
static FLEET_STATE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_fleet_state() -> Result<MutexGuard<'static, ()>, String> {
    FLEET_STATE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Herdr fleet state lock is poisoned".to_string())
}

/// Multiplexer selection is per managed FirstMate. Tmux remains the explicit
/// conservative default during the Herdr rollout; no ambient `HERDR_ENV`
/// changes this choice.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FirstMateMultiplexer {
    #[default]
    Tmux,
    Herdr,
}

impl FirstMateMultiplexer {
    pub(crate) fn is_herdr(self) -> bool {
        matches!(self, Self::Herdr)
    }

    pub(crate) fn from_agent_env(env: &BTreeMap<String, String>) -> Result<Self, String> {
        match env
            .get("BUZZ_FIRSTMATE_MULTIPLEXER")
            .map(String::as_str)
            .unwrap_or("tmux")
        {
            "tmux" => Ok(Self::Tmux),
            "herdr" => Ok(Self::Herdr),
            value => Err(format!("unsupported FirstMate multiplexer: {value}")),
        }
    }
}

/// Stable identity for a managed FirstMate home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HerdrManagedAgentIdentity {
    pub managed_agent_id: String,
    pub canonical_home: PathBuf,
    pub home_sha256: String,
}

impl HerdrManagedAgentIdentity {
    pub(crate) fn new(managed_agent_id: impl Into<String>, home: &Path) -> Result<Self, String> {
        let managed_agent_id = managed_agent_id.into();
        if managed_agent_id.trim().is_empty() {
            return Err("Herdr managed-agent identity is empty".to_string());
        }
        let canonical_home = std::fs::canonicalize(home)
            .map_err(|error| format!("failed to canonicalize FirstMate home: {error}"))?;
        if !canonical_home.is_dir() {
            return Err("FirstMate home must resolve to a directory".to_string());
        }
        Ok(Self {
            managed_agent_id,
            home_sha256: home_sha256(&canonical_home),
            canonical_home,
        })
    }

    fn key(&self) -> String {
        format!("{}:{}", self.managed_agent_id, self.home_sha256)
    }
}

/// Exact Herdr endpoint authority for one FirstMate primary. Labels are
/// intentionally absent: labels are presentation and must not authorize a
/// workspace, pane, or recovery operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HerdrParentIdentity {
    pub session: String,
    pub socket_path: PathBuf,
    pub workspace_id: String,
    pub root_tab_id: String,
    pub root_pane_id: String,
}

impl HerdrParentIdentity {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.session.trim().is_empty()
            || self.workspace_id.trim().is_empty()
            || self.root_tab_id.trim().is_empty()
            || self.root_pane_id.trim().is_empty()
        {
            return Err("Herdr parent identity is incomplete".to_string());
        }
        if !self.socket_path.is_absolute() {
            return Err("Herdr socket path must be absolute".to_string());
        }
        Ok(())
    }

    /// The explicit, adapter-facing contract. The FirstMate adapter validates
    /// this endpoint against the socket before it performs workspace discovery.
    pub(crate) fn firstmate_env(
        &self,
        home: &Path,
        herdr_binary: &Path,
    ) -> Result<Vec<(&'static str, String)>, String> {
        self.validate()?;
        let canonical_home = std::fs::canonicalize(home)
            .map_err(|error| format!("failed to canonicalize FirstMate home: {error}"))?;
        let canonical_binary = verified_executable(herdr_binary).ok_or_else(|| {
            format!(
                "Herdr binary is not an executable regular file: {}",
                herdr_binary.display()
            )
        })?;
        Ok(vec![
            ("BUZZ_FIRSTMATE_HERDR_CONTRACT_VERSION", "1".to_string()),
            ("BUZZ_FIRSTMATE_HOME", canonical_home.display().to_string()),
            (
                "BUZZ_FIRSTMATE_HERDR_BINARY",
                canonical_binary.display().to_string(),
            ),
            ("BUZZ_FIRSTMATE_HERDR_SESSION", self.session.clone()),
            (
                "BUZZ_FIRSTMATE_HERDR_SOCKET_PATH",
                self.socket_path.display().to_string(),
            ),
            (
                "BUZZ_FIRSTMATE_HERDR_WORKSPACE_ID",
                self.workspace_id.clone(),
            ),
            ("BUZZ_FIRSTMATE_HERDR_ROOT_TAB_ID", self.root_tab_id.clone()),
            (
                "BUZZ_FIRSTMATE_HERDR_ROOT_PANE_ID",
                self.root_pane_id.clone(),
            ),
        ])
    }
}

/// Persisted owner endpoint and the server ownership fact required for safe
/// teardown. This is intentionally independent from a process PID: the
/// endpoint remains useful for preflight/recovery after Desktop restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HerdrSpawnSnapshot {
    pub identity: HerdrManagedAgentIdentity,
    pub parent: HerdrParentIdentity,
    /// Opaque broker-issued lease identifying the shared server instance.
    pub lease_id: String,
    pub server_started_by_buzz: bool,
}

/// Persisted state for the shared server and its per-agent leases.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HerdrFleetState {
    #[serde(default = "default_herdr_session")]
    pub session: String,
    /// Ownership belongs to the dedicated shared server, not to whichever
    /// FirstMate happened to be started first or stopped last.
    #[serde(default)]
    pub server_started_by_buzz: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_lease: Option<HerdrSessionLease>,
    #[serde(default)]
    pub agents: BTreeMap<String, HerdrSpawnSnapshot>,
}

fn default_herdr_session() -> String {
    DEFAULT_HERDR_SESSION.to_string()
}

/// Result of the broker's dedicated-session ensure/adopt operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HerdrSessionLease {
    pub session: String,
    pub socket_path: PathBuf,
    pub lease_id: String,
    /// `true` only if this invocation created the server. An adopted session is
    /// never eligible for automatic stop.
    pub started_by_buzz: bool,
}

/// Request for a stable workspace belonging to one owner identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrWorkspaceRequest {
    pub identity: HerdrManagedAgentIdentity,
}

/// The small broker surface the fleet manager needs. Implementations must use
/// exact IDs returned by Herdr; a label lookup is not a valid implementation.
pub(crate) trait HerdrFleetBroker {
    fn ensure_or_adopt_session(&mut self, session: &str) -> Result<HerdrSessionLease, String>;
    fn ensure_workspace(
        &mut self,
        lease: &HerdrSessionLease,
        request: &HerdrWorkspaceRequest,
    ) -> Result<HerdrParentIdentity, String>;
    /// Verify this exact root pane belongs to this exact canonical home. A
    /// pane/tab/workspace topology check alone is insufficient: it could bind
    /// Atlas to Nova's otherwise valid workspace.
    fn preflight(
        &mut self,
        parent: &HerdrParentIdentity,
        identity: &HerdrManagedAgentIdentity,
    ) -> Result<(), String>;
    /// Close only the exact owner workspace and prove it is absent afterwards.
    /// Implementations must never select a workspace by its presentation label.
    fn release_workspace(&mut self, parent: &HerdrParentIdentity) -> Result<(), String>;
    fn stop_session(&mut self, lease: &HerdrSessionLease) -> Result<(), String>;
}

/// Coordinates leases for the single dedicated Herdr session.
#[derive(Debug, Clone)]
pub(crate) struct HerdrFleetManager {
    state: HerdrFleetState,
}

impl HerdrFleetManager {
    pub(crate) fn new(session: impl Into<String>) -> Result<Self, String> {
        let session = session.into();
        validate_dedicated_session(&session)?;
        Ok(Self {
            state: HerdrFleetState {
                session,
                server_started_by_buzz: false,
                server_lease: None,
                agents: BTreeMap::new(),
            },
        })
    }

    pub(crate) fn from_state(state: HerdrFleetState) -> Result<Self, String> {
        validate_dedicated_session(&state.session)?;
        if !state.agents.is_empty() && state.server_lease.is_none() {
            return Err("Herdr fleet state with agents is missing its server lease".to_string());
        }
        if state.server_started_by_buzz && state.server_lease.is_none() {
            return Err("Buzz-owned Herdr fleet state is missing its server lease".to_string());
        }
        for (key, snapshot) in &state.agents {
            snapshot.parent.validate()?;
            if snapshot.lease_id.trim().is_empty() {
                return Err("Herdr fleet state contains an empty lease id".to_string());
            }
            if snapshot.identity.home_sha256 != home_sha256(&snapshot.identity.canonical_home) {
                return Err(
                    "Herdr fleet state contains a mismatched FirstMate home hash".to_string(),
                );
            }
            if key != &snapshot.identity.key() {
                return Err(
                    "Herdr fleet state contains an endpoint under the wrong owner key".to_string(),
                );
            }
            if snapshot.parent.session != state.session {
                return Err("Herdr endpoint belongs to a different session".to_string());
            }
            let lease = state
                .server_lease
                .as_ref()
                .ok_or("Herdr fleet state is missing its lease")?;
            if snapshot.parent.socket_path != lease.socket_path
                || snapshot.lease_id != lease.lease_id
            {
                return Err(
                    "Herdr endpoint does not belong to the persisted server lease".to_string(),
                );
            }
        }
        Ok(Self { state })
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &HerdrFleetState {
        &self.state
    }

    /// Read persisted endpoint state. Absence is a first launch, not an error;
    /// malformed state fails closed before it can point a FirstMate at an
    /// unknown workspace.
    pub(crate) fn load_from_path(path: &Path) -> Result<Self, String> {
        let contents = match std::fs::read(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::new(DEFAULT_HERDR_SESSION);
            }
            Err(error) => return Err(format!("failed to read Herdr fleet state: {error}")),
        };
        let state = serde_json::from_slice(&contents)
            .map_err(|error| format!("invalid Herdr fleet state: {error}"))?;
        Self::from_state(state)
    }

    /// Persist the shared-server ownership fact and every exact endpoint as a
    /// single atomic JSON record. Callers persist after a successful
    /// ensure/release transition, before they launch/stop an ACP child.
    pub(crate) fn save_to_path(&self, path: &Path) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or("Herdr fleet state path has no parent directory")?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create Herdr fleet state directory: {error}"))?;
        let payload = serde_json::to_vec_pretty(&self.state)
            .map_err(|error| format!("failed to serialize Herdr fleet state: {error}"))?;
        super::storage::atomic_write_json(path, &payload)
    }

    /// Ensure/adopt the shared server and create/adopt only this agent's exact
    /// workspace. A stale snapshot fails closed rather than falling back to a
    /// label search that could target another FirstMate.
    pub(crate) fn ensure_agent(
        &mut self,
        broker: &mut impl HerdrFleetBroker,
        identity: HerdrManagedAgentIdentity,
    ) -> Result<HerdrSpawnSnapshot, String> {
        let key = identity.key();
        if let Some(snapshot) = self.state.agents.get(&key) {
            broker
                .preflight(&snapshot.parent, &identity)
                .map_err(|error| {
                    format!(
                        "stale Herdr endpoint for managed agent {}: {error}",
                        identity.managed_agent_id
                    )
                })?;
            return Ok(snapshot.clone());
        }

        let lease = broker.ensure_or_adopt_session(&self.state.session)?;
        if lease.session != self.state.session {
            return Err("Herdr broker returned a different session than requested".to_string());
        }
        if !lease.socket_path.is_absolute() {
            return Err("Herdr broker returned a non-absolute socket path".to_string());
        }
        if lease.lease_id.trim().is_empty() {
            return Err("Herdr broker returned an empty server lease id".to_string());
        }
        let effective_lease = if let Some(persisted) = &self.state.server_lease {
            if persisted.session != lease.session || persisted.socket_path != lease.socket_path {
                return Err(
                    "Herdr broker returned a different server than the persisted lease".to_string(),
                );
            }
            // A Desktop restart adopts the same live server and receives a new
            // observation result from the CLI. Keep the original persisted
            // lease id/ownership proof; comparing a newly observed opaque id
            // would make valid restart recovery fail closed forever.
            persisted.clone()
        } else {
            lease.clone()
        };
        let parent = broker.ensure_workspace(
            &lease,
            &HerdrWorkspaceRequest {
                identity: identity.clone(),
            },
        )?;
        parent.validate()?;
        if parent.session != lease.session || parent.socket_path != lease.socket_path {
            return Err("Herdr broker returned a workspace outside the leased session".to_string());
        }
        if let Err(error) = preflight_fresh_endpoint(broker, &parent, &identity) {
            let preflight_error = format!(
                "Herdr endpoint failed preflight for managed agent {}: {error}",
                identity.managed_agent_id
            );
            return match broker.release_workspace(&parent) {
                Ok(()) => Err(preflight_error),
                Err(rollback_error) => Err(format!(
                    "{preflight_error}; exact workspace rollback also failed: {rollback_error}"
                )),
            };
        }

        let snapshot = HerdrSpawnSnapshot {
            identity,
            parent,
            lease_id: effective_lease.lease_id.clone(),
            server_started_by_buzz: lease.started_by_buzz,
        };
        if self.state.server_lease.is_none() {
            self.state.server_lease = Some(effective_lease);
        }
        if lease.started_by_buzz {
            self.state.server_started_by_buzz = true;
        }
        self.state.agents.insert(key, snapshot.clone());
        Ok(snapshot)
    }

    /// Release one FirstMate endpoint. The server is stopped only when it was
    /// created by Buzz *and* no Buzz endpoint remains in the dedicated session.
    pub(crate) fn release_agent(
        &mut self,
        broker: &mut impl HerdrFleetBroker,
        identity: &HerdrManagedAgentIdentity,
    ) -> Result<(), String> {
        if !self.state.agents.contains_key(&identity.key()) {
            return Ok(());
        }
        if self.state.agents.len() == 1 && self.state.server_started_by_buzz {
            let lease = self
                .state
                .server_lease
                .clone()
                .ok_or("Buzz-owned Herdr server has no persisted lease")?;
            // Do not discard the endpoint or lease until stop succeeds: a
            // failure must retain retry authority rather than silently
            // orphaning a server.
            broker.stop_session(&lease)?;
            self.state.agents.remove(&identity.key());
            self.state.server_started_by_buzz = false;
            self.state.server_lease = None;
            return Ok(());
        }

        let snapshot = self
            .state
            .agents
            .get(&identity.key())
            .cloned()
            .ok_or("FirstMate Herdr endpoint disappeared during release")?;
        // The server remains alive (another Buzz agent, or an adopted server),
        // so close this exact workspace before changing persisted state. If
        // close/preflight fails, retain the snapshot for a safe retry.
        broker.release_workspace(&snapshot.parent)?;
        self.state.agents.remove(&identity.key());
        if self.state.agents.is_empty() {
            self.state.server_lease = None;
        }
        Ok(())
    }

    /// Remove ambient Herdr identity before spawning a FirstMate. `HERDR_ENV`
    /// alone must never select a workspace; callers add the validated
    /// `BUZZ_FIRSTMATE_HERDR_*` contract afterwards.
    pub(crate) fn scrub_inherited_herdr_env(command: &mut std::process::Command) {
        for key in HERDR_INHERITED_ENV {
            command.env_remove(key);
        }
        for key in HERDR_CONTRACT_ENV {
            command.env_remove(key);
        }
    }
}

fn preflight_fresh_endpoint(
    broker: &mut impl HerdrFleetBroker,
    parent: &HerdrParentIdentity,
    identity: &HerdrManagedAgentIdentity,
) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 0..FRESH_ENDPOINT_PREFLIGHT_ATTEMPTS {
        match broker.preflight(parent, identity) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < FRESH_ENDPOINT_PREFLIGHT_ATTEMPTS {
                    let delay = FRESH_ENDPOINT_PREFLIGHT_BACKOFF_MS * (attempt as u64 + 1);
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "fresh Herdr endpoint preflight did not run".to_string()))
}

/// Concrete local broker for the dedicated named Herdr session. Every command
/// carries `--session`; it never uses `HERDR_ENV` or the user's default
/// session. JSON response parsing is deliberately strict: unsupported Herdr
/// output is an error, not an invitation to recover by label.
pub(crate) struct HerdrCliBroker {
    executable: PathBuf,
}

impl HerdrCliBroker {
    /// Resolve once before the first broker operation. Finder-launched Desktop
    /// processes commonly have a minimal PATH. An installed FirstMate bundle
    /// carries its broker-compatible Herdr beside `buzz-desktop`; prefer that
    /// exact companion before consulting the user's login shell or PATH.
    fn discover() -> Result<Self, String> {
        let executable = resolve_bundled_herdr()
            .or_else(|| super::discovery::find_via_login_shell("herdr"))
            .and_then(|path| verified_executable(&path))
            .or_else(resolve_herdr_from_process_path)
            .ok_or("Herdr is unavailable: install it or add it to your login-shell PATH")?;
        Ok(Self { executable })
    }

    fn executable(&self) -> &Path {
        &self.executable
    }

    fn command(&self, session: &str) -> Command {
        let mut command = Command::new(&self.executable);
        for key in HERDR_INHERITED_ENV {
            command.env_remove(key);
        }
        // `--session` is a global Herdr option. Herdr 0.8 rejects it after
        // workspace/tab/pane subcommands, so pin the dedicated session before
        // adding the operation. Never depend on ambient HERDR_ENV.
        command.arg("--session").arg(session);
        command
    }

    fn output(&self, session: &str, args: &[&str]) -> Result<String, String> {
        let mut command = self.command(session);
        let output = command
            .args(args)
            .output()
            .map_err(|error| format!("failed to execute Herdr: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "Herdr command failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn status_socket(&self, session: &str) -> Result<Option<PathBuf>, String> {
        let status = self.json_output(session, &["status", "--json"])?;
        if !status
            .pointer("/server/running")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(None);
        }
        let socket = status
            .pointer("/server/socket")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or("Herdr status JSON did not report an absolute socket path")?;
        Ok(Some(socket))
    }

    fn json_output(&self, session: &str, args: &[&str]) -> Result<serde_json::Value, String> {
        let output = self.output(session, args)?;
        serde_json::from_str(&output).map_err(|error| {
            format!(
                "Herdr returned invalid JSON for {}: {error}",
                args.join(" ")
            )
        })
    }
}

fn resolve_bundled_herdr() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    verified_executable(&executable.parent()?.join("herdr"))
}

fn resolve_herdr_from_process_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join("herdr"))
        .find_map(|candidate| verified_executable(&candidate))
}

fn verified_executable(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(canonical)
}

impl HerdrFleetBroker for HerdrCliBroker {
    fn ensure_or_adopt_session(&mut self, session: &str) -> Result<HerdrSessionLease, String> {
        validate_dedicated_session(session)?;
        if let Some(socket_path) = self.status_socket(session)? {
            return Ok(HerdrSessionLease {
                session: session.to_string(),
                socket_path,
                lease_id: format!("adopt:{}", session),
                started_by_buzz: false,
            });
        }

        let mut command = self.command(session);
        command
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start dedicated Herdr server: {error}"))?;
        // The server runs independently of this short-lived broker. Reap its
        // launcher child so a later `server stop` cannot leave a zombie owned
        // by the Desktop process.
        let _ = thread::Builder::new()
            .name("buzz-herdr-server-reaper".to_string())
            .spawn(move || {
                let _ = child.wait();
            });
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(100));
            if let Some(socket_path) = self.status_socket(session)? {
                return Ok(HerdrSessionLease {
                    session: session.to_string(),
                    socket_path,
                    lease_id: format!("buzz:{}:{}", session, uuid::Uuid::new_v4()),
                    started_by_buzz: true,
                });
            }
        }
        Err("dedicated Herdr server did not become ready".to_string())
    }

    fn ensure_workspace(
        &mut self,
        lease: &HerdrSessionLease,
        request: &HerdrWorkspaceRequest,
    ) -> Result<HerdrParentIdentity, String> {
        // This label is creation-time presentation only. We never query it:
        // once Herdr returns IDs, all lifecycle operations use those IDs.
        let label = format!(
            "firstmate-{}-{}",
            request.identity.managed_agent_id,
            &request.identity.home_sha256[..12]
        );
        let home = request
            .identity
            .canonical_home
            .to_string_lossy()
            .into_owned();
        let response = self.json_output(
            &lease.session,
            &[
                "workspace",
                "create",
                "--cwd",
                &home,
                "--label",
                &label,
                "--no-focus",
            ],
        )?;
        let workspace_id = json_required_string(&response, &["workspace_id"])?;
        let root_tab_id = json_required_string(&response, &["root_tab_id", "tab_id"])?;
        let root_pane_id = json_required_string(&response, &["root_pane_id", "pane_id"])?;
        Ok(HerdrParentIdentity {
            session: lease.session.clone(),
            socket_path: lease.socket_path.clone(),
            workspace_id,
            root_tab_id,
            root_pane_id,
        })
    }

    fn preflight(
        &mut self,
        parent: &HerdrParentIdentity,
        identity: &HerdrManagedAgentIdentity,
    ) -> Result<(), String> {
        parent.validate()?;
        let socket = self
            .status_socket(&parent.session)?
            .ok_or("dedicated Herdr session is not running")?;
        if socket != parent.socket_path {
            return Err("Herdr session socket differs from the persisted endpoint".to_string());
        }
        let workspace = self.json_output(&parent.session, &["workspace", "list"])?;
        let tab = self.json_output(&parent.session, &["tab", "get", &parent.root_tab_id])?;
        let pane = self.json_output(&parent.session, &["pane", "get", &parent.root_pane_id])?;
        if !json_contains_exact_string(&workspace, &["workspace_id", "id"], &parent.workspace_id) {
            return Err("Herdr response does not prove exact workspace_id or id".to_string());
        }
        json_has_exact_string(&tab, &["tab_id"], &parent.root_tab_id)?;
        json_has_exact_string(&tab, &["workspace_id"], &parent.workspace_id)?;
        json_has_exact_string(&pane, &["pane_id"], &parent.root_pane_id)?;
        json_has_exact_string(&pane, &["tab_id"], &parent.root_tab_id)?;
        json_has_exact_string(&pane, &["workspace_id"], &parent.workspace_id)?;
        let home = identity.canonical_home.to_string_lossy();
        json_has_exact_string(&pane, &["foreground_cwd"], &home)?;
        Ok(())
    }

    fn release_workspace(&mut self, parent: &HerdrParentIdentity) -> Result<(), String> {
        parent.validate()?;
        self.output(
            &parent.session,
            &["workspace", "close", &parent.workspace_id],
        )?;
        let workspaces = self.json_output(&parent.session, &["workspace", "list"])?;
        if json_contains_exact_string(&workspaces, &["workspace_id", "id"], &parent.workspace_id) {
            return Err("Herdr workspace remained after exact close".to_string());
        }
        Ok(())
    }

    fn stop_session(&mut self, lease: &HerdrSessionLease) -> Result<(), String> {
        if !lease.started_by_buzz {
            return Err("refusing to stop an adopted Herdr session".to_string());
        }
        self.output(&lease.session, &["server", "stop"])?;
        Ok(())
    }
}

fn json_required_string(value: &serde_json::Value, keys: &[&str]) -> Result<String, String> {
    find_json_string(value, keys)
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "Herdr response omitted required field {}",
                keys.join(" or ")
            )
        })
}

fn json_has_exact_string(
    value: &serde_json::Value,
    keys: &[&str],
    expected: &str,
) -> Result<(), String> {
    if find_json_string(value, keys).is_some_and(|actual| actual == expected) {
        Ok(())
    } else {
        Err(format!(
            "Herdr response does not prove exact {}",
            keys.join(" or ")
        ))
    }
}

fn json_contains_exact_string(value: &serde_json::Value, keys: &[&str], expected: &str) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            keys.iter().any(|key| {
                map.get(*key)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|actual| actual == expected)
            }) || map
                .values()
                .any(|child| json_contains_exact_string(child, keys, expected))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|child| json_contains_exact_string(child, keys, expected)),
        _ => false,
    }
}

fn find_json_string<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key).and_then(serde_json::Value::as_str) {
                    return Some(value);
                }
            }
            map.values().find_map(|value| find_json_string(value, keys))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_json_string(value, keys)),
        _ => None,
    }
}

fn fleet_state_path<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<PathBuf, String> {
    Ok(super::storage::managed_agents_base_dir(app)?.join("firstmate-herdr-fleet.json"))
}

/// Runtime spawn boundary: ensure, preflight, and persist before ACP starts.
/// The returned environment is the complete v1 adapter contract.
pub(crate) fn ensure_firstmate_herdr_endpoint<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    managed_agent_id: &str,
    home: &Path,
) -> Result<Vec<(&'static str, String)>, String> {
    let _guard = lock_fleet_state()?;
    let path = fleet_state_path(app)?;
    let mut manager = HerdrFleetManager::load_from_path(&path)?;
    let identity = HerdrManagedAgentIdentity::new(managed_agent_id, home)?;
    let mut broker = HerdrCliBroker::discover()?;
    let snapshot = manager.ensure_agent(&mut broker, identity)?;
    // Persist before spawn: if Desktop dies after the child starts, recovery
    // still has the exact owner endpoint rather than a label to rediscover.
    manager.save_to_path(&path)?;
    snapshot.parent.firstmate_env(home, broker.executable())
}

/// Runtime stop boundary. The state file is written only after a successful
/// release; an unsuccessful owned-server stop retains its endpoint and lease
/// for an explicit retry.
pub(crate) fn release_firstmate_herdr_endpoint<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    managed_agent_id: &str,
    home: &Path,
) -> Result<(), String> {
    let _guard = lock_fleet_state()?;
    let path = fleet_state_path(app)?;
    let mut manager = HerdrFleetManager::load_from_path(&path)?;
    let identity = HerdrManagedAgentIdentity::new(managed_agent_id, home)?;
    let mut broker = HerdrCliBroker::discover()?;
    manager.release_agent(&mut broker, &identity)?;
    manager.save_to_path(&path)
}

fn home_sha256(home: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(home.as_os_str().as_encoded_bytes());
    hex::encode(hasher.finalize())
}

fn validate_dedicated_session(session: &str) -> Result<(), String> {
    if session.trim().is_empty() {
        return Err("Herdr fleet state has no dedicated session".to_string());
    }
    if session.eq_ignore_ascii_case("default") {
        return Err("FirstMate managed agents must not adopt Herdr's default session".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, VecDeque};

    #[derive(Default)]
    struct FakeBroker {
        started_by_buzz: bool,
        preflight_error: Option<String>,
        transient_preflight_errors: VecDeque<String>,
        preflight_calls: usize,
        stopped: usize,
        workspaces: usize,
        closed_workspaces: Vec<String>,
        live_workspaces: BTreeSet<String>,
        close_error: Option<String>,
        owner_home_hash: Option<String>,
    }

    impl HerdrFleetBroker for FakeBroker {
        fn ensure_or_adopt_session(&mut self, session: &str) -> Result<HerdrSessionLease, String> {
            Ok(HerdrSessionLease {
                session: session.to_string(),
                socket_path: PathBuf::from("/tmp/buzz-firstmate-herdr.sock"),
                lease_id: "lease-1".to_string(),
                started_by_buzz: self.started_by_buzz,
            })
        }

        fn ensure_workspace(
            &mut self,
            lease: &HerdrSessionLease,
            request: &HerdrWorkspaceRequest,
        ) -> Result<HerdrParentIdentity, String> {
            self.workspaces += 1;
            let workspace_id = format!("ws-{}", request.identity.home_sha256);
            self.live_workspaces.insert(workspace_id.clone());
            Ok(HerdrParentIdentity {
                session: lease.session.clone(),
                socket_path: lease.socket_path.clone(),
                workspace_id,
                root_tab_id: format!("tab-{}", request.identity.managed_agent_id),
                root_pane_id: format!("pane-{}", request.identity.managed_agent_id),
            })
        }

        fn preflight(
            &mut self,
            _parent: &HerdrParentIdentity,
            identity: &HerdrManagedAgentIdentity,
        ) -> Result<(), String> {
            self.preflight_calls += 1;
            if self
                .owner_home_hash
                .as_ref()
                .is_some_and(|expected| expected != &identity.home_sha256)
            {
                return Err("root pane owner home does not match managed agent".to_string());
            }
            if let Some(error) = self.transient_preflight_errors.pop_front() {
                return Err(error);
            }
            self.preflight_error.clone().map_or(Ok(()), Err)
        }

        fn release_workspace(&mut self, parent: &HerdrParentIdentity) -> Result<(), String> {
            if let Some(error) = self.close_error.clone() {
                return Err(error);
            }
            self.closed_workspaces.push(parent.workspace_id.clone());
            self.live_workspaces.remove(&parent.workspace_id);
            Ok(())
        }

        fn stop_session(&mut self, _lease: &HerdrSessionLease) -> Result<(), String> {
            self.stopped += 1;
            Ok(())
        }
    }

    fn temp_home(name: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("buzz-herdr-fleet-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn two_homes_share_server_but_get_owner_exact_endpoints() {
        let atlas_home = temp_home("atlas");
        let nova_home = temp_home("nova");
        let atlas = HerdrManagedAgentIdentity::new("atlas", &atlas_home).unwrap();
        let nova = HerdrManagedAgentIdentity::new("nova", &nova_home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker::default();

        let atlas_snapshot = manager.ensure_agent(&mut broker, atlas).unwrap();
        let nova_snapshot = manager.ensure_agent(&mut broker, nova).unwrap();

        assert_eq!(atlas_snapshot.parent.session, DEFAULT_HERDR_SESSION);
        assert_eq!(
            atlas_snapshot.parent.socket_path,
            nova_snapshot.parent.socket_path
        );
        assert_ne!(
            atlas_snapshot.parent.workspace_id,
            nova_snapshot.parent.workspace_id
        );
        assert_ne!(
            atlas_snapshot.parent.root_pane_id,
            nova_snapshot.parent.root_pane_id
        );
        assert_eq!(broker.workspaces, 2);
        let _ = std::fs::remove_dir_all(atlas_home);
        let _ = std::fs::remove_dir_all(nova_home);
    }

    #[test]
    fn stale_snapshot_fails_closed_without_workspace_recovery_by_label() {
        let home = temp_home("stale");
        let identity = HerdrManagedAgentIdentity::new("atlas", &home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker::default();
        manager.ensure_agent(&mut broker, identity.clone()).unwrap();
        broker.preflight_error = Some("root pane does not exist".to_string());

        assert!(manager
            .ensure_agent(&mut broker, identity)
            .unwrap_err()
            .contains("stale Herdr endpoint"));
        assert_eq!(broker.workspaces, 1);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn fresh_preflight_failure_rolls_back_the_exact_new_workspace() {
        let home = temp_home("fresh-preflight-rollback");
        let identity = HerdrManagedAgentIdentity::new("atlas", &home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker {
            preflight_error: Some("root pane disappeared".to_string()),
            ..Default::default()
        };

        assert!(manager.ensure_agent(&mut broker, identity).is_err());
        assert_eq!(broker.preflight_calls, FRESH_ENDPOINT_PREFLIGHT_ATTEMPTS);
        assert_eq!(broker.closed_workspaces.len(), 1);
        assert!(broker.live_workspaces.is_empty());
        assert!(manager.state().agents.is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn fresh_preflight_retries_a_transient_shell_cwd_before_persisting() {
        let home = temp_home("fresh-preflight-retry");
        let identity = HerdrManagedAgentIdentity::new("nova", &home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker {
            transient_preflight_errors: VecDeque::from([
                "does not prove exact foreground_cwd".to_string()
            ]),
            ..Default::default()
        };

        let snapshot = manager.ensure_agent(&mut broker, identity).unwrap();
        assert_eq!(broker.preflight_calls, 2);
        assert!(broker.closed_workspaces.is_empty());
        assert!(broker
            .live_workspaces
            .contains(&snapshot.parent.workspace_id));
        assert!(manager
            .state()
            .agents
            .contains_key(&snapshot.identity.key()));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn workspace_membership_accepts_expected_id_after_another_workspace() {
        let workspaces = serde_json::json!({
            "result": {
                "workspaces": [
                    { "workspace_id": "w1" },
                    { "workspace_id": "w2" }
                ]
            }
        });
        assert!(json_contains_exact_string(
            &workspaces,
            &["workspace_id", "id"],
            "w2"
        ));
    }

    #[test]
    fn typed_tab_and_pane_ids_ignore_the_cli_envelope_id() {
        let tab = serde_json::json!({
            "id": "cli:tab:get",
            "result": { "tab": { "tab_id": "w2:t1", "workspace_id": "w2" } }
        });
        let pane = serde_json::json!({
            "id": "cli:pane:get",
            "result": { "pane": { "pane_id": "w2:p1", "tab_id": "w2:t1" } }
        });
        assert!(json_has_exact_string(&tab, &["tab_id"], "w2:t1").is_ok());
        assert!(json_has_exact_string(&pane, &["pane_id"], "w2:p1").is_ok());
    }

    #[test]
    fn releasing_one_agent_never_stops_shared_server() {
        let atlas_home = temp_home("release-atlas");
        let nova_home = temp_home("release-nova");
        let atlas = HerdrManagedAgentIdentity::new("atlas", &atlas_home).unwrap();
        let nova = HerdrManagedAgentIdentity::new("nova", &nova_home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker {
            started_by_buzz: true,
            ..Default::default()
        };
        let atlas_snapshot = manager.ensure_agent(&mut broker, atlas.clone()).unwrap();
        manager.ensure_agent(&mut broker, nova.clone()).unwrap();

        manager.release_agent(&mut broker, &atlas).unwrap();
        assert_eq!(broker.stopped, 0);
        assert_eq!(broker.closed_workspaces.len(), 1);
        assert_eq!(
            broker.closed_workspaces[0],
            atlas_snapshot.parent.workspace_id
        );
        assert!(manager.state().agents.contains_key(&nova.key()));
        assert!(!broker
            .live_workspaces
            .contains(&atlas_snapshot.parent.workspace_id));
        assert!(broker
            .live_workspaces
            .contains(&format!("ws-{}", nova.home_sha256)));
        manager.release_agent(&mut broker, &nova).unwrap();
        assert_eq!(broker.stopped, 1);
        let _ = std::fs::remove_dir_all(atlas_home);
        let _ = std::fs::remove_dir_all(nova_home);
    }

    #[test]
    fn close_failure_retains_owner_snapshot_for_retry() {
        let home = temp_home("close-failure");
        let identity = HerdrManagedAgentIdentity::new("atlas", &home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker::default();
        manager.ensure_agent(&mut broker, identity.clone()).unwrap();
        broker.close_error = Some("workspace close failed".to_string());

        assert!(manager.release_agent(&mut broker, &identity).is_err());
        assert!(manager.state().agents.contains_key(&identity.key()));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn restart_after_release_creates_one_replacement_anchor_not_a_leak() {
        let home = temp_home("restart-anchor");
        let identity = HerdrManagedAgentIdentity::new("atlas", &home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker::default();
        let first = manager.ensure_agent(&mut broker, identity.clone()).unwrap();
        manager.release_agent(&mut broker, &identity).unwrap();
        let second = manager.ensure_agent(&mut broker, identity).unwrap();

        assert_eq!(broker.closed_workspaces, vec![first.parent.workspace_id]);
        assert_eq!(broker.workspaces, 2);
        assert_eq!(broker.live_workspaces.len(), 1);
        assert_eq!(manager.state().agents.len(), 1);
        assert_eq!(
            second.parent.workspace_id,
            "ws-".to_string() + &second.identity.home_sha256
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn adopted_server_is_never_stopped_automatically() {
        let home = temp_home("adopted");
        let identity = HerdrManagedAgentIdentity::new("atlas", &home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker::default();
        manager.ensure_agent(&mut broker, identity.clone()).unwrap();
        manager.release_agent(&mut broker, &identity).unwrap();
        assert_eq!(broker.stopped, 0);
        assert_eq!(broker.closed_workspaces.len(), 1);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn broker_rejects_cross_home_anchor_even_when_endpoint_is_topologically_valid() {
        let atlas_home = temp_home("forged-atlas");
        let nova_home = temp_home("forged-nova");
        let atlas = HerdrManagedAgentIdentity::new("atlas", &atlas_home).unwrap();
        let nova = HerdrManagedAgentIdentity::new("nova", &nova_home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker {
            owner_home_hash: Some(nova.home_sha256.clone()),
            ..Default::default()
        };

        let error = manager.ensure_agent(&mut broker, atlas).unwrap_err();
        assert!(error.contains("owner home does not match"));
        assert!(manager.state().agents.is_empty());
        let _ = std::fs::remove_dir_all(atlas_home);
        let _ = std::fs::remove_dir_all(nova_home);
    }

    #[test]
    fn repeated_ensure_reuses_one_exact_endpoint() {
        let home = temp_home("repeat");
        let identity = HerdrManagedAgentIdentity::new("atlas", &home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker::default();
        manager.ensure_agent(&mut broker, identity.clone()).unwrap();
        manager.ensure_agent(&mut broker, identity).unwrap();
        assert_eq!(broker.workspaces, 1);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn rejects_default_session() {
        assert!(HerdrFleetManager::new("default").is_err());
    }

    #[test]
    fn executable_resolution_requires_a_real_executable_file() {
        let directory = std::env::temp_dir();
        assert!(verified_executable(&directory).is_none());
        let current = std::env::current_exe().unwrap();
        assert_eq!(
            verified_executable(&current),
            std::fs::canonicalize(current).ok()
        );
    }

    #[test]
    fn cli_places_global_session_before_the_operation() {
        let broker = HerdrCliBroker {
            executable: std::env::current_exe().unwrap(),
        };
        let mut command = broker.command(DEFAULT_HERDR_SESSION);
        command.args(["workspace", "list"]);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["--session", DEFAULT_HERDR_SESSION, "workspace", "list"]
        );
    }

    #[test]
    fn scrub_removes_only_firstmate_herdr_authority_from_child_command() {
        let mut command = Command::new("true");
        command
            .env("HERDR_ENV", "1")
            .env("HERDR_SOCKET_PATH", "/tmp/personal.sock")
            .env("BUZZ_FIRSTMATE_HERDR_SESSION", "forged")
            .env("UNRELATED_RUNTIME_ENV", "kept");
        HerdrFleetManager::scrub_inherited_herdr_env(&mut command);
        let env: BTreeMap<_, _> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(env.get("HERDR_ENV"), Some(&None));
        assert_eq!(env.get("HERDR_SOCKET_PATH"), Some(&None));
        assert_eq!(env.get("BUZZ_FIRSTMATE_HERDR_SESSION"), Some(&None));
        assert_eq!(
            env.get("UNRELATED_RUNTIME_ENV"),
            Some(&Some("kept".to_string()))
        );
    }

    #[test]
    fn persisted_endpoint_cannot_claim_another_session() {
        let home = temp_home("state-session");
        let identity = HerdrManagedAgentIdentity::new("atlas", &home).unwrap();
        let lease = HerdrSessionLease {
            session: DEFAULT_HERDR_SESSION.to_string(),
            socket_path: PathBuf::from("/tmp/herdr.sock"),
            lease_id: "lease-1".to_string(),
            started_by_buzz: false,
        };
        let snapshot = HerdrSpawnSnapshot {
            identity: identity.clone(),
            parent: HerdrParentIdentity {
                session: "other-session".to_string(),
                socket_path: lease.socket_path.clone(),
                workspace_id: "1".to_string(),
                root_tab_id: "1:1".to_string(),
                root_pane_id: "1-1".to_string(),
            },
            lease_id: lease.lease_id.clone(),
            server_started_by_buzz: false,
        };
        let state = HerdrFleetState {
            session: DEFAULT_HERDR_SESSION.to_string(),
            server_started_by_buzz: false,
            server_lease: Some(lease),
            agents: BTreeMap::from([(identity.key(), snapshot)]),
        };
        assert!(HerdrFleetManager::from_state(state).is_err());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn tmux_is_the_explicit_rollout_fallback() {
        assert_eq!(FirstMateMultiplexer::default(), FirstMateMultiplexer::Tmux);
        assert!(!FirstMateMultiplexer::Tmux.is_herdr());
        assert!(FirstMateMultiplexer::Herdr.is_herdr());
    }

    #[test]
    fn effective_persona_env_can_select_herdr_for_a_keyed_instance() {
        let effective_env = BTreeMap::from([(
            "BUZZ_FIRSTMATE_MULTIPLEXER".to_string(),
            "herdr".to_string(),
        )]);
        assert_eq!(
            FirstMateMultiplexer::from_agent_env(&effective_env),
            Ok(FirstMateMultiplexer::Herdr)
        );
    }

    #[test]
    fn persisted_state_retains_exact_endpoint_and_server_lease() {
        let home = temp_home("persist");
        let state_dir =
            std::env::temp_dir().join(format!("buzz-herdr-state-{}", std::process::id()));
        let state_path = state_dir.join("fleet.json");
        let identity = HerdrManagedAgentIdentity::new("atlas", &home).unwrap();
        let mut manager = HerdrFleetManager::new(DEFAULT_HERDR_SESSION).unwrap();
        let mut broker = FakeBroker {
            started_by_buzz: true,
            ..Default::default()
        };
        let snapshot = manager.ensure_agent(&mut broker, identity).unwrap();
        manager.save_to_path(&state_path).unwrap();
        let loaded = HerdrFleetManager::load_from_path(&state_path).unwrap();
        let restored = loaded.state().agents.values().next().unwrap();
        assert_eq!(restored.parent, snapshot.parent);
        assert_eq!(restored.lease_id, "lease-1");
        assert!(loaded.state().server_started_by_buzz);
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn env_contract_contains_only_explicit_parent_authority() {
        let parent = HerdrParentIdentity {
            session: DEFAULT_HERDR_SESSION.to_string(),
            socket_path: PathBuf::from("/tmp/herdr.sock"),
            workspace_id: "5".to_string(),
            root_tab_id: "5:1".to_string(),
            root_pane_id: "5-1".to_string(),
        };
        let home = temp_home("env");
        let executable = std::env::current_exe().unwrap();
        let env = parent.firstmate_env(&home, &executable).unwrap();
        assert_eq!(env.len(), 8);
        assert!(env.iter().any(|(key, _)| *key == "BUZZ_FIRSTMATE_HOME"));
        assert!(env
            .iter()
            .all(|(key, _)| key.starts_with("BUZZ_FIRSTMATE_")));
        assert!(env
            .iter()
            .any(|(key, value)| *key == "BUZZ_FIRSTMATE_HERDR_BINARY"
                && value
                    == &std::fs::canonicalize(&executable)
                        .unwrap()
                        .display()
                        .to_string()));
        assert!(env.iter().all(|(key, _)| *key != "HERDR_ENV"));
        let _ = std::fs::remove_dir_all(home);
    }
}
