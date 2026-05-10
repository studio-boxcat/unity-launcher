use std::ffi::CString;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ShellResult {
    pub output: String,
    pub exit: i32,
}

// Synchronous shell, captures combined stdout+stderr. Used only for the auth hook.
pub fn shell(cmd: &str, log: Option<&Path>) -> ShellResult {
    match Command::new("/bin/zsh").args(["-c", cmd]).output() {
        Ok(o) => {
            let mut output = String::from_utf8_lossy(&o.stdout).to_string();
            output.push_str(&String::from_utf8_lossy(&o.stderr));
            let exit = o.status.code().unwrap_or(-1);
            if let Some(p) = log {
                let _ = std::fs::write(p, &output);
            }
            ShellResult { output, exit }
        }
        Err(_) => ShellResult {
            output: String::new(),
            exit: -1,
        },
    }
}

pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z")
}

fn epoch_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let mi = ((secs / 60) % 60) as u32;
    let h = ((secs / 3600) % 24) as u32;
    let mut days = (secs / 86400) as i64;
    let mut y: i64 = 1970;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let dim = [31, if is_leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0usize;
    while mo < 12 && days >= dim[mo] {
        days -= dim[mo];
        mo += 1;
    }
    (y as u32, (mo + 1) as u32, (days + 1) as u32, h, mi, s)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// MARK: Process discovery

pub fn find_processes(needle: &str) -> Vec<libc::pid_t> {
    find_processes_impl(needle, false)
}

pub fn find_process(needle: &str) -> Option<libc::pid_t> {
    find_processes_impl(needle, true).into_iter().next()
}

// Pre-filters on `proc_name` (≤16 chars short name) using the first whitespace-delimited
// token of the needle — drops the per-PID `KERN_PROCARGS2` sysctl from ~all-of-them
// down to a handful, the difference between ~30ms and <1ms here.
fn find_processes_impl(needle: &str, first_only: bool) -> Vec<libc::pid_t> {
    let needle_lower = needle.to_lowercase();
    let comm_filter = needle
        .split_whitespace()
        .next()
        .unwrap_or(needle)
        .to_lowercase();

    unsafe {
        let needed = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if needed <= 0 {
            return vec![];
        }
        let cap = (needed as usize) + 64;
        let mut pids: Vec<libc::pid_t> = vec![0; cap];
        let bytes = libc::proc_listallpids(
            pids.as_mut_ptr() as *mut _,
            (cap * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        );
        if bytes <= 0 {
            return vec![];
        }
        let actual = bytes as usize / std::mem::size_of::<libc::pid_t>();
        pids.truncate(actual);

        let mut matches = vec![];
        let mut name_buf = [0u8; 256];
        for &pid in &pids {
            if pid <= 0 {
                continue;
            }

            let n = libc::proc_name(
                pid,
                name_buf.as_mut_ptr() as *mut _,
                name_buf.len() as u32,
            );
            if n <= 0 {
                continue;
            }
            let name = std::str::from_utf8(&name_buf[..n as usize])
                .unwrap_or("")
                .to_lowercase();
            if name != comm_filter {
                continue;
            }

            let mut arg_mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
            let mut arg_size: libc::size_t = 0;
            if libc::sysctl(
                arg_mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut arg_size,
                std::ptr::null_mut(),
                0,
            ) != 0
                || arg_size == 0
            {
                continue;
            }
            let mut buf: Vec<u8> = vec![0; arg_size];
            if libc::sysctl(
                arg_mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut _,
                &mut arg_size,
                std::ptr::null_mut(),
                0,
            ) != 0
            {
                continue;
            }
            for j in 4..arg_size {
                if buf[j] == 0 {
                    buf[j] = b' ';
                }
            }
            let cmdline = String::from_utf8_lossy(&buf[4..arg_size]).to_lowercase();
            if cmdline.contains(&needle_lower) {
                matches.push(pid);
                if first_only {
                    return matches;
                }
            }
        }
        matches
    }
}

// True if `pid` is a live process whose short name is "Unity".
pub fn is_unity_alive(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe {
        if libc::kill(pid, 0) != 0 {
            return false;
        }
        let mut buf = [0u8; 32];
        let n = libc::proc_name(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32);
        if n <= 0 {
            return false;
        }
        std::str::from_utf8(&buf[..n as usize])
            .map(|s| s.eq_ignore_ascii_case("Unity"))
            .unwrap_or(false)
    }
}

// MARK: Unity spawning
//
// Direct posix_spawn so we capture Unity's real PID (the shell-with-`&` approach
// would give us the shell's PID instead). Unity reparents to launchd when we exit.

pub fn spawn_unity_detached(
    unity: &Path,
    project: &Path,
    log: &Path,
) -> Result<libc::pid_t, super::AppError> {
    spawn_unity(unity, project, log, false, true)
}

pub fn spawn_unity_sync(
    unity: &Path,
    project: &Path,
    log: &Path,
) -> Result<i32, super::AppError> {
    let pid = spawn_unity(unity, project, log, true, false)?;
    unsafe {
        let mut status: libc::c_int = 0;
        libc::waitpid(pid, &mut status, 0);
        Ok(if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        })
    }
}

fn spawn_unity(
    unity: &Path,
    project: &Path,
    log: &Path,
    batchmode: bool,
    detach_stdio: bool,
) -> Result<libc::pid_t, super::AppError> {
    let unity_c = path_c(unity)?;
    let project_c = path_c(project)?;
    let log_c = path_c(log)?;

    let mut argv_owned: Vec<CString> = vec![
        unity_c.clone(),
        c"-projectPath".to_owned(),
        project_c,
        c"-disable-assembly-updater".to_owned(),
        c"-logFile".to_owned(),
        log_c,
    ];
    if batchmode {
        argv_owned.push(c"-batchmode".to_owned());
        argv_owned.push(c"-nographics".to_owned());
    }
    let mut argv: Vec<*const libc::c_char> = argv_owned.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());

    unsafe {
        // Unity reads MONO_CRASH_NOFILE — set in our env before spawn so it inherits.
        libc::setenv(c"MONO_CRASH_NOFILE".as_ptr(), c"1".as_ptr(), 1);

        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        if libc::posix_spawn_file_actions_init(&mut actions) != 0 {
            return Err(super::AppError {
                message: "posix_spawn_file_actions_init failed".into(),
                detail: None,
            });
        }
        if detach_stdio {
            let devnull = c"/dev/null".as_ptr();
            libc::posix_spawn_file_actions_addopen(
                &mut actions, libc::STDIN_FILENO, devnull, libc::O_RDONLY, 0);
            libc::posix_spawn_file_actions_addopen(
                &mut actions, libc::STDOUT_FILENO, devnull, libc::O_WRONLY, 0);
            libc::posix_spawn_file_actions_addopen(
                &mut actions, libc::STDERR_FILENO, devnull, libc::O_WRONLY, 0);
        }

        let mut pid: libc::pid_t = 0;
        let r = libc::posix_spawn(
            &mut pid,
            unity_c.as_ptr(),
            &actions,
            std::ptr::null(),
            argv.as_ptr() as *const *mut _,
            std::ptr::null(),
        );
        libc::posix_spawn_file_actions_destroy(&mut actions);

        if r != 0 {
            return Err(super::AppError {
                message: "Failed to spawn Unity".into(),
                detail: Some(format!("posix_spawn returned {r}")),
            });
        }
        Ok(pid)
    }
}

