// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Opt-in launch timing.
//!
//! When the `axis` binary starts with `AXIS_TIMING_FILE` set, named spans and
//! marks are kept in memory and written to that file as Chrome Trace Event
//! JSON, which Perfetto opens, when [`finish`] runs. Without the variable
//! every call is a no-op. Only the `axis` binary calls [`init_from_env`];
//! library code only records.
//!
//! Span names are `group.step`, for example `backend.prepare`. Benchmarks
//! group rows by the part before the first dot. The file holds names,
//! timestamps, process and thread ids, and the metadata passed to [`meta`]:
//! never arguments, environment, or policy content.

use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Names the file `axis` writes its timing trace to.
pub const TIMING_FILE_ENV: &str = "AXIS_TIMING_FILE";
/// Marks the moment timing was enabled, first thing in `main`.
pub const PROCESS_MAIN_MARK: &str = "process.main";
/// Marks the moment the timing file was written, just before exit.
pub const PROCESS_FINISH_MARK: &str = "process.finish";
/// Marks the moment a backend saw its child process exit.
pub const CHILD_REAPED_MARK: &str = "child.reaped";
/// Category of every event `axis` records.
pub const CATEGORY: &str = "axis";

static RECORDER: OnceLock<Recorder> = OnceLock::new();
static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static THREAD_ID: Cell<u64> = const { Cell::new(0) };
}

/// Enables timing when `AXIS_TIMING_FILE` names a file. Call once, as the
/// first statement of `main`, so the calling thread becomes thread 1.
pub fn init_from_env(process_name: &'static str) -> bool {
    match std::env::var_os(TIMING_FILE_ENV) {
        Some(path) if !path.is_empty() => {
            let recorder = Recorder::new(PathBuf::from(path), process_name);
            RECORDER.set(recorder).is_ok()
        }
        _ => false,
    }
}

/// Whether timing was enabled for this process.
pub fn enabled() -> bool {
    RECORDER.get().is_some()
}

/// Starts a span that ends when the returned guard is dropped.
pub fn span(name: &'static str) -> Span<'static> {
    match RECORDER.get() {
        Some(recorder) => recorder.span(name),
        None => Span::disabled(),
    }
}

/// Records an instant event.
pub fn mark(name: &'static str) {
    if let Some(recorder) = RECORDER.get() {
        recorder.mark(name);
    }
}

/// Records a key/value pair in the trace metadata. The value is formatted
/// only when timing is enabled.
pub fn meta(key: &'static str, value: impl std::fmt::Display) {
    if let Some(recorder) = RECORDER.get() {
        recorder.meta(key, value);
    }
}

/// Writes the timing file. Spans still open end now. `std::process::exit`
/// runs no destructors, so call this before every exit. Later calls do
/// nothing.
pub fn finish() {
    if let Some(recorder) = RECORDER.get() {
        recorder.finish();
    }
}

/// Guard returned by [`span`].
#[must_use = "a span ends when this guard is dropped"]
pub struct Span<'a> {
    recorder: Option<&'a Recorder>,
    id: u64,
}

impl Span<'_> {
    fn disabled() -> Self {
        Self {
            recorder: None,
            id: 0,
        }
    }
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        if let Some(recorder) = self.recorder {
            recorder.end_span(self.id);
        }
    }
}

struct Event {
    name: &'static str,
    thread: u64,
    start_us: u64,
    duration_us: Option<u64>,
    unfinished: bool,
}

#[derive(Default)]
struct State {
    events: Vec<Event>,
    open: BTreeMap<u64, Event>,
    threads: BTreeMap<u64, String>,
    metadata: BTreeMap<&'static str, String>,
    next_span: u64,
    finished: bool,
}

struct Recorder {
    path: PathBuf,
    process_name: &'static str,
    pid: u32,
    anchor: Instant,
    anchor_unix_us: u64,
    state: Mutex<State>,
}

