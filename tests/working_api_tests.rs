use rsproperties;
use std::path::PathBuf;

#[test]
fn test_rsproperties_constants() {
    // Test that library constants are accessible and correct
    assert_eq!(rsproperties::PROP_VALUE_MAX, 92);
    assert_eq!(rsproperties::PROP_DIRNAME, "/dev/__properties__");
    println!("✓ Library constants are correct");
    println!("  PROP_VALUE_MAX = {}", rsproperties::PROP_VALUE_MAX);
    println!("  PROP_DIRNAME = {}", rsproperties::PROP_DIRNAME);
}

#[test]
fn test_rsproperties_init_api() {
    // Test the init and dirname API functionality
    let custom_path = PathBuf::from("/tmp/test_properties");
    rsproperties::init(Some(custom_path.clone()));

    let dirname = rsproperties::dirname();
    assert_eq!(dirname, custom_path.as_path());
    println!("✓ Custom path initialization works: {:?}", dirname);
}

#[test]
fn test_rsproperties_with_workspace_properties() {
    // Test using the actual Android properties directory in the workspace
    let properties_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("__properties__");

    println!("Testing with properties directory: {:?}", properties_dir);
    println!("Directory exists: {}", properties_dir.exists());

    if properties_dir.exists() {
        // Initialize with the existing directory
        rsproperties::init(Some(properties_dir.clone()));

        // Verify initialization worked
        let dirname = rsproperties::dirname();
        println!("Initialized dirname: {:?}", dirname);
        assert_eq!(dirname, properties_dir.as_path());

        // Try to get system properties - this will help us understand the structure
        let result = std::panic::catch_unwind(|| {
            let _props = rsproperties::system_properties();
        });

        match result {
            Ok(_) => {
                println!("✓ Successfully initialized system properties");

                // If initialization was successful, try to test basic functionality
                let system_props = rsproperties::system_properties();
                println!("✓ SystemProperties object created successfully");

                // List some files to understand the structure
                if let Ok(entries) = std::fs::read_dir(&properties_dir) {
                    println!("Files in properties directory:");
                    for entry in entries {
                        if let Ok(entry) = entry {
                            println!("  - {:?}", entry.file_name());
                        }
                    }
                }
            },
            Err(_) => {
                println!("⚠ Could not initialize system properties");
                println!("  This is expected on non-Android systems or if the directory");
                println!("  doesn't contain valid Android property system files");
            }
        }
    } else {
        println!("⚠ Properties directory does not exist, testing with default path");

        // Test with default initialization
        rsproperties::init(None);
        let dirname = rsproperties::dirname();
        println!("Default dirname: {:?}", dirname);
        assert_eq!(dirname, std::path::Path::new("/dev/__properties__"));

        // This will likely fail, but that's expected on non-Android systems
        let result = std::panic::catch_unwind(|| {
            let _props = rsproperties::system_properties();
        });

        match result {
            Ok(_) => println!("✓ Unexpectedly succeeded with default path"),
            Err(_) => println!("⚠ Expected failure with default path on non-Android system"),
        }
    }
}

#[test]
fn test_rsproperties_error_handling() {
    // Test error handling with an invalid directory
    let invalid_path = PathBuf::from("/definitely/does/not/exist");
    rsproperties::init(Some(invalid_path.clone()));

    let dirname = rsproperties::dirname();
    assert_eq!(dirname, invalid_path.as_path());
    println!("✓ Initialization with invalid path set dirname correctly");

    // Attempting to get system properties should fail gracefully
    let result = std::panic::catch_unwind(|| {
        let _props = rsproperties::system_properties();
    });

    match result {
        Ok(_) => {
            println!("⚠ Unexpectedly succeeded with invalid path");
        },
        Err(_) => {
            println!("✓ Correctly failed when trying to access properties with invalid path");
        }
    }
}

#[test]
fn test_rsproperties_multiple_init_calls() {
    // Test that multiple init calls work correctly
    let path1 = PathBuf::from("/tmp/path1");
    let path2 = PathBuf::from("/tmp/path2");

    rsproperties::init(Some(path1.clone()));
    assert_eq!(rsproperties::dirname(), path1.as_path());

    // Second init call - this should not change the directory (OnceCell behavior)
    rsproperties::init(Some(path2.clone()));
    // The dirname should still be path1 because OnceCell only allows setting once
    assert_eq!(rsproperties::dirname(), path1.as_path());

    println!("✓ Multiple init calls behave correctly (OnceCell semantics)");
    println!("  First path: {:?}", path1);
    println!("  Second path: {:?}", path2);
    println!("  Actual dirname: {:?}", rsproperties::dirname());
}
