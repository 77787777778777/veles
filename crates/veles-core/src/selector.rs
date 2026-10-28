//! Matching a person-in-a-conversation against a rule's selectors, and
//! ranking how specifically it matched.
//!
//! Two features want the identical question — "which of these rules is
//! the one in force for this caller, here?" — and they want it answered
//! the same way or the answers drift:
//!
//! * `[sandbox.assign]` / runtime binds, which decide the container.
//! * `[[model.binds]]`, which decide the route a NEW session starts on.
//!
//! The logic lived inside `sandboxes::bind_specificity` and was about to
//! be copied. Sixty lines of precedence rules maintained twice is the
//! shape this tree keeps finding at the bottom of its own bugs, so it
//! moved here instead and `bind_specificity` became a wrapper.
//!
//! ## The ranking, and why it is a tuple
//!
//! Most specific wins, compared as `(exact_identity, mask_specificity,
//! scope_bits)` in that order:
//!
//! 1. an identity naming the person exactly beats any pattern;
//! 2. then how narrow the hostmask was (`ircmask::MaskSpecificity`);
//! 3. then how many scope selectors were pinned, channel weighing most.
//!
//! Ties keep the FIRST match, and config binds are listed before runtime
//! ones, so an equally specific runtime bind does not silently displace
//! a declared one.
//!
//! ## Roles compare, they do not only equal
//!
//! The old matcher did `Role::parse(r) != Some(principal.role)` — an
//! exact match. So `role = "<=trusted"` parsed as nothing, matched
//! nobody, and sat in a config file doing exactly that, silently. And
//! "everyone at or below trusted" is the case the feature exists for:
//! it is how you say "strangers and regulars get the cheap model, I do
//! not".
//!
//! `<=`, `>=`, `<`, `>` and a bare name are all accepted. `Role` derives
//! `Ord` (`Banned < Guest < User < Trusted < Admin < Owner`), so the
//! comparison is the obvious one.

use crate::acl::{Principal, Role, SenderContext};

/// The five selectors every scoped rule carries.
///
/// Borrowed rather than owned: the callers already hold their own
/// config structs and this exists to ask a question about them, not to
/// become a second representation of them.
#[derive(Debug, Clone, Copy, Default)]
pub struct Selector<'a> {
    /// A hostmask, a platform id, or a subject (`user:<name>` reaches a
    /// whole declared person).
    pub identity: Option<&'a str>,
    pub network: Option<&'a str>,
    pub channel: Option<&'a str>,
    pub platform: Option<&'a str>,
    /// `trusted`, `<=trusted`, `>=user`, `<admin`, `>guest`.
    pub role: Option<&'a str>,
}

/// How specifically this selector matched, or `None` for "did not".
pub type Specificity = (bool, crate::ircmask::MaskSpecificity, u8);

/// Does `spec` describe this principal's role?
///
/// Returns `false` for a spelling that names no role, which is the
/// conservative answer: a rule nobody can parse must not apply to
/// everybody. It is reported separately by each feature's own config
/// audit, so an unparseable role is loud rather than merely inert.
pub fn role_matches(spec: &str, actual: Role) -> bool {
    let s = spec.trim();
    let (op, name) = if let Some(r) = s.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = s.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = s.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = s.strip_prefix('>') {
        (">", r)
    } else {
        ("==", s)
    };
    let Some(want) = Role::parse(name) else {
        return false;
    };
    match op {
        "<=" => actual <= want,
        ">=" => actual >= want,
        "<" => actual < want,
        ">" => actual > want,
        _ => actual == want,
    }
}

/// Is `spec` a role expression this tree understands?
///
/// Separate from [`role_matches`] so a config audit can say "that is not
/// a role" instead of a rule quietly never firing — the failure mode the
/// old exact-match comparison had for every `<=` spelling anyone wrote.
pub fn role_spec_is_valid(spec: &str) -> bool {
    let s = spec.trim();
    let name = s
        .strip_prefix("<=")
        .or_else(|| s.strip_prefix(">="))
        .or_else(|| s.strip_prefix('<'))
        .or_else(|| s.strip_prefix('>'))
        .unwrap_or(s);
    Role::parse(name).is_some()
}

fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// Match a selector against a caller and rank how specifically it hit.
///
/// Every selector that is PRESENT must match; absent ones are wildcards.
pub fn specificity(
    sel: &Selector<'_>,
    principal: &Principal,
    ctx: &SenderContext,
) -> Option<Specificity> {
    let eq = |want: Option<&str>, got: Option<&str>| match want {
        None => true,
        Some(w) => got.is_some_and(|g| g.eq_ignore_ascii_case(w)),
    };
    if !eq(sel.network, ctx.network.as_deref()) {
        return None;
    }
    if !eq(sel.platform, ctx.platform.as_deref()) {
        return None;
    }
    // A channel selector matches either the room or the chat id, because
    // the two spellings are the same place on different platforms.
    if sel.channel.is_some()
        && !eq(sel.channel, ctx.channel.as_deref())
        && !eq(sel.channel, ctx.chat_id.as_deref())
    {
        return None;
    }
    if let Some(r) = sel.role {
        if !role_matches(r, principal.role) {
            return None;
        }
    }

    let mut exact = false;
    let mut mask_spec = crate::ircmask::MaskSpecificity::default();
    if let Some(id) = sel.identity {
        if crate::ircmask::looks_like_mask(id) {
            let observed = ctx.mask.as_deref()?;
            let p = crate::ircmask::MaskPattern::parse(id).ok()?;
            if !p.matches(observed) {
                return None;
            }
            mask_spec = p.specificity();
        } else {
            let want = normalize(id);
            let matches = principal
                .id
                .as_deref()
                .is_some_and(|i| normalize(i) == want)
                || normalize(&principal.subject) == want;
            if !matches {
                return None;
            }
            exact = true;
        }
    }

    let scope = (if sel.channel.is_some() { 4 } else { 0 })
        + (if sel.network.is_some() { 2 } else { 0 })
        + (if sel.platform.is_some() { 1 } else { 0 })
        + (if sel.role.is_some() { 1 } else { 0 });
    Some((exact, mask_spec, scope))
}

