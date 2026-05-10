// Verifies HOME is populated for every spawn path the launcher uses, even when the
// launcher itself is started with a sparse env (the Finder/.app case).

use std::process::Command;

fn launcher() -> &'static str {
    env!("CARGO_BIN_EXE_unity-launcher")
}

fn debug_env_with_sparse_parent() -> String {
    let out = Command::new("/usr/bin/env")
        .args(["-i", launcher(), "__debug-env"])
        .output()
        .expect("run launcher");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "launcher failed: stdout={stdout}\nstderr={stderr}"
    );
    stdout
}

fn home_from(out: &str, prefix: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("missing {prefix} line in:\n{out}"))
        .to_string()
}

#[test]
fn launcher_self_has_home_after_ensure_user_env() {
    let out = debug_env_with_sparse_parent();
    let home = home_from(&out, "self HOME=");
    assert!(home.starts_with('/'), "self HOME should be a path, got {home:?}");
}

#[test]
fn shell_child_inherits_home() {
    let out = debug_env_with_sparse_parent();
    let home = home_from(&out, "shell HOME=");
    assert!(home.starts_with('/'), "shell HOME should be a path, got {home:?}");
}

#[test]
fn posix_spawn_child_inherits_home() {
    let out = debug_env_with_sparse_parent();
    let home = home_from(&out, "spawn HOME=");
    assert!(home.starts_with('/'), "spawn HOME should be a path, got {home:?}");
}
