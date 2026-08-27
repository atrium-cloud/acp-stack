use super::*;

pub(super) struct HostedInitManager {
    pub(super) active: Mutex<Option<Arc<HostedInitSession>>>,
    pub(super) shutdown: Arc<Notify>,
    activity: Mutex<tokio::time::Instant>,
    shutdown_reason: Mutex<Option<&'static str>>,
    /// The serve process's shared store handle, handed to every session's
    /// wizard thread so its mutations stay visible to the deposit route.
    secret_store: SharedSecretStore,
}

pub(super) enum StartSessionError {
    Active,
}

impl HostedInitManager {
    pub(super) fn new(secret_store: SharedSecretStore) -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(None),
            shutdown: Arc::new(Notify::new()),
            activity: Mutex::new(tokio::time::Instant::now()),
            shutdown_reason: Mutex::new(None),
            secret_store,
        })
    }

    pub(super) fn start_session(
        self: &Arc<Self>,
        init_args: InitArgs,
    ) -> std::result::Result<StartInitResponse, StartSessionError> {
        let mut active = lock_unpoisoned(&self.active);
        if let Some(session) = active.as_ref()
            && session.is_active()
        {
            return Err(StartSessionError::Active);
        }

        let session = HostedInitSession::new(
            next_bootstrap_session_id(),
            self.shutdown.clone(),
            init_args.defer_provider_credentials,
        );
        let response = StartInitResponse {
            session_id: session.id.clone(),
            status: session.status(),
        };
        *active = Some(session.clone());
        let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver {
            session: session.clone(),
        });
        let secret_store = self.secret_store.clone();
        std::thread::spawn(move || {
            // Backstop for a panic OUTSIDE any recorded step body, so a panic can
            // never leave the session `running` until a reaper cancels it. The
            // `init_runs` row stays non-terminal here (the `StateStore` died with
            // the `InitFlow`); `acps init --resume` recovers it.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                prompt::with_hosted_driver(driver, || {
                    run_hosted_init(init_args, InitMode::Operator, secret_store)
                })
            }));
            let result = outcome.unwrap_or_else(|payload| {
                Err(StackError::InitStepPanicked {
                    kind: "init".to_owned(),
                    message: crate::runtime::init_runner::panic_payload_message(payload.as_ref()),
                })
            });
            if let Err(error) = result {
                if !session.has_result() {
                    session.set_error(error.error_code(), error.public_message());
                } else {
                    // A result was already published, so the run error would
                    // otherwise vanish here.
                    tracing::error!(
                        error = %error,
                        "hosted init failed after a result was already published; \
                         see the durable init_steps/init_runs rows for the settled state",
                    );
                }
            }
        });
        Ok(response)
    }

    /// Look a session up by id. Deliberately does not count as client activity;
    /// routes that represent activity touch the session explicitly.
    pub(super) fn session(&self, id: &str) -> Option<Arc<HostedInitSession>> {
        lock_unpoisoned(&self.active)
            .as_ref()
            .filter(|session| session.id == id)
            .cloned()
    }

    /// Record API activity on the server-level idle clock, which governs only
    /// the pre-session window.
    pub(super) fn touch_activity(&self) {
        *lock_unpoisoned(&self.activity) = tokio::time::Instant::now();
    }

    pub(super) fn activity_age(&self) -> std::time::Duration {
        lock_unpoisoned(&self.activity).elapsed()
    }

    pub(super) async fn wait_for_terminal(&self) {
        self.shutdown.notified().await;
    }

    /// Shut the server down without a session transition. The reason is recorded
    /// so `terminal_result()` exits non-zero on a timed-out bootstrap.
    pub(super) fn initiate_shutdown(&self, reason: &'static str) {
        *lock_unpoisoned(&self.shutdown_reason) = Some(reason);
        self.shutdown.notify_one();
    }

    /// `initiate_shutdown` for the idle reaper: the no-session check and the
    /// shutdown fire under one lock so a session created in between is not
    /// silently dropped. Returns false when a session already exists.
    pub(super) fn shutdown_if_no_session(&self, reason: &'static str) -> bool {
        let active = lock_unpoisoned(&self.active);
        if active.is_some() {
            return false;
        }
        self.initiate_shutdown(reason);
        true
    }

    pub(super) fn terminal_result(&self) -> Result<()> {
        let Some(session) = self.session_current() else {
            let reason = *lock_unpoisoned(&self.shutdown_reason);
            return match reason {
                Some(reason) => Err(StackError::InvalidParam {
                    field: "init",
                    reason: format!(
                        "hosted init server shut down before any session completed: {reason}"
                    ),
                }),
                None => Ok(()),
            };
        };
        match session.status().as_str() {
            "cancelled" => Err(StackError::InvalidParam {
                field: "init",
                reason: "hosted init session was cancelled".to_owned(),
            }),
            "errored" => {
                let snapshot = session.status_snapshot();
                let reason = snapshot
                    .error
                    .map(|error| format!("{}: {}", error.code, error.message))
                    .unwrap_or_else(|| "hosted init session failed".to_owned());
                Err(StackError::InvalidParam {
                    field: "init",
                    reason,
                })
            }
            _ => Ok(()),
        }
    }

    pub(super) fn session_current(&self) -> Option<Arc<HostedInitSession>> {
        lock_unpoisoned(&self.active).as_ref().cloned()
    }
}

