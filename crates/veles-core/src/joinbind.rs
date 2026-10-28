//! Metadata returned to a desktop client for a gateway-owned join.
//! The gateway's existing `join:gateway:v1:` binding is the only durable
//! execution record. Desktop-local return-view state is not stored here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub join_id: String,
    pub from_instance: String,
    pub from_session: String,
    /// Empty means the target gateway is local to the caller. A remote
    /// gateway does not store or authorize from a client-supplied address.
    #[serde(default)]
    pub target_instance: String,
    pub target_session: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_return_metadata_is_not_an_execution_record() {
        let b = Binding {
            join_id: "j-test".into(),
            from_instance: "laptop".into(),
            from_session: "mine".into(),
            target_instance: String::new(),
            target_session: "alice".into(),
        };
        let encoded = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<Binding>(&encoded).unwrap(), b);
        assert!(!encoded.contains("target_principal"));
        assert!(!encoded.contains("owner_platform"));
    }
}
