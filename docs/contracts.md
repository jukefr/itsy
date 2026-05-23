# Contracts

Contracts are the multi-agent orchestration layer. A contract breaks a large task into a sequential queue of features, each executed by an isolated worker session. An orchestrator LLM owns the contract lifecycle; workers are ephemeral and know only their assigned feature.

---

## Concepts

| Term | Definition |
|------|-----------|
| **Contract** | The top-level work unit: a goal, a feature queue, and persisted state on disk. |
| **Orchestrator** | The session that created the contract. Plans features, calls `propose_contract` and `start_contract_run`, reviews handoffs, decides next steps. |
| **Worker** | A short-lived agent session spawned by the runner for one feature. Reads its feature description and a skill file, does the work, calls `end_feature_run`. |
| **Feature** | A single unit of work in `features.json`: one worker does one feature. Has a status (`pending` → `in_progress` → `completed`/`cancelled`). |
| **Milestone** | An optional tag on features grouping them for validation. When all implementation features for a milestone complete, the runner auto-injects validation features. |
| **Handoff** | The structured output a worker writes via `end_feature_run`: what was built, what was left undone, discovered issues, verification evidence. |
| **Runner** | The in-process controller (not a separate process) that loops: read state → pick next pending feature → spawn worker → await completion → repeat. |

---

## State Machine

### Contract states

```
initializing
    │
    ▼
running ──── (user pause / worker action) ──→ paused
    │                                            │
    │         (orchestrator review needed)       │
    ├──────────────────────────────────────────→ orchestrator_turn
    │
    ▼
completed
```

| State | Meaning |
|-------|---------|
| `initializing` | Artifacts are being authored; `start_contract_run` not yet called. |
| `running` | Runner is active, workers are being spawned. |
| `paused` | User interrupted, usage limit hit, or runner explicitly paused. |
| `orchestrator_turn` | Runner returned control: a worker's handoff has actionable items, or no pending features remain but contract is not complete. |
| `completed` | All features are `completed` or `cancelled`. |

### Feature statuses

```
pending → in_progress → completed
                     └→ cancelled
```

---

## Disk Layout

All contract state lives under a single directory per orchestrator session:

```
~/.local/share/<app>/sessions/<baseSessionId>/contracts/<baseSessionId>/
├── state.json               # Contract-level state (id, state, workingDirectory, timestamps)
├── features.json            # Ordered feature queue
├── progress_log.jsonl       # Append-only event log (worker lifecycle, pauses, milestones)
├── handoffs.jsonl           # Legacy single-file handoff log (deprecated; dir preferred)
├── handoffs/                # Per-worker handoff JSON files  (<workerSessionId>-<ts>.json)
├── mission.md               # Human-readable contract description / proposal markdown
├── architecture.md          # Architecture overview authored by orchestrator
├── validation-contract.md   # Assertions the implementation must satisfy
├── validation-state.json    # Assertion ID → status map ("pending" | "pass" | "fail")
├── AGENTS.md                # Guidance injected into every worker session
├── services.yaml            # Service definitions (commands, ports, boundaries)
├── init.sh                  # One-time environment setup script (optional)
├── library/                 # Shared reference material for workers
├── skills/                  # Skill files referenced by feature.skillName
│   └── <skillName>/
│       └── SKILL.md
└── model-settings.json      # Per-contract model overrides
```

### `state.json`

```jsonc
{
  "missionId": "mis_a1b2c3d4",     // stable ID, generated once at creation
  "state": "running",              // ContractState enum
  "workingDirectory": "/repo",     // workers spawn here
  "createdAt": "2025-01-01T00:00:00Z",
  "updatedAt": "2025-01-01T01:23:45Z"
}
```

### `features.json`

```jsonc
{
  "features": [
    {
      "id": "feat_001",
      "description": "Implement the user authentication flow",
      "skillName": "implement-feature",   // maps to skills/<skillName>/SKILL.md
      "milestone": "1",                   // optional grouping tag
      "preconditions": ["Database schema migrated"],
      "expectedBehavior": ["Login returns JWT", "Logout invalidates token"],
      "fulfills": ["auth.login", "auth.logout"],   // assertion IDs in validation-contract.md
      "status": "pending",                         // pending | in_progress | completed | cancelled
      "workerSessionIds": [],                      // all workers ever assigned this feature
      "currentWorkerSessionId": null,              // active worker, if any
      "completedWorkerSessionId": null             // worker that completed it
    }
  ]
}
```

