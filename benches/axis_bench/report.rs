// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Plain-text output.

use crate::analysis::{self, Diff, Table};
use crate::run_dir::Manifest;
use std::fmt::Write;
use std::path::Path;

/// What each row means, by the first part of its name.
const DESCRIPTIONS: &[(&str, &str)] = &[
    (
        analysis::AXIS_START,
        "starting the axis process, until its main()",
    ),
    (
        "cli",
        "axis start-up: async runtime, logging, arguments, policy, daemon check, config, proxy",
    ),
    (
        "sandbox",
        "sandbox create, start, wait and destroy, outside their steps",
    ),
    (
        "front_door",
        "checks of the policy, the platform and the system service",
    ),
    (
        "backend",
        "preparing the backend: policy translation and preflight checks",
    ),
    (
        "support_files",
        "workspace, temporary directory, and config or profile files",
    ),
    (
        "resource_setup",
        "resource limits: rlimits, cgroups, Windows job object",
    ),
    ("network_setup", "network isolation"),
    ("filesystem_setup", "file-system isolation"),
    ("pty_setup", "terminal setup"),
    ("child_setup", "preparing seccomp for the sandboxed process"),
    ("spawn", "starting the sandboxed process"),
    (
        "child_setup_handoff",
        "handing connection attribution to the sandboxed process",
    ),
    ("post_spawn_handoff", "watchers started after the spawn"),
    (
        analysis::BEFORE_COMMAND,
        "waiting for the sandbox runtime to reach the command's main()",
    ),
    (
        analysis::COMMAND,
        "the command itself; the no-op child takes microseconds",
    ),
    (
        analysis::AFTER_COMMAND,
        "from the command's exit until axis sees the sandbox exit",
    ),
    (
        analysis::UNTIL_COMMAND_EXIT,
        "waiting for the command; it reported no timestamps",
    ),
    ("cleanup", "cleanup after the sandbox exits"),
    (
        analysis::AXIS_EXIT,
        "from writing the timing file to the axis process exiting",
    ),
    (
        analysis::UNTRACED,
        "axis main-thread time outside every step",
    ),
    (
        analysis::TOTAL,
        "spawning axis until it exits, measured by axis-bench",
    ),
];

pub fn print_launch(manifest: &Manifest, table: &Table, run_dir: &Path) {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "axis-bench launch \"{}\"  {}",
        manifest.name, manifest.started_utc
    );
    let _ = writeln!(
        out,
        "policy {}, {} after {} per build, {} ms pause, seed {}",
        manifest.policy,
        count(manifest.runs, "round"),
        count(manifest.warmup, "warm-up"),
        manifest.pause_ms,
        manifest.seed
    );
    let busy = manifest
        .cpu_busy_percent
        .map(|busy| format!(", CPU {busy:.0}% busy during the rounds"))
        .unwrap_or_default();
    let host = &manifest.host;
    let _ = writeln!(
        out,
        "host {}: {}/{}, {} CPUs{busy}",
        host.hostname, host.os, host.arch, host.cpus
    );
    let command = if manifest.command.is_empty() {
        "axis-bench-child (prints two timestamps and exits)".to_string()
    } else {
        manifest.command.join(" ")
    };
    let daemon = if manifest.daemon_probe {
        "probed as usual"
    } else {
        "skipped"
    };
    let _ = writeln!(out, "command: {command}; daemon check: {daemon}");
    out.push_str(&builds(table, |tag| {
        manifest
            .builds
            .iter()
            .find(|build| build.tag == tag)
            .map(|build| (build.source.clone(), build.timing))
    }));
    out.push_str(&warnings(table));
    for column in &table.columns {
        let timing = manifest
            .builds
            .iter()
            .any(|build| build.tag == column.tag && build.timing);
        if timing && column.ok > 0 && column.traced == 0 {
            let _ = writeln!(
                out,
                "warning: {} had timing on but wrote no timing files, as builds older than \
                 timing support do",
                column.tag
            );
        }
    }
    out.push('\n');
    out.push_str(&self::table(table));
    out.push('\n');
    out.push_str(&legend(table, true));
    let _ = writeln!(out, "saved to {}", run_dir.display());
    print!("{out}");
}

