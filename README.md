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

The launcher locates the project by walking up from its own executable path,
or — if the binary lives outside the project tree (e.g. on `$PATH`) — from the
current working directory, looking for `ProjectSettings/ProjectVersion.txt`.
So both of these work:

- drop-in: `<project>/!unity.app/Contents/MacOS/unity-launcher`
- from `$PATH`: `cd <project> && unity-launcher [focus|quit|...]`

`quit` sends `SIGTERM` — Unity catches it and exits, but it isn't the same as
a cmd-Q AppleEvent quit. If your project needs an in-editor "save then quit"
hook, drive it through your own command channel.

## PID tracking

After spawning Unity, the PID is recorded at `<project>/Temp/.unity-launcher.pid`.
`focus` and `quit` read that file first, then validate the PID is still a live
`Unity` process. If the file is missing or stale (Unity launched outside this
tool, or crashed), it falls back to a process scan.

## Auth hook (optional)

On license-init failure during launch, the launcher checks for
`$XDG_CONFIG_HOME/unity-launcher/auth.sh` (default `~/.config/unity-launcher/auth.sh`).
If present, it runs with `UNITY` set to the failing project's Unity binary path
(output captured to `<project>/Logs/unity-auth-*.log`), then relaunches Unity.
Lines beginning with `ERROR:` are surfaced in the launcher's error output.

A reference hook lives at `config/auth.sh` in this repo. Install it with:

```sh
just install-config
cp config/credentials.env.example ~/.config/unity-launcher/credentials.env
# edit ~/.config/unity-launcher/credentials.env with USERNAME / PASSWORD / SERIAL_KEY
```

`credentials.env` sits next to the symlink (i.e. in `~/.config/unity-launcher/`),
never in the repo. The hook resolves it via `$(dirname "$0")`, so the symlink
location wins over the symlink target.

## Profile

```sh
PROJECT=~/path/to/unity-project just profile
```

Runs hyperfine 100× on the fast path (Unity already running). `UL_NO_FOCUS=1`
is set to skip the activation call so the benchmark doesn't steal window
focus on every iteration.