fn path_c(p: &Path) -> Result<CString, super::AppError> {
    let s = p.to_str().ok_or(super::AppError {
        message: "Path is not valid UTF-8".into(),
        detail: Some(p.display().to_string()),
    })?;
    CString::new(s).map_err(|_| super::AppError {
        message: "Path contains a NUL byte".into(),
        detail: Some(p.display().to_string()),
    })
}

// MARK: Activation
//
// On macOS 14+ a non-app CLI cannot force-activate another app — cooperative activation
// requires NSApp init (~55ms) and osascript is ~149ms once running. We fork(), let the
// child handle osascript, and the parent returns immediately (~0.5ms); the child
// reparents to launchd when we exit.

pub fn focus_process(pid: libc::pid_t) {
    let script = CString::new(format!(
        r#"tell application "System Events" to set frontmost of (first process whose unix id is {pid}) to true"#
    )).expect("script has no NULs");

    unsafe {
        let f = libc::fork();
        if f != 0 {
            return; // parent (or fork failed)
        }

        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        if devnull >= 0 {
            libc::dup2(devnull, libc::STDOUT_FILENO);
            libc::dup2(devnull, libc::STDERR_FILENO);
            if devnull > libc::STDERR_FILENO {
                libc::close(devnull);
            }
        }

        let argv: [*const libc::c_char; 4] = [
            c"osascript".as_ptr(),
            c"-e".as_ptr(),
            script.as_ptr(),
            std::ptr::null(),
        ];
        libc::execv(c"/usr/bin/osascript".as_ptr(), argv.as_ptr() as *const _);
        libc::_exit(127);
    }
}

pub fn show_alert(message: &str, detail: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        r#"display alert "{}" message "{}" as critical"#,
        esc(message),
        esc(detail)
    );
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output();
}
