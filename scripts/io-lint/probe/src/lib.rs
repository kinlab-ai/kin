// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Shapes a raw filesystem or process call can take. Each marked line states
//! what the boundary lint must report there.

use std::fs::read_to_string as slurp;
use std::path::{Path, PathBuf};

/// Graph-owned state whose accessor shares a name with a filesystem probe.
pub struct Lease;

impl Lease {
    pub fn metadata(&self) -> u32 {
        7
    }
}

macro_rules! read_through_a_macro {
    ($path:expr) => {
        std::fs::read($path)
    };
}

pub fn direct(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default() // io-lint: reported std::fs::read_to_string
}

pub fn aliased(path: &Path) -> String {
    slurp(path).unwrap_or_default() // io-lint: reported std::fs::read_to_string
}

pub fn through_a_path_buf(path: &PathBuf) -> bool {
    path.exists() // io-lint: reported std::path::Path::exists
}

pub fn glob_import(path: &Path) -> usize {
    use std::fs::*;
    read(path).map(|bytes| bytes.len()).unwrap_or(0) // io-lint: reported std::fs::read
}

pub fn module_path(path: &Path) -> u64 {
    use std::fs;
    fs::metadata(path).map(|m| m.len()).unwrap_or(0) // io-lint: reported std::fs::metadata
}

pub fn builder(path: &Path) -> bool {
    std::fs::OpenOptions::new().read(true).open(path).is_ok() // io-lint: reported std::fs::OpenOptions::open
}

pub fn function_pointer(paths: &[PathBuf]) -> usize {
    paths.iter().map(std::fs::read_to_string).count() // io-lint: reported std::fs::read_to_string
}

pub fn fully_qualified(path: &Path) -> bool {
    <Path>::is_file(path) // io-lint: reported std::path::Path::is_file
}

pub fn in_a_closure(paths: &[PathBuf]) -> usize {
    paths.iter().filter(|p| p.is_dir()).count() // io-lint: reported std::path::Path::is_dir
}

pub fn through_a_local_macro(path: &Path) -> usize {
    read_through_a_macro!(path).map(|b| b.len()).unwrap_or(0) // io-lint: reported std::fs::read
}

pub fn opened_handle(path: &Path) -> bool {
    let file = std::fs::File::open(path); // io-lint: reported std::fs::File::open
    file.and_then(|f| f.metadata()).is_ok() // io-lint: reported std::fs::File::metadata
}

pub fn traversal(path: &Path) -> usize {
    std::fs::read_dir(path).map(|d| d.count()).unwrap_or(0) // io-lint: reported std::fs::read_dir
}

pub fn a_search_tool() -> bool {
    let mut search = std::process::Command::new("rg"); // io-lint: reported std::process::Command::new
    search.arg("needle").status().is_ok()
}

pub type Reader = fn(&'static Path) -> std::io::Result<Vec<u8>>;

pub const READER: Reader = std::fs::read::<&'static Path>; // io-lint: reported std::fs::read

pub fn graph_lease(lease: &Lease) -> u32 {
    lease.metadata() // io-lint: clean
}

pub fn own_process_id() -> u32 {
    std::process::id() // io-lint: clean
}

pub fn a_command_in_a_signature(command: &mut std::process::Command) -> &mut std::process::Command {
    command.arg("--version") // io-lint: clean
}

pub fn a_type_already_read(kind: std::fs::FileType) -> bool {
    kind.is_dir() || kind.is_file() // io-lint: clean
}
