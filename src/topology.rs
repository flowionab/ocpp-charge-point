//! Shared helpers for translating between this crate's `(evse_id, connector_id)` addressing and
//! OCPP 1.6J's flat `connectorId` numbering (1-based across the whole charge point, no EVSE
//! concept). Every 1.6J adapter that addresses a specific connector on the wire needs this same
//! translation, in one direction or the other -
//! [`crate::availability::Ocpp1_6StatusNotifier`]/`crate::transactions`'s 1.6J adapter flatten
//! outbound reports; inbound CSMS requests (`UnlockConnector`, `ChangeAvailability`) need the
//! reverse - so it lives here once rather than being copied into each. See `docs/ROADMAP.md` §0.

/// Flattens this crate's `(evse_id, connector_id)` addressing into OCPP 1.6J's single flat
/// `connectorId` numbering: 1-based across the whole charge point, in `evse_id`/`connector_id`
/// order (`0` is reserved by the spec to mean the charge point itself, not any connector).
/// `connector_counts` is each EVSE's connector count, in `evse_id` order - `None` if
/// `evse_id`/`connector_id` doesn't address a real connector under that topology.
pub(crate) fn flatten_ocpp_1_6_connector_id(
    connector_counts: &[usize],
    evse_id: usize,
    connector_id: usize,
) -> Option<i64> {
    if connector_id >= *connector_counts.get(evse_id)? {
        return None;
    }
    let preceding: usize = connector_counts[..evse_id].iter().sum();
    i64::try_from(preceding + connector_id + 1).ok()
}

/// The reverse of [`flatten_ocpp_1_6_connector_id`]: given a CSMS-supplied flat 1.6J
/// `connectorId`, returns which `(evse_id, connector_id)` it addresses under `connector_counts`
/// (each EVSE's connector count, in `evse_id` order). `None` if `connector_id` is `0` (meaning
/// "the whole charge point," not any specific connector - callers that care about that case
/// should check for it before calling this) or doesn't address a real connector under the given
/// topology (negative, or past the last connector).
pub(crate) fn unflatten_ocpp_1_6_connector_id(
    connector_counts: &[usize],
    connector_id: i64,
) -> Option<(usize, usize)> {
    let mut remaining = usize::try_from(connector_id).ok()?.checked_sub(1)?;
    for (evse_id, &count) in connector_counts.iter().enumerate() {
        if remaining < count {
            return Some((evse_id, remaining));
        }
        remaining -= count;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{flatten_ocpp_1_6_connector_id, unflatten_ocpp_1_6_connector_id};

    #[test]
    fn flattening_numbers_connectors_sequentially_across_evses_starting_at_one() {
        let connector_counts = [2, 1, 3];

        assert_eq!(
            flatten_ocpp_1_6_connector_id(&connector_counts, 0, 0),
            Some(1)
        );
        assert_eq!(
            flatten_ocpp_1_6_connector_id(&connector_counts, 0, 1),
            Some(2)
        );
        assert_eq!(
            flatten_ocpp_1_6_connector_id(&connector_counts, 1, 0),
            Some(3)
        );
        assert_eq!(
            flatten_ocpp_1_6_connector_id(&connector_counts, 2, 0),
            Some(4)
        );
        assert_eq!(
            flatten_ocpp_1_6_connector_id(&connector_counts, 2, 2),
            Some(6)
        );
    }

    #[test]
    fn flattening_an_out_of_range_connector_id_is_none() {
        let connector_counts = [2, 1];

        assert_eq!(flatten_ocpp_1_6_connector_id(&connector_counts, 0, 2), None);
    }

    #[test]
    fn flattening_an_out_of_range_evse_id_is_none() {
        let connector_counts = [2, 1];

        assert_eq!(flatten_ocpp_1_6_connector_id(&connector_counts, 5, 0), None);
    }

    #[test]
    fn unflattening_is_the_inverse_of_flattening() {
        let connector_counts = [2, 1, 3];

        for evse_id in 0..connector_counts.len() {
            for connector_id in 0..connector_counts[evse_id] {
                let flat = flatten_ocpp_1_6_connector_id(&connector_counts, evse_id, connector_id)
                    .unwrap();
                assert_eq!(
                    unflatten_ocpp_1_6_connector_id(&connector_counts, flat),
                    Some((evse_id, connector_id))
                );
            }
        }
    }

    #[test]
    fn unflattening_zero_is_none() {
        let connector_counts = [2, 1];

        assert_eq!(unflatten_ocpp_1_6_connector_id(&connector_counts, 0), None);
    }

    #[test]
    fn unflattening_a_negative_id_is_none() {
        let connector_counts = [2, 1];

        assert_eq!(unflatten_ocpp_1_6_connector_id(&connector_counts, -1), None);
    }

    #[test]
    fn unflattening_past_the_last_connector_is_none() {
        let connector_counts = [2, 1];

        assert_eq!(unflatten_ocpp_1_6_connector_id(&connector_counts, 4), None);
    }
}
