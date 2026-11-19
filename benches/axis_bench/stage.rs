// Copyright 2026 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Copies each build into a private directory and hands out fresh launch
//! workspaces. On Linux, AXIS runs a helper only when every directory above
//! it is owned by root or the user and is neither group- nor world-writable,
//! which rules out `/tmp` and checkouts made with a group-writable umask.

use crate::Result;
use crate::run_dir::FileInfo;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

const EXE: &str = std::env::consts::EXE_SUFFIX;

/// Programs AXIS looks for beside its own executable.
#[cfg(target_os = "linux")]
const HELPERS: &[&str] = &["axis-seccomp-launcher", "axis-netns-helper", "lxc-exec"];
#[cfg(windows)]
const HELPERS: &[&str] = &["wxc-exec.exe", "wxc.exe", "mxc-exec.exe"];
#[cfg(not(any(target_os = "linux", windows)))]
const HELPERS: &[&str] = &[];

pub struct Stage {
    root: tempfile::TempDir,
    next_workspace: Cell<u32>,
}

pub struct StagedBuild {
    pub tag: String,
    pub source: PathBuf,
    pub axis: PathBuf,
    pub files: Vec<FileInfo>,
}

impl Stage {
    /// A new private directory under `base`, removed when dropped.
    pub fn new(base: Option<&Path>) -> Result<Self> {
        let base = std::path::absolute(base.map_or_else(default_base, Path::to_path_buf))?;
        create_private_dir_all(&base)?;
        let root = tempfile::Builder::new()
            .prefix("stage-")
            .tempdir_in(&base)?;
        set_mode(root.path(), 0o700)?;
        check_private(root.path())?;
        Ok(Self {
            root,
            next_workspace: Cell::new(0),
        })
    }

    /// Copies `axis` and the helpers beside it from `path`, a build directory
    /// or the `axis` binary itself.
    pub fn stage_build(&self, tag: &str, path: &Path) -> Result<StagedBuild> {
        let source = if path.is_dir() {
            path.to_path_buf()
        } else if path.is_file() {
            path.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            return Err(format!("--build {}: no such folder or file", path.display()).into());
        };
        let axis_name = format!("axis{EXE}");
        if !source.join(&axis_name).is_file() {
            return Err(format!(
                "no {axis_name} in {}; pass --build with the folder holding the axis to time",
                source.display()
            )
            .into());
        }
        let dir = self.root.path().join(tag);
        std::fs::create_dir(&dir)?;
        set_mode(&dir, 0o755)?;
        let mut files = Vec::new();
        for name in std::iter::once(axis_name.as_str()).chain(HELPERS.iter().copied()) {
            let from = source.join(name);
            if !from.is_file() {
                continue;
            }
            let to = dir.join(name);
            std::fs::copy(&from, &to)?;
            set_mode(&to, 0o755)?;
            files.push(FileInfo {
                name: name.into(),
                bytes: std::fs::metadata(&to)?.len(),
                sha256: sha256(&to)?,
            });
        }
        Ok(StagedBuild {
            tag: tag.into(),
            source: std::path::absolute(&source).unwrap_or(source),
            axis: dir.join(axis_name),
            files,
        })
    }

    /// A fresh, empty directory for one launch.
    pub fn workspace(&self) -> Result<PathBuf> {
        let n = self.next_workspace.get();
        self.next_workspace.set(n + 1);
        let dir = self.root.path().join(format!("ws-{n}"));
        std::fs::create_dir(&dir)?;
        Ok(dir)
    }

    /// A `--socket` value that makes `axis run` give up on the daemon at once.
    pub fn no_daemon_socket(&self) -> OsString {
        if cfg!(windows) {
            "0.0.0.0:9".into()
        } else {
            self.root.path().join("no-axisd.sock").into_os_string()
        }
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }
}

fn default_base() -> PathBuf {
    let preferred = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    };
    preferred
        .unwrap_or_else(std::env::temp_dir)
        .join("axis-bench")
}

fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn check_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let path = std::fs::canonicalize(path)?;
    let user = std::fs::metadata(&path)?.uid();
    for dir in path.ancestors() {
        let metadata = std::fs::metadata(dir)?;
        if (metadata.uid() != 0 && metadata.uid() != user) || metadata.mode() & 0o022 != 0 {
            return Err(format!(
                "{} is writable by others or owned by another user, so AXIS would refuse \
                 helpers copied below it; pass --stage-dir with a private directory",
                dir.display()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path) -> Result<()> {
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
