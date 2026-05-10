# unity-launcher

Tiny native launcher for Unity projects on macOS. Reads `m_EditorVersion` and
launches the matching Unity Hub editor — or focuses an existing instance.

Targets **<10ms** on the "already running" fast path.

## Build

```sh
just            # → bin/unity-launcher
just install    # symlink to ~/.local/bin/unity-launcher
```

`bin/unity-launcher` is version-controlled so consumers can grab it without a
Rust toolchain.

## Subcommands

| Command                     | Behavior                                                                  |
|-----------------------------|---------------------------------------------------------------------------|
| `unity-launcher` (default)  | Launch Unity, or focus the existing instance if one is running.           |
| `unity-launcher launch`     | Same as above. Pass `-batchmode` to run headless (auto-pairs `-nographics`). |
| `unity-launcher focus`      | Focus the running Unity for this project.                                 |
| `unity-launcher quit`       | Send `SIGTERM` to the running Unity for this project.                     |

The launcher locates the project by walking up from its own executable path
looking for `ProjectSettings/ProjectVersion.txt` — drop the binary anywhere
inside the project tree (raw at the root, or wrapped in a `.app`).

## PID tracking

After spawning Unity, the PID is recorded at `<project>/Temp/.unity-launcher.pid`.
`focus` and `quit` read that file first, then validate the PID is still a live
`Unity` process. If the file is missing or stale (Unity launched outside this
tool, or crashed), it falls back to a process scan.

## Auth hook (optional)

On license-init failure during launch, the launcher checks for
`<project>/.unity-launcher/auth.sh`. If present, it runs (capturing output to
`<project>/Logs/unity-auth-*.log`) and then relaunches Unity. Lines beginning
with `ERROR:` are surfaced in the failure dialog.

## Profile

```sh
PROJECT=~/path/to/unity-project just profile
```

Runs hyperfine 100× on the fast path (Unity already running). `UL_NO_FOCUS=1`
is set to skip the activation call so the benchmark doesn't steal window
focus on every iteration.
