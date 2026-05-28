// Tests config/auth.sh — exercises the script's credentials validation and the
// argv handling for $UNITY against a stub Unity binary. Covers two zsh-specific
// pitfalls that aren't visible to Rust:
//   - $USERNAME is a zsh built-in (renaming to UNITY_USERNAME avoids the shadow).
//   - zsh does not word-split unquoted parameter expansion (array form needed
//     to pass -serial KEY as two argv elements rather than one).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

fn auth_sh_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("config/auth.sh")
}

struct Tempdir(PathBuf);
impl Tempdir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "unity-launcher-authsh-{label}-{}-{nanos}",
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

fn set_executable(path: &Path) {
    let mut perm = fs::metadata(path).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(path, perm).unwrap();
}

// Stub Unity: appends its argv (one per line) to $ARGV_OUT, then exits 0.
const STUB_UNITY: &str = "#!/bin/sh\n\
: > \"$ARGV_OUT\"\n\
for arg in \"$@\"; do printf '%s\\n' \"$arg\" >> \"$ARGV_OUT\"; done\n\
exit 0\n";

struct Fixture {
    _tmp: Tempdir,
    auth: PathBuf,
    stub: PathBuf,
    argv_out: PathBuf,
}

fn fixture(label: &str, creds: &str) -> Fixture {
    let tmp = Tempdir::new(label);
    // Copy (not symlink) so $(dirname "$0") inside auth.sh resolves to the tempdir.
    let auth = tmp.path().join("auth.sh");
    fs::copy(auth_sh_src(), &auth).unwrap();
    set_executable(&auth);
    fs::write(tmp.path().join("credentials.env"), creds).unwrap();
    let stub = tmp.path().join("unity-stub.sh");
    fs::write(&stub, STUB_UNITY).unwrap();
    set_executable(&stub);
    let argv_out = tmp.path().join("argv.out");
    Fixture { _tmp: tmp, auth, stub, argv_out }
}

fn run(fx: &Fixture) -> Output {
    Command::new(&fx.auth)
        .env("UNITY", &fx.stub)
        .env("ARGV_OUT", &fx.argv_out)
        .output()
        .expect("run auth.sh")
}

#[test]
fn errors_when_unity_username_missing() {
    let fx = fixture("no-username", "UNITY_PASSWORD='pw'\n");
    let out = run(&fx);
    assert!(!out.status.success(), "expected non-zero exit");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ERROR: UNITY_USERNAME not set"),
        "missing expected ERROR line; stdout was:\n{stdout}"
    );
    assert!(!fx.argv_out.exists(), "stub Unity should not have been invoked");
}

#[test]
fn errors_when_unity_password_missing() {
    let fx = fixture("no-password", "UNITY_USERNAME='u@e'\n");
    let out = run(&fx);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ERROR: UNITY_PASSWORD not set"), "stdout was:\n{stdout}");
}

#[test]
fn passes_serial_as_two_argv_elements() {
    let fx = fixture(
        "serial-split",
        "UNITY_USERNAME='u@e'\nUNITY_PASSWORD='pw'\nUNITY_SERIAL_KEY='AA-BB-CC'\n",
    );
    let out = run(&fx);
    assert!(
        out.status.success(),
        "auth.sh failed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let argv = fs::read_to_string(&fx.argv_out).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    let pos = args
        .iter()
        .position(|s| *s == "-serial")
        .unwrap_or_else(|| panic!("no -serial in argv: {args:?}"));
    assert_eq!(args[pos + 1], "AA-BB-CC", "serial key fused with flag: {args:?}");
}

#[test]
fn omits_serial_arg_when_key_unset() {
    let fx = fixture("no-serial", "UNITY_USERNAME='u@e'\nUNITY_PASSWORD='pw'\n");
    let out = run(&fx);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let argv = fs::read_to_string(&fx.argv_out).unwrap();
    assert!(!argv.contains("-serial"), "should omit -serial when key unset: {argv}");
}

#[test]
fn passes_username_and_password_to_unity() {
    let fx = fixture("creds", "UNITY_USERNAME='u@e'\nUNITY_PASSWORD='hunter2'\n");
    let out = run(&fx);
    assert!(out.status.success());
    let argv = fs::read_to_string(&fx.argv_out).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    let u = args.iter().position(|s| *s == "-username").unwrap();
    assert_eq!(args[u + 1], "u@e");
    let p = args.iter().position(|s| *s == "-password").unwrap();
    assert_eq!(args[p + 1], "hunter2");
}
