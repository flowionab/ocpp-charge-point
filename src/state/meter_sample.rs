/// A single meter reading, sampled by hardware and pushed in via
/// `ConnectorEvent::MeterValueSampled`. Only `energy_wh` is required - a hardware integration
/// that can't measure a given quantity simply leaves it `None`, and the wire adapter (see
/// `docs/ROADMAP.md` §10) omits it from the reported `meterValue` rather than fabricating one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct MeterSample {
    /// Cumulative active energy imported by the EV, in Wh (OCPP `Energy.Active.Import.Register`).
    pub energy_wh: i64,
    /// Active power currently imported by the EV, in W (OCPP `Power.Active.Import`).
    pub power_w: Option<i64>,
    /// Current currently imported by the EV, in mA (OCPP `Current.Import`) - milliamps rather
    /// than whole amps for enough resolution to be useful at typical EV charging currents.
    pub current_ma: Option<i64>,
    /// Voltage present at the connector, in V (OCPP `Voltage`).
    pub voltage_v: Option<i64>,
    /// The EV's state of charge, as a percentage 0-100 (OCPP `SoC`) - only reported by vehicles
    /// that expose it (e.g. over ISO 15118), so hardware without that capability leaves it
    /// `None`.
    pub soc_percent: Option<u8>,
}
