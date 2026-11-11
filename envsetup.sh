#!/bin/bash

# Check if TOP_DIR is already set
if [ -z "$TOP_DIR" ]; then
    # Set TOP_DIR to the current working directory if it's not already set
    TOP_DIR=$(pwd)
    export TOP_DIR
else
    echo "TOP_DIR is already set to $TOP_DIR."
fi

# The Android Studio default SDK location, only when ANDROID_HOME is unset:
# CI (setup-android) and custom installs set it themselves.
if [ -z "$ANDROID_HOME" ]; then
    case "$OSTYPE" in
    darwin*) export ANDROID_HOME=$HOME/Library/Android/sdk ;;
    linux*) export ANDROID_HOME=$HOME/Android/Sdk ;;
    esac
fi

if ! echo "$PATH" | grep -q -E "(^|:)$ANDROID_HOME/tools(:|$)"; then
    export PATH=$PATH:$ANDROID_HOME/tools:$ANDROID_HOME/tools/bin:$ANDROID_HOME/platform-tools
fi

# Every function below returns non-zero on the first failure, so a caller
# can chain them with `&&` and a CI step fails even where bash's `-e` is
# off (inside a function called from an `&&` list).

# The examples ndk_sync pushes and ndk_run_tests smoke-tests.
NDK_EXAMPLES="getprop setprop"

# The test binaries of the last ndk_build, one host path per line.
function ndk_test_manifest() {
    echo "$source_directory/android-tests.txt"
}

function ndk_build() {
    read_remote_android || return 1
    if ! command -v jq >/dev/null; then
        echo "Error: jq is required to list the test binaries."
        return 1
    fi

    # No features: the Android client is built without `builder`.
    echo "Building rsproperties..."
    cargo ndk -t "$cargo_ndk_target" build --no-default-features -p rsproperties || return 1
    echo "Building rsproperties examples..."
    cargo ndk -t "$cargo_ndk_target" build --examples -p rsproperties || return 1

    # The test binaries come from cargo's own artifact list, not from a
    # search of the target directory: ndk_sync pushes exactly these and
    # ndk_run_tests runs exactly these, so a binary that does not reach
    # the device fails the run instead of being skipped.
    echo "Building rsproperties tests..."
    local manifest json
    manifest=$(ndk_test_manifest)
    json=$(cargo ndk -t "$cargo_ndk_target" -- test --no-run -p rsproperties --message-format=json) || return 1
    printf '%s\n' "$json" \
        | jq -R -r 'fromjson? | select(.reason == "compiler-artifact" and .profile.test and .executable != null) | .executable' \
        >"$manifest" || return 1
    if [ ! -s "$manifest" ]; then
        echo "Error: cargo reported no test binaries."
        return 1
    fi
    echo "Test binaries ($(wc -l <"$manifest")):"
    sed 's|.*/|  |' "$manifest"
}

function ndk_sync() {
    read_remote_android || return 1

    local manifest file name
    manifest=$(ndk_test_manifest)
    if [ ! -s "$manifest" ]; then
        echo "Error: $manifest is missing; run 'ndk_build' first."
        return 1
    fi

    echo "Syncing files from: $source_directory"
    echo "To remote directory: $remote_directory"
    for name in $NDK_EXAMPLES; do
        file="$source_directory/examples/$name"
        echo "Pushing: $name"
        adb push "$file" "$remote_directory/" || { echo "Error: failed to push $file"; return 1; }
    done
    while IFS= read -r file; do
        echo "Pushing: $(basename "$file")"
        adb push "$file" "$remote_directory/" || { echo "Error: failed to push $file"; return 1; }
    done <"$manifest"
}

function read_remote_android() {
    file="REMOTE_ANDROID"

    if [ ! -f "$file" ]; then
        echo "The file '$file' does not exist."
        echo "Please create the '$file' file with the following format:"
        echo
        echo "Please use the cargo ndk target information on the first line"
        echo "and the remote directory information on the second line."
        echo
        echo "Example:"
        echo "arm64-v8a"
        echo "aarch64"
        echo "/data/local/tmp/rsproperties"
        # `return`, not `exit`: this file is sourced, and `exit` would end
        # the caller's shell.
        return 1
    fi

    {
        read cargo_ndk_target
        read ndk_target
        read remote_directory
    } <"$file"

    source_directory="$TOP_DIR/target/$ndk_target-linux-android/debug"
}

