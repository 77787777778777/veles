#!/usr/bin/env tclsh
# Mutation harness for slopdrop art on Bluesky (PLAN-slopdrop-art.md).
# Each mutant removes one guard and must turn its named test RED; the
# source is restored after every run.
#
# P0: a public reply's `MEDIA:` line is never an attachment (the policy
# keeps it as text, the plugin ignores markers on a shaped reply), and a
# quote by anyone but the configured owner names a post of the thread or
# their own, within the per-post and per-author caps, and never a post
# whose author said no.
#
# P3: a Bluesky reply's drawing goes out as a picture. The evaluation
# carries its raw output in veles' own envelope (a forged one is
# dropped); the owner's drawing is drawn, checked once by the image
# check, labelled with what it saw, and held on the hold set or a failed
# check, never when the owner approved it, always when the owner deleted
# it; anyone else's is asked about first; a withheld text withholds the
# picture with it. The plugin takes only PNGs and self-label values, and
# posts the PNG as drawn with its shape and the labels.
#
# P4: a held picture is kept on its ledger entry, asked about with the
# picture (at most five asks an hour per requester), shown in the hosted
# copy, and an approve reposts it as a picture through the bus, takes the
# withheld post down only when the repost's receipt arrives, and
# allowlists the drawing.
#
# P6: a Bluesky reply cut to its head also carries the whole text drawn
# as one picture, only when the text the picture shows passes the word
# checks, which score a long text in pieces the scorer reads whole; a
# short text is never drawn.
#
# P6 image macros: only a picture the thread shows (HTTPS, at most 8 MB,
# a bounded canvas), the ops as named, captions written, and a PNG that
# fits Bluesky's limit.
#
# Wrap-up round (USER 2026-10-12): anyone's macros, a stranger's through
# the word checks on what it writes and the image check's stricter set;
# the base library's `magick_scheme` programs drawn (only the programs no
# other names, their handles no post text, names folding case, the
# library's blank canvases made here, a clockwise turn, bounded in
# operations, pixels and steps, no program naming itself, annotate's
# words checked, only the thread's pictures fetched); a withheld long
# text keeps its whole text drawn and an approve reposts the head and
# the link with it, in one post.
#
# The plugin's quote check (2026-10-12): the author's own self-label
# refuses a quote only when it is an adult one; `bot` does not.
#
# P5: anyone else's drawing is withheld, unshown to the image check, when
# the bridge gives no proc index, when the line reaches the bad list
# (tracked procs through their bodies, the base library one hop, stamps
# by their `say` names, a random `stampsay`), when the typed line names a
# denylisted word, or when the words the drawing shows fail the word
# checks (links read from the text as shown, the denylist, the scorer);
# then the image check holds it on the public set. A quote of a thread
# post whose text fails the word checks is dropped.
#
# Not a mutant yet, on purpose: trigger answers shaped with owner=false.
# A trigger answer carries no spec, so it draws nothing and quotes
# nothing, and flipping the flag changes nothing a test can see.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-12-slopdrop-art.tcl [--dry] [a filter]
#
# In a worktree whose .envrc is not allowed, MUTCHECK_INHERIT_ENV=1 runs
# cargo in the caller's environment (already the devShell's) instead.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set PUB  crates/veles-slopdrop/src/public.rs
set BSKY crates/veles-slopdrop/src/bsky.rs
set QCAP crates/veles-slopdrop/src/quotecap.rs
set ADP  crates/veles-atproto/src/adapter.rs
set ART  crates/veles-slopdrop/src/art.rs
set EVAL crates/veles-slopdrop/src/eval.rs
set RICH crates/veles-atproto/src/rich.rs
set ASK  crates/veles-slopdrop/src/ask.rs
set PBL  crates/veles-slopdrop/src/publish.rs
set RCPT crates/veles-slopdrop/src/receipts.rs
set SAFE crates/veles-slopdrop/src/safety.rs
set SYNC crates/veles-core/src/syncask.rs
set CHK  crates/veles-slopdrop/src/check.rs
set MAG  crates/veles-slopdrop/src/magick.rs
set SCH  crates/veles-slopdrop/src/scheme.rs
set CORE {-p veles-core --lib}
set LIB  {-p veles-slopdrop --lib}
set AT   {-p veles-atproto --lib}

