//! `ChallengeMap`: the pending HTTP-01 challenges the `:80` listener answers (ADR-0015,
//! architecture §7.2).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

/// Token → key authorization for every HTTP-01 challenge in flight. Cloning shares the map.
///
/// The ACME task inserts a token before it tells the CA the challenge is ready and removes it
/// once the order is done; the `:80` service reads it on every
/// `/.well-known/acme-challenge/<token>` request. Like [`CertMap`](super::CertMap), every write
/// publishes a new snapshot, so reads are lock-free and never see a partial update.
#[derive(Debug, Clone, Default)]
pub struct ChallengeMap {
    snapshot: Arc<ArcSwap<HashMap<String, Arc<str>>>>,
}

impl ChallengeMap {
    /// Answers `token` with `key_authorization` from now on, replacing any earlier answer.
    pub fn insert(&self, token: impl Into<String>, key_authorization: impl Into<Arc<str>>) {
        let (token, key_authorization) = (token.into(), key_authorization.into());
        self.snapshot.rcu(|map| {
            let mut map = HashMap::clone(map);
            map.insert(token.clone(), Arc::clone(&key_authorization));
            map
        });
    }

    /// Stops answering `token`.
    pub fn remove(&self, token: &str) {
        self.snapshot.rcu(|map| {
            let mut map = HashMap::clone(map);
            map.remove(token);
            map
        });
    }

    /// The key authorization for `token`, if a challenge for it is in flight. Tokens are
    /// case-sensitive (base64url).
    pub fn get(&self, token: &str) -> Option<Arc<str>> {
        self.snapshot.load().get(token).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserted_tokens_are_answered_until_removed_by_every_clone() {
        let challenges = ChallengeMap::default();
        let reader = challenges.clone();
        assert!(reader.get("tok").is_none());

        challenges.insert("tok", "tok.thumbprint");
        challenges.insert("other", "other.thumbprint");
        assert_eq!(reader.get("tok").as_deref(), Some("tok.thumbprint"));
        assert!(reader.get("TOK").is_none(), "tokens are case-sensitive");

        challenges.insert("tok", "tok.renewed");
        assert_eq!(reader.get("tok").as_deref(), Some("tok.renewed"));

        challenges.remove("tok");
        assert!(reader.get("tok").is_none());
        assert_eq!(reader.get("other").as_deref(), Some("other.thumbprint"));
        challenges.remove("missing");
    }
}
