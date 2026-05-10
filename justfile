default: build

# Build release binary and copy into version-controlled bin/
build:
    cargo build --release
    mkdir -p bin
    cp target/release/unity-launcher bin/unity-launcher

# Symlink ~/.local/bin/unity-launcher → bin/unity-launcher
install: build
    mkdir -p ~/.local/bin
    ln -sf "{{justfile_directory()}}/bin/unity-launcher" ~/.local/bin/unity-launcher
    @echo "→ ~/.local/bin/unity-launcher"

# Project-specific: copy into meow-tower's !meow.app bundle
install-meow: build
    cp bin/unity-launcher "$MEOW_CLIENT/!meow.app/Contents/MacOS/unity-launcher"
    @echo "→ $MEOW_CLIENT/!meow.app/Contents/MacOS/unity-launcher"

# Profile fast-path latency (Unity already running). Targets <10ms.
# Skips the focus call (UL_NO_FOCUS=1) so the benchmark doesn't steal window focus
# on every iteration. Usage: PROJECT=/path/to/unity-project just profile
profile: build
    #!/usr/bin/env bash
    set -eu
    : "${PROJECT:?set PROJECT=/path/to/unity-project}"
    target_dir="$PROJECT/.unity-launcher-profile"
    mkdir -p "$target_dir"
    trap 'rm -rf "$target_dir"' EXIT
    cp bin/unity-launcher "$target_dir/unity-launcher"
    bin_path="$target_dir/unity-launcher"
    UL_NO_FOCUS=1 hyperfine --warmup 10 --runs 100 --shell=none "$bin_path"

clean:
    cargo clean
    rm -rf bin
