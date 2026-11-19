// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The files of a saved run: `manifest.json`, `samples.jsonl` (one launch per
//! line), `summary.json`, and `traces/<tag>/<round>.json`.

use crate::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "manifest.json";
pub const SAMPLES: &str = "samples.jsonl";
pub const SUMMARY: &str = "summary.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub tool_version: String,
    pub name: String,
    pub started_utc: String,
    pub policy: String,
    pub runs: u32,
    pub warmup: u32,
    pub pause_ms: u64,
    pub seed: u64,
    pub daemon_probe: bool,
    /// The command each launch ran; empty means `axis-bench-child`.
    pub command: Vec<String>,
    pub host: Host,
    /// Share of all CPUs busy during the measured rounds, when known.
    pub cpu_busy_percent: Option<f64>,
    pub builds: Vec<BuildInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub cpus: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildInfo {
    pub tag: String,
    pub source: String,
    pub timing: bool,
    pub files: Vec<FileInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

/// One measured launch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub build: String,
    pub round: u32,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub spawn_unix_us: u64,
    pub exit_unix_us: u64,
    /// Monotonic time from spawning `axis` to its exit.
    pub total_us: u64,
    pub child_main_us: Option<u64>,
    pub child_exit_us: Option<u64>,
    /// Merged trace, relative to the run directory.
    pub trace: Option<String>,
}

impl Sample {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    /// The child's timestamps, when they fall inside the launch.
    pub fn child_marks(&self) -> Option<(u64, u64)> {
        let (main, exit) = (self.child_main_us?, self.child_exit_us?);
        (self.spawn_unix_us <= main && main <= exit && exit <= self.exit_unix_us)
            .then_some((main, exit))
    }
}

/// The directory for a new run, which must not exist yet.
pub fn new_path(out: &Path, stamp: &str, name: &str) -> Result<PathBuf> {
    let dir = out.join(format!("{stamp}_{name}"));
    if dir.exists() {
        return Err(format!("run directory {} already exists", dir.display()).into());
    }
    Ok(dir)
}

pub fn create(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir.join("traces"))?;
    Ok(())
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text)?;
    Ok(())
}

pub fn append_sample(dir: &Path, sample: &Sample) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(SAMPLES))?;
    writeln!(file, "{}", serde_json::to_string(sample)?)?;
    Ok(())
}

pub fn load(dir: &Path) -> Result<(Manifest, Vec<Sample>)> {
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .map_err(|err| format!("cannot read {}: {err}", dir.join(name).display()))
    };
    let manifest: Manifest = serde_json::from_str(&read(MANIFEST)?)?;
    let samples = read(SAMPLES)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<Sample>, _>>()?;
    Ok((manifest, samples))
}