Features are processed top-to-bottom. The runner always picks the first `pending` entry.

### `progress_log.jsonl`

One JSON object per line, append-only. Entry types:

| `type` | Fields | Meaning |
|--------|--------|---------|
| `worker_started` | `workerSessionId`, `spawnId`, `featureId`, `timestamp` | Worker spawned |
| `worker_selected_feature` | `workerSessionId`, `featureId`, `timestamp` | Feature assigned to worker |
| `worker_completed` | `workerSessionId`, `exitCode`, `timestamp` | Worker finished cleanly |
| `worker_failed` | `workerSessionId`, `exitCode`, `reason`, `timestamp` | Worker crashed or timed out |
| `worker_paused` | `workerSessionId`, `featureId`, `timestamp` | Worker interrupted mid-run |
| `contract_paused` | `timestamp` | Contract-level pause recorded |
| `milestone_validation_triggered` | `milestone`, `featureId`, `timestamp` | Validation features injected |

---

## Tools

### `propose_contract`

**Who calls it:** Orchestrator.  
**When:** After the orchestrator has analyzed the task and composed a plan.  
**Effect:** Presents the proposal to the user for review; creates the contract directory if accepted.  
**Requires user confirmation:** yes.

**Input:**
```typescript
{
  title: string;                  // Short contract title
  proposal: string;               // Markdown: plan overview, milestones, environment setup
  workingDirectory?: string;      // Workers CWD (defaults to current cwd)
}
```

**Output:**
```typescript
{
  accepted: boolean;
  missionDir?: string;            // Absolute path to contract directory (if accepted)
  isEdited?: boolean;             // User chose to manually edit before proceeding
  llmGuidance?: string;           // Next-step instructions for the orchestrator
}
```

If accepted, the orchestrator must author all required artifacts before calling `start_contract_run`.

---

### `start_contract_run`

**Who calls it:** Orchestrator.  
**When:** All required artifacts are in place.  
**Effect:** Blocking — returns only when a worker handoff needs orchestrator attention, the user pauses, or all features complete.

**Preconditions (runner validates these):**
- `validation-contract.md` and `validation-state.json` exist and are valid
- `features.json` exists with at least one feature
- `skills/<skillName>/SKILL.md` exists for every `skillName` used
- `AGENTS.md` exists
- `services.yaml` exists

**Input:**
```typescript
{
  message?: string;                  // Optional log message for this run
  resumeWorkerSessionId?: string;    // Resume a specific paused worker instead of spawning new
  restartFeature?: boolean;          // Discard paused worker, restart feature from scratch
}
```

**Output (on return):**
```typescript
{
  started: boolean;
  workerHandoffs?: WorkerHandoffSummary[];    // All handoffs since last run
  latestWorkerHandoff?: {
    featureId: string;
    resultState: "pass" | "fail";
    handoffFile: string;
    handoffJson: string;                      // Full handoff contents, inline
  };
  systemMessage?: string;                     // Instructions for the orchestrator
  pauseReason?: "unrecoverable_usage_402";
  completedFeatures?: { id: string; description?: string }[];
  totalFeatures?: number;
  workerCount?: number;
  startedAt?: string;
  progressSnapshot?: ContractProgressSnapshot;
}
```

---

### `end_feature_run` (worker-side)

**Who calls it:** Worker, at the end of its session.  
**Effect:** Records the handoff, marks the feature `completed` or triggers orchestrator return.

**Input:**
```typescript
{
  featureId: string;
  resultState: "pass" | "fail";
  returnToOrchestrator: boolean;    // true = runner pauses and hands back to orchestrator
  commitId?: string;                // Git commit SHA for the work
  repoPath?: string;
  validatorsPassed?: boolean;
  handoff: {
    salientSummary: string;         // ≤750 chars, ≤6 sentences
    whatWasImplemented: string;     // min 50 chars
    whatWasLeftUndone: string;      // empty string if fully complete
    verification: {
      commandsRun: { command, exitCode, observation }[];
      interactiveChecks?: { action, observed }[];
    };
    tests: {
      added: { file: string; cases: { name, verifies }[] }[];
      updated?: string[];
      coverage: string;
    };
    discoveredIssues: {
      severity: "blocking" | "non_blocking" | "suggestion";
      description: string;
      suggestedFix?: string;
    }[];
    skillFeedback?: {
      followedProcedure: boolean;
      deviations: { step, whatIDidInstead, why }[];
      suggestedChanges?: string[];
    };
  };
}
```

