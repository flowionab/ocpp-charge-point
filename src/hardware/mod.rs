//! The hardware abstraction layer: the traits an integrator implements to plug real (or
//! simulated) hardware into this crate, and the small pieces of glue connecting them to the
//! charge point's actor. See [`ChargePoint`] for the entry point.
//!
//! Per `CLAUDE.md`, this is the crate's primary - ideally *only* - integration surface:
//! protocol handling, state machines, transaction lifecycle, and networking are this crate's own
//! responsibility, not something an integrator needs to touch.

mod capabilities;
mod certificate;
mod charge_point;
mod command_executor;
mod command_receiver;
mod connector;
mod display;
mod event_sender;
mod evse;
#[cfg(test)]
mod fault_injection;
mod file_transfer;
mod firmware;
mod key_storage;
mod storage;
mod watchdog;

pub use self::capabilities::{
    CAPABILITY_GATES, Capabilities, CapabilityGate, Iso15118SupportLevel,
    supported_feature_profiles_1_6, warn_on_feature_mismatches,
};
pub use self::certificate::{
    CertificateHashData, CertificateStore, CertificateUse, DEFAULT_MAX_CERTIFICATES,
    DeleteCertificateOutcome, HashAlgorithm, InstallCertificateOutcome, InstalledCertificate,
    NoCertificateStore, NoCertificateStoreError, StoredCertificates, StoredCertificatesError,
};
pub use self::charge_point::ChargePoint;
pub use self::command_executor::execute_hardware_command;
pub use self::command_receiver::HardwareCommandReceiver;
pub use self::connector::Connector;
pub use self::display::{Display, NoDisplay, NoDisplayError};
pub use self::event_sender::HardwareEventSender;
pub use self::evse::Evse;
pub use self::file_transfer::{
    FileTransfer, LogKind, NoFileTransfer, NoFileTransferError, TransferProgress, TransferReport,
    UploadSource,
};
pub use self::firmware::{
    FirmwareInstallOutcome, FirmwareInstaller, NoFirmwareInstaller, NoFirmwareInstallerError,
};
pub use self::key_storage::{
    DEFAULT_MAX_KEYS, GeneratedKeyPair, KeyHandle, KeyStore, KeyStoreBacking, NoKeyStore,
    NoKeyStoreError, PublicKey, SignatureAlgorithm, SoftKeyStore, SoftKeyStoreError,
    SoftwareCrypto,
};
pub use self::storage::{AtomicStorage, NoStorage, NoStorageError, Storage};
#[cfg(feature = "std")]
pub use self::storage::{InMemoryStorage, InMemoryStorageError};
pub use self::watchdog::{NoWatchdog, Watchdog};