impl Recorder {
    fn new(path: PathBuf, process_name: &'static str) -> Self {
        let recorder = Self {
            path,
            process_name,
            pid: std::process::id(),
            anchor: Instant::now(),
            anchor_unix_us: unix_micros(SystemTime::now()),
            state: Mutex::new(State::default()),
        };
        recorder.mark(PROCESS_MAIN_MARK);
        recorder
    }

    fn now_us(&self) -> u64 {
        u64::try_from(self.anchor.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn span(&self, name: &'static str) -> Span<'_> {
        let start_us = self.now_us();
        let thread = self.current_thread();
        let mut state = self.lock();
        if state.finished {
            return Span::disabled();
        }
        state.next_span += 1;
        let id = state.next_span;
        state.open.insert(
            id,
            Event {
                name,
                thread,
                start_us,
                duration_us: None,
                unfinished: false,
            },
        );
        Span {
            recorder: Some(self),
            id,
        }
    }

    fn end_span(&self, id: u64) {
        let end_us = self.now_us();
        let mut state = self.lock();
        if let Some(mut event) = state.open.remove(&id) {
            event.duration_us = Some(end_us.saturating_sub(event.start_us));
            state.events.push(event);
        }
    }

    fn mark(&self, name: &'static str) {
        let start_us = self.now_us();
        let thread = self.current_thread();
        let mut state = self.lock();
        if !state.finished {
            state.events.push(Event {
                name,
                thread,
                start_us,
                duration_us: None,
                unfinished: false,
            });
        }
    }

    fn meta(&self, key: &'static str, value: impl std::fmt::Display) {
        let mut state = self.lock();
        if !state.finished {
            state.metadata.insert(key, value.to_string());
        }
    }

    fn finish(&self) {
        self.mark(PROCESS_FINISH_MARK);
        let end_us = self.now_us();
        let trace = {
            let mut state = self.lock();
            if state.finished {
                return;
            }
            state.finished = true;
            let open = std::mem::take(&mut state.open);
            for (_, mut event) in open {
                event.duration_us = Some(end_us.saturating_sub(event.start_us));
                event.unfinished = true;
                state.events.push(event);
            }
            self.trace_json(&mut state)
        };
        if let Err(err) = write_atomically(&self.path, &trace) {
            eprintln!(
                "axis: warning: cannot write timing file {}: {err}",
                self.path.display()
            );
        }
    }

    /// The calling thread's id, recording its name the first time it is seen.
    fn current_thread(&self) -> u64 {
        let (id, first_use) = THREAD_ID
            .try_with(|cell| match cell.get() {
                0 => {
                    let id = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
                    cell.set(id);
                    (id, true)
                }
                id => (id, false),
            })
            .unwrap_or((0, false));
        if first_use {
            let name = std::thread::current()
                .name()
                .unwrap_or("thread")
                .to_string();
            self.lock().threads.entry(id).or_insert(name);
        }
        id
    }

    fn trace_json(&self, state: &mut State) -> Value {
        state.events.sort_by_key(|event| event.start_us);
        let pid = self.pid;
        let mut events = vec![json!({
            "ph": "M", "name": "process_name", "pid": pid, "tid": 0,
            "args": { "name": self.process_name },
        })];
        events.extend(state.threads.iter().map(|(tid, name)| {
            json!({
                "ph": "M", "name": "thread_name", "pid": pid, "tid": tid,
                "args": { "name": name },
            })
        }));
        events.extend(state.events.iter().map(|event| {
            let ts = self.anchor_unix_us.saturating_add(event.start_us);
            let mut value = match event.duration_us {
                Some(dur) => json!({
                    "ph": "X", "name": event.name, "cat": CATEGORY,
                    "pid": pid, "tid": event.thread, "ts": ts, "dur": dur,
                }),
                None => json!({
                    "ph": "i", "s": "t", "name": event.name, "cat": CATEGORY,
                    "pid": pid, "tid": event.thread, "ts": ts,
                }),
            };
            if event.unfinished {
                value["args"] = json!({ "unfinished": true });
            }
            value
        }));
        let mut other = serde_json::Map::new();
        other.insert("process".into(), json!(self.process_name));
        other.insert("pid".into(), json!(pid));
        for (key, value) in &state.metadata {
            other.insert((*key).into(), json!(value));
        }
        json!({
            "traceEvents": events,
            "displayTimeUnit": "ms",
            "otherData": other,
        })
    }
}

fn unix_micros(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn write_atomically(path: &Path, trace: &Value) -> std::io::Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, serde_json::to_vec(trace)?)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_trace(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).expect("trace file")).expect("trace JSON")
    }

