// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Splits launches into rows and summarizes them.
//!
//! A row is self time on axis's main thread: the time inside a span minus the
//! spans nested in it, grouped by the span name up to `depth` dot-separated
//! parts. The time `sandbox.wait` spends waiting is split by the child's
//! timestamps into before command, command, and after command, and where
//! processes were recorded, the waits before and after the command are split
//! again by the newest process running. Every launch's rows therefore add up
//! to its total, and the remainder is `untraced`.

use crate::run_dir::{Manifest, Process, Sample};
use axis_core::timing::{CATEGORY, CHILD_REAPED_MARK, PROCESS_FINISH_MARK, PROCESS_MAIN_MARK};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

pub const AXIS_START: &str = "axis start";
pub const BEFORE_COMMAND: &str = "before command";
pub const COMMAND: &str = "command";
pub const AFTER_COMMAND: &str = "after command";
pub const UNTIL_COMMAND_EXIT: &str = "until command exit";
pub const AXIS_EXIT: &str = "axis exit";
pub const UNTRACED: &str = "untraced";
pub const TOTAL: &str = "total";
const WAIT_SPAN: &str = "sandbox.wait";
/// Programs that run beside a console program instead of doing its work.
const BYSTANDERS: &[&str] = &["conhost"];

/// The `axis` events of a trace.
#[derive(Debug, Default)]
pub struct AxisTrace {
    spans: Vec<TraceSpan>,
    marks: Vec<TraceMark>,
    backend: Option<String>,
    executor: Option<String>,
}

#[derive(Debug)]
struct TraceSpan {
    name: String,
    tid: u64,
    start: u64,
    end: u64,
}

#[derive(Debug)]
struct TraceMark {
    name: String,
    tid: u64,
    ts: u64,
}

impl AxisTrace {
    /// Reads an `axis` timing file, or a merged trace that holds one.
    pub fn parse(trace: &Value) -> Self {
        let mut parsed = Self::default();
        for event in trace["traceEvents"].as_array().into_iter().flatten() {
            let (Some(name), Some(ts)) = (event["name"].as_str(), event["ts"].as_u64()) else {
                continue;
            };
            if event["cat"] != CATEGORY {
                continue;
            }
            let tid = event["tid"].as_u64().unwrap_or(0);
            match event["ph"].as_str() {
                Some("X") => parsed.spans.push(TraceSpan {
                    name: name.into(),
                    tid,
                    start: ts,
                    end: ts + event["dur"].as_u64().unwrap_or(0),
                }),
                Some("i") => parsed.marks.push(TraceMark {
                    name: name.into(),
                    tid,
                    ts,
                }),
                _ => {}
            }
        }
        let metadata = match &trace["otherData"]["axis"] {
            Value::Object(_) => &trace["otherData"]["axis"],
            _ => &trace["otherData"],
        };
        parsed.backend = metadata["backend"].as_str().map(Into::into);
        parsed.executor = metadata["executor"].as_str().map(Into::into);
        parsed
    }

    fn mark(&self, name: &str) -> Option<&TraceMark> {
        self.marks.iter().find(|mark| mark.name == name)
    }
}

/// One launch split into rows, in the order they first happened.
#[derive(Debug)]
pub struct Launch {
    pub round: u32,
    pub ok: bool,
    pub total_us: u64,
    pub rows: Vec<(String, u64)>,
    pub traced: bool,
    pub backend: Option<String>,
    pub executor: Option<String>,
}

impl Launch {
    fn value(&self, row: &str) -> Option<u64> {
        if row == TOTAL {
            return Some(self.total_us);
        }
        self.rows
            .iter()
            .find(|(name, _)| name == row)
            .map(|(_, us)| *us)
    }
}

