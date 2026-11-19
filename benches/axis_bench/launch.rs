// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `axis-bench launch`: times `axis run` for one or more builds.

use crate::processes::Watcher;
use crate::run_dir::{self, BuildInfo, Manifest, Process, Sample};
use crate::stage::{Stage, StagedBuild};
use crate::{Result, analysis, host, report};
use axis_core::timing::TIMING_FILE_ENV;
use serde_json::{Value, json};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CHILD_LINE: &str = "AXIS_BENCH_CHILD ";
/// Trace process that holds one thread per recorded process. It is above
/// Linux's largest pid and not a multiple of 4 like Windows pids, so it
/// cannot clash with a real process.
const PROCESSES_TRACK: u32 = 4_194_305;

#[derive(clap::Args)]
pub struct LaunchArgs {
    /// A build to time: the folder holding `axis` and its helpers, usually a
    /// checkout's target/release, or the `axis` binary. Repeat to compare
    /// builds; the first one is the baseline. Default: the folder holding
    /// axis-bench.
    #[arg(long = "build", value_name = "FOLDER")]
    builds: Vec<PathBuf>,
    /// Name of each --build, in the same order, used as its column header and
    /// trace folder. Default: the name of the checkout holding the build.
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Policy for every launch: a built-in name or a file.
    #[arg(long, default_value = "minimal")]
    policy: String,
    /// Measured launches per build.
    #[arg(long, default_value_t = 30)]
    runs: u32,
    /// Unmeasured launches per build before the first round.
    #[arg(long, default_value_t = 2)]
    warmup: u32,
    /// Pause before each launch, in milliseconds.
    #[arg(long, default_value_t = 500)]
    pause_ms: u64,
    /// Seconds before a launch is killed and counted as failed.
    #[arg(long, default_value_t = 60)]
    timeout: u64,
    /// Name of this run, used in its directory name.
    #[arg(long, default_value = "launch")]
    name: String,
    /// Directory that receives the run directory.
    #[arg(long, value_name = "DIR", default_value = "bench-runs")]
    output: PathBuf,
    /// Seed for the launch order (default: the clock).
    #[arg(long)]
    seed: Option<u64>,
    /// How many dot-separated parts of a step name make a row.
    #[arg(long, default_value_t = 1)]
    depth: usize,
    /// Do not ask any build to write timing files.
    #[arg(long)]
    no_timing: bool,
    /// Do not ask the build with this tag to write timing files. Repeatable.
    #[arg(long, value_name = "TAG")]
    timing_off: Vec<String>,
    /// Let axis look for the daemon as usual instead of skipping it.
    #[arg(long)]
    daemon_probe: bool,
    /// Directory for the private copies of the builds.
    #[arg(long, value_name = "DIR")]
    stage_dir: Option<PathBuf>,
    /// No-op program to launch (default: axis-bench-child beside axis-bench).
    #[arg(long)]
    child: Option<PathBuf>,
    /// Command to launch instead of the no-op program.
    #[arg(last = true)]
    command: Vec<String>,
}

