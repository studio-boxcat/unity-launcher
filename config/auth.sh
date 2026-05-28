#!/bin/zsh -f
# Unity license recovery hook. Invoked by unity-launcher after a license-init failure.
# Symlinked into ~/.config/unity-launcher/auth.sh by `just install-config`.
# `-f` skips .zshenv so the user's shell config can't override $UNITY (or anything
# else the launcher injects into the environment).
#
# Env from launcher:
#   UNITY  — absolute path to the Unity binary for the failing project
# Sibling files (in ~/.config/unity-launcher/, not the repo):
#   credentials.env — defines UNITY_USERNAME, UNITY_PASSWORD, UNITY_SERIAL_KEY (optional)

set -u

# Resolve sibling files relative to the symlink, not the repo target.
CONFIG_DIR="$(dirname "$0")"
CREDS_FILE="$CONFIG_DIR/credentials.env"

if [[ ! -f "$CREDS_FILE" ]]; then
    echo "ERROR: Credentials file not found"
    echo "ERROR: Create $CREDS_FILE with UNITY_USERNAME and UNITY_PASSWORD"
    exit 1
fi

# Prefixed names avoid shadowing zsh built-ins ($USERNAME auto-populates to login user).
source "$CREDS_FILE"

if [[ -z "${UNITY_USERNAME:-}" ]]; then
    echo "ERROR: UNITY_USERNAME not set"
    echo "ERROR: Add UNITY_USERNAME='your@email.com' to $CREDS_FILE"
    exit 1
fi

if [[ -z "${UNITY_PASSWORD:-}" ]]; then
    echo "ERROR: UNITY_PASSWORD not set"
    echo "ERROR: Add UNITY_PASSWORD='yourpassword' to $CREDS_FILE"
    exit 1
fi

if [[ -z "${UNITY:-}" || ! -x "$UNITY" ]]; then
    echo "ERROR: Unity binary not provided or not executable"
    echo "ERROR: UNITY=${UNITY:-<unset>}"
    exit 1
fi

echo "Authenticating Unity ($UNITY)..."

# Array, not string: zsh does not word-split unquoted parameter expansions, so a string
# "-serial KEY" would be passed as a single argv element. Array preserves the pair.
serial_args=()
[[ -n "${UNITY_SERIAL_KEY:-}" ]] && serial_args=(-serial "$UNITY_SERIAL_KEY")

# -createProject points Unity at a throwaway dir so we don't load the real project
# (which may have compile errors that derail the license activation).
TEMP_PROJECT=$(mktemp -d)
"$UNITY" -quit -batchmode -createProject "$TEMP_PROJECT" -username "$UNITY_USERNAME" -password "$UNITY_PASSWORD" "${serial_args[@]}" 2>&1
EXIT_CODE=$?
rm -rf "$TEMP_PROJECT" 2>/dev/null

if [[ "$EXIT_CODE" -eq 0 ]]; then
    echo "SUCCESS: Unity authenticated"
    exit 0
fi

echo "ERROR: Authentication failed (exit $EXIT_CODE)"

LOG_FILE="$HOME/Library/Logs/Unity/Editor.log"
if [[ -f "$LOG_FILE" ]]; then
    if grep -q "com.unity.editor.headless" "$LOG_FILE"; then
        echo "ERROR: Batchmode requires Unity Pro. Sign in via Unity Hub instead."
    elif grep -q "Invalid username or password" "$LOG_FILE"; then
        echo "ERROR: Invalid username or password"
    elif grep -q "License activation failed" "$LOG_FILE"; then
        echo "ERROR: License activation failed - check serial key"
    elif grep -q "No network connection" "$LOG_FILE"; then
        echo "ERROR: No network connection"
    elif grep -q "Too many activations" "$LOG_FILE"; then
        echo "ERROR: Too many activations - deactivate on another machine"
    else
        LAST_ERROR=$(grep -i "licensing.*error\|error.*licens" "$LOG_FILE" | tail -1)
        if [[ -n "$LAST_ERROR" ]]; then
            echo "ERROR: $LAST_ERROR"
        else
            echo "ERROR: Check $LOG_FILE for details"
        fi
    fi
fi

exit $EXIT_CODE
