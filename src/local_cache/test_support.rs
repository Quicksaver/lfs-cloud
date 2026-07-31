pub(super) use std::{
    fs,
    path::Path,
    process::Command,
    str::FromStr,
    sync::{Arc, Barrier, mpsc},
    thread,
    time::Duration,
};

pub(super) use crate::LfsObjectSize;

use super::*;

pub(super) fn oid(value: &str) -> LfsOid {
    LfsOid::from_str(value).expect("test OID should be valid")
}

pub(super) fn object_for_bytes(bytes: &[u8]) -> LfsObject {
    let digest = sha256_hex(bytes);

    LfsObject::new(oid(&digest), LfsObjectSize::new(bytes.len() as u64))
}

pub(super) fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("test parent directory should be created");
    }
    fs::write(path, bytes).expect("test file should be written");
}

#[cfg(target_os = "macos")]
pub(super) fn is_apfs(path: &Path) -> bool {
    let file_system = rustix::fs::statfs(path)
        .expect("test filesystem should be inspectable")
        .f_fstypename;
    let file_system = file_system
        .iter()
        .copied()
        .take_while(|byte| *byte != 0)
        .map(|byte| byte as u8)
        .collect::<Vec<_>>();

    file_system == b"apfs"
}

pub(super) fn initialize_git_worktree(path: &Path) {
    fs::create_dir_all(path).expect("test worktree should be created");
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .output()
        .expect("git init should start");
    assert!(
        output.status.success(),
        "git init should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) fn git_add(path: &Path, relative_paths: &[&Path]) {
    let output = Command::new("git")
        .arg("add")
        .arg("--")
        .args(relative_paths)
        .current_dir(path)
        .output()
        .expect("git add should start");
    assert!(
        output.status.success(),
        "git add should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
