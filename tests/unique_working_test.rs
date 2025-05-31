#[test]
fn unique_test_constants_check() {
    assert_eq!(rsproperties::PROP_VALUE_MAX, 92);
    assert_eq!(rsproperties::PROP_DIRNAME, "/dev/__properties__");
    println!("✓ Library constants are correct");
}

#[test]
fn unique_test_init_functionality() {
    use std::path::PathBuf;
    let custom_path = PathBuf::from("/tmp/unique_test_properties");
    rsproperties::init(Some(custom_path.clone()));

    let dirname = rsproperties::dirname();
    assert_eq!(dirname, custom_path.as_path());
    println!("✓ Init functionality works: {:?}", dirname);
}
