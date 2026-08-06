---
doc: scheduler
project: ruse
title: "ruse Background Scheduler (C-SCHEDULER)"
summary: >
  The single central scheduler that owns ALL background work (INV-SCHED-1). Defines Task metadata,
  coalescing + supersede-cancellation for only-latest-matters work, per-service/per-plugin CPU/memory/IO
  budgets (mechanism decided, numbers open — D-018), backpressure with bounded lanes, fairness
  (starvation + priority-inversion detection, cross-workspace + per-plugin quota), load-based feature
  degradation, and how off-main-thread jobs defer results to the single-threaded deterministic executor
  (D-002) without ever mutating editor state. Numbers here are TUNABLE placeholders per D-018.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../architecture/architecture.md            # §8 async/executor, §9 performance
  - ../architecture/design-requirements.md      # §5 SCHED, §17 PERFS
  - stability-and-observability.md              # supervisors §9, cancellation, status §11
  - editing-language.md                         # house style; =/! filter tasks (D-025)
  - ../invariants/reference-invariants.md       # INV-SCHED-1, INV-ASYNC-ORDER
  - ../../spec/DECISIONS.md                      # D-018, D-002, D-001, D-019, D-034
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Background Scheduler (C-SCHEDULER)

> One editor, one scheduler. Neovim's slow-with-many-plugins failure is not any single plugin — it is that
> nobody owns the *sum* of background work. `C-SCHEDULER` is the one component that sees every parse, index,
> git refresh, search, LSP request, prefetch, and plugin job, and can therefore starve none of them, drop
> the redundant ones, and never let them outrank the cursor. This doc specifies the *mechanism*; the budget
> **numbers** are deliberately open (D-018) and marked TUNABLE.

## Problem

Background work in an editor is unbounded by nature: every keystroke can trigger a reparse, every save a
re-index, every focus change a git refresh, every plugin its own ambitions. If each producer schedules its
own threads and races the main loop, three things break: (1) interactive latency collapses under load;
(2) results race the document they were computed against and get applied stale; (3) resource use is the
uncontrolled *sum* of every plugin's appetite. Neovim shows the end state — "many plugins ⇒ slow" with no
single owner to blame or throttle.

`C-SCHEDULER` is the single owner of all background work (INV-SCHED-1, D-018). It must guarantee input and
render always win, collapse only-latest-matters duplicates, bound every queue, share fairly across
workspaces and plugins, degrade feature quality (not availability) under load, and hand results back to the
single-threaded deterministic executor (D-002) without any worker ever touching editor state.

## Goals

- **Total ownership.** No background work exists outside the scheduler — no per-plugin thread pools, no ad
  hoc `thread::spawn`, no nested async runtimes (INV-SCHED-1; design-requirements §5).
- **Input/render supremacy — structural, not best-effort.** User input and screen refresh are handled on the
  main loop and always outrank any background task.
- **Coalesce + supersede.** Duplicate per-document parse/index (and git-status, screen-refresh) requests
  collapse to the latest; superseded work is *cancelled*, not merely discarded (design-requirements §5).
- **Budgets.** Per-service and per-plugin CPU / memory / IO (and remote bandwidth) budgets, enforced by a
  mechanism whose numbers are tunable (D-018 open).
- **Fairness.** Detect starvation and priority inversion; share across workspaces; a per-plugin quota so one
  plugin cannot monopolize the pool.
- **Graceful degradation under load** (`full semantic index → current file → visible range`, INV-CAP-DEGRADE).
- **Deterministic hand-off.** Jobs are async producers that DEFER results to the main loop; stale results are
  dropped (INV-ASYNC-ORDER).

## Non-goals

- Not the async/event *contract* itself (that is architecture §8 / INV-ASYNC-ORDER) — this doc is the
  resource-control engine that lives under it.
- Not the transaction/undo model (D-001/INV-TXN) — the scheduler produces *requests* that flow through the
  normal transaction pipeline; it never writes documents.
- Not the supervisor/restart model for crashed services (stability §9) — the scheduler *drives* supervised
  services but restart policy lives there.
- **Not the final budget numbers.** Per-task priority/deadline/cost/budget specifics are open (D-018); this
  doc fixes the *shape* of the knobs and their enforcement, not their values.
- Not remote transport (D-031) — only the bandwidth-budget hook into it.

## Terminology

Glossary terms (Document, View, Workspace, Revision, Snapshot, Plugin) per
[spec/glossary.yaml](../../spec/glossary.yaml). New local terms:

