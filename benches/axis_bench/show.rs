// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `axis-bench show`: prints a saved run's table again, at any depth.

use crate::{Result, analysis, report, run_dir};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct ShowArgs {
    /// Saved run directory.
    run: PathBuf,
    /// How many dot-separated parts of a step name make a row.
    #[arg(long, default_value_t = 1)]
    depth: usize,
}

pub fn run(args: ShowArgs) -> Result<()> {
    let (manifest, samples) = run_dir::load(&args.run)?;
    let table = analysis::run_table(&args.run, &manifest, &samples, args.depth);
    report::print_launch(&manifest, &table, &args.run);
    Ok(())
}
