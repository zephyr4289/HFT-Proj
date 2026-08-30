#[test]
fn test_compile_fail_suite() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
