//! jsonkit — see SPEC.md. Nothing is implemented yet.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsonError {
    pub message: String,
    pub position: usize,
}

pub fn parse(_src: &str) -> Result<Json, JsonError> { todo!() }
pub fn serialize(_v: &Json) -> String { todo!() }
pub fn pointer<'a>(_v: &'a Json, _ptr: &str) -> Option<&'a Json> { todo!() }
pub fn patch(_doc: &Json, _ops: &Json) -> Result<Json, JsonError> { todo!() }
pub fn merge_patch(_doc: &Json, _patch: &Json) -> Json { todo!() }
