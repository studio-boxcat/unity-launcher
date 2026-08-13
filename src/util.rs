use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// macOS quirk: posix_spawn(envp=NULL) reads from a snapshot taken at process start,
// not the live `environ`. Setenv-after-launch updates aren't visible to the child.
// Pass `*_NSGetEnviron()` explicitly so the child sees our HOME/USER/LOGNAME backfill.
unsafe extern "C" {
    fn _NSGetEnviron() -> *mut *mut *mut libc::c_char;
}

unsafe fn current_envp() -> *const *mut libc::c_char {
    *_NSGetEnviron() as *const *mut libc::c_char
}

#[derive(Debug)]
pub struct AppError {
    pub message: String,
    pub detail: Option<String>,
}

pub struct ShellResult {
    pub output: String,
    pub exit: i32,
}

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

// Hand-rolled to avoid pulling chrono/time for one timestamp string.
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
    let comm_filter = needle
        .split_whitespace()
        .next()
        .unwrap_or(needle)
        .as_bytes();
    let needle_bytes = needle.as_bytes();

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
        // proc_listallpids returns how many PIDs it wrote, not how many bytes. Clamp rather than
        // trust it — unwritten slots stay 0 and are skipped below either way.
        let actual = (bytes as usize).min(cap);
        pids.truncate(actual);

        let mut matches = vec![];
        let mut name_buf = [0u8; 256];
        for &pid in &pids {
            if pid <= 0 {
                continue;
            }

            let n = libc::proc_name(pid, name_buf.as_mut_ptr() as *mut _, name_buf.len() as u32);
            if n <= 0 {
                continue;
            }
            if !name_buf[..n as usize].eq_ignore_ascii_case(comm_filter) {
                continue;
            }

            let mut arg_mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
            let mut arg_size: libc::size_t = 0;
            if libc::sysctl(arg_mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut arg_size, std::ptr::null_mut(), 0) != 0
                || arg_size == 0
            {
                continue;
            }
            let mut buf: Vec<u8> = vec![0; arg_size];
            if libc::sysctl(arg_mib.as_mut_ptr(), 3, buf.as_mut_ptr() as *mut _, &mut arg_size, std::ptr::null_mut(), 0) != 0 {
                continue;
            }
            // Replace argv null separators with spaces so a substring search spans
            // argv boundaries. The first 4 bytes are KERN_PROCARGS2's argc header.
            for byte in &mut buf[4..arg_size] {
                if *byte == 0 {
                    *byte = b' ';
                }
            }
            if memmem_ci(&buf[4..arg_size], needle_bytes) {
                matches.push(pid);
                if first_only {
                    return matches;
                }
            }
        }
        matches
    }
}

// ASCII case-insensitive substring search. Avoids allocating lowered-case copies.
fn memmem_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    for window in haystack.windows(needle.len()) {
        if window.eq_ignore_ascii_case(needle) {
            return true;
        }
    }
    false
}

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
        n > 0 && buf[..n as usize].eq_ignore_ascii_case(b"Unity")
    }
}

// MARK: Unity spawning
//
// Direct posix_spawn so we capture Unity's real PID (the shell-with-`&` approach
// would give us the shell's PID instead). Unity reparents to launchd when we exit.

pub enum SpawnMode {
    /// Foreground editor: detach stdio so we can exit while Unity keeps running.
    Detached,
    /// Headless: inherit stdio, caller will waitpid.
    BatchSync,
}