/// One client answer: the value plus the `deferred` sibling flag, which only a
/// confirm prompt distinguishing decline from run-it-later reads.
#[derive(Debug, Clone)]
pub(super) struct HostedAnswer {
    pub(super) value: Value,
    pub(super) deferred: bool,
}

#[cfg(test)]
impl HostedAnswer {
    pub(super) fn plain(value: Value) -> Self {
        Self {
            value,
            deferred: false,
        }
    }
}

pub(super) struct HostedInitSession {
    pub(super) id: String,
    pub(super) inner: Mutex<SessionInner>,
    input_ready: Condvar,
    events: broadcast::Sender<String>,
    shutdown: Arc<Notify>,
    activity: Mutex<tokio::time::Instant>,
    connected_ws: std::sync::atomic::AtomicUsize,
    defer_provider_credentials: bool,
}

pub(super) struct SessionInner {
    status: String,
    next_seq: u64,
    history: Vec<Value>,
    pub(super) pending_input: Option<PublicInputRequest>,
    pending_response: Option<(String, HostedAnswer)>,
    current_step: Option<&'static str>,
    /// Whether `current_step` is still running. A failure between steps belongs
    /// to no lane and must not badge the category the last step settled.
    step_in_flight: bool,
    /// Every `signal` event so far, in order, for the `hello`/status replay.
    signal_log: Vec<Value>,
    result_json: Option<String>,
    error: Option<PublicError>,
    error_acked: bool,
    errored_at: Option<tokio::time::Instant>,
}

impl HostedInitSession {
    pub(super) fn new(
        id: String,
        shutdown: Arc<Notify>,
        defer_provider_credentials: bool,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(INIT_WS_CHANNEL_CAPACITY);
        let session = Arc::new(Self {
            id,
            inner: Mutex::new(SessionInner {
                status: "running".to_owned(),
                next_seq: 0,
                history: Vec::new(),
                pending_input: None,
                pending_response: None,
                current_step: None,
                step_in_flight: false,
                signal_log: Vec::new(),
                result_json: None,
                error: None,
                error_acked: false,
                errored_at: None,
            }),
            input_ready: Condvar::new(),
            events,
            shutdown,
            activity: Mutex::new(tokio::time::Instant::now()),
            connected_ws: std::sync::atomic::AtomicUsize::new(0),
            defer_provider_credentials,
        });
        session.push_event(ServerEvent::Progress {
            message: "init session started".to_owned(),
        });
        session
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    pub(super) fn status(&self) -> String {
        lock_unpoisoned(&self.inner).status.clone()
    }

    /// Whether the start request declared that provider credentials arrive
    /// out-of-band after init.
    pub(super) fn defer_provider_credentials(&self) -> bool {
        self.defer_provider_credentials
    }

    pub(super) fn is_active(&self) -> bool {
        let inner = lock_unpoisoned(&self.inner);
        match inner.status.as_str() {
            "running" | "waiting_for_input" | "completed_awaiting_ack" => true,
            // A parked failure keeps the session alive so the backend can replay
            // and acknowledge the typed error.
            "errored" => !inner.error_acked,
            _ => false,
        }
    }

    pub(super) fn touch(&self) {
        *lock_unpoisoned(&self.activity) = tokio::time::Instant::now();
    }

    pub(super) fn last_activity_age_secs(&self) -> u64 {
        lock_unpoisoned(&self.activity).elapsed().as_secs()
    }

    pub(super) fn ws_connected(&self) {
        self.connected_ws
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.touch();
    }

    pub(super) fn ws_disconnected(&self) {
        // `fetch_update` only errs when the counter was already 0, which the
        // ConnectionGuard pairing makes unreachable.
        self.connected_ws
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |count| count.checked_sub(1),
            )
            .ok();
        // The idle clock starts at disconnect, giving a dropped backend the full
        // idle timeout to reconnect and ack before the reaper expires it.
        self.touch();
    }

