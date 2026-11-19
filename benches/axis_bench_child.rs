// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! No-op command for `axis-bench launch`. Prints when its `main` started and
//! when it was about to exit, in microseconds since the Unix epoch, so the
//! benchmark can tell sandbox start-up and teardown apart from the command.

use std::time::{SystemTime, UNIX_EPOCH};

fn unix_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros())
        .unwrap_or(0)
}

fn main() {
    let main_us = unix_micros();
    let exit_us = unix_micros();
    println!(
        "AXIS_BENCH_CHILD {main_us} {exit_us} {}",
        std::process::id()
    );
}
