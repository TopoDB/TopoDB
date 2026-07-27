use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::recovery::{contract_preserved, NoopRepairer, Repairer, Rung};
use crate::runner::cancel::CancelToken;
use crate::runner::command::{CommandRequest, CommandRunner};
use crate::runner::{AgentRunner, NodeOutcome, NodeRequest};
use crate::schema::validate::Validated;
use crate::schema::NodeKind;
use crate::store::run::{NodeState, RunStore};
use crate::store::SghError;

#[derive(Debug, Default, PartialEq)]
pub struct RunReport {
    pub succeeded: Vec<String>,
    pub blocked: Vec<String>,
    pub skipped: Vec<String>,
    /// Why each *failed* blocked node blocked, keyed by node id: the last
    /// error it produced before the recovery ladder gave up. A node that
    /// blocked for a real failure has an entry; a `gate` blocks on purpose
    /// (an intentional checkpoint, not a failure) and has none, which is how
    /// a caller tells the two apart without re-deriving it. Without this, a
    /// blocked id said only *that* a node stopped, never *why* — a tool-denied
    /// node looked identical to any other block.
    pub blocked_reasons: BTreeMap<String, String>,
    pub model_calls: u64,
    /// Shell command executions. Counted separately from `model_calls`
    /// because `Bound` budgets them as a distinct dimension.
    pub command_runs: u64,
}

/// Advances every node of a `Validated` graph through the state machine,
/// sequentially, in topological order. The executor never invents
/// structure: it only ever consults the graph and store it was given, and
/// a node is only ever handed the outputs of its own declared dependencies
/// (see `execute_node`'s `inputs` assembly). A failed node climbs the
/// recovery ladder (retry, then repair, then block) in `execute_node`;
/// REPLAN (regenerating graph structure) is out of scope — the ladder
/// stops at `Blocked`.
/// Everything `execute_node` needs, shareable across worker threads. Holds
/// the executor's state behind interior mutability (atomics for counters
/// and the clock, a mutex for the reasons map) so a future concurrent
/// executor can share one `Shared` across threads without each node needing
/// `&mut`.
struct Shared<'r> {
    store: RunStore,
    graph: Validated,
    runner: &'r dyn AgentRunner,
    repairer: &'r dyn Repairer,
    command_runner: Option<&'r dyn CommandRunner>,
    clock: AtomicI64,
    /// When set, `tick` clamps the logical clock forward to wall time
    /// instead of just incrementing it. See `ClockFn` and `with_clock`.
    wall_clock: Option<ClockFn>,
    model_calls: AtomicU64,
    command_runs: AtomicU64,
    /// Why each failed node blocked, captured at the block point where the
    /// error is still in hand. Read into `RunReport` by `report()`. Gate
    /// blocks add no entry — they are checkpoints, not failures.
    blocked_reasons: Mutex<BTreeMap<String, String>>,
    cancel: Option<CancelToken>,
    max_inflight: usize,
    /// Set when a worker returns `Err(SghError)` in parallel mode and no
    /// `CancelToken` was supplied. Checked alongside `cancel` before every
    /// claim so an internal engine failure stops the scheduler from
    /// claiming further work even without cooperative cancellation wired in.
    aborted: AtomicBool,
}

/// A wall-clock source, injected via `Executor::with_clock`. Returns epoch
/// milliseconds (or any caller-chosen unit, as long as it's non-decreasing
/// enough for the monotone clamp in `tick` to be meaningful).
pub type ClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct Executor<'r> {
    shared: Shared<'r>,
}

impl<'r> Executor<'r> {
    pub fn new(store: RunStore, graph: Validated, runner: &'r dyn AgentRunner) -> Self {
        Executor {
            shared: Shared {
                store,
                graph,
                runner,
                repairer: &NoopRepairer,
                command_runner: None,
                clock: AtomicI64::new(0),
                wall_clock: None,
                model_calls: AtomicU64::new(0),
                command_runs: AtomicU64::new(0),
                blocked_reasons: Mutex::new(BTreeMap::new()),
                cancel: None,
                max_inflight: 1,
                aborted: AtomicBool::new(false),
            },
        }
    }

    /// Wires in a model-backed (or hand-written stub) repairer for the
    /// REPAIR rung. Without one, `NoopRepairer` always declines, so a
    /// contract-preserving revision is never available and the ladder
    /// falls straight from RETRY to BLOCK.
    pub fn with_repairer(mut self, repairer: &'r dyn Repairer) -> Self {
        self.shared.repairer = repairer;
        self
    }

