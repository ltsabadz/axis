// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `axis-bench`: times real `axis run` launches end to end and shows where
//! the time goes. See docs/launch-timing.md.

mod analysis;
mod compare;
mod host;
mod launch;
mod report;
mod run_dir;
mod show;
mod stage;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "axis-bench", about = "Times real `axis run` launches")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Launch one or more builds alternately and report where launch time goes.
    Launch(launch::LaunchArgs),
    /// Print a saved run's table again, at any depth.
    Show(show::ShowArgs),
    /// Compare one build from each of two saved runs.
    Compare(compare::CompareArgs),
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Launch(args) => launch::run(args),
        Command::Show(args) => show::run(args),
        Command::Compare(args) => compare::run(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("axis-bench: {err}");
            ExitCode::FAILURE
        }
    }
}