    pub(super) fn has_connected_ws(&self) -> bool {
        self.connected_ws.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    pub(super) fn status_snapshot(&self) -> InitStatusResponse {
        let inner = lock_unpoisoned(&self.inner);
        InitStatusResponse {
            session_id: self.id.clone(),
            status: inner.status.clone(),
            signals: inner.signal_log.clone(),
            last_seq: inner.next_seq,
            pending_input: inner.pending_input.clone(),
            recent_events: inner.history.iter().rev().take(50).cloned().collect(),
            result_available: inner.result_json.is_some(),
            error: inner.error.clone(),
            last_activity_age_secs: self.last_activity_age_secs(),
        }
    }

    pub(super) fn hello_frame(&self) -> String {
        let snapshot = self.status_snapshot();
        frame_json(ServerFrame::Hello {
            session_id: &self.id,
            status: &snapshot.status,
            signals: &snapshot.signals,
            last_seq: snapshot.last_seq,
            pending_input: snapshot.pending_input.as_ref(),
            result_available: snapshot.result_available,
            error: snapshot.error.as_ref(),
        })
    }

    pub(super) fn events_after(&self, after_seq: u64) -> Vec<Value> {
        lock_unpoisoned(&self.inner)
            .history
            .iter()
            .filter(|event| event.get("seq").and_then(Value::as_u64).unwrap_or(0) > after_seq)
            .cloned()
            .collect()
    }

    pub(super) fn push_event(&self, event: ServerEvent) {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            // Once terminal, the client has treated the terminal frame as the
            // last word; a wizard thread still running after cancel/expire must
            // not keep streaming progress past it.
            if is_terminal_status(&inner.status) {
                return;
            }
            self.emit_event_locked(&mut inner, event)
        };
        let _ = self.events.send(frame.to_string());
    }

    /// Record a typed event; a payload that will not encode parks the session as
    /// errored rather than leaving a client's fold behind the wizard.
    pub(super) fn emit_event_locked(&self, inner: &mut SessionInner, event: ServerEvent) -> Value {
        let event_type = event.type_str();
        match event.payload() {
            Ok(payload) => self.record_event_locked(inner, event_type, payload),
            Err(error) => {
                tracing::warn!(
                    event = event_type,
                    error = %error,
                    "hosted init event payload could not be encoded; parking the session as errored"
                );
                // Bypasses `set_error_locked`: the encode-failure park wants the
                // error frame alone, not a step-finish signal.
                self.record_error_locked(
                    inner,
                    FRAME_ENCODE_FAILED_CODE,
                    FRAME_ENCODE_FAILED_MESSAGE.to_owned(),
                )
            }
        }
    }

    /// Record a `signal` event and keep a copy for the hello/status replay.
    /// Deliberately undeduped so a late joiner folds the identical stream.
    fn emit_signal_locked(&self, inner: &mut SessionInner, payload: Map<String, Value>) -> Value {
        let frame = self.emit_event_locked(inner, ServerEvent::Signal(payload));
        inner.signal_log.push(frame.clone());
        frame
    }

