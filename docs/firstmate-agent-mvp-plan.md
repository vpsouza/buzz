# FirstMate as a Native Buzz Agent — MVP Implementation Plan

Status: proposed
Target: private Buzz fork
Initial platform: macOS, local managed agents, Codex ACP
Upstream baseline reviewed: `block/buzz@820a858`

## 1. Decision

The MVP will make a FirstMate home appear in Buzz as a managed agent with its
own Buzz identity. Buzz owns identity, relay connectivity, presence, inbound
authorization, and process lifecycle. FirstMate keeps ownership of project
intake, backlog, worktree creation, crew dispatch, supervision, and delivery.

The MVP will extend `buzz-acp` and Buzz Desktop rather than build a second
message-polling sidecar. A FirstMate instance will initially use Codex ACP as
its inner harness, launched with its FirstMate home as the ACP working
directory and with one agent-scoped ACP session shared by all addressed Buzz
channels.

No FirstMate orchestration logic will be copied into Buzz.

## 2. MVP outcome

At the end of the MVP, a user can:

1. Create or select a FirstMate home on the local machine.
2. Create a Buzz managed agent backed by that home.
3. Give the agent its own Buzz/Nostr identity and add it to a Buzz team.
4. Mention or DM the FirstMate from Buzz.
5. Have one persistent FirstMate primary session receive messages from
   multiple Buzz origins without creating competing primary sessions.
6. Let the FirstMate spawn and supervise its normal crewmates and worktrees.
7. Receive acknowledgement, progress, and final delivery in the originating
   Buzz thread.
8. Allow two FirstMate identities, each with a different `FM_HOME`, to mention
   one another and delegate work using an explicit allowlist.
9. Restart a FirstMate managed agent and recover its durable FirstMate state.

## 3. Non-goals

The following are deliberately outside the MVP:

- Replacing FirstMate's runtime backends or worktree model.
- Making every crewmate visible as a Buzz managed agent.
- Remote FirstMate deployment through Kubernetes providers.
- Supporting every FirstMate primary harness. The MVP supports Codex ACP only.
- Changing the Buzz relay protocol or database schema.
- A fully typed inter-FirstMate delegation protocol. The MVP uses normal Buzz
  mentions plus stable task metadata in message content.
- Automatic project routing across a fleet of FirstMates.
- Upstream-ready generalization, branding, or migration tooling.

## 4. Architectural invariants

These are release blockers, not implementation preferences.

### 4.1 One home, one primary authority

Exactly one mutable FirstMate primary session may own a given `FM_HOME`.
Messages from different Buzz channels must not create independent ACP sessions
against the same home.

### 4.2 Separate FirstMates require separate homes and identities

Every FirstMate managed agent has:

- one canonical, absolute, non-symlink `FM_HOME`;
- one Buzz keypair and pubkey;
- one local managed-agent process;
- one agent-scoped ACP session;
- one FirstMate session lock;
- zero or more crewmates managed by FirstMate, not by Buzz Desktop.

The same home cannot be attached to two live managed-agent records.

### 4.3 Buzz does not become the orchestrator

Buzz may start, stop, observe, and message the FirstMate primary. It must not
create FirstMate task worktrees, edit FirstMate backlog state, or control
crewmates directly.

### 4.4 FirstMate startup remains authoritative

The primary must run `bin/fm-session-start.sh` exactly once for each new ACP
session and must acquire the normal FirstMate lock before any fleet mutation.
Buzz must not reproduce bootstrap, lock, recovery, or wake-drain logic.

### 4.5 Existing agents remain unchanged

The default managed-agent behavior stays channel-scoped and continues to run
from Buzz's default work directory. All new persisted fields must deserialize
with backward-compatible defaults.

### 4.6 Agent-to-agent access is explicit

FirstMates use Buzz's `allowlist` inbound gate. The owner remains implicitly
authorized. Other FirstMates must be explicitly listed by pubkey.

## 5. Target runtime flow

```text
Buzz Desktop
  -> starts buzz-acp in canonical FM_HOME
  -> buzz-acp starts codex-acp
  -> buzz-acp creates one agent-scoped ACP session
  -> Codex loads FM_HOME/AGENTS.md
  -> FirstMate session-start acquires the home lock

Buzz relay event addressed to FirstMate
  -> owner/allowlist gate
  -> per-agent FIFO queue
  -> origin envelope added to prompt
  -> shared FirstMate ACP session
  -> FirstMate dispatches normal crew/worktrees
  -> FirstMate replies through buzz-cli to the origin thread
```

## 6. Persisted model changes

Add two local instance fields to `ManagedAgentRecord`. They must not be
published as portable persona/team definition defaults because filesystem
paths and mutable-home authority are machine-local.

