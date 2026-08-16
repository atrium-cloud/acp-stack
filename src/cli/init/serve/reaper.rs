use super::*;

/// Parse a duration-suffix flag (`30s`, `15m`, `1h`); `0s` maps to `None`
/// (disabled). Mirrors the `acps logs --since` parsing.
pub(super) fn parse_optional_duration(
    raw: &str,
    field: &'static str,
) -> Result<Option<std::time::Duration>> {
    let duration =
        crate::time_util::parse_duration_suffix(raw).ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("not a valid duration (use e.g. 30s, 15m, 1h): {raw}"),
        })?;
    let duration = duration.to_std().map_err(|_| StackError::InvalidParam {
        field,
        reason: format!("duration out of range: {raw}"),
    })?;
    if duration.is_zero() {
        return Ok(None);
    }
    Ok(Some(duration))
}

/// Expire the hosted session once it has been idle (no connected WebSocket
/// client and no API activity) for `timeout`. A server that never received a
/// session idles out on the same clock — measured from the last authenticated
/// API call, not just server start — so an abandoned bootstrap process cannot
/// pin the bind port indefinitely while an actively polling backend can.
/// `None` disables the idle clock but not the loop: the error-ack grace check
/// is unconditional, since an unacked parked failure would otherwise keep the
/// process alive forever.
pub(super) async fn reap_idle_session(
    manager: Arc<HostedInitManager>,
    timeout: Option<std::time::Duration>,
) {
    loop {
        tokio::time::sleep(IDLE_REAPER_TICK).await;
        match manager.session_current() {
            Some(session) => {
                // A parked failure is owned by the ack grace alone: the idle
                // clock must not pre-empt it, so the backend is guaranteed
                // the full grace to retrieve and acknowledge the error.
                if let Some(age) = session.unacked_error_age() {
                    if age >= ERROR_ACK_GRACE {
                        session.expire("error_ack_timeout");
                        break;
                    }
                    continue;
                }
                let Some(timeout) = timeout else { continue };
                if !session.has_connected_ws()
                    && session.last_activity_age_secs() >= timeout.as_secs()
                {
                    session.expire("idle_timeout");
                    break;
                }
            }
            None => {
                let Some(timeout) = timeout else { continue };
                if manager.activity_age() >= timeout
                    && manager.shutdown_if_no_session("idle_timeout")
                {
                    break;
                }
            }
        }
    }
}

pub(super) async fn enforce_max_lifetime(
    manager: Arc<HostedInitManager>,
    lifetime: std::time::Duration,
) {
    tokio::time::sleep(lifetime).await;
    if let Some(session) = manager.session_current()
        && session.is_active()
    {
        session.expire("max_lifetime");
        return;
    }
    manager.initiate_shutdown("max_lifetime");
}

pub(super) fn resolve_bootstrap_token(args: &InitServeArgs) -> Result<String> {
    let token = if let Some(path) = args.token_file.as_ref() {
        std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?
    } else {
        std::env::var(&args.token_env).map_err(|_| StackError::MissingField {
            field: INIT_BOOTSTRAP_TOKEN_FIELD,
        })?
    };
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(StackError::InvalidParam {
            field: INIT_BOOTSTRAP_TOKEN_FIELD,
            reason: "bootstrap token must not be empty".to_owned(),
        });
    }
    Ok(token)
}
