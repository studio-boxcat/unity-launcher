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

// Drop-in (`<project>/foo.app/Contents/MacOS/unity-launcher`) wins via the exe
// path; `unity-launcher` from $PATH wins via the cwd fallback.
fn project_path() -> Result<PathBuf, AppError> {
    if let Ok(exe) = env::current_exe() {
        if let Some(p) = walk_up_for_project(&exe) {
            return Ok(p);
        }
    }
    if let Ok(cwd) = env::current_dir() {
        if let Some(p) = walk_up_for_project(&cwd) {
            return Ok(p);
        }
        if cwd.join("ProjectSettings/ProjectVersion.txt").exists() {
            return Ok(cwd);
        }
    }
    Err(AppError {
        message: "Not inside a Unity project".into(),
        detail: Some("ProjectSettings/ProjectVersion.txt not found in any ancestor of the executable or cwd".into()),
    })
}

fn walk_up_for_project(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .skip(1)
        .find(|p| p.join("ProjectSettings/ProjectVersion.txt").exists())
        .map(Path::to_path_buf)
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

fn run_auth(project: &Path) -> Result<bool, AppError> {
    let script = project.join(".unity-launcher/auth.sh");
    if !script.exists() {
        return Ok(false);
    }

    println!("Running auth hook: {}", script.display());
    let log = project.join(format!("Logs/unity-auth-{}.log", util::timestamp()));
    let r = util::shell(&format!("\"{}\"", script.display()), Some(&log));
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

fn launch(unity: &Path, project: &Path, batchmode: bool) -> Result<bool, AppError> {
    let log = project.join(format!("Logs/unity-{}.log", util::timestamp()));
    let _ = std::fs::create_dir_all(project.join("Logs"));
    println!("Log: {}", log.display());

    if batchmode {
        println!("Pairing -nographics with -batchmode");
        let pid = util::spawn_unity(unity, project, &log, SpawnMode::BatchSync)?;
        let exit = util::waitpid_exit(pid);
        if exit != 0 {
            return Err(AppError {
                message: format!("Unity batchmode exited {exit}"),
                detail: Some(format!("See {}", log.display())),
            });
        }
        return Ok(true);
    }

    let pid = util::spawn_unity(unity, project, &log, SpawnMode::Detached)?;
    println!("Launching Unity (pid {pid})...");
    write_pid(project, pid);

    let iters = (STARTUP_TIMEOUT.as_millis() / POLL_INTERVAL.as_millis()) as usize;
    for _ in 0..iters {
        sleep(POLL_INTERVAL);
        let Ok(c) = std::fs::read_to_string(&log) else { continue };
        if !c.contains("Licensing is initialized") {
            continue;
        }
        if c.contains("Successfully updated license") {
            println!("Unity started.");
            return Ok(true);
        }
        println!("License error.");
        unsafe { libc::kill(pid, libc::SIGTERM) };
        clear_pid(project);
        return Ok(false);
    }
    // License-init didn't appear within the budget. Treat as success — Unity may just
    // be slow today and the user wants to keep using the editor.
    println!("Timeout.");
    Ok(true)
}

fn cmd_launch(project: &Path, batchmode: bool) -> Result<(), AppError> {
    if batchmode {
        println!("[BATCHMODE]");
    }
    println!("Project: {}", project.display());

    // Check for an already-running instance before doing the version/path lookups —
    // those aren't needed on the fast path.
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

    if !batchmode {
        // Suppress the "Recover Scenes?" modal after a crash.
        let backup = project.join("Temp/__Backupscenes");
        if backup.exists() {
            let dest = project.join(format!("Temp/__Backupscenes.{}", util::timestamp()));
            if let Err(e) = std::fs::rename(&backup, &dest) {
                println!("Warning: failed to move scene backup: {e}");
            }
        }
    }

    if launch(&unity, project, batchmode)? {
        return Ok(());
    }

    if !run_auth(project)? {
        return Err(AppError {
            message: "Unity license error".into(),
            detail: Some("Check Unity Hub auth, or add a .unity-launcher/auth.sh hook".into()),
        });
    }
    println!("Relaunching...");
    let log = project.join(format!("Logs/unity-{}.log", util::timestamp()));
    let pid = util::spawn_unity(&unity, project, &log, SpawnMode::Detached)?;
    write_pid(project, pid);
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

// Hidden test/diagnostic helper. Prints HOME as seen by:
//  - the launcher itself (after ensure_user_env)
//  - a child spawned via util::shell (zsh)
//  - a child spawned via util::spawn_capture (raw posix_spawn — same code path as Unity)
fn cmd_debug_env() -> Result<(), AppError> {
    let self_home = env::var("HOME").unwrap_or_else(|_| "<unset>".into());
    println!("self HOME={self_home}");

    let r = util::shell("/usr/bin/printenv HOME", None);
    let shell_home = r.output.trim();
    println!("shell HOME={}", if shell_home.is_empty() { "<unset>" } else { shell_home });

    let spawn_out = util::spawn_capture(Path::new("/usr/bin/printenv"), &["HOME"]);
    let spawn_home = spawn_out.trim();
    println!("spawn HOME={}", if spawn_home.is_empty() { "<unset>" } else { spawn_home });
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
