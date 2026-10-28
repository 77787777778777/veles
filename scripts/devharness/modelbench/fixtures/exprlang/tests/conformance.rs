use exprlang::{eval, EvalError, Value};

fn num(s: &str) -> f64 {
    match eval(s) {
        Ok(Value::Num(n)) => n,
        other => panic!("{s:?} => {other:?}, wanted a number"),
    }
}
fn b(s: &str) -> bool {
    match eval(s) {
        Ok(Value::Bool(v)) => v,
        other => panic!("{s:?} => {other:?}, wanted a bool"),
    }
}
fn st(s: &str) -> String {
    match eval(s) {
        Ok(Value::Str(v)) => v,
        other => panic!("{s:?} => {other:?}, wanted a string"),
    }
}
fn err(s: &str) -> EvalError {
    match eval(s) {
        Err(e) => e,
        other => panic!("{s:?} => {other:?}, wanted an error"),
    }
}

#[test] fn integer_literal()        { assert_eq!(num("42"), 42.0); }
#[test] fn decimal_literal()        { assert_eq!(num("2.5"), 2.5); }
#[test] fn addition()               { assert_eq!(num("1 + 2"), 3.0); }
#[test] fn subtraction()            { assert_eq!(num("5 - 8"), -3.0); }
#[test] fn multiplication()         { assert_eq!(num("6 * 7"), 42.0); }
#[test] fn division()               { assert_eq!(num("9 / 2"), 4.5); }
#[test] fn modulo()                 { assert_eq!(num("9 % 4"), 1.0); }
#[test] fn precedence_mul_over_add() { assert_eq!(num("2 + 3 * 4"), 14.0); }
#[test] fn precedence_mod_like_mul() { assert_eq!(num("2 + 9 % 4"), 3.0); }
#[test] fn left_assoc_subtraction() { assert_eq!(num("10 - 3 - 2"), 5.0); }
#[test] fn left_assoc_division()    { assert_eq!(num("100 / 5 / 2"), 10.0); }
#[test] fn parens_override()        { assert_eq!(num("(2 + 3) * 4"), 20.0); }
#[test] fn nested_parens()          { assert_eq!(num("((1 + 2) * (3 + 4))"), 21.0); }
#[test] fn unary_minus()            { assert_eq!(num("-5"), -5.0); }
#[test] fn unary_minus_binds_tight() { assert_eq!(num("-2 * 3"), -6.0); }
#[test] fn double_unary_minus()     { assert_eq!(num("--5"), 5.0); }
#[test] fn unary_minus_on_parens()  { assert_eq!(num("-(2 + 3)"), -5.0); }
#[test] fn whitespace_is_free()     { assert_eq!(num("  1   +\t2  "), 3.0); }

#[test] fn less_than()              { assert!(b("1 < 2")); }
#[test] fn less_equal_boundary()    { assert!(b("2 <= 2")); }
#[test] fn greater_than_false()     { assert!(!b("1 > 2")); }
#[test] fn greater_equal()          { assert!(b("3 >= 2")); }
#[test] fn equality_numbers()       { assert!(b("2 == 2")); }
#[test] fn inequality_numbers()     { assert!(b("2 != 3")); }
#[test] fn comparison_below_arith() { assert!(b("1 + 1 == 2")); }
#[test] fn equality_on_bools()      { assert!(b("(1 < 2) == true")); }

#[test] fn logical_and()            { assert!(b("true && true")); }
#[test] fn logical_or()             { assert!(b("false || true")); }
#[test] fn logical_not()            { assert!(b("!false")); }
#[test] fn and_binds_tighter_than_or() { assert!(b("true || false && false")); }
#[test] fn and_short_circuits()     { assert!(!b("false && (1 / 0 == 0)")); }
#[test] fn or_short_circuits()      { assert!(b("true || (1 / 0 == 0)")); }

#[test] fn string_literal()         { assert_eq!(st("\"hi\""), "hi"); }
#[test] fn string_concat()          { assert_eq!(st("\"a\" + \"b\""), "ab"); }
#[test] fn string_escape_quote()    { assert_eq!(st("\"a\\\"b\""), "a\"b"); }
#[test] fn string_escape_backslash() { assert_eq!(st("\"a\\\\b\""), "a\\b"); }
#[test] fn string_equality()        { assert!(b("\"x\" == \"x\"")); }

#[test] fn ternary_true_branch()    { assert_eq!(num("1 < 2 ? 10 : 20"), 10.0); }
#[test] fn ternary_false_branch()   { assert_eq!(num("1 > 2 ? 10 : 20"), 20.0); }
#[test] fn ternary_right_assoc()    { assert_eq!(num("false ? 1 : true ? 2 : 3"), 2.0); }

#[test] fn builtin_min()            { assert_eq!(num("min(3, 5)"), 3.0); }
#[test] fn builtin_max()            { assert_eq!(num("max(3, 5)"), 5.0); }
#[test] fn builtin_abs()            { assert_eq!(num("abs(-7)"), 7.0); }
#[test] fn builtin_len()            { assert_eq!(num("len(\"hello\")"), 5.0); }
#[test] fn builtin_upper()          { assert_eq!(st("upper(\"abc\")"), "ABC"); }
#[test] fn builtin_nested()         { assert_eq!(num("max(min(1,2), abs(-3))"), 3.0); }
#[test] fn builtin_arg_is_expr()    { assert_eq!(num("min(1 + 5, 2 * 2)"), 4.0); }

#[test] fn err_div_by_zero()        { assert!(err("1 / 0").message.to_lowercase().contains("zero")); }
#[test] fn err_mod_by_zero()        { assert!(err("1 % 0").message.to_lowercase().contains("zero")); }
#[test] fn err_type_mismatch()      { assert!(!err("1 + true").message.is_empty()); }
#[test] fn err_unknown_identifier() { assert!(err("nope").message.to_lowercase().contains("nope")); }
#[test] fn err_unexpected_token()   { assert!(!err("1 +").message.is_empty()); }
#[test] fn err_unclosed_paren()     { assert!(!err("(1 + 2").message.is_empty()); }
#[test] fn err_position_is_reported() { assert_eq!(err("1 + nope").position, 4); }
#[test] fn err_logic_needs_bools()  { assert!(!err("1 && true").message.is_empty()); }
