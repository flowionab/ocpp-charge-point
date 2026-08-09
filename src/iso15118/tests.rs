//! Version-independent tests for `Get15118EVCertificate`'s shared refusal/delivery logic. Each
//! version adapter's own wire-shape tests live alongside it (`ocpp_2_0_1.rs`/`ocpp_2_1.rs`).

use super::*;
use crate::hardware::{Capabilities, Iso15118SupportLevel, NoIso15118Controller};

#[test]
fn refused_locally_carries_no_payload() {
    let result = refused_locally();
    assert_eq!(result.status, Iso15118CertificateStatus::Failed);
    assert_eq!(result.exi_response, "");
    assert_eq!(result.remaining_contracts, None);
}

#[test]
fn iso15118_supported_is_false_only_for_none() {
    for level in [
        Iso15118SupportLevel::None,
        Iso15118SupportLevel::Iso15118_2,
        Iso15118SupportLevel::Iso15118_20,
    ] {
        let capabilities = Capabilities {
            iso15118_support: level,
            ..Capabilities::default()
        };
        assert_eq!(
            iso15118_supported(&capabilities),
            level != Iso15118SupportLevel::None
        );
    }
}

#[tokio::test]
async fn delivering_to_no_controller_logs_rather_than_panics() {
    // The whole point of `NoIso15118Controller` is that this never panics or blocks - it just
    // reports (via `tracing`, asserted indirectly by this simply completing) that nothing was
    // there to deliver to.
    deliver(&NoIso15118Controller, &refused_locally()).await;
}
