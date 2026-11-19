// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Records every process started under axis-bench, with when it started and
//! exited. On Windows, axis-bench joins a job object whose completion port
//! reports each process its children start, including the executor's helpers
//! and the command inside their own nested jobs. Other platforms record none.

use crate::run_dir::Process;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Watcher {
    processes: Arc<Mutex<Vec<Process>>>,
}

impl Watcher {
    /// Starts recording, or returns `None` where processes cannot be watched.
    pub fn start() -> Option<Self> {
        let watcher = Self::default();
        #[cfg(windows)]
        if let Err(err) = job::watch(watcher.processes.clone()) {
            eprintln!("axis-bench: warning: cannot watch processes, so waits are not split: {err}");
            return None;
        }
        cfg!(windows).then_some(watcher)
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    /// The processes recorded since [`Watcher::clear`], once `pid` has been
    /// seen to exit or a moment has passed.
    pub fn take(&self, pid: u32) -> Vec<Process> {
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            let mut processes = self.lock();
            let exited = processes
                .iter()
                .any(|process| process.pid == pid && process.exit_us.is_some());
            if exited || Instant::now() >= deadline {
                return std::mem::take(&mut *processes);
            }
            drop(processes);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Process>> {
        self.processes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(windows)]
mod job {
    use super::*;
    use crate::host::unix_micros;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
        JobObjectAssociateCompletionPortInformation, SetInformationJobObject,
    };
    use windows::Win32::System::SystemServices::{
        JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS, JOB_OBJECT_MSG_EXIT_PROCESS,
        JOB_OBJECT_MSG_NEW_PROCESS,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, INFINITE, OpenProcess, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    /// The job has no limits, and it and its port stay open until
    /// axis-bench exits.
    pub fn watch(processes: Arc<Mutex<Vec<Process>>>) -> windows::core::Result<()> {
        let port = unsafe {
            let job = CreateJobObjectW(None, None)?;
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1)?;
            let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: std::ptr::null_mut(),
                CompletionPort: port,
            };
            SetInformationJobObject(
                job,
                JobObjectAssociateCompletionPortInformation,
                (&raw const association).cast(),
                size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )?;
            AssignProcessToJobObject(job, GetCurrentProcess())?;
            port.0 as usize
        };
        std::thread::spawn(move || read(HANDLE(port as *mut _), &processes));
        Ok(())
    }

    fn read(port: HANDLE, processes: &Mutex<Vec<Process>>) {
        loop {
            let (mut message, mut key) = (0u32, 0usize);
            let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
            let received = unsafe {
                GetQueuedCompletionStatus(port, &mut message, &mut key, &mut overlapped, INFINITE)
            };
            if received.is_err() {
                return;
            }
            let at = unix_micros();
            // Process messages carry the process id in place of the OVERLAPPED.
            let pid = overlapped as usize as u32;
            match message {
                JOB_OBJECT_MSG_NEW_PROCESS => {
                    let name = image_name(pid);
                    lock(processes).push(Process {
                        name,
                        pid,
                        start_us: at,
                        exit_us: None,
                    });
                }
                JOB_OBJECT_MSG_EXIT_PROCESS | JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS => {
                    let mut processes = lock(processes);
                    if let Some(process) = processes
                        .iter_mut()
                        .rev()
                        .find(|process| process.pid == pid && process.exit_us.is_none())
                    {
                        process.exit_us = Some(at);
                    }
                }
                _ => {}
            }
        }
    }

    fn lock(processes: &Mutex<Vec<Process>>) -> MutexGuard<'_, Vec<Process>> {
        processes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn image_name(pid: u32) -> String {
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let found = unsafe {
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).and_then(|process| {
                let result = QueryFullProcessImageNameW(
                    process,
                    PROCESS_NAME_WIN32,
                    PWSTR(buffer.as_mut_ptr()),
                    &mut size,
                );
                let _ = CloseHandle(process);
                result
            })
        };
        match found {
            Ok(()) => super::program_name(&String::from_utf16_lossy(&buffer[..size as usize])),
            Err(_) => "unknown".into(),
        }
    }
}

/// `wxc-exec` for `C:\tools\wxc-exec.exe`.
#[cfg_attr(not(windows), allow(dead_code))]
fn program_name(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let stem = match file.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => file,
    };
    stem.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_names_drop_folder_and_extension() {
        assert_eq!(program_name(r"C:\tools\wxc-exec.exe"), "wxc-exec");
        assert_eq!(program_name("/usr/bin/bwrap"), "bwrap");
        assert_eq!(program_name(r"C:\x\my.tool.exe"), "my.tool");
        assert_eq!(program_name(".hidden"), ".hidden");
    }

    #[test]
    fn take_returns_what_was_recorded_and_clears_it() {
        let watcher = Watcher::default();
        watcher.lock().push(Process {
            name: "axis".into(),
            pid: 7,
            start_us: 1,
            exit_us: Some(2),
        });
        assert_eq!(watcher.take(7).len(), 1);
        assert!(watcher.take(7).is_empty());
    }
}
