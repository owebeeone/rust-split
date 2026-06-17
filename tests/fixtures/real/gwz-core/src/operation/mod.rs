use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model;
use crate::runtime::clock::TimestampMs;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionKind {
    CreateWorkspace,
    InitFromSources,
    AddExistingRepo,
    CreateRepo,
    Materialize,
    Status,
    Snapshot,
    Tag,
    PullHead,
    PullSnapshot,
    Push,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlannedAction {
    Noop,
    Clone,
    Fetch,
    FastForward,
    Checkout,
    InitRepo,
    AddManifestMember,
    WriteManifest,
    WriteLock,
    WriteSnapshot,
    WriteTag,
    Push,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationContext {
    pub operation_id: String,
    pub request_id: String,
    pub schema_version: String,
    pub action: ActionKind,
    pub dry_run: bool,
    pub attribution: Option<model::OperationAttribution>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationPlan {
    pub operation_id: String,
    pub action: ActionKind,
    pub dry_run: bool,
    pub members: Vec<MemberPlan>,
}

impl OperationPlan {
    pub fn requires_mutation(&self) -> bool {
        self.members.iter().any(|member| member.requires_mutation)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberPlan {
    pub member_id: Option<model::MemberId>,
    pub member_path: String,
    pub source_kind: model::SourceKind,
    pub action: PlannedAction,
    pub requires_mutation: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionReport {
    pub members: Vec<MemberExecution>,
    pub errors: Vec<OperationError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberExecution {
    pub member_id: Option<model::MemberId>,
    pub member_path: String,
    pub source_kind: model::SourceKind,
    pub status: MemberExecutionStatus,
    pub error: Option<OperationError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberExecutionStatus {
    Ok,
    Noop,
    Skipped,
    Rejected,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationError {
    pub code: model::ErrorCode,
    pub message: String,
}

impl OperationError {
    pub fn new(code: model::ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub enum OperationRequest {
    CreateWorkspace(crate::CreateWorkspaceRequest),
    InitFromSources(crate::InitFromSourcesRequest),
    AddExistingRepo(crate::AddExistingRepoRequest),
    CreateRepo(crate::CreateRepoRequest),
    Materialize(crate::MaterializeRequest),
    Status(crate::StatusRequest),
    Snapshot(crate::SnapshotRequest),
    Tag(crate::TagRequest),
    PullHead(crate::PullHeadRequest),
    PullSnapshot(crate::PullSnapshotRequest),
    Push(crate::PushRequest),
}

impl OperationRequest {
    pub fn context(&self, operation_id: impl Into<String>) -> model::ModelResult<OperationContext> {
        let (action, meta) = match self {
            Self::CreateWorkspace(request) => (ActionKind::CreateWorkspace, &request.meta),
            Self::InitFromSources(request) => (ActionKind::InitFromSources, &request.meta),
            Self::AddExistingRepo(request) => (ActionKind::AddExistingRepo, &request.meta),
            Self::CreateRepo(request) => (ActionKind::CreateRepo, &request.meta),
            Self::Materialize(request) => (ActionKind::Materialize, &request.meta),
            Self::Status(request) => (ActionKind::Status, &request.meta),
            Self::Snapshot(request) => (ActionKind::Snapshot, &request.meta),
            Self::Tag(request) => (ActionKind::Tag, &request.meta),
            Self::PullHead(request) => (ActionKind::PullHead, &request.meta),
            Self::PullSnapshot(request) => (ActionKind::PullSnapshot, &request.meta),
            Self::Push(request) => (ActionKind::Push, &request.meta),
        };
        OperationContext::from_meta(operation_id.into(), action, meta)
    }
}

impl OperationContext {
    fn from_meta(
        operation_id: String,
        action: ActionKind,
        meta: &crate::RequestMeta,
    ) -> model::ModelResult<Self> {
        let attribution = meta
            .attribution
            .as_ref()
            .map(attribution_from_protocol)
            .transpose()?;
        Ok(Self {
            operation_id,
            request_id: meta.request_id.clone(),
            schema_version: meta.schema_version.clone(),
            action,
            dry_run: meta.dry_run.unwrap_or(false),
            attribution,
        })
    }
}

pub struct ResponseBuilder;

impl ResponseBuilder {
    pub fn accepted(context: &OperationContext, members: &[MemberPlan]) -> crate::ResponseEnvelope {
        crate::ResponseEnvelope {
            meta: crate::ResponseMeta {
                request_id: context.request_id.clone(),
                schema_version: context.schema_version.clone(),
                action: context.action.into(),
                aggregate_status: crate::AggregateStatus::Accepted,
                operation_id: Some(context.operation_id.clone()),
                message: None,
                attribution: context.attribution.as_ref().map(Into::into),
            },
            members: members.iter().map(member_plan_to_protocol).collect(),
            errors: Vec::new(),
        }
    }

    pub fn result(
        context: &OperationContext,
        report: &ExecutionReport,
        started_at_ms: TimestampMs,
        finished_at_ms: TimestampMs,
    ) -> crate::OperationResult {
        crate::OperationResult {
            operation_id: context.operation_id.clone(),
            request_id: context.request_id.clone(),
            action: context.action.into(),
            aggregate_status: aggregate_status(report),
            started_at_ms: started_at_ms.0,
            finished_at_ms: finished_at_ms.0,
            members: report
                .members
                .iter()
                .map(member_execution_to_protocol)
                .collect(),
            errors: report
                .errors
                .iter()
                .map(operation_error_to_protocol)
                .collect(),
            attribution: context.attribution.as_ref().map(Into::into),
        }
    }
}

#[derive(Clone)]
pub struct OperationRuntime {
    records: Arc<Mutex<HashMap<String, Arc<OperationRecord>>>>,
    event_capacity: usize,
}

impl OperationRuntime {
    pub fn new(event_capacity: usize) -> Self {
        Self {
            records: Arc::new(Mutex::new(HashMap::new())),
            event_capacity: event_capacity.max(2),
        }
    }

    pub fn submit<F>(
        &self,
        context: OperationContext,
        handler: F,
    ) -> model::ModelResult<crate::ResponseEnvelope>
    where
        F: FnOnce(OperationContext, RuntimeEventSink) -> ExecutionReport + Send + 'static,
    {
        let record = Arc::new(OperationRecord::new(self.event_capacity));
        self.records
            .lock()
            .expect("operation registry poisoned")
            .insert(context.operation_id.clone(), Arc::clone(&record));

        let accepted = ResponseBuilder::accepted(&context, &[]);
        thread::spawn(move || {
            let started_at_ms = now_ms();
            let sink = RuntimeEventSink {
                context: context.clone(),
                record: Arc::clone(&record),
            };
            sink.emit(
                crate::EventKind::OperationStarted,
                crate::Severity::Info,
                None,
                None,
                Some("operation started".to_owned()),
            );
            let report = handler(context.clone(), sink.clone());
            sink.emit(
                crate::EventKind::OperationFinished,
                crate::Severity::Info,
                None,
                None,
                Some("operation finished".to_owned()),
            );
            let result = ResponseBuilder::result(&context, &report, started_at_ms, now_ms());
            record.complete(result);
        });
        Ok(accepted)
    }

    pub fn subscribe(&self, operation_id: &str) -> model::ModelResult<EventSubscription> {
        Ok(EventSubscription {
            record: self.record(operation_id)?,
            next_sequence: 0,
        })
    }

    pub fn try_result(
        &self,
        operation_id: &str,
    ) -> model::ModelResult<Option<crate::OperationResult>> {
        let record = self.record(operation_id)?;
        Ok(record
            .state
            .lock()
            .expect("operation record poisoned")
            .result
            .clone())
    }

    pub fn wait(&self, operation_id: &str) -> model::ModelResult<crate::OperationResult> {
        let record = self.record(operation_id)?;
        let mut state = record.state.lock().expect("operation record poisoned");
        loop {
            if let Some(result) = &state.result {
                return Ok(result.clone());
            }
            state = record
                .complete
                .wait(state)
                .expect("operation record poisoned");
        }
    }

    fn record(&self, operation_id: &str) -> model::ModelResult<Arc<OperationRecord>> {
        self.records
            .lock()
            .expect("operation registry poisoned")
            .get(operation_id)
            .cloned()
            .ok_or_else(|| {
                model::ModelError::new(
                    model::ErrorCode::OperationNotFound,
                    format!("operation {operation_id} not found"),
                )
            })
    }
}

/// Delivery seam for operation events: an implementation decides what to do
/// with each event (buffer it, stream it as JSONL, render progress, drop it).
/// Handlers stay producers; consumers plug in here without the thread runtime.
pub trait EventSink: Send + Sync {
    fn deliver(&self, event: crate::OperationEvent);
}

/// Discards every event. Default for callers that do not consume events.
pub struct NullSink;

impl EventSink for NullSink {
    fn deliver(&self, _event: crate::OperationEvent) {}
}

/// Builds protocol `OperationEvent`s (envelope + monotonic sequence) and
/// forwards them to a sink. A handler holds one per operation and emits as it
/// works; the sink decides how to consume them.
pub struct EventEmitter<'a> {
    operation_id: String,
    request_id: String,
    attribution: Option<crate::OperationAttribution>,
    sequence: AtomicI64,
    /// Minimum ms between member_progress events per member; 0 = no limit.
    progress_min_interval_ms: i64,
    last_progress_ms: Mutex<HashMap<String, i64>>,
    sink: &'a dyn EventSink,
}

impl<'a> EventEmitter<'a> {
    pub fn new(
        context: &OperationContext,
        sink: &'a dyn EventSink,
        progress_min_interval_ms: i64,
    ) -> Self {
        Self {
            operation_id: context.operation_id.clone(),
            request_id: context.request_id.clone(),
            attribution: context.attribution.as_ref().map(Into::into),
            sequence: AtomicI64::new(0),
            progress_min_interval_ms: progress_min_interval_ms.max(0),
            last_progress_ms: Mutex::new(HashMap::new()),
            sink,
        }
    }

    fn emit(
        &self,
        kind: crate::EventKind,
        severity: crate::Severity,
        member_id: Option<String>,
        member_path: Option<String>,
        message: Option<String>,
        progress: Option<crate::GitTransferProgress>,
    ) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        self.sink.deliver(crate::OperationEvent {
            operation_id: self.operation_id.clone(),
            request_id: self.request_id.clone(),
            sequence,
            timestamp_ms: now_ms().0,
            kind,
            severity,
            member_id,
            member_path,
            message,
            member: None,
            error: None,
            attribution: self.attribution.clone(),
            progress,
        });
    }

    pub fn operation_started(&self) {
        self.emit(
            crate::EventKind::OperationStarted,
            crate::Severity::Info,
            None,
            None,
            Some("operation started".to_owned()),
            None,
        );
    }

    pub fn operation_finished(&self) {
        self.emit(
            crate::EventKind::OperationFinished,
            crate::Severity::Info,
            None,
            None,
            Some("operation finished".to_owned()),
            None,
        );
    }

    pub fn member_started(&self, member_id: &str, member_path: &str) {
        self.emit(
            crate::EventKind::MemberStarted,
            crate::Severity::Info,
            Some(member_id.to_owned()),
            Some(member_path.to_owned()),
            None,
            None,
        );
    }

    pub fn member_progress(
        &self,
        member_id: &str,
        member_path: &str,
        progress: crate::GitTransferProgress,
    ) {
        if !self.should_emit_progress(member_path) {
            return;
        }
        self.emit(
            crate::EventKind::MemberProgress,
            crate::Severity::Info,
            Some(member_id.to_owned()),
            Some(member_path.to_owned()),
            None,
            Some(progress),
        );
    }

    /// Rate-limits per-member progress to one event per
    /// `progress_min_interval_ms`. The first update for a member always passes,
    /// so a fast member still reports at least once.
    fn should_emit_progress(&self, member_path: &str) -> bool {
        if self.progress_min_interval_ms == 0 {
            return true;
        }
        let now = now_ms().0;
        let mut last = self.last_progress_ms.lock().expect("progress map poisoned");
        match last.get(member_path) {
            Some(&prev) if now - prev < self.progress_min_interval_ms => false,
            _ => {
                last.insert(member_path.to_owned(), now);
                true
            }
        }
    }

    pub fn member_finished(&self, member_id: &str, member_path: &str) {
        self.emit(
            crate::EventKind::MemberFinished,
            crate::Severity::Info,
            Some(member_id.to_owned()),
            Some(member_path.to_owned()),
            None,
            None,
        );
    }
}

/// Default global ceiling on concurrent member network operations (`--jobs`).
pub const DEFAULT_JOBS: usize = 50;
/// Default maximum concurrent connections to any one host.
pub const DEFAULT_MAX_PER_HOST: usize = 8;

/// Global ceiling on concurrent member operations: the driver's `--jobs` value
/// when valid, otherwise [`DEFAULT_JOBS`].
pub fn resolve_jobs(requested: Option<i64>) -> usize {
    match requested {
        Some(jobs) if jobs >= 1 => jobs as usize,
        _ => DEFAULT_JOBS,
    }
}

/// Maximum concurrent operations against any one host: the driver's value when
/// valid, otherwise [`DEFAULT_MAX_PER_HOST`].
pub fn resolve_per_host(requested: Option<i64>) -> usize {
    match requested {
        Some(limit) if limit >= 1 => limit as usize,
        _ => DEFAULT_MAX_PER_HOST,
    }
}

/// A counting semaphore over a fixed permit budget; used as the global ceiling.
struct Semaphore {
    permits: Mutex<usize>,
    available: Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits.max(1)),
            available: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut permits = self.permits.lock().expect("semaphore poisoned");
        while *permits == 0 {
            permits = self.available.wait(permits).expect("semaphore poisoned");
        }
        *permits -= 1;
    }

    fn release(&self) {
        *self.permits.lock().expect("semaphore poisoned") += 1;
        self.available.notify_one();
    }
}

/// Applies `f` to each item across scoped worker threads, bounding concurrency
/// both globally (`global_limit`, the `--jobs` ceiling) and per host
/// (`per_host_limit`). Items are keyed by `host_of`; `None` means no parseable
/// host (local) and is bounded only by the global ceiling. Different hosts run
/// concurrently, and results preserve input order. `f` runs on workers, so
/// anything it borrows (sink, backend, workspace root) must be `Sync`.
pub fn par_map_per_host<T, R, K, F>(
    items: Vec<T>,
    global_limit: usize,
    per_host_limit: usize,
    host_of: K,
    f: F,
) -> Vec<R>
where
    T: Send,
    R: Send,
    K: Fn(&T) -> Option<String>,
    F: Fn(T) -> R + Sync,
{
    let count = items.len();
    if count == 0 {
        return Vec::new();
    }
    // Group input indices by host (None = local).
    let mut groups: HashMap<Option<String>, Vec<usize>> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        groups.entry(host_of(item)).or_default().push(index);
    }
    let group_list: Vec<(Option<String>, Vec<usize>)> = groups.into_iter().collect();
    let cursors: Vec<AtomicUsize> = (0..group_list.len()).map(|_| AtomicUsize::new(0)).collect();
    let slots: Vec<Mutex<Option<T>>> = items
        .into_iter()
        .map(|item| Mutex::new(Some(item)))
        .collect();
    let results: Vec<Mutex<Option<R>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let global = Semaphore::new(global_limit);
    let f = &f;

    std::thread::scope(|scope| {
        for (group_index, (host, indices)) in group_list.iter().enumerate() {
            // A hosted group runs at most `per_host_limit` at once; local items
            // are bounded only by the global ceiling.
            let group_limit = if host.is_some() {
                per_host_limit.max(1)
            } else {
                global_limit.max(1)
            };
            for _ in 0..group_limit.min(indices.len()) {
                let cursor = &cursors[group_index];
                let indices = indices.as_slice();
                let slots = &slots;
                let results = &results;
                let global = &global;
                scope.spawn(move || {
                    loop {
                        let position = cursor.fetch_add(1, Ordering::Relaxed);
                        if position >= indices.len() {
                            break;
                        }
                        let index = indices[position];
                        let item = slots[index]
                            .lock()
                            .expect("par_map slot poisoned")
                            .take()
                            .expect("each item is taken once");
                        global.acquire();
                        let result = f(item);
                        global.release();
                        *results[index].lock().expect("par_map result poisoned") = Some(result);
                    }
                });
            }
        }
    });

    results
        .into_iter()
        .map(|cell| {
            cell.into_inner()
                .expect("par_map result poisoned")
                .expect("every index produces a result")
        })
        .collect()
}

#[derive(Clone)]
pub struct RuntimeEventSink {
    context: OperationContext,
    record: Arc<OperationRecord>,
}

impl RuntimeEventSink {
    pub fn emit(
        &self,
        kind: crate::EventKind,
        severity: crate::Severity,
        member_id: Option<String>,
        member_path: Option<String>,
        message: Option<String>,
    ) {
        let mut state = self.record.state.lock().expect("operation record poisoned");
        push_event(&mut state, &self.context);
        let event = crate::OperationEvent {
            operation_id: self.context.operation_id.clone(),
            request_id: self.context.request_id.clone(),
            sequence: state.next_sequence,
            timestamp_ms: now_ms().0,
            kind,
            severity,
            member_id,
            member_path,
            message,
            member: None,
            error: None,
            attribution: self.context.attribution.as_ref().map(Into::into),
            progress: None,
        };
        state.next_sequence += 1;
        state.events.push_back(event);
    }
}

pub struct EventSubscription {
    record: Arc<OperationRecord>,
    next_sequence: i64,
}

impl EventSubscription {
    pub fn drain(&mut self) -> Vec<crate::OperationEvent> {
        let state = self.record.state.lock().expect("operation record poisoned");
        let events: Vec<_> = state
            .events
            .iter()
            .filter(|event| event.sequence >= self.next_sequence)
            .cloned()
            .collect();
        if let Some(last) = events.last() {
            self.next_sequence = last.sequence + 1;
        }
        events
    }
}

struct OperationRecord {
    state: Mutex<OperationState>,
    complete: Condvar,
}

impl OperationRecord {
    fn new(event_capacity: usize) -> Self {
        Self {
            state: Mutex::new(OperationState {
                events: VecDeque::with_capacity(event_capacity),
                event_capacity,
                next_sequence: 0,
                result: None,
            }),
            complete: Condvar::new(),
        }
    }

    fn complete(&self, result: crate::OperationResult) {
        let mut state = self.state.lock().expect("operation record poisoned");
        state.result = Some(result);
        self.complete.notify_all();
    }
}

struct OperationState {
    events: VecDeque<crate::OperationEvent>,
    event_capacity: usize,
    next_sequence: i64,
    result: Option<crate::OperationResult>,
}

fn push_event(state: &mut OperationState, context: &OperationContext) {
    if state.events.len() < state.event_capacity {
        return;
    }

    state.events.clear();
    let reset = crate::OperationEvent {
        operation_id: context.operation_id.clone(),
        request_id: context.request_id.clone(),
        sequence: state.next_sequence,
        timestamp_ms: now_ms().0,
        kind: crate::EventKind::Reset,
        severity: crate::Severity::Warn,
        member_id: None,
        member_path: None,
        message: Some("event buffer overflow; history incomplete".to_owned()),
        member: None,
        error: None,
        attribution: context.attribution.as_ref().map(Into::into),
        progress: None,
    };
    state.next_sequence += 1;
    state.events.push_back(reset);
}

fn now_ms() -> TimestampMs {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    TimestampMs(millis.min(i64::MAX as u128) as i64)
}

#[derive(Default)]
pub struct MemberLockManager {
    locked: Mutex<HashSet<String>>,
}

impl MemberLockManager {
    pub fn try_lock<'a>(&'a self, member_id: &model::MemberId) -> Option<MemberMutationGuard<'a>> {
        let mut locked = self.locked.lock().expect("member lock manager poisoned");
        if locked.insert(member_id.to_string()) {
            Some(MemberMutationGuard {
                manager: self,
                member_id: member_id.to_string(),
            })
        } else {
            None
        }
    }
}

pub struct MemberMutationGuard<'a> {
    manager: &'a MemberLockManager,
    member_id: String,
}

impl Drop for MemberMutationGuard<'_> {
    fn drop(&mut self) {
        self.manager
            .locked
            .lock()
            .expect("member lock manager poisoned")
            .remove(&self.member_id);
    }
}

fn aggregate_status(report: &ExecutionReport) -> crate::AggregateStatus {
    if report
        .members
        .iter()
        .any(|member| member.status == MemberExecutionStatus::Failed)
    {
        crate::AggregateStatus::Failed
    } else if !report.errors.is_empty()
        || report
            .members
            .iter()
            .any(|member| member.status == MemberExecutionStatus::Rejected)
    {
        crate::AggregateStatus::Rejected
    } else if report
        .members
        .iter()
        .all(|member| member.status == MemberExecutionStatus::Noop)
    {
        crate::AggregateStatus::Noop
    } else {
        crate::AggregateStatus::Ok
    }
}

fn member_plan_to_protocol(member: &MemberPlan) -> crate::MemberResponse {
    crate::MemberResponse {
        member_id: member
            .member_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        member_path: member.member_path.clone(),
        source_kind: member.source_kind.into(),
        status: crate::MemberStatus::Planned,
        error: None,
        planned: Some(crate::PlannedChange {
            action: member.action.into(),
            from_ref: None,
            to_ref: None,
            message: member.message.clone(),
        }),
        state: None,
        git_status: None,
        lock_match: None,
    }
}

fn member_execution_to_protocol(member: &MemberExecution) -> crate::MemberResponse {
    crate::MemberResponse {
        member_id: member
            .member_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        member_path: member.member_path.clone(),
        source_kind: member.source_kind.into(),
        status: member.status.into(),
        error: member.error.as_ref().map(operation_error_to_protocol),
        planned: None,
        state: None,
        git_status: None,
        lock_match: None,
    }
}

fn operation_error_to_protocol(error: &OperationError) -> crate::GwzError {
    crate::GwzError {
        code: error.code.into(),
        message: error.message.clone(),
        member_id: None,
        member_path: None,
        detail: None,
    }
}

fn attribution_from_protocol(
    value: &crate::OperationAttribution,
) -> model::ModelResult<model::OperationAttribution> {
    let attribution = model::OperationAttribution {
        actor: value.actor.as_ref().map(|actor| model::OperationActor {
            actor_id: actor.actor_id.clone(),
            display_name: actor.display_name.clone(),
            email: actor.email.clone(),
            authority: actor.authority.clone(),
        }),
        git_author: value.git_author.as_ref().map(git_identity_from_protocol),
        git_committer: value.git_committer.as_ref().map(git_identity_from_protocol),
        credential_ref: value.credential_ref.clone(),
    };
    attribution.validate()?;
    Ok(attribution)
}

fn git_identity_from_protocol(value: &crate::GitObjectIdentity) -> model::GitObjectIdentity {
    model::GitObjectIdentity {
        name: value.name.clone(),
        email: value.email.clone(),
        time_ms: value.time_ms.map(TimestampMs),
        timezone_offset_minutes: value.timezone_offset_minutes,
    }
}

impl From<ActionKind> for crate::ActionKind {
    fn from(value: ActionKind) -> Self {
        match value {
            ActionKind::CreateWorkspace => Self::CreateWorkspace,
            ActionKind::InitFromSources => Self::InitFromSources,
            ActionKind::AddExistingRepo => Self::AddExistingRepo,
            ActionKind::CreateRepo => Self::CreateRepo,
            ActionKind::Materialize => Self::Materialize,
            ActionKind::Status => Self::Status,
            ActionKind::Snapshot => Self::Snapshot,
            ActionKind::Tag => Self::Tag,
            ActionKind::PullHead => Self::PullHead,
            ActionKind::PullSnapshot => Self::PullSnapshot,
            ActionKind::Push => Self::Push,
        }
    }
}

impl From<PlannedAction> for crate::PlannedAction {
    fn from(value: PlannedAction) -> Self {
        match value {
            PlannedAction::Noop => Self::Noop,
            PlannedAction::Clone => Self::Clone,
            PlannedAction::Fetch => Self::Fetch,
            PlannedAction::FastForward => Self::FastForward,
            PlannedAction::Checkout => Self::Checkout,
            PlannedAction::InitRepo => Self::InitRepo,
            PlannedAction::AddManifestMember => Self::AddManifestMember,
            PlannedAction::WriteManifest => Self::WriteManifest,
            PlannedAction::WriteLock => Self::WriteLock,
            PlannedAction::WriteSnapshot => Self::WriteSnapshot,
            PlannedAction::WriteTag => Self::WriteTag,
            PlannedAction::Push => Self::Push,
        }
    }
}

impl From<MemberExecutionStatus> for crate::MemberStatus {
    fn from(value: MemberExecutionStatus) -> Self {
        match value {
            MemberExecutionStatus::Ok => Self::Ok,
            MemberExecutionStatus::Noop => Self::Noop,
            MemberExecutionStatus::Skipped => Self::Skipped,
            MemberExecutionStatus::Rejected => Self::Rejected,
            MemberExecutionStatus::Failed => Self::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::{
        GitObjectIdentity, MemberId, OperationActor, OperationAttribution, SourceKind,
    };
    use crate::runtime::clock::TimestampMs;

    use super::*;

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<crate::OperationEvent>>,
    }

    impl EventSink for CollectingSink {
        fn deliver(&self, event: crate::OperationEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl CollectingSink {
        fn take(&self) -> Vec<crate::OperationEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    fn sample_progress() -> crate::GitTransferProgress {
        crate::GitTransferProgress {
            phase: crate::GitProgressPhase::Receiving,
            received_objects: Some(1),
            total_objects: Some(10),
            received_bytes: None,
            indexed_deltas: None,
            total_deltas: None,
        }
    }

    fn progress_event_count(events: &[crate::OperationEvent]) -> usize {
        events
            .iter()
            .filter(|event| event.kind == crate::EventKind::MemberProgress)
            .count()
    }

    #[test]
    fn member_progress_rate_limit_coalesces_per_member() {
        let context = sample_context(false);
        let sink = CollectingSink::default();
        // A 10s window: rapid successive updates fall inside it and coalesce.
        let emitter = EventEmitter::new(&context, &sink, 10_000);

        emitter.member_progress("mem_a", "repos/a", sample_progress()); // first: emits
        emitter.member_progress("mem_a", "repos/a", sample_progress()); // coalesced
        emitter.member_progress("mem_a", "repos/a", sample_progress()); // coalesced
        emitter.member_progress("mem_b", "repos/b", sample_progress()); // other member: emits

        // One per member (the first update each), the rest within the window dropped.
        assert_eq!(progress_event_count(&sink.take()), 2);
    }

    #[test]
    fn member_progress_unlimited_when_interval_zero() {
        let context = sample_context(false);
        let sink = CollectingSink::default();
        let emitter = EventEmitter::new(&context, &sink, 0);

        for _ in 0..5 {
            emitter.member_progress("mem_a", "repos/a", sample_progress());
        }

        assert_eq!(progress_event_count(&sink.take()), 5);
    }

    fn run_tracking_peak<K>(global: usize, per_host: usize, host_of: K) -> usize
    where
        K: Fn(&usize) -> Option<String>,
    {
        let active = AtomicUsize::new(0);
        let max_active = AtomicUsize::new(0);
        let results = par_map_per_host(
            (0..8).collect(),
            global,
            per_host,
            host_of,
            |value: usize| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(10));
                active.fetch_sub(1, Ordering::SeqCst);
                value * 10
            },
        );
        assert_eq!(results, (0..8).map(|value| value * 10).collect::<Vec<_>>());
        max_active.load(Ordering::SeqCst)
    }

    #[test]
    fn par_map_per_host_caps_concurrency_per_host() {
        // One host, per-host 2: capped at 2 despite a high global ceiling.
        let peak = run_tracking_peak(50, 2, |_| Some("h".to_owned()));
        assert_eq!(peak, 2, "single host should run exactly per_host=2 at once");
    }

    #[test]
    fn par_map_per_host_overlaps_distinct_hosts() {
        // Two hosts, per-host 1: each host serialized, but the two overlap.
        let peak = run_tracking_peak(50, 1, |value| {
            Some(if value % 2 == 0 { "a" } else { "b" }.to_owned())
        });
        assert_eq!(peak, 2, "two hosts at per_host=1 should overlap to 2");
        assert_eq!(
            par_map_per_host(Vec::<usize>::new(), 4, 8, |_| None, |value| value),
            Vec::new()
        );
    }

    #[test]
    fn par_map_per_host_bounds_hostless_items_by_global_only() {
        // No host: bounded only by the global ceiling.
        let peak = run_tracking_peak(3, 1, |_| None);
        assert_eq!(peak, 3, "hostless items ignore per_host, use global=3");
    }

    #[test]
    fn event_emitter_sequences_events_and_carries_progress() {
        let context = sample_context(false);
        let sink = CollectingSink::default();
        let emitter = EventEmitter::new(&context, &sink, 0);

        emitter.operation_started();
        emitter.member_started("mem_app", "repos/app");
        emitter.member_progress(
            "mem_app",
            "repos/app",
            crate::GitTransferProgress {
                phase: crate::GitProgressPhase::Receiving,
                received_objects: Some(5),
                total_objects: Some(10),
                received_bytes: Some(1024),
                indexed_deltas: None,
                total_deltas: None,
            },
        );
        emitter.member_finished("mem_app", "repos/app");
        emitter.operation_finished();

        let events = sink.take();
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec![
                crate::EventKind::OperationStarted,
                crate::EventKind::MemberStarted,
                crate::EventKind::MemberProgress,
                crate::EventKind::MemberFinished,
                crate::EventKind::OperationFinished,
            ]
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert_eq!(events[0].operation_id, "op_0001");
        assert_eq!(events[1].member_path.as_deref(), Some("repos/app"));
        let progress = events[2].progress.as_ref().expect("progress carried");
        assert_eq!(progress.phase, crate::GitProgressPhase::Receiving);
        assert_eq!(progress.received_objects, Some(5));
        assert!(events[3].progress.is_none());
    }

    #[test]
    fn dry_run_plan_reports_member_plans_without_execution() {
        let context = sample_context(true);
        let plan = OperationPlan {
            operation_id: context.operation_id.clone(),
            action: ActionKind::Status,
            dry_run: context.dry_run,
            members: vec![MemberPlan {
                member_id: Some(MemberId::parse_str("mem_01").unwrap()),
                member_path: "repos/example".to_owned(),
                source_kind: SourceKind::Git,
                action: PlannedAction::Noop,
                requires_mutation: false,
                message: Some("status only".to_owned()),
            }],
        };

        assert!(plan.dry_run);
        assert!(!plan.requires_mutation());
        assert_eq!(plan.members[0].action, PlannedAction::Noop);
    }

    #[test]
    fn accepted_response_carries_operation_id_and_attribution() {
        let context = sample_context(false);
        let response = ResponseBuilder::accepted(&context, &[]);

        assert_eq!(response.meta.operation_id.as_deref(), Some("op_0001"));
        assert_eq!(response.meta.request_id, "req-1");
        assert_eq!(response.meta.action, crate::ActionKind::Status);
        assert_eq!(
            response.meta.aggregate_status,
            crate::AggregateStatus::Accepted
        );
        assert_eq!(
            response
                .meta
                .attribution
                .as_ref()
                .and_then(|value| value.actor.as_ref())
                .map(|actor| actor.actor_id.as_str()),
            Some("agent://local/session")
        );
    }

    #[test]
    fn execution_report_assembles_final_operation_result() {
        let context = sample_context(false);
        let report = ExecutionReport {
            members: vec![MemberExecution {
                member_id: Some(MemberId::parse_str("mem_01").unwrap()),
                member_path: "repos/example".to_owned(),
                source_kind: SourceKind::Git,
                status: MemberExecutionStatus::Rejected,
                error: Some(OperationError::new(
                    crate::model::ErrorCode::DivergedMember,
                    "member diverged",
                )),
            }],
            errors: vec![OperationError::new(
                crate::model::ErrorCode::DivergedMember,
                "member diverged",
            )],
        };

        let result = ResponseBuilder::result(&context, &report, TimestampMs(10), TimestampMs(20));

        assert_eq!(result.operation_id, "op_0001");
        assert_eq!(result.aggregate_status, crate::AggregateStatus::Rejected);
        assert_eq!(result.members[0].status, crate::MemberStatus::Rejected);
        assert_eq!(result.errors[0].code, crate::GwzErrorCode::DivergedMember);
        assert_eq!(
            result
                .attribution
                .as_ref()
                .and_then(|value| value.git_committer.as_ref())
                .map(|identity| identity.email.as_str()),
            Some("bot@example.invalid")
        );
    }

    #[test]
    fn dispatch_context_preserves_status_request_meta() {
        let request = crate::StatusRequest {
            meta: crate::RequestMeta {
                request_id: "req-1".to_owned(),
                schema_version: "gwz.v0".to_owned(),
                dry_run: Some(true),
                attribution: Some(crate::OperationAttribution::from(&sample_attribution())),
                ..crate::RequestMeta::default()
            },
            ..Default::default()
        };

        let context = OperationRequest::Status(request)
            .context("op_0001")
            .expect("status context");

        assert_eq!(context.action, ActionKind::Status);
        assert_eq!(context.operation_id, "op_0001");
        assert_eq!(context.request_id, "req-1");
        assert!(context.dry_run);
        assert_eq!(
            context
                .attribution
                .as_ref()
                .unwrap()
                .actor
                .as_ref()
                .unwrap()
                .actor_id,
            "agent://local/session"
        );
    }

    #[test]
    fn submit_returns_accepted_before_handler_finishes() {
        let runtime = OperationRuntime::new(8);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let response = runtime
            .submit(sample_context(false), move |_context, _sink| {
                release_rx.recv().unwrap();
                ExecutionReport::default()
            })
            .unwrap();

        assert_eq!(
            response.meta.aggregate_status,
            crate::AggregateStatus::Accepted
        );
        assert_eq!(response.meta.operation_id.as_deref(), Some("op_0001"));
        assert!(runtime.try_result("op_0001").unwrap().is_none());

        release_tx.send(()).unwrap();
        assert_eq!(
            runtime.wait("op_0001").unwrap().aggregate_status,
            crate::AggregateStatus::Noop
        );
    }

    #[test]
    fn subscriber_receives_events_and_wait_does_not_require_drain() {
        let runtime = OperationRuntime::new(8);
        runtime
            .submit(sample_context(false), |_context, sink| {
                sink.emit(
                    crate::EventKind::MemberProgress,
                    crate::Severity::Info,
                    Some("mem_01".to_owned()),
                    Some("repos/example".to_owned()),
                    Some("checking status".to_owned()),
                );
                ExecutionReport::default()
            })
            .unwrap();
        let mut subscription = runtime.subscribe("op_0001").unwrap();

        let result = runtime.wait("op_0001").unwrap();
        let events = subscription.drain();

        assert_eq!(result.aggregate_status, crate::AggregateStatus::Noop);
        assert!(
            events
                .iter()
                .any(|event| event.kind == crate::EventKind::OperationStarted)
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == crate::EventKind::MemberProgress)
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == crate::EventKind::OperationFinished)
        );
        assert_eq!(
            events
                .first()
                .and_then(|event| event.attribution.as_ref())
                .and_then(|attribution| attribution.actor.as_ref())
                .map(|actor| actor.actor_id.as_str()),
            Some("agent://local/session")
        );
    }

    #[test]
    fn event_buffer_overflow_emits_reset_and_preserves_result() {
        let runtime = OperationRuntime::new(3);
        runtime
            .submit(sample_context(false), |_context, sink| {
                for index in 0..10 {
                    sink.emit(
                        crate::EventKind::MemberProgress,
                        crate::Severity::Info,
                        Some("mem_01".to_owned()),
                        Some("repos/example".to_owned()),
                        Some(format!("event {index}")),
                    );
                }
                ExecutionReport::default()
            })
            .unwrap();
        let mut subscription = runtime.subscribe("op_0001").unwrap();

        let result = runtime.wait("op_0001").unwrap();
        let events = subscription.drain();

        assert_eq!(result.operation_id, "op_0001");
        assert!(
            events
                .iter()
                .any(|event| event.kind == crate::EventKind::Reset)
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == crate::EventKind::OperationFinished)
        );
    }

    #[test]
    fn event_sequence_numbers_are_monotonic() {
        let runtime = OperationRuntime::new(16);
        runtime
            .submit(sample_context(false), |_context, sink| {
                sink.emit(
                    crate::EventKind::MemberStarted,
                    crate::Severity::Info,
                    None,
                    None,
                    None,
                );
                sink.emit(
                    crate::EventKind::MemberFinished,
                    crate::Severity::Info,
                    None,
                    None,
                    None,
                );
                ExecutionReport::default()
            })
            .unwrap();
        let mut subscription = runtime.subscribe("op_0001").unwrap();

        runtime.wait("op_0001").unwrap();
        let events = subscription.drain();

        assert!(
            events
                .windows(2)
                .all(|window| window[0].sequence < window[1].sequence)
        );
    }

    #[test]
    fn member_lock_manager_serializes_mutating_member_access() {
        let locks = MemberLockManager::default();
        let member_id = MemberId::parse_str("mem_01").unwrap();
        let first = locks.try_lock(&member_id).expect("first lock");

        assert!(locks.try_lock(&member_id).is_none());
        drop(first);
        assert!(locks.try_lock(&member_id).is_some());
    }

    fn sample_context(dry_run: bool) -> OperationContext {
        OperationContext {
            operation_id: "op_0001".to_owned(),
            request_id: "req-1".to_owned(),
            schema_version: "gwz.v0".to_owned(),
            action: ActionKind::Status,
            dry_run,
            attribution: Some(sample_attribution()),
        }
    }

    fn sample_attribution() -> OperationAttribution {
        OperationAttribution {
            actor: Some(OperationActor::new("agent://local/session")),
            git_author: Some(GitObjectIdentity::new("Agent", "agent@example.invalid")),
            git_committer: Some(GitObjectIdentity::new("Bot", "bot@example.invalid")),
            credential_ref: Some("cred:test".to_owned()),
        }
    }
}
