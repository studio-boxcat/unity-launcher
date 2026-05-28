mod util;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use util::{AppError, SpawnMode};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const PID_FILE: &str = "Temp/.unity-launcher.pid";
// Per-project log retention. Each launch writes a fresh timestamped file; without a
// cap they accumulate forever (10MB+ each is common). Auth runs are rare, so smaller.
const KEEP_LAUNCH_LOGS: usize = 30;
const KEEP_AUTH_LOGS: usize = 10;

// Headless by default; opt in to the macOS alert via UNITY_LAUNCHER_GUI=1.
fn show_error(e: &AppError) {
    eprintln!("Error: {}", e.message);
    if let Some(d) = &e.detail {
        eprintln!("  {d}");
    }
    if env::var_os("UNITY_LAUNCHER_GUI").is_some() {
        util::show_alert(&e.message, e.detail.as_deref().unwrap_or(""));
    }
}

// Drop-in (`<project>/foo.app/Contents/MacOS/unity-launcher`) resolves via the exe
// path; `unity-launcher` from $PATH falls back to cwd.
fn project_path() -> Result<PathBuf, AppError> {
    [env::current_exe().ok(), env::current_dir().ok()]
        .into_iter()
        .flatten()
        .find_map(|start| {
            start
                .ancestors()
                .find(|p| p.join("ProjectSettings/ProjectVersion.txt").exists())
                .map(Path::to_path_buf)
        })
        .ok_or(AppError {
            message: "Not inside a Unity project".into(),
            detail: Some("ProjectSettings/ProjectVersion.txt not found in any ancestor of the executable or cwd".into()),
        })
}

fn unity_version(project: &Path) -> Result<String, AppError> {
    let path = project.join("ProjectSettings/ProjectVersion.txt");
    let contents = std::fs::read_to_string(&path).map_err(|_| AppError {
        message: "Could not read Unity version".into(),
        detail: Some("ProjectSettings/ProjectVersion.txt".into()),
    })?;
    contents
        .lines()
        .find_map(|l| l.strip_prefix("m_EditorVersion:"))
        .map(|v| v.trim().to_string())
        .ok_or(AppError {
            message: "Could not parse Unity version".into(),
            detail: Some("m_EditorVersion: line missing".into()),
        })
}

fn unity_path(ver: &str) -> PathBuf {
    PathBuf::from(format!(
        "/Applications/Unity/Hub/Editor/{ver}/Unity.app/Contents/MacOS/Unity"
    ))
}

fn resolve_unity_pid(project: &Path) -> Option<libc::pid_t> {
    if let Ok(s) = std::fs::read_to_string(project.join(PID_FILE)) {
        if let Ok(pid) = s.trim().parse::<libc::pid_t>() {
            if util::is_unity_alive(pid) {
                return Some(pid);
            }
        }
    }
    let needle = format!("Unity -projectPath {}", project.display());
    util::find_process(&needle)
}

fn write_pid(project: &Path, pid: libc::pid_t) {
    let path = project.join(PID_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, pid.to_string());
}

fn clear_pid(project: &Path) {
    let _ = std::fs::remove_file(project.join(PID_FILE));
}

// Closure-based matcher because "unity-" prefix overlaps "unity-auth-" — naive
// starts_with would conflate the two log streams.
fn prune_logs<F>(logs_dir: &Path, matches: F, keep: usize)
where
    F: Fn(&str) -> bool,
{
    let Ok(entries) = std::fs::read_dir(logs_dir) else { return };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| matches(&e.file_name().to_string_lossy()))
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    for f in &files[..files.len() - keep] {
        let _ = std::fs::remove_file(f.path());
    }
}

fn is_launch_log(name: &str) -> bool {
    name.starts_with("unity-") && !name.starts_with("unity-auth-") && name.ends_with(".log")
}

fn is_auth_log(name: &str) -> bool {
    name.starts_with("unity-auth-") && name.ends_with(".log")
}

// Machine-level hook at $XDG_CONFIG_HOME/unity-launcher/auth.sh (default ~/.config/...).
// Symlink the version-controlled config/auth.sh into place via `just install-config`.
fn auth_script_path() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".config")
        });
    base.join("unity-launcher/auth.sh")
}

