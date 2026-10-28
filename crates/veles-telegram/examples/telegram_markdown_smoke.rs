//! Telegram MarkdownV2 durable smoke test — adversarial model output.
//!
//! The formatter must never panic and must preserve content on the
//! inputs model output actually produces: nested formatting, unclosed
//! fences, raw specials, tables, URLs with parens, unicode. A naive
//! MarkdownV2 send on any of these gets rejected by Telegram; these
//! assertions pin the pipeline's structural guarantees.
//!
//! Run: cargo run --example telegram_markdown_smoke

use veles_telegram::markdown::{format_message, strip_mdv2};

fn assert_content_preserved(md: &str, must_contain: &[&str]) {
    let out = format_message(md);
    let plain = strip_mdv2(&out);
    for needle in must_contain {
        assert!(
            plain.contains(needle),
            "content lost: {needle:?}\nformatted: {out}\nstripped: {plain}"
        );
    }
}

fn main() {
    // 1) realistic agent reply: headings + bold + code + table + link.
    // Note: the pipeline converts raw **bold** → MarkdownV2 *bold* and
    // escapes parens inside link URLs (valid MarkdownV2; hermes-parity),
    // so assertions target the FORMATTED shapes, not raw markdown.
    let md = "## Summary\n\nThe build **passed** with `0 warnings`.\n\n| Step | Time |\n|---|---|\n| compile | 12s |\n| test | 4s |\n\nSee [the docs](https://example.com/guide(v2)) for details.";
    let out = format_message(md);
    assert!(out.contains("*Summary*"), "heading bold: {out}");
    assert!(out.contains("*passed*"), "bold: {out}");
    assert!(out.contains("`0 warnings`"), "code: {out}");
    assert!(
        out.contains("*compile*"),
        "table heading (mdv2 bold): {out}"
    );
    assert!(out.contains("• Step: compile"), "table bullets: {out}");
    assert!(out.contains("[the docs]("), "link text preserved: {out}");
    // MarkdownV2 spec: only ) and \ need escaping inside link URLs (the
    // ( stays bare) — hermes does exactly this in _convert_link.
    assert!(out.contains("guide(v2\\)"), "url ) escaped: {out}");
    // and the plain-text round trip keeps everything readable
    let plain = strip_mdv2(&out);
    assert!(
        plain.contains("Summary") && plain.contains("passed"),
        "plain: {plain}"
    );
    println!("1) realistic agent reply ✓");

    // 2) content preservation across the pipeline
    assert_content_preserved(
        "**bold** and *italic* and `code` and [x](http://y.z)",
        &["bold", "italic", "code", "x"],
    );
    println!("2) content preservation ✓");

    // 3) adversarial: unclosed fence must not panic
    let _ = format_message("```rust\nlet x = 1;\n// never closed");
    println!("3) unclosed fence survives ✓");

    // 4) adversarial: raw specials everywhere
    let specials = "price: $5.00 (wow!) [1] {2} | a > b & c < d #tag";
    let out = format_message(specials);
    let plain = strip_mdv2(&out);
    assert!(plain.contains("$5.00"), "specials: {plain}");
    println!("4) raw specials escaped ✓");

    // 5) adversarial: nested bold inside header inside list
    let nested = "- item with **bold** and *both* `mix`\n## **Bold heading**";
    let _ = format_message(nested);
    println!("5) nested constructs survive ✓");

    // 6) tables left alone inside fences
    let fenced = "```\n| a | b |\n|---|---|\n| 1 | 2 |\n```";
    let out = format_message(fenced);
    assert!(out.contains("| a | b |"), "fenced table untouched: {out}");
    println!("6) fenced tables untouched ✓");

    // 7) blockquote conversion
    let out = format_message("> thinking line one\n> line two");
    assert!(out.contains("> thinking line one"), "quote: {out}");
    println!("7) blockquotes ✓");

    // 8) empty + whitespace (whitespace chars aren't MarkdownV2 specials)
    assert_eq!(format_message(""), "");
    assert_eq!(format_message("   "), "   ");
    println!("8) empty/whitespace ✓");

    // 9) strikethrough + spoiler
    let out = format_message("~~gone~~ and ||secret||");
    assert!(out.contains("~gone~"), "strike: {out}");
    assert!(out.contains("||secret||"), "spoiler: {out}");
    println!("9) strikethrough + spoiler ✓");

    println!("\nTELEGRAM MARKDOWN SMOKE PASSED");
}