pub fn spawn_unity(
    unity: &Path,
    project: &Path,
    log: &Path,
    mode: SpawnMode,
) -> Result<libc::pid_t, AppError> {
    let unity_c = path_c(unity);
    let project_c = path_c(project);
    let log_c = path_c(log);

    let mut argv_owned: Vec<CString> = vec![
        unity_c.clone(),
        c"-projectPath".to_owned(),
        project_c,
        c"-disable-assembly-updater".to_owned(),
        c"-logFile".to_owned(),
        log_c,
    ];
    if matches!(mode, SpawnMode::BatchSync) {
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
            return Err(AppError {
                message: "posix_spawn_file_actions_init failed".into(),
                detail: None,
            });
        }
        if matches!(mode, SpawnMode::Detached) {
            let devnull = c"/dev/null".as_ptr();
            libc::posix_spawn_file_actions_addopen(&mut actions, libc::STDIN_FILENO, devnull, libc::O_RDONLY, 0);
            libc::posix_spawn_file_actions_addopen(&mut actions, libc::STDOUT_FILENO, devnull, libc::O_WRONLY, 0);
            libc::posix_spawn_file_actions_addopen(&mut actions, libc::STDERR_FILENO, devnull, libc::O_WRONLY, 0);
        }

        let mut pid: libc::pid_t = 0;
        let r = libc::posix_spawn(
            &mut pid,
            unity_c.as_ptr(),
            &actions,
            std::ptr::null(),
            argv.as_ptr() as *const *mut _,
            current_envp(),
        );
        libc::posix_spawn_file_actions_destroy(&mut actions);

        if r != 0 {
            return Err(AppError {
                message: "Failed to spawn Unity".into(),
                detail: Some(format!("posix_spawn returned {r}")),
            });
        }
        Ok(pid)
    }
}

// Spawn a command via raw posix_spawn (mimicking spawn_unity), capture stdout.
// Used by the __debug-env test path to verify env inheritance through the same
// code path Unity uses.
pub fn spawn_capture(path: &Path, args: &[&str]) -> String {
    use std::ffi::CString;
    use std::io::Read;
    let path_c = path_c(path);
    let arg_cs: Vec<CString> = std::iter::once(path_c.clone())
        .chain(args.iter().map(|a| CString::new(*a).expect("arg has no NULs")))
        .collect();
    let mut argv: Vec<*const libc::c_char> = arg_cs.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    unsafe {
        let mut fds = [0i32; 2];
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return String::new();
        }
        let (rd, wr) = (fds[0], fds[1]);

        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        libc::posix_spawn_file_actions_init(&mut actions);
        libc::posix_spawn_file_actions_adddup2(&mut actions, wr, libc::STDOUT_FILENO);
        libc::posix_spawn_file_actions_addclose(&mut actions, rd);
        libc::posix_spawn_file_actions_addclose(&mut actions, wr);

        let mut pid: libc::pid_t = 0;
        let r = libc::posix_spawn(
            &mut pid,
            path_c.as_ptr(),
            &actions,
            std::ptr::null(),
            argv.as_ptr() as *const *mut _,
            current_envp(),
        );
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::close(wr);
        if r != 0 {
            libc::close(rd);
            return String::new();
        }

        let mut file = std::fs::File::from_raw_fd(rd);
        let mut buf = String::new();
        let _ = file.read_to_string(&mut buf);
        let mut status: libc::c_int = 0;
        libc::waitpid(pid, &mut status, 0);
        buf
    }
}

pub fn waitpid_exit(pid: libc::pid_t) -> i32 {
    unsafe {
        let mut status: libc::c_int = 0;
        libc::waitpid(pid, &mut status, 0);
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        }
    }
}

fn path_c(p: &Path) -> CString {
    let s = p.to_str().expect("path is valid UTF-8");
    CString::new(s).expect("path contains no NUL byte")
}

// Backfill HOME/USER/LOGNAME from the passwd database when missing — Finder-launched
// .app bundles get a sparse environment, and Unity (and anything else we shell out to)
// refuses to run without HOME. Call once at startup so every child inherits.
pub unsafe fn ensure_user_env() {
    let needs = [c"HOME".as_ptr(), c"USER".as_ptr(), c"LOGNAME".as_ptr()];
    if needs.iter().all(|k| !libc::getenv(*k).is_null()) {
        return;
    }
    let pw = libc::getpwuid(libc::getuid());
    if pw.is_null() {
        return;
    }
    let set_if_missing = |key: *const libc::c_char, val: *const libc::c_char| {
        if !val.is_null() && libc::getenv(key).is_null() {
            libc::setenv(key, val, 1);
        }
    };
    set_if_missing(c"HOME".as_ptr(), (*pw).pw_dir);
    set_if_missing(c"USER".as_ptr(), (*pw).pw_name);
    set_if_missing(c"LOGNAME".as_ptr(), (*pw).pw_name);
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
            return;
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