```rust
pub enum SessionScope {
    Channel,
    Agent,
}

pub struct ManagedAgentRecord {
    // existing fields...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub session_scope: SessionScope,
}
```

Defaults:

- `working_directory = None`: retain `default_agent_workdir()`.
- `session_scope = Channel`: retain current Buzz behavior.

For a FirstMate instance:

- `working_directory = canonical FM_HOME`;
- `session_scope = Agent`;
- `parallelism = 1`;
- `runtime = codex` for the MVP;
- `respond_to = owner-only` or `allowlist`;
- `BUZZ_FIRSTMATE_HOME` is injected with the same canonical path.

The spawn-config snapshot must include both fields so a change produces the
existing restart-required/auto-restart behavior.

## 7. Workstreams

### Workstream A — Safe per-agent working directory

Buzz Desktop changes:

1. Add `working_directory` to the managed-agent record, create/update command
   payloads, storage migration, frontend types, and spawn-config snapshot.
2. Resolve the effective directory before any spawn side effect.
3. Require an absolute, existing, real directory.
4. Refuse symlinks and canonicalize once at save/start time.
5. Set `Command::current_dir()` to the validated directory; retain
   `default_agent_workdir()` when absent.
6. Add a live-home registry keyed by canonical path. Refuse a second live
   agent-scoped managed process for the same path.
7. Scrub `BUZZ_FIRSTMATE_HOME` from arbitrary user environment overrides and
   set it only from trusted persisted configuration.

UI changes:

- Add a local “Working directory” field to managed-agent advanced settings.
- Display the canonical path and validation errors.
- Do not publish the path in shared agent definitions, teams, relay events, or
  exported persona packs.

### Workstream B — Agent-scoped ACP session mode

`buzz-acp` changes:

1. Add `SessionScope::{Channel, Agent}` to configuration.
2. Add `BUZZ_ACP_SESSION_SCOPE=channel|agent`, defaulting to `channel`.
3. In agent scope, use one logical session key for all public channels and DMs.
4. Preserve the original channel, event, author, thread root, and reply target
   in every prompt envelope.
5. Serialize turns through one FIFO queue even if events arrive concurrently.
6. Force pool size to one or fail startup when agent scope is combined with
   `BUZZ_ACP_AGENTS > 1`.
7. Define control semantics:
   - `!cancel` cancels the single in-flight FirstMate turn;
   - `!rotate` rotates the shared session globally;
   - `!shutdown` requests the managed primary lifecycle path.
8. Preserve current per-channel deduplication and replay cursors so restart
   does not re-execute acknowledged events.

No relay changes are expected.

### Workstream C — FirstMate managed-agent mode

Add a local FirstMate mode to agent creation/editing. This is an instance
configuration, not a portable ACP runtime definition.

Creation flow:

1. User selects “FirstMate” as agent mode.
2. User selects a FirstMate home.
3. Desktop validates that it contains `AGENTS.md`, `bin/`, and the effective
   state directory expected by FirstMate.
4. Desktop selects Codex ACP and checks its adapter availability.
5. Desktop fixes `session_scope=agent` and `parallelism=1`.
6. Desktop creates the ordinary Buzz identity/auth tag.
7. Desktop starts the process in the selected home.

Runtime environment:

```text
BUZZ_FIRSTMATE_HOME=<canonical path>
BUZZ_ACP_SESSION_SCOPE=agent
BUZZ_ACP_AGENTS=1
BUZZ_ACP_MULTIPLE_EVENT_HANDLING=steer
BUZZ_ACP_DEDUP=queue
```

The effective system prompt should add only Buzz-specific operating context:

- this session is the Buzz-facing primary for the named FirstMate identity;
- run and trust FirstMate's normal session-start contract;
- use the bundled `buzz-cli` skill for Buzz communication;
- preserve origin/thread metadata when replying;
- do not let Buzz instructions override FirstMate lock or authority rules.

It must not restate FirstMate's full `AGENTS.md`.

### Workstream D — Startup and lifecycle integration

Startup acceptance requires proof that the session is operational, not only
that `buzz-acp` connected to the relay.

1. Verify experimentally whether Codex ACP runs the tracked FirstMate
   SessionStart and Stop hooks from the selected home.
2. If supported, use the existing hooks unchanged.
3. If SessionStart is not supported, add a minimal ACP bootstrap injection that
   asks the primary to run `bin/fm-session-start.sh` before handling the first
   user event. The script remains the sole bootstrap owner.
4. If Stop hooks are not supported, map Buzz's existing MCP `_Stop` hook
   surface to `bin/fm-turnend-guard.sh` and treat failure to establish
   supervision as unhealthy.