    /// Supply a runner for command nodes. Without one, `run` refuses any
    /// graph containing a command node — the library must not be able to
    /// execute shell steps by accident.
    pub fn with_command_runner(mut self, command_runner: &'r dyn CommandRunner) -> Self {
        self.shared.command_runner = Some(command_runner);
        self
    }

    /// Wire in a cancellation token. Allows cooperative cancellation of the run.
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.shared.cancel = Some(token);
        self
    }

    /// How many nodes may run concurrently. Clamped to at least 1 — a
    /// caller passing 0 gets the sequential default rather than a run that
    /// can never claim anything. `n == 1` (the default) takes the exact
    /// sequential loop from Task 5, unchanged, so every prior guarantee
    /// (bit-identical schedules included) keeps holding for callers who
    /// never touch this knob.
    pub fn with_max_inflight(mut self, n: usize) -> Self {
        self.shared.max_inflight = n.max(1);
        self
    }

    /// Override the tick source with a real clock (epoch ms). Ticks remain
    /// non-decreasing via a monotone clamp: `tick = max(clock(), previous)`.
    /// Default (no clock): the existing logical counter — unchanged.
    ///
    /// Deliberate spec deviation: the spec says "the executor's internal
    /// logical tick disappears"; instead the logical tick stays as the
    /// DEFAULT and the wall clock is opt-in. Rationale: every existing test
    /// (including `state_history_is_preserved`) pins logical-tick
    /// determinism, and library users get deterministic replay for free;
    /// the CLI always injects the wall clock, so production behavior
    /// matches the spec's intent. Treat this as chosen, not missed.
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.shared.wall_clock = Some(clock);
        self
    }

    /// Read-only access to the run store, for inspection and tests.
    pub fn store_ref(&self) -> &RunStore {
        &self.shared.store
    }

    /// The executor's current logical clock value, i.e. the timestamp of the
    /// most recent state write `run()` made. Exposed so a caller recording
    /// something that happened *after* the run (e.g. a replan revision) can
    /// stamp it with a timestamp that is guaranteed to be later than every
    /// write the run itself made, keeping the run's timeline strictly
    /// increasing and any `as_of` reconstruction faithful to what actually
    /// happened when.
    pub fn clock(&self) -> i64 {
        self.shared.clock.load(Ordering::SeqCst)
    }

    pub fn run(&mut self, start_ms: i64) -> Result<RunReport, SghError> {
        // Command nodes have a shell path only when a CommandRunner is
        // configured. Without one, dispatching a command node through
        // `AgentRunner` would send a shell command to a model as a prompt —
        // a real model call the cost bound never budgeted for. `Executor` is
        // public, so this refusal must live here rather than relying on a
        // caller to remember `.with_command_runner(..)` — otherwise any
        // other library caller could drive the executor straight past its
        // own published bound without deliberately supplying a runner.
        if self.shared.command_runner.is_none() {
            let offenders: Vec<String> = self
                .shared
                .graph
                .graph
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Command)
                .map(|n| n.id.clone())
                .collect();
            if !offenders.is_empty() {
                return Err(SghError::NoCommandRunner { nodes: offenders });
            }
        }

        self.shared.clock.store(start_ms, Ordering::SeqCst);

        if self.shared.max_inflight == 1 {
            // Topological order makes a single forward pass sufficient:
            // every dependency is resolved (or has failed and been
            // skipped) before its dependents are considered. This is the
            // exact Task 5 loop, untouched — kept as its own code path
            // (rather than routed through the scheduler with n=1) so its
            // behavior, including bit-identical schedules, can never be
            // perturbed by scheduler changes.
            let order = self.shared.graph.topo_order.clone();
            for id in order {
                // Resume-awareness: a node already `Succeeded` (from a prior
                // run of this same store) is done — no Ready write, no
                // execution. This read is unconditional, so a fresh run
                // (every node `Pending`) costs one extra read per node and
                // behaves identically to before.
                if self.shared.store.state(&id)? == NodeState::Succeeded {
                    continue;
                }

                let deps = self
                    .shared
                    .graph
                    .graph
                    .node(&id)
                    .expect("node exists")
                    .needs
                    .clone();

                let mut any_dep_unfinished = false;
                for d in &deps {
                    if self.shared.store.state(d)? != NodeState::Succeeded {
                        any_dep_unfinished = true;
                        break;
                    }
                }

                if any_dep_unfinished {
                    let t = tick(&self.shared);
                    self.shared.store.set_state(&id, NodeState::Skipped, t)?;
                    continue;
                }

                // Check for cancellation before starting the node
                if let Some(token) = &self.shared.cancel {
                    if token.is_cancelled() {
                        let t = tick(&self.shared);
                        self.shared.store.set_state(&id, NodeState::Skipped, t)?;
                        continue;
                    }
                }

                let t = tick(&self.shared);
                self.shared.store.set_state(&id, NodeState::Ready, t)?;

                execute_node(&self.shared, &id)?;
            }
        } else {
            run_parallel(&self.shared)?;
        }

        self.report()
    }

    fn report(&self) -> Result<RunReport, SghError> {
        let mut r = RunReport {
            model_calls: self.shared.model_calls.load(Ordering::SeqCst),
            command_runs: self.shared.command_runs.load(Ordering::SeqCst),
            blocked_reasons: self.shared.blocked_reasons.lock().unwrap().clone(),
            ..Default::default()
        };
        for id in &self.shared.graph.topo_order {
            match self.shared.store.state(id)? {
                NodeState::Succeeded => r.succeeded.push(id.clone()),
                NodeState::Blocked => r.blocked.push(id.clone()),
                NodeState::Skipped => r.skipped.push(id.clone()),
                _ => {}
            }
        }
        Ok(r)
    }
}

