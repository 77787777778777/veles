//! IRC hostmask patterns for ACL matching.
//!
//! USER (2026-08-30): "irc should use full nick!ident@hostname masks …
//! with wildcards/regexes supported in each segment".
//!
//! A pattern is `nick!user@host`. Each of the three segments matches
//! independently and case-insensitively:
//!
//! - **glob** (the default): `*` is any run of characters, `?` exactly
//!   one — the eggdrop/weechat hostmask spelling (`*!*@gateway/wiki`);
//! - **regex**: a segment written `re:<expr>` is compiled as a
//!   case-insensitive regular expression anchored to the whole segment.
//!
//! Missing segments default to `*` — `*@gateway/foo` (no nick given)
//! means "any nick", matching what people type when they only care
//! about the host. The observed mask comes off the wire prefix
//! (`nick!user@host`), so it always has all three parts.
//!
//! A nick is choosable by the client, an ident half so (`~` marks an
//! unverified one), a host or cloak is assigned by the server. That
//! ordering is not a reason to distrust masks — it is the ordering the
//! ACL SORTS BY: `MaskSpecificity` compares host, then ident, then
//! nick, so `*!ident@*` outranks `vic!*@*` and pinning the host
//! outranks both. Pin `ident@host`, not a bare nick; a nick-only mask
//! stays legal (IRC semantics) and stays spoofable, and `/acl explain`
//! says which rule decided.

use regex::Regex;

/// One segment of a mask pattern.
#[derive(Debug, Clone)]
enum Segment {
    /// Compiled from a glob (`*`/`?`) or a literal. Anchored,
    /// case-insensitive.
    Glob(Regex),
    /// `re:` prefix: user-supplied regex, anchored, case-insensitive.
    Regex(Regex),
}

impl Segment {
    fn compile_glob(raw: &str) -> Result<Regex, String> {
        let mut out = String::from("(?i)^");
        for ch in raw.chars() {
            match ch {
                '*' => out.push_str(".*"),
                '?' => out.push('.'),
                // Everything else matches literally; regex::escape on
                // the single char keeps the engine from reading it as
                // syntax (`.` in hostnames is the everyday case).
                c => out.push_str(&regex::escape(&c.to_string())),
            }
        }
        out.push('$');
        Regex::new(&out).map_err(|e| format!("internal glob compile failed: {e}"))
    }

    fn parse(raw: &str) -> Result<Segment, String> {
        if raw.trim().is_empty() {
            // An omitted segment means "any".
            return Ok(Segment::Glob(Regex::new("(?i)^.*$").expect("static")));
        }
        if let Some(expr) = raw.strip_prefix("re:") {
            let anchored = format!("(?i)^(?:{expr})$");
            return Regex::new(&anchored)
                .map(Segment::Regex)
                .map_err(|e| format!("bad regex in segment `{raw}`: {e}"));
        }
        Ok(Segment::Glob(Self::compile_glob(raw)?))
    }

    fn matches(&self, value: &str) -> bool {
        match self {
            Segment::Glob(r) | Segment::Regex(r) => r.is_match(value),
        }
    }
}

/// How much text a mask actually pins down, per segment.
///
/// The ACL honours the MOST SPECIFIC matching rule regardless of where
/// it was declared (USER, 2026-09-09: *"If I add everyone with
/// `*!*@*network-default-masking` as a user … but `me!me@1234-…` is the
/// owner and `jimmy!asshole@444-…` is banned … I should get
/// admin/owner rights, jimmy should get his dunce cap and everyone else
/// user"*). That needs a total order on patterns, and this is it.
///
/// Compared field by field in DECLARATION ORDER — host, then ident,
/// then nick — because that is the order of how hard each segment is to
/// choose. A host (or a cloak) is assigned by the server; an ident is
/// half-chosen (`~` marks an unverified one); a nick is free. So
/// `*!ident@*` outranks `vic!*@*`, and pinning the host outranks both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaskSpecificity {
    pub host: u32,
    pub ident: u32,
    pub nick: u32,
}

/// Regex metacharacters, for counting how much of a `re:` segment is
/// literal text rather than syntax.
const RE_META: &[char] = &[
    '^', '$', '.', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\', '/', '-', ',',
];

