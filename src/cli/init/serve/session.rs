use super::*;

pub(super) struct HostedInitManager {
    pub(super) active: Mutex<Option<Arc<HostedInitSession>>>,
    pub(super) shutdown: Arc<Notify>,
}

pub(super) enum StartSessionError {
    Active,
}

impl HostedInitManager {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(None),
            shutdown: Arc::new(Notify::new()),
        })
    }

    pub(super) fn start_session(
        self: &Arc<Self>,
        request: StartInitRequest,
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
        let init_args = request.into_init_args();
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

    pub(super) fn session(&self, id: &str) -> Option<Arc<HostedInitSession>> {
        lock_unpoisoned(&self.active)
            .as_ref()
            .filter(|session| session.id == id)
            .cloned()
    }

    pub(super) async fn wait_for_terminal(&self) {
        self.shutdown.notified().await;
    }

    pub(super) fn terminal_result(&self) -> Result<()> {
        let Some(session) = self.session_current() else {
            return Ok(());
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

    fn session_current(&self) -> Option<Arc<HostedInitSession>> {
        lock_unpoisoned(&self.active).as_ref().cloned()
    }
}

pub(super) struct HostedInitSession {
    pub(super) id: String,
    pub(super) inner: Mutex<SessionInner>,
    input_ready: Condvar,
    events: broadcast::Sender<String>,
    shutdown: Arc<Notify>,
}

pub(super) struct SessionInner {
    status: String,
    next_seq: u64,
    history: Vec<Value>,
    pub(super) pending_input: Option<PublicInputRequest>,
    pending_response: Option<(String, Value)>,
    result_json: Option<String>,
    error: Option<PublicError>,
}

impl HostedInitSession {
    pub(super) fn new(id: String, shutdown: Arc<Notify>) -> Arc<Self> {
        let (events, _) = broadcast::channel(INIT_WS_CHANNEL_CAPACITY);
        let session = Arc::new(Self {
            id,
            inner: Mutex::new(SessionInner {
                status: "running".to_owned(),
                next_seq: 0,
                history: Vec::new(),
                pending_input: None,
                pending_response: None,
                result_json: None,
                error: None,
            }),
            input_ready: Condvar::new(),
            events,
            shutdown,
        });
        session.push_event("progress", json!({"message": "init session started"}));
        session
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    pub(super) fn status(&self) -> String {
        lock_unpoisoned(&self.inner).status.clone()
    }

    pub(super) fn is_active(&self) -> bool {
        matches!(
            self.status().as_str(),
            "running" | "waiting_for_input" | "completed_awaiting_ack"
        )
    }

    pub(super) fn status_snapshot(&self) -> InitStatusResponse {
        let inner = lock_unpoisoned(&self.inner);
        InitStatusResponse {
            session_id: self.id.clone(),
            status: inner.status.clone(),
            last_seq: inner.next_seq,
            pending_input: inner.pending_input.clone(),
            recent_events: inner.history.iter().rev().take(50).cloned().collect(),
            result_available: inner.result_json.is_some(),
            error: inner.error.clone(),
        }
    }

    pub(super) fn hello_frame(&self) -> String {
        let snapshot = self.status_snapshot();
        json!({
            "type": "hello",
            "session_id": self.id,
            "status": snapshot.status,
            "last_seq": snapshot.last_seq,
            "pending_input": snapshot.pending_input,
            "result_available": snapshot.result_available
        })
        .to_string()
    }

    pub(super) fn events_after(&self, after_seq: u64) -> Vec<Value> {
        lock_unpoisoned(&self.inner)
            .history
            .iter()
            .filter(|event| event.get("seq").and_then(Value::as_u64).unwrap_or(0) > after_seq)
            .cloned()
            .collect()
    }

    pub(super) fn push_event(&self, event_type: &str, mut payload: Value) {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            self.record_event_locked(&mut inner, event_type, &mut payload)
        };
        let _ = self.events.send(frame.to_string());
    }

    fn record_event_locked(
        &self,
        inner: &mut SessionInner,
        event_type: &str,
        payload: &mut Value,
    ) -> Value {
        inner.next_seq = inner.next_seq.saturating_add(1);
        let seq = inner.next_seq;
        let mut object = BTreeMap::new();
        object.insert("type".to_owned(), Value::String(event_type.to_owned()));
        object.insert("seq".to_owned(), Value::Number(seq.into()));
        object.insert("session_id".to_owned(), Value::String(self.id.clone()));
        if let Some(map) = payload.as_object_mut() {
            for (key, value) in std::mem::take(map) {
                object.insert(key, value);
            }
        } else {
            object.insert("data".to_owned(), payload.clone());
        }
        let frame = Value::Object(object.into_iter().collect());
        inner.history.push(frame.clone());
        if inner.history.len() > INIT_EVENT_HISTORY_LIMIT {
            inner.history.remove(0);
        }
        frame
    }

    pub(super) fn request_input(&self, request: HostedPromptRequest) -> Result<Option<Value>> {
        if !should_handle_hosted_prompt(&request) {
            return Ok(None);
        }
        let public = public_input_request(request);
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(inner.status.as_str(), "canceled" | "closed") {
                return Err(StackError::InvalidParam {
                    field: "init",
                    reason: "hosted init session was cancelled".to_owned(),
                });
            }
            inner.status = "waiting_for_input".to_owned();
            inner.pending_response = None;
            inner.pending_input = Some(public.clone());
            let mut payload = json!({ "input": public });
            self.record_event_locked(&mut inner, "input_required", &mut payload)
        };
        let _ = self.events.send(frame.to_string());

        let mut inner = lock_unpoisoned(&self.inner);
        loop {
            if matches!(inner.status.as_str(), "canceled" | "closed") {
                return Err(StackError::InvalidParam {
                    field: "init",
                    reason: "hosted init session was cancelled".to_owned(),
                });
            }
            if let Some((request_id, value)) = inner.pending_response.take()
                && request_id == public.request_id
            {
                inner.status = "running".to_owned();
                inner.pending_input = None;
                return Ok(Some(value));
            }
            inner = self
                .input_ready
                .wait(inner)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(super) fn submit_input(
        &self,
        request_id: &str,
        value: Value,
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
            inner.pending_response = Some((request_id.to_owned(), value));
            let mut payload = json!({ "request_id": request_id });
            self.record_event_locked(&mut inner, "input_accepted", &mut payload)
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
            inner.pending_input = None;
            let mut payload = json!({ "status": "completed_awaiting_ack" });
            let frame = self.record_event_locked(&mut inner, "result_ready", &mut payload);
            let _ = self.events.send(frame.to_string());
        }
        if let Some(frame) = self.result_frame() {
            let _ = self.events.send(frame);
        }
        self.input_ready.notify_all();
    }

    pub(super) fn result_frame(&self) -> Option<String> {
        let inner = lock_unpoisoned(&self.inner);
        let result = inner.result_json.as_ref()?;
        Some(format!(
            r#"{{"type":"result","session_id":"{}","payload":{}}}"#,
            self.id, result
        ))
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
            let mut payload = json!({ "status": "closed" });
            self.record_event_locked(&mut inner, "result_acked", &mut payload)
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
        Ok(())
    }

    pub(super) fn cancel(&self, reason: &str) {
        let Some(frame) = ({
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(
                inner.status.as_str(),
                "completed_awaiting_ack" | "closed" | "canceled"
            ) {
                return;
            }
            inner.status = "canceled".to_owned();
            inner.pending_input = None;
            inner.pending_response = None;
            let mut payload = json!({ "reason": reason });
            Some(self.record_event_locked(&mut inner, "canceled", &mut payload))
        }) else {
            return;
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
    }

    pub(super) fn set_error(&self, code: &str, message: String) {
        let Some(frame) = ({
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(
                inner.status.as_str(),
                "canceled" | "closed" | "completed_awaiting_ack"
            ) {
                return;
            }
            inner.status = "errored".to_owned();
            inner.pending_input = None;
            inner.error = Some(PublicError {
                code: code.to_owned(),
                message: message.clone(),
            });
            let mut payload = json!({ "code": code, "message": message });
            Some(self.record_event_locked(&mut inner, "error", &mut payload))
        }) else {
            return;
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
    }
}

fn next_bootstrap_session_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    format!("init_{nanos:020}_{sequence:010}_{pid:010}")
}
