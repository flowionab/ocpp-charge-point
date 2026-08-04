/// A single meter reading, sampled by hardware and pushed in via
/// `ConnectorEvent::MeterValueSampled`. Only the energy register is modeled for now (the
/// measurand nearly every billing flow actually needs) - see `docs/ROADMAP.md` §10 for the
/// rest (power, current, voltage, SoC, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterSample {
    /// Cumulative active energy imported by the EV, in Wh (OCPP `Energy.Active.Import.Register`).
    pub energy_wh: i64,
}