pub fn load_launch(run_dir: &Path, sample: &Sample, depth: usize) -> Launch {
    let trace = sample
        .trace
        .as_ref()
        .and_then(|relative| std::fs::read(run_dir.join(relative)).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .map(|trace| AxisTrace::parse(&trace));
    breakdown(sample, trace.as_ref(), depth)
}

pub fn breakdown(sample: &Sample, trace: Option<&AxisTrace>, depth: usize) -> Launch {
    let mut rows = Rows::default();
    let child = sample.child_marks();
    let bounds = trace.and_then(|trace| {
        Some((
            trace.mark(PROCESS_MAIN_MARK)?,
            trace.mark(PROCESS_FINISH_MARK)?,
            trace,
        ))
    });
    match bounds {
        Some((main, finish, trace)) => {
            rows.add(AXIS_START, main.ts.saturating_sub(sample.spawn_unix_us));
            add_spans(&mut rows, trace, main.tid, child, &sample.processes, depth);
            rows.add(AXIS_EXIT, sample.exit_unix_us.saturating_sub(finish.ts));
            let untraced = sample.total_us.saturating_sub(rows.sum());
            rows.add(UNTRACED, untraced);
        }
        None => {
            if let Some((main, exit)) = child {
                let processes = &sample.processes;
                let before = (sample.spawn_unix_us, main);
                add_by_process(&mut rows, BEFORE_COMMAND, before, processes, depth);
                rows.add(COMMAND, exit - main);
                let after = (exit, sample.exit_unix_us);
                add_by_process(&mut rows, AFTER_COMMAND, after, processes, depth);
            }
        }
    }
    Launch {
        round: sample.round,
        ok: sample.ok(),
        total_us: sample.total_us,
        rows: rows.0,
        traced: bounds.is_some(),
        backend: trace.and_then(|trace| trace.backend.clone()),
        executor: trace.and_then(|trace| trace.executor.clone()),
    }
}

#[derive(Default)]
struct Rows(Vec<(String, u64)>);

impl Rows {
    fn add(&mut self, name: &str, us: u64) {
        match self.0.iter_mut().find(|(row, _)| row == name) {
            Some((_, total)) => *total += us,
            None => self.0.push((name.into(), us)),
        }
    }

    fn sum(&self) -> u64 {
        self.0.iter().map(|(_, us)| us).sum()
    }
}

fn group(name: &str, depth: usize) -> String {
    name.split('.')
        .take(depth.max(1))
        .collect::<Vec<_>>()
        .join(".")
}

/// Adds `[from, to)` to `row` or, where processes were recorded, to
/// `row.<program>` for the newest process running at each moment.
fn add_by_process(
    rows: &mut Rows,
    row: &str,
    (from, to): (u64, u64),
    processes: &[Process],
    depth: usize,
) {
    if processes.is_empty() {
        rows.add(row, to.saturating_sub(from));
        return;
    }
    let mut cuts: Vec<u64> = processes
        .iter()
        .flat_map(|process| [Some(process.start_us), process.exit_us])
        .flatten()
        .filter(|&at| from < at && at < to)
        .collect();
    cuts.sort_unstable();
    let mut start = from;
    for end in cuts.into_iter().chain([to]) {
        if end > start {
            let name = match newest_running(processes, start) {
                Some(process) => format!("{row}.{}", process.name.replace('.', "_")),
                None => row.to_string(),
            };
            rows.add(&group(&name, depth), end - start);
        }
        start = end;
    }
}

fn newest_running(processes: &[Process], at: u64) -> Option<&Process> {
    processes
        .iter()
        .filter(|process| process.start_us <= at && process.exit_us.is_none_or(|exit| at < exit))
        .filter(|process| {
            !BYSTANDERS
                .iter()
                .any(|name| process.name.eq_ignore_ascii_case(name))
        })
        .max_by_key(|process| process.start_us)
}

struct SpanTree<'a> {
    spans: Vec<&'a TraceSpan>,
    children: Vec<Vec<usize>>,
    child: Option<(u64, u64)>,
    reaped: Option<u64>,
    processes: &'a [Process],
    depth: usize,
    wait_split: bool,
}

