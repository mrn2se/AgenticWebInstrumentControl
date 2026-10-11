//! `rec.wait` (SPECIFICATION.md §2.9.3): registers a matcher on the
//! recording's append path and resolves on the first match after
//! registration (or after `from_s`), with an honest timeout — never blocking
//! the connection's other requests, because `ws.rs` already dispatches each
//! incoming request on its own spawned task (see `ws::handle_socket`), so an
//! `.await` here only ever suspends this one request's task.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::oneshot;

use awic_proto::{PatternMatch, RecWaitResult};

use super::{RecOpError, RecordingEntry};

/// `timeout_ms` cap (SPEC §2.9.3).
const MAX_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// How a pending wait ends (SPEC §2.9.3: "the wait ends with an error if the
/// recording stops").
pub enum WaitOutcome {
    Matched(awic_pattern::Match),
    Ended(String),
}

/// One registered `rec.wait`. Kept on `RecordingEntry::waiters` and driven
/// from the append path ([`feed_waiters`]); the request handler
/// ([`wait_for_pattern`]) only creates it and awaits its result.
pub struct Waiter {
    matcher: StdMutex<awic_pattern::Matcher>,
    resume_sample: AtomicU64,
    is_analog: bool,
    done: StdMutex<Option<oneshot::Sender<WaitOutcome>>>,
}

/// Feeds every registered waiter with whatever is newly known, called after
/// each successful append (SPEC §2.9.3: "it runs on the recorder's append
/// path"). Resolved waiters are dropped from the list here.
pub fn feed_waiters(entry: &RecordingEntry) {
    let rec = entry.read_rec();
    let head = rec.head_sample();
    let mut waiters = entry.waiters.lock().unwrap();
    waiters.retain(|w| {
        let resume = w.resume_sample.load(Ordering::SeqCst);
        if resume >= head {
            return true; // Nothing new for this waiter yet.
        }
        let outcome = if w.is_analog {
            match rec.extract_analog(resume, head) {
                Ok(codes) => {
                    let m = w.matcher.lock().unwrap().feed_analog(resume, &codes);
                    w.resume_sample.store(head, Ordering::SeqCst);
                    Ok(m.into_iter().next())
                }
                Err(err) => Err(err),
            }
        } else {
            match rec.runs(resume, head) {
                Ok(runs) => {
                    if let Some(last) = runs.last() {
                        let last_start = last.start;
                        let pattern_runs = super::to_pattern_runs(&runs);
                        let m = w.matcher.lock().unwrap().feed_runs(&pattern_runs, head);
                        w.resume_sample.store(last_start, Ordering::SeqCst);
                        Ok(m.into_iter().next())
                    } else {
                        Ok(None)
                    }
                }
                Err(err) => Err(err),
            }
        };
        match outcome {
            Ok(Some(m)) => {
                if let Some(tx) = w.done.lock().unwrap().take() {
                    let _ = tx.send(WaitOutcome::Matched(m));
                }
                false // resolved: drop it
            }
            Ok(None) => true, // keep waiting
            Err(err) => {
                if let Some(tx) = w.done.lock().unwrap().take() {
                    let _ = tx.send(WaitOutcome::Ended(err.to_string()));
                }
                false
            }
        }
    });
}

/// Resolves every still-pending waiter with `reason` (the recording just
/// stopped or errored — SPEC §2.9.3).
pub fn end_all(entry: &RecordingEntry, reason: String) {
    let mut waiters = entry.waiters.lock().unwrap();
    for w in waiters.drain(..) {
        if let Some(tx) = w.done.lock().unwrap().take() {
            let _ = tx.send(WaitOutcome::Ended(reason.clone()));
        }
    }
}

/// `rec.wait { rec, pattern, timeout_ms, from_s }` (SPEC §2.9.3).
pub async fn wait_for_pattern(
    state: &crate::state::AppState,
    rec: u16,
    pattern_text: &str,
    timeout_ms: Option<u32>,
    from_s: Option<f64>,
) -> Result<RecWaitResult, RecOpError> {
    let entry = state
        .recorder()
        .get(rec)
        .ok_or_else(|| RecOpError::not_found(rec))?;
    if !entry.is_recording() {
        return Err(RecOpError::new(
            "recording_ended",
            format!(
                "recording {rec} is not recording ({:?}); rec.wait only runs on a live recording",
                entry.state()
            ),
        ));
    }
    let pattern = entry.parse_pattern(pattern_text)?;
    let is_analog = pattern.is_analog();
    let mut matcher = awic_pattern::Matcher::new(&pattern);

    let (first, head) = {
        let g = entry.read_rec();
        (g.first_sample(), g.head_sample())
    };
    let resume = match from_s {
        Some(s) => ((s * entry.rate_hz as f64).round() as u64).clamp(first, head),
        None => head,
    };
    // The first run this waiter's matcher sees starts at `resume` — the
    // call, `from_s`, or the retained start — wherever the level on the line
    // really began, so its length is only a lower bound and its onset is not
    // an event after the call. Without this, a level already held when the
    // wait began answered it at once, timed at the call (B-78).
    matcher.mark_first_run_open_left();

    let (tx, rx) = oneshot::channel();
    let waiter = Arc::new(Waiter {
        matcher: StdMutex::new(matcher),
        resume_sample: AtomicU64::new(resume),
        is_analog,
        done: StdMutex::new(Some(tx)),
    });
    entry.waiters.lock().unwrap().push(waiter.clone());
    // Catches up on any backlog between `resume` and `head` right away (a
    // `from_s` in the past, or data appended between registering the
    // pattern and pushing the waiter) instead of waiting for the next
    // append to notice it.
    feed_waiters(&entry);

    let timeout = Duration::from_millis(
        (timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS as u32) as u64).min(MAX_TIMEOUT_MS),
    );
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(WaitOutcome::Matched(m))) => Ok(RecWaitResult::Match {
            r#match: PatternMatch {
                t_s: m.sample as f64 / entry.rate_hz as f64,
                sample: m.sample,
                truncated: false,
            },
        }),
        Ok(Ok(WaitOutcome::Ended(reason))) => Err(RecOpError::new("recording_ended", reason)),
        Ok(Err(_)) => Err(RecOpError::new(
            "internal_error",
            "wait was dropped before it resolved",
        )),
        Err(_) => {
            // Timed out: stop the matcher from doing any more work on
            // future appends.
            entry
                .waiters
                .lock()
                .unwrap()
                .retain(|w| !Arc::ptr_eq(w, &waiter));
            Ok(RecWaitResult::Timeout { timeout: true })
        }
    }
}