function ndk_prepare() {
    read_remote_android || return 1

    adb root || return 1
    adb wait-for-device || return 1
    if adb shell ls $remote_directory 1>/dev/null 2>&1; then
        echo "Directory already exists: $remote_directory"
    else
        echo "Directory does not exist, creating: $remote_directory"
        adb shell mkdir -p $remote_directory || return 1
    fi
    adb shell "rm -rf $remote_directory/*" || return 1
    # ndk_test_shell runs from here as the shell user. The pushed files
    # keep their host mode (0755 from cargo); the directory, if root made
    # it, needs the bits for the shell user to reach them.
    adb shell "chmod 755 $remote_directory" || return 1
}

function version_update() {
    local NEW_VERSION="$1"

    find . -name "Cargo.toml" -exec sed -i '' "s/^version = \".*\"/version = \"$NEW_VERSION\"/" {} \;
    find . -name "Cargo.toml" -exec sed -i '' "/version = \"[^\"]*\", path =/ s/version = \"[^\"]*\"/version = \"$NEW_VERSION\"/" {} \;
}

function update_cargo_lock() {
    echo "Updating Cargo.lock for Rust 1.77 environment..."

    # Check if rustup is available
    if ! command -v rustup &> /dev/null; then
        echo "Error: rustup is not installed. Please install rustup first."
        return 1
    fi

    # Install or update to Rust 1.77 if not already installed
    echo "Ensuring Rust 1.77 toolchain is available..."
    rustup toolchain install 1.77

    # Remove existing Cargo.lock files to force regeneration
    echo "Removing existing Cargo.lock files..."
    find . -name "Cargo.lock" -type f -delete

    # Update dependencies and regenerate Cargo.lock using Rust 1.77
    echo "Updating dependencies and regenerating Cargo.lock with Rust 1.77..."
    rustup run 1.77 cargo update

    # Verify the update was successful
    if [ $? -eq 0 ]; then
        echo "Successfully updated Cargo.lock for Rust 1.77!"
        echo "Used Rust version:"
        rustup run 1.77 rustc --version
    else
        echo "Error: Failed to update Cargo.lock"
        return 1
    fi
}

# Runs the smoke test and every test binary as root (ndk_prepare ran
# `adb root`). Returns the run's exit status. No test data is pushed: the
# tests that read `tests/android` or `__properties__` are builder tests,
# which the Android build (no `builder` feature) does not include.
function ndk_test() {
    ndk_run_tests
}

# Runs the same binaries as the shell user. adbd restarts unrooted, so the
# tests run in the `shell` SELinux domain, where reading some property
# areas is denied — paths a root (`su` domain) run never reaches.
function ndk_test_shell() {
    read_remote_android || return 1
    adb unroot || return 1
    adb wait-for-device || return 1
    local uid
    uid=$(adb shell id -u | tr -d '\r') || return 1
    if [ "$uid" = 0 ]; then
        echo "Error: adbd still runs as root after 'adb unroot'."
        return 1
    fi
    ndk_run_tests
}

# Smoke-tests getprop/setprop, then runs every binary of the test manifest
# once, as whichever user adbd runs as. A binary missing on the device
# fails the run. `--test-threads=1` for all: the tests share the device's
# properties (e.g. `wait(None, …)` returns on any change), so tests of one
# binary running in parallel could wake or overwrite each other.
function ndk_run_tests() {
    read_remote_android || return 1

    local manifest names
    manifest=$(ndk_test_manifest)
    if [ ! -s "$manifest" ]; then
        echo "Error: $manifest is missing; run 'ndk_build' first."
        return 1
    fi
    names=$(sed 's|.*/||' "$manifest" | tr '\n' ' ')

    echo "Running tests on Android device..."
    local status=0
    adb shell "
        cd $remote_directory || exit 1
        export RUST_BACKTRACE=1
        export RUST_LOG=debug
        echo \"Running as uid \$(id -u) (\$(id -Z 2>/dev/null)) in \$(pwd)\"

        ./getprop ro.build.version.sdk || { echo '❌ getprop failed'; exit 1; }
        ./setprop debug.test.prop test_value || { echo '❌ setprop failed'; exit 1; }
        if [ \"\$(./getprop debug.test.prop)\" != test_value ]; then
            echo '❌ debug.test.prop does not read back as set'
            exit 1
        fi
        echo '---'

        count=0
        for test_file in $names; do
            if [ ! -x \"\$test_file\" ]; then
                echo \"❌ \$test_file is not on the device\"
                exit 1
            fi
            echo \"Running: \$test_file\"
            ./\"\$test_file\" --test-threads=1 || { echo \"❌ \$test_file failed\"; exit 1; }
            count=\$((count + 1))
            echo '---'
        done
        echo \"✅ \$count test binaries passed\"
    " || status=$?
    if [ $status -ne 0 ]; then
        echo "❌ Tests failed on the device (exit $status)"
    fi
    return $status
}
