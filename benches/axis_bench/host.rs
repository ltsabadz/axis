// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Host facts and small OS helpers.

use crate::run_dir::Host;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn describe() -> Host {
    Host {
        hostname: hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        cpus: std::thread::available_parallelism().map_or(1, usize::from),
    }
}

fn hostname() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

pub fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `2026-11-19_120102` and `2026-11-19T12:01:02Z` for the current UTC time.
pub fn utc_stamps() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let (hour, minute, second) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    (
        format!("{year:04}-{month:02}-{day:02}_{hour:02}{minute:02}{second:02}"),
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"),
    )
}

/// Gregorian date of a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Busy and total CPU time across all CPUs, in OS ticks.
pub fn cpu_times() -> Option<(u64, u64)> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let fields: Vec<u64> = stat
            .lines()
            .next()?
            .split_whitespace()
            .skip(1)
            .take(8)
            .map(|field| field.parse().ok())
            .collect::<Option<_>>()?;
        let total: u64 = fields.iter().sum();
        let idle = fields.get(3)? + fields.get(4)?;
        Some((total - idle, total))
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::GetSystemTimes;
        let (mut idle, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: the three pointers are valid for the duration of the call.
        unsafe {
            GetSystemTimes(
                Some(&raw mut idle),
                Some(&raw mut kernel),
                Some(&raw mut user),
            )
        }
        .ok()?;
        let ticks =
            |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        // Kernel time includes idle time.
        let total = ticks(kernel) + ticks(user);
        Some((total - ticks(idle), total))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

pub fn busy_percent(before: Option<(u64, u64)>, after: Option<(u64, u64)>) -> Option<f64> {
    let ((busy0, total0), (busy1, total1)) = (before?, after?);
    let total = total1.checked_sub(total0).filter(|total| *total > 0)?;
    Some(100.0 * busy1.saturating_sub(busy0) as f64 / total as f64)
}

/// Forcefully stops a launch that ran past its timeout.
pub fn kill_tree(pid: u32) {
    let pid = pid.to_string();
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("taskkill");
        command.args(["/F", "/T", "/PID", &pid]);
        command
    } else {
        let mut command = std::process::Command::new("kill");
        command.args(["-KILL", &pid]);
        command
    };
    let _ = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// SplitMix64: a small seeded generator for shuffling the launch order.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_776), (2026, 11, 19));
    }

    #[test]
    fn shuffle_is_seeded_permutation() {
        let mut first = [0, 1, 2, 3, 4, 5];
        let mut second = first;
        Rng::new(7).shuffle(&mut first);
        Rng::new(7).shuffle(&mut second);
        assert_eq!(first, second);
        let mut sorted = first;
        sorted.sort();
        assert_eq!(sorted, [0, 1, 2, 3, 4, 5]);
    }
}