5. Mark presence as ready only after ACP initialization and successful first
   session creation. Surface lock refusal prominently and keep the session
   read-only.

Stopping:

1. Add a FirstMate pre-stop command owned by the FirstMate repository, for
   example `bin/fm-buzz-lifecycle.sh prepare-stop`.
2. It reports one of: `safe`, `supervision-transferred`, or `refused`.
3. Desktop refuses ordinary Stop on `refused` and shows the reason.
4. A separately labeled force-stop remains a deliberate destructive action.
5. Killing the Buzz primary must not claim that crewmates were cancelled.

The MVP may ship without force-stop UI if the existing process kill path cannot
be made sufficiently explicit in the first iteration.

### Workstream E — Buzz communication skill for FirstMate

Add a FirstMate-owned skill and thin scripts that use `buzz-cli`; do not add
agent-facing operations to `buzz-dev-mcp`.

Required operations:

- reply to origin thread;
- mention another agent by pubkey/name;
- fetch the relevant thread;
- publish progress/status;
- publish a final result or diff;
- resolve and validate a managed teammate identity.

Suggested durable link in FirstMate task metadata:

```text
buzz_relay=<relay URL>
buzz_channel=<channel UUID>
buzz_event=<origin event ID>
buzz_thread=<thread root ID>
buzz_author=<author pubkey>
buzz_trace=<stable task trace ID>
```

The FirstMate task ID remains the execution authority. Buzz IDs are delivery
coordinates.

### Workstream F — Two-FirstMate team demonstration

Create two independent fixture homes and identities:

```text
Atlas -> FM_HOME_A -> mobile/project portfolio
Nova  -> FM_HOME_B -> backend/project portfolio
```

Configure each with the owner's pubkey plus the other FirstMate in the inbound
allowlist. Demonstrate:

1. Owner asks Atlas for a cross-project change.
2. Atlas accepts and creates its local task.
3. Atlas mentions Nova with a stable parent/trace ID.
4. Nova accepts, creates its own local task, and responds in the thread.
5. Nova's crew produces a result.
6. Nova reports completion to Atlas.
7. Atlas integrates and gives the owner the final response.

For the MVP, delegation messages use a documented Markdown envelope:

```text
[firstmate-task-offer]
trace: <uuid>
parent: <atlas-task-id>
requested-by: <atlas-pubkey>
project: <project-key>
delivery: report|branch|pr

<human-readable request>
```

Receivers must deduplicate on `trace + requested-by`. A typed relay event can
be designed after the workflow is proven.

## 8. Proposed PR sequence

### PR 1 — Managed-agent working directory

Scope:

- backend record/storage/API changes;
- trusted path validation;
- spawn `current_dir` behavior;
- spawn snapshot/restart drift;
- frontend advanced field;
- unit tests and backward-compatibility fixtures.

Exit criterion: a normal Codex managed agent can be launched in an explicitly
selected repository, while existing agents behave byte-for-byte as before.

### PR 2 — Agent-scoped sessions in `buzz-acp`

Scope:

- config/env parsing;
- shared session key;
- single FIFO across channel origins;
- global rotate/cancel semantics;
- fake-ACP integration tests.

Exit criterion: messages from two channels produce one `session/new`, ordered
prompts, and replies with correct origin metadata.

### PR 3 — FirstMate mode and startup health

Scope:

- FirstMate home validator;
- instance mode and UI;
- enforced Codex/agent-scope/single-worker configuration;
- trusted environment;
- startup/lock health reporting;
- focused FirstMate fixtures.

Exit criterion: a FirstMate home starts from Buzz, acquires its lock, and
receives one real Buzz request without a competing session.

### PR 4 — FirstMate Buzz communication and lifecycle

Scope split across repositories:

- FirstMate Buzz skill/scripts and durable task link;
- reply/progress/final delivery;
- pre-stop handshake;
- restart recovery test.

Exit criterion: a spawned crewmate completes work and the primary posts the
result back to the origin thread after at least one managed-agent restart.

### PR 5 — Team of FirstMates E2E

Scope:

- allowlist setup UX/docs;
- delegation envelope;
- two-home fixture/demo;
- operator runbook and known limitations.

Exit criterion: Atlas delegates to Nova, both use their own homes and crews,
and the owner receives a final integrated result with a traceable thread.

## 9. Test plan

### Unit tests

- Old managed-agent JSON loads with `working_directory=None` and
  `session_scope=Channel`.
- Relative, missing, file, and symlink work directories are refused.
- Canonically identical homes cannot start twice.
- Spawn snapshot changes when workdir or session scope changes.
- User env cannot override trusted `BUZZ_FIRSTMATE_HOME`.
- Agent scope rejects pool sizes greater than one.
- Agent scope maps multiple channel IDs to one ACP session.
- Rotate invalidates the shared session.
- Deduplication remains origin-aware.

