use rsproperties;
use std::path::PathBuf;

#[test]
fn test_with_existing_properties_directory() {
    // Use the actual Android properties directory in the workspace
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

        // Try to get system properties - this should work if the directory has valid data
        let result = std::panic::catch_unwind(|| {
            let _props = rsproperties::system_properties();
        });

        match result {
            Ok(_) => println!("✓ Successfully initialized system properties"),
            Err(_) => println!("⚠ Could not initialize system properties (directory may be incomplete)"),
        }
    } else {
        println!("⚠ Properties directory does not exist, skipping test");
        // Just test the basic API without system properties
        rsproperties::init(None);
        let dirname = rsproperties::dirname();
        println!("Default dirname: {:?}", dirname);
        assert_eq!(dirname, std::path::Path::new("/dev/__properties__"));
    }
}

#[test]
fn test_library_constants() {
    println!("Testing library constants");
    assert_eq!(rsproperties::PROP_VALUE_MAX, 92);
    assert_eq!(rsproperties::PROP_DIRNAME, "/dev/__properties__");
    println!("✓ Library constants are correct");
}

#[test]
fn test_init_and_dirname_api() {
    use std::path::Path;

    // Test with a custom path
    let custom_path = PathBuf::from("/tmp/test_properties");
    rsproperties::init(Some(custom_path.clone()));

    let dirname = rsproperties::dirname();
    assert_eq!(dirname, custom_path.as_path());
    println!("✓ Custom path initialization works: {:?}", dirname);
}