/// Pick the most specific of a set of rules, keeping the FIRST on a tie.
///
/// The tie rule is load-bearing: callers list config rules before
/// runtime ones, so an equally specific runtime rule does not displace a
/// declared one without saying so.
pub fn most_specific<T>(
    rules: impl IntoIterator<Item = T>,
    principal: &Principal,
    ctx: &SenderContext,
    sel_of: impl Fn(&T) -> Selector<'_>,
) -> Option<T> {
    let mut best: Option<(Specificity, T)> = None;
    for r in rules {
        let Some(spec) = specificity(&sel_of(&r), principal, ctx) else {
            continue;
        };
        match &best {
            Some((bs, _)) if *bs >= spec => {}
            _ => best = Some((spec, r)),
        }
    }
    best.map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comparison the exact-match rule could not express, and the
    /// silent no-op it produced.
    ///
    /// `role = "<=trusted"` used to parse as no role at all, so the rule
    /// matched nobody and sat in the config doing nothing, with no
    /// warning. "Everyone at or below trusted" is the case per-class
    /// defaults exist for — strangers and regulars on the cheap model,
    /// the operator not.
    #[test]
    fn a_role_selector_compares_and_does_not_only_equal() {
        assert!(role_matches("trusted", Role::Trusted));
        assert!(!role_matches("trusted", Role::User));

        assert!(role_matches("<=trusted", Role::Guest));
        assert!(role_matches("<=trusted", Role::User));
        assert!(role_matches("<=trusted", Role::Trusted));
        assert!(!role_matches("<=trusted", Role::Admin));
        assert!(!role_matches("<=trusted", Role::Owner));

        assert!(role_matches(">=admin", Role::Owner));
        assert!(!role_matches(">=admin", Role::Trusted));
        assert!(role_matches("<guest", Role::Banned));
        assert!(!role_matches("<guest", Role::Guest));
        assert!(role_matches(">user", Role::Trusted));
        assert!(!role_matches(">user", Role::User));

        // Whitespace is tolerated; nonsense is not, and it matches
        // NOBODY rather than everybody.
        assert!(role_matches(" <= trusted ", Role::User));
        assert!(!role_matches("nonsense", Role::Owner));
        assert!(!role_matches("<=nonsense", Role::Guest));

        assert!(role_spec_is_valid("<=trusted"));
        assert!(role_spec_is_valid("owner"));
        assert!(!role_spec_is_valid("<=nonsense"));
        assert!(!role_spec_is_valid(""));
    }

    fn ctx() -> SenderContext {
        SenderContext {
            mask: Some("wrath!~w@users/wrath".into()),
            network: Some("irc.synirc.net".into()),
            channel: Some("#flashsupport".into()),
            platform: Some("irc".into()),
            chat_id: Some("#flashsupport".into()),
            direct: false,
            handle: None,
            post: None,
        }
    }

    fn who(role: Role) -> Principal {
        Principal::known("irc:synirc/wrath", role)
    }

    #[test]
    fn every_present_selector_must_match_and_absent_ones_are_wildcards() {
        let p = who(Role::User);
        let c = ctx();
        assert!(specificity(&Selector::default(), &p, &c).is_some(), "bare");
        assert!(specificity(
            &Selector {
                platform: Some("irc"),
                ..Default::default()
            },
            &p,
            &c
        )
        .is_some());
        assert!(specificity(
            &Selector {
                platform: Some("telegram"),
                ..Default::default()
            },
            &p,
            &c
        )
        .is_none());
        assert!(
            specificity(
                &Selector {
                    network: Some("IRC.SYNIRC.NET"),
                    ..Default::default()
                },
                &p,
                &c
            )
            .is_some(),
            "case-insensitive"
        );
        assert!(specificity(
            &Selector {
                channel: Some("#coven"),
                ..Default::default()
            },
            &p,
            &c
        )
        .is_none());
    }

    /// Narrower beats wider, and the order of the tuple is the order of
    /// the rules: an exact identity beats a mask, a mask beats scope.
    #[test]
    fn the_most_specific_rule_wins_and_a_tie_keeps_the_first() {
        let p = who(Role::User);
        let c = ctx();
        // (label, selector)
        let rules: Vec<(&str, Selector<'_>)> = vec![
            ("wide", Selector::default()),
            (
                "platform",
                Selector {
                    platform: Some("irc"),
                    ..Default::default()
                },
            ),
            (
                "channel",
                Selector {
                    channel: Some("#flashsupport"),
                    ..Default::default()
                },
            ),
            (
                "exact",
                Selector {
                    identity: Some("irc:synirc/wrath"),
                    ..Default::default()
                },
            ),
        ];
        let got = most_specific(rules.clone(), &p, &c, |r| r.1).expect("one matched");
        assert_eq!(got.0, "exact", "an exact identity outranks every scope");

        let no_exact: Vec<_> = rules.iter().filter(|r| r.0 != "exact").cloned().collect();
        let got = most_specific(no_exact, &p, &c, |r| r.1).expect("one matched");
        assert_eq!(got.0, "channel", "channel outweighs platform");

        // A tie keeps the FIRST, which is why callers list config rules
        // before runtime ones.
        let tied: Vec<(&str, Selector<'_>)> = vec![
            (
                "declared",
                Selector {
                    platform: Some("irc"),
                    ..Default::default()
                },
            ),
            (
                "runtime",
                Selector {
                    platform: Some("irc"),
                    ..Default::default()
                },
            ),
        ];
        let got = most_specific(tied, &p, &c, |r| r.1).expect("matched");
        assert_eq!(got.0, "declared");
    }

    #[test]
    fn a_role_range_selects_a_group_and_excludes_the_operator() {
        let c = ctx();
        let sel = Selector {
            role: Some("<=trusted"),
            ..Default::default()
        };
        for r in [Role::Guest, Role::User, Role::Trusted] {
            assert!(
                specificity(&sel, &who(r), &c).is_some(),
                "{r:?} is at or below trusted"
            );
        }
        for r in [Role::Admin, Role::Owner] {
            assert!(specificity(&sel, &who(r), &c).is_none(), "{r:?} is above");
        }
    }

    /// A mask selector needs an observed mask; without one the rule does
    /// not match rather than matching loosely.
    #[test]
    fn a_mask_selector_without_an_observed_mask_does_not_match() {
        let p = who(Role::User);
        let sel = Selector {
            identity: Some("*!*@users/wrath"),
            ..Default::default()
        };
        assert!(specificity(&sel, &p, &ctx()).is_some());
        let bare = SenderContext {
            mask: None,
            ..ctx()
        };
        assert!(specificity(&sel, &p, &bare).is_none());
    }
}
