use textkit::truncate;

#[test]
fn short_strings_are_untouched() {
    assert_eq!(truncate("hello", 8), "hello");
}

#[test]
fn the_result_never_exceeds_n_characters() {
    // n is the budget for the WHOLE result, ellipsis included.
    let out = truncate("hello world", 8);
    assert_eq!(out.chars().count(), 8, "got {out:?}");
}
