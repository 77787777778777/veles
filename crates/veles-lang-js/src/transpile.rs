//! TypeScript in, JavaScript out: oxc parses, strips the types (and
//! lowers what QuickJS has no notion of — enums, namespaces, parameter
//! properties) and prints the result. Plain JavaScript goes through
//! untouched: QuickJS parses it itself, and a second parser would only
//! add a second opinion on what is valid.
//!
//! Only the TypeScript transform runs (`TransformOptions::default()`):
//! no syntax is lowered to an older ECMAScript, because QuickJS-ng
//! speaks the current one.

use std::path::Path;

use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};

/// Whether `path` names a file the transpiler must see first.
pub fn is_typescript(path: &Path) -> bool {
    SourceType::from_path(path)
        .map(|t| t.is_typescript())
        .unwrap_or(false)
}

/// The JavaScript to hand the engine for `source` at `path`. A `.ts`,
/// `.mts` or `.tsx` file is transformed; anything else is returned as
/// it came. Errors name the file and every diagnostic oxc raised.
pub fn to_js(path: &Path, source: &str) -> Result<String, String> {
    let Ok(source_type) = SourceType::from_path(path) else {
        return Ok(source.to_string());
    };
    if !source_type.is_typescript() {
        return Ok(source.to_string());
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.diagnostics.has_errors() {
        return Err(format!(
            "{}: {}",
            path.display(),
            join_diagnostics(parsed.diagnostics.errors())
        ));
    }
    let mut program = parsed.program;
    let semantic = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        // The enum transform reads its evaluated member values from
        // the scoping (oxc asserts this flag).
        .with_enum_eval(true)
        .build(&program);
    if semantic.diagnostics.has_errors() {
        return Err(format!(
            "{}: {}",
            path.display(),
            join_diagnostics(semantic.diagnostics.errors())
        ));
    }
    let scoping = semantic.semantic.into_scoping();
    let options = TransformOptions::default();
    let transformed =
        Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
    if transformed.diagnostics.has_errors() {
        return Err(format!(
            "{}: {}",
            path.display(),
            join_diagnostics(transformed.diagnostics.errors())
        ));
    }
    Ok(Codegen::new().build(&program).code)
}

fn join_diagnostics<'a>(it: impl Iterator<Item = &'a oxc::diagnostics::OxcDiagnostic>) -> String {
    it.map(|d| {
        d.to_string()
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    })
    .collect::<Vec<_>>()
    .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_types_are_stripped_and_enums_lowered() {
        let src = "enum Lane { A = 1, B = 2 }\ninterface Spec { name: string }\nexport function f(x: number): Spec { return { name: `n${x + Lane.B}` }; }\n";
        let out = to_js(Path::new("plugin.ts"), src).unwrap();
        assert!(!out.contains("interface"), "{out}");
        assert!(!out.contains(": number"), "{out}");
        assert!(
            out.contains("Lane"),
            "the enum became a runtime object: {out}"
        );
        assert!(out.contains("export function f(x)"), "{out}");
    }

    #[test]
    fn javascript_passes_through_untouched() {
        let src = "const x = 1;\n";
        assert_eq!(to_js(Path::new("plugin.js"), src).unwrap(), src);
        assert_eq!(to_js(Path::new("plugin.mjs"), src).unwrap(), src);
        assert!(!is_typescript(Path::new("plugin.js")));
        assert!(is_typescript(Path::new("plugin.ts")));
        assert!(is_typescript(Path::new("plugin.mts")));
    }

    #[test]
    fn a_syntax_error_names_the_file() {
        let e = to_js(Path::new("broken.ts"), "let x: number = ;").unwrap_err();
        assert!(e.starts_with("broken.ts:"), "{e}");
    }
}
