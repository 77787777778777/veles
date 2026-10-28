# exprlang

A single-expression evaluator. `eval(src)` returns a `Value` or an
`EvalError` carrying a message and the 0-indexed byte position where the
problem starts.

The test suite in `tests/` is the authority on every detail below.

1.  **Numbers** — integer and decimal literals evaluate to `Value::Num`.
2.  **Arithmetic** — `+ - * / %`, C precedence, left-associative.
3.  **Unary minus**, binding tighter than `*`.
4.  **Parentheses** override precedence.
5.  **Comparison** — `< <= > >= == !=` yield `Value::Bool`.
6.  **Logic** — `&& || !`, short-circuiting, operands must be Bool.
7.  **Strings** — double-quoted, `\"` and `\\` escapes; `+` concatenates
    two strings.
8.  **Ternary** — `cond ? a : b`, right-associative, lowest precedence.
9.  **Builtins** — `min(a,b)`, `max(a,b)`, `abs(x)`, `len(s)`,
    `upper(s)`.
10. **Errors** — division by zero, type mismatch, unknown identifier,
    unexpected token, unexpected end of input. Each reports a position.

Whitespace between tokens is insignificant.