- **Task** — one unit of background work with metadata; the scheduler's scheduling atom.
- **Job** — the actual computation a Task runs on a worker (`BackgroundJob`).
- **Owner** — the principal a Task's resource use is charged to: a core `ServiceId` or a `PluginId`.
- **Lane** — a bounded producer→scheduler channel with a fixed overflow policy (backpressure unit).
- **Class** — coarse urgency band: `Interactive` / `Background` / `IdleOnly`.
- **Completion** — a finished Job's result, queued for the main loop to drain and (if fresh) apply.
- **Degrade tier** — the current feature-quality level of a service under load.

## Invariants

Depends on (defined in [reference-invariants.md](../invariants/reference-invariants.md)):
**INV-SCHED-1** (central ownership; input/render outrank; coalesce + supersede),
**INV-ASYNC-ORDER** (deterministic executor; async results carry id+revision; stale dropped),
**INV-QUERY-SNAPSHOT** (workers read immutable snapshots, never live core objects),
**INV-CAP-DEGRADE** (degrade, don't disappear), **INV-FAIL-BOUNDED** (bound blast radius),
**INV-PLUGIN-NO-CORE** / **INV-PLUGIN-ISOLATED** (no direct core mutation; failures isolated),
**INV-TXN** / **INV-ORIGIN** (results apply as transactions with an origin), **INV-STATUS** (per-component
status), **INV-REMOTE-FIRST** (bandwidth is a first-class remote budget). This doc mints no new `INV-*`
(registry rule, D-021/D-022).

## Proposed design

### 1. Where the scheduler sits

```
 terminal reader thread ──key/paste──▶ [input lane: NeverDrop] ─┐
 file watcher / timers ─────────────────────────────────────────┤
                                                                 ▼
                                                    ┌────────────────────────┐
                                                    │  MAIN LOOP (1 thread)  │   D-002 deterministic
                                                    │  = the editor state    │   executor
                                                    │  owner. No lock.       │
                                                    └────────────────────────┘
                                submit(Task) ▲            │ drain completions ▲
                                             │            ▼ dispatch          │
                                     ┌───────────────────────────────────────────────┐
                                     │             C-SCHEDULER (main-loop side)        │
                                     │  admission · coalesce map · budgets · fairness  │
                                     │  degrade controller · lanes · completion queue  │
                                     └───────────────────────────────────────────────┘
                                                          │ hand Job + snapshot + cancel
                                                          ▼
                                     ┌───────────────────────────────────────────────┐
                                     │   SHARED bounded worker pool  (≈ N_cores-1)     │  no per-plugin pool
                                     │   + owned external procs (LSP/git) + WASM insts │  metered, not free
                                     └───────────────────────────────────────────────┘
```

The scheduler's *bookkeeping* (admission, coalesce map, budget accounting, fairness state, degrade tiers)
runs **on the main loop thread** — it is plain owned state, no `Arc<Mutex<_>>` (INV-NO-GLOBAL-STATE). Only
Job *execution* happens on worker threads. The worker pool is a single shared, bounded pool; plugins and
services do not own pools (INV-SCHED-1; architecture §9 "no duplicate per-plugin processes / unbounded
spawn").

### 2. Task metadata

```rust
/// Process-local, monotonic. Never persisted, never crosses a protocol (ID scoping, design-reqs §7).
pub struct TaskId(u64);

pub struct Task {
    pub id:            TaskId,
    pub key:           TaskKey,          // coalescing identity (§4)
    pub class:         Class,            // Interactive | Background | IdleOnly
    pub priority:      Priority,         // ordinal within class; lower = more urgent (§6)
    pub deadline:      Option<Deadline>, // monotonic Instant from the INJECTED clock (D-002/DET)
    pub cost:          CostEstimate,     // untrusted hint; actuals are metered (§5, Security)
    pub cancel:        CancelToken,      // cooperative + external hooks (§8)
    pub workspace:     WorkspaceId,      // fairness bucket (§7)
    pub document:      Option<DocumentId>,
    pub base_revision: Option<Revision>, // the revision the Job reads; stale-drop key (§9)
    pub owner:         Owner,            // budget + quota bucket (§5, §7)
}

pub enum Owner { Core(ServiceId), Plugin(PluginId) }

pub enum Class {
    Interactive, // a user is waiting: completion, hover, current-file on-type parse, search-as-you-type
    Background,  // proactive but user isn't blocked: workspace index, git refresh, prefetch
    IdleOnly,    // only when the loop is idle: full semantic index, cache prewarm, GC/compaction
}

pub struct CostEstimate { pub cpu: Millis, pub mem_peak: Bytes, pub io: IoClass }
pub enum   IoClass { None, Disk(Bytes), Net(Bytes) }   // Net = remote bandwidth (INV-REMOTE-FIRST)

pub struct Deadline(Instant);   // from Clock service; never `Instant::now()` in core (DET)
pub struct Priority(u8);        // per-class ordinal; aging (§7) computes an *effective* priority
```

Input and render carry **no** `Task` — they are not background work. They are drained by the main loop
*before* any background dispatch (§6), which is how INV-SCHED-1's "input + render always outrank background"
is made structural rather than a priority number that could be mis-tuned.

### 3. Jobs never touch editor state

```rust
pub trait BackgroundJob: Send + 'static {
    type Output: Send + 'static;
    /// Runs on a worker thread. MUST NOT reference editor state — only the snapshot in `cx`.
    /// MUST call `cx.cancel.checkpoint()?` at the documented granularity (§8).
    fn run(self, cx: &JobCx) -> Result<Self::Output, JobError>;
}

pub struct JobCx {
    pub cancel:   CancelToken,
    pub snapshot: SnapshotHandle,  // immutable, taken at submit (INV-QUERY-SNAPSHOT); carries base_revision
    pub meter:    BudgetMeter,     // charge CPU/IO as you go; also the ReDoS-style step budget (D-028)
}
```

A Job reads an immutable document/workspace **snapshot** (INV-QUERY-SNAPSHOT; architecture §3.3 "background
parsers read a snapshot"). It cannot see a `Rope`, `View`, slotmap, or undo node (INV-PLUGIN-NO-CORE). Its
`Output` is a *value* (a parse tree, an index delta, a diagnostic set, a **transaction request**), never a
mutation. Completion hand-off (§9) is the only way that value re-enters editor state, and only through the
normal transaction/command path with an `origin` (INV-TXN/INV-ORIGIN).

Submission binds a main-loop continuation so the caller stays declarative:

```rust
// Runs on the MAIN LOOP when the completion is drained (§9). `ctx` is the editor state.
pub type OnComplete<T> = Box<dyn FnOnce(Result<T, JobError>, &mut MainCtx) + 'static>;

impl Scheduler {
    pub fn submit<J: BackgroundJob>(&mut self, task: Task, job: J, on_complete: OnComplete<J::Output>)
        -> Admission;   // Admitted(TaskId) | Coalesced(TaskId) | Rejected(Backpressure) (§4, §10)
}
```

### 4. Coalescing and supersede-cancellation (only-latest-matters)

```rust
pub struct TaskKey { pub kind: TaskKind, pub scope: TaskScope }
pub enum TaskScope { Document(DocumentId), Workspace(WorkspaceId), Global }
pub enum TaskKind { Parse, SemanticIndex, GitStatus, Search, Diagnostics, Hover, Completion,
                    Format, Prefetch, /* … non-exhaustive, owned per service */ }

pub enum CoalescePolicy {
    LatestWins,               // parse, index, git-status, screen-refresh: only the newest matters
    Debounce { window: Millis }, // collapse a watcher/keystroke storm into one submit
    Distinct,                 // every submission matters (save, run-tests, external filter =/!)
}
```

The scheduler keeps `live: HashMap<TaskKey, TaskId>` — the currently in-flight/queued task per key. On
`submit` of a `LatestWins` key:

```
if let Some(prev) = live.get(&key):
    if prev is Queued  -> replace it in place (new meta/job/continuation), keep or improve queue position
    if prev is Running -> prev.cancel.cancel(Superseded); enqueue new; live[key] = new.id
else: enqueue new; live[key] = new.id
```

This is where "cancel superseded, don't just discard" is enforced: a superseded Running task's
`CancelToken` is tripped **immediately**, so a worker mid-parse stops burning CPU at its next checkpoint
(§8) rather than finishing a result nobody will read. `Debounce` folds a burst (watcher events, on-type
edits) into a single trailing submit; the classic "unbounded parse requests for one document" anti-pattern
(architecture §8) cannot occur because the coalesce map holds at most one live task per `(kind, document)`.

### 5. Budgets (mechanism decided; numbers TUNABLE — D-018)

Every Task is charged to its `Owner`. Each owner has a `Budget`; the scheduler enforces it with **token
buckets** (rate) and **admission gates** (level):

```rust
pub struct Budget {
    pub cpu_ms_per_sec:   Millis, // token bucket: CPU-time granted per wall second
    pub mem_peak_bytes:   Bytes,  // ceiling on the owner's summed in-flight peak estimate
    pub io_bytes_per_sec: Bytes,  // disk IO token bucket
    pub bandwidth_bps:    Option<Bytes>, // remote only (INV-REMOTE-FIRST); charged on IoClass::Net
    pub max_concurrent:   u16,    // in-flight cap for this owner
}
// NUMBERS ARE PLACEHOLDERS. e.g. a "background" plugin might start at cpu=150ms/s, mem=64MiB,
// max_concurrent=2 — all TUNABLE, resolved by D-018 re-eval on F-011/F-014 real workloads.
```

Enforcement, on each dispatch decision (§6):

1. **CPU/IO rate** — a Task is eligible only if its owner's CPU (and IO, if `IoClass` non-`None`) bucket has
   tokens. The running Job's `meter.charge_cpu()` drains the bucket as it runs; when empty, that owner's
   remaining tasks stay `Queued` until refill. This throttles a hot owner without blocking others.
2. **Memory** — admit a Task only if `owner.in_flight_mem + task.cost.mem_peak ≤ budget.mem_peak_bytes`.
   Actual peak is sampled by the meter; sustained overshoot marks the owner `Degraded{Overbudget}` and
   throttles new admissions.
3. **Concurrency** — never exceed `max_concurrent` per owner and never exceed the global pool size.
4. **Bandwidth** — `IoClass::Net(bytes)` is charged against the remote `bandwidth_bps` bucket (design-reqs
   §5 "bandwidth budget in remote environments").

Cost estimates are **untrusted hints** (a plugin may lie); admission uses them for *ordering*, but the token
buckets meter **actuals**, so a lying estimate can never exceed the enforced ceiling (see Security).

### 6. The main-loop cycle: how input/render win, structurally

```
loop {
  1. DRAIN INPUT      pop all input-lane events (NeverDrop). key → command → transaction.   ── highest
  2. DRAIN COMPLETIONS pop a *bounded batch* of background completions; apply fresh, drop stale (§9).
  3. RENDER           if dirty, lower ONE coalesced frame (screen-refresh is a single pending-frame flag,
                      never a queue — a burst collapses to one paint).                       ── 2nd
  4. DISPATCH BG      refill budget buckets; admit Background tasks up to budgets + pool room; hand Jobs
                      to workers. Interactive-class tasks are admitted here too but ordered first.
  5. IDLE WORK        iff step 1 saw no input AND nothing is dirty AND no Interactive task is pending:
                      admit IdleOnly tasks; run degrade-recovery (§11).
  6. PARK             compute next wake from timers/deadlines; block on the input lane or the wake.
}
```

Because steps 1–3 always precede step 4, and because workers live on *separate* threads bounded to
`N_cores-1` (leaving the main-loop thread a core), background CPU can never preempt input handling or a
frame. `IdleOnly` work only exists in step 5 — the "idle-time vs interactive work" split is a cycle
position, not a fragile priority number.

**Ordering / fairness within a dispatch (step 4).** Eligible tasks (budget available, snapshot still fresh,
idle-gate satisfied) are ordered by:

```
(1) class            Interactive  <  Background  <  IdleOnly
(2) deadline         Earliest-Deadline-First among tasks that carry a Deadline
(3) effective prio   Priority after aging boost (§7)
(4) fairness vtime   Deficit Round Robin across (workspace → owner) as tie-break
```

### 7. Fairness: starvation, priority inversion, quotas

- **Cross-workspace + per-plugin fairness (DRR/WFQ).** Fairness state is a two-level Deficit Round Robin:
  first across `WorkspaceId`, then across `Owner` within a workspace. Each owner accrues a deficit
  proportional to its weight and spends it when its tasks run. One busy workspace or one greedy plugin
  therefore gets its *share*, not the whole pool. A **per-plugin quota** is just that plugin's DRR weight ×
  its `Budget.max_concurrent`.

- **Starvation detection (aging).** Each `Queued` task tracks `waited = now - enqueued_at`. If `waited`
  exceeds a per-class threshold (TUNABLE), its **effective priority** is boosted (aging) and a
  `sched.starvation` event is emitted with the owner/kind. Aging is bounded so a starved Background task
  eventually runs, but can never leapfrog Interactive class.

- **Priority-inversion detection.** A shared *single-flight resource* (one LSP process, one git worktree
  lock) is modeled as a serialized lane per `ServiceId`. When an Interactive task arrives for a service
  currently running a lower-priority Background task on that resource, the scheduler resolves the inversion
  by the resource's declared policy: **supersede** (if the background task is `LatestWins` — cancel it, run
  the interactive one) or **priority-inheritance** (let the background task finish but temporarily raise its
  effective priority so it can't itself be preempted by other background work). Either way a
  `sched.priority_inversion` event records it. This is the concrete answer to design-requirements §5
  "detect starvation and priority inversion."

### 8. Cancellation that actually stops computing

"A cancelled task must stop computing, not just discard its result." A `CancelToken` therefore has three
enforcement paths matched to the three kinds of worker:

```rust
pub struct CancelToken(Arc<CancelState>);          // cheap clone; shared with the running Job
impl CancelToken {
    pub fn is_cancelled(&self) -> bool;
    pub fn cancel(&self, reason: CancelReason);
    pub fn checkpoint(&self) -> Result<(), Cancelled>;  // call in hot loops; also decrements step budget
}
pub enum CancelReason { Superseded, DeadlineMissed, Shutdown, BudgetRevoked, Degraded, RevisionStale, UserAbort }
```

1. **In-process Rust jobs** (parse, index, search, regex) — cooperative. The job body calls
   `cx.cancel.checkpoint()?` at a documented granularity (per node / per line / per N match steps — the same
   step budget that bounds ReDoS, D-028). A job that never checks is a bug caught by the cancel-grace watchdog
   (below).
2. **External processes** (LSP, git, formatters, `=`/`!` filter operators from D-025) — cancel closes stdin /
   sends the cancel request / on grace-timeout `SIGTERM` then `SIGKILL`. The process's CPU is metered and its
   slot freed.
3. **WASM plugin instances** (post-MVP, D-004/D-009) — `cancel` bumps the wasmtime **epoch** (fuel
   interruption), so a spinning guest is unwound at the next epoch boundary rather than trusted to poll.

**Cancel-grace watchdog.** After `cancel`, a task has a grace deadline (TUNABLE) to stop. If it doesn't
(ignored checkpoint / runaway process / stuck guest), the scheduler escalates: kill the process / unwind the
guest, mark the owner *misbehaving*, and hand it to the supervisor (stability §9) which applies
backoff/disable. The editor is never blocked waiting for a wedged job (INV-FAIL-BOUNDED).

### 9. Completion hand-off to the deterministic executor (INV-ASYNC-ORDER)

Workers do not call back into editor state. A finished Job pushes a `Completion` onto a bounded MPSC queue
that the main loop drains in step 2:

```rust
pub struct Completion {
    pub task_id:  TaskId,
    pub seq:      CompletionSeq,      // monotonic ordering sequence assigned at enqueue (DET/design-reqs §4)
    pub base_revision: Option<Revision>,
    pub result:   Result<Erased, JobError>,   // + the OnComplete continuation
}
```

Drain rule, on the main loop:

```
for c in completions.drain(batch_limit):     // batch_limit keeps step 2 bounded so input stays responsive
    if let Some(rev) = c.base_revision:
        if rev != document(c).revision():    // superseded by a newer edit
            drop c (stale); metric sched.completion.stale += 1; continue    // INV-ASYNC-ORDER
    run c.on_complete(result, &mut ctx)       // applies as a Transaction with an origin (INV-TXN/INV-ORIGIN)
```

Completions are drained in `seq` order (deterministic replay, D-002/DET; architecture §3.4 "stale results
are not applied"). This is the *only* place a background result influences editor state, and it is entirely
on the single-threaded executor — so no worker ever races the document (the Neovim re-entrancy trap,
architecture §8).

### 10. Backpressure: bounded lanes with fixed overflow policies

Every producer feeds the scheduler through a **lane** with a bounded capacity and a declared overflow
policy. There is no unbounded queue anywhere (design-requirements §5; architecture §8).

| Lane | Producer | Bound | Overflow policy | Rationale |
|---|---|---|---|---|
| **Input** | terminal reader | large ring | **NeverDrop** (back-pressures the tty, drops nothing; a paste is one bulk event) | key input is never lost |
| **Screen refresh** | render requests | 1 (a dirty flag) | **CoalesceLatest** | a burst collapses to one paint |
| **Parse / Index** | edits, watcher | 1 per `(kind,document)` | **CoalesceLatest** (LatestWins, §4) | only newest revision matters |
| **Git status** | fs events, focus | 1 per workspace | **CoalesceLatest** | latest-only |
| **Search / Completion** | typing | small | **CoalesceLatest** (query is the key) | only current query matters |
| **Logs / telemetry sink** | everything | bounded ring | **DropByLevel** (drop trace→debug→info first; keep warn/error/fatal) | never drop a failure record |
| **Generic background** | services/plugins | per-owner bounded | **RejectNewest → `Backpressure` typed error** | caller learns it's overloaded |

`RejectNewest` returns `Admission::Rejected(Backpressure)` (a typed error, INV-ERR-CLASS) so the submitter
degrades instead of blocking. Key input and failure logs are the two things that structurally cannot be
dropped; everything only-latest-matters is coalesced; everything else back-pressures with a signal.

### 11. Load-based degradation (INV-CAP-DEGRADE)

A per-service degrade tier is driven by a load controller, not by the service itself:

```rust
pub enum IndexTier { FullWorkspace, CurrentFile, VisibleRange }  // design-reqs §5 ladder

pub struct LoadController {
    tier: BTreeMap<ServiceId, IndexTier>,
    // load signals (all main-loop-observable):
    interactive_p95_ms: Millis,   // rolling p95 of Interactive completion latency
    budget_saturation:  f32,      // fraction of owners at their CPU ceiling
    queue_depth:        usize,
}
```

When load is high (interactive p95 over budget — D-019 — or sustained budget saturation), the controller
**steps each service's tier down** `FullWorkspace → CurrentFile → VisibleRange`, cancelling the now-excess
tasks (e.g. a full-workspace index task is `cancel(Degraded)`-ed; the service re-submits at `CurrentFile`).
When the loop goes idle (step 5) and signals recover, it steps tiers **back up**, with **hysteresis**
(separate up/down thresholds + a dwell time) so it doesn't flap. The feature never *disappears* — semantic
navigation still works on the visible range; it just covers less until headroom returns (INV-CAP-DEGRADE;
stability §7).

### 12. No per-plugin pools / no unbounded spawn — enforced, not requested

- Plugins and services obtain background execution **only** via `Scheduler::submit`. There is no
  `thread::spawn` in core or in a plugin host, and the WASM/process host (D-004) exposes no thread/timer
  primitive that bypasses the scheduler.
- A **dependency that spawns its own runtime or thread pool** (e.g. a crate that pulls in a nested Tokio or
  starts a rayon pool) oversubscribes cores and re-introduces nondeterminism — it *clashes* with the single
  executor. Policy (D-034): such a crate is either **wrapped behind an Adapter and driven on our shared
  pool**, or **run as an external process** with its own `Owner` budget. Nested async runtimes in core are
  forbidden; if a dep needs async IO it is driven on the one shared runtime. The dependency-gate CI (D-034
  cost types include *threads* and *native*) flags any dep that spawns, so this is caught before merge.

## Failure modes

- **Task ignores cancellation / keeps computing.** Mitigated by cooperative checkpoints with a step budget
  (§8); the cancel-grace watchdog then force-terminates (kill / epoch-unwind) and marks the owner
  misbehaving. A wedged job never blocks the editor (INV-FAIL-BOUNDED).
- **Dep spawns its own runtime/thread pool.** Adapter-or-external-process policy + CI thread-count gate
  (§12, D-034). Forbidden in core.
- **Worker panics.** Caught at the worker boundary (stability §8, panic policy); the Task becomes
  `Failed(code)`, the supervisor records + backs off/restarts (stability §9); never crosses to the main loop;
  editor keeps running (INV-PLUGIN-ISOLATED).
- **Result arrives stale** (document advanced past `base_revision`). Dropped deterministically at drain
  (§9) — designed behavior, counted as `sched.completion.stale`, not an error.
- **Overload.** Budgets saturate → degrade tier down (§11), not crash. Interactive latency stays protected
  because input/render precede dispatch (§6).
- **Deadline missed.** Task `cancel(DeadlineMissed)`; caller gets a typed timeout; degrade counter++.
- **Lane overflow.** `RejectNewest` → `Backpressure` typed error (§10); input and failure logs are exempt by
  policy.
- **Priority inversion / starvation.** Detected and resolved (supersede / inheritance / aging, §7), each with
  a log event.
- **Coalesce-map leak** (a key whose task vanished without clearing `live`). Guarded by an assert on
  completion/cancel that the key maps to the finishing id; a mismatch is an invariant violation (assert, not
  error — stability §1).

## Recovery behavior

- **Load subsides** → degrade tiers climb back with hysteresis (§11).
- **Repeatedly misbehaving owner** → supervisor disables it (stability §9); its budget/quota is reclaimed and
  redistributed to the remaining owners' DRR weights.
- **Shutdown** → `cancel(Shutdown)` broadcast; workers stop at checkpoints, external procs `SIGTERM`→`SIGKILL`
  on grace; the editor exits without waiting on background work (architecture §3.4 "background work does not
  block editor shutdown").
- **Crash** → scheduler state is process-local and fully reconstructable; nothing to persist or recover.
  After restart, services re-derive their tasks from current document/workspace state.

## Security impact

- **Resource-DoS containment.** Per-plugin CPU/mem/IO/bandwidth budgets + concurrency caps bound a buggy or
  malicious plugin's footprint (INV-PLUGIN-ISOLATED, INV-TRUST-1); it can degrade its *own* feature, not the
  editor.
- **Untrusted cost estimates.** Estimates only *order* work; token buckets meter actuals, so a lying estimate
  cannot exceed the enforced ceiling (§5).
- **No live core objects to workers.** Jobs get immutable snapshots and return values; a plugin job cannot
  reach into editor state (INV-PLUGIN-NO-CORE / INV-QUERY-SNAPSHOT).
- **Remote bandwidth budget** prevents a plugin from saturating the SSH link and starving interactive remote
  round-trips (INV-REMOTE-FIRST, D-031).

## Performance impact

- **Interactive latency is the protected metric** (design-requirements §17; D-019 p95/p99 budgets): input and
  render precede all dispatch (§6), workers leave the main-loop thread a core, and `IdleOnly` work only runs
  when nothing interactive is pending.
- **Coalescing** removes the O(edits) redundant reparses/reindexes (architecture §8/§9 "apply only the
  latest").
- **Bounded shared pool** avoids the thread explosion of per-plugin pools (architecture §9); DRR ordering is
  O(log n) per dispatch and admission is O(1) amortized.
- **Snapshot reads** keep Jobs off the main loop entirely — no lock, no blocking (INV-QUERY-SNAPSHOT).
- Benchmarks must run "with a **real plugin set**, not an empty editor" (design-requirements §17): interactive
  p95 under a saturating background index + git + LSP load is the headline gate.

## Compatibility impact

- The scheduler is an **Internal** API in the MVP (D-009); plugins reach it only through the host `submit`
  surface, never by spawning. Budget *numbers* are open (D-018) and can change without an API break —
  they're runtime config, not contract.
- Evolution is additive (INV-ADDITIVE): new `Class`/`TaskKind`/budget fields are added, not repurposed. The
  ordering rule (class → EDF → aged priority → fairness) is the stable contract; weights/thresholds are tunable.

## Observability

Task lifecycle **is** the metric/status source (design-requirements §5 "task states as metrics/status"):

```
Submitted ─▶ Queued ─▶ Running ─▶ Completed
               │  └──▶ Coalesced(into: TaskId)          Running ─▶ Failed(ErrorCode)
               └──▶ Rejected(Backpressure)              Running ─▶ Superseded ─▶ Cancelled(reason)
      Queued/Running ─▶ Cancelled(reason)
```

- **Metrics** (per `kind × owner × workspace`): queued/running gauges; wait-time and run-time p50/p95;
  counters for cancelled / superseded / stale-dropped / rejected / deadline-missed; per-owner budget
  saturation %; current degrade tier.
- **Status** (INV-STATUS): each service exposes `ServiceStatus` — `Ready` /
  `Degraded{reason: Overloaded|Overbudget}` / `Recovering` — into the Health Registry the status bar
  subscribes to (stability §11.4). Health carries **freshness** (`Index: CurrentFile, 300ms ago`).
- **Structured log events** (once, at the scheduler boundary — INV-ERR-CLASS): `sched.task.coalesced`,
  `sched.task.superseded`, `sched.starvation`, `sched.priority_inversion`, `sched.degrade`,
  `sched.completion.stale`, `sched.backpressure` — each with `owner`, `workspace_id`, `kind`, `task_id`,
  `waited_ms`, `run_ms`, `reason`.
- **Debug surface** (stability §14): `:debug scheduler` shows the live queue by class/owner, per-owner budget
  buckets, current degrade tiers, and the coalesce map — so "why is indexing slow?" is answered by
  inspection, not guesswork.

## Test strategy

- **Determinism/replay** (D-002/DET): with the injected clock and a seeded fairness/DRR state, task dispatch
  order and completion drain order are reproducible; a recorded event log replays to the same scheduling
  trace.
- **Property tests** (against invariants): coalescing keeps exactly the latest per key; every superseded task
  is `Cancelled` before its supersessor completes; a stale completion is never applied (INV-ASYNC-ORDER); no
  Job produces effects after `checkpoint()` returns `Cancelled` (cancel actually stops work).
- **Fault injection** (design-requirements §18): a job that ignores cancellation ⇒ watchdog escalates within
  grace; a dep that spawns a runtime ⇒ CI gate fails; overload ⇒ degrade ladder steps down then recovers with
  hysteresis; plugin panic ⇒ isolated + supervised.
- **Fairness/starvation**: N workspaces × M plugins saturating the pool ⇒ each owner's throughput within its
  weighted share; every Queued task's wait time bounded by the aging threshold.
- **Latency gate** (D-019): interactive p95/p99 under a real background load stays within budget on the fixed
  CI machine.

## Migration strategy

- Land the mechanism now with **conservative fixed budgets** and a **single-worker fallback** flag; no MVP
  feature exercises the scheduler heavily (D-018 — first real users are post-MVP F-011 index / F-014 git /
  F-015). Ship a **kill-switch** that disables all `IdleOnly` work and pins every service to `CurrentFile`.
- Tune budgets and fairness weights against F-011/F-014 real workloads (D-018 re-evaluation), then promote the
  numbers from placeholders to defaults. The API shape (Task metadata, lanes, ordering rule) is stable across
  this; only the constants move.

## Alternatives

- **Per-service / per-plugin thread pools with a global semaphore.** Simpler to write, but no single owner
  sees the *sum*; fairness and coalescing become per-pool and can't be reasoned about globally. Rejected —
  violates INV-SCHED-1.
- **A general async runtime (Tokio) as the scheduler.** Gives spawning and IO for free, but its work-stealing
  scheduler is not deterministic, offers no per-owner CPU/mem budget or coalescing, and re-introduces the
  re-entrancy race the deterministic executor exists to avoid (architecture §8). We instead run one bounded
  pool + a deterministic main loop, and drive any async-IO dep on a single shared runtime.
- **Priority as a single global number (no classes).** One mis-tuned number could let background work outrank
  input. Rejected in favor of the structural class ordering + main-loop cycle position (§6), so input/render
  supremacy cannot be tuned away.

## Rejected approaches

- **Discard-on-supersede without cancel.** Lets a superseded parse run to completion and waste a core.
  Rejected: supersede *cancels* (§4/§8) — design-requirements §5 is explicit that the result-discard is not
  enough.
- **Workers mutate editor state under a lock.** Compiles, wrecks determinism and invites stale-result races
  (architecture §8, the Neovim trap). Rejected: workers read snapshots and defer via the completion queue
  (§9) — INV-ASYNC-ORDER / INV-QUERY-SNAPSHOT.
- **Unbounded queues with "we'll debounce later".** The exact accumulation failure INV-SCHED-1 forbids.
  Rejected: every lane is bounded with a declared overflow policy (§10).
- **Locking the budget numbers now.** Would stabilize unvalidated constants against D-018/D-010/APIX.
  Rejected: mechanism decided, numbers TUNABLE until real load exists.

## Trade-offs

- A central scheduler is more machinery than "just spawn a thread" and puts one component on the critical
  path of all background work. Accepted: it is the only place that can enforce input supremacy, coalescing,
  budgets, fairness, and deterministic hand-off *together* — the whole point of D-018/INV-SCHED-1
  (Architecture > Code).
- Cooperative cancellation asks every Job to poll checkpoints. Accepted: it's the only *safe* way to stop
  in-process CPU (you cannot safely kill a Rust thread mid-computation); the watchdog covers the ones that
  don't cooperate.
- Snapshots cost memory and a copy-on-write discipline. Accepted: it is the price of keeping workers off the
  main loop and results race-free.

## Open questions

- **Budget/priority/deadline numbers** (D-018): per-class CPU/mem/IO/bandwidth defaults; aging thresholds;
  degrade up/down hysteresis and dwell — all pending F-011/F-014 workloads.
- **Cost-estimate calibration**: whether estimates are learned (rolling actuals per `kind`) or declared, and
  how much they may bias ordering before actuals correct them.
- **Fairness granularity**: DRR across `(workspace, owner)` vs `(workspace, owner, kind)` — does a plugin's
  interactive vs background work need separate shares?
- **Interactive deadline source**: what sets the `Deadline` for an LSP completion/hover round-trip
  (frame-budget-derived vs fixed)?
- **`=`/`!` external-filter operators** (D-025 open): they run as `Distinct` external-process tasks — confirm
  their budget owner (the editing core vs a filter service) and their interaction with the single-transaction
  rule.
- **WASM epoch granularity** (post-MVP, D-004): epoch tick interval vs cancel latency vs overhead.

## Reference Invariants

This doc depends on (defined in [reference-invariants.md](../invariants/reference-invariants.md); not
redefined here):

- **INV-SCHED-1** — all background work is centrally owned; input/render outrank background; duplicate
  per-document parse/index coalesced and superseded requests cancelled. (§1, §4, §6) — budget *specifics*
  open per D-018.
- **INV-ASYNC-ORDER** — single-threaded deterministic executor; async results carry id + revision; stale
  results dropped. (§6, §9)
- **INV-QUERY-SNAPSHOT** — workers read immutable snapshots, never live core objects. (§3)
- **INV-CAP-DEGRADE** — features degrade under load, they don't disappear. (§11)
- **INV-FAIL-BOUNDED** — a wedged/failed task is bounded; the editor is never blocked. (§8, Failure modes)
- **INV-PLUGIN-NO-CORE** / **INV-PLUGIN-ISOLATED** — no direct core mutation; worker/plugin failures isolated.
  (§3, §8, Failure modes)
- **INV-TXN** / **INV-ORIGIN** — background results re-enter state only as transactions with an origin. (§9)
- **INV-STATUS** — per-service scheduler status feeds the Health Registry. (Observability)
- **INV-REMOTE-FIRST** — remote bandwidth is a first-class budget. (§5)
- **INV-NO-GLOBAL-STATE** / **INV-ERR-CLASS** — scheduler bookkeeping is owned main-loop state, no global
  mutex; backpressure/overflow surface as typed errors. (§1, §10)

Related decisions: **D-018** (scheduler owns all background work; budgets open), **D-002** (deterministic
executor), **D-001** (transaction-only mutation), **D-019** (latency budgets gate CI), **D-034**
(dependency policy: no rogue runtimes), **D-028** (step-budget cancellation), **D-025** (`=`/`!` filter tasks).
