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

    # Build the main project first with --release and without tests
    # Explicitly specify no features to ensure builder is not enabled
    if cargo ndk -t $cargo_ndk_target build --no-default-features -p rsproperties && cargo ndk -t $cargo_ndk_target -- test --no-run -p rsproperties; then
        echo "Main build successful, building examples..."

        # Build examples from rsproperties package without builder feature
        echo "Building rsproperties examples..."
        cargo ndk -t $cargo_ndk_target build --examples -p rsproperties

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
    adb shell "rm -rf $remote_directory/*"
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

function ndk_test() {
    read_remote_android
    echo "Copying test data files first..."

    # Copy test data files (Android property files needed by tests)
    if [ -d "$TOP_DIR/rsproperties/tests/android" ]; then
        echo "Creating test data directory on device..."
        adb shell "mkdir -p $remote_directory/rsproperties/tests"
        echo "Copying Android test data files..."
        adb push "$TOP_DIR/rsproperties/tests/android" "$remote_directory/rsproperties/tests/"
        echo "Test data files copied successfully"
    else
        echo "Warning: rsproperties/tests/android directory not found"
    fi

    # Copy property info files from __properties__ directory
    if [ -d "$TOP_DIR/rsproperties/__properties__" ]; then
        echo "Creating properties directory on device..."
        adb shell "mkdir -p $remote_directory/rsproperties"
        echo "Copying property info files..."
        adb push "$TOP_DIR/rsproperties/__properties__" "$remote_directory/rsproperties/"
        echo "Property info files copied successfully"
    fi

    echo "Copying test executables from deps directory..."
    deps_directory="$source_directory/deps"
    if [ -d "$deps_directory" ]; then
        echo "Found deps directory: $deps_directory"
        echo "Looking for rsproperties test executables..."
        find "$deps_directory" -name "*rsproperties*" -type f -executable | grep -E "(test|spec)" | while read test_file; do
            echo "Copying test file: $test_file"
            adb push "$test_file" "$remote_directory/"
        done
        echo "Test executable copy completed"
    else
        echo "deps directory not found: $deps_directory"
        echo "Looking for test binaries in alternative locations..."
        find "$TOP_DIR/target" -name "*rsproperties*" -type f -executable | grep -E "(test|spec)" | head -5 | while read test_file; do
            echo "Found alternative test file: $test_file"
            adb push "$test_file" "$remote_directory/"
        done
    fi

    # Copy basic executables, made executable and copy getprop/setprop
    find "$source_directory" -name "getprop" -o -name "setprop" | while read basic_file; do
        echo "Copying basic executable: $basic_file"
        adb push "$basic_file" "$remote_directory/"
    done

    echo "Test files and executables copied to Android device"
    echo "Running tests on Android device..."
    adb shell "
            cd $remote_directory
            export RUST_BACKTRACE=1
            export RUST_LOG=debug

            echo 'Test environment setup:'
            echo 'Working directory: \$(pwd)'
            echo 'Available files:'
            ls -la

            # Test getprop command
            if [ -f \"getprop\" ] && [ -x \"getprop\" ]; then
                echo \"✅ Testing getprop command...\"
                chmod +x getprop
                echo \"Running: ./getprop ro.build.version.sdk\"
                ./getprop ro.build.version.sdk || { echo \"❌ getprop command failed\"; exit 1; }
                echo \"---\"
            else
                echo \"❌ getprop executable not found\"
                exit 1
            fi

            # Test setprop command
            if [ -f \"setprop\" ] && [ -x \"setprop\" ]; then
                echo \"✅ Testing setprop command...\"
                chmod +x setprop
                echo \"Running: ./setprop debug.test.prop test_value\"
                ./setprop debug.test.prop test_value || { echo \"❌ setprop command failed\"; exit 1; }
                # Verify the property was set correctly
                if [ \"\$(./getprop debug.test.prop)\" = \"test_value\" ]; then
                    echo \"✅ Property set successfully!\"
                else
                    echo \"❌ Failed to set property or verify its value\"
                    exit 1
                fi
                echo \"---\"
            else
                echo \"❌ setprop executable not found\"
                exit 1
            fi

            # Run all test executables except those starting with getprop or setprop
            for test_file in *; do
                if [ -f \"\$test_file\" ] && [ -x \"\$test_file\" ] && [[ \"\$test_file\" != getprop* ]] && [[ \"\$test_file\" != setprop* ]]; then
                    echo \"Running executable: \$test_file\"
                    chmod +x \"\$test_file\"
                    if ./\"\$test_file\"; then
                        echo \"✅ \$test_file executed successfully\"
                    else
                        echo \"❌ \$test_file failed with error code \$?\"
                        exit 1
                    fi
                    echo \"Finished running: \$test_file\"
                    echo \"---\"
                fi
            done            # Run specific tests for rsproperties executables (excluding getprop/setprop)
            for test_file in *rsproperties*; do
                if [ -f \"\$test_file\" ] && [ -x \"\$test_file\" ] && [[ \"\$test_file\" != getprop* ]] && [[ \"\$test_file\" != setprop* ]]; then
                    echo \"Running rsproperties test: \$test_file\"
                    chmod +x \"\$test_file\"
                    if ./\"\$test_file\" --test-threads=1; then
                        echo \"✅ Test \$test_file PASSED\"
                    else
                        echo \"❌ Test \$test_file FAILED\"
                        exit 1
                    fi
                    echo \"Finished running: \$test_file\"
                    echo \"---\"
                fi
            done

            echo '✅ All tests passed!'
        "
        test_exit_code=$?
        if [ $test_exit_code -ne 0 ]; then
            echo "❌ Tests failed with exit code: $test_exit_code"
        else
            echo "✅ All tests passed on Android device"
        fi
    # No else block needed as tests always run
}