---

### `dismiss_handoff_items`

**Who calls it:** Orchestrator, after reviewing a handoff with actionable items.  
**Effect:** Records each item as explicitly handled, allowing `start_contract_run` to proceed.

**Input:**
```typescript
{
  dismissals: {
    type: "discovered_issue" | "critical_context" | "incomplete_work";
    sourceFeatureId: string;
    summary: string;
    justification: string;    // min 20 chars; must cite a tracking feature or explain why permanent
  }[];
}
```

---

## Execution Flow

### Initial setup (orchestrator)

```
1. Orchestrator receives large task
2. Calls propose_contract({ title, proposal, workingDirectory })
   └── User reviews and approves
3. Orchestrator authors artifacts in missionDir/:
   - architecture.md
   - validation-contract.md  (assertions, one ID per line)
   - validation-state.json   (all assertion IDs → "pending")
   - features.json           (ordered feature list)
   - AGENTS.md               (shared guidance for workers)
   - services.yaml
   - skills/<name>/SKILL.md  (one file per skillName used)
   - init.sh                 (if env setup needed)
4. Calls start_contract_run()   ← BLOCKS
```

### Runner loop (internal)

```
while running:
  read state.json
  if state == completed  → break
  if state == paused     → break
  if state == orchestrator_turn → break

  inProgressFeature = features where status == in_progress
  if inProgressFeature and resumeWorkerSessionId:
    resume that worker, await completion
  else:
    nextFeature = first feature where status == pending
    if none:
      inject milestone validation features if needed
      if still none and all completed → state = completed, break
      else → state = orchestrator_turn, break
    spawn worker for nextFeature
    await worker completion

  if worker.returnToOrchestrator → state = orchestrator_turn, break
  if worker.missionPaused        → break
  loop
```

### Worker session lifecycle

```
1. Spawned with system prompt injecting:
   - Worker session ID
   - Assigned feature JSON
   - missionDir file listing
   - agent-browser session naming rules (if applicable)

2. Worker's first user message instructs:
   a. Invoke startup skill (base-worker-procedures)
   b. Invoke feature skill (feature.skillName)
   c. Call end_feature_run when done

3. Worker executes, making code changes in workingDirectory

4. Worker calls end_feature_run({ featureId, resultState, handoff, ... })
   └── Feature marked completed/in_progress based on returnToOrchestrator

5. Worker session closes
```

### Orchestrator review cycle

When `start_contract_run` returns (non-final):

```
orchestrator receives:
  - latestWorkerHandoff (full JSON inline)
  - systemMessage (what to do next)

orchestrator decides:
  - If handoff has discoveredIssues or whatWasLeftUndone:
      → create new features OR update existing ones
      → call dismiss_handoff_items for each item addressed
      → call start_contract_run again to continue
  - If worker requested orchestrator (returnToOrchestrator=true):
      → review handoff, take action, call start_contract_run
  - If usage limit hit:
      → tell user to top up / change limits
      → call start_contract_run when resolved
```

---

## Milestone Validation

When all implementation features for a milestone complete, the runner auto-injects two validation features at the top of the queue:

1. **Scrutiny validation** (`scrutiny-validation-<milestone>`) — runs test suite, typecheck, lint; spawns review subagents per feature; synthesizes findings; always returns to orchestrator.

2. **User-testing validation** (`user-testing-validation-<milestone>`) — determines testable assertions from `fulfills` mappings; sets up environment; spawns flow validator subagents; updates `validation-state.json`.

Both can be skipped via `model-settings.json` (`skipScrutiny`, `skipUserTesting` flags).

---

## Model Settings (`model-settings.json`)

```jsonc
{
  "workerModel": "claude-opus-4-5",
  "workerReasoningEffort": "high",
  "validationWorkerModel": "claude-sonnet-4-5",
  "validationWorkerReasoningEffort": "medium",
  "orchestratorModel": "claude-opus-4-5",          // via global session settings
  "skipScrutiny": false,
  "skipUserTesting": false
}
```

Workers use `workerModel`; validation workers use `validationWorkerModel`. These can differ to optimize cost.

---

## Orphan Recovery

On `start_contract_run`, if the runner finds an `in_progress` feature with a tracked worker session that is no longer alive, it:

1. Attempts to close the orphaned session (best-effort)
2. Appends a `worker_failed` entry to `progress_log.jsonl` with `reason: "orphan_cleanup"`
3. Resets the feature back to `pending`
4. Proceeds with normal loop