    /// Forward one raw init state signal; the client folds the category view.
    pub(super) fn apply_state_signal(&self, signal: InitStateSignal) {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            // A cancel unwinds the wizard thread, which keeps emitting signals on
            // the way out; those must not extend the stream past the terminal
            // frame.
            if is_terminal_status(&inner.status) {
                return;
            }
            track_step(&mut inner, &signal);
            self.emit_signal_locked(&mut inner, signal.wire_payload())
        };
        let _ = self.events.send(frame.to_string());
    }

    fn record_event_locked(
        &self,
        inner: &mut SessionInner,
        event_type: &str,
        payload: Map<String, Value>,
    ) -> Value {
        inner.next_seq = inner.next_seq.saturating_add(1);
        let frame = envelope(event_type, inner.next_seq, &self.id, payload);
        inner.history.push(frame.clone());
        if inner.history.len() > INIT_EVENT_HISTORY_LIMIT {
            inner.history.remove(0);
        }
        frame
    }

    pub(super) fn request_input(
        &self,
        request: HostedPromptRequest,
    ) -> Result<Option<HostedAnswer>> {
        if !should_handle_hosted_prompt(&request) {
            return Ok(None);
        }
        let public = public_input_request(request);
        let input_frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            terminal_status_error(&inner)?;
            inner.status = "waiting_for_input".to_owned();
            inner.pending_response = None;
            inner.pending_input = Some(public.clone());
            self.emit_event_locked(
                &mut inner,
                ServerEvent::InputRequired {
                    input: Box::new(public.clone()),
                },
            )
        };
        let _ = self.events.send(input_frame.to_string());

        let answer = {
            let mut inner = lock_unpoisoned(&self.inner);
            loop {
                terminal_status_error(&inner)?;
                if let Some((request_id, answer)) = inner.pending_response.take()
                    && request_id == public.request_id
                {
                    inner.status = "running".to_owned();
                    inner.pending_input = None;
                    break answer;
                }
                inner = self
                    .input_ready
                    .wait(inner)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        Ok(Some(answer))
    }

    #[cfg(test)]
    pub(super) fn submit_input(
        &self,
        request_id: &str,
        value: Value,
    ) -> std::result::Result<(), String> {
        self.submit_answer(request_id, HostedAnswer::plain(value))
    }

    pub(super) fn submit_answer(
        &self,
        request_id: &str,
        answer: HostedAnswer,
    ) -> std::result::Result<(), String> {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            let Some(pending) = inner.pending_input.as_ref() else {
                return Err("no input request is pending".to_owned());
            };
            if pending.request_id != request_id {
                return Err(format!(
                    "stale request_id `{request_id}`; current request_id is `{}`",
                    pending.request_id
                ));
            }
            inner.pending_response = Some((request_id.to_owned(), answer));
            self.emit_event_locked(
                &mut inner,
                ServerEvent::InputAccepted {
                    request_id: request_id.to_owned(),
                },
            )
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        Ok(())
    }

    pub(super) fn set_result(&self, payload: Value) {
        let mut result_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned());
        {
            let mut inner = lock_unpoisoned(&self.inner);
            // Refuse ANY terminal status, not just cancelled/closed: letting a
            // late failed handoff overwrite an `errored` session would publish
            // `result_ready` after the error frame and flip `terminal_result`
            // from Err to Ok, exiting zero on a failed bootstrap.
            if is_terminal_status(&inner.status) {
                result_json.zeroize();
                return;
            }
            inner.status = "completed_awaiting_ack".to_owned();
            inner.result_json = Some(result_json);
            inner.pending_input = None;
            let frame = self.emit_event_locked(&mut inner, ServerEvent::ResultReady);
            drop(inner);
            let _ = self.events.send(frame.to_string());
        }
        if let Some(frame) = self.result_frame() {
            let _ = self.events.send(frame);
        }
        self.input_ready.notify_all();
    }

    pub(super) fn result_frame(&self) -> Option<String> {
        let inner = lock_unpoisoned(&self.inner);
        // Borrowed under the guard so the plaintext handoff is never copied into
        // an intermediate buffer.
        let result = inner.result_json.as_ref()?;
        match ResultFrame::new(&self.id, result).and_then(|frame| frame.to_json()) {
            Ok(frame) => Some(frame),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "hosted init result frame could not be encoded; sending an encode-failure frame instead"
                );
                Some(encode_failure_frame())
            }
        }
    }

    pub(super) fn has_result(&self) -> bool {
        lock_unpoisoned(&self.inner).result_json.is_some()
    }

    pub(super) fn ack_result(&self) -> std::result::Result<(), String> {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            let Some(mut result) = inner.result_json.take() else {
                return Err("no init result is awaiting acknowledgement".to_owned());
            };
            result.zeroize();
            inner.status = "closed".to_owned();
            inner.pending_input = None;
            self.emit_event_locked(&mut inner, ServerEvent::ResultAcked)
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
        Ok(())
    }

    pub(super) fn cancel(&self, reason: &str) {
        let Some(frame) = ({
            let mut inner = lock_unpoisoned(&self.inner);
            // A cancel racing a parked failure must not overwrite the typed
            // error; the backend releases one with `ack_error` instead.
            if is_terminal_status(&inner.status) {
                return;
            }
            inner.status = "cancelled".to_owned();
            inner.pending_input = None;
            inner.pending_response = None;
            Some(self.emit_event_locked(
                &mut inner,
                ServerEvent::Canceled {
                    reason: reason.to_owned(),
                },
            ))
        }) else {
            return;
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
    }

    /// Force the session terminal for the lifetime reapers. Unlike `cancel` this
    /// also fires from `completed_awaiting_ack`; the un-acked result carries
    /// plaintext handoff keys, so it is zeroized before the session closes.
    pub(super) fn expire(&self, reason: &str) {
        let Some(frame) = ({
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(inner.status.as_str(), "closed" | "cancelled") {
                return;
            }
            if inner.status == "errored" {
                // Mark acked and fire shutdown while KEEPING status `errored`, so
                // `terminal_result` reports the typed failure rather than a
                // generic cancellation.
                if inner.error_acked {
                    return;
                }
                inner.error_acked = true;
                let frame = self.emit_event_locked(
                    &mut inner,
                    ServerEvent::ErrorExpired {
                        reason: reason.to_owned(),
                    },
                );
                drop(inner);
                let _ = self.events.send(frame.to_string());
                self.input_ready.notify_all();
                self.shutdown.notify_one();
                return;
            }
            if let Some(mut result) = inner.result_json.take() {
                result.zeroize();
            }
            inner.status = "cancelled".to_owned();
            inner.pending_input = None;
            inner.pending_response = None;
            Some(self.emit_event_locked(
                &mut inner,
                ServerEvent::Canceled {
                    reason: reason.to_owned(),
                },
            ))
        }) else {
            return;
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
    }

    /// Record a failure and park the session. Deliberately does NOT notify
    /// shutdown: the `error` frame may have zero receivers, so the server stays
    /// up until `ack_error`, cancel, or the error-ack grace reaper.
    pub(super) fn set_error(&self, code: &str, message: String) {
        let frames = {
            let mut inner = lock_unpoisoned(&self.inner);
            self.set_error_locked(&mut inner, code, message)
        };
        for frame in frames {
            let _ = self.events.send(frame.to_string());
        }
    }

    /// Lock-held `set_error`. Returns frames in broadcast order: the running
    /// step's finish signal first, then the terminal `error` frame last.
    pub(super) fn set_error_locked(
        &self,
        inner: &mut SessionInner,
        code: &str,
        message: String,
    ) -> Vec<Value> {
        if is_terminal_status(&inner.status) {
            // Keep the first error; later ones are downstream of it.
            return Vec::new();
        }
        let mut frames = Vec::new();
        if let Some(step) = inner.current_step.filter(|_| inner.step_in_flight) {
            // Finish the running step rather than emitting a bare
            // `category_failed`, so the client's fold applies the same
            // don't-double-blame guards a normally-failing step gets.
            let payload = InitStateSignal::StepFinished {
                kind: step,
                disposition: StepDisposition::Executed,
                error_code: Some(code.to_owned()),
            }
            .wire_payload();
            frames.push(self.emit_signal_locked(inner, payload));
        }
        inner.pending_input = None;
        frames.push(self.record_error_locked(inner, code, message));
        frames
    }

    /// Park the session on a failure and record the `error` event. Records the
    /// event even when already terminal: the client is owed a contiguous history.
    fn record_error_locked(&self, inner: &mut SessionInner, code: &str, message: String) -> Value {
        if !is_terminal_status(&inner.status) {
            inner.status = "errored".to_owned();
            inner.pending_input = None;
            inner.errored_at = Some(tokio::time::Instant::now());
            inner.error = Some(PublicError {
                code: code.to_owned(),
                message: message.clone(),
            });
        }
        // Cannot re-enter through `emit_event_locked`: `ServerEvent::Error` has
        // an infallible payload, so that function's encode arm is unreachable.
        let frame = self.emit_event_locked(
            inner,
            ServerEvent::Error {
                code: code.to_owned(),
                message,
            },
        );
        // Fired under the lock so no failure path can park the session without
        // releasing a wizard thread blocked in `request_input`.
        self.input_ready.notify_all();
        frame
    }

    /// Seq-less replay of the stored failure, for a backend that reconnected
    /// after the original broadcast `error` frame was lost.
    pub(super) fn error_replay_frame(&self) -> Option<String> {
        let inner = lock_unpoisoned(&self.inner);
        let error = inner.error.as_ref()?;
        Some(frame_json(ServerFrame::ErrorReplay {
            session_id: &self.id,
            code: &error.code,
            message: &error.message,
        }))
    }

    pub(super) fn ack_error(&self) -> std::result::Result<(), String> {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            if inner.status == "errored" && inner.error_acked {
                return Err("init error was already acknowledged or expired".to_owned());
            }
            if inner.status != "errored" {
                return Err("no init error is awaiting acknowledgement".to_owned());
            }
            // Status stays `errored` so `terminal_result` still exits non-zero.
            inner.error_acked = true;
            self.emit_event_locked(&mut inner, ServerEvent::ErrorAcked)
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
        Ok(())
    }

    /// True while a failure is parked waiting for `ack_error`, with the time
    /// since it was recorded — the error-ack grace reaper's input.
    pub(super) fn unacked_error_age(&self) -> Option<std::time::Duration> {
        let inner = lock_unpoisoned(&self.inner);
        if inner.status == "errored" && !inner.error_acked {
            inner.errored_at.map(|instant| instant.elapsed())
        } else {
            None
        }
    }
}

/// Statuses after which nothing new may be said about the run: signal emission
/// and step tracking both stop here.
fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "cancelled" | "closed" | "errored" | "completed_awaiting_ack"
    )
}