fn add_spans(
    rows: &mut Rows,
    trace: &AxisTrace,
    main_tid: u64,
    child: Option<(u64, u64)>,
    processes: &[Process],
    depth: usize,
) {
    let mut spans: Vec<&TraceSpan> = trace
        .spans
        .iter()
        .filter(|span| span.tid == main_tid)
        .collect();
    spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
    let mut children = vec![Vec::new(); spans.len()];
    let mut roots = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        while open.last().is_some_and(|&top| spans[top].end <= span.start) {
            open.pop();
        }
        match open.last() {
            Some(&parent) => children[parent].push(index),
            None => roots.push(index),
        }
        open.push(index);
    }
    let mut tree = SpanTree {
        spans,
        children,
        child,
        reaped: trace.mark(CHILD_REAPED_MARK).map(|mark| mark.ts),
        processes,
        depth,
        wait_split: false,
    };
    for root in roots {
        tree.visit(root, rows);
    }
}

impl SpanTree<'_> {
    fn visit(&mut self, index: usize, rows: &mut Rows) {
        let span = self.spans[index];
        let mut gaps = Vec::new();
        let mut cursor = span.start;
        for &child in &self.children[index] {
            let child = self.spans[child];
            if child.start > cursor {
                gaps.push((cursor, child.start.min(span.end)));
            }
            cursor = cursor.max(child.end.min(span.end));
        }
        if span.end > cursor {
            gaps.push((cursor, span.end));
        }
        if span.name == WAIT_SPAN && !self.wait_split {
            self.wait_split = true;
            for gap in gaps {
                self.split_wait(gap, rows);
            }
        } else {
            let self_us = gaps.iter().map(|(from, to)| to.saturating_sub(*from)).sum();
            rows.add(&group(&span.name, self.depth), self_us);
        }
        for child in self.children[index].clone() {
            self.visit(child, rows);
        }
    }

    fn split_wait(&self, (from, to): (u64, u64), rows: &mut Rows) {
        let mut cuts: Vec<u64> = [
            self.child.map(|c| c.0),
            self.child.map(|c| c.1),
            self.reaped,
        ]
        .into_iter()
        .flatten()
        .filter(|&at| from < at && at < to)
        .collect();
        cuts.sort_unstable();
        let mut start = from;
        for end in cuts.into_iter().chain([to]) {
            if end > start {
                let row = self.wait_row(start);
                if row == AFTER_COMMAND {
                    // Lists a command that took no time, between the waits around it.
                    rows.add(COMMAND, 0);
                }
                if [BEFORE_COMMAND, AFTER_COMMAND, UNTIL_COMMAND_EXIT].contains(&row.as_str()) {
                    add_by_process(rows, &row, (start, end), self.processes, self.depth);
                } else {
                    rows.add(&row, end - start);
                }
            }
            start = end;
        }
    }

    fn wait_row(&self, at: u64) -> String {
        if self.reaped.is_some_and(|reaped| at >= reaped) {
            return group(WAIT_SPAN, self.depth);
        }
        match self.child {
            Some((main, _)) if at < main => BEFORE_COMMAND.into(),
            Some((_, exit)) if at < exit => COMMAND.into(),
            Some(_) => AFTER_COMMAND.into(),
            None => UNTIL_COMMAND_EXIT.into(),
        }
    }
}

/// Rows by build, with medians, p90s, and differences from the first build.
#[derive(Debug, Serialize)]
pub struct Table {
    pub rows: Vec<String>,
    pub columns: Vec<Column>,
}

