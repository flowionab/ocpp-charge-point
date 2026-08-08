//! Moving a file between this charge point and somewhere else on the network - the one piece of
//! hardware surface that OCPP's Firmware Management (B3) and Diagnostics (B5) blocks both need,
//! and the reason `docs/PRODUCTION-ROADMAP.md` says to do those two together.
//!
//! # Why this is a hardware trait and not something this crate implements
//!
//! Every other network concern in this crate is delegated to `ocpp-client` (per `CLAUDE.md`), but
//! a file transfer is not an OCPP concern at all: OCPP hands over a bare URL and says nothing
//! about how to fetch it. The scheme is whatever the operator deployed - HTTPS, FTP/FTPS, SFTP,
//! or a vendor's own - and the credentials, trust store and network interface are the
//! integrator's. A charge point that hard-coded one HTTP client would be unable to talk to half
//! the deployments that exist.
//!
//! # Why the bytes never pass through this crate
//!
//! [`FileTransfer::download`] returns no content. A firmware image is megabytes and this crate
//! targets microcontrollers with kilobytes of RAM, so buffering one to hand it back would be
//! impossible on the hardware this is written for - and pointless besides, since the only thing
//! the crate would do with it is give it straight back to be flashed. The integrator streams it
//! wherever it belongs (a flash partition, an A/B slot, a filesystem) and this crate never sees
//! it. That is also why [`crate::hardware::Storage`] is not reused here: it is a key-value store
//! whose values are `Vec<u8>`, which is the wrong shape for something that must never be resident
//! in full.
//!
//! Upload is the asymmetric one, because the two things a charge point uploads have different
//! owners - see [`UploadSource`].

use alloc::boxed::Box;
use alloc::string::String;

/// Which of a charge point's logs an upload is asking for, mirroring OCPP's `LogEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// The diagnostics log - whatever the integrator considers diagnostic output. 1.6J's
    /// `GetDiagnostics` has no other kind, so it always means this one.
    Diagnostics,
    /// The security log (OCPP A04.FR.04). This crate owns its contents - see
    /// [`crate::security::SecurityEventLog`] - so it arrives as [`UploadSource::Bytes`] rather
    /// than being read from the integrator.
    Security,
    /// 2.1's data-collector log. Integrator-owned, like [`Self::Diagnostics`].
    DataCollector,
}

/// Where the bytes of an upload come from.
///
/// The split is not a convenience - the two things a charge point uploads genuinely have
/// different owners. The **security log** is this crate's: it is bounded (50 entries by default,
/// tens of KB), lives in [`crate::security::SecurityEventLog`], and no integrator can produce it
/// because no integrator is told when a security event happens. A **diagnostics log** is the
/// integrator's: this crate has no idea what a given charge point considers diagnostic output,
/// and it may be far too large to hold in memory.
///
/// So one arrives as bytes and the other as a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadSource<'a> {
    /// Content this crate is handing over, already rendered. Bounded by construction.
    Bytes(&'a [u8]),
    /// A log the integrator holds and streams itself. This crate never sees the content, for the
    /// same reason it never sees a downloaded firmware image.
    Local(LogKind),
}

/// A callback for reporting how far a transfer has got.
///
/// Progress is *pushed by the integrator* rather than polled, the same shape
/// [`crate::state::ConnectorEvent::MeterValueSampled`] uses for metering: only the code doing the
/// transfer knows how far it is, and a charge point should not be spinning a timer to ask.
///
/// Reporting is entirely optional. A transfer that reports nothing until it returns is still
/// correct - OCPP's `Downloading`/`Downloaded`/`DownloadFailed` transitions are derivable from
/// the call being in flight and its `Result`, and this crate derives them that way (B3.2). What
/// progress adds is the *percentage* 2.x's `PublishFirmwareStatusNotification` carries, and a
/// slow link's reassurance that something is still happening.
pub struct TransferProgress<'a> {
    #[allow(clippy::type_complexity)]
    report: Option<&'a (dyn Fn(TransferReport) + Send + Sync)>,
}

/// One progress report from an in-flight transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferReport {
    /// Bytes moved so far.
    pub transferred_bytes: u64,
    /// The total the transfer expects to move, if it knows. `None` is normal and not a failure -
    /// a server that sends no `Content-Length` leaves a downloader genuinely unable to say.
    pub total_bytes: Option<u64>,
}

