//! The MCP monitor (SPECIFICATION.md §8.5): what the web UI's MCP tab shows.
//!
//! Every exchange on `/mcp` passes through [`McpMonitor::record`] (from
//! `route.rs`, the one place all MCP traffic — the stdio shim's included —
//! arrives). Sessions and their counters are always tracked: they are a few
//! hundred bytes each and the bound on them is `route::MAX_SESSIONS`. The
//! *message log* — requests, responses and the pictures results carried —
//! is the part that costs memory, so it can be switched off (`[mcp].log`,
//! or `mcp.logging` from the UI) and is held under a byte budget
//! (`[mcp].log_memory_mb`), dropping the oldest entries first.
//!
//! Nothing is pushed to the UI but a small `mcp.activity` event; a client
//! looking at the tab pulls `mcp.log` from its last `seq`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use awic_core::{EventBus, McpConfig};
use awic_proto::{
    Event, McpEntryKind, McpImageInfo, McpImageResult, McpLogEntry, McpLogResult, McpLoggingState,
    McpSessionInfo, PROTOCOL_VERSION,
};
use serde_json::Value;

/// Requests and responses are kept pretty-printed up to this many bytes
/// each; past it the text is cut and says so. Pictures are counted
/// separately (they are the bulk, and the switch for them is separate).
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// `mcp.log` returns at most this many entries when no `limit` is given.
pub const DEFAULT_LOG_LIMIT: u32 = 500;

/// Ended sessions kept for display, beyond the live ones.
const KEPT_ENDED_SESSIONS: usize = 16;

/// Fixed per-entry overhead counted against the budget (struct, strings'
/// headers, the deque slot) — an estimate, so a log of many tiny entries
/// still meets its bound.
const ENTRY_OVERHEAD: usize = 256;

/// Length of the session label shown instead of the full `Mcp-Session-Id`:
/// enough to tell sessions apart, without putting a usable id on screen.
const SESSION_LABEL_LEN: usize = 8;

pub fn session_label(id: &str) -> String {
    id.chars().take(SESSION_LABEL_LEN).collect()
}

pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// One exchange to record, as `route.rs` saw it.
pub struct Exchange<'a> {
    /// The full `Mcp-Session-Id`, when the request named a known session.
    pub session: Option<&'a str>,
    pub kind: McpEntryKind,
    pub method: &'a str,
    pub request: &'a Value,
    /// The JSON-RPC response body; `None` for a notification.
    pub response: Option<&'a Value>,
    pub started_ms: f64,
    pub duration_ms: f64,
}

struct StoredEntry {
    entry: McpLogEntry,
    /// Base64 data per `entry.images` slot; `None` where the picture was not kept.
    image_data: Vec<Option<String>>,
    bytes: usize,
}

struct SessionRecord {
    full_id: String,
    info: McpSessionInfo,
}

struct Inner {
    enabled: bool,
    images: bool,
    limit_bytes: usize,
    used_bytes: usize,
    entries: VecDeque<StoredEntry>,
    last_seq: u32,
    dropped: u32,
    /// Live and recently ended sessions, oldest first.
    sessions: Vec<SessionRecord>,
}

impl Inner {
    fn logging_state(&self) -> McpLoggingState {
        McpLoggingState {
            enabled: self.enabled,
            images: self.images,
            memory_used_bytes: self.used_bytes as u64,
            memory_limit_bytes: self.limit_bytes as u64,
            entries: self.entries.len() as u32,
            dropped: self.dropped,
        }
    }

    fn session_mut(&mut self, full_id: &str) -> Option<&mut SessionRecord> {
        self.sessions.iter_mut().find(|s| s.full_id == full_id)
    }

    fn push(&mut self, mut stored: StoredEntry) {
        // An entry that alone breaks the budget gives up its pictures first.
        if stored.bytes > self.limit_bytes {
            for (slot, info) in stored.image_data.iter_mut().zip(&mut stored.entry.images) {
                if slot.take().is_some() {
                    info.kept = false;
                }
            }
            stored.bytes = entry_bytes(&stored.entry, &stored.image_data);
        }
        self.used_bytes += stored.bytes;
        self.entries.push_back(stored);
        while self.used_bytes > self.limit_bytes && self.entries.len() > 1 {
            if let Some(old) = self.entries.pop_front() {
                self.used_bytes -= old.bytes;
                self.dropped += 1;
            }
        }
    }