pub fn print_compare(runs: [(&Path, &Manifest, &str); 2], table: &Table) {
    let mut out = String::from("axis-bench compare\n");
    for ((dir, manifest, build), column) in runs.iter().zip(&table.columns) {
        let _ = writeln!(
            out,
            "  {}: build {build} of {}, {}, policy {}, host {}",
            column.tag,
            dir.display(),
            manifest.started_utc,
            manifest.policy,
            manifest.host.hostname
        );
    }
    let [(_, before, _), (_, after, _)] = runs;
    for (what, differs) in [
        ("hosts", before.host.hostname != after.host.hostname),
        ("policies", before.policy != after.policy),
        ("commands", before.command != after.command),
        (
            "axis-bench versions",
            before.tool_version != after.tool_version,
        ),
    ] {
        if differs {
            let _ = writeln!(out, "warning: the runs used different {what}");
        }
    }
    out.push_str(&warnings(table));
    let _ = writeln!(
        out,
        "note: separate runs are not paired, so there is no interval; for one, measure both \
         builds in one axis-bench launch"
    );
    out.push('\n');
    out.push_str(&self::table(table));
    out.push('\n');
    out.push_str(&legend(table, false));
    print!("{out}");
}

fn count(n: u32, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

fn builds(table: &Table, info: impl Fn(&str) -> Option<(String, bool)>) -> String {
    let mut out = String::from("builds:\n");
    for column in &table.columns {
        let (source, timing) = info(&column.tag).unwrap_or_default();
        let mut facts = Vec::new();
        if let Some(backend) = &column.backend {
            facts.push(format!("backend {backend}"));
        }
        if let Some(executor) = &column.executor {
            facts.push(format!("executor {executor}"));
        }
        facts.push(format!("{} ok, {} failed", column.ok, column.failed));
        facts.push(if timing {
            format!("timing on, {} traced", column.traced)
        } else {
            "timing off".into()
        });
        let _ = writeln!(out, "  {}: {source}\n    {}", column.tag, facts.join(", "));
    }
    out
}

fn warnings(table: &Table) -> String {
    let mut out = String::new();
    let backends: Vec<&str> = table
        .columns
        .iter()
        .filter_map(|column| column.backend.as_deref())
        .collect();
    if backends.windows(2).any(|pair| pair[0] != pair[1])
        || backends.iter().any(|b| b.contains('+'))
    {
        let _ = writeln!(
            out,
            "warning: launches used different backends ({}), so their rows are not comparable",
            backends.join(", ")
        );
    }
    let untraced = table.rows.iter().position(|row| row == analysis::UNTRACED);
    let total = table.rows.len() - 1;
    for column in &table.columns {
        if column.failed > 0 {
            let _ = writeln!(
                out,
                "warning: {} launches of {} failed; see samples.jsonl",
                column.failed, column.tag
            );
        }
        if column.traced == 0 {
            continue;
        }
        if column.traced < column.ok {
            let _ = writeln!(
                out,
                "warning: {} launches of {} left no trace; its step rows use the {} that did",
                column.ok - column.traced,
                column.tag,
                column.traced
            );
        }
        let share = untraced
            .and_then(|row| column.cells[row])
            .zip(column.cells[total])
            .map(|(untraced, total)| 100.0 * untraced.median_ms / total.median_ms);
        if let Some(share) = share.filter(|share| *share > 5.0) {
            let _ = writeln!(
                out,
                "warning: untraced is {share:.0}% of total for {}; a step lacks a span",
                column.tag
            );
        }
    }
    out
}

/// Rows of medians and p90s per build, then differences from the first build.
pub fn table(table: &Table) -> String {
    let width = table
        .rows
        .iter()
        .map(String::len)
        .max()
        .unwrap_or(0)
        .max(16);
    let base = table
        .columns
        .first()
        .map_or("", |column| column.tag.as_str());
    let intervals = table
        .columns
        .iter()
        .flat_map(|column| &column.diffs)
        .any(|diff| diff.is_some_and(|diff| diff.interval_ms.is_some()));
    let interval_header = if intervals { "95% interval" } else { "" };
    let mut out = String::new();
    let _ = write!(out, "{:width$}", "");
    for column in &table.columns {
        let _ = write!(out, "  {:>17}", column.tag);
    }
    for column in table.columns.iter().skip(1) {
        let _ = write!(out, "  {:>30}", format!("{} - {base}", column.tag));
    }
    let _ = write!(out, "\n{:width$}", "ms");
    let line_start = out.len();
    for _ in &table.columns {
        let _ = write!(out, "  {:>8} {:>8}", "median", "p90");
    }
    for _ in table.columns.iter().skip(1) {
        let _ = write!(out, "  {:>8} {:>21}", "median", interval_header);
    }
    out.truncate(out.trim_end().len());
    let rule = "-".repeat(width + out.len() - line_start);
    out.push('\n');
    for (index, row) in table.rows.iter().enumerate() {
        if row == analysis::TOTAL {
            let _ = writeln!(out, "{rule}");
        }
        let _ = write!(out, "{row:width$}");
        for column in &table.columns {
            let _ = match column.cells[index] {
                Some(cell) => write!(out, "  {:>8.2} {:>8.2}", cell.median_ms, cell.p90_ms),
                None => write!(out, "  {:>8} {:>8}", "-", "-"),
            };
        }
        for column in table.columns.iter().skip(1) {
            let _ = write!(out, "  {}", diff(column.diffs[index]));
        }
        out.truncate(out.trim_end().len());
        out.push('\n');
    }
    out
}

fn diff(diff: Option<Diff>) -> String {
    let Some(diff) = diff else {
        return format!("{:>8} {:21}  ", "-", "");
    };
    let interval = diff
        .interval_ms
        .map(|(low, high)| format!("[{low:+.2}, {high:+.2}]"))
        .unwrap_or_default();
    let marker = if diff.excludes_zero() { "*" } else { " " };
    format!("{:>+8.2} {interval:>21} {marker}", diff.median_ms)
}

fn legend(table: &Table, paired: bool) -> String {
    let mut out = String::from(
        "Each row is time spent in that step on axis's main thread, not counting steps\n\
         nested inside it, so a launch's rows add up to its total.\n",
    );
    let mut seen = Vec::new();
    for row in &table.rows {
        let key = row.split('.').next().unwrap_or(row);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if let Some((_, description)) = DESCRIPTIONS.iter().find(|(name, _)| *name == key) {
            let _ = writeln!(out, "  {key:<20} {description}");
        }
    }
    if table.columns.len() > 1 {
        out.push_str(if paired {
            "x - base: median of the per-round differences and its 95% interval;\n\
             * marks an interval that excludes 0. With many rows, expect an occasional *\n\
             by chance; run one build against itself to see this machine's noise.\n"
        } else {
            "x - base: difference of the medians.\n"
        });
    }
    if table
        .columns
        .iter()
        .any(|column| column.ok > 0 && column.traced == 0)
    {
        out.push_str(
            "Builds without timing show only before command, command, and after command,\n\
             split by the child's timestamps. Rows other than total are compared only\n\
             between builds that both have timing or both do not.\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{Cell, Column};

    fn column(tag: &str, diffs: Vec<Option<Diff>>) -> Column {
        let cell = Some(Cell {
            median_ms: 2.0,
            p90_ms: 3.0,
        });
        Column {
            tag: tag.into(),
            ok: 30,
            failed: 0,
            traced: 30,
            backend: Some("LinuxMxc".into()),
            executor: None,
            cells: vec![cell, None, cell],
            diffs,
        }
    }

    #[test]
    fn table_aligns_and_marks_real_differences() {
        let diff = Diff {
            median_ms: 1.5,
            interval_ms: Some((0.5, 2.5)),
            pairs: 30,
        };
        let table = Table {
            rows: vec!["cli".into(), "backend".into(), analysis::TOTAL.into()],
            columns: vec![
                column("main", Vec::new()),
                column("mine", vec![Some(diff), None, Some(diff)]),
            ],
        };
        let text = self::table(&table);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].trim_start().starts_with("main"));
        assert!(lines[0].ends_with("mine - main"));
        assert!(lines[2].starts_with("cli"));
        assert!(lines[2].contains("+1.50"));
        assert!(lines[2].ends_with("[+0.50, +2.50] *"));
        assert!(lines[3].contains("       -        -"));
        assert!(lines[4].chars().all(|c| c == '-'));
        assert!(lines[5].starts_with("total"));
        assert!(text.is_ascii());
        assert!(legend(&table, true).contains("cli "));
    }
}