#[derive(Debug, Serialize)]
pub struct Column {
    pub tag: String,
    pub ok: usize,
    pub failed: usize,
    pub traced: usize,
    pub backend: Option<String>,
    pub executor: Option<String>,
    /// One per row; `None` when no launch of this build has the row.
    pub cells: Vec<Option<Cell>>,
    /// One per row, against the first column; empty for the first column.
    pub diffs: Vec<Option<Diff>>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Cell {
    pub median_ms: f64,
    pub p90_ms: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Diff {
    pub median_ms: f64,
    /// 95% interval for the median paired difference; paired runs only.
    pub interval_ms: Option<(f64, f64)>,
    pub pairs: usize,
}

impl Diff {
    pub fn excludes_zero(&self) -> bool {
        self.interval_ms
            .is_some_and(|(low, high)| low > 0.0 || high < 0.0)
    }
}

pub fn run_table(run_dir: &Path, manifest: &Manifest, samples: &[Sample], depth: usize) -> Table {
    let columns: Vec<(String, Vec<Launch>)> = manifest
        .builds
        .iter()
        .map(|build| {
            let launches = samples
                .iter()
                .filter(|sample| sample.build == build.tag)
                .map(|sample| load_launch(run_dir, sample, depth))
                .collect();
            (build.tag.clone(), launches)
        })
        .collect();
    table(&columns, true)
}

/// Successful launches of one build. When some have traces, only those give
/// the step rows, since a launch without one has coarser rows.
struct Build<'a> {
    all: Vec<&'a Launch>,
    rows: Vec<&'a Launch>,
    traced: bool,
}

impl<'a> Build<'a> {
    fn new(launches: &'a [Launch]) -> Self {
        let all: Vec<&Launch> = launches.iter().filter(|launch| launch.ok).collect();
        let traced = all.iter().any(|launch| launch.traced);
        let rows = all
            .iter()
            .copied()
            .filter(|launch| launch.traced == traced)
            .collect();
        Self { all, rows, traced }
    }

    fn for_row(&self, row: &str) -> &[&'a Launch] {
        if row == TOTAL { &self.all } else { &self.rows }
    }
}

/// `paired` launches share round numbers, so differences are taken per round.
/// Step rows are compared only between builds that both have traces or both
/// do not; `total` is always compared.
pub fn table(columns: &[(String, Vec<Launch>)], paired: bool) -> Table {
    let builds: Vec<Build> = columns
        .iter()
        .map(|(_, launches)| Build::new(launches))
        .collect();
    let mut rows: Vec<String> = Vec::new();
    for traced in [true, false] {
        for build in builds.iter().filter(|build| build.traced == traced) {
            for launch in &build.rows {
                for (row, _) in &launch.rows {
                    if !rows.contains(row) {
                        rows.push(row.clone());
                    }
                }
            }
        }
    }
    for last in [AXIS_EXIT, UNTRACED] {
        if let Some(position) = rows.iter().position(|row| row == last) {
            let row = rows.remove(position);
            rows.push(row);
        }
    }
    rows.push(TOTAL.into());

    let columns = columns
        .iter()
        .zip(&builds)
        .enumerate()
        .map(|(index, ((tag, launches), build))| {
            let cells = rows
                .iter()
                .map(|row| cell(build.for_row(row), row))
                .collect();
            let diffs = match builds.first() {
                Some(baseline) if index > 0 => rows
                    .iter()
                    .map(|row| diff(baseline, build, row, paired))
                    .collect(),
                _ => Vec::new(),
            };
            let ok = &build.all;
            Column {
                tag: tag.clone(),
                ok: ok.len(),
                failed: launches.len() - ok.len(),
                traced: ok.iter().filter(|launch| launch.traced).count(),
                backend: common(ok.iter().map(|launch| launch.backend.as_deref())),
                executor: common(ok.iter().map(|launch| launch.executor.as_deref())),
                cells,
                diffs,
            }
        })
        .collect();
    Table { rows, columns }
}

fn values(launches: &[&Launch], row: &str) -> Option<Vec<f64>> {
    launches
        .iter()
        .any(|launch| launch.value(row).is_some())
        .then(|| {
            let mut values: Vec<f64> = launches
                .iter()
                .map(|launch| ms(launch.value(row).unwrap_or(0)))
                .collect();
            values.sort_by(f64::total_cmp);
            values
        })
}

fn cell(launches: &[&Launch], row: &str) -> Option<Cell> {
    let values = values(launches, row)?;
    Some(Cell {
        median_ms: median(&values)?,
        p90_ms: p90(&values)?,
    })
}

fn diff(baseline: &Build, build: &Build, row: &str, paired: bool) -> Option<Diff> {
    if row != TOTAL && baseline.traced != build.traced {
        return None;
    }
    let (before, after) = (baseline.for_row(row), build.for_row(row));
    let (before_values, after_values) = (values(before, row)?, values(after, row)?);
    if !paired {
        return Some(Diff {
            median_ms: median(&after_values)? - median(&before_values)?,
            interval_ms: None,
            pairs: 0,
        });
    }
    let by_round: HashMap<u32, &Launch> = before
        .iter()
        .map(|launch| (launch.round, *launch))
        .collect();
    let mut differences: Vec<f64> = after
        .iter()
        .filter_map(|launch| {
            let base = by_round.get(&launch.round)?;
            Some(ms(launch.value(row).unwrap_or(0)) - ms(base.value(row).unwrap_or(0)))
        })
        .collect();
    differences.sort_by(f64::total_cmp);
    Some(Diff {
        median_ms: median(&differences)?,
        interval_ms: median_interval(&differences),
        pairs: differences.len(),
    })
}

fn common<'a>(values: impl Iterator<Item = Option<&'a str>>) -> Option<String> {
    let distinct: BTreeSet<&str> = values.flatten().collect();
    (!distinct.is_empty()).then(|| distinct.into_iter().collect::<Vec<_>>().join(" + "))
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

pub fn median(sorted: &[f64]) -> Option<f64> {
    let n = sorted.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(sorted[n / 2]),
        _ => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0),
    }
}

