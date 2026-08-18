use super::*;

pub(super) struct HostedInitManager {
    pub(super) active: Mutex<Option<Arc<HostedInitSession>>>,
    pub(super) shutdown: Arc<Notify>,
    activity: Mutex<tokio::time::Instant>,
    shutdown_reason: Mutex<Option<&'static str>>,
}

pub(super) enum StartSessionError {
    Active,
}

impl HostedInitManager {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(None),
            shutdown: Arc::new(Notify::new()),
            activity: Mutex::new(tokio::time::Instant::now()),
            shutdown_reason: Mutex::new(None),
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

        let session = HostedInitSession::new(next_bootstrap_session_id(), self.shutdown.clone());
        let response = StartInitResponse {
            session_id: session.id.clone(),
            status: session.status(),
        };
        *active = Some(session.clone());
        let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver {
            session: session.clone(),
        });
        std::thread::spawn(move || {
            let result = prompt::with_hosted_driver(driver, || {
                run_hosted_init(init_args, InitMode::Operator)
            });
            if let Err(error) = result
                && !session.has_result()
            {
                session.set_error(error.error_code(), error.public_message());
            }
        });
        Ok(response)
    }

    /// Look a session up by id. This does not count as client activity on its
    /// own: routes that represent activity touch the session explicitly, after
    /// reading anything time-sensitive, so a status poll can report how long
    /// the session was idle *before* that poll.
    pub(super) fn session(&self, id: &str) -> Option<Arc<HostedInitSession>> {
        lock_unpoisoned(&self.active)
            .as_ref()
            .filter(|session| session.id == id)
            .cloned()
    }

    /// Record authenticated API activity on the server-level idle clock. This
    /// clock only governs the pre-session window; once a session exists its
    /// own activity timestamp takes over.
    pub(super) fn touch_activity(&self) {
        *lock_unpoisoned(&self.activity) = tokio::time::Instant::now();
    }

    pub(super) fn activity_age(&self) -> std::time::Duration {
        lock_unpoisoned(&self.activity).elapsed()
    }

    pub(super) async fn wait_for_terminal(&self) {
        self.shutdown.notified().await;
    }

    /// Shut the server down without a session transition. Used by the
    /// lifetime reapers when the server idled out before any session was
    /// created. `Notify` retains one permit, so firing before
    /// `wait_for_terminal()` awaits is safe. The reason is recorded so
    /// `terminal_result()` can exit non-zero: an orchestrator must be able to
    /// tell a timed-out bootstrap apart from a successful one.
    pub(super) fn initiate_shutdown(&self, reason: &'static str) {
        *lock_unpoisoned(&self.shutdown_reason) = Some(reason);
        self.shutdown.notify_one();
    }

    /// `initiate_shutdown` variant for the idle reaper's pre-session branch:
    /// the no-session check and the shutdown fire atomically under the same
    /// lock so a session created between the reaper's check and the shutdown
    /// is not silently dropped. Returns false when a session already exists.
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
            "canceled" => Err(StackError::InvalidParam {
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

/// One client answer: the answer value plus the optional `deferred` sibling
/// flag from the input frame. The flag is meaningful only to a confirm prompt
/// whose caller distinguishes a decline from a backend-run-it-later answer;
/// every other prompt ignores it.
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
}

pub(super) struct SessionInner {
    status: String,
    next_seq: u64,
    history: Vec<Value>,
    pub(super) pending_input: Option<PublicInputRequest>,
    /// Identity of the prompt occupying `pending_input`, kept beside it so the
    /// category waiting on the client is derived rather than stored.
    pending_kind: Option<HostedPromptKind>,
    pending_response: Option<(String, HostedAnswer)>,
    current_step: Option<&'static str>,
    /// Whether `current_step` is still running. `current_step` itself stays put
    /// after a step finishes because the wire wants the last step the run was
    /// inside, but a failure surfacing between steps belongs to no lane and
    /// must not badge the settled category the previous step left behind.
    step_in_flight: bool,
    categories: CategoryMap,
    /// Last snapshot put on the wire. A signal that changes nothing observable
    /// must not burn a seq, so every emission compares against this first.
    last_state: Option<StateSnapshot>,
    result_json: Option<String>,
    error: Option<PublicError>,
    error_acked: bool,
    errored_at: Option<tokio::time::Instant>,
}

impl HostedInitSession {
    pub(super) fn new(id: String, shutdown: Arc<Notify>) -> Arc<Self> {
        let (events, _) = broadcast::channel(INIT_WS_CHANNEL_CAPACITY);
        // The starting snapshot seeds the dedup guard rather than being sent:
        // every client sees it in `hello`, so the first `state` event on the
        // wire is a real transition away from it.
        let categories = CategoryMap::default();
        let last_state = Some(derive_snapshot(&categories, None, None));
        let session = Arc::new(Self {
            id,
            inner: Mutex::new(SessionInner {
                status: "running".to_owned(),
                next_seq: 0,
                history: Vec::new(),
                pending_input: None,
                pending_kind: None,
                pending_response: None,
                current_step: None,
                step_in_flight: false,
                categories,
                last_state,
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

    pub(super) fn is_active(&self) -> bool {
        let inner = lock_unpoisoned(&self.inner);
        match inner.status.as_str() {
            "running" | "waiting_for_input" | "completed_awaiting_ack" => true,
            // A parked failure keeps the session (and its WebSocket) alive so
            // the backend can replay and acknowledge the typed error,
            // symmetric with `completed_awaiting_ack`.
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
        // ConnectionGuard pairing makes unreachable; the count stays correct
        // either way.
        self.connected_ws
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |count| count.checked_sub(1),
            )
            .ok();
        // The idle clock starts at disconnect: a listen-only backend whose
        // socket drops mid-init gets the full idle timeout to reconnect and
        // replay/ack the result before the reaper may expire the session.
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
            // Derived live rather than read from `last_state`: a REST poller
            // must see the current frontier even when the last transition
            // deduped to no frame.
            state: derive_snapshot(&inner.categories, inner.current_step, inner.pending_kind),
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
        // `error` rides along so a backend that reconnects after its socket
        // dropped mid-failure learns the typed error from the hello alone.
        frame_json(ServerFrame::Hello {
            session_id: &self.id,
            status: &snapshot.status,
            state: &snapshot.state,
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
            self.emit_event_locked(&mut inner, event)
        };
        let _ = self.events.send(frame.to_string());
    }

    /// Record a typed event, absorbing the single failure mode a frame has.
    /// A payload that will not encode parks the session as errored through the
    /// normal error path: the transition it described is lost, so continuing
    /// would leave the client's category map permanently behind the wizard.
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
                // Deliberately not `set_error_locked`: that derives a fresh
                // state snapshot, which is one of the two payloads that can
                // fail this way, and the retry would recurse.
                self.record_error_locked(
                    inner,
                    FRAME_ENCODE_FAILED_CODE,
                    FRAME_ENCODE_FAILED_MESSAGE.to_owned(),
                )
            }
        }
    }

    /// Derive the category snapshot and record it if it moved. Returns the
    /// frame to broadcast once the caller drops the lock, or `None` when the
    /// mutation changed nothing observable — dedup happens before any seq is
    /// allocated, so a no-op signal leaves the sequence untouched.
    pub(super) fn emit_state_locked(&self, inner: &mut SessionInner) -> Option<Value> {
        if is_terminal_status(&inner.status) {
            return None;
        }
        let snapshot = derive_snapshot(&inner.categories, inner.current_step, inner.pending_kind);
        if inner.last_state.as_ref() == Some(&snapshot) {
            return None;
        }
        let frame = self.emit_event_locked(inner, ServerEvent::State(snapshot.clone()));
        // A snapshot whose payload would not encode parks the session instead
        // of reaching the client, so it must not seed the dedup guard: the next
        // snapshot has to be treated as a real transition.
        if inner.status != "errored" {
            inner.last_state = Some(snapshot);
        }
        Some(frame)
    }

    /// Fold one init state signal into the category map and publish whatever
    /// it moved. Called from the wizard thread via the hosted prompt driver.
    pub(super) fn apply_state_signal(&self, signal: InitStateSignal) {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            // The frontier freezes with the session. A cancel while a prompt is
            // pending unwinds the wizard thread, which keeps emitting signals
            // on the way out; folding them would move the categories reported
            // by `hello` and the status route even though `emit_state_locked`
            // will not put another frame on the wire.
            if is_terminal_status(&inner.status) {
                return;
            }
            apply_state_signal_locked(&mut inner, signal);
            self.emit_state_locked(&mut inner)
        };
        if let Some(frame) = frame {
            let _ = self.events.send(frame.to_string());
        }
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
        let kind = request.kind;
        let public = public_input_request(request);
        let (input_frame, state_frame) = {
            let mut inner = lock_unpoisoned(&self.inner);
            terminal_status_error(&inner)?;
            inner.status = "waiting_for_input".to_owned();
            inner.pending_response = None;
            inner.pending_input = Some(public.clone());
            inner.pending_kind = Some(kind);
            // `input_required` is recorded first so the state frame announcing
            // `awaiting_input` never arrives before the prompt it refers to.
            let input_frame = self.emit_event_locked(
                &mut inner,
                ServerEvent::InputRequired {
                    input: Box::new(public.clone()),
                },
            );
            (input_frame, self.emit_state_locked(&mut inner))
        };
        let _ = self.events.send(input_frame.to_string());
        if let Some(frame) = state_frame {
            let _ = self.events.send(frame.to_string());
        }

        let (answer, state_frame) = {
            let mut inner = lock_unpoisoned(&self.inner);
            loop {
                terminal_status_error(&inner)?;
                if let Some((request_id, answer)) = inner.pending_response.take()
                    && request_id == public.request_id
                {
                    inner.status = "running".to_owned();
                    inner.pending_input = None;
                    inner.pending_kind = None;
                    // The answer itself settles nothing: settlement comes from
                    // the config-write sites, which know what was actually
                    // written and never carry a secret value.
                    break (answer, self.emit_state_locked(&mut inner));
                }
                inner = self
                    .input_ready
                    .wait(inner)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        if let Some(frame) = state_frame {
            let _ = self.events.send(frame.to_string());
        }
        Ok(Some(answer))
    }

    /// Answer shorthand for tests that do not exercise the `deferred` sibling.
    /// The wire path always goes through `submit_answer`.
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
            if matches!(inner.status.as_str(), "canceled" | "closed") {
                result_json.zeroize();
                return;
            }
            inner.status = "completed_awaiting_ack".to_owned();
            inner.result_json = Some(result_json);
            // `pending_kind` follows `pending_input` everywhere: they are two
            // halves of one slot, and a stale kind would keep deriving a
            // category as `awaiting_input` after the prompt is gone.
            inner.pending_input = None;
            inner.pending_kind = None;
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
        // The borrow lives entirely inside the guard: the frame points
        // straight at the stored result, so the plaintext handoff is never
        // copied into an intermediate buffer just to be serialized.
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
            inner.pending_kind = None;
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
            // `errored` is excluded like `completed_awaiting_ack`: a cancel
            // racing a parked failure must not overwrite the typed error the
            // backend is entitled to (`terminal_result` would report a
            // generic cancellation). The backend releases a parked failure
            // with `ack_error`; only the internal reapers may expire it.
            if is_terminal_status(&inner.status) {
                return;
            }
            inner.status = "canceled".to_owned();
            inner.pending_input = None;
            inner.pending_kind = None;
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

    /// Force the session terminal on behalf of the internal lifetime
    /// reapers. Unlike backend-driven `cancel`, this also fires from
    /// `completed_awaiting_ack`: an abandoned session holding an un-acked
    /// result must not pin the server forever. The un-acked result carries
    /// plaintext handoff keys, so it is zeroized before the session closes.
    pub(super) fn expire(&self, reason: &str) {
        let Some(frame) = ({
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(inner.status.as_str(), "closed" | "canceled") {
                return;
            }
            if inner.status == "errored" {
                // A parked failure the backend never acknowledged: mark it
                // acked and fire shutdown while KEEPING status `errored`, so
                // `terminal_result` reports the typed failure instead of a
                // generic cancellation. Without this branch the reaper would
                // stop without ever releasing the terminal waiter.
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
            inner.status = "canceled".to_owned();
            inner.pending_input = None;
            inner.pending_kind = None;
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

    /// Record a failure and park the session. Unlike `cancel`/`expire`, this
    /// does NOT notify shutdown: the broadcast `error` frame may have zero
    /// receivers (dropped socket, REST-polling backend), so the server stays
    /// up in `errored` until `ack_error`, cancel, or the error-ack grace
    /// reaper — symmetric with how `set_result` waits for `ack_result`.
    pub(super) fn set_error(&self, code: &str, message: String) {
        let frames = {
            let mut inner = lock_unpoisoned(&self.inner);
            self.set_error_locked(&mut inner, code, message)
        };
        for frame in frames {
            let _ = self.events.send(frame.to_string());
        }
    }

    /// Lock-held `set_error`. Returns the frames to broadcast in order: the
    /// state frame badging the failed category first, then the terminal
    /// `error` frame, which is the last thing a client should see for this
    /// transition.
    pub(super) fn set_error_locked(
        &self,
        inner: &mut SessionInner,
        code: &str,
        message: String,
    ) -> Vec<Value> {
        if is_terminal_status(&inner.status) {
            // A session already parked on a typed failure keeps the first
            // error: it is the one that explains what actually broke, and
            // whatever the wizard thread propagated afterwards is downstream
            // of it.
            return Vec::new();
        }
        let mut frames = Vec::new();
        if let Some(category) = inner
            .current_step
            .filter(|_| inner.step_in_flight)
            .and_then(category_for_step_kind)
        {
            inner
                .categories
                .fail_step_category(category, code.to_owned());
        }
        inner.pending_input = None;
        inner.pending_kind = None;
        if let Some(frame) = self.emit_state_locked(inner) {
            frames.push(frame);
        }
        frames.push(self.record_error_locked(inner, code, message));
        frames
    }

    /// Park the session on a failure and record the `error` event. Split out
    /// so the frame-encode path can park without deriving a state snapshot,
    /// and always records the event: a session that was already terminal still
    /// owes the client a contiguous history.
    fn record_error_locked(&self, inner: &mut SessionInner, code: &str, message: String) -> Value {
        if !is_terminal_status(&inner.status) {
            inner.status = "errored".to_owned();
            inner.pending_input = None;
            inner.pending_kind = None;
            inner.errored_at = Some(tokio::time::Instant::now());
            inner.error = Some(PublicError {
                code: code.to_owned(),
                message: message.clone(),
            });
        }
        // Routing back through `emit_event_locked` cannot re-enter this
        // function: `ServerEvent::Error` builds its payload with the
        // infallible `error_payload`, so the encode arm is unreachable for it.
        let frame = self.emit_event_locked(
            inner,
            ServerEvent::Error {
                code: code.to_owned(),
                message,
            },
        );
        // Fired under the lock so no failure path can park the session without
        // releasing a wizard thread blocked in `request_input`; the waiter
        // wakes once this caller drops the guard.
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
                // Lost the race to the grace reaper (or a duplicate ack);
                // the end state is what the backend wanted, so say so
                // instead of implying no error ever existed.
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

/// The statuses after which nothing new may be said about the run's shape. The
/// terminal frame is the last word a client gets, so both the category fold and
/// the state emission stop here.
fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "canceled" | "closed" | "errored" | "completed_awaiting_ack"
    )
}

/// The statuses a wizard thread waiting on a prompt must give up on. `errored`
/// is one of them: a session parked by a frame-encode failure has to release
/// the thread, since no client will be asked to answer anything again.
fn terminal_status_error(inner: &SessionInner) -> Result<()> {
    match inner.status.as_str() {
        "canceled" | "closed" => Err(StackError::InvalidParam {
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

/// Fold a signal into the session's durable state. Kept beside `SessionInner`
/// rather than in `state.rs` because it is the only place that writes both the
/// category map and `current_step`.
fn apply_state_signal_locked(inner: &mut SessionInner, signal: InitStateSignal) {
    match signal {
        InitStateSignal::StepStarted { kind } => {
            inner.current_step = Some(kind);
            inner.step_in_flight = true;
        }
        InitStateSignal::StepFinished {
            kind, error_code, ..
        } => {
            inner.step_in_flight = false;
            if let Some(category) = category_for_step_kind(kind) {
                match &error_code {
                    Some(code) => inner.categories.fail_step_category(category, code.clone()),
                    // Executed and skipped are the same verdict here: a
                    // resumed run replays already-verified rows as skipped,
                    // and both mean the lane is done being driven.
                    None => inner.categories.settle_unresolved(category),
                }
            }
            // The sweep says "init finished and nothing is left to drive", so a
            // failed final step must not settle the lanes it never reached.
            if kind == step_kind::INIT_COMPLETE && error_code.is_none() {
                inner.categories.settle_remaining();
            }
        }
        InitStateSignal::CategoryApplicability {
            category,
            applicable,
            source,
            reason,
        } => inner
            .categories
            .set_applicability(category, applicable, source, reason),
        InitStateSignal::CategorySettled { category, value } => {
            inner.categories.settle(category, value)
        }
        InitStateSignal::CategoryProvisionallySettled { category, value } => {
            inner.categories.settle_provisional(category, value)
        }
        InitStateSignal::CategoryFailed { category, code } => inner.categories.fail(category, code),
    }
}

fn next_bootstrap_session_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    format!("init_{nanos:020}_{sequence:010}_{pid:010}")
}
