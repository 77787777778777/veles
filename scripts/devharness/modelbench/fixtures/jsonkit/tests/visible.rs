use jsonkit::*;
use std::collections::BTreeMap;

fn p(s: &str) -> Json { parse(s).expect(s) }
fn obj(pairs: &[(&str, Json)]) -> Json {
    Json::Obj(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<BTreeMap<_, _>>())
}
fn s(x: &str) -> Json { Json::Str(x.into()) }
fn n(x: f64) -> Json { Json::Num(x) }

// ── parse ──
#[test] fn parse_null()      { assert_eq!(p("null"), Json::Null); }
#[test] fn parse_true()      { assert_eq!(p("true"), Json::Bool(true)); }
#[test] fn parse_int()       { assert_eq!(p("42"), n(42.0)); }
#[test] fn parse_negative()  { assert_eq!(p("-7"), n(-7.0)); }
#[test] fn parse_frac()      { assert_eq!(p("2.5"), n(2.5)); }
#[test] fn parse_exp()       { assert_eq!(p("1e3"), n(1000.0)); }
#[test] fn parse_string()    { assert_eq!(p("\"hi\""), s("hi")); }
#[test] fn parse_escapes()   { assert_eq!(p(r#""a\nb""#), s("a\nb")); }
#[test] fn parse_unicode()   { assert_eq!(p(r#""A""#), s("A")); }
#[test] fn parse_empty_arr() { assert_eq!(p("[]"), Json::Arr(vec![])); }
#[test] fn parse_arr()       { assert_eq!(p("[1,2]"), Json::Arr(vec![n(1.0), n(2.0)])); }
#[test] fn parse_nested()    { assert_eq!(p("[[1]]"), Json::Arr(vec![Json::Arr(vec![n(1.0)])])); }
#[test] fn parse_obj()       { assert_eq!(p(r#"{"a":1}"#), obj(&[("a", n(1.0))])); }
#[test] fn parse_ws()        { assert_eq!(p(" { \"a\" : 1 } "), obj(&[("a", n(1.0))])); }
#[test] fn parse_err_trailing_comma() { assert!(parse("[1,]").is_err()); }
#[test] fn parse_err_bare_word()      { assert!(parse("nope").is_err()); }
#[test] fn parse_err_unclosed()       { assert!(parse("[1").is_err()); }
#[test] fn parse_err_position()       { assert_eq!(parse("[1,]").unwrap_err().position, 3); }

// ── serialize ──
#[test] fn ser_null()   { assert_eq!(serialize(&Json::Null), "null"); }
#[test] fn ser_int()    { assert_eq!(serialize(&n(42.0)), "42"); }
#[test] fn ser_frac()   { assert_eq!(serialize(&n(2.5)), "2.5"); }
#[test] fn ser_str()    { assert_eq!(serialize(&s("hi")), "\"hi\""); }
#[test] fn ser_escape() { assert_eq!(serialize(&s("a\nb")), r#""a\nb""#); }
#[test] fn ser_arr()    { assert_eq!(serialize(&Json::Arr(vec![n(1.0), n(2.0)])), "[1,2]"); }
#[test] fn ser_obj()    { assert_eq!(serialize(&obj(&[("a", n(1.0))])), r#"{"a":1}"#); }
#[test] fn ser_sorted() { assert_eq!(serialize(&obj(&[("b", n(2.0)), ("a", n(1.0))])), r#"{"a":1,"b":2}"#); }
#[test] fn roundtrip()  { let t = r#"{"a":[1,{"b":null}],"c":"x"}"#; assert_eq!(serialize(&p(t)), t); }

// ── pointer ──
#[test] fn ptr_whole()   { let d = p(r#"{"a":1}"#); assert_eq!(pointer(&d, ""), Some(&d)); }
#[test] fn ptr_key()     { let d = p(r#"{"a":1}"#); assert_eq!(pointer(&d, "/a"), Some(&n(1.0))); }
#[test] fn ptr_index()   { let d = p("[9,8]"); assert_eq!(pointer(&d, "/1"), Some(&n(8.0))); }
#[test] fn ptr_deep()    { let d = p(r#"{"a":[{"b":3}]}"#); assert_eq!(pointer(&d, "/a/0/b"), Some(&n(3.0))); }
#[test] fn ptr_missing() { let d = p(r#"{"a":1}"#); assert_eq!(pointer(&d, "/z"), None); }
#[test] fn ptr_oob()     { let d = p("[1]"); assert_eq!(pointer(&d, "/5"), None); }

// ── patch ──
#[test] fn patch_add()     { assert_eq!(patch(&p("{}"), &p(r#"[{"op":"add","path":"/a","value":1}]"#)).unwrap(), obj(&[("a", n(1.0))])); }
#[test] fn patch_remove()  { assert_eq!(patch(&p(r#"{"a":1}"#), &p(r#"[{"op":"remove","path":"/a"}]"#)).unwrap(), p("{}")); }
#[test] fn patch_replace() { assert_eq!(patch(&p(r#"{"a":1}"#), &p(r#"[{"op":"replace","path":"/a","value":2}]"#)).unwrap(), obj(&[("a", n(2.0))])); }
#[test] fn patch_append()  { assert_eq!(patch(&p("[1]"), &p(r#"[{"op":"add","path":"/-","value":2}]"#)).unwrap(), p("[1,2]")); }
#[test] fn patch_test_ok() { assert!(patch(&p(r#"{"a":1}"#), &p(r#"[{"op":"test","path":"/a","value":1}]"#)).is_ok()); }
#[test] fn patch_test_bad() { assert!(patch(&p(r#"{"a":1}"#), &p(r#"[{"op":"test","path":"/a","value":2}]"#)).is_err()); }
#[test] fn patch_remove_missing() { assert!(patch(&p("{}"), &p(r#"[{"op":"remove","path":"/a"}]"#)).is_err()); }
#[test] fn patch_atomic()  { let d = p(r#"{"a":1}"#); assert!(patch(&d, &p(r#"[{"op":"add","path":"/b","value":2},{"op":"remove","path":"/zz"}]"#)).is_err()); }

// ── merge_patch ──
#[test] fn merge_set()    { assert_eq!(merge_patch(&p("{}"), &p(r#"{"a":1}"#)), obj(&[("a", n(1.0))])); }
#[test] fn merge_delete() { assert_eq!(merge_patch(&p(r#"{"a":1,"b":2}"#), &p(r#"{"a":null}"#)), obj(&[("b", n(2.0))])); }
#[test] fn merge_nested() { assert_eq!(merge_patch(&p(r#"{"a":{"x":1}}"#), &p(r#"{"a":{"y":2}}"#)), p(r#"{"a":{"x":1,"y":2}}"#)); }
#[test] fn merge_scalar() { assert_eq!(merge_patch(&p(r#"{"a":1}"#), &p("5")), n(5.0)); }