/// Statuses a wizard thread waiting on a prompt must give up on, `errored`
/// included: no client will be asked to answer anything again.
fn terminal_status_error(inner: &SessionInner) -> Result<()> {
    match inner.status.as_str() {
        "cancelled" | "closed" => Err(StackError::InvalidParam {
            field: "init",
            reason: "hosted init session was cancelled".to_owned(),
        }),
        "errored" => Err(StackError::InvalidParam {
            field: "init",
            reason: inner.error.as_ref().map_or_else(
                || "hosted init session failed".to_owned(),
                |error| format!("{}: {}", error.code, error.message),
            ),
        }),
        _ => Ok(()),
    }
}

/// Track the running step so a failure that parks the session can badge its
/// lane. `current_step` deliberately survives a step's finish so
/// `step_in_flight` alone distinguishes a mid-step failure from a between-steps
/// one.
fn track_step(inner: &mut SessionInner, signal: &InitStateSignal) {
    match signal {
        InitStateSignal::StepStarted { kind } => {
            inner.current_step = Some(kind);
            inner.step_in_flight = true;
        }
        InitStateSignal::StepFinished { .. } => {
            inner.step_in_flight = false;
        }
        _ => {}
    }
}

fn next_bootstrap_session_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    format!("init_{nanos:020}_{sequence:010}_{pid:010}")
}