/// Score one raw segment.
///
/// Three tiers, so the comparison is about kind first and length
/// second:
///
/// - 0 — a pure wildcard (`*`, `**`, or omitted): pins nothing.
/// - 100 + literal chars — a glob or a regex: pins some text, and a `?`
///   or an anchor still pins a shape, which is why the tier floor is
///   reached even at zero literal characters.
/// - 1000 + length — a plain literal, no wildcards at all: pins
///   everything. An exact host beats every glob on the same segment,
///   however long.
///
/// A `re:` segment never reaches the literal tier: a regex that happens
/// to match one string is not provably exact, and the ACL does not
/// guess.
fn segment_specificity(raw: &str) -> u32 {
    let raw = raw.trim();
    if raw.is_empty() {
        return 0;
    }
    if let Some(expr) = raw.strip_prefix("re:") {
        let literals = expr.chars().filter(|c| !RE_META.contains(c)).count() as u32;
        return 100 + literals;
    }
    if raw.chars().all(|c| c == '*') {
        return 0;
    }
    if raw.contains('*') || raw.contains('?') {
        let literals = raw.chars().filter(|c| *c != '*' && *c != '?').count() as u32;
        return 100 + literals;
    }
    1000 + raw.chars().count() as u32
}

/// A parsed `nick!user@host` pattern.
#[derive(Debug, Clone)]
pub struct MaskPattern {
    nick: Segment,
    user: Segment,
    host: Segment,
    spec: MaskSpecificity,
}

/// Does this identity string describe a mask pattern (rather than an
/// exact `platform:principal` identity)? Exact identities never carry
/// `!` or `@` in veles' `irc:<account>` / `telegram:<id>` shapes.
pub fn looks_like_mask(s: &str) -> bool {
    s.contains('!') || s.contains('@')
}

impl MaskPattern {
    /// Parse `nick!user@host` with either delimiter optional (a missing
    /// segment becomes `*`). An empty pattern matches nothing — a grant
    /// has to name SOMETHING.
    pub fn parse(pattern: &str) -> Result<MaskPattern, String> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err("empty mask pattern".into());
        }
        let (nick, rest) = match pattern.split_once('!') {
            Some((n, rest)) => (n, rest),
            None => ("*", pattern),
        };
        let (user, host) = match rest.split_once('@') {
            Some((u, h)) => (u, h),
            None => (rest, "*"),
        };
        Ok(MaskPattern {
            nick: Segment::parse(nick)?,
            user: Segment::parse(user)?,
            host: Segment::parse(host)?,
            spec: MaskSpecificity {
                host: segment_specificity(host),
                ident: segment_specificity(user),
                nick: segment_specificity(nick),
            },
        })
    }

    /// How much this pattern pins down — the ACL's precedence key.
    pub fn specificity(&self) -> MaskSpecificity {
        self.spec
    }

    /// Match against an observed `nick!user@host` from the wire.
    pub fn matches(&self, observed: &str) -> bool {
        let (nick, user, host) = split(observed);
        self.nick.matches(&nick) && self.user.matches(&user) && self.host.matches(&host)
    }
}

/// Split an observed mask into its three parts. Tolerant of missing
/// delimiters (a line with no `@` yields an empty host) rather than
/// failing — matching a partial observation against a `*` segment must
/// stay possible.
pub fn split(observed: &str) -> (String, String, String) {
    let (nick, rest) = match observed.split_once('!') {
        Some((n, rest)) => (n.to_string(), rest),
        None => return (observed.to_string(), String::new(), String::new()),
    };
    match rest.split_once('@') {
        Some((u, h)) => (nick, u.to_string(), h.to_string()),
        None => (nick, rest.to_string(), String::new()),
    }
}

/// How two patterns' CALLER SETS relate.
///
/// `specificity` orders two rules that already matched somebody. Before
/// a rule can be called redundant, something has to say whether two
/// rules can ever match the SAME caller, and which of them reaches more
/// of them — a question about two pattern texts with no caller in sight.
/// It is only worth answering when the texts prove it: a glob against a
/// glob is `Unknown`, not a guess, because "nobody can act on a probably"
/// and an operator who is told one rule covers another will delete one
/// of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternRelation {
    /// The two match exactly the same callers.
    Equal,
    /// Everything the first matches, the second matches too.
    AWithinB,
    /// Everything the second matches, the first matches too.
    BWithinA,
    /// Each reaches callers the other does not, and some callers match
    /// both — so the one that loses the precedence comparison still
    /// decides something, and neither is a duplicate of the other.
    Overlap,
    /// No caller can match both.
    Disjoint,
    /// Not provable from the two texts. Said instead of guessed.
    Unknown,
}