/// Every write advances a logical clock rather than reading wall time, so a
/// run's timeline is reproducible. `fetch_add` preserves both uniqueness and
/// monotonicity; in today's single-threaded caller this produces the exact
/// same sequence as the previous `self.clock += 1; self.clock`.
///
/// When a `wall_clock` is injected (see `Executor::with_clock`), ticks
/// clamp forward to wall time instead: `shared.clock` tracks the high-water
/// mark ever observed, so a stalled or backwards-jumping clock still only
/// ever produces non-decreasing ticks. Consecutive ticks are allowed to be
/// equal in this path (unlike the default, which is always strictly
/// increasing) — supersession's close-old/create-new writes are meant to
/// land atomically at the same wall-clock instant, and rejecting an equal
/// timestamp there would turn a real atomic swap into an artificially
/// ordered pair.
fn tick(shared: &Shared) -> i64 {
    match &shared.wall_clock {
        None => shared.clock.fetch_add(1, Ordering::SeqCst) + 1,
        Some(c) => {
            let now = c();
            shared.clock.fetch_max(now, Ordering::SeqCst);
            shared.clock.load(Ordering::SeqCst).max(now)
        }
    }
}

/// The ready-set scheduler for `max_inflight > 1`: workers borrow `&Shared`
/// inside `std::thread::scope` (no `Arc`, no clone — `RunStore` isn't
/// `Clone` and doesn't need to be, since the scope guarantees every spawned
/// thread joins before this function returns). A node is *ready* once every
/// dependency's state is `Succeeded` and it hasn't been started; a node with
/// a dependency that reached a terminal-but-not-`Succeeded` state can never
/// become ready and is marked `Skipped` immediately — the same store write
/// the sequential loop makes, just discovered dynamically instead of in one
/// forward pass. Ready nodes are claimed in `topo_order` order, at most
/// `max_inflight` at a time; the scheduler blocks on the results channel
/// whenever it's saturated or has nothing left to claim but work is still
/// inflight, so there is no busy-spin.
fn run_parallel(shared: &Shared) -> Result<(), SghError> {
    let order = shared.graph.topo_order.clone();
    let cap = shared.max_inflight;

    let deps_of: HashMap<&str, &[String]> = order
        .iter()
        .map(|id| {
            let node = shared.graph.graph.node(id).expect("node exists");
            (id.as_str(), node.needs.as_slice())
        })
        .collect();

    let mut started: HashSet<String> = HashSet::new();
    let mut first_error: Option<SghError> = None;

    std::thread::scope(|scope| -> Result<(), SghError> {
        let (tx, rx) = mpsc::channel::<(String, Result<(), SghError>)>();
        let mut inflight: usize = 0;

        loop {
            if started.len() == order.len() && inflight == 0 {
                break;
            }

            let cancelled = shared
                .cancel
                .as_ref()
                .map(|t| t.is_cancelled())
                .unwrap_or(false)
                || shared.aborted.load(Ordering::SeqCst);

            if cancelled {
                // Drain every already-inflight worker first, so a genuine
                // engine error surfaces deterministically as the returned
                // error rather than being silently dropped by the implicit
                // scope join.
                while inflight > 0 {
                    let (_, res) = rx.recv().expect("a worker is inflight");
                    inflight -= 1;
                    if let Err(e) = res {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                }
                for id in &order {
                    if !started.contains(id) {
                        let t = tick(shared);
                        shared.store.set_state(id, NodeState::Skipped, t)?;
                        started.insert(id.clone());
                    }
                }
                break;
            }

            // Claim pass: walk topo_order, skipping already-started nodes,
            // marking never-ready nodes Skipped, and spawning workers for
            // whatever is ready, up to the cap.
            for id in &order {
                if inflight >= cap {
                    break;
                }
                if started.contains(id) {
                    continue;
                }

                // Resume-awareness: a node already `Succeeded` (from a prior
                // run of this same store) is completed from the start — no
                // Ready write, no worker spawned. Dependents still see it as
                // finished via the ordinary dep-state checks below.
                if shared.store.state(id)? == NodeState::Succeeded {
                    started.insert(id.clone());
                    continue;
                }

                let mut any_unfinished = false;
                let mut any_dead = false;
                for d in deps_of[id.as_str()] {
                    let st = shared.store.state(d)?;
                    if st != NodeState::Succeeded {
                        any_unfinished = true;
                        if st.is_terminal() {
                            any_dead = true;
                        }
                    }
                }

                if any_dead {
                    let t = tick(shared);
                    shared.store.set_state(id, NodeState::Skipped, t)?;
                    started.insert(id.clone());
                    continue;
                }
                if any_unfinished {
                    continue;
                }

                let t = tick(shared);
                shared.store.set_state(id, NodeState::Ready, t)?;
                started.insert(id.clone());
                inflight += 1;

                let tx = tx.clone();
                let id_owned = id.clone();
                scope.spawn(move || {
                    // A panicking runner must not deadlock the scheduler: if
                    // the worker never sends, `rx.recv()` below blocks
                    // forever (this thread's own `tx` clone keeps the
                    // channel alive), and `thread::scope` only re-raises the
                    // panic after every spawned closure returns — which
                    // never happens. Catch it and report a clean error
                    // instead of resuming the unwind.
                    let res = catch_unwind(AssertUnwindSafe(|| execute_node(shared, &id_owned)))
                        .unwrap_or_else(|_| {
                            Err(SghError::WorkerPanic {
                                node: id_owned.clone(),
                            })
                        });
                    let _ = tx.send((id_owned, res));
                });
            }

            if started.len() == order.len() && inflight == 0 {
                break;
            }

            if inflight > 0 {
                let (_, res) = rx.recv().expect("a worker is inflight");
                inflight -= 1;
                if let Err(e) = res {
                    if let Some(token) = &shared.cancel {
                        token.cancel();
                    } else {
                        shared.aborted.store(true, Ordering::SeqCst);
                    }
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }

        Ok(())
    })?;

    if let Some(e) = first_error {
        return Err(e);
    }
    Ok(())
}

fn execute_node(shared: &Shared, id: &str) -> Result<(), SghError> {
    let original = shared.graph.graph.node(id).expect("node exists").clone();

    // Gate nodes halt the run for human approval; there is no interactive
    // surface yet, so a gate blocks — unless a resumed run's history
    // already carries an "approve" attempt (recorded out-of-band, e.g. by
    // a CLI approval command), in which case the gate passes straight
    // through: tick, `Succeeded`, no output, no blocked_reasons entry.
    if original.kind == NodeKind::Gate {
        let prior = shared.store.attempts(id)?;
        let approved = prior.iter().any(|(rung, _)| rung == "approve");
        let t = tick(shared);
        if approved {
            shared.store.set_state(id, NodeState::Succeeded, t)?;
        } else {
            shared.store.set_state(id, NodeState::Blocked, t)?;
        }
        return Ok(());
    }

    // Bounded context: inputs are exactly the outputs of this node's
    // declared dependencies. Nothing else in the run is reachable from
    // here, and this map is the only channel through which a node sees
    // prior work.
    let mut inputs = BTreeMap::new();
    for dep in &original.needs {
        if let Some(out) = shared.store.output(dep)? {
            inputs.insert(dep.clone(), out);
        }
    }

    // Commands are retry-only: there is no model to consult for a shell
    // invocation, so their repair budget is ignored. Task 3's cost model
    // (`bound.rs`) has no repair term for commands for the same reason.
    let repair_budget = match original.kind {
        NodeKind::Agent => original.budget.repairs,
        _ => 0,
    };

    // `node` is the revisable working copy the ladder operates on; only
    // its prompt ever changes (via a contract-preserving repair).
    // `original` stays untouched so every repair is checked against the
    // node's true, frozen contract, not against the last revision.
    let mut node = original.clone();

    // Resume-awareness: budget consumed in a prior run of this node counts
    // against this run's budget too, so total spend across resumes never
    // exceeds the original bound. Counted by exact rung string — "block",
    // "cancelled", and "approve" never consumed budget and still don't.
    // `saturating_sub` means a store somehow carrying more recorded attempts
    // than the budget allows just yields 0 remaining rather than panicking.
    let prior = shared.store.attempts(id)?;
    let prior_retries = prior.iter().filter(|(rung, _)| rung == "retry").count() as u32;
    let prior_repairs = prior.iter().filter(|(rung, _)| rung == "repair").count() as u32;
    let mut retries_left = original.budget.retries.saturating_sub(prior_retries);
    let mut repairs_left = repair_budget.saturating_sub(prior_repairs);

    loop {
        // Check for cancellation at the top of each ladder iteration
        if let Some(token) = &shared.cancel {
            if token.is_cancelled() {
                let t = tick(shared);
                shared
                    .store
                    .record_attempt(id, "cancelled", "cancelled", t)?;
                shared
                    .blocked_reasons
                    .lock()
                    .unwrap()
                    .insert(id.to_string(), "cancelled".to_string());
                let t = tick(shared);
                shared.store.set_state(id, NodeState::Blocked, t)?;
                return Ok(());
            }
        }

        let t = tick(shared);
        shared.store.set_state(id, NodeState::Running, t)?;

        let outcome = if node.kind == NodeKind::Command {
            let creq = CommandRequest {
                node_id: id.to_string(),
                run: node.run.clone().expect("validated: command nodes have run"),
                inputs: inputs.clone(),
                // Mirrors the agent branch. The runner uses this only to
                // decide whether to pass stdout through verbatim (schema
                // declared) or wrap it as {"stdout":..,"exit_code":..}.
                // Validation itself still happens in `validate_output`.
                output_schema: node.output.as_ref().map(|o| o.schema.clone()),
            };
            shared.command_runs.fetch_add(1, Ordering::SeqCst);
            let runner = shared
                .command_runner
                .expect("checked in run(): command nodes imply a command runner");
            match runner.run(&creq) {
                Ok(o) => o,
                Err(e) => NodeOutcome::Failed {
                    error: e.to_string(),
                },
            }
        } else {
            let req = NodeRequest {
                node_id: id.to_string(),
                prompt: node.prompt.clone().or(node.run.clone()).unwrap_or_default(),
                inputs: inputs.clone(),
                output_schema: node.output.as_ref().map(|o| o.schema.clone()),
                tools: node.tools.clone(),
            };

            if node.kind == NodeKind::Agent {
                shared.model_calls.fetch_add(1, Ordering::SeqCst);
            }

            match shared.runner.run(&req) {
                Ok(o) => o,
                Err(e) => NodeOutcome::Failed {
                    error: e.to_string(),
                },
            }
        };

        let error = match outcome {
            NodeOutcome::Succeeded { output } => match validate_output(&node, &output) {
                Ok(()) => {
                    let t = tick(shared);
                    shared.store.record_output(id, &output, t)?;
                    let t = tick(shared);
                    shared.store.set_state(id, NodeState::Succeeded, t)?;
                    return Ok(());
                }
                Err(reason) => reason,
            },
            NodeOutcome::Failed { error } => error,
            NodeOutcome::Denied { tool } => format!(
                "provider denied tool {tool} — the node cannot have done its work \
                 under this provider's granted surface"
            ),
        };

        let t = tick(shared);
        shared.store.set_state(id, NodeState::Failed, t)?;

        // Strict ascent: retries, then repairs, then block. No
        // classifier decides which rung a failure "deserves" — that
        // would be a heuristic governing autonomous work, exactly the
        // implicit control flow this project exists to remove.
        let rung = if retries_left > 0 {
            retries_left -= 1;
            Rung::Retry
        } else if repairs_left > 0 {
            repairs_left -= 1;
            Rung::Repair
        } else {
            Rung::Block
        };

        let t = tick(shared);
        shared.store.record_attempt(id, rung.as_str(), &error, t)?;

        match rung {
            Rung::Retry => {
                // A bare retry re-runs the identical prompt, so a model
                // that replied with prose (failing a schema node's output
                // check) tends to reply with the same prose again — an
                // idempotent re-run against a finished workspace could
                // block forever this way. Feed the failure back and, for a
                // schema node, demand JSON. This spends no extra model call
                // (the retry is already budgeted and bounded); it only
                // changes what the next attempt is told. The downstream
                // command node still verifies the claim, so coaxing a
                // parseable reply cannot mask unfinished work.
                if node.kind == NodeKind::Agent {
                    let base = original.prompt.as_deref().unwrap_or_default();
                    node.prompt = Some(retry_prompt(base, &error, original.output.is_some()));
                }
                let t = tick(shared);
                shared.store.set_state(id, NodeState::Recovering, t)?;
            }
            Rung::Repair => {
                // The bound (`bound.rs`) budgets `2*repairs` model calls
                // per agent node: one call to consult the recovery
                // model, then one re-execution of the node. Only the
                // re-execution was counted before this fix (at the top
                // of the loop); count the consultation itself here so
                // `RunReport.model_calls` and `Bound.agent_calls` meter
                // the same thing. Repair budget is always 0 for
                // non-agent nodes, so this rung is unreachable for them
                // and the guard is just documentation-by-code.
                if node.kind == NodeKind::Agent {
                    shared.model_calls.fetch_add(1, Ordering::SeqCst);
                }
                match shared.repairer.repair(&node, &error) {
                    // A repair that breaks the contract is not a repair
                    // — refuse it and block rather than let the graph
                    // silently mutate.
                    Some(revised) if contract_preserved(&original, &revised) => {
                        node = revised;
                        let t = tick(shared);
                        shared.store.set_state(id, NodeState::Recovering, t)?;
                    }
                    _ => {
                        let t = tick(shared);
                        shared
                            .blocked_reasons
                            .lock()
                            .unwrap()
                            .insert(id.to_string(), error.clone());
                        shared.store.set_state(id, NodeState::Blocked, t)?;
                        return Ok(());
                    }
                }
            }
            Rung::Block => {
                let t = tick(shared);
                shared
                    .blocked_reasons
                    .lock()
                    .unwrap()
                    .insert(id.to_string(), error.clone());
                shared.store.set_state(id, NodeState::Blocked, t)?;
                return Ok(());
            }
        }
    }
}

/// Build the prompt for a retried agent node: the original task, then the
/// previous failure and a directive to fix it. For a schema-bearing node the
/// directive demands JSON-only — the common reason a schema node's retry keeps
/// failing is that the model narrated prose again instead of emitting the
/// required JSON. Always rebuilt from the original prompt (not the last
/// retry's), so corrections do not accumulate across retries.
fn retry_prompt(base: &str, error: &str, expects_json: bool) -> String {
    let mut p = String::from(base);
    p.push_str("\n\n## Correction — your previous attempt was rejected\n\n");
    p.push_str("Reason: ");
    p.push_str(error);
    p.push('\n');
    if expects_json {
        p.push_str(
            "Reply with ONLY the JSON object matching the required schema — no prose, \
             no explanation, no code fences. Even if the work is already complete and \
             you change nothing, still output JSON reflecting the current state.",
        );
    } else {
        p.push_str("Correct the issue above and complete the task.");
    }
    p
}

/// A node returning output that does not match its declared schema has
/// failed — this is what makes declared outputs load-bearing rather than
/// documentation. A node with no declared output schema is unconstrained.
fn validate_output(node: &crate::schema::Node, output: &str) -> Result<(), String> {
    let Some(spec) = &node.output else {
        return Ok(());
    };
    let value: serde_json::Value =
        serde_json::from_str(output).map_err(|e| format!("output is not valid json: {e}"))?;
    let compiled = jsonschema::JSONSchema::compile(&spec.schema)
        .map_err(|e| format!("output schema does not compile: {e}"))?;
    if let Err(errors) = compiled.validate(&value) {
        let msgs: Vec<String> = errors.map(|e| e.to_string()).collect();
        return Err(format!("output does not match schema: {}", msgs.join("; ")));
    }
    Ok(())
}