---

## Pausing and Resuming

**Pause triggers:**
- User sends SIGINT
- `start_contract_run` call is interrupted via the session interrupt mechanism
- Worker hits an unrecoverable 402 (usage limit) from the LLM provider

**On pause:** current worker session is interrupted; `worker_paused` and `contract_paused` are appended to `progress_log.jsonl`; `state.json` is set to `paused`.

**Resuming a paused contract:**
- Call `start_contract_run()` — runner auto-resumes the paused worker if one is tracked
- Call `start_contract_run({ resumeWorkerSessionId })` — resume a specific worker explicitly
- Call `start_contract_run({ restartFeature: true })` — discard paused worker, start feature fresh

---

## Feature Reordering

The orchestrator can reorder features in `features.json` at any time between runs. The runner always picks the first `pending` entry. To preempt an in-progress feature:

1. Insert the urgent feature at the top of `features.json`
2. Call `start_contract_run()`
3. The runner resets the in-progress feature to `pending`, runs the inserted feature first, then re-runs the preempted feature later

---

## Artifact Authoring Guide

### `validation-contract.md`

Defines what the implementation must satisfy. Each assertion has a stable ID used in `features.json#fulfills`:

```markdown
## Authentication

- auth.login: POST /auth/login returns a signed JWT on valid credentials
- auth.logout: POST /auth/logout invalidates the session token
- auth.refresh: POST /auth/refresh extends a valid token without re-authentication

## Data Integrity

- data.writes-atomic: All writes to the orders table are wrapped in a transaction
```

### `validation-state.json`

Initialized with every assertion ID set to `"pending"`. Updated by validation workers:

```json
{
  "auth.login": "pending",
  "auth.logout": "pending",
  "auth.refresh": "pass",
  "data.writes-atomic": "fail"
}
```

### `AGENTS.md`

Injected into every worker session. Cover:
- Repo layout
- Build / test commands
- Coding conventions
- Off-limits areas
- How to run services locally

### `services.yaml`

Defines services the workers can start and their ports:

```yaml
services:
  api:
    command: npm run dev
    port: 3000
    readyPattern: "Server listening"
  db:
    command: docker compose up -d postgres
    port: 5432
```

### `skills/<name>/SKILL.md`

Step-by-step procedure for a worker assigned this skill. Workers must follow it exactly or document deviations in their handoff's `skillFeedback`. A default `base-worker-procedures` skill handles startup (read context files, initialize environment) and cleanup (close browser sessions, call `end_feature_run`).

---

## Integration Points

### Session manager

The runner calls into the session manager to:
- `spawnWorkerSession({ cwd, baseSessionId, modelId, interactionMode, autonomyLevel, ... })` — creates a new session with the worker system prompt
- `closeSession(sessionId)` — terminates a session
- `interruptSession(sessionId)` — sends interrupt signal to a running session
- `loadSession(sessionId)` — reattaches to an existing session for resumption

### Notifications

State changes and progress events are broadcast as project notifications:

| Notification type | Payload |
|-------------------|---------|
| `contract_state_changed` | `{ state, updatedAt }` |
| `contract_features_changed` | `{ features }` |
| `contract_progress_entry` | `{ progressLog }` |
| `contract_heartbeat` | `{ timestamp }` |
| `contract_worker_started` | `{ workerSessionId }` |
| `contract_worker_completed` | `{ workerSessionId, featureId, exitCode }` |

### Protocol messages

The daemon protocol exposes contract events as `session_notification` messages with these subtypes: `contract_accepted`, `contract_paused`, `contract_resumed`, `contract_run_started`, `worker_started`, `worker_completed`.

---

## Error Handling

| Scenario | Behavior |
|----------|---------|
| Worker crashes (non-zero exit) | `worker_failed` logged; feature reset to `pending`; runner returns to orchestrator |
| Worker spawn fails | `worker_failed` logged; feature reset to `pending`; runner returns to orchestrator |
| Unrecoverable 402 from LLM | Contract paused; `state = paused`; `pauseReason: "unrecoverable_usage_402"` returned to orchestrator |
| `state.json` missing | Runner stops immediately |
| Missing `missionId` in state | Runner stops; returns to orchestrator with error message |
| Orphaned in-progress worker | Cleaned up at next `start_contract_run`, feature reset to pending |
| Skill file missing | Worker calls `end_feature_run({ returnToOrchestrator: true })`; orchestrator must fix skills dir |