/// Compare two `nick!user@host` pattern texts, segment by segment.
///
/// Both sides are read the way [`MaskPattern::parse`] reads them, so an
/// omitted segment is `*` here too and the answer is about the same
/// callers matching would be about.
pub fn pattern_relation(a: &str, b: &str) -> PatternRelation {
    let (an, au, ah) = raw_segments(a);
    let (bn, bu, bh) = raw_segments(b);
    combine_relations(&[
        segment_relation(&an, &bn),
        segment_relation(&au, &bu),
        segment_relation(&ah, &bh),
    ])
}

/// Fold per-part relations into one relation over the whole caller set.
///
/// Public because the ACL combines a pattern's three segments AND a
/// rule's three scope fields, and there should be one definition of how
/// containment composes rather than two that disagree.
pub fn combine_relations(rels: &[PatternRelation]) -> PatternRelation {
    use PatternRelation::*;
    // One part no caller can satisfy means no caller can satisfy the
    // whole, however the other parts line up.
    if rels.contains(&Disjoint) {
        return Disjoint;
    }
    if rels.contains(&Unknown) {
        return Unknown;
    }
    let a_in_b = rels.iter().all(|r| matches!(r, Equal | AWithinB));
    let b_in_a = rels.iter().all(|r| matches!(r, Equal | BWithinA));
    match (a_in_b, b_in_a) {
        (true, true) => Equal,
        (true, false) => AWithinB,
        (false, true) => BWithinA,
        // One part narrower each way: a caller pinned inside both
        // exists, and so does one only each side reaches.
        (false, false) => Overlap,
    }
}

/// The three raw segments of a pattern text, with `parse`'s defaults.
fn raw_segments(pattern: &str) -> (String, String, String) {
    let pattern = pattern.trim();
    let (nick, rest) = match pattern.split_once('!') {
        Some((n, rest)) => (n.to_string(), rest.to_string()),
        None => ("*".to_string(), pattern.to_string()),
    };
    let (user, host) = match rest.split_once('@') {
        Some((u, h)) => (u.to_string(), h.to_string()),
        None => (rest, "*".to_string()),
    };
    (nick, user, host)
}

fn is_pure_wild(raw: &str) -> bool {
    let raw = raw.trim();
    raw.is_empty() || raw.chars().all(|c| c == '*')
}

/// Pins text with no wildcard in it — one caller, and only that one.
fn is_literal(raw: &str) -> bool {
    let raw = raw.trim();
    !raw.is_empty() && !raw.contains('*') && !raw.contains('?') && !raw.starts_with("re:")
}