impl TransferReport {
    /// How far along this transfer is, as a whole percentage, or `None` when the total is
    /// unknown.
    ///
    /// Clamped to 100: a server whose declared total turns out to be short should not produce a
    /// percentage OCPP has no field wide enough for, and "we are past what was promised" is not
    /// something a progress bar can usefully say anyway.
    pub fn percent(&self) -> Option<u8> {
        let total = self.total_bytes?;
        if total == 0 {
            // A zero-length transfer is complete the moment it starts; dividing by it is not.
            return Some(100);
        }
        // Clamp *before* scaling, so the arithmetic below only ever sees a real fraction.
        let transferred = self.transferred_bytes.min(total);
        let percent = match transferred.checked_mul(100) {
            Some(scaled) => scaled / total,
            // Only reachable for a total above ~1.8×10^17 bytes, which no firmware image will
            // ever be - but `saturating_mul` here would be worse than an overflow, because it
            // silently returns a *wrong* percentage rather than a wrong-and-obvious one. Dividing
            // first loses at most a percent and cannot overflow: `total / 100` is non-zero
            // exactly when the multiply would have overflowed.
            None => transferred / (total / 100),
        };
        Some(percent.min(100) as u8)
    }
}

impl<'a> TransferProgress<'a> {
    /// A progress handle that forwards every report to `report`.
    pub fn new(report: &'a (dyn Fn(TransferReport) + Send + Sync)) -> Self {
        Self {
            report: Some(report),
        }
    }

    /// A progress handle that discards every report - for a caller that has nowhere to send them,
    /// so an integrator can call [`Self::report`] unconditionally rather than branching.
    pub fn ignored() -> Self {
        Self { report: None }
    }

    /// Reports that `transferred_bytes` of `total_bytes` have moved.
    ///
    /// Cheap and non-blocking, deliberately: an integrator should feel free to call this from the
    /// middle of a read loop, and one that had to await here would be paying for progress out of
    /// its transfer throughput.
    pub fn report(&self, transferred_bytes: u64, total_bytes: Option<u64>) {
        if let Some(report) = self.report {
            report(TransferReport {
                transferred_bytes,
                total_bytes,
            });
        }
    }
}

impl core::fmt::Debug for TransferProgress<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TransferProgress")
            .field("reporting", &self.report.is_some())
            .finish()
    }
}

/// Fetching a file from a URL, and sending one to a URL.
///
/// Implemented by the integrator. See the module docs for why this is hardware surface rather than
/// something this crate does itself, and why no content crosses the boundary on the way down.
///
/// # Error handling
///
/// Both methods are fallible and, per `CLAUDE.md`, expected to be: a URL may not resolve, a
/// server may reject credentials, flash may be full, a link may drop halfway. A failure must be
/// returned rather than panicking - callers turn it into the protocol-correct status
/// (`DownloadFailed`, `UploadFailure`) and, where OCPP asks for it, retry. **Retries are the
/// caller's**, not the implementor's: OCPP carries `retries`/`retryInterval` on the requests that
/// start a transfer, so honouring them here as well would multiply out to a retry count no CSMS
/// asked for.
///
/// # Cancellation
///
/// A caller may drop the returned future - OCPP lets a CSMS supersede a transfer with a new
/// request, and 2.1's `AcceptedCanceled` says as much explicitly. An implementation should be
/// safe to cancel at an await point, leaving no half-written artifact that a later download would
/// mistake for a complete one.
#[async_trait::async_trait]
pub trait FileTransfer {
    /// The error type returned by a failed transfer.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Fetches `url` and stores it locally, wherever this hardware keeps a pending firmware image.
    ///
    /// Returns nothing on success: the content stays with the implementor (see the module docs).
    /// Whether the image is *valid* is a separate question this crate cannot answer - signature
    /// and certificate verification needs crypto and a trust store the integrator owns, and is
    /// `docs/PRODUCTION-ROADMAP.md` B3.3.
    async fn download(&self, url: &str, progress: &TransferProgress<'_>)
    -> Result<(), Self::Error>;

    /// Sends `source` to `url`.
    ///
    /// See [`UploadSource`] for why the content sometimes arrives as bytes and sometimes as a
    /// name.
    async fn upload(
        &self,
        url: &str,
        source: UploadSource<'_>,
        progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: FileTransfer + Send + Sync + ?Sized> FileTransfer for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn download(
        &self,
        url: &str,
        progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        (**self).download(url, progress).await
    }

    async fn upload(
        &self,
        url: &str,
        source: UploadSource<'_>,
        progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        (**self).upload(url, source, progress).await
    }
}

/// A [`FileTransfer`] for charge points that cannot transfer files at all, mirroring
/// [`NoStorage`](crate::hardware::NoStorage).
///
/// Every operation fails with [`NoFileTransferError`]. That is deliberately *not* the same as
/// succeeding silently: a charge point that reported `Downloaded` without downloading anything
/// would have a CSMS believing an update is staged and about to be installed. Failing is the
/// honest answer, and it maps onto the statuses OCPP already has for it.
///
/// An integrator with no transfer capability should also leave
/// [`Capabilities::firmware_management`](crate::hardware::Capabilities::firmware_management) and
/// [`Capabilities::diagnostics`](crate::hardware::Capabilities::diagnostics) `false`, so the
/// handlers are never registered and the CSMS is told up front rather than per request.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFileTransfer;

/// The error every [`NoFileTransfer`] operation returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoFileTransferError {
    /// What was attempted, for the log.
    pub attempted: String,
}

impl core::fmt::Display for NoFileTransferError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "this charge point has no file-transfer capability, so it cannot {}",
            self.attempted
        )
    }
}

