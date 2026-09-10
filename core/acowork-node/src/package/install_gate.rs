//! Per-node serial install gate.
//!
//! The Node is the *execution seat* for package installation: it is the
//! last point every install path converges on (the Gateway's HTTP
//! handler, the CLI's direct-to-broker dispatch, the bundled bootstrap),
//! so it is the only place where "one install at a time" can actually be
//! guaranteed. The Android analogue is `PackageManagerService`: callers
//! submit intents, the device serializes them.
//!
//! The gate owns no policy of its own beyond the two invariants from
//! [`acowork_core::install`]:
//!
//! - **serialization** — one install runs at a time; the rest queue,
//!   system-lane tickets first. Enforced by
//!   [`acowork_core::install::InstallScheduler::take_next`], so a worker
//!   physically cannot start a second install.
//! - **instance identity** — a job is identified by the `instance_id`
//!   it will create, so a repeated request for the same instance never
//!   queues a second job.
//! - **idempotency of declarative requests** — an `Ensure` whose
//!   precondition already holds is a no-op. Re-checked *at execution
//!   time* (not just at admission), because the condition can change
//!   while a ticket waits in the queue: with serialization, the
//!   preceding install may well be the one that satisfied it. This
//!   check — not any package-level deduplication — is what collapses N
//!   concurrent "ensure the System Agent is installed" calls into one
//!   installed instance.
//!
//! Explicit installs are never collapsed by package: two `Install`
//! intents with different instance ids produce two instances (ADR-073).

use std::sync::Arc;

use acowork_core::agent_instance_id::AgentInstanceId;
use acowork_core::install::{
    AdmitOutcome, InstallKind, InstallOutcome, InstallRequest, InstallScheduler, InstallTicket,
};
use acowork_core::operation::OperationId;
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

/// Side effects of one install, supplied by the host.
///
/// Kept as a trait so the gate's serialization and idempotency can be
/// tested against a fake executor — no filesystem, no MQTT, no
/// subprocesses.
#[async_trait]
pub trait InstallExecutor: Send + Sync + 'static {
    /// Perform the install described by `ticket`.
    ///
    /// Must not itself serialize: the gate guarantees it is never
    /// called concurrently with another `execute` for this node.
    async fn execute(&self, ticket: &InstallTicket) -> InstallOutcome;

    /// Look up an existing instance of `agent_id` in the host's install
    /// table.
    ///
    /// Consulted only for [`InstallKind::Ensure`] tickets. The default
    /// of "nothing installed" makes the gate behave conservatively for
    /// hosts that do not track an inventory.
    async fn existing_instance(&self, _agent_id: &str) -> Option<AgentInstanceId> {
        None
    }
}

/// Terminal outcomes of queued jobs, as they happen.
///
/// The gate owns the job lifecycle, so it — not the executor — is the
/// authority on *when* a job reached a terminal outcome: a job
/// suppressed by the post-dequeue precondition check never reaches the
/// executor at all. Hosts project this onto their own status surface
/// (the Node's `NodeEvent` reply, the Gateway's `OperationStore`); a
/// host that only listened to the executor would leave those operations
/// running forever.
///
/// Implementations must be cheap and non-failing: the sink is a status
/// projection, never a participant in the install.
#[async_trait]
pub trait InstallOutcomeSink: Send + Sync + 'static {
    /// Called once per job, after the slot is released, with the
    /// ticket that produced the outcome.
    async fn on_terminal(&self, ticket: &InstallTicket, outcome: &InstallOutcome);
}

/// Sink for hosts with no status surface.
pub struct NullOutcomeSink;

#[async_trait]
impl InstallOutcomeSink for NullOutcomeSink {
    async fn on_terminal(&self, _ticket: &InstallTicket, _outcome: &InstallOutcome) {}
}

/// Serial install gate for one node.
///
/// Cheap to clone via `Arc`; the queue lives behind the shared handle.
pub struct InstallGate {
    scheduler: Mutex<InstallScheduler>,
    /// Single-slot wakeup token: a poke is pending or it is not, so the
    /// worker cannot miss a submission between finishing a drain and
    /// awaiting the next one.
    wake_tx: mpsc::Sender<()>,
    wake_rx: Mutex<mpsc::Receiver<()>>,
}