### Integration tests with fake ACP

- Two channel events create one session and two ordered prompts.
- A DM and public mention share the session without losing origin metadata.
- Cancellation affects only the active turn; queued events remain queued.
- Restart resumes from relay cursors without duplicating completed prompts.
- Lock-refused startup is surfaced and never marked writable/healthy.

### FirstMate fixture tests

- Session start runs exactly once.
- The selected home owns the session lock.
- A task spawn writes only to that home's state.
- Two different homes can run concurrently.
- One home cannot be bound to two live identities.
- Stop with active unsupervised work is refused.
- Restart recovers task metadata and queued wakes.

### Manual credentialed E2E

- Real local relay and Buzz Desktop.
- Real Codex ACP runtime.
- One FirstMate completing a small repository task.
- Two FirstMates delegating across a Buzz thread.
- Desktop restart during an active crew task.
- Network disconnect/reconnect and replay.

### Repository gates

For every Buzz PR:

```sh
. ./bin/activate-hermit
just fix-all
just ci
```

Run `just test` as well if relay, DB, or auth code becomes necessary. Commits
must use `git commit -s`; production Rust must add no `unsafe`, `unwrap()`, or
`expect()` paths, and new public APIs require doc comments.

## 10. Observability

Add structured log fields to the existing managed-agent and ACP logs:

```text
agent_pubkey
agent_name
agent_mode=firstmate
fm_home_hash
session_scope=agent
acp_session_id
origin_channel
origin_event
fm_lock_state
fm_task_id
buzz_trace
```

Never log the private key, auth tag, full filesystem home, prompt bodies, or
captain memory. Display the full local home path only in the trusted local UI.

Expose a compact health summary:

```text
transport: connected|disconnected
primary: starting|ready|read-only|stopping|failed
lock: owned|refused|unknown
work: idle|active|supervised|attention-required
```

## 11. Security boundaries

- Preserve Buzz's NIP-OA identity/auth handling and OS keyring storage.
- Default FirstMate inbound access to `owner-only`.
- Require explicit pubkey allowlisting for agent-to-agent work.
- Treat messages from allowed agents as task requests, not unrestricted shell
  authority.
- Preserve FirstMate's existing escalation rules for destructive or
  security-sensitive work.
- Do not allow team/persona imports to choose a local filesystem path.
- Do not publish `FM_HOME` or captain memory to the relay.
- Do not equate a Buzz process kill with confirmed FirstMate crew cancellation.
- Fail closed when home validation, session lock, or stop postconditions are
  ambiguous.

## 12. Go/no-go gates

### Gate 1 — Codex ACP compatibility

Go only if a Codex ACP session launched in `FM_HOME` loads the expected
FirstMate instructions and can execute the normal session-start contract.
If project Stop hooks do not fire, the fallback MCP `_Stop` integration must be
proved before PR 3 ships.

### Gate 2 — Single-session correctness

Go only if two simultaneous channel events cannot create competing sessions or
overlapping prompts against one home.

### Gate 3 — Restart recovery

Go only if a managed-agent restart preserves FirstMate task state and does not
replay a completed Buzz event as new work.

### Gate 4 — Safe lifecycle

Go only if ordinary stop either transfers supervision or refuses with an
actionable reason. A force kill must be explicit and must not claim crew
termination.

### Gate 5 — Two-home isolation

Go only if the two-FirstMate E2E proves separate locks, state directories,
task IDs, credentials, and crews.

## 13. First implementation spike

Before PR 1, run one disposable spike without committing product code:

1. Start a local relay and Buzz Desktop.
2. Start `buzz-acp` manually with its process CWD set to a disposable FirstMate
   clone and `BUZZ_ACP_AGENT_COMMAND=codex-acp`.
3. Send one owner mention.
4. Record whether Codex loads `AGENTS.md`, whether SessionStart/Stop hooks fire,
   and whether `fm-session-start.sh` acquires the home lock.
5. Spawn one harmless scout task and verify a completion wake reaches the
   primary.
6. Restart `buzz-acp` and verify recovery plus relay deduplication.
7. Send messages from two channels and capture the current per-channel session
   behavior as the regression baseline for PR 2.

The spike decides only whether lifecycle needs an MCP-hook fallback. It does
not change the architecture or relax the invariants above.

## 14. Definition of done

The MVP is done when all five PR exit criteria and all five go/no-go gates pass,
the full Buzz `just ci` gate is green, the FirstMate repository's relevant
supervision/control tests are green, and the two-FirstMate credentialed demo is
recorded in a reproducible operator runbook.