    fn events_named<'a>(trace: &'a Value, name: &str) -> Vec<&'a Value> {
        trace["traceEvents"]
            .as_array()
            .expect("events")
            .iter()
            .filter(|event| event["name"] == name)
            .collect()
    }

    #[test]
    fn global_calls_are_noops_without_init() {
        assert!(!enabled());
        let guard = span("test.disabled");
        assert!(guard.recorder.is_none());
        drop(guard);
        mark("test.disabled");
        meta("key", "value");
        finish();
    }

    #[test]
    fn writes_nested_spans_marks_and_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trace.json");
        let recorder = Recorder::new(path.clone(), "axis");
        {
            let _outer = recorder.span("outer.step");
            let _inner = recorder.span("inner.step");
            recorder.mark("test.mark");
        }
        recorder.meta("backend", "LinuxMxc");
        recorder.finish();

        let trace = read_trace(&path);
        assert_eq!(trace["otherData"]["backend"], "LinuxMxc");
        assert_eq!(trace["otherData"]["process"], "axis");
        let outer = events_named(&trace, "outer.step")[0];
        let inner = events_named(&trace, "inner.step")[0];
        assert_eq!(outer["ph"], "X");
        assert_eq!(outer["cat"], CATEGORY);
        let start = |event: &Value| event["ts"].as_u64().expect("ts");
        let end = |event: &Value| start(event) + event["dur"].as_u64().expect("dur");
        assert!(start(outer) <= start(inner) && end(inner) <= end(outer));
        assert_eq!(events_named(&trace, "test.mark")[0]["ph"], "i");
        let main = events_named(&trace, PROCESS_MAIN_MARK)[0];
        let finished = events_named(&trace, PROCESS_FINISH_MARK)[0];
        assert!(start(main) <= start(outer) && end(outer) <= start(finished));
        assert!(!dir.path().join("trace.json.tmp").exists());
    }

    #[test]
    fn finish_closes_open_spans_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trace.json");
        let recorder = Recorder::new(path.clone(), "axis");
        let open = recorder.span("still.open");
        recorder.finish();
        drop(open);
        let late = recorder.span("after.finish");
        drop(late);
        recorder.finish();

        let trace = read_trace(&path);
        let open = events_named(&trace, "still.open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["args"]["unfinished"], true);
        assert!(events_named(&trace, "after.finish").is_empty());
        assert_eq!(events_named(&trace, PROCESS_FINISH_MARK).len(), 1);
    }

    #[test]
    fn threads_get_distinct_named_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trace.json");
        let recorder = Recorder::new(path.clone(), "axis");
        drop(recorder.span("caller.step"));
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("timing-worker".into())
                .spawn_scoped(scope, || drop(recorder.span("worker.step")))
                .expect("spawn");
        });
        recorder.finish();

        let trace = read_trace(&path);
        let caller = events_named(&trace, "caller.step")[0]["tid"].clone();
        let worker = events_named(&trace, "worker.step")[0]["tid"].clone();
        assert_ne!(caller, worker);
        let worker_name = events_named(&trace, "thread_name")
            .into_iter()
            .find(|event| event["tid"] == worker)
            .expect("worker thread name");
        assert_eq!(worker_name["args"]["name"], "timing-worker");
    }

    #[test]
    fn unwritable_path_only_warns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = Recorder::new(dir.path().join("missing").join("trace.json"), "axis");
        drop(recorder.span("some.step"));
        recorder.finish();
    }
}
