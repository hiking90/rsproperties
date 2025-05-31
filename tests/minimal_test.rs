#[test]
fn simple_test() {
    assert_eq!(2 + 2, 4);
    println!("This is a simple test");
}

#[test]
fn test_constants() {
    assert_eq!(rsproperties::PROP_VALUE_MAX, 92);
    println!("Constants test passed");
}
