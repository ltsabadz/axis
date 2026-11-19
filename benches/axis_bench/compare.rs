// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `axis-bench compare`: one build from each of two saved runs, side by side.

use crate::run_dir::{self, Manifest};
use crate::{Result, analysis, report};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct CompareArgs {
    /// Saved run directory with the baseline.
    before: PathBuf,
    /// Saved run directory to compare with it.
    after: PathBuf,
    /// Tag of the build to use from the first run (default: its first build).
    #[arg(long, value_name = "TAG")]
    before_tag: Option<String>,
    /// Tag of the build to use from the second run (default: its first build).
    #[arg(long, value_name = "TAG")]
    after_tag: Option<String>,
    /// How many dot-separated parts of a step name make a row.
    #[arg(long, default_value_t = 1)]
    depth: usize,
}

pub fn run(args: CompareArgs) -> Result<()> {
    let (before, before_samples) = run_dir::load(&args.before)?;
    let (after, after_samples) = run_dir::load(&args.after)?;
    let before_build = pick(&before, args.before_tag.as_deref())?;
    let after_build = pick(&after, args.after_tag.as_deref())?;
    let launches = |dir: &PathBuf, samples: &[run_dir::Sample], build: &str| {
        samples
            .iter()
            .filter(|sample| sample.build == build)
            .map(|sample| analysis::load_launch(dir, sample, args.depth))
            .collect::<Vec<_>>()
    };
    let table = analysis::table(
        &[
            (
                "before".into(),
                launches(&args.before, &before_samples, &before_build),
            ),
            (
                "after".into(),
                launches(&args.after, &after_samples, &after_build),
            ),
        ],
        false,
    );
    report::print_compare(
        [
            (&args.before, &before, &before_build),
            (&args.after, &after, &after_build),
        ],
        &table,
    );
    Ok(())
}

fn pick(manifest: &Manifest, tag: Option<&str>) -> Result<String> {
    let found = match tag {
        Some(tag) => manifest.builds.iter().find(|build| build.tag == tag),
        None => manifest.builds.first(),
    };
    let tags: Vec<&str> = manifest
        .builds
        .iter()
        .map(|build| build.tag.as_str())
        .collect();
    found
        .map(|build| build.tag.clone())
        .ok_or_else(|| format!("run {} has builds {}", manifest.name, tags.join(", ")).into())
}