pub fn run(args: LaunchArgs) -> Result<()> {
    let builds = resolve_builds(&args.builds, &args.tags)?;
    if let Some(tag) = args
        .timing_off
        .iter()
        .find(|tag| !builds.iter().any(|(build, _)| build == *tag))
    {
        return Err(format!("--timing-off {tag}: no build has that tag").into());
    }
    if args.runs == 0 {
        return Err("--runs must be at least 1".into());
    }
    let child = if args.command.is_empty() {
        Some(find_child(args.child.as_deref())?)
    } else {
        None
    };
    let policy = match Path::new(&args.policy) {
        path if path.is_file() => std::path::absolute(path)?.display().to_string(),
        _ => args.policy.clone(),
    };

    let stage = Stage::new(args.stage_dir.as_deref())?;
    let staged = builds
        .iter()
        .map(|(tag, path)| stage.stage_build(tag, path))
        .collect::<Result<Vec<_>>>()?;
    let timing: Vec<bool> = staged
        .iter()
        .map(|build| !args.no_timing && !args.timing_off.contains(&build.tag))
        .collect();
    let (stamp, started_utc) = host::utc_stamps();
    // Absolute, because axis writes its timing file from the launch workspace.
    let dir = std::path::absolute(run_dir::new_path(&args.output, &stamp, &args.name)?)?;
    let seed = args.seed.unwrap_or_else(host::unix_micros);
    let mut manifest = Manifest {
        tool_version: env!("CARGO_PKG_VERSION").into(),
        name: args.name.clone(),
        started_utc,
        policy: policy.clone(),
        runs: args.runs,
        warmup: args.warmup,
        pause_ms: args.pause_ms,
        seed,
        daemon_probe: args.daemon_probe,
        command: args.command.clone(),
        host: host::describe(),
        cpu_busy_percent: None,
        builds: staged
            .iter()
            .zip(&timing)
            .map(|(build, &timing)| BuildInfo {
                tag: build.tag.clone(),
                source: build.source.display().to_string(),
                timing,
                files: build.files.clone(),
            })
            .collect(),
    };

    let watcher = Watcher::start();
    let launcher = Launcher {
        stage: &stage,
        run_dir: &dir,
        policy: &policy,
        child: child.as_deref(),
        command: &args.command,
        daemon_probe: args.daemon_probe,
        timeout: Duration::from_secs(args.timeout),
        watcher: watcher.as_ref(),
    };
    let pause = Duration::from_millis(args.pause_ms);
    for (build, &timing) in staged.iter().zip(&timing) {
        for _ in 0..args.warmup {
            std::thread::sleep(pause);
            let sample = launcher.launch(build, None, timing)?;
            if let Some(error) = sample.error {
                return Err(format!("warm-up launch of {} failed: {error}", build.tag).into());
            }
        }
    }
    run_dir::create(&dir)?;
    run_dir::write_json(&dir.join(run_dir::MANIFEST), &manifest)?;

    let mut rng = host::Rng::new(seed);
    let progress = std::io::stderr().is_terminal();
    let cpu_before = host::cpu_times();
    let mut samples = Vec::new();
    for round in 0..args.runs {
        let mut order: Vec<usize> = (0..staged.len()).collect();
        rng.shuffle(&mut order);
        for index in order {
            std::thread::sleep(pause);
            let sample = launcher.launch(&staged[index], Some(round), timing[index])?;
            if let Some(error) = &sample.error {
                eprintln!("\r{} round {round} failed: {error}", sample.build);
            }
            run_dir::append_sample(&dir, &sample)?;
            samples.push(sample);
        }
        if progress {
            eprint!("\rround {}/{}", round + 1, args.runs);
        }
    }
    if progress {
        eprintln!();
    }
    manifest.cpu_busy_percent = host::busy_percent(cpu_before, host::cpu_times());
    run_dir::write_json(&dir.join(run_dir::MANIFEST), &manifest)?;

    let summary = analysis::run_table(&dir, &manifest, &samples, 1);
    run_dir::write_json(&dir.join(run_dir::SUMMARY), &summary)?;
    let table = match args.depth {
        1 => summary,
        depth => analysis::run_table(&dir, &manifest, &samples, depth),
    };
    report::print_launch(&manifest, &table, &dir);
    Ok(())
}