fn run_auth(project: &Path, unity: &Path) -> Result<bool, AppError> {
    let script = auth_script_path();
    if !script.exists() {
        return Ok(false);
    }

    println!("Running auth hook: {}", script.display());
    let logs_dir = project.join("Logs");
    prune_logs(&logs_dir, is_auth_log, KEEP_AUTH_LOGS);
    let log = logs_dir.join(format!("unity-auth-{}.log", util::timestamp()));
    // Single-quote paths; macOS Unity Hub paths contain no single quotes.
    let r = util::shell(
        &format!("UNITY='{}' '{}'", unity.display(), script.display()),
        Some(&log),
    );
    if r.exit != 0 {
        let errors: Vec<String> = r
            .output
            .lines()
            .filter_map(|l| l.strip_prefix("ERROR:"))
            .map(|m| m.trim().to_string())
            .collect();
        let message = errors.first().cloned().unwrap_or_else(|| "Auth hook failed".into());
        let detail = errors.get(1).cloned();
        return Err(AppError { message, detail });
    }
    Ok(true)
}

fn prepare_log(project: &Path) -> PathBuf {
    let logs_dir = project.join("Logs");
    let _ = std::fs::create_dir_all(&logs_dir);
    prune_logs(&logs_dir, is_launch_log, KEEP_LAUNCH_LOGS);
    let log = logs_dir.join(format!("unity-{}.log", util::timestamp()));
    println!("Log: {}", log.display());
    log
}

fn start_unity_detached(unity: &Path, project: &Path) -> Result<(libc::pid_t, PathBuf), AppError> {
    let log = prepare_log(project);
    let pid = util::spawn_unity(unity, project, &log, SpawnMode::Detached)?;
    write_pid(project, pid);
    Ok((pid, log))
}

fn launch_batchmode(unity: &Path, project: &Path) -> Result<(), AppError> {
    println!("[BATCHMODE] Pairing -nographics with -batchmode");
    let log = prepare_log(project);
    let pid = util::spawn_unity(unity, project, &log, SpawnMode::BatchSync)?;
    let exit = util::waitpid_exit(pid);
    if exit != 0 {
        return Err(AppError {
            message: format!("Unity batchmode exited {exit}"),
            detail: Some(format!("See {}", log.display())),
        });
    }
    Ok(())
}

// Returns Ok(true) on success, Ok(false) on probable license failure (Unity exited
// before the success marker), Err on spawn failure.
fn launch(unity: &Path, project: &Path) -> Result<bool, AppError> {
    let (pid, log) = start_unity_detached(unity, project)?;
    println!("Launching Unity (pid {pid})...");

    // Marker choice: Unity 6000.2.x omits "Licensing is initialized" on warm starts,
    // but emits this success line on both warm and cold paths.
    let iters = (STARTUP_TIMEOUT.as_millis() / POLL_INTERVAL.as_millis()) as usize;
    for _ in 0..iters {
        sleep(POLL_INTERVAL);
        let Ok(c) = std::fs::read_to_string(&log) else { continue };
        if c.contains("Successfully updated license") {
            println!("Unity started.");
            return Ok(true);
        }
        if !util::is_unity_alive(pid) {
            println!("Unity exited during startup.");
            clear_pid(project);
            return Ok(false);
        }
    }
    println!("Timeout.");
    Ok(true)
}

fn cmd_launch(project: &Path, batchmode: bool) -> Result<(), AppError> {
    println!("Project: {}", project.display());

    // Fast path before version/path lookups — those aren't needed if Unity is up.
    if let Some(pid) = resolve_unity_pid(project) {
        if batchmode {
            return Err(AppError {
                message: "Unity already running".into(),
                detail: Some(format!(
                    "Cannot launch batchmode while another instance holds the project lock (pid {pid})"
                )),
            });
        }
        write_pid(project, pid); // refresh in case it came from a fallback scan
        if env::var_os("UL_NO_FOCUS").is_none() {
            util::focus_process(pid);
        }
        println!("Already running.");
        return Ok(());
    }

    let ver = unity_version(project)?;
    println!("Unity: {ver}");
    let unity = unity_path(&ver);

    // Hot Reload (Unity asset) leaves CodePatcherCLI children running after a crashed
    // editor; they hold ports/locks that block a clean re-launch.
    for pid in util::find_processes("CodePatcherCLI") {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }

    if batchmode {
        return launch_batchmode(&unity, project);
    }

    // Suppress the "Recover Scenes?" modal after a crash.
    let backup = project.join("Temp/__Backupscenes");
    if backup.exists() {
        let dest = project.join(format!("Temp/__Backupscenes.{}", util::timestamp()));
        if let Err(e) = std::fs::rename(&backup, &dest) {
            println!("Warning: failed to move scene backup: {e}");
        }
    }

    if launch(&unity, project)? {
        return Ok(());
    }
    if !run_auth(project, &unity)? {
        return Err(AppError {
            message: "Unity license error".into(),
            detail: Some("Check Unity Hub auth, or install ~/.config/unity-launcher/auth.sh (just install-config)".into()),
        });
    }
    println!("Relaunching...");
    start_unity_detached(&unity, project)?;
    Ok(())
}