/// Nearest-rank 90th percentile.
pub fn p90(sorted: &[f64]) -> Option<f64> {
    let rank = (sorted.len() * 9).div_ceil(10);
    sorted.get(rank.checked_sub(1)?).copied()
}

/// Distribution-free 95% interval for the median of `sorted`, from the order
/// statistics a sign test allows. `None` below 6 values.
pub fn median_interval(sorted: &[f64]) -> Option<(f64, f64)> {
    let n = sorted.len();
    let mut cumulative = 0.0;
    let mut log_pmf = n as f64 * 0.5f64.ln();
    let mut k = 0;
    for i in 0..n {
        cumulative += log_pmf.exp();
        if cumulative > 0.025 {
            break;
        }
        k = i + 1;
        log_pmf += ((n - i) as f64 / (i + 1) as f64).ln();
    }
    (k > 0).then(|| (sorted[k - 1], sorted[n - k]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(child: Option<(u64, u64)>) -> Sample {
        Sample {
            build: "a".into(),
            round: 0,
            exit_code: Some(0),
            error: None,
            spawn_unix_us: 1_000,
            exit_unix_us: 2_000,
            total_us: 1_000,
            child_main_us: child.map(|c| c.0),
            child_exit_us: child.map(|c| c.1),
            trace: None,
            processes: Vec::new(),
        }
    }

    fn with_processes(child: (u64, u64)) -> Sample {
        let process = |name: &str, pid, start_us, exit_us| Process {
            name: name.into(),
            pid,
            start_us,
            exit_us: Some(exit_us),
        };
        Sample {
            processes: vec![
                process("axis", 1, 1_000, 2_000),
                process("wxc-exec", 2, 1_250, 1_750),
                process("conhost", 3, 1_260, 1_750),
                process("bfscfg", 4, 1_320, 1_350),
                process("axis-bench-child", 5, 1_380, 1_610),
            ],
            ..sample(Some(child))
        }
    }

    fn span_on(tid: u64, name: &str, ts: u64, dur: u64) -> Value {
        json!({"ph": "X", "cat": CATEGORY, "name": name, "tid": tid, "ts": ts, "dur": dur})
    }

    fn span(name: &str, ts: u64, dur: u64) -> Value {
        span_on(1, name, ts, dur)
    }

    fn mark(name: &str, ts: u64) -> Value {
        json!({"ph": "i", "cat": CATEGORY, "name": name, "tid": 1, "ts": ts})
    }

    fn trace() -> AxisTrace {
        AxisTrace::parse(&json!({
            "traceEvents": [
                mark(PROCESS_MAIN_MARK, 1_100),
                span("cli.runtime", 1_100, 50),
                span("sandbox.create", 1_200, 100),
                span("front_door.check", 1_210, 20),
                span("backend.prepare", 1_240, 50),
                span("backend.preflight.x", 1_250, 10),
                span("sandbox.wait", 1_300, 600),
                mark(CHILD_REAPED_MARK, 1_800),
                span("cleanup.after_stop", 1_810, 80),
                span("cleanup.parent_death_guard", 1_820, 50),
                span_on(2, "worker.ignored", 1_300, 500),
                mark(PROCESS_FINISH_MARK, 1_950),
            ],
            "otherData": {"backend": "LinuxMxc"},
        }))
    }

    fn row(launch: &Launch, name: &str) -> u64 {
        launch
            .value(name)
            .unwrap_or_else(|| panic!("no row {name}: {:?}", launch.rows))
    }

    #[test]
    fn rows_are_self_time_and_add_up_to_total() {
        let launch = breakdown(&sample(Some((1_400, 1_600))), Some(&trace()), 1);
        assert_eq!(launch.backend.as_deref(), Some("LinuxMxc"));
        assert_eq!(row(&launch, AXIS_START), 100);
        assert_eq!(row(&launch, "cli"), 50);
        assert_eq!(row(&launch, "sandbox"), 30 + 20);
        assert_eq!(row(&launch, "front_door"), 20);
        assert_eq!(row(&launch, "backend"), 50);
        assert_eq!(row(&launch, BEFORE_COMMAND), 100);
        assert_eq!(row(&launch, COMMAND), 200);
        assert_eq!(row(&launch, AFTER_COMMAND), 200);
        assert_eq!(row(&launch, "cleanup"), 80);
        assert_eq!(row(&launch, AXIS_EXIT), 50);
        assert_eq!(row(&launch, UNTRACED), 100);
        let sum: u64 = launch.rows.iter().map(|(_, us)| us).sum();
        assert_eq!(sum, launch.total_us);
        assert!(launch.value("worker").is_none());
    }

    #[test]
    fn depth_two_splits_groups() {
        let launch = breakdown(&sample(Some((1_400, 1_600))), Some(&trace()), 2);
        assert_eq!(row(&launch, "backend.prepare"), 40);
        assert_eq!(row(&launch, "backend.preflight"), 10);
        assert_eq!(row(&launch, "cleanup.after_stop"), 30);
        assert_eq!(row(&launch, "cleanup.parent_death_guard"), 50);
        assert_eq!(row(&launch, "sandbox.wait"), 20);
    }

    #[test]
    fn command_row_stays_when_the_command_takes_no_time() {
        let launch = breakdown(&sample(Some((1_500, 1_500))), Some(&trace()), 1);
        assert_eq!(row(&launch, BEFORE_COMMAND), 200);
        assert_eq!(row(&launch, COMMAND), 0);
        assert_eq!(row(&launch, AFTER_COMMAND), 300);
    }

    #[test]
    fn without_child_marks_the_wait_is_one_row() {
        let launch = breakdown(&sample(None), Some(&trace()), 1);
        assert_eq!(row(&launch, UNTIL_COMMAND_EXIT), 500);
        assert!(launch.value(COMMAND).is_none());
    }

    #[test]
    fn without_a_trace_only_child_marks_split_the_launch() {
        let launch = breakdown(&sample(Some((1_300, 1_400))), None, 1);
        assert!(!launch.traced);
        assert_eq!(row(&launch, BEFORE_COMMAND), 300);
        assert_eq!(row(&launch, COMMAND), 100);
        assert_eq!(row(&launch, AFTER_COMMAND), 600);
        assert!(breakdown(&sample(None), None, 1).rows.is_empty());
    }

    #[test]
    fn waits_are_split_by_the_newest_running_process() {
        let sample = with_processes((1_400, 1_600));
        let launch = breakdown(&sample, Some(&trace()), 2);
        assert_eq!(row(&launch, "before command.wxc-exec"), 20 + 30);
        assert_eq!(row(&launch, "before command.bfscfg"), 30);
        assert_eq!(row(&launch, "before command.axis-bench-child"), 20);
        assert_eq!(row(&launch, COMMAND), 200);
        assert_eq!(row(&launch, "after command.axis-bench-child"), 10);
        assert_eq!(row(&launch, "after command.wxc-exec"), 140);
        assert_eq!(row(&launch, "after command.axis"), 50);
        assert!(launch.value("before command.conhost").is_none());
        let position = |name: &str| launch.rows.iter().position(|(row, _)| row == name).unwrap();
        assert!(position("before command.axis-bench-child") < position(COMMAND));
        assert!(position(COMMAND) < position("after command.axis-bench-child"));
        let sum: u64 = launch.rows.iter().map(|(_, us)| us).sum();
        assert_eq!(sum, launch.total_us);

        let launch = breakdown(&sample, Some(&trace()), 1);
        assert_eq!(row(&launch, BEFORE_COMMAND), 100);
        assert_eq!(row(&launch, AFTER_COMMAND), 200);

        let launch = breakdown(&sample, None, 2);
        assert_eq!(row(&launch, "before command.axis"), 250);
        assert_eq!(row(&launch, "before command.wxc-exec"), 70 + 30);
        assert_eq!(row(&launch, "after command.axis"), 250);
    }

    #[test]
    fn child_marks_outside_the_launch_are_ignored() {
        assert_eq!(sample(Some((900, 1_500))).child_marks(), None);
        assert_eq!(sample(Some((1_500, 1_400))).child_marks(), None);
    }

    #[test]
    fn statistics() {
        assert_eq!(median(&[1.0, 2.0, 3.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
        let sixteen: Vec<f64> = (1..=16).map(f64::from).collect();
        assert_eq!(p90(&sixteen), Some(15.0));
        assert_eq!(median_interval(&sixteen), Some((4.0, 13.0)));
        let thirty: Vec<f64> = (1..=30).map(f64::from).collect();
        assert_eq!(median_interval(&thirty), Some((10.0, 21.0)));
        assert_eq!(median_interval(&[1.0, 2.0, 3.0, 4.0, 5.0]), None);
    }

    #[test]
    fn step_rows_are_compared_only_between_builds_traced_alike() {
        let launches = |trace: Option<&AxisTrace>| -> Vec<Launch> {
            (0..6)
                .map(|round| {
                    let mut sample = sample(Some((1_400, 1_600)));
                    sample.round = round;
                    breakdown(&sample, trace, 1)
                })
                .collect()
        };
        let trace = trace();
        let table = table(
            &[
                ("old".into(), launches(None)),
                ("new".into(), launches(Some(&trace))),
            ],
            true,
        );
        let index = |name: &str| table.rows.iter().position(|row| row == name).expect(name);
        assert_eq!(table.rows[0], AXIS_START);
        assert!(table.columns[0].cells[index(BEFORE_COMMAND)].is_some());
        assert!(table.columns[0].cells[index(UNTRACED)].is_none());
        assert!(table.columns[1].diffs[index(BEFORE_COMMAND)].is_none());
        assert!(table.columns[1].diffs[index(TOTAL)].is_some());
    }

    #[test]
    fn paired_table_reports_differences_per_round() {
        let launches = |offset: u64| -> Vec<Launch> {
            (0..10)
                .map(|round| {
                    let mut sample = sample(None);
                    sample.round = round;
                    sample.total_us = 1_000 + offset + u64::from(round);
                    breakdown(&sample, None, 1)
                })
                .collect()
        };
        let table = table(
            &[("a".into(), launches(0)), ("b".into(), launches(500))],
            true,
        );
        assert_eq!(table.rows, vec![TOTAL.to_string()]);
        let diff = table.columns[1].diffs[0].expect("diff");
        assert_eq!(diff.pairs, 10);
        assert!((diff.median_ms - 0.5).abs() < 1e-9);
        assert!(diff.excludes_zero());
    }
}