/// Each build with its tag: from `--tag` in the same order, or else from the
/// build's path. Without `--build`, the build is the folder holding
/// axis-bench.
fn resolve_builds(paths: &[PathBuf], tags: &[String]) -> Result<Vec<(String, PathBuf)>> {
    let paths = match paths {
        [] => vec![
            std::env::current_exe()?
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        ],
        paths => paths.to_vec(),
    };
    if !tags.is_empty() && tags.len() != paths.len() {
        return Err("give one --tag per --build, in the same order".into());
    }
    // Tags name directories, which differ only in case on Windows and macOS.
    let taken = |builds: &[(String, PathBuf)], tag: &str| {
        builds
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(tag))
    };
    let mut builds: Vec<(String, PathBuf)> = Vec::new();
    for (index, path) in paths.into_iter().enumerate() {
        let tag = match tags.get(index) {
            Some(tag) => {
                let valid = !tag.is_empty()
                    && !tag.starts_with('.')
                    && tag
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
                if !valid {
                    return Err(format!(
                        "--tag {tag}: a tag uses letters, digits, '-', '_' and '.', and does \
                         not start with '.'"
                    )
                    .into());
                }
                if taken(&builds, tag) {
                    return Err(format!("--tag {tag}: another build has that tag").into());
                }
                tag.clone()
            }
            None => {
                let base = default_tag(&path);
                let mut tag = base.clone();
                for n in 2.. {
                    if !taken(&builds, &tag) {
                        break;
                    }
                    tag = format!("{base}-{n}");
                }
                tag
            }
        };
        builds.push((tag, path));
    }
    Ok(builds)
}

/// The name of the checkout holding the build's `target` folder, or else of
/// the build folder itself, in the characters a tag allows.
fn default_tag(path: &Path) -> String {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = match path.parent() {
        Some(parent) if path.is_file() => parent,
        _ => &path,
    };
    let names: Vec<String> = dir
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let name = match names.iter().rposition(|name| name == "target") {
        Some(index) if index > 0 => &names[index - 1],
        _ => names.last().map_or("", String::as_str),
    };
    let tag: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    match tag.trim_start_matches('.') {
        "" => "build".into(),
        tag => tag.into(),
    }
}

fn find_child(path: Option<&Path>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe()?
            .with_file_name(format!("axis-bench-child{}", std::env::consts::EXE_SUFFIX)),
    };
    if !path.is_file() {
        return Err(format!(
            "no-op program {} not found; build it with `cargo build --release -p axis-bench` \
             or pass --child",
            path.display()
        )
        .into());
    }
    Ok(std::path::absolute(path)?)
}

struct Launcher<'a> {
    stage: &'a Stage,
    run_dir: &'a Path,
    policy: &'a str,
    child: Option<&'a Path>,
    command: &'a [String],
    daemon_probe: bool,
    timeout: Duration,
    watcher: Option<&'a Watcher>,
}