fn cmd_focus(project: &Path) -> Result<(), AppError> {
    let Some(pid) = resolve_unity_pid(project) else {
        return Err(AppError {
            message: "Unity not running".into(),
            detail: Some(format!("No live process found for {}", project.display())),
        });
    };
    write_pid(project, pid);
    util::focus_process(pid);
    println!("Focused Unity (pid {pid}).");
    Ok(())
}

fn cmd_quit(project: &Path) -> Result<(), AppError> {
    let Some(pid) = resolve_unity_pid(project) else {
        clear_pid(project);
        println!("Unity not running.");
        return Ok(());
    };
    unsafe { libc::kill(pid, libc::SIGTERM) };
    clear_pid(project);
    println!("Sent SIGTERM to Unity (pid {pid}).");
    Ok(())
}

fn print_usage() {
    eprintln!("Usage: unity-launcher [launch [-batchmode] | focus | quit]");
    eprintln!("  launch (default) — launch Unity, or focus if already running");
    eprintln!("  focus            — focus the running Unity for this project");
    eprintln!("  quit             — send SIGTERM to the running Unity for this project");
}

fn run(batchmode: bool) -> Result<(), AppError> {
    let args: Vec<String> = env::args().skip(1).collect();
    let sub = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);

    // __debug-env doesn't need a project — keep it usable from a sparse-env test.
    if let Some("__debug-env") = sub {
        return cmd_debug_env();
    }

    let project = project_path()?;
    match sub {
        None | Some("launch") => cmd_launch(&project, batchmode),
        Some("focus") => cmd_focus(&project),
        Some("quit") => cmd_quit(&project),
        Some(other) => {
            print_usage();
            Err(AppError {
                message: format!("unknown subcommand: {other}"),
                detail: None,
            })
        }
    }
}

// Hidden diagnostic. Prints HOME as seen by the launcher itself (after ensure_user_env),
// a zsh child (util::shell), and a raw posix_spawn child (same code path as Unity).
fn cmd_debug_env() -> Result<(), AppError> {
    let show = |label: &str, val: &str| {
        println!("{label} HOME={}", if val.is_empty() { "<unset>" } else { val });
    };
    show("self", &env::var("HOME").unwrap_or_default());
    show("shell", util::shell("/usr/bin/printenv HOME", None).output.trim());
    show("spawn", util::spawn_capture(Path::new("/usr/bin/printenv"), &["HOME"]).trim());
    Ok(())
}

