// NEVER placed in the work tree until grading. These are spec corners
// the visible suite does not reach: an implementation that chased the
// visible tests fails here, one that actually read SPEC.md passes.
use jsonkit::*;
use std::collections::BTreeMap;

fn p(s: &str) -> Json {
    parse(s).expect(s)
}
fn obj(pairs: &[(&str, Json)]) -> Json {
    Json::Obj(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}
fn s(x: &str) -> Json {
    Json::Str(x.into())
}
fn n(x: f64) -> Json {
    Json::Num(x)
}

// ── parse corners ──
#[test] fn h_surrogate_pair()      { assert_eq!(p(r#""😀""#), s("\u{1F600}")); }
#[test] fn h_escape_solidus()      { assert_eq!(p(r#""a\/b""#), s("a/b")); }
#[test] fn h_escape_all_shorts()   { assert_eq!(p(r#""\b\f\r\t""#), s("\u{8}\u{c}\r\t")); }
#[test] fn h_exp_capital_signed()  { assert_eq!(p("-2.5E-2"), n(-0.025)); }
#[test] fn h_exp_plus()            { assert_eq!(p("1e+2"), n(100.0)); }
#[test] fn h_zero()                { assert_eq!(p("0"), n(0.0)); }
#[test] fn h_reject_leading_zero()  { assert!(parse("01").is_err()); }
#[test] fn h_reject_bare_frac()     { assert!(parse(".5").is_err()); }
#[test] fn h_reject_trailing_dot()  { assert!(parse("5.").is_err()); }
#[test] fn h_reject_nan()           { assert!(parse("NaN").is_err()); }
#[test] fn h_reject_infinity()      { assert!(parse("Infinity").is_err()); }
#[test] fn h_reject_single_quote()  { assert!(parse("'a'").is_err()); }
#[test] fn h_reject_comment()       { assert!(parse("1 // x").is_err()); }
#[test] fn h_reject_trailing()      { assert!(parse("1 2").is_err()); }
#[test] fn h_duplicate_key_last_wins() { assert_eq!(p(r#"{"a":1,"a":2}"#), obj(&[("a", n(2.0))])); }

// ── serialize corners ──
#[test] fn h_ser_control_char()     { assert_eq!(serialize(&s("\u{1}")), r#""\u0001""#); }
#[test] fn h_ser_newline_short()    { assert_eq!(serialize(&s("\n")), r#""\n""#); }
#[test] fn h_ser_quote_backslash()  { assert_eq!(serialize(&s("a\"b\\c")), r#""a\"b\\c""#); }
#[test] fn h_ser_non_ascii_literal() { assert_eq!(serialize(&s("é")), "\"é\""); }
#[test] fn h_ser_large_int()        { assert_eq!(serialize(&n(1e15)), "1000000000000000"); }

// ── pointer corners ──
#[test] fn h_ptr_escape_slash()     { let d = p(r#"{"a/b":1}"#); assert_eq!(pointer(&d, "/a~1b"), Some(&n(1.0))); }
#[test] fn h_ptr_escape_tilde()     { let d = p(r#"{"a~b":1}"#); assert_eq!(pointer(&d, "/a~0b"), Some(&n(1.0))); }
#[test] fn h_ptr_escape_order()     { let d = p(r#"{"a~1b":1}"#); assert_eq!(pointer(&d, "/a~01b"), Some(&n(1.0))); }
#[test] fn h_ptr_empty_key()        { let d = p(r#"{"":1}"#); assert_eq!(pointer(&d, "/"), Some(&n(1.0))); }
#[test] fn h_ptr_dash_not_index()   { let d = p("[1]"); assert_eq!(pointer(&d, "/-"), None); }
#[test] fn h_ptr_leading_zero_index() { let d = p("[1,2]"); assert_eq!(pointer(&d, "/01"), None); }

// ── patch corners ──
#[test] fn h_patch_move() { assert_eq!(patch(&p(r#"{"a":1}"#), &p(r#"[{"op":"move","from":"/a","path":"/b"}]"#)).unwrap(), obj(&[("b", n(1.0))])); }
#[test] fn h_patch_copy() { assert_eq!(patch(&p(r#"{"a":1}"#), &p(r#"[{"op":"copy","from":"/a","path":"/b"}]"#)).unwrap(), p(r#"{"a":1,"b":1}"#)); }
#[test] fn h_patch_add_replaces_key() { assert_eq!(patch(&p(r#"{"a":1}"#), &p(r#"[{"op":"add","path":"/a","value":9}]"#)).unwrap(), obj(&[("a", n(9.0))])); }
#[test] fn h_patch_insert_middle() { assert_eq!(patch(&p("[1,3]"), &p(r#"[{"op":"add","path":"/1","value":2}]"#)).unwrap(), p("[1,2,3]")); }
#[test] fn h_patch_move_into_child_fails() { assert!(patch(&p(r#"{"a":{"b":1}}"#), &p(r#"[{"op":"move","from":"/a","path":"/a/b/c"}]"#)).is_err()); }
#[test] fn h_patch_test_deep_equality() { assert!(patch(&p(r#"{"a":[1,{"b":2}]}"#), &p(r#"[{"op":"test","path":"/a","value":[1,{"b":2}]}]"#)).is_ok()); }

// ── merge_patch corners ──
#[test] fn h_merge_nested_delete() { assert_eq!(merge_patch(&p(r#"{"a":{"x":1,"y":2}}"#), &p(r#"{"a":{"x":null}}"#)), p(r#"{"a":{"y":2}}"#)); }
#[test] fn h_merge_array_replaces() { assert_eq!(merge_patch(&p(r#"{"a":[1,2]}"#), &p(r#"{"a":[3]}"#)), p(r#"{"a":[3]}"#)); }
#[test] fn h_merge_null_on_missing() { assert_eq!(merge_patch(&p("{}"), &p(r#"{"a":null}"#)), p("{}")); }
#[test] fn h_merge_replaces_scalar_with_obj() { assert_eq!(merge_patch(&p(r#"{"a":1}"#), &p(r#"{"a":{"b":2}}"#)), p(r#"{"a":{"b":2}}"#)); }