set mutations [list \
    [list "policy: a MEDIA line is passed through" \
        $PUB \
        "        let output = neutralise_media(&text_src);" \
        "        let output = std::borrow::Cow::Borrowed(text_src.as_str()); // MUTATION" \
        $LIB public::tests::a_media_line_in_a_public_reply_is_never_an_attachment] \
    [list "policy: a form feed does not start a line" \
        $PUB \
        "        if matches!(c, '\\n' | '\\r' | '\\u\{c\}') \{" \
        "        if matches!(c, '\\n' | '\\r') \{ // MUTATION" \
        $LIB public::tests::a_media_line_in_a_public_reply_is_never_an_attachment] \
    [list "plugin: markers are read from a shaped reply" \
        $ADP \
        "    if shaped_from_code \{" \
        "    if false \{ // MUTATION" \
        $AT adapter::tests::only_a_turn_reply_carries_attachment_markers] \
    [list "spec: anyone quotes anything" \
        $BSKY \
        "    if allowed.owner \{
        return uri.starts_with(\"at://\")" \
        "    if true \{ // MUTATION
        return uri.starts_with(\"at://\")" \
        $LIB bsky::tests::a_quote_names_a_post_of_the_thread_unless_the_owner_asked] \
    [list "spec: an own post is any uri naming the DID" \
        $BSKY \
        "            .is_some_and(|did| did.starts_with(\"did:\") && uri_repo(uri) == Some(did))" \
        "            .is_some_and(|did| uri.contains(did)) // MUTATION" \
        $LIB bsky::tests::a_quote_names_a_post_of_the_thread_unless_the_owner_asked] \
    [list "spec: the thread includes the parent only via the thread list" \
        $BSKY \
        "            add(reply.get(\"parent\").and_then(Value::as_str));" \
        "            // MUTATION" \
        $LIB bsky::tests::the_quotable_posts_are_the_thread_the_caller_is_in] \
    [list "policy: a dormant ACL makes everyone the owner" \
        $PUB \
        "    cfg.acl.is_configured()
        && " \
        "    true // MUTATION
        && " \
        $LIB public::tests::a_quote_rides_inside_the_thread_and_is_counted_unless_the_owner_asked] \
    [list "policy: quotes are not counted" \
        $PUB \
        "            if let Some(why) = crate::quotecap::refusal(uri) \{" \
        "            if let Some(why) = None::<String> \{ // MUTATION" \
        $LIB public::tests::a_quote_rides_inside_the_thread_and_is_counted_unless_the_owner_asked] \
    [list "quotecap: no per-author limit" \
        $QCAP \
        "    if recent.iter().filter(|q| q.author == author).count() >= PER_AUTHOR \{" \
        "    if false \{ // MUTATION" \
        $LIB quotecap::tests::quotes_of_one_author_are_capped_per_day_and_per_post] \
    [list "quotecap: the counts never expire" \
        $QCAP \
        "        .filter(|q| at.saturating_sub(q.at) < WINDOW_SECS)" \
        "        .filter(|_| true) // MUTATION" \
        $LIB quotecap::tests::quotes_of_one_author_are_capped_per_day_and_per_post] \
    [list "plugin: a postgate is ignored" \
        $ADP \
        "    if flag(\"/viewer/embeddingDisabled\") \{" \
        "    if false \{ // MUTATION" \
        $AT adapter::tests::a_post_that_says_no_is_not_quoted] \
    [list "plugin: a block is ignored" \
        $ADP \
        "    if flag(\"/author/viewer/blockedBy\") || blocking \{" \
        "    if false \{ // MUTATION" \
        $AT adapter::tests::a_post_that_says_no_is_not_quoted] \
    [list "plugin: labels are ignored" \
        $ADP \
        "    if labelled(\"/labels\") || labelled(\"/author/labels\") \{" \
        "    if false \{ // MUTATION" \
        $AT adapter::tests::a_post_that_says_no_is_not_quoted] \
    [list "plugin: the author's own bot label refuses a quote" \
        $ADP \
        "                    v != \"!no-unauthenticated\" && (!own || ADULT_SELF_LABELS.contains(&v))" \
        "                    let _ = own; v != \"!no-unauthenticated\" // MUTATION" \
        $AT adapter::tests::a_post_that_says_no_is_not_quoted] \
    [list "plugin: the author's own adult label is no reason" \
        $ADP \
        "                    v != \"!no-unauthenticated\" && (!own || ADULT_SELF_LABELS.contains(&v))" \
        "                    v != \"!no-unauthenticated\" && !own // MUTATION" \
        $AT adapter::tests::a_post_that_says_no_is_not_quoted] \
    [list "plugin: a labeler's label counts as the author's own" \
        $ADP \
        "                    let own = author.is_some()" \
        "                    let own = true || author.is_some() // MUTATION" \
        $AT adapter::tests::a_post_that_says_no_is_not_quoted] \
    [list "eval: a forged envelope is kept" \
        $EVAL \
        "            crate::art::strip_private(&mut spec);" \
        "            // MUTATION" \
        $LIB eval::more_routing_tests::a_bluesky_evaluation_carries_its_raw_output_in_an_envelope] \
    [list "eval: no envelope on Bluesky" \
        $EVAL \
        "            if bluesky \{
                let env" \
        "            if false \{ // MUTATION
                let env" \
        $LIB eval::more_routing_tests::a_bluesky_evaluation_carries_its_raw_output_in_an_envelope] \
    [list "eval: a quote-only answer says ok (no output)" \
        $EVAL \
        "            if s.is_empty() && !(bluesky && embeds && !r.is_error) \{" \
        "            if s.is_empty() \{ // MUTATION" \
        $LIB eval::more_routing_tests::a_bluesky_evaluation_carries_its_raw_output_in_an_envelope] \
    [list "policy: anyone's drawing skips the public checks" \
        $PUB \
        "    if !owner && !approved \{" \
        "    if false \{ // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "policy: an approved drawing is held anyway" \
        $PUB \
        "    let approved = hashes.iter().all(|h| safety::allowlisted(h));" \
        "    let approved = false; // MUTATION" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "policy: a drawing the owner deleted is checked and posted" \
        $PUB \
        "    if hashes.iter().any(|h| safety::blocked(h))" \
        "    if false // MUTATION" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "policy: a failed check lets the picture out" \
        $PUB \
        "    \} else if let Some(e) = failed \{" \
        "    \} else if let Some(e) = None::<String> \{ // MUTATION" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "policy: what the check saw labels nothing" \
        $PUB \
        "                for l in s.labels() \{" \
        "                for l in Vec::<&str>::new() \{ // MUTATION" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "policy: a withheld text lets its picture out" \
        $PUB \
        "        (Pictures::Post \{ .. \}, Some(t)) if t.kind == PostKind::Withheld => \{" \
        "        (Pictures::Post \{ .. \}, Some(t)) if false => \{ // MUTATION" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "art: the owner's hold set holds nothing" \
        $ART \
        "        let set: &\[&str\] = if owner \{ &OWNER_HOLD \} else \{ &PUBLIC_WITHHOLD \};" \
        "        let set: &\[&str\] = if owner \{ &\[\] \} else \{ &PUBLIC_WITHHOLD \}; // MUTATION" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "art: a kept verdict is never read" \
        $ART \
        "    if let Some(s) = cached(sha) \{" \
        "    if let Some(s) = None::<Seen> \{ // MUTATION" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "art: an unknown category is ignored" \
        $ART \
        "            return Err(format!(\"the image check named an unknown category \{c:?\}\"));" \
        "            continue; // MUTATION" \
        $LIB art::tests::the_verdict_is_read_strictly] \
    [list "art: one line of prose is a drawing" \
        $ART \
        "    if text.lines().filter(|l| !l.trim().is_empty()).count() < 2 \{" \
        "    if false \{ // MUTATION" \
        $LIB art::tests::a_drawing_is_art_and_prose_is_not] \
    [list "plugin: any bytes are a picture" \
        $RICH \
        "                    if png.starts_with(b\"\\x89PNG\\r\\n\\x1a\\n\") && png.len() <= MAX_IMAGE_BYTES \{" \
        "                    if true \{ // MUTATION" \
        $AT rich::tests::a_spec_carries_drawn_pictures_and_self_labels] \
    [list "plugin: any label value rides" \
        $RICH \
        "                    if SELF_LABELS.contains(&v.as_str()) && !spec.labels.contains(&v) \{" \
        "                    if !spec.labels.contains(&v) \{ // MUTATION" \
        $AT rich::tests::a_spec_carries_drawn_pictures_and_self_labels] \
    [list "plugin: a post carries no self-labels" \
        $ADP \
        "        crate::post::self_label(&mut rec, &spec.labels);" \
        "        // MUTATION" \
        $AT adapter::tests::a_drawn_picture_is_posted_as_a_labelled_png_with_its_shape] \
    [list "plugin: a drawing has no shape" \
        $ADP \
        "                let ratio = crate::post::ratio_of(a.width, a.height)
                    .or_else(|| crate::post::aspect_ratio(&a.png));" \
        "                let ratio = None; // MUTATION" \
        $AT adapter::tests::a_drawn_picture_is_posted_as_a_labelled_png_with_its_shape] \
    [list "policy: a held drawing is not kept on its entry" \
        $PUB \
        "            pictures: picture_refs," \
        "            pictures: Vec::new(), // MUTATION" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "safety: the hosted copy shows no picture" \
        $SAFE \
        "    if pictures.is_empty() \{
        return (md, html);" \
        "    if true \{ // MUTATION
        return (md, html);" \
        $LIB public::tests::the_owners_drawing_goes_out_as_a_checked_and_labelled_picture] \
    [list "ask: the question has no picture" \
        $ASK \
        "                    image = crate::art::kept_png(&first.sha)" \
        "                    image = None::<Vec<u8>> // MUTATION" \
        $LIB ask::tests::a_held_drawing_is_asked_with_its_picture] \
    [list "ask: no cap per requester" \
        $ASK \
        "            if *n >= ASKS_PER_HOUR \{" \
        "            if false \{ // MUTATION" \
        $LIB ask::tests::one_requester_is_asked_at_most_five_times_an_hour] \
    [list "core: a source's picture never reaches the ask" \
        $SYNC \
        "                    image = q.image;" \
        "                    let _ = q.image; // MUTATION" \
        $CORE syncask::tests::a_registered_source_is_scanned_asked_and_decided_as_its_own] \
    [list "publish: a held picture is reposted as text" \
        $PBL \
        "        WithheldAction::Approve if pictures => repost_pictures(&entry).await?," \
        "        WithheldAction::Approve if false => repost_pictures(&entry).await?, // MUTATION" \
        $LIB publish::tests::approving_a_held_picture_reposts_it_then_takes_the_note_down] \
    [list "publish: approve leaves the drawing off the allowlist" \
        $PBL \
        "        safety::allowlist_add(&p.text_hash)?;" \
        "        // MUTATION" \
        $LIB publish::tests::approving_a_held_picture_reposts_it_then_takes_the_note_down] \
    [list "receipts: the withheld post never comes down" \
        $RCPT \
        "        let _ = store.update(id, |e| gone = std::mem::take(&mut e.superseded));" \
        "        // MUTATION" \
        $LIB publish::tests::approving_a_held_picture_reposts_it_then_takes_the_note_down] \
    [list "publish: the note comes down before the repost lands" \
        $PBL \
        "        WithheldAction::Approve if pictures => repost_pictures(&entry).await?," \
        "        WithheldAction::Approve if pictures => \{ let a = repost_pictures(&entry).await?; take_down_posts(&entry.post_uris).await; a \} // MUTATION" \
        $LIB publish::tests::approving_a_held_picture_reposts_it_then_takes_the_note_down] \
    [list "public: no proc index lets anyone's drawing through" \
        $PUB \
        "        return Some(\"the proc index is unavailable\".to_string());" \
        "        return None; // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "public: what the line reaches is not read" \
        $PUB \
        "    if let Some(n) = idx.bad_reach(c.code, &crate::safety::verdicts()) \{" \
        "    if let Some(n) = None::<String> \{ let _ = idx; // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "public: the typed line skips the denylist" \
        $PUB \
        "    if crate::check::denylist_hit(c.code) \{" \
        "    if false \{ // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "public: the words a drawing shows are not read" \
        $PUB \
        "    let words: Vec<&str> = pics.iter().map(|p| p.visible.as_str()).collect();" \
        "    let words: Vec<&str> = Vec::new(); // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "public: the link check reads words, which drop the ://" \
        $PUB \
        "    let words: Vec<&str> = pics.iter().map(|p| p.visible.as_str()).collect();" \
        "    let words: Vec<&str> = pics.iter().map(|p| p.words.as_str()).collect(); // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "public: a quote of the thread is not checked" \
        $PUB \
        "                    Some(s) if !owner => \{" \
        "                    Some(s) if false => \{ // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "public: a quote that fails the check is kept" \
        $PUB \
        "                if !checked.outcome.pass() \{" \
        "                if false \{ // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "check: a drawing's words skip the link check" \
        $CHK \
        "    if link_or_contact(text, mentions) \{
        return plain(" \
        "    if false \{ // MUTATION
        return plain(" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "check: a drawing's words skip the denylist" \
        $CHK \
        "    if denylist_hit(text) \{
        return plain(" \
        "    if false \{ // MUTATION
        return plain(" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "check: a drawing's words skip the scorer" \
        $CHK \
        "        let c = score(endpoints, piece, tox, id).await;" \
        "        let c = plain(Outcome::Pass); let _ = (endpoints, piece, tox, id); // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "safety: a <x>say on the line names no stamp" \
        $SAFE \
        "            .chain(stamps_named(code))" \
        "            .chain(Vec::<String>::new()) // MUTATION" \
        $LIB safety::tests::the_base_library_is_reached_one_hop_and_stamps_by_their_say_names] \
    [list "safety: -s names no stamp" \
        $SAFE \
        "        if *w == \"-s\" \{" \
        "        if false \{ // MUTATION" \
        $LIB safety::tests::the_base_library_is_reached_one_hop_and_stamps_by_their_say_names] \
    [list "safety: a random stampsay passes" \
        $SAFE \
        "    words.iter().any(|w| *w == \"stampsay\" || *w == \"stampthink\") && !words.contains(&\"-s\")" \
        "    let _ = words; false // MUTATION" \
        $LIB safety::tests::the_base_library_is_reached_one_hop_and_stamps_by_their_say_names] \
    [list "safety: a tracked body's random stamp passes" \
        $SAFE \
        "            if !base && random_stamp(body) \{" \
        "            if false \{ // MUTATION" \
        $LIB safety::tests::the_base_library_is_reached_one_hop_and_stamps_by_their_say_names] \
    [list "safety: a base body's calls are followed" \
        $SAFE \
        "                .filter(|_| !base)" \
        "                .filter(|_| true) // MUTATION" \
        $LIB safety::tests::the_base_library_is_reached_one_hop_and_stamps_by_their_say_names] \
    [list "safety: a base body is not read for its stamps" \
        $SAFE \
        "                .chain(stamps_named(body));" \
        "                .chain(Vec::<String>::new()); // MUTATION" \
        $LIB safety::tests::the_base_library_is_reached_one_hop_and_stamps_by_their_say_names] \
    [list "safety: no proc is the base library's" \
        $SAFE \
        "            if p\[\"origin\"\].as_str() == Some(\"base\") \{" \
        "            if false \{ // MUTATION" \
        $LIB safety::tests::the_base_library_is_reached_one_hop_and_stamps_by_their_say_names] \
    [list "safety: the index leaves the base library out" \
        $SAFE \
        "    let procs = crate::bridge::get_json(cfg, \"/api/procs?base=1\")" \
        "    let procs = crate::bridge::get_json(cfg, \"/api/procs\") // MUTATION" \
        $LIB public::tests::anyones_drawing_passes_the_public_checks_then_the_image_check] \
    [list "long text: never drawn" \
        $PUB \
        "                long_text_picture(&cfg, &r.output, &mentions).await" \
        "                \{ let _ = (&cfg, &r.output, &mentions); None \} // MUTATION" \
        $LIB public::tests::a_long_text_goes_out_with_the_whole_text_drawn] \
    [list "long text: a short reply is drawn too" \
        $PUB \
        "                    && crate::safety::public_text(&clean).elided =>" \
        "                    && true => // MUTATION" \
        $LIB public::tests::a_long_text_goes_out_with_the_whole_text_drawn] \
    [list "long text: drawn whatever the check says" \
        $PUB \
        "    if let Some(why) = checked.outcome.reason() \{" \
        "    if let Some(why) = None::<String> \{ // MUTATION" \
        $LIB public::tests::a_long_text_goes_out_with_the_whole_text_drawn] \
    [list "long text: the picture's text is not what is checked" \
        $PUB \
        "        &pic.visible,
        cfg.output_classifier_tox" \
        "        \"\", // MUTATION
        cfg.output_classifier_tox" \
        $LIB public::tests::a_long_text_goes_out_with_the_whole_text_drawn] \
    [list "check: a long text is scored whole" \
        $CHK \
        "    for piece in score_chunks(text, SCORE_CHUNK_CHARS) \{" \
        "    for piece in \[text\] \{ // MUTATION" \
        $LIB public::tests::a_long_text_goes_out_with_the_whole_text_drawn] \
    [list "check: a piece is never cut at a line end" \
        $CHK \
        "            let cut = last_break.filter(|&b| b > start).unwrap_or(i);" \
        "            let cut = i; let _ = last_break; // MUTATION" \
        $LIB check::tests::a_long_text_is_scored_in_pieces] \
    [list "macro: any URL is fetched" \
        $MAG \
        "    if !fetchable(url) || !allowed.iter().any(|a| a == url) \{" \
        "    if !fetchable(url) \{ let _ = allowed; // MUTATION" \
        $LIB public::tests::an_image_macro_is_drawn_checked_and_attached] \
    [list "macro: plain http is fetched" \
        $MAG \
        "    url.starts_with(\"https://\") || (cfg!(test) && url.starts_with(\"http://127.0.0.1:\"))" \
        "    let _ = url; true // MUTATION" \
        $LIB magick::tests::only_a_picture_the_thread_shows_is_fetched] \
    [list "macro: an announced size past 8 MB is read" \
        $MAG \
        "        .is_some_and(|n| n > MAX_FETCH_BYTES as u64)" \
        "        .is_some_and(|n| n > u64::MAX - 1) // MUTATION" \
        $LIB magick::tests::a_picture_past_8_mb_is_not_read] \
    [list "macro: an unannounced body past 8 MB is read" \
        $MAG \
        "        if body.len() + chunk.len() > MAX_FETCH_BYTES \{" \
        "        if false \{ // MUTATION" \
        $LIB magick::tests::a_picture_past_8_mb_is_not_read] \
    [list "macro: the decoder has no size limit" \
        $MAG \
        "    reader.limits(limits);" \
        "    let _ = limits; // MUTATION" \
        $LIB magick::tests::a_huge_canvas_is_not_decoded] \
    [list "macro: the ops are not run" \
        $MAG \
        "        img = apply(img, op)?;" \
        "        let _ = op; // MUTATION" \
        $LIB magick::tests::the_ops_change_the_picture_as_named] \
    [list "macro: a caption writes nothing" \
        $MAG \
        "            caption(&mut out, top, bottom);" \
        "            let _ = (top, bottom); // MUTATION" \
        $LIB magick::tests::the_ops_change_the_picture_as_named] \
    [list "macro: a PNG past 1 MB goes out" \
        $MAG \
        "        if out.len() <= MAX_PNG_BYTES \{" \
        "        if true \{ // MUTATION" \
        $LIB magick::tests::the_ops_change_the_picture_as_named] \
    [list "wrap-up macro: a stranger's macro is dropped" \
        $PUB \
        "    let macros: &\[crate::magick::Macro\] = &plan.macros;" \
        "    let macros: &\[crate::magick::Macro\] = if owner \{ &plan.macros \} else \{ &\[\] \}; // MUTATION" \
        $LIB public::tests::an_image_macro_is_drawn_checked_and_attached] \
    [list "wrap-up macro: the words a macro writes are not checked" \
        $PUB \
        "        let (png, width, height, words) = crate::magick::draw(&m, &sources)?;" \
        "        let (png, width, height, _w) = crate::magick::draw(&m, &sources)?; let words = String::new(); // MUTATION" \
        $LIB public::tests::an_image_macro_is_drawn_checked_and_attached] \
    [list "scheme: the handle is post text" \
        $PUB \
        "        let text_src = crate::art::without_handles(text_src, &plan.handles);" \
        "        let text_src = text_src.to_string(); // MUTATION" \
        $LIB public::tests::a_magick_scheme_drawing_goes_out_without_its_handle] \
    [list "scheme: every program is drawn, named or not" \
        $SCH \
        "        .filter(|p| !referenced.contains(&p.id))" \
        "        .filter(|_| true) // MUTATION" \
        $LIB scheme::tests::a_handle_runs_the_program_it_names] \
    [list "scheme: a handle cuts a longer one" \
        $ART \
        "                if !after.starts_with(|c: char| c.is_ascii_digit()) \{" \
        "                if true \{ // MUTATION" \
        $LIB art::tests::handles_are_no_post_text] \
    [list "scheme: names do not fold case" \
        $SCH \
        ".find(|(n, _)| n.eq_ignore_ascii_case(s))" \
        ".find(|(n, _)| n == s) // MUTATION" \
        $LIB scheme::tests::the_librarys_programs_draw_what_they_say] \
    [list "scheme: the library's blank canvases are not made" \
        $SCH \
        "        let img = if let Some(c) = canvas_for(s) \{" \
        "        let img = if let Some(c) = None::<RgbaImage> \{ // MUTATION" \
        $LIB scheme::tests::the_librarys_programs_draw_what_they_say] \
    [list "scheme: an odd angle turns counter-clockwise" \
        $SCH \
        "    let (sin, cos) = d.to_radians().sin_cos();" \
        "    let (sin, cos) = (-d).to_radians().sin_cos(); // MUTATION" \
        $LIB scheme::tests::the_librarys_programs_draw_what_they_say] \
    [list "scheme: no cap on image operations" \
        $SCH \
        "        if self.ops > MAX_IMAGE_OPS \{" \
        "        if false \{ // MUTATION" \
        $LIB scheme::tests::a_program_stays_inside_its_bounds] \
    [list "scheme: no cap on pixels" \
        $SCH \
        "        if self.pixels > MAX_PIXELS \{" \
        "        if false \{ // MUTATION" \
        $LIB scheme::tests::a_program_stays_inside_its_bounds] \
    [list "scheme: no cap on steps" \
        $SCH \
        "        if self.steps > MAX_STEPS \{" \
        "        if false \{ // MUTATION" \
        $LIB scheme::tests::a_program_stays_inside_its_bounds] \
    [list "scheme: a program may name itself" \
        $SCH \
        "        if self.running.iter().any(|r| r == id) \{" \
        "        if false \{ // MUTATION" \
        $LIB scheme::tests::a_handle_runs_the_program_it_names] \
    [list "scheme: annotate's words are not kept" \
        $SCH \
        "                            self.words.push(t.trim().to_string());" \
        "                            let _ = &t; // MUTATION" \
        $LIB scheme::tests::written_words_are_drawn_and_kept] \
    [list "scheme: a URL the thread does not show is handed to fetch" \
        $MAG \
        "        if !ops && !allowed.contains(&url) \{" \
        "        if false \{ // MUTATION" \
        $LIB public::tests::a_magick_scheme_drawing_goes_out_without_its_handle] \
    [list "long text: a withheld one keeps no drawing" \
        $PUB \
        "                if s.kind == PostKind::Withheld\n                    && pictures.is_none()" \
        "                if false\n                    && pictures.is_none() // MUTATION" \
        $LIB public::tests::a_long_text_goes_out_with_the_whole_text_drawn] \
    [list "long text: an approve reposts it with no text" \
        $PBL \
        "            .unwrap_or_else(|| repost_text(entry))," \
        "            .unwrap_or_default(), // MUTATION" \
        $LIB publish::tests::approving_a_held_picture_reposts_it_then_takes_the_note_down] \
    [list "long text: the head pushes the link to a second post" \
        $SAFE \
        "pub const ELISION_HEAD_CHARS: usize = 220;" \
        "pub const ELISION_HEAD_CHARS: usize = 260; // MUTATION" \
        $LIB publish::tests::approving_a_held_picture_reposts_it_then_takes_the_note_down] \
    [list "eval: a macro-only answer says ok (no output)" \
        $EVAL \
        "                        Some(\"art\" | \"quote\" | \"magick\")" \
        "                        Some(\"art\" | \"quote\") // MUTATION" \
        $LIB eval::more_routing_tests::a_bluesky_evaluation_carries_its_raw_output_in_an_envelope] \
]

proc run_cmd {argv} {
    set pre [expr {[info exists ::env(MUTCHECK_INHERIT_ENV)] ? "" : "direnv exec . "}]
    set f [open "| $pre$argv </dev/null 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    return $out
}

proc run_test {target filter} {
    set out [run_cmd "cargo test $target $filter"]
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {[regexp {process didn't exit successfully|\(signal: 6|SIGABRT} $out]} { return "RED" }
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} { return "NO-SUCH-TEST" }
    return [expr {[string match "*test result: FAILED*" $out] ? "RED" : "GREEN"}]
}

proc read_file {file} {
    set fh [open $file r]
    fconfigure $fh -encoding utf-8 -translation binary
    set s [read $fh]
    close $fh
    return [encoding convertfrom utf-8 $s]
}

proc write_file {file s} {
    set fh [open $file w]
    fconfigure $fh -translation binary
    puts -nonewline $fh [encoding convertto utf-8 $s]
    close $fh
}

proc apply_and_run {file find repl target filter} {
    set orig [read_file $file]
    if {[string first $find $orig] < 0} { return PATTERN-MISSING }
    if {$::dry} { return FOUND }
    write_file $file [string map [list $find $repl] $orig]
    set verdict [catch {run_test $target $filter} r]
    write_file $file $orig
    if {$verdict} { return "ERROR: $r" }
    return $r
}

set dry 0
if {[lindex $argv 0] eq "--dry"} { set dry 1; set argv [lrange $argv 1 end] }
set filter [lindex $argv 0]
set verified 0
set failures 0
foreach m $mutations {
    lassign $m name file find repl target test
    if {$filter ne "" && [string first $filter $name] < 0} { continue }
    puts -nonewline "[format %-72s $name] "
    flush stdout
    set r [apply_and_run $file $find $repl $target $test]
    puts $r
    if {$r eq "RED" || ($dry && $r eq "FOUND")} { incr verified } else { incr failures }
}
puts ""
puts "mutcheck slopdrop-art 2026-10-12: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
