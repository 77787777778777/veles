//! Skills system smoke — no tokens spent.
//! 1. discovers the real ~/.hermes/skills library (migration root)
//! 2. skill_view returns body + linked files; near-miss suggestions work
//! 3. skill_manage create/patch/delete round-trip in the veles skills root
//! 4. read-only guard: patch/delete refuse hermes-imported skills
//! 5. path-traversal guard on linked files

fn main() {
    use veles_core::skills::{view_skill, SkillSet};

    let set = SkillSet::load();
    println!("1) discovered {} skills", set.skills.len());
    assert!(set.skills.len() > 50, "expected the real hermes library");

    // pick a real skill for the view test
    let probe = set
        .skills
        .iter()
        .find(|s| s.name.contains("computer-use"))
        .unwrap_or(&set.skills[0]);
    println!(
        "   probing skill: {} ({}, {} root)",
        probe.name, probe.category, probe.root
    );
    let body = view_skill(&set, &probe.name, None, None, None);
    assert!(body.len() > 100, "skill body too short");
    assert!(body.contains("---"), "missing frontmatter/body");
    println!("2) skill_view: {} chars ✓", body.len());

    // near-miss suggestion
    let partial: String = probe
        .name
        .chars()
        .take(probe.name.len().saturating_sub(3))
        .collect();
    let miss = view_skill(&set, &partial, None, None, None);
    assert!(miss.contains("not found"), "should report not-found");
    println!("   near-miss: {}", miss.lines().next().unwrap());

    // 3. manage round-trip in the veles root
    let rt = tokio::runtime::Runtime::new().unwrap();
    let create = rt.block_on(veles_core::tools::Tool::call(
        &veles_core::skills::SkillManageTool,
        serde_json::json!({
            "action": "create",
            "name": "veles-smoke-skill",
            "category": "testing",
            "content": "---\nname: veles-smoke-skill\ndescription: smoke test skill for the veles skills system\n---\n# Smoke\nStep one: nothing to do.\n"
        }),
    ));
    println!("3) create: {create}");
    assert!(create.contains("created"), "create failed: {create}");

    let set2 = SkillSet::load();
    assert!(
        set2.get("veles-smoke-skill").is_some(),
        "created skill not discovered"
    );
    assert!(set2.get("veles-smoke-skill").unwrap().root == "veles");

    let patch = rt.block_on(veles_core::tools::Tool::call(
        &veles_core::skills::SkillManageTool,
        serde_json::json!({
            "action": "patch",
            "name": "veles-smoke-skill",
            "old_string": "Step one: nothing to do.",
            "new_string": "Step one: verified working."
        }),
    ));
    println!("   patch: {patch}");
    assert!(patch.contains("patched"), "patch failed: {patch}");
    let body2 = view_skill(&set2, "veles-smoke-skill", None, None, None);
    assert!(body2.contains("verified working"));

    // 4. read-only guard on hermes root
    let guard = rt.block_on(veles_core::tools::Tool::call(
        &veles_core::skills::SkillManageTool,
        serde_json::json!({"action": "delete", "name": probe.name}),
    ));
    println!("4) delete-hermes guard: {guard}");
    assert!(guard.contains("read-only"), "guard failed: {guard}");

    // 5. traversal guard
    let trav = view_skill(
        &set2,
        "veles-smoke-skill",
        Some("../../../../etc/passwd"),
        None,
        None,
    );
    println!("5) traversal guard: {}", trav.lines().next().unwrap_or(""));
    assert!(!trav.contains("root:"), "traversal leaked");

    // cleanup the test skill
    let del = rt.block_on(veles_core::tools::Tool::call(
        &veles_core::skills::SkillManageTool,
        serde_json::json!({"action": "delete", "name": "veles-smoke-skill"}),
    ));
    assert!(del.contains("deleted"), "cleanup failed: {del}");

    println!("\nSKILLS SMOKE PASSED");
}
