//! exprlang — see SPEC.md. Nothing is implemented yet.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Num(f64),
    Bool(bool),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalError {
    pub message: String,
    pub position: usize,
}

pub fn eval(_src: &str) -> Result<Value, EvalError> {
    todo!("implement per SPEC.md")
}