/// One segment against one segment.
fn segment_relation(a: &str, b: &str) -> PatternRelation {
    use PatternRelation::*;
    let (a, b) = (a.trim(), b.trim());
    if is_pure_wild(a) && is_pure_wild(b) {
        return Equal;
    }
    // A pure wild reaches every value of the segment, so the other side
    // is inside it by construction.
    if is_pure_wild(b) {
        return AWithinB;
    }
    if is_pure_wild(a) {
        return BWithinA;
    }
    if a.eq_ignore_ascii_case(b) {
        return Equal;
    }
    // One side pins text and the other does not: the wider side's own
    // matcher answers whether it covers the pinned one. Sound in both
    // directions — a literal stands for exactly one value — and it is
    // the everyday case (`octo` inside `o*`, `gateway/veles` inside
    // `gateway/*`). A `re:` side is matched the same way when the OTHER
    // side is the literal.
    if is_literal(a) {
        return match Segment::parse(b) {
            Ok(seg) if seg.matches(a) => AWithinB,
            Ok(_) => Disjoint,
            Err(_) => Unknown,
        };
    }
    if is_literal(b) {
        return match Segment::parse(a) {
            Ok(seg) if seg.matches(b) => BWithinA,
            Ok(_) => Disjoint,
            Err(_) => Unknown,
        };
    }
    // Two globs, or a `re:` against a pattern: whether one language
    // covers another is not a question two texts answer.
    Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str) -> MaskPattern {
        MaskPattern::parse(p).expect("pattern must parse")
    }

    #[test]
    fn exact_mask_matches_itself() {
        let p = m("wrath!~w@gateway/veles");
        assert!(p.matches("wrath!~w@gateway/veles"));
        assert!(!p.matches("nyx!~w@gateway/veles"));
        assert!(!p.matches("wrath!~w@other.host"));
    }

    #[test]
    fn segments_are_case_insensitive() {
        let p = m("Wrath!~W@Gateway/Veles");
        assert!(p.matches("wrath!~w@gateway/veles"));
        assert!(p.matches("WRATH!~W@GATEWAY/VELES"));
    }

    #[test]
    fn globs_in_each_segment() {
        assert!(m("*!*@gateway/*").matches("anyone!~a@gateway/wiki"));
        assert!(m("w*!*@*").matches("wrath!~w@wherever"));
        assert!(m("wrath!?w@*").matches("wrath!~w@h"));
        assert!(
            !m("wrath!?w@*").matches("wrath!~ww@h"),
            "? is exactly one char"
        );
        assert!(m("*!*@*").matches("a!b@c"));
    }

    #[test]
    fn missing_segments_default_to_wild() {
        assert!(m("*@gateway/foo").matches("anyone!anything@gateway/foo"));
        assert!(m("wrath!~w").matches("wrath!~w@anything.at.all"));
    }

    #[test]
    fn regex_segments() {
        let p = m("re:^(wrath|nyx)$!~w@re:^gateway/.*");
        assert!(p.matches("wrath!~w@gateway/veles"));
        assert!(p.matches("NYX!~w@gateway/x"));
        assert!(!p.matches("jackma!~w@gateway/veles"));
        // Anchored: the regex must cover the WHOLE segment.
        assert!(!m("re:wrath!*@*").matches("notwrath!a@b"));
    }

    #[test]
    fn mixing_glob_and_regex() {
        assert!(m("*!re:^~[a-z]$@*.example.com").matches("bob!~x@box.example.com"));
        assert!(!m("*!re:^~[a-z]$@*.example.com").matches("bob!xx@box.example.com"));
    }

    #[test]
    fn a_bad_regex_is_an_error_not_a_panic() {
        let err = MaskPattern::parse("re:[unclosed!*@*").unwrap_err();
        assert!(err.contains("bad regex"), "{err}");
    }

    #[test]
    fn empty_pattern_refused() {
        assert!(MaskPattern::parse("").is_err());
        assert!(MaskPattern::parse("   ").is_err());
    }

    #[test]
    fn looks_like_mask_distinguishes_identities() {
        assert!(looks_like_mask("wrath!~w@host"));
        assert!(looks_like_mask("*@host"));
        assert!(looks_like_mask("nick!user"));
        assert!(!looks_like_mask("irc:wrath"));
        assert!(!looks_like_mask("telegram:12345"));
    }

    #[test]
    fn specificity_is_three_tiers_per_segment() {
        // A pure wildcard pins nothing.
        assert_eq!(
            m("*!*@*").specificity(),
            MaskSpecificity {
                host: 0,
                ident: 0,
                nick: 0
            }
        );
        // An omitted segment is the same as `*`.
        assert_eq!(m("*@*").specificity().nick, 0);
        // A literal is the top tier and grows with length.
        let exact = m("me!me@1234-net").specificity();
        assert_eq!(exact.host, 1000 + 8);
        assert_eq!(exact.ident, 1000 + 2);
        assert_eq!(exact.nick, 1000 + 2);
        // A glob sits between, scored by the text it does pin.
        let glob = m("*!*@*.example.com").specificity();
        assert_eq!(glob.host, 100 + 12);
        assert!(glob.host < exact.host, "a glob never beats a literal");
        // `?` pins a shape even with no literal characters.
        assert_eq!(m("*!?@*").specificity().ident, 100);
        // A regex is scored by its literal content and never reaches
        // the literal tier — a regex that happens to match one string
        // is not provably exact.
        let re = m("re:^(wrath|nyx)$!*@*").specificity();
        assert_eq!(re.nick, 100 + 8);
        assert!(re.nick < 1000);
        // …and a `.*` regex pins nothing but its own anchoring.
        assert_eq!(m("re:.*!*@*").specificity().nick, 100);
    }

    #[test]
    fn specificity_orders_host_then_ident_then_nick() {
        // Compared field by field in that order, so a pinned ident
        // outranks a pinned nick and a pinned host outranks both.
        assert!(m("*!ident@*").specificity() > m("vic!*@*").specificity());
        assert!(m("*!*@a.host").specificity() > m("*!ident@*").specificity());
        assert!(m("me!me@a.host").specificity() > m("*!*@a.host").specificity());
    }

    #[test]
    fn split_tolerates_partial_observations() {
        assert_eq!(split("a!b@c"), ("a".into(), "b".into(), "c".into()));
        assert_eq!(split("justnick"), ("justnick".into(), "".into(), "".into()));
        assert_eq!(split("a!b"), ("a".into(), "b".into(), "".into()));
    }

    #[test]
    fn regex_metacharacters_in_globs_are_literal() {
        // A host with dots and brackets is common; the glob must read
        // them as themselves, not as regex syntax.
        assert!(m("*!*@*.freenode.net").matches("x!y@shell.freenode.net"));
        assert!(!m("*!*@*.freenode.net").matches("x!y@shell-freenode-net"));
        assert!(m("a(b)!u@h").matches("a(b)!u@h"));
    }

    #[test]
    fn two_patterns_say_whose_callers_they_share() {
        use PatternRelation::*;
        // The same callers, including case and an omitted segment
        // (both sides are read the way `parse` reads them).
        assert_eq!(pattern_relation("octo!*@*", "OCTO!*@*"), Equal);
        assert_eq!(pattern_relation("octo!~o", "octo!~o@*"), Equal);
        // Containment, the everyday direction: a literal nick inside a
        // glob, a pinned host inside every host.
        assert_eq!(pattern_relation("octo!*@*", "o*!*@*"), AWithinB);
        assert_eq!(pattern_relation("*!*@gateway/*", "*!*@*"), AWithinB);
        assert_eq!(pattern_relation("*!*@*", "nyx!~n@host"), BWithinA);
        // Crossing: the nick narrower one way, the host narrower the
        // other. Neither is a duplicate of the other, and the one that
        // loses a precedence comparison still decides for the callers
        // only it reaches — which is the whole reason Overlap exists.
        assert_eq!(pattern_relation("octo!*@*", "*!*@gateway/*"), Overlap);
        // No caller can match both.
        assert_eq!(pattern_relation("octo!*@*", "nyx!*@*"), Disjoint);
        assert_eq!(
            pattern_relation("*!*@libera.chat", "*!*@oftc.net"),
            Disjoint
        );
        // Two globs, and two regexes: not provable, and saying so is the
        // answer rather than a guess an operator would act on.
        assert_eq!(pattern_relation("o*!*@*", "*o!*@*"), Unknown);
        assert_eq!(pattern_relation("re:^o.*!*@*", "re:^oc.*!*@*"), Unknown);
        // A regex against a LITERAL is provable: the literal stands for
        // exactly one value, so the regex's own matcher decides.
        assert_eq!(
            pattern_relation("re:^(octo|nyx)$!*@*", "octo!*@*"),
            BWithinA
        );
        assert_eq!(pattern_relation("re:^octo$!*@*", "nyx!*@*"), Disjoint);
    }

    #[test]
    fn one_impossible_part_makes_the_whole_impossible() {
        use PatternRelation::*;
        // Disjoint dominates: a caller cannot satisfy a host that no
        // caller satisfies, whatever the other segments say.
        assert_eq!(combine_relations(&[AWithinB, Disjoint, Unknown]), Disjoint);
        // Unknown dominates everything that is still possible, because
        // "probably contained" is not something to delete a rule over.
        assert_eq!(combine_relations(&[AWithinB, Unknown, Equal]), Unknown);
        assert_eq!(combine_relations(&[Equal, Equal, Equal]), Equal);
        assert_eq!(combine_relations(&[AWithinB, Equal, Equal]), AWithinB);
        assert_eq!(combine_relations(&[Equal, BWithinA, Equal]), BWithinA);
        assert_eq!(combine_relations(&[AWithinB, BWithinA]), Overlap);
    }
}