impl Launcher<'_> {
    /// Runs `axis run` once. `round` is `None` for warm-ups, which keep no
    /// trace. Problems with the launch itself are recorded in the sample.
    fn launch(&self, build: &StagedBuild, round: Option<u32>, timing: bool) -> Result<Sample> {
        let workspace = self.stage.workspace()?;
        let mut command = Command::new(&build.axis);
        if !self.daemon_probe {
            command.arg("--socket").arg(self.stage.no_daemon_socket());
        }
        command.args(["run", "--policy", self.policy, "--"]);
        match self.child {
            Some(child) => {
                let copy = workspace.join(child.file_name().unwrap_or_default());
                std::fs::copy(child, &copy)?;
                command.arg(copy);
            }
            None => {
                command.args(self.command);
            }
        }
        let raw_trace = timing.then(|| match round {
            Some(round) => self
                .run_dir
                .join("traces")
                .join(&build.tag)
                .join(format!("{round:03}.axis.json")),
            None => self.stage.path().join("warm-up.axis.json"),
        });
        command.env_remove(TIMING_FILE_ENV);
        if let Some(path) = &raw_trace {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            command.env(TIMING_FILE_ENV, path);
        }
        command
            .current_dir(&workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(watcher) = self.watcher {
            watcher.clear();
        }
        let spawn_unix_us = host::unix_micros();
        let started = Instant::now();
        let mut process = command
            .spawn()
            .map_err(|err| format!("cannot start {}: {err}", build.axis.display()))?;
        let stdout = read_in_background(process.stdout.take());
        let stderr = read_in_background(process.stderr.take());
        let (stop_watchdog, watchdog) = watchdog(process.id(), self.timeout);
        let status = process.wait()?;
        let total_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let exit_unix_us = host::unix_micros();
        let _ = stop_watchdog.send(());
        let timed_out = watchdog.join().unwrap_or(false);
        let (stdout, stderr) = (collect(stdout), collect(stderr));
        let processes = self.watcher.map_or_else(Vec::new, |watcher| {
            with_axis(
                watcher.take(process.id()),
                process.id(),
                spawn_unix_us,
                exit_unix_us,
            )
        });
        let _ = std::fs::remove_dir_all(&workspace);

        let child_line = parse_child_line(&stdout);
        let error = if timed_out {
            Some(format!("killed after {} s", self.timeout.as_secs()))
        } else if !status.success() {
            Some(format!("axis exited with {status}: {}", tail(&stderr)))
        } else if self.child.is_some() && child_line.is_none() {
            Some(format!(
                "the no-op program printed nothing: {}",
                tail(&stderr)
            ))
        } else {
            None
        };
        let mut sample = Sample {
            build: build.tag.clone(),
            round: round.unwrap_or(0),
            exit_code: status.code(),
            error,
            spawn_unix_us,
            exit_unix_us,
            total_us,
            child_main_us: child_line.map(|line| line.0),
            child_exit_us: child_line.map(|line| line.1),
            trace: None,
            processes,
        };
        if let Some(raw) = &raw_trace {
            let child_pid = child_line.and_then(|line| line.2);
            sample.trace = round.and_then(|round| self.merge_trace(raw, round, &sample, child_pid));
            let _ = std::fs::remove_file(raw);
        }
        Ok(sample)
    }

    /// Writes `traces/<tag>/<round>.json`: axis's events plus the launch as
    /// axis-bench saw it and the command as it reported itself.
    fn merge_trace(
        &self,
        raw: &Path,
        round: u32,
        sample: &Sample,
        child_pid: Option<u32>,
    ) -> Option<String> {
        let process = |pid: u32, name: &str| json!({"ph": "M", "name": "process_name", "pid": pid, "tid": 0, "args": {"name": name}});
        let thread = |pid: u32, tid: u32, name: &str| json!({"ph": "M", "name": "thread_name", "pid": pid, "tid": tid, "args": {"name": name}});
        let span = |pid: u32, tid: u32, name: &str, ts: u64, dur: u64| {
            json!({"ph": "X", "name": name, "cat": "axis-bench", "pid": pid, "tid": tid, "ts": ts,
                   "dur": dur})
        };
        let mut trace: Value = serde_json::from_slice(&std::fs::read(raw).ok()?).ok()?;
        let events = trace["traceEvents"].as_array_mut()?;
        let tool = std::process::id();
        let launch_us = sample.exit_unix_us.saturating_sub(sample.spawn_unix_us);
        events.push(process(tool, "axis-bench"));
        events.push(span(tool, 1, "launch", sample.spawn_unix_us, launch_us));
        if let Some((main, exit)) = sample.child_marks() {
            let pid = child_pid.unwrap_or(0);
            events.push(process(pid, "command"));
            events.push(span(pid, 1, "command", main, exit - main));
        }
        if !sample.processes.is_empty() {
            events.push(process(PROCESSES_TRACK, "processes"));
        }
        for started in &sample.processes {
            let (pid, tid) = (PROCESSES_TRACK, started.pid);
            let end = started.exit_us.unwrap_or(sample.exit_unix_us);
            let dur = end.saturating_sub(started.start_us);
            events.push(thread(pid, tid, &format!("{} {tid}", started.name)));
            events.push(span(pid, tid, &started.name, started.start_us, dur));
        }
        let axis = trace["otherData"].take();
        trace["otherData"] = json!({
            "axis": axis, "build": sample.build, "round": round, "exit_code": sample.exit_code
        });
        let relative = format!("traces/{}/{round:03}.json", sample.build);
        std::fs::write(
            self.run_dir.join(&relative),
            serde_json::to_vec(&trace).ok()?,
        )
        .ok()?;
        Some(relative)
    }
}

