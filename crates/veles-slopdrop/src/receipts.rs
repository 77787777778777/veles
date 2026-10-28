//! The ledger's delivery observer (PLAN-plugins-installer.md R5): a
//! public reply this module shaped carries its ledger entry id in the
//! outbound `meta` (`ledger.entry`); when the platform's plugin reports
//! the delivery, the receipt's ids and `hosted.*` meta are recorded on
//! THAT entry (and no other), so a later delete takes every piece down.

use std::collections::BTreeMap;

use veles_core::bus::DeliveryObserver;
use veles_types::bus::meta_keys;

use crate::withheld::{PostIds, WithheldStore};

pub struct Observer;

impl DeliveryObserver for Observer {
    fn delivered(
        &self,
        outbound_meta: &BTreeMap<String, String>,
        ids: &[String],
        receipt_meta: &BTreeMap<String, String>,
    ) {
        let Some(id) = outbound_meta.get(meta_keys::LEDGER_ENTRY) else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        let blobs = match (
            receipt_meta.get(meta_keys::HOSTED_BLOB_HTML),
            receipt_meta.get(meta_keys::HOSTED_BLOB_MD),
        ) {
            (Some(h), Some(d)) => Some((h.clone(), d.clone())),
            _ => None,
        };
        let posts = PostIds {
            post_uris: ids.to_vec(),
            blobs,
            link: receipt_meta.get(meta_keys::HOSTED_LINK).cloned(),
            short_id: receipt_meta.get(meta_keys::HOSTED_SHORT_ID).cloned(),
            record_rkey: receipt_meta.get(meta_keys::HOSTED_RECORD_RKEY).cloned(),
        };
        // A miss is logged, not fatal — the entry may have been deleted
        // between the post and the report.
        let store = WithheldStore::load();
        if let Err(e) = store.add_post(id, posts) {
            veles_core::module::log(
                tracing::Level::WARN,
                &format!(
                    "tcl: recording {} post id(s) on ledger entry {id}: {e}",
                    ids.len()
                ),
            );
            return;
        }
        // An approved picture's repost landed: the withheld post(s) it
        // replaced come down now, never before.
        let mut gone = Vec::new();
        let _ = store.update(id, |e| gone = std::mem::take(&mut e.superseded));
        if !gone.is_empty() {
            veles_core::rt::spawn(async move {
                let n = crate::publish::take_down_posts(&gone).await;
                veles_core::module::log(
                    tracing::Level::INFO,
                    &format!(
                        "tcl: an approved repost landed; {n} of {} withheld post(s) came down",
                        gone.len()
                    ),
                );
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::withheld::{PostKind, WithheldEntry};

    #[test]
    fn a_receipt_lands_on_the_entry_the_message_named() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let ws = WithheldStore::load();
        ws.insert(WithheldEntry {
            id: "cafe0001".into(),
            kind: PostKind::Withheld,
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            output: "x".into(),
            ..Default::default()
        })
        .unwrap();
        let mut om = BTreeMap::new();
        om.insert(meta_keys::LEDGER_ENTRY.to_string(), "cafe0001".to_string());
        let mut rm = BTreeMap::new();
        rm.insert(
            meta_keys::HOSTED_LINK.to_string(),
            "https://x/t/abc".to_string(),
        );
        rm.insert(meta_keys::HOSTED_SHORT_ID.to_string(), "abc".to_string());
        rm.insert(meta_keys::HOSTED_RECORD_RKEY.to_string(), "rk".to_string());
        Observer.delivered(&om, &["at://did/app.bsky.feed.post/p1".to_string()], &rm);
        let e = ws.get("cafe0001").unwrap();
        assert_eq!(e.post_uris, vec!["at://did/app.bsky.feed.post/p1"]);
        assert_eq!(e.link.as_deref(), Some("https://x/t/abc"));
        assert_eq!(e.short_ids, vec!["abc"]);
        assert_eq!(e.record_rkeys, vec!["rk"]);
        // A receipt with no ledger key is nobody's.
        Observer.delivered(&BTreeMap::new(), &["at://x".to_string()], &rm);
        assert_eq!(ws.get("cafe0001").unwrap().post_uris.len(), 1);
    }
}