fn main() -> ExitCode {
    unsafe { util::ensure_user_env() };
    let batchmode = env::args().any(|a| a == "-batchmode");
    match run(batchmode) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            show_error(&e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    struct Tempdir(PathBuf);
    impl Tempdir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = env::temp_dir().join(format!(
                "unity-launcher-test-{label}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Tempdir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Tempdir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn touch_with_age(path: &Path, age_secs: u64) {
        fs::write(path, b"").unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        let f = fs::File::open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(when)).unwrap();
    }

    #[test]
    fn is_launch_log_excludes_auth_and_other_logs() {
        assert!(is_launch_log("unity-2026-05-28T06-55-40Z.log"));
        assert!(!is_launch_log("unity-auth-2026-05-28T06-55-40Z.log"));
        assert!(!is_launch_log("AssetImportWorker0.log"));
        assert!(!is_launch_log("unity-2026.txt"));
        assert!(!is_launch_log(".DS_Store"));
    }

    #[test]
    fn is_auth_log_matches_only_auth_logs() {
        assert!(is_auth_log("unity-auth-2026-05-28T06-55-40Z.log"));
        assert!(!is_auth_log("unity-2026-05-28T06-55-40Z.log"));
        assert!(!is_auth_log("auth.log"));
    }

    fn write_project_version(dir: &Path, contents: &str) {
        fs::create_dir_all(dir.join("ProjectSettings")).unwrap();
        fs::write(dir.join("ProjectSettings/ProjectVersion.txt"), contents).unwrap();
    }

    #[test]
    fn unity_version_parses_typical_file() {
        let tmp = Tempdir::new("uv-typical");
        write_project_version(
            tmp.path(),
            "m_EditorVersion: 6000.2.7f2\nm_EditorVersionWithRevision: 6000.2.7f2 (abc)\n",
        );
        assert_eq!(unity_version(tmp.path()).unwrap(), "6000.2.7f2");
    }

    #[test]
    fn unity_version_trims_surrounding_whitespace() {
        let tmp = Tempdir::new("uv-ws");
        write_project_version(tmp.path(), "m_EditorVersion:    6000.2.7f2   \n");
        assert_eq!(unity_version(tmp.path()).unwrap(), "6000.2.7f2");
    }

    #[test]
    fn unity_version_errors_when_file_missing() {
        let tmp = Tempdir::new("uv-missing");
        let err = unity_version(tmp.path()).unwrap_err();
        assert_eq!(err.message, "Could not read Unity version");
    }

    #[test]
    fn unity_version_errors_when_marker_line_missing() {
        let tmp = Tempdir::new("uv-no-marker");
        write_project_version(tmp.path(), "something-else: foo\n");
        let err = unity_version(tmp.path()).unwrap_err();
        assert_eq!(err.message, "Could not parse Unity version");
    }

    #[test]
    fn prune_logs_keeps_n_newest_by_mtime() {
        let tmp = Tempdir::new("prune-newest");
        let names = [
            "unity-2026-01-01T00-00-00Z.log",
            "unity-2026-02-01T00-00-00Z.log",
            "unity-2026-03-01T00-00-00Z.log",
            "unity-2026-04-01T00-00-00Z.log",
            "unity-2026-05-01T00-00-00Z.log",
        ];
        for (i, n) in names.iter().enumerate() {
            // Older first: index 0 is oldest (5000s ago), index 4 is newest (1000s ago).
            touch_with_age(&tmp.path().join(n), (5 - i as u64) * 1000);
        }
        prune_logs(tmp.path(), is_launch_log, 2);
        for i in 0..3 {
            assert!(!tmp.path().join(names[i]).exists(), "expected {} deleted", names[i]);
        }
        for i in 3..5 {
            assert!(tmp.path().join(names[i]).exists(), "expected {} kept", names[i]);
        }
    }

    // Regression test for the prefix-collision bug fix. Without is_launch_log's
    // !starts_with("unity-auth-") guard, an old auth log would be pruned as a launch log.
    #[test]
    fn prune_launch_logs_does_not_touch_auth_logs() {
        let tmp = Tempdir::new("prune-isolate");
        let launch_logs: Vec<PathBuf> = (0..5)
            .map(|i| {
                let p = tmp.path().join(format!("unity-2026-0{}-01T00-00-00Z.log", i + 1));
                touch_with_age(&p, (5 - i) * 1000);
                p
            })
            .collect();
        let auth_log = tmp.path().join("unity-auth-2026-01-01T00-00-00Z.log");
        // Make auth log the oldest so a naive prefix-only matcher would delete it.
        touch_with_age(&auth_log, 99999);

        prune_logs(tmp.path(), is_launch_log, 2);

        assert!(auth_log.exists(), "auth log must survive launch-log pruning");
        let surviving = launch_logs.iter().filter(|p| p.exists()).count();
        assert_eq!(surviving, 2);
    }

    #[test]
    fn prune_logs_is_noop_under_cap() {
        let tmp = Tempdir::new("prune-under");
        for i in 0..3 {
            touch_with_age(&tmp.path().join(format!("unity-2026-0{i}-01T00-00-00Z.log")), 100);
        }
        prune_logs(tmp.path(), is_launch_log, 10);
        let count = fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(count, 3);
    }
}