impl core::error::Error for NoFileTransferError {}

#[async_trait::async_trait]
impl FileTransfer for NoFileTransfer {
    type Error = NoFileTransferError;

    async fn download(
        &self,
        _url: &str,
        _progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        Err(NoFileTransferError {
            attempted: "download a file".into(),
        })
    }

    async fn upload(
        &self,
        _url: &str,
        _source: UploadSource<'_>,
        _progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        Err(NoFileTransferError {
            attempted: "upload a file".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

    #[test]
    fn percent_is_none_until_the_total_is_known() {
        // Not zero, which would read as "no progress" - a downloader that cannot know the total
        // is a normal case (no `Content-Length`), not a stalled one.
        assert_eq!(
            TransferReport {
                transferred_bytes: 4_096,
                total_bytes: None
            }
            .percent(),
            None
        );
    }

    #[test]
    fn percent_tracks_the_transfer_and_clamps_at_a_hundred() {
        let at = |transferred, total| {
            TransferReport {
                transferred_bytes: transferred,
                total_bytes: Some(total),
            }
            .percent()
        };

        assert_eq!(at(0, 200), Some(0));
        assert_eq!(at(100, 200), Some(50));
        assert_eq!(at(200, 200), Some(100));
        // A server whose declared total was short. OCPP has no field for 150%.
        assert_eq!(at(300, 200), Some(100));
        // A zero-length file is complete on arrival; dividing by its length is not.
        assert_eq!(at(0, 0), Some(100));
    }

    #[test]
    fn percent_stays_correct_on_a_transfer_too_large_to_scale_directly() {
        // `transferred × 100` overflows a u64 above ~1.8e17, and the tempting fix - a saturating
        // multiply - is worse than the overflow: it silently reports 1% for a finished transfer.
        assert_eq!(
            TransferReport {
                transferred_bytes: u64::MAX,
                total_bytes: Some(u64::MAX),
            }
            .percent(),
            Some(100)
        );
        assert_eq!(
            TransferReport {
                transferred_bytes: u64::MAX / 2,
                total_bytes: Some(u64::MAX),
            }
            .percent(),
            Some(50),
            "dividing before scaling loses at most a percent, which is the price of not \
             overflowing - it must not turn a half-done transfer into something else"
        );
    }

    #[test]
    fn an_ignored_progress_handle_swallows_reports_rather_than_requiring_a_branch() {
        let progress = TransferProgress::ignored();
        progress.report(1, Some(2));
        progress.report(2, Some(2));
    }

    #[test]
    fn a_reporting_handle_forwards_every_report_in_order() {
        let seen: BlockingMutex<CriticalSectionRawMutex, RefCell<alloc::vec::Vec<TransferReport>>> =
            BlockingMutex::new(RefCell::new(alloc::vec::Vec::new()));
        let record = |report: TransferReport| {
            seen.lock(|seen| seen.borrow_mut().push(report));
        };

        let progress = TransferProgress::new(&record);
        progress.report(0, Some(10));
        progress.report(10, Some(10));

        seen.lock(|seen| {
            let seen = seen.borrow();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[0].percent(), Some(0));
            assert_eq!(seen[1].percent(), Some(100));
        });
    }

    #[tokio::test]
    async fn a_charge_point_without_file_transfer_fails_rather_than_pretending() {
        // The failure mode this guards: a CSMS told `Downloaded` by a charge point that
        // downloaded nothing would go on to install an update that does not exist.
        let transfer = NoFileTransfer;

        let downloaded = transfer
            .download(
                "https://example.invalid/fw.bin",
                &TransferProgress::ignored(),
            )
            .await;
        let uploaded = transfer
            .upload(
                "https://example.invalid/logs",
                UploadSource::Local(LogKind::Diagnostics),
                &TransferProgress::ignored(),
            )
            .await;

        assert!(downloaded.is_err());
        assert!(uploaded.is_err());
    }

    /// A deliberately realistic implementor: streams in chunks, reports as it goes, and keeps the
    /// content to itself. Written to check the *shape* works - that an integrator can hold state,
    /// await between chunks, and report progress without fighting a lifetime.
    struct ChunkedTransfer {
        chunk_size: u64,
        total: u64,
        stored: BlockingMutex<CriticalSectionRawMutex, RefCell<u64>>,
        uploaded: BlockingMutex<CriticalSectionRawMutex, RefCell<alloc::vec::Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl FileTransfer for ChunkedTransfer {
        type Error = NoFileTransferError;

        async fn download(
            &self,
            _url: &str,
            progress: &TransferProgress<'_>,
        ) -> Result<(), Self::Error> {
            let mut written = 0;
            while written < self.total {
                // An await between chunks: this is where a cancelled transfer would stop, and the
                // trait's contract says stopping here must not leave a usable artifact behind.
                tokio::task::yield_now().await;
                written = (written + self.chunk_size).min(self.total);
                self.stored.lock(|stored| *stored.borrow_mut() = written);
                progress.report(written, Some(self.total));
            }
            Ok(())
        }

        async fn upload(
            &self,
            _url: &str,
            source: UploadSource<'_>,
            progress: &TransferProgress<'_>,
        ) -> Result<(), Self::Error> {
            match source {
                UploadSource::Bytes(bytes) => {
                    self.uploaded
                        .lock(|uploaded| uploaded.borrow_mut().extend_from_slice(bytes));
                    progress.report(bytes.len() as u64, Some(bytes.len() as u64));
                    Ok(())
                }
                // Nothing local to send in this fake, which is itself the honest answer for a
                // charge point asked for a log it does not keep.
                UploadSource::Local(_) => Err(NoFileTransferError {
                    attempted: "upload a log this hardware does not keep".into(),
                }),
            }
        }
    }

    #[tokio::test]
    async fn a_streaming_implementor_can_report_progress_without_handing_over_content() {
        let transfer = alloc::sync::Arc::new(ChunkedTransfer {
            chunk_size: 25,
            total: 100,
            stored: BlockingMutex::new(RefCell::new(0)),
            uploaded: BlockingMutex::new(RefCell::new(alloc::vec::Vec::new())),
        });

        let seen: BlockingMutex<CriticalSectionRawMutex, RefCell<alloc::vec::Vec<u8>>> =
            BlockingMutex::new(RefCell::new(alloc::vec::Vec::new()));
        let record = |report: TransferReport| {
            if let Some(percent) = report.percent() {
                seen.lock(|seen| seen.borrow_mut().push(percent));
            }
        };

        // Through an `Arc`, because every caller in this crate shares hardware that way.
        let shared: alloc::sync::Arc<ChunkedTransfer> = transfer.clone();
        shared
            .download(
                "https://example.invalid/fw.bin",
                &TransferProgress::new(&record),
            )
            .await
            .unwrap();

        seen.lock(|seen| assert_eq!(*seen.borrow(), alloc::vec![25, 50, 75, 100]));
        // The content stayed with the integrator - the crate never received a byte of it.
        transfer
            .stored
            .lock(|stored| assert_eq!(*stored.borrow(), 100));
    }

    #[tokio::test]
    async fn the_crates_own_log_bytes_reach_an_uploader_while_a_local_log_stays_a_name() {
        let transfer = ChunkedTransfer {
            chunk_size: 8,
            total: 8,
            stored: BlockingMutex::new(RefCell::new(0)),
            uploaded: BlockingMutex::new(RefCell::new(alloc::vec::Vec::new())),
        };

        transfer
            .upload(
                "https://example.invalid/logs",
                UploadSource::Bytes(b"security log"),
                &TransferProgress::ignored(),
            )
            .await
            .unwrap();
        transfer.uploaded.lock(|uploaded| {
            assert_eq!(uploaded.borrow().as_slice(), b"security log");
        });

        // The other arm carries no content at all - which is the whole point of the split.
        assert!(
            transfer
                .upload(
                    "https://example.invalid/logs",
                    UploadSource::Local(LogKind::Diagnostics),
                    &TransferProgress::ignored(),
                )
                .await
                .is_err()
        );
    }
}
