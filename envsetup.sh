#!/bin/bash

# Check if TOP_DIR is already set
if [ -z "$TOP_DIR" ]; then
    # Set TOP_DIR to the current working directory if it's not already set
    TOP_DIR=$(pwd)
    export TOP_DIR
else
    echo "TOP_DIR is already set to $TOP_DIR."
fi

if [[ "$OSTYPE" == "darwin"* ]]; then
    export ANDROID_HOME=$HOME/Library/Android/sdk
elif [ "$OSTYPE" = "linux"* ]; then
    export ANDROID_HOME=$HOME/Android/Sdk
fi

if ! echo "$PATH" | grep -q -E "(^|:)$ANDROID_HOME/tools(:|$)"; then
    export PATH=$PATH:$ANDROID_HOME/tools:$ANDROID_HOME/tools/bin:$ANDROID_HOME/platform-tools
fi

function ndk_build() {
    read_remote_android

    # Build the main project first
    if cargo ndk --no-strip -t $cargo_ndk_target build && cargo ndk --no-strip -t $cargo_ndk_target -- test --no-run; then
        echo "Main build successful, building examples..."

        # Build examples from rsproperties package
        echo "Building rsproperties examples..."
        cargo ndk --no-strip -t $cargo_ndk_target build --examples -p rsproperties

        # Build examples from rsproperties-service package
        echo "Building rsproperties-service examples..."
        cargo ndk --no-strip -t $cargo_ndk_target build --examples -p rsproperties-service

        echo "All builds completed successfully!"
    else
        echo "Main build failed, skipping examples build."
        return 1
    fi
}

function ndk_sync() {
    read_remote_android

    if [[ "$OSTYPE" == "darwin"* ]]; then
        # macOS - use BSD find syntax with +111 for executable files
        find_command="find \"$source_directory\" -maxdepth 2 -type f -perm +111"
    else
        # Linux - use GNU find syntax
        find_command="find \"$source_directory\" -maxdepth 2 -type f -executable"
    fi

    echo "Syncing files from: $source_directory"
    echo "To remote directory: $remote_directory"

    if [ ! -d "$source_directory" ]; then
        echo "Error: Source directory does not exist: $source_directory"
        echo "Please run 'ndk_build' first to build the project."
        return 1
    fi

    eval $find_command | while read file; do
        echo "Pushing: $(basename "$file")"
        adb push "$file" "$remote_directory/"
    done
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
        echo "/data/rsproperties"
        exit 1
    fi

    {
        read cargo_ndk_target
        read ndk_target
        read remote_directory
    } <"$file"

    source_directory="$TOP_DIR/target/$ndk_target-linux-android/debug"
}

function ndk_prepare() {
    read_remote_android

    adb root
    if adb shell ls $remote_directory 1>/dev/null 2>&1; then
        echo "Directory already exists: $remote_directory"
    else
        echo "Directory does not exist, creating: $remote_directory"
        adb shell mkdir -p $remote_directory
    fi
}

function version_update() {
    local NEW_VERSION="$1"

    find . -name "Cargo.toml" -exec sed -i '' "s/^version = \".*\"/version = \"$NEW_VERSION\"/" {} \;
    find . -name "Cargo.toml" -exec sed -i '' "/version = \"[^\"]*\", path =/ s/version = \"[^\"]*\"/version = \"$NEW_VERSION\"/" {} \;
}
