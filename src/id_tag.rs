//! Shared helper for mapping this crate's [`crate::state::IdToken`] to OCPP 1.6J's `IdTag` - a
//! bare 20-byte-bounded string with no type/kind metadata at all, unlike every later version's
//! richer `IdTokenType`. Every 1.6J adapter that sends an identifier on the wire needs this same
//! mapping (`Authorize`, `StartTransaction`/`StopTransaction`, and eventually `ReserveNow`/
//! `RemoteStartTransaction`), so it lives here once rather than being copied into each. See
//! `docs/ROADMAP.md` §0.

use crate::state::{IdToken, IdTokenKind};
use ocpp_client::ocpp_types::v16::IdTag;

/// The largest byte-boundary-safe prefix of `value` no longer than `max_bytes`.
fn truncate_to_byte_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Maps a presented identifier to 1.6J's `IdTag`, truncating to fit its 20-byte bound if needed
/// (1.6J's is much tighter than later versions' `IdTokenType.idToken`, up to 255 bytes) and
/// falling back to an empty tag if `id_token` is absent altogether - which shouldn't happen
/// through this crate's own state machine, but the types it flows through (e.g.
/// [`crate::state::Transaction::id_token`]) aren't shaped to make that impossible. 1.6J's `IdTag`
/// carries no type/kind at all, so [`crate::state::IdTokenKind`] is dropped entirely - there's no
/// wire field to put it in.
pub(crate) fn map_id_tag(id_token: Option<&IdToken>) -> IdTag {
    let value = id_token.map(|token| token.value.as_str()).unwrap_or("");
    let truncated = truncate_to_byte_boundary(value, 20);
    IdTag::try_from(truncated)
        .unwrap_or_else(|()| IdTag::try_from("").expect("empty string always fits IdTag"))
}

/// The reverse of [`map_id_tag`]: wraps a CSMS-supplied `IdTag` back into this crate's
/// [`IdToken`]. 1.6J's `IdTag` carries no type/kind at all, so there's no wire information to
/// recover [`IdTokenKind`] from - every 1.6J adapter that receives an identifier this way
/// (`RemoteStartTransaction`, and eventually `ReserveNow`) picks [`IdTokenKind::Central`], the
/// closest fit for "an identifier the CSMS itself is presenting on the token's behalf" rather
/// than one read directly off physical hardware.
pub(crate) fn map_id_token(id_tag: &IdTag) -> IdToken {
    IdToken {
        value: id_tag.as_str().into(),
        kind: IdTokenKind::Central,
    }
}

#[cfg(test)]
mod tests {
    use super::{map_id_tag, map_id_token};
    use crate::state::{IdToken, IdTokenKind};

    fn id_token(value: &str) -> IdToken {
        IdToken {
            value: value.into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    #[test]
    fn a_short_id_token_maps_unchanged() {
        assert_eq!(map_id_tag(Some(&id_token("04A224B2"))).as_str(), "04A224B2");
    }

    #[test]
    fn an_id_token_over_twenty_bytes_is_truncated() {
        let token = id_token(&"a".repeat(30));

        let mapped = map_id_tag(Some(&token));

        assert_eq!(mapped.as_str().len(), 20);
        assert_eq!(mapped.as_str(), "a".repeat(20));
    }

    #[test]
    fn a_missing_id_token_maps_to_an_empty_tag() {
        assert_eq!(map_id_tag(None).as_str(), "");
    }

    #[test]
    fn an_id_tag_maps_back_to_an_id_token_with_a_central_kind() {
        let id_tag = ocpp_client::ocpp_types::v16::IdTag::try_from("04A224B2").unwrap();

        assert_eq!(
            map_id_token(&id_tag),
            IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::Central,
            }
        );
    }
}
