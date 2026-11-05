//! The screenshot portal (S2): `org.freedesktop.portal.Screenshot` over the session bus, with
//! `interactive: false` (COSMIC answers without a prompt). The answer arrives as a `Response`
//! signal on a request object whose path is known beforehand, so the signal is watched before
//! the call and no answer is missed. Every wait is bounded.

use super::shot::{request_path, response_file};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedValue, Value};
use zbus::MatchRule;

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const OBJECT: &str = "/org/freedesktop/portal/desktop";
const SCREENSHOT: &str = "org.freedesktop.portal.Screenshot";
const REQUEST: &str = "org.freedesktop.portal.Request";
/// How long the portal has to answer a screenshot.
const SHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the check that the portal exists may take, and how long its answer is reused.
const CHECK_TIMEOUT: Duration = Duration::from_secs(1);
const CHECK_REUSE: Duration = Duration::from_secs(3);

/// How many portal waits may be alive at once; a portal that stops answering cannot pile up threads.
const MAX_WAITS: usize = 4;

static TOKENS: AtomicU32 = AtomicU32::new(1);
static WAITS: AtomicUsize = AtomicUsize::new(0);

/// Counts a live wait; released when the thread is done.
struct Wait;

impl Wait {
    fn begin(cap: usize) -> Option<Wait> {
        let mut live = WAITS.load(Ordering::Acquire);
        while live < cap {
            match WAITS.compare_exchange_weak(live, live + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(Wait),
                Err(now) => live = now,
            }
        }
        None
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        WAITS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Runs `work` on a thread of its own and waits at most `limit` for its result. A result that
/// arrives after the wait gave up goes to `discard` instead, so what it holds is not leaked.
fn within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    discard: impl FnOnce(T) + Send + 'static,
) -> Result<T, String> {
    within_capped(MAX_WAITS, limit, work, discard)
}

fn within_capped<T: Send + 'static>(
    cap: usize,
    limit: Duration,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    discard: impl FnOnce(T) + Send + 'static,
) -> Result<T, String> {
    let wait = Wait::begin(cap).ok_or("the screenshot portal is still busy with earlier requests")?;
    let (done, result) = channel();
    std::thread::Builder::new()
        .name("computer-portal".into())
        .spawn(move || {
            let _wait = wait;
            if let Err(std::sync::mpsc::SendError(Ok(value))) = done.send(work()) {
                discard(value);
            }
        })
        .map_err(|e| format!("cannot start a thread: {e}"))?;
    result.recv_timeout(limit).map_err(|_| "the screenshot portal did not answer in time".to_string())?
}

#[derive(Default)]
pub struct Portal {
    connection: Mutex<Option<Connection>>,
    checked: Mutex<Option<(Instant, bool)>>,
}

impl Portal {
    fn connection(&self) -> Result<Connection, String> {
        let mut slot = self.connection.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(connection) = slot.as_ref() {
            return Ok(connection.clone());
        }
        let connection = Connection::session().map_err(|e| format!("cannot reach the session bus: {e}"))?;
        *slot = Some(connection.clone());
        Ok(connection)
    }

    /// Whether the desktop offers the Screenshot portal.
    pub fn available(&self) -> bool {
        let mut checked = self.checked.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, answer)) = *checked {
            if at.elapsed() < CHECK_REUSE {
                return answer;
            }
        }
        let answer = self
            .connection()
            .and_then(|connection| {
                within(
                    CHECK_TIMEOUT,
                    move || {
                        let proxy = Proxy::new(&connection, DESTINATION, OBJECT, SCREENSHOT).map_err(|e| e.to_string())?;
                        proxy.get_property::<u32>("version").map_err(|e| e.to_string())
                    },
                    drop,
                )
            })
            .is_ok();
        *checked = Some((Instant::now(), answer));
        answer
    }

    /// The PNG of the whole layout; the file the portal wrote is deleted.
    pub fn screenshot(&self) -> Result<Vec<u8>, String> {
        let connection = self.connection()?;
        let path = within(SHOT_TIMEOUT, move || request_screenshot(&connection), |late| remove_shot(&late))?;
        let bytes = std::fs::read(&path).map_err(|e| format!("cannot read the screenshot: {e}"));
        remove_shot(&path);
        bytes
    }
}

/// Only the PNG the portal just made is removed, never anything else the answer names.
fn remove_shot(path: &std::path::Path) {
    if path.extension().is_some_and(|ext| ext == "png") {
        let _ = std::fs::remove_file(path);
    }
}

fn request_screenshot(connection: &Connection) -> Result<std::path::PathBuf, String> {
    let unique = connection.unique_name().ok_or("the session bus gave no name")?.to_string();
    let token = format!("gentle_dot_{}", TOKENS.fetch_add(1, Ordering::Relaxed));
    let path = request_path(&unique, &token);
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(REQUEST)
        .and_then(|b| b.member("Response"))
        .and_then(|b| b.path(path.as_str()))
        .map_err(|e| e.to_string())?
        .build();
    // Subscribed before the call, so the answer cannot arrive unseen.
    let mut answers = MessageIterator::for_match_rule(rule, connection, Some(4)).map_err(|e| e.to_string())?;
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("interactive", Value::from(false));
    connection
        .call_method(Some(DESTINATION), OBJECT, Some(SCREENSHOT), "Screenshot", &("", options))
        .map_err(|e| format!("the screenshot portal refused: {e}"))?;
    let message = answers.next().ok_or("the session bus closed")?.map_err(|e| e.to_string())?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize().map_err(|e| e.to_string())?;
    let uri = results.get("uri").and_then(|value| match &**value {
        Value::Str(uri) => Some(uri.as_str().to_string()),
        _ => None,
    });
    response_file(code, uri.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn a_late_answer_is_discarded_and_waits_are_capped() {
        // Late: the caller gives up, then the work finishes and its result reaches `discard`.
        let (release, gate) = sync_channel::<()>(0);
        let (late, seen) = channel();
        let result = within_capped(2, Duration::from_millis(20), move || {
            gate.recv().ok();
            Ok(7)
        }, move |value| {
            late.send(value).unwrap();
        });
        assert!(result.unwrap_err().contains("in time"));
        release.send(()).unwrap();
        assert_eq!(seen.recv_timeout(Duration::from_secs(2)), Ok(7));

        while WAITS.load(Ordering::Acquire) != 0 {
            std::thread::yield_now();
        }
        // Cap: with every slot held by stuck waits, the next one is refused without a thread.
        let (release, gate) = sync_channel::<()>(0);
        let gate = std::sync::Arc::new(Mutex::new(gate));
        let mut stuck = Vec::new();
        for _ in 0..2 {
            let gate = gate.clone();
            stuck.push(within_capped(2, Duration::from_millis(5), move || {
                gate.lock().unwrap().recv().ok();
                Ok(())
            }, drop));
        }
        assert!(stuck.iter().all(|r| r.as_ref().unwrap_err().contains("in time")));
        let refused = within_capped(2, Duration::from_millis(5), || Ok(()), drop);
        assert!(refused.unwrap_err().contains("busy"));
        release.send(()).unwrap();
        release.send(()).unwrap();
    }
}