/// The watcher's processes, with `axis` itself timed as axis-bench saw it.
fn with_axis(mut processes: Vec<Process>, pid: u32, start_us: u64, exit_us: u64) -> Vec<Process> {
    match processes.iter_mut().find(|process| process.pid == pid) {
        Some(axis) => {
            axis.start_us = start_us;
            axis.exit_us = Some(exit_us);
        }
        None => processes.insert(
            0,
            Process {
                name: "axis".into(),
                pid,
                start_us,
                exit_us: Some(exit_us),
            },
        ),
    }
    processes
}

fn read_in_background<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sender.send(bytes);
        });
    }
    receiver
}

/// Output of a finished launch. A grandchild still holding the pipe open
/// would block forever, so give up after a few seconds.
fn collect(receiver: mpsc::Receiver<Vec<u8>>) -> String {
    let bytes = receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Kills `pid` and its children unless told to stop within `timeout`.
fn watchdog(pid: u32, timeout: Duration) -> (mpsc::Sender<()>, std::thread::JoinHandle<bool>) {
    let (sender, receiver) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || match receiver.recv_timeout(timeout) {
        Err(mpsc::RecvTimeoutError::Timeout) => {
            host::kill_tree(pid);
            true
        }
        _ => false,
    });
    (sender, handle)
}

/// `(main, exit, pid)` from the no-op program's line.
fn parse_child_line(stdout: &str) -> Option<(u64, u64, Option<u32>)> {
    let line = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(CHILD_LINE))?;
    let mut fields = line.split_whitespace();
    let main = fields.next()?.parse().ok()?;
    let exit = fields.next()?.parse().ok()?;
    let pid = fields.next().and_then(|pid| pid.parse().ok());
    Some((main, exit, pid))
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    match tail.char_indices().nth(400) {
        Some((cut, _)) => format!("{}...", &tail[..cut]),
        None if tail.is_empty() => "no output on stderr".into(),
        None => tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_come_in_build_order_or_from_the_checkout() {
        let resolve = |paths: &[&str], tags: &[&str]| {
            resolve_builds(
                &paths.iter().map(PathBuf::from).collect::<Vec<_>>(),
                &tags.iter().map(|tag| tag.to_string()).collect::<Vec<_>>(),
            )
        };
        let builds = resolve(&["/a", "/b/axis"], &["main", "mine"]).expect("valid");
        assert_eq!(builds[1], ("mine".into(), PathBuf::from("/b/axis")));
        let same = "/src/axis-main/target/release";
        let builds = resolve(&[same, same], &[]).expect("valid");
        assert_eq!(builds[0].0, "axis-main");
        assert_eq!(builds[1].0, "axis-main-2");
        let builds = resolve(&[], &["mine"]).expect("the folder holding axis-bench");
        assert_eq!(builds.len(), 1);
        assert!(resolve(&["/a", "/b"], &["main"]).is_err());
        assert!(resolve(&["/a"], &[""]).is_err());
        assert!(resolve(&["/a"], &["../x"]).is_err());
        assert!(resolve(&["/a"], &["a/b"]).is_err());
        assert!(resolve(&["/a", "/b"], &["main", "Main"]).is_err());
    }

    #[test]
    fn default_tag_names_the_checkout() {
        let tag = |path: &str| default_tag(Path::new(path));
        assert_eq!(tag("/src/axis-main/target/release"), "axis-main");
        assert_eq!(
            tag("/src/axis-main/target/x86_64-pc-windows-msvc/release"),
            "axis-main"
        );
        assert_eq!(tag("/home/me/.axis-base/target/release"), "axis-base");
        assert_eq!(tag("/opt/axis builds/v1"), "v1");
        assert_eq!(tag("/opt/axis builds"), "axis-builds");
        assert_eq!(tag("/"), "build");
    }

    #[test]
    fn child_line_is_found_among_log_lines() {
        let stdout = "INFO starting\nAXIS_BENCH_CHILD 10 20 7\nINFO done\n";
        assert_eq!(parse_child_line(stdout), Some((10, 20, Some(7))));
        assert_eq!(parse_child_line("AXIS_BENCH_CHILD 10"), None);
        assert_eq!(parse_child_line(""), None);
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\n\nb\nc\nd\n"), "b | c | d");
        assert_eq!(tail(""), "no output on stderr");
    }
}