    fn prune_ended_sessions(&mut self) {
        let ended = self.sessions.iter().filter(|s| !s.info.active).count();
        let mut excess = ended.saturating_sub(KEPT_ENDED_SESSIONS);
        self.sessions.retain(|s| {
            if excess > 0 && !s.info.active {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
}

fn entry_bytes(entry: &McpLogEntry, image_data: &[Option<String>]) -> usize {
    ENTRY_OVERHEAD
        + entry.summary.len()
        + entry.method.len()
        + entry.client.len()
        + entry.request.as_ref().map_or(0, String::len)
        + entry.response.as_ref().map_or(0, String::len)
        + image_data.iter().flatten().map(String::len).sum::<usize>()
}

/// Cuts `text` to [`MAX_TEXT_BYTES`] on a character boundary, saying how much went.
fn bounded(mut text: String) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }
    let total = text.len();
    let mut cut = MAX_TEXT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str(&format!("\n… truncated, {} of {total} bytes shown", cut));
    text
}

fn pretty(value: &Value) -> String {
    bounded(serde_json::to_string_pretty(value).unwrap_or_default())
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

/// Pulls the pictures out of a `tools/call` result, replacing each one's
/// data in the response with a short placeholder, so the response text
/// stays readable and the bytes are counted (and kept) once.
fn take_images(response: &mut Value) -> Vec<(String, String)> {
    let Some(content) = response
        .get_mut("result")
        .and_then(|r| r.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return Vec::new();
    };
    let mut images = Vec::new();
    for block in content.iter_mut() {
        if block.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        let mime_type = block
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string();
        let Some(data) = block.get_mut("data") else {
            continue;
        };
        let taken = data.as_str().unwrap_or_default().to_string();
        *data = Value::String(format!(
            "<{} bytes of {mime_type}, image #{} in the MCP tab>",
            taken.len(),
            images.len() + 1
        ));
        images.push((mime_type, taken));
    }
    images
}

fn is_error(response: Option<&Value>) -> bool {
    response.is_some_and(|r| {
        r.get("error").is_some()
            || r.get("result")
                .and_then(|res| res.get("isError"))
                .and_then(Value::as_bool)
                == Some(true)
    })
}

/// The one line shown in the list: a tool's arguments, a failure's message,
/// or nothing worth more than the method name.
fn summarize(request: &Value, response: Option<&Value>, failed: bool) -> String {
    if failed {
        if let Some(msg) = response
            .and_then(|r| r.get("error"))
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            return one_line(msg, 200);
        }
        if let Some(text) = response
            .and_then(|r| r.get("result"))
            .and_then(|r| r.get("content"))
            .and_then(|c| c.get(0))
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
        {
            return one_line(text, 200);
        }
    }
    match request.get("params") {
        Some(params) if params.get("arguments").is_some() => {
            one_line(&params["arguments"].to_string(), 200)
        }
        Some(params) if params.get("clientInfo").is_some() => {
            one_line(&params["clientInfo"].to_string(), 200)
        }
        _ => String::new(),
    }
}

/// See the module documentation.
pub struct McpMonitor {
    inner: Mutex<Inner>,
    bus: EventBus,
}

impl McpMonitor {
    pub fn new(config: &McpConfig, bus: EventBus) -> Self {
        let limit_bytes = usize::try_from(config.log_memory_mb.saturating_mul(1024 * 1024))
            .unwrap_or(usize::MAX)
            .max(64 * 1024);
        Self {
            inner: Mutex::new(Inner {
                enabled: config.log,
                images: config.log_images,
                limit_bytes,
                used_bytes: 0,
                entries: VecDeque::new(),
                last_seq: 0,
                dropped: 0,
                sessions: Vec::new(),
            }),
            bus,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding this lock leaves only counters half-updated;
        // the monitor must never take `/mcp` down with it.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn announce(&self, seq: u32, sessions_changed: bool) {
        self.bus.publish(Event::McpActivity {
            v: PROTOCOL_VERSION,
            seq,
            sessions_changed,
        });
    }

    /// `initialize` succeeded and minted `full_id`.
    pub fn session_started(&self, full_id: &str, params: &Value, protocol_version: &str) {
        let client_info = params.get("clientInfo");
        let client = client_info
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let client_version = client_info
            .and_then(|c| c.get("version"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let now = now_ms();
        let mut inner = self.lock();
        inner.sessions.push(SessionRecord {
            full_id: full_id.to_string(),
            info: McpSessionInfo {
                id: session_label(full_id),
                client,
                client_version,
                protocol_version: protocol_version.to_string(),
                started_ms: now,
                last_seen_ms: now,
                requests: 0,
                tool_calls: 0,
                errors: 0,
                last_tool: None,
                active: true,
                ended: None,
            },
        });
        let seq = inner.last_seq;
        drop(inner);
        self.announce(seq, true);
    }

    /// The session went away: deleted by its client, or evicted.
    pub fn session_ended(&self, full_id: &str, reason: &str) {
        let mut inner = self.lock();
        let Some(record) = inner.session_mut(full_id) else {
            return;
        };
        if !record.info.active {
            return;
        }
        record.info.active = false;
        record.info.ended = Some(reason.to_string());
        let (client, label) = (record.info.client.clone(), record.info.id.clone());
        if inner.enabled {
            inner.last_seq += 1;
            let entry = McpLogEntry {
                seq: inner.last_seq,
                t_ms: now_ms(),
                session: Some(label),
                client,
                kind: McpEntryKind::SessionEnd,
                method: "session".to_string(),
                tool: None,
                duration_ms: None,
                is_error: false,
                summary: reason.to_string(),
                request: None,
                response: None,
                images: Vec::new(),
            };
            let bytes = entry_bytes(&entry, &[]);
            inner.push(StoredEntry {
                entry,
                image_data: Vec::new(),
                bytes,
            });
        }
        inner.prune_ended_sessions();
        let seq = inner.last_seq;
        drop(inner);
        self.announce(seq, true);
    }

    /// Records one exchange: always against its session's counters, and in
    /// the message log when logging is on.
    pub fn record(&self, exchange: Exchange<'_>) {
        let failed = is_error(exchange.response);
        let tool = (exchange.method == "tools/call")
            .then(|| {
                exchange
                    .request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();

        let mut inner = self.lock();
        let mut client = "unknown".to_string();
        let mut label = None;
        if let Some(full_id) = exchange.session {
            if let Some(record) = inner.session_mut(full_id) {
                let info = &mut record.info;
                info.last_seen_ms = now_ms();
                if exchange.method != "initialize" {
                    info.requests += 1;
                }
                if let Some(tool) = &tool {
                    info.tool_calls += 1;
                    info.last_tool = Some(tool.clone());
                }
                if failed {
                    info.errors += 1;
                }
                client = info.client.clone();
                label = Some(info.id.clone());
            }
        }

        if !inner.enabled {
            let seq = inner.last_seq;
            drop(inner);
            // Counters moved; the sessions table is what shows them.
            self.announce(seq, true);
            return;
        }
        let keep_images = inner.images;
        drop(inner);

        // Formatting happens outside the lock: a picture can be large.
        let mut response = exchange.response.cloned();
        let images = response.as_mut().map(take_images).unwrap_or_default();
        let summary = summarize(exchange.request, exchange.response, failed);
        let mut entry = McpLogEntry {
            seq: 0,
            t_ms: exchange.started_ms,
            session: label,
            client,
            kind: exchange.kind,
            method: exchange.method.to_string(),
            tool,
            duration_ms: Some(exchange.duration_ms),
            is_error: failed,
            summary,
            request: Some(pretty(exchange.request)),
            response: response.as_ref().map(pretty),
            images: images
                .iter()
                .map(|(mime_type, data)| McpImageInfo {
                    mime_type: mime_type.clone(),
                    bytes: data.len() as u32,
                    kept: keep_images,
                })
                .collect(),
        };
        let image_data: Vec<Option<String>> = images
            .into_iter()
            .map(|(_, data)| keep_images.then_some(data))
            .collect();

        let mut inner = self.lock();
        if !inner.enabled {
            return;
        }
        inner.last_seq += 1;
        entry.seq = inner.last_seq;
        let bytes = entry_bytes(&entry, &image_data);
        inner.push(StoredEntry {
            entry,
            image_data,
            bytes,
        });
        let seq = inner.last_seq;
        drop(inner);
        self.announce(seq, exchange.session.is_some());
    }

    /// Live sessions first, then ended ones; most recent first within each.
    pub fn sessions(&self) -> Vec<McpSessionInfo> {
        let inner = self.lock();
        let mut sessions: Vec<McpSessionInfo> = inner
            .sessions
            .iter()
            .rev()
            .map(|s| s.info.clone())
            .collect();
        sessions.sort_by_key(|s| !s.active);
        sessions
    }

    pub fn logging(&self) -> McpLoggingState {
        self.lock().logging_state()
    }

    pub fn log(&self, after: Option<u32>, limit: Option<u32>) -> McpLogResult {
        let inner = self.lock();
        let after = after.unwrap_or(0);
        let limit = limit.unwrap_or(DEFAULT_LOG_LIMIT).max(1) as usize;
        let newer: Vec<&StoredEntry> = inner
            .entries
            .iter()
            .filter(|s| s.entry.seq > after)
            .collect();
        // The newest `limit` of them: a client that fell far behind wants
        // what is happening now, not the start of a backlog.
        let skip = newer.len().saturating_sub(limit);
        McpLogResult {
            entries: newer[skip..].iter().map(|s| s.entry.clone()).collect(),
            last_seq: inner.last_seq,
            logging: inner.logging_state(),
        }
    }

    pub fn image(&self, seq: u32, index: u32) -> Result<McpImageResult, String> {
        let inner = self.lock();
        let stored = inner
            .entries
            .iter()
            .find(|s| s.entry.seq == seq)
            .ok_or_else(|| format!("no MCP log entry {seq}; it may have been dropped"))?;
        let info = stored
            .entry
            .images
            .get(index as usize)
            .ok_or_else(|| format!("MCP log entry {seq} has no image {index}"))?;
        match stored.image_data.get(index as usize).and_then(Option::as_ref) {
            Some(data) => Ok(McpImageResult {
                mime_type: info.mime_type.clone(),
                data_b64: data.clone(),
            }),
            None => Err(format!(
                "image {index} of MCP log entry {seq} was not kept (image logging off, or over the memory limit)"
            )),
        }
    }

    pub fn set_logging(
        &self,
        enabled: Option<bool>,
        images: Option<bool>,
        clear: bool,
    ) -> McpLoggingState {
        let mut inner = self.lock();
        if let Some(enabled) = enabled {
            inner.enabled = enabled;
        }
        if let Some(images) = images {
            inner.images = images;
        }
        if clear {
            inner.entries.clear();
            inner.used_bytes = 0;
            inner.dropped = 0;
        }
        let state = inner.logging_state();
        let seq = inner.last_seq;
        drop(inner);
        self.announce(seq, false);
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn monitor(memory_mb: u64) -> McpMonitor {
        McpMonitor::new(
            &McpConfig {
                log: true,
                log_images: true,
                log_memory_mb: memory_mb,
            },
            EventBus::default(),
        )
    }

    fn call(name: &str) -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":{"device":"3-3"}}})
    }

    fn record_call(m: &McpMonitor, session: &str, request: &Value, response: &Value) {
        m.record(Exchange {
            session: Some(session),
            kind: McpEntryKind::Request,
            method: "tools/call",
            request,
            response: Some(response),
            started_ms: now_ms(),
            duration_ms: 1.5,
        });
    }

    fn start(m: &McpMonitor, id: &str) {
        m.session_started(
            id,
            &json!({"clientInfo":{"name":"Visual Studio Code","version":"1.105"}}),
            "2025-06-18",
        );
    }

    #[test]
    fn a_tool_call_is_logged_and_counted_against_its_session() {
        let m = monitor(16);
        start(&m, "0123456789abcdef");
        let response = json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{}"}],"isError":false}});
        record_call(&m, "0123456789abcdef", &call("list_instruments"), &response);

        let log = m.log(None, None);
        assert_eq!(log.entries.len(), 1);
        let entry = &log.entries[0];
        assert_eq!(entry.tool.as_deref(), Some("list_instruments"));
        assert_eq!(entry.client, "Visual Studio Code");
        assert_eq!(entry.session.as_deref(), Some("01234567"));
        assert!(entry.summary.contains("3-3"));
        assert!(!entry.is_error);

        let sessions = m.sessions();
        assert_eq!(sessions[0].tool_calls, 1);
        assert_eq!(sessions[0].client_version.as_deref(), Some("1.105"));
        assert_eq!(sessions[0].last_tool.as_deref(), Some("list_instruments"));
    }

    #[test]
    fn a_tool_error_is_flagged_and_summarized_by_its_message() {
        let m = monitor(16);
        start(&m, "s1");
        let response = json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"device_busy: call rec.stop first"}],"isError":true}});
        record_call(&m, "s1", &call("capture"), &response);
        let entry = &m.log(None, None).entries[0];
        assert!(entry.is_error);
        assert_eq!(entry.summary, "device_busy: call rec.stop first");
        assert_eq!(m.sessions()[0].errors, 1);
    }

    #[test]
    fn pictures_are_lifted_out_of_the_response_and_served_separately() {
        let m = monitor(16);
        start(&m, "s1");
        let response = json!({"jsonrpc":"2.0","id":1,"result":{"content":[
            {"type":"text","text":"{}"},
            {"type":"image","mimeType":"image/svg+xml","data":"PHN2Zz4="}
        ],"isError":false}});
        record_call(&m, "s1", &call("render_view"), &response);

        let entry = &m.log(None, None).entries[0];
        assert_eq!(entry.images.len(), 1);
        assert!(entry.images[0].kept);
        assert!(!entry.response.as_ref().unwrap().contains("PHN2Zz4="));
        let image = m.image(entry.seq, 0).unwrap();
        assert_eq!(image.mime_type, "image/svg+xml");
        assert_eq!(image.data_b64, "PHN2Zz4=");
        assert!(m.image(entry.seq, 1).is_err());
    }

    #[test]
    fn with_image_logging_off_the_picture_is_listed_but_not_kept() {
        let m = monitor(16);
        m.set_logging(None, Some(false), false);
        start(&m, "s1");
        let response = json!({"jsonrpc":"2.0","id":1,"result":{"content":[
            {"type":"image","mimeType":"image/svg+xml","data":"PHN2Zz4="}
        ]}});
        record_call(&m, "s1", &call("render_view"), &response);
        let entry = &m.log(None, None).entries[0];
        assert!(!entry.images[0].kept);
        assert!(m.image(entry.seq, 0).is_err());
    }

    #[test]
    fn with_logging_off_nothing_is_stored_but_sessions_still_count() {
        let m = monitor(16);
        m.set_logging(Some(false), None, false);
        start(&m, "s1");
        record_call(
            &m,
            "s1",
            &call("survey"),
            &json!({"jsonrpc":"2.0","id":1,"result":{}}),
        );
        let log = m.log(None, None);
        assert!(log.entries.is_empty());
        assert_eq!(log.logging.memory_used_bytes, 0);
        assert_eq!(m.sessions()[0].tool_calls, 1);
    }

    #[test]
    fn the_log_stays_inside_its_memory_bound_dropping_the_oldest() {
        // The bound has a 64 KiB floor; 0 MB asks for that floor.
        let m = monitor(0);
        start(&m, "s1");
        let big = "x".repeat(20 * 1024);
        for _ in 0..20 {
            let response =
                json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":big}]}});
            record_call(&m, "s1", &call("read_capture"), &response);
        }
        let log = m.log(None, None);
        assert!(log.logging.memory_used_bytes <= log.logging.memory_limit_bytes);
        assert!(log.logging.dropped > 0);
        assert_eq!(log.entries.last().unwrap().seq, log.last_seq);
        assert_eq!(log.last_seq, 20);
    }

    #[test]
    fn log_resumes_after_a_seq_and_returns_the_newest_when_limited() {
        let m = monitor(16);
        start(&m, "s1");
        for _ in 0..5 {
            record_call(
                &m,
                "s1",
                &call("ping"),
                &json!({"jsonrpc":"2.0","id":1,"result":{}}),
            );
        }
        let after = m.log(Some(3), None);
        assert_eq!(
            after.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [4, 5]
        );
        let limited = m.log(None, Some(2));
        assert_eq!(
            limited.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [4, 5]
        );
    }

    #[test]
    fn ended_sessions_are_marked_and_logged_and_the_old_ones_pruned() {
        let m = monitor(16);
        start(&m, "live");
        for i in 0..(KEPT_ENDED_SESSIONS + 4) {
            let id = format!("gone{i}");
            start(&m, &id);
            m.session_ended(&id, "deleted by client");
        }
        let sessions = m.sessions();
        assert!(sessions[0].active, "live sessions come first");
        assert_eq!(
            sessions.iter().filter(|s| !s.active).count(),
            KEPT_ENDED_SESSIONS
        );
        let log = m.log(None, None);
        assert!(log
            .entries
            .iter()
            .all(|e| e.kind == McpEntryKind::SessionEnd));
    }

    #[test]
    fn clearing_empties_the_log_but_keeps_the_sequence() {
        let m = monitor(16);
        start(&m, "s1");
        record_call(
            &m,
            "s1",
            &call("survey"),
            &json!({"jsonrpc":"2.0","id":1,"result":{}}),
        );
        let state = m.set_logging(None, None, true);
        assert_eq!(state.entries, 0);
        assert_eq!(state.memory_used_bytes, 0);
        record_call(
            &m,
            "s1",
            &call("survey"),
            &json!({"jsonrpc":"2.0","id":1,"result":{}}),
        );
        assert_eq!(m.log(None, None).entries[0].seq, 2);
    }

    #[test]
    fn long_text_is_truncated_on_a_character_boundary() {
        let text = "é".repeat(MAX_TEXT_BYTES);
        let cut = bounded(text);
        assert!(cut.contains("truncated"));
        assert!(cut.len() < MAX_TEXT_BYTES + 100);
    }
}