/// Debug is intentionally coarse: a `Debug` impl must not lock the
/// scheduler (it is reachable from `NodeState`'s derived `Debug`, which
/// must never block or await).
impl std::fmt::Debug for InstallGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallGate").finish_non_exhaustive()
    }
}

impl InstallGate {
    /// Create an idle gate.
    pub fn new() -> Arc<Self> {        let (wake_tx, wake_rx) = mpsc::channel(1);
        Arc::new(Self {
            scheduler: Mutex::new(InstallScheduler::new()),
            wake_tx,
            wake_rx: Mutex::new(wake_rx),
        })
    }

    /// Admit an install request, applying the coalescing rules.
    ///
    /// Returns as soon as the request is queued (or merged into a queued
    /// job): the caller then awaits `completion()` for the result. The
    /// duplicate callers that produced three instances on one intent
    /// now receive the *same* [`AdmitOutcome::Coalesced`] handle.
    pub async fn admit(&self, request: InstallRequest) -> AdmitOutcome {
        let outcome = self.scheduler.lock().await.admit(request);
        if outcome.is_accepted() {
            // Full channel means a wakeup is already pending — either
            // way the worker will drain after this push.
            let _ = self.wake_tx.try_send(());
        }
        outcome
    }

    /// Admit and await the terminal outcome.
    ///
    /// Convenience for hosts that answer the caller with the install
    /// result (the Node's control-plane reply is the Gateway's
    /// completion signal).
    pub async fn admit_and_wait(&self, request: InstallRequest) -> InstallOutcome {
        match self.admit(request).await {
            AdmitOutcome::AlreadySatisfied { instance_id } => {
                InstallOutcome::AlreadySatisfied { instance_id }
            }
            AdmitOutcome::Accepted { completion, .. }
            | AdmitOutcome::Coalesced { completion, .. } => completion.wait().await,
        }
    }

    /// Run the serial worker.
    ///
    /// Spawn this once per node: it drains the queue, executing at most
    /// one ticket at a time, then parks on the wakeup channel. It holds
    /// a handle to the gate, so it is a process-lifetime task — there is
    /// no shutdown path today, matching the node daemon's own lifetime.
    ///
    /// Every terminal outcome is reported to `sink` — including the ones
    /// the executor never saw — after the slot is released, so a slow
    /// status projection can never stall the next install.
    pub async fn run_worker(
        self: Arc<Self>,
        executor: Arc<dyn InstallExecutor>,
        sink: Arc<dyn InstallOutcomeSink>,
    ) {
        loop {
            loop {
                let next = self.scheduler.lock().await.take_next();
                let Some(ticket) = next else { break };
                let outcome = self.run_one(&ticket, executor.as_ref()).await;
                let released = self
                    .scheduler
                    .lock()
                    .await
                    .finish(&ticket.operation_id, outcome.clone());
                debug_assert!(
                    released,
                    "completed ticket {} was not the in-flight one",
                    ticket.operation_id
                );
                sink.on_terminal(&ticket, &outcome).await;
            }
            let mut rx = self.wake_rx.lock().await;
            if rx.recv().await.is_none() {
                return;
            }
        }
    }

    /// Execute one ticket, re-checking declarative preconditions.
    async fn run_one(
        &self,
        ticket: &InstallTicket,
        executor: &dyn InstallExecutor,
    ) -> InstallOutcome {
        if ticket.kind == InstallKind::Ensure
            && let Some(existing) = executor.existing_instance(&ticket.agent_id).await
        {
            tracing::info!(
                agent_id = %ticket.agent_id,
                instance_id = %existing,
                requested_instance_id = %ticket.instance_id,
                operation_id = %ticket.operation_id,
                "Ensure install satisfied by an existing instance — skipping install"
            );
            return InstallOutcome::AlreadySatisfied {
                instance_id: existing,
            };
        }
        let outcome = executor.execute(ticket).await;
        match &outcome {
            InstallOutcome::Ready { instance_id } => tracing::info!(
                agent_id = %ticket.agent_id,
                instance_id = %instance_id,
                operation_id = %ticket.operation_id,
                "Install gate completed"
            ),
            InstallOutcome::Failed { message } => tracing::warn!(
                agent_id = %ticket.agent_id,
                instance_id = %ticket.instance_id,
                operation_id = %ticket.operation_id,
                error = %message,
                "Install gate failed"
            ),
            InstallOutcome::AlreadySatisfied { instance_id } => tracing::debug!(
                instance_id = %instance_id,
                "Install gate no-op"
            ),
        }
        outcome
    }

