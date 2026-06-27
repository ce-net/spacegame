//! Host-side verification of the node-signed account vouch a client presents in its [`Join`].
//!
//! "Your local node is your login" (Leif): a browser asks the CE node it is running to vouch for it,
//! and the node mints a capability (`ce-iam nodeauth`) whose abilities carry the binding —
//! `account:login`, `account:peer:<peerId>`, `account:name:<name>`. The client then presents that cap
//! in its [`crate::wire::ClientMsg::Join`]. This module verifies it on the sector host.
//!
//! The trust model is FEDERATED: any CE node may vouch for its own users (it is a root only for the
//! accounts it issues), so the host accepts the cap's own root issuer as the authority for that one
//! vouch. What the host gains is a cryptographic statement "a CE node vouches this player's name" —
//! the basis for a verified-identity badge, name reservation, and later trust/anti-abuse policy.
//!
//! This is mesh-only (the `mesh` feature): it runs on the authoritative host at ingest, NOT inside the
//! deterministic [`crate::room::apply_client_msg`] every replica runs. A failed/absent cap never blocks
//! play — the game stays open; the vouch is additive.
//!
//! NOTE (one live-integration follow-up): we do not yet hard-bind the cap's `account:peer:<peerId>` to
//! the authenticated mesh sender `from`, because the in-tab peer's id and the relay-bridged `from` can
//! differ in encoding. Once the in-tab peer's id reaches the host verbatim as `from`, add a third
//! `authorize(..., &ability_peer(from), ...)` check here to make the vouch sender-bound.

use ce_cap::{authorize, decode_chain};
use std::time::{SystemTime, UNIX_EPOCH};

/// A CE node id — the 32-byte Ed25519 public key (the `ce_identity::NodeId` alias, inlined to avoid a
/// dependency on ce-identity here).
type NodeId = [u8; 32];

/// A verified vouch: which node vouched, and the name it bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vouch {
    /// The vouching CE node id (hex) — the account's authority.
    pub node: String,
    /// The display name the node vouched for.
    pub name: String,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The ability binding a display name into a vouch cap — mirror of `ce_iam::ability_name`.
fn ability_name(name: &str) -> String {
    format!("account:name:{name}")
}

/// Verify a node-signed vouch capability binds `name`. Returns the [`Vouch`] on success, or an error
/// string safe to log. `name` is the name the client claims in its Join — it must match the name the
/// node signed, so a peer cannot claim a name it was not vouched for.
pub fn verify_join(name: &str, cap: &str) -> Result<Vouch, String> {
    let chain = decode_chain(cap).map_err(|e| format!("malformed vouch cap: {e}"))?;
    let root: NodeId = chain.first().ok_or("empty vouch chain")?.cap.issuer;
    let leaf_audience: NodeId = chain.last().ok_or("empty vouch chain")?.cap.audience;
    let now = now_secs();
    let never_revoked = |_: &NodeId, _: u64| false;
    // The vouching node is the accepted root for its own users (federated identity). The cap's resource
    // is `Any`, so `self_id`/tags do not gate it — pass the root as a valid placeholder self id.
    authorize(
        &root,
        &[root],
        &[],
        now,
        &leaf_audience,
        ce_iam_login_ability(),
        &chain,
        &never_revoked,
    )?;
    authorize(
        &root,
        &[root],
        &[],
        now,
        &leaf_audience,
        &ability_name(name),
        &chain,
        &never_revoked,
    )?;
    Ok(Vouch { node: hex::encode(root), name: name.to_string() })
}

/// The login ability string — mirror of `ce_iam::ABILITY_LOGIN`.
fn ce_iam_login_ability() -> &'static str {
    "account:login"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage_cap() {
        assert!(verify_join("Ada", "not-hex").is_err());
        assert!(verify_join("Ada", "").is_err());
    }
}