    /// Number of queued installs (diagnostics / tests).
    pub async fn pending_len(&self) -> usize {
        self.scheduler.lock().await.pending_len()
    }

    /// The running install's operation id, if any (diagnostics / tests).
    pub async fn inflight_operation_id(&self) -> Option<OperationId> {
        self.scheduler.lock().await.inflight_operation_id().cloned()
    }

    /// Whether the gate is completely idle.
    pub async fn is_idle(&self) -> bool {
        self.scheduler.lock().await.is_idle()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use acowork_core::install::{InstallPriority, InstallSource};

    /// Records every terminal outcome the gate reports.
    #[derive(Default)]
    struct RecordingSink {
        outcomes: Mutex<Vec<(String, InstallOutcome)>>,
    }

    impl RecordingSink {
        /// One `(operation_id, outcome)` pair per terminal job.
        async fn records(&self) -> Vec<(String, InstallOutcome)> {
            self.outcomes.lock().await.clone()
        }
    }

    #[async_trait]
    impl InstallOutcomeSink for RecordingSink {
        async fn on_terminal(&self, ticket: &InstallTicket, outcome: &InstallOutcome) {
            self.outcomes
                .lock()
                .await
                .push((ticket.operation_id.as_str().to_string(), outcome.clone()));
        }
    }

    /// Records concurrency and per-agent execution counts.
    #[derive(Default)]
    struct FakeExecutor {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        executed: Mutex<Vec<String>>,
        installed: Mutex<Vec<(String, AgentInstanceId)>>,
        /// Pre-existing instances the host would report.
        preexisting: Mutex<Vec<(String, AgentInstanceId)>>,
        /// Fail the n-th execute call (0-based).
        fail_on_call: AtomicUsize,
        calls: AtomicUsize,
    }

    impl FakeExecutor {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                fail_on_call: AtomicUsize::new(usize::MAX),
                ..Default::default()
            })
        }

        async fn installed_for(&self, agent_id: &str) -> Vec<AgentInstanceId> {
            self.installed
                .lock()
                .await
                .iter()
                .filter(|(id, _)| id == agent_id)
                .map(|(_, instance)| instance.clone())
                .collect()
        }

        async fn executed_agents(&self) -> Vec<String> {
            self.executed.lock().await.clone()
        }

        async fn max_concurrency(&self) -> usize {
            self.max_in_flight.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl InstallExecutor for FakeExecutor {
        async fn execute(&self, ticket: &InstallTicket) -> InstallOutcome {
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(current, Ordering::SeqCst);
            self.executed.lock().await.push(ticket.agent_id.clone());
            // Yield so a second concurrent `execute` (if the gate ever
            // allowed one) would be observed by the counter above.
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == self.fail_on_call.load(Ordering::SeqCst) {
                return InstallOutcome::Failed {
                    message: "injected failure".to_string(),
                };
            }
            self.installed
                .lock()
                .await
                .push((ticket.agent_id.clone(), ticket.instance_id.clone()));
            InstallOutcome::Ready {
                instance_id: ticket.instance_id.clone(),
            }
        }

        async fn existing_instance(&self, agent_id: &str) -> Option<AgentInstanceId> {
            {
                let preexisting = self.preexisting.lock().await;
                let found = preexisting
                    .iter()
                    .find(|(id, _)| id == agent_id)
                    .map(|(_, instance)| instance.clone());
                if found.is_some() {
                    return found;
                }
            }
            let installed = self.installed.lock().await;
            installed
                .iter()
                .find(|(id, _)| id == agent_id)
                .map(|(_, instance)| instance.clone())
        }
    }

    /// Payload origin used by gate tests. The gate never interprets it;
    /// it must simply survive admission into the executed ticket.
    fn test_source() -> InstallSource {
        InstallSource::local_file("D:/tmp/com.test.agent")
    }

    fn install_request(agent_id: &str, is_system: bool) -> InstallRequest {
        InstallRequest::install(
            OperationId::new(),
            agent_id,
            AgentInstanceId::new(),
            is_system,
            test_source(),
        )
    }

    fn ensure_request(agent_id: &str, is_system: bool) -> InstallRequest {
        InstallRequest::ensure(
            OperationId::new(),
            agent_id,
            AgentInstanceId::new(),
            is_system,
            test_source(),
        )
    }

    fn spawn(gate: &Arc<InstallGate>, executor: &Arc<FakeExecutor>) {
        spawn_with_sink(gate, executor, Arc::new(RecordingSink::default()));
    }

    fn spawn_with_sink(
        gate: &Arc<InstallGate>,
        executor: &Arc<FakeExecutor>,
        sink: Arc<RecordingSink>,
    ) {
        let gate = gate.clone();
        let executor: Arc<dyn InstallExecutor> = executor.clone();
        let sink: Arc<dyn InstallOutcomeSink> = sink;
        tokio::spawn(async move { gate.run_worker(executor, sink).await });
    }

    /// The regression behind this whole change: N concurrent "ensure the
    /// System Agent is installed" calls arrive as N *independent*
    /// requests — each with its own instance id, because the Gateway
    /// mints one per request — and must still land exactly one
    /// instance.
    ///
    /// Nothing here deduplicates by package: the second and third
    /// tickets are separate jobs that run in turn, and each finds the
    /// instance the first one landed. That is why the fix survives
    /// dropping the agent-id key.
    #[tokio::test]
    async fn concurrent_ensure_calls_land_one_instance() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();
        spawn(&gate, &executor);

        let (a, b, c) = tokio::join!(
            gate.admit_and_wait(ensure_request("com.acowork.system", true)),
            gate.admit_and_wait(ensure_request("com.acowork.system", true)),
            gate.admit_and_wait(ensure_request("com.acowork.system", true)),
        );

        for outcome in [&a, &b, &c] {
            assert!(outcome.is_satisfied(), "expected success, got {outcome:?}");
        }
        // Every caller resolves to the same, single instance.
        assert_eq!(a.instance_id(), b.instance_id());
        assert_eq!(b.instance_id(), c.instance_id());
        assert_eq!(
            executor.installed_for("com.acowork.system").await.len(),
            1
        );
        // The two later requests were satisfied, not re-installed.
        assert!(matches!(b, InstallOutcome::AlreadySatisfied { .. }));
        assert!(matches!(c, InstallOutcome::AlreadySatisfied { .. }));
    }

    /// Re-submitting the *same* instance id is the same install: it is
    /// merged at admission and never reaches the executor twice.
    #[tokio::test]
    async fn resubmitted_instance_id_is_coalesced_not_executed_twice() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();
        let instance = AgentInstanceId::new();

        let first = gate
            .admit(InstallRequest::install(
                OperationId::new(),
                "com.acowork.senior-engineer",
                instance.clone(),
                false,
                test_source(),
            ))
            .await;
        let second = gate
            .admit(InstallRequest::install(
                OperationId::new(),
                "com.acowork.senior-engineer",
                instance.clone(),
                false,
                test_source(),
            ))
            .await;

        assert!(first.is_accepted());
        assert!(second.is_coalesced());
        assert_eq!(second.operation_id(), first.operation_id());

        spawn(&gate, &executor);
        let outcome = first
            .completion()
            .expect("completion handle")
            .clone()
            .wait()
            .await;
        assert!(outcome.is_satisfied());
        assert_eq!(executor.executed_agents().await.len(), 1);
    }

    /// Two explicit installs of the same package are a legal request for
    /// two instances — the gate must serialize them, not merge them.
    #[tokio::test]
    async fn two_explicit_installs_of_one_package_land_two_instances() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();
        spawn(&gate, &executor);

        let (a, b) = tokio::join!(
            gate.admit_and_wait(install_request("com.acowork.senior-engineer", false)),
            gate.admit_and_wait(install_request("com.acowork.senior-engineer", false)),
        );

        assert!(a.is_satisfied());
        assert!(b.is_satisfied());
        assert_ne!(a.instance_id(), b.instance_id());
        assert_eq!(
            executor
                .installed_for("com.acowork.senior-engineer")
                .await
                .len(),
            2
        );
    }

    /// An explicit install is never suppressed by an existing instance:
    /// "an instance of this package already exists" is not a reason to
    /// refuse another copy (ADR-073).
    #[tokio::test]
    async fn explicit_install_is_not_suppressed_by_an_existing_instance() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();
        let existing = AgentInstanceId::new();
        executor
            .preexisting
            .lock()
            .await
            .push(("com.acowork.senior-engineer".to_string(), existing.clone()));
        spawn(&gate, &executor);

        let outcome =
            gate.admit_and_wait(install_request("com.acowork.senior-engineer", false)).await;

        assert!(matches!(outcome, InstallOutcome::Ready { .. }));
        assert_ne!(outcome.instance_id(), Some(&existing));
        assert_eq!(executor.executed_agents().await.len(), 1);
    }

    #[tokio::test]
    async fn installs_never_run_concurrently() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();
        spawn(&gate, &executor);

        let mut handles = Vec::new();
        for i in 0..6 {
            let gate = gate.clone();
            handles.push(tokio::spawn(async move {
                gate.admit_and_wait(install_request(&format!("com.test.agent-{i}"), false))
                    .await
            }));
        }
        for handle in handles {
            assert!(handle.await.expect("join").is_satisfied());
        }

        assert_eq!(executor.max_concurrency().await, 1);
        assert_eq!(executor.executed_agents().await.len(), 6);
    }

    #[tokio::test]
    async fn system_lane_is_served_first() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();

        // Queue a user ticket, then a system ticket, with no worker
        // running yet so the dequeue order is what decides the outcome.
        let user = gate.admit(install_request("com.test.user", false)).await;
        let system = gate.admit(install_request("com.acowork.system", true)).await;
        assert!(user.is_accepted() && system.is_accepted());
        spawn(&gate, &executor);

        let system_outcome = system
            .completion()
            .expect("completion handle")
            .clone()
            .wait()
            .await;
        let user_outcome = user
            .completion()
            .expect("completion handle")
            .clone()
            .wait()
            .await;
        assert!(system_outcome.is_satisfied() && user_outcome.is_satisfied());

        let order = executor.executed_agents().await;
        let system_pos = order
            .iter()
            .position(|id| id == "com.acowork.system")
            .expect("system install ran");
        let user_pos = order
            .iter()
            .position(|id| id == "com.test.user")
            .expect("user install ran");
        assert!(
            system_pos < user_pos,
            "system ticket must dequeue first, got order {order:?}"
        );
    }

    /// The precondition can be satisfied *while* a ticket waits in the
    /// queue — by the install in front of it, or by an inventory entry
    /// restored from disk. The execution-time re-check is what closes
    /// that window.
    #[tokio::test]
    async fn ensure_is_suppressed_when_satisfied_while_queued() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();

        // Block the worker behind a slow install, then queue the Ensure.
        let blocker = gate.admit(install_request("com.test.blocker", false)).await;
        let ensure = gate.admit(ensure_request("com.acowork.system", true)).await;
        assert!(blocker.is_accepted() && ensure.is_accepted());

        // While the Ensure sits in the queue, the instance shows up in
        // the host's inventory (e.g. the node restored it from disk).
        let restored = AgentInstanceId::new();
        executor
            .preexisting
            .lock()
            .await
            .push(("com.acowork.system".to_string(), restored.clone()));

        spawn(&gate, &executor);
        let _ = blocker
            .completion()
            .expect("completion handle")
            .clone()
            .wait()
            .await;
        let outcome = ensure
            .completion()
            .expect("completion handle")
            .clone()
            .wait()
            .await;

        match outcome {
            InstallOutcome::AlreadySatisfied { instance_id } => {
                assert_eq!(instance_id, restored)
            }
            other => panic!("expected AlreadySatisfied, got {other:?}"),
        }
        // The Ensure never reached the executor.
        assert!(
            !executor
                .executed_agents()
                .await
                .iter()
                .any(|id| id == "com.acowork.system")
        );
    }

    /// Every terminal outcome must reach the sink — including the one the
    /// executor never saw. A host that only heard from the executor would
    /// leave the suppressed operation `Running` until it expired.
    #[tokio::test]
    async fn sink_reports_every_terminal_outcome_including_suppressed_jobs() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();
        let sink = Arc::new(RecordingSink::default());

        let blocker = gate.admit(install_request("com.test.blocker", false)).await;
        let ensure = gate.admit(ensure_request("com.acowork.system", true)).await;
        let doomed = gate.admit(install_request("com.test.boom", false)).await;
        assert!(blocker.is_accepted() && ensure.is_accepted() && doomed.is_accepted());
        executor.fail_on_call.store(1, Ordering::SeqCst);

        // While the Ensure is queued, its precondition becomes true.
        let restored = AgentInstanceId::new();
        executor
            .preexisting
            .lock()
            .await
            .push(("com.acowork.system".to_string(), restored.clone()));

        spawn_with_sink(&gate, &executor, sink.clone());
        for admitted in [&blocker, &ensure, &doomed] {
            let _ = admitted
                .completion()
                .expect("completion handle")
                .clone()
                .wait()
                .await;
        }

        // Order is the gate's (system lane first), not admission order —
        // what matters is that each job reported exactly once, keyed by
        // the operation id the caller is tracking.
        let reported = sink.records().await;
        assert_eq!(reported.len(), 3, "one terminal event per job: {reported:?}");
        let mut reported_ids = reported.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        reported_ids.sort();
        let mut admitted_ids = [&blocker, &ensure, &doomed]
            .iter()
            .map(|admitted| {
                admitted
                    .operation_id()
                    .expect("queued job")
                    .as_str()
                    .to_string()
            })
            .collect::<Vec<_>>();
        admitted_ids.sort();
        assert_eq!(reported_ids, admitted_ids);

        assert_eq!(
            reported
                .iter()
                .filter(|entry| matches!(entry.1, InstallOutcome::Ready { .. }))
                .count(),
            1
        );
        assert_eq!(
            reported
                .iter()
                .filter(|entry| matches!(entry.1, InstallOutcome::Failed { .. }))
                .count(),
            1
        );
        assert_eq!(
            reported
                .iter()
                .filter(|entry| matches!(
                    entry.1,
                    InstallOutcome::AlreadySatisfied { ref instance_id } if instance_id == &restored
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn failure_releases_the_slot_for_the_next_ticket() {
        let gate = InstallGate::new();
        let executor = FakeExecutor::new();
        executor.fail_on_call.store(0, Ordering::SeqCst);
        spawn(&gate, &executor);

        let (failed, ok) = tokio::join!(
            gate.admit_and_wait(install_request("com.test.boom", false)),
            gate.admit_and_wait(install_request("com.test.after", false)),
        );

        let outcomes = [failed, ok];
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, InstallOutcome::Failed { .. }))
                .count(),
            1,
            "exactly one ticket must fail: {outcomes:?}"
        );
        assert_eq!(
            outcomes.iter().filter(|o| o.is_satisfied()).count(),
            1,
            "the failure must not block the next ticket: {outcomes:?}"
        );
        assert!(gate.is_idle().await);
        assert!(gate.inflight_operation_id().await.is_none());
    }

    #[tokio::test]
    async fn ensure_with_known_instance_never_reaches_the_queue() {
        let gate = InstallGate::new();
        let existing = AgentInstanceId::new();
        let outcome = gate
            .admit(
                ensure_request("com.acowork.system", true).with_already_installed(existing.clone()),
            )
            .await;

        assert!(matches!(
            outcome,
            AdmitOutcome::AlreadySatisfied { ref instance_id } if instance_id == &existing
        ));
        assert!(gate.is_idle().await);
        assert_eq!(gate.pending_len().await, 0);
    }

    #[tokio::test]
    async fn priority_comes_from_the_system_flag() {
        let gate = InstallGate::new();
        let system = gate.admit(ensure_request("com.acowork.system", true)).await;
        let user = gate.admit(install_request("com.test.agent", false)).await;

        assert_eq!(
            system.ticket().expect("admitted").priority,
            InstallPriority::System
        );
        assert_eq!(
            user.ticket().expect("admitted").priority,
            InstallPriority::User
        );
    }
}
