//! The tariff store - default tariffs installed per EVSE (or charge-point-wide), the state behind
//! OCPP 2.1's `SetDefaultTariff`/`ClearTariffs`/`GetTariffs`, and the priced structure a charge
//! point needs to calculate a cost from one locally (I07-I12). See `docs/ROADMAP.md` §9 and
//! `docs/PRODUCTION-ROADMAP.md` B7.1.
//!
//! **2.1-only.** 1.6J and 2.0.1 have no tariff messages at all - a spec boundary, not a gap - so
//! this model exists purely for the 2.1 adapter (`crate::tariff::ocpp_2_1`) to project onto; there
//! is nothing to downgrade.
//!
//! A tariff assigned to one running transaction (OCPP `ChangeTransactionTariff`) is deliberately
//! **not** modeled here. It lives on [`crate::state::EvseState::transaction_tariffs`] instead,
//! indexed the same way [`crate::state::EvseState::running_costs`] already is for `CostUpdated` -
//! cleared the moment the transaction it was assigned to starts or ends, so a new session on the
//! same connector never inherits a previous driver's tariff. See `crate::tariff`.
//!
//! # Money is not `f64` here
//!
//! Every price in this module is a fixed-point [`Money`] or [`TaxPercent`], never a float. The
//! wire carries OCPP `decimal`s, which `ocpp-types` renders as `f64`; that conversion happens once
//! at the adapter boundary ([`Money::from_decimal`]) and never again. A cost is then a chain of
//! multiplications and divisions over `i64`/`i128`, so the same tariff and the same meter readings
//! produce the same total on every target, and `Tariff` can derive `Eq` - which a model carrying
//! `f64` could not. See [`crate::pricing`] for the arithmetic itself.

use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, NaiveDate, Utc, Weekday};

/// A tariff's CSMS-assigned identifier (OCPP `tariffId`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TariffId(pub String);

/// A monetary amount, in millionths of one unit of the tariff's currency ("micro-units").
///
/// Six decimal places is what OCPP's own examples need and more: the specification prices energy
/// per kWh and time per minute at two to three decimals (`0.25`, `0.909`), and a per-minute price
/// is divided by 60 before it ever becomes a cost. Six places carries that division without the
/// result collapsing to zero, while `i64` still spans ±9.2×10⁶ currency units - about seven orders
/// of magnitude more than the most expensive charging session anyone will have.
///
/// Deliberately not a float. See this module's docs.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct Money(pub i64);

impl Money {
    /// Nothing.
    pub const ZERO: Self = Self(0);

    /// Micro-units per whole currency unit.
    pub const SCALE: i64 = 1_000_000;

    /// Converts an OCPP wire `decimal` to a fixed-point amount, or `None` if it is not finite or
    /// does not fit.
    ///
    /// Rounds to the **nearest** micro-unit rather than truncating, because this is a
    /// *representation* conversion, not a cost calculation: the CSMS sent an exact decimal
    /// (`0.29`), JSON parsing turned it into the nearest `f64` (`0.28999999999999998`), and
    /// truncating there would silently shave a micro-unit off a price the operator wrote down
    /// exactly. Rounding a *cost* is a different question and is answered the other way - see
    /// [`crate::pricing`].
    pub fn from_decimal(value: f64) -> Option<Self> {
        fixed_from_decimal(value, Self::SCALE).map(Self)
    }

    /// The amount as an OCPP wire `decimal`. Exact for every value this crate produces: an `f64`
    /// carries 53 bits of mantissa, and a micro-unit total below 9×10¹⁵ is representable to the
    /// unit.
    pub fn to_decimal(self) -> f64 {
        self.0 as f64 / Self::SCALE as f64
    }

    /// Saturating addition - a total that somehow overflowed `i64` micro-units is clamped rather
    /// than wrapped, because wrapping would turn a huge overcharge into a refund.
    pub fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

/// Converts an OCPP wire `decimal` into a fixed-point integer with `scale` steps per unit,
/// rounding to nearest, or `None` if it is not finite or does not fit.
///
/// Nearest rather than truncating for the reason [`Money::from_decimal`] documents: this is the
/// one place a `decimal` the CSMS wrote exactly meets binary floating point, and losing a step
/// here would be losing it from the operator's own figure rather than from a calculation.
fn fixed_from_decimal(value: f64, scale: i64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let scaled = value * scale as f64;
    // `i64::MAX as f64` rounds *up* to 2^63, so compare against a bound `as i64` cannot saturate
    // on rather than against `i64::MAX` itself.
    if !(-9.223_372_036_854_775e18..=9.223_372_036_854_775e18).contains(&scaled) {
        return None;
    }
    // `f64::round` lives in `std`; this crate is `no_std`. Truncate and correct the half.
    let truncated = scaled as i64;
    let fraction = scaled - truncated as f64;
    Some(if fraction >= 0.5 {
        truncated.saturating_add(1)
    } else if fraction <= -0.5 {
        truncated.saturating_sub(1)
    } else {
        truncated
    })
}

/// Converts an OCPP wire `decimal` physical quantity (Wh, W, A) into thousandths of its unit -
/// the mWh/mW/mA that [`TariffConditions`] stores thresholds in, and that
/// [`MeterSample`](crate::state::MeterSample) already reports current in. `None` if the value is
/// not finite or does not fit.
pub fn milli_from_decimal(value: f64) -> Option<i64> {
    fixed_from_decimal(value, 1_000)
}

/// The inverse of [`milli_from_decimal`], for reporting a threshold or a usage figure back on the
/// wire.
pub fn milli_to_decimal(value: i64) -> f64 {
    value as f64 / 1_000.0
}

/// A tax percentage, in millionths of one percent.
///
/// Same fixed-point reasoning as [`Money`]: a VAT rate is an exact decimal the operator wrote
/// down, and `21.0` must not become `20.999999999999996` on the way to a receipt.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct TaxPercent(pub i64);

impl TaxPercent {
    /// Micro-percent per whole percent.
    pub const SCALE: i64 = 1_000_000;

    /// Converts an OCPP wire `decimal` percentage, or `None` if it is not finite or does not fit.
    /// Rounds to nearest, for the reason [`Money::from_decimal`] documents.
    pub fn from_decimal(value: f64) -> Option<Self> {
        Money::from_decimal(value).map(|money| Self(money.0))
    }

    /// The percentage as an OCPP wire `decimal`.
    pub fn to_decimal(self) -> f64 {
        self.0 as f64 / Self::SCALE as f64
    }
}

/// One tax that applies to a tariff component (OCPP `TaxRateType`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaxRate {
    /// What this tax is called on the receipt, e.g. `"vat"`, `"federal"` (OCPP `type`).
    pub kind: String,
    /// The percentage.
    pub percent: TaxPercent,
    /// Which layer of the tax stack this belongs to (OCPP `stack`, absent means `0`): `0` is
    /// charged on the net price, `1` on the net price *including* every stack-`0` tax, and so on.
    /// See I12.FR.13, which [`crate::pricing`] implements.
    pub stack: u8,
}

/// A time of day in **local** time, to the minute - OCPP's `startTimeOfDay`/`endTimeOfDay`
/// (`hh:mm`).
///
/// Local rather than UTC by requirement (I07.FR.17): the tariff states the times a driver is
/// shown on the station's display, which is what makes a "cheap after 18:00" tariff mean the same
/// thing in summer and winter without the CSMS shipping two tariffs. [`crate::pricing`] converts
/// the station's UTC clock into local time before comparing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct TimeOfDay {
    /// Hour, `0..=23`.
    pub hour: u8,
    /// Minute, `0..=59`.
    pub minute: u8,
}

impl TimeOfDay {
    /// Builds a time of day, or `None` for an hour past 23 or a minute past 59.
    pub fn new(hour: u8, minute: u8) -> Option<Self> {
        (hour <= 23 && minute <= 59).then_some(Self { hour, minute })
    }

    /// Minutes since local midnight, for comparison against a window.
    pub fn minutes_since_midnight(self) -> u16 {
        self.hour as u16 * 60 + self.minute as u16
    }
}

/// Which kind of EVSE a price applies to (OCPP `EvseKindEnumType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EvseKind {
    /// AC charging.
    Ac,
    /// DC charging.
    Dc,
}

/// When a [`EnergyPrice`] or [`TimePrice`] applies (OCPP `TariffConditionsType`).
///
/// Every field that is `Some`/non-empty must hold for the price to be active - OCPP treats
/// multiple restrictions as a logical AND. All `None`/empty is "always applicable".
///
/// The `min_*`/`max_*` physical thresholds are stored in thousandths of their OCPP unit (mWh, mW,
/// mA) so a `decimal` on the wire survives without a float: `min_current_ma` lines up exactly with
/// [`MeterSample::current_ma`](crate::state::MeterSample), and the other two are the same reading
/// scaled by 1000.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TariffConditions {
    /// Start of the daily window, local time. See [`TimeOfDay`].
    pub start_time_of_day: Option<TimeOfDay>,
    /// End of the daily window, local time, **exclusive**. An end earlier than the start wraps
    /// around midnight; `00:00` therefore means "to the end of the day", exactly as OCPP says.
    pub end_time_of_day: Option<TimeOfDay>,
    /// Days of the week this price applies to, local time. Empty means every day.
    pub day_of_week: Vec<Weekday>,
    /// First local date this price applies to, inclusive.
    pub valid_from_date: Option<NaiveDate>,
    /// Last local date this price applies to, **exclusive**.
    pub valid_to_date: Option<NaiveDate>,
    /// Restricts the price to AC or DC EVSEs.
    pub evse_kind: Option<EvseKind>,
    /// Minimum energy consumed so far, in mWh, inclusive.
    pub min_energy_mwh: Option<i64>,
    /// Maximum energy consumed so far, in mWh, exclusive.
    pub max_energy_mwh: Option<i64>,
    /// Minimum instantaneous current summed over the phases, in mA, inclusive.
    pub min_current_ma: Option<i64>,
    /// Maximum instantaneous current summed over the phases, in mA, exclusive.
    pub max_current_ma: Option<i64>,
    /// Minimum instantaneous power, in mW, inclusive.
    pub min_power_mw: Option<i64>,
    /// Maximum instantaneous power, in mW, exclusive.
    pub max_power_mw: Option<i64>,
    /// Minimum transaction duration (charging *and* idle) in seconds, inclusive.
    pub min_time_secs: Option<i64>,
    /// Maximum transaction duration (charging *and* idle) in seconds, exclusive.
    pub max_time_secs: Option<i64>,
    /// Minimum time spent charging, in seconds, inclusive.
    pub min_charging_time_secs: Option<i64>,
    /// Maximum time spent charging, in seconds, exclusive.
    pub max_charging_time_secs: Option<i64>,
    /// Minimum time spent idle (connected, not charging), in seconds, inclusive.
    pub min_idle_time_secs: Option<i64>,
    /// Maximum time spent idle, in seconds, exclusive.
    pub max_idle_time_secs: Option<i64>,
}

impl TariffConditions {
    /// Whether any restriction at all is set. A price with no conditions is always applicable, and
    /// [`Tariff::has_conditions`] uses this to answer I07.FR.03/I11.FR.03 for a station that
    /// reports `TariffCostCtrlr.ConditionsSupported[Tariff] = false`.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// When a [`FixedPrice`] applies (OCPP `TariffConditionsFixedType`).
///
/// The fixed-fee counterpart to [`TariffConditions`], minus everything that can only be known once
/// energy is flowing - a fixed fee is evaluated once, at the start of the transaction
/// (I12.FR.30) - and plus the two payment fields, which only a start fee has any use for.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TariffConditionsFixed {
    /// Start of the daily window, local time.
    pub start_time_of_day: Option<TimeOfDay>,
    /// End of the daily window, local time, exclusive; wraps around midnight when earlier than
    /// the start.
    pub end_time_of_day: Option<TimeOfDay>,
    /// Days of the week this price applies to, local time. Empty means every day.
    pub day_of_week: Vec<Weekday>,
    /// First local date this price applies to, inclusive.
    pub valid_from_date: Option<NaiveDate>,
    /// Last local date this price applies to, exclusive.
    pub valid_to_date: Option<NaiveDate>,
    /// Restricts the price to AC or DC EVSEs.
    pub evse_kind: Option<EvseKind>,
    /// The payment brand this (ad hoc) price applies to, from the presented token's
    /// `additionalInfo` of type `PaymentBrand`.
    pub payment_brand: Option<String>,
    /// The kind of ad hoc payment (`"CC"`, `"Debit"`, ...) this price applies to, from the
    /// presented token's `additionalInfo` of type `PaymentRecognition`.
    pub payment_recognition: Option<String>,
}

impl TariffConditionsFixed {
    /// Whether any restriction at all is set - see [`TariffConditions::is_empty`].
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One energy price and the conditions it applies under (OCPP `TariffEnergyPriceType`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnergyPrice {
    /// Price per kWh, excluding tax.
    pub price_per_kwh: Money,
    /// When this price applies; `None` is always.
    pub conditions: Option<TariffConditions>,
}

/// One time price and the conditions it applies under (OCPP `TariffTimePriceType`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimePrice {
    /// Price per minute, excluding tax.
    pub price_per_minute: Money,
    /// When this price applies; `None` is always.
    pub conditions: Option<TariffConditions>,
}

/// One fixed fee and the conditions it applies under (OCPP `TariffFixedPriceType`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FixedPrice {
    /// The fee, excluding tax.
    pub price: Money,
    /// When this fee applies; `None` is always.
    pub conditions: Option<TariffConditionsFixed>,
}

/// The energy dimension of a tariff (OCPP `TariffEnergyType`): its price list and the taxes on it.
///
/// The price list is ordered and the order is load-bearing: when several prices' conditions hold
/// at once, the **first** applicable one is the one charged (I12.FR.34).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnergyComponent {
    /// Candidate prices, most specific first by convention - see this type's docs.
    pub prices: Vec<EnergyPrice>,
    /// Taxes applied to this dimension's cost. Empty means no tax is applicable, which OCPP
    /// distinguishes from a declared 0% rate.
    pub tax_rates: Vec<TaxRate>,
}

/// A time dimension of a tariff (OCPP `TariffTimeType`) - charging time, idle time or reservation
/// time. See [`EnergyComponent`] for why the price order matters.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimeComponent {
    /// Candidate prices, most specific first.
    pub prices: Vec<TimePrice>,
    /// Taxes applied to this dimension's cost.
    pub tax_rates: Vec<TaxRate>,
}

/// A fixed-fee dimension of a tariff (OCPP `TariffFixedType`) - the start fee, or a fixed
/// reservation fee. See [`EnergyComponent`] for why the price order matters.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FixedComponent {
    /// Candidate fees, most specific first.
    pub prices: Vec<FixedPrice>,
    /// Taxes applied to this dimension's cost.
    pub tax_rates: Vec<TaxRate>,
}

/// A price with and without tax (OCPP `PriceType`), used for a tariff's `minCost`/`maxCost`.
///
/// OCPP allows either half to be absent, and the two are independent bounds rather than two
/// renderings of one: a `maxCost` with only `inclTax` caps what the driver pays, one with only
/// `exclTax` caps the net. [`crate::pricing`] applies whichever halves are present.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Price {
    /// The amount excluding tax.
    pub excl_tax: Option<Money>,
    /// The amount including tax.
    pub incl_tax: Option<Money>,
}

/// A tariff as installed on the charge point - OCPP's `TariffType`, minus the human-readable
/// `description` (this crate renders nothing itself; see `crate::display_message` for the surface
/// that would).
///
/// Everything a cost can be derived from is here, in fixed point (see this module's docs), which
/// is what lets [`crate::pricing`] answer I07-I12 locally rather than waiting for the CSMS to send
/// a `CostUpdated`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tariff {
    /// This tariff's identifier - see [`TariffId`].
    pub id: TariffId,
    /// The tariff's currency, ISO 4217.
    pub currency: String,
    /// When this tariff becomes active, in *system* time (I07.FR.18 - UTC, unlike the local-time
    /// conditions). `None` means immediately. A tariff whose `valid_from` is still in the future
    /// is installed but not used (I07.FR.19); see [`TariffStore::effective_at`].
    pub valid_from: Option<DateTime<Utc>>,
    /// Price per kWh of energy transferred.
    pub energy: Option<EnergyComponent>,
    /// Price per minute spent charging.
    pub charging_time: Option<TimeComponent>,
    /// Price per minute spent connected but not charging.
    pub idle_time: Option<TimeComponent>,
    /// Fixed fee charged once, at the start of the transaction (I12.FR.30).
    pub fixed_fee: Option<FixedComponent>,
    /// Price per minute of a reservation that preceded the transaction (I12.FR.07).
    pub reservation_time: Option<TimeComponent>,
    /// Fixed fee for a reservation that preceded the transaction (I12.FR.08).
    pub reservation_fixed: Option<FixedComponent>,
    /// The least this transaction may cost. Applied at the end, replacing the calculated total
    /// (I12.FR.38).
    pub min_cost: Option<Price>,
    /// The most this transaction may cost. Once reached, cost calculation freezes (I12.FR.39)
    /// while usage keeps accumulating (I12.FR.40).
    pub max_cost: Option<Price>,
}

impl Tariff {
    /// A tariff with an id and a currency and no priced structure at all - the starting point for
    /// building one field by field, and what a `GetTariffs`-only test needs.
    ///
    /// A tariff like this costs nothing: no price element applies to any dimension, so
    /// [`crate::pricing`] charges zero rather than guessing.
    pub fn new(id: TariffId, currency: impl Into<String>) -> Self {
        Self {
            id,
            currency: currency.into(),
            valid_from: None,
            energy: None,
            charging_time: None,
            idle_time: None,
            fixed_fee: None,
            reservation_time: None,
            reservation_fixed: None,
            min_cost: None,
            max_cost: None,
        }
    }

    /// The largest number of price elements any one dimension carries.
    ///
    /// This is what `TariffCostCtrlr.MaxElements[Tariff]` bounds and what I07.FR.02/I11.FR.02
    /// answer `TooManyElements` against. OCPP words the requirement as "the total count of
    /// TariffEnergyType, TariffTimeType and TariffFixedType elements", but the variable it is
    /// bounded by is defined per dimension ("the maximum number of prices elements ... in **each**
    /// TariffEnergyType (energy), TariffTimeType (chargingTime, idleTime) and TariffFixedType
    /// (fixedFee)"). The per-dimension reading is the one taken here: it accepts strictly more
    /// tariffs than a summed reading would, and the summed reading would refuse tariffs the
    /// station can price perfectly well.
    pub fn max_prices_per_dimension(&self) -> usize {
        let energy = self.energy.as_ref().map_or(0, |c| c.prices.len());
        let times = [&self.charging_time, &self.idle_time, &self.reservation_time]
            .into_iter()
            .flatten()
            .map(|c| c.prices.len())
            .max()
            .unwrap_or(0);
        let fixed = [&self.fixed_fee, &self.reservation_fixed]
            .into_iter()
            .flatten()
            .map(|c| c.prices.len())
            .max()
            .unwrap_or(0);
        energy.max(times).max(fixed)
    }

    /// Whether any price in this tariff carries a non-empty condition - what I07.FR.03/I11.FR.03
    /// refuse with `ConditionNotSupported` on a station reporting
    /// `TariffCostCtrlr.ConditionsSupported[Tariff] = false`.
    ///
    /// A `conditions` object that is present but entirely empty does not count: it restricts
    /// nothing, so a station that cannot evaluate conditions loses nothing by accepting it.
    pub fn has_conditions(&self) -> bool {
        let timed = [&self.charging_time, &self.idle_time, &self.reservation_time]
            .into_iter()
            .flatten()
            .any(|component| {
                component
                    .prices
                    .iter()
                    .any(|price| price.conditions.as_ref().is_some_and(|c| !c.is_empty()))
            });
        let energy = self.energy.as_ref().is_some_and(|component| {
            component
                .prices
                .iter()
                .any(|price| price.conditions.as_ref().is_some_and(|c| !c.is_empty()))
        });
        let fixed = [&self.fixed_fee, &self.reservation_fixed]
            .into_iter()
            .flatten()
            .any(|component| {
                component
                    .prices
                    .iter()
                    .any(|price| price.conditions.as_ref().is_some_and(|c| !c.is_empty()))
            });
        energy || timed || fixed
    }

    /// Whether this tariff may be used to price a transaction at `now` (I07.FR.19): a tariff with
    /// a `valid_from` in the future is installed and reportable, but not yet in force.
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        self.valid_from.is_none_or(|valid_from| now >= valid_from)
    }
}

/// Which connector scope a default tariff was installed against. OCPP addresses this as an
/// `evseId` where `0` means "every EVSE"; this type makes that sentinel explicit rather than
/// leaving a magic zero to be remembered at every use site - mirrors
/// [`crate::state::ChargingProfileScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TariffScope {
    /// Installed charge-point-wide (OCPP `evseId` `0`). The default for every EVSE that has no
    /// tariff of its own.
    ChargePoint,
    /// Installed against one EVSE, overriding the charge-point-wide default for it.
    Evse(usize),
}

/// A tariff together with the scope it was installed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledTariff {
    /// Where it was installed - see [`TariffScope`].
    pub scope: TariffScope,
    /// The tariff itself.
    pub tariff: Tariff,
}

/// Why [`TariffStore::set_default`] refused a tariff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TariffSetRejection {
    /// The store is already holding [`crate::state::StateLimits::max_tariffs`] tariffs and this
    /// one is not replacing any of them (mirrors
    /// [`ChargingProfileRejection::TooManyProfiles`](crate::state::ChargingProfileRejection::TooManyProfiles)'s
    /// reasoning - a bound a remote peer can push past is not a bound).
    TooManyTariffs,
    /// A tariff with this id is already installed at a *different* scope. A tariff id identifies
    /// one tariff on the whole charge point, so installing the same id at a second scope would
    /// leave the CSMS unable to say which scope's copy is authoritative - refused rather than
    /// silently duplicated.
    DuplicateTariffId,
}

/// Which tariffs a `ClearTariffs` request selects. Every field that is `Some` must match; a
/// request with neither selects every installed tariff, matching OCPP's absent-means-all rule.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TariffClearCriteria {
    /// Match only tariffs installed at this scope.
    pub scope: Option<TariffScope>,
    /// Match one specific tariff id.
    pub id: Option<TariffId>,
}

impl TariffClearCriteria {
    /// Whether `installed` is selected by these criteria.
    pub fn matches(&self, installed: &InstalledTariff) -> bool {
        self.scope.is_none_or(|scope| installed.scope == scope)
            && self.id.as_ref().is_none_or(|id| &installed.tariff.id == id)
    }
}

/// Every default tariff installed on the charge point, across every scope - the state behind
/// OCPP 2.1's `SetDefaultTariff`/`ClearTariffs`/`GetTariffs`.
///
/// Bounded by [`crate::state::StateLimits::max_tariffs`]. Owned by
/// [`crate::state::ChargePointState`] and mutated only through
/// [`crate::state::ChargePointEvent::DefaultTariffSet`]/
/// [`TariffsCleared`](crate::state::ChargePointEvent::TariffsCleared), like every other piece of
/// state in this crate.
///
/// # Why a scope can hold more than one tariff
///
/// I07's scenario #3 installs tomorrow's tariff today: a `SetDefaultTariff` carrying a
/// `validFrom` in the future must neither be refused nor take effect immediately, so the scope
/// holds both until the clock passes `validFrom` (I07.FR.19/.20/.21/.23). Which one is actually
/// charged is decided at *use* time by [`Self::effective_at`], not at install time - the store
/// has no clock, and inventing one here would put a time source inside the state machine that the
/// rest of this crate deliberately keeps outside it.
#[derive(Debug, Clone, PartialEq)]
pub struct TariffStore {
    tariffs: Vec<InstalledTariff>,
    max_tariffs: usize,
}

impl TariffStore {
    /// An empty store holding at most `max_tariffs` tariffs (clamped to at least 1).
    pub fn with_limit(max_tariffs: usize) -> Self {
        Self {
            tariffs: Vec::new(),
            max_tariffs: max_tariffs.max(1),
        }
    }

    /// The configured maximum - see [`crate::state::StateLimits::max_tariffs`].
    pub fn max_tariffs(&self) -> usize {
        self.max_tariffs
    }

    /// Every installed default tariff, in installation order.
    pub fn installed(&self) -> &[InstalledTariff] {
        &self.tariffs
    }

    /// How many tariffs are installed.
    pub fn len(&self) -> usize {
        self.tariffs.len()
    }

    /// Whether nothing is installed.
    pub fn is_empty(&self) -> bool {
        self.tariffs.is_empty()
    }

    /// Installs `tariff` as a default for `scope`.
    ///
    /// An existing tariff at that scope is *replaced* only when the incoming one takes over at
    /// exactly the same instant (the same `validFrom`, absent counting as "already valid") or
    /// carries the same id. Anything else is kept alongside it, and which of them prices a
    /// transaction is decided by [`Self::effective_at`].
    ///
    /// That split is what makes I07's four replacement requirements come out right without a
    /// clock in the state machine. I07.FR.22 (two undated tariffs) replaces here. I07.FR.20 and
    /// .21 (the incoming one is dated later) must *not* replace here - I07's own scenario #3
    /// installs tomorrow's tariff today, and dropping today's on the spot would leave the station
    /// unpriced until midnight; `effective_at` hands over the moment `validFrom` arrives, which is
    /// what those requirements actually ask for. I07.FR.23 (the incoming one is dated *earlier*
    /// than one already installed) must not replace either, and does not.
    ///
    /// The cost is that a superseded dated tariff lingers until a later `SetDefaultTariff` shares
    /// its `validFrom` or a `ClearTariffs` removes it. It is never *used* once superseded, and
    /// [`crate::state::StateLimits::max_tariffs`] still bounds the store, so the worst case is a
    /// CSMS that queues many dated tariffs eventually being told `TooManyElements` - which is the
    /// truth about a station with a fixed tariff budget.
    pub fn set_default(
        &mut self,
        scope: TariffScope,
        tariff: Tariff,
    ) -> Result<(), TariffSetRejection> {
        let duplicate_id = self
            .tariffs
            .iter()
            .any(|installed| installed.scope != scope && installed.tariff.id == tariff.id);
        if duplicate_id {
            return Err(TariffSetRejection::DuplicateTariffId);
        }
        let superseded = |installed: &InstalledTariff| {
            installed.scope == scope
                && (installed.tariff.id == tariff.id
                    || installed.tariff.valid_from == tariff.valid_from)
        };
        let replacing = self.tariffs.iter().any(superseded);
        if !replacing && self.tariffs.len() >= self.max_tariffs {
            return Err(TariffSetRejection::TooManyTariffs);
        }
        self.tariffs.retain(|installed| !superseded(installed));
        self.tariffs.push(InstalledTariff { scope, tariff });
        Ok(())
    }

    /// Removes every tariff matching `criteria`, returning how many were removed (`0` means the
    /// CSMS's `ClearTariffs` matched nothing).
    ///
    /// A cleared tariff that a running transaction is already using keeps pricing that
    /// transaction to its end (I10.FR.07) - which is automatic here, because a transaction's
    /// tariff is held on the connector
    /// ([`EvseState::transaction_tariffs`](crate::state::EvseState::transaction_tariffs)) rather
    /// than looked up in this store per meter sample.
    pub fn clear(&mut self, criteria: &TariffClearCriteria) -> usize {
        let before = self.tariffs.len();
        self.tariffs
            .retain(|installed| !criteria.matches(installed));
        before - self.tariffs.len()
    }

    /// Every tariff matching `criteria`, in installation order - what [`Self::clear`] would
    /// remove.
    pub fn matching(&self, criteria: &TariffClearCriteria) -> Vec<&InstalledTariff> {
        self.tariffs
            .iter()
            .filter(|installed| criteria.matches(installed))
            .collect()
    }

    /// The tariffs `GetTariffs` reports for `evse_id`: every installed tariff when `evse_id` is
    /// `None` (OCPP's `evseId = 0`, "from all EVSEs"), or the charge-point-wide default plus
    /// `Evse(evse_id)`'s own tariff (if either is installed) otherwise.
    pub fn selected_by_evse(&self, evse_id: Option<usize>) -> Vec<&InstalledTariff> {
        let Some(evse_id) = evse_id else {
            return self.tariffs.iter().collect();
        };
        self.tariffs
            .iter()
            .filter(|installed| match installed.scope {
                TariffScope::ChargePoint => true,
                TariffScope::Evse(id) => id == evse_id,
            })
            .collect()
    }

    /// The default tariff that actually prices a transaction starting on `evse_id` at `now`
    /// (I07.FR.14/.19).
    ///
    /// An EVSE's own tariff wins over the charge-point-wide one, and among tariffs at the same
    /// scope the one that became valid most recently wins - a tariff whose `validFrom` is still in
    /// the future is skipped until the clock reaches it (I07.FR.19), and among two that are both
    /// valid the later `validFrom` is the newer instruction (I07.FR.20/.21). Ties break towards
    /// the most recently installed, which is the only ordering that lets I07.FR.22 (replace a
    /// tariff that has no `validFrom` with another that has none) mean anything.
    pub fn effective_at(&self, evse_id: usize, now: DateTime<Utc>) -> Option<&Tariff> {
        self.effective_for_scope(TariffScope::Evse(evse_id), now)
            .or_else(|| self.effective_for_scope(TariffScope::ChargePoint, now))
    }

    fn effective_for_scope(&self, scope: TariffScope, now: DateTime<Utc>) -> Option<&Tariff> {
        self.tariffs
            .iter()
            .filter(|installed| installed.scope == scope && installed.tariff.is_valid_at(now))
            // `max_by_key` returns the *last* maximum, which is the most recently installed.
            .max_by_key(|installed| installed.tariff.valid_from)
            .map(|installed| &installed.tariff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use chrono::TimeZone;

    fn tariff(id: &str) -> Tariff {
        Tariff::new(TariffId(id.into()), "EUR")
    }

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid instant")
    }

    #[test]
    fn a_decimal_price_becomes_exact_micro_units() {
        assert_eq!(Money::from_decimal(0.25), Some(Money(250_000)));
        assert_eq!(Money::from_decimal(0.909), Some(Money(909_000)));
        // The `f64` nearest to 0.29 is below it; rounding to nearest keeps the operator's decimal.
        assert_eq!(Money::from_decimal(0.29), Some(Money(290_000)));
        assert_eq!(Money::from_decimal(-0.4), Some(Money(-400_000)));
    }

    #[test]
    fn a_non_finite_or_oversized_price_is_refused() {
        assert_eq!(Money::from_decimal(f64::NAN), None);
        assert_eq!(Money::from_decimal(f64::INFINITY), None);
        assert_eq!(Money::from_decimal(1e30), None);
    }

    #[test]
    fn a_price_round_trips_through_the_wire_decimal() {
        assert_eq!(Money(250_000).to_decimal(), 0.25);
        assert_eq!(
            Money::from_decimal(Money(3_141_593).to_decimal()),
            Some(Money(3_141_593))
        );
    }

    #[test]
    fn a_tariff_with_no_conditions_anywhere_reports_none() {
        let mut tariff = tariff("t1");
        tariff.energy = Some(EnergyComponent {
            prices: alloc::vec![EnergyPrice {
                price_per_kwh: Money(250_000),
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });

        assert!(!tariff.has_conditions());
    }

    #[test]
    fn an_empty_conditions_object_is_not_a_condition() {
        let mut tariff = tariff("t1");
        tariff.energy = Some(EnergyComponent {
            prices: alloc::vec![EnergyPrice {
                price_per_kwh: Money(250_000),
                conditions: Some(TariffConditions::default()),
            }],
            tax_rates: Vec::new(),
        });

        assert!(!tariff.has_conditions());
    }

    #[test]
    fn a_condition_on_any_dimension_is_reported() {
        let mut tariff = tariff("t1");
        tariff.idle_time = Some(TimeComponent {
            prices: alloc::vec![TimePrice {
                price_per_minute: Money(1_000_000),
                conditions: Some(TariffConditions {
                    min_idle_time_secs: Some(300),
                    ..Default::default()
                }),
            }],
            tax_rates: Vec::new(),
        });

        assert!(tariff.has_conditions());
    }

    #[test]
    fn the_element_count_is_the_largest_single_dimension() {
        let mut tariff = tariff("t1");
        tariff.energy = Some(EnergyComponent {
            prices: alloc::vec![
                EnergyPrice {
                    price_per_kwh: Money(1),
                    conditions: None
                },
                EnergyPrice {
                    price_per_kwh: Money(2),
                    conditions: None
                },
            ],
            tax_rates: Vec::new(),
        });
        tariff.fixed_fee = Some(FixedComponent {
            prices: alloc::vec![FixedPrice {
                price: Money(3),
                conditions: None
            }],
            tax_rates: Vec::new(),
        });

        assert_eq!(tariff.max_prices_per_dimension(), 2);
    }

    #[test]
    fn installing_at_a_new_scope_stacks_beside_an_existing_one() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("t1"))
            .unwrap();
        store
            .set_default(TariffScope::Evse(0), tariff("t2"))
            .unwrap();

        assert_eq!(store.len(), 2);
    }

    #[test]
    fn installing_at_an_occupied_scope_replaces_it() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::Evse(0), tariff("t1"))
            .unwrap();
        store
            .set_default(TariffScope::Evse(0), tariff("t2"))
            .unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].tariff.id, TariffId("t2".into()));
    }

    // I07 scenario #3: tomorrow's tariff is installed today and must not displace today's until
    // its `validFrom` arrives (I07.FR.19).
    #[test]
    fn a_future_tariff_is_installed_beside_the_current_one() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::Evse(0), tariff("today"))
            .unwrap();
        let mut tomorrow = tariff("tomorrow");
        tomorrow.valid_from = Some(at(86_400));
        store.set_default(TariffScope::Evse(0), tomorrow).unwrap();

        assert_eq!(store.len(), 2);
        assert_eq!(
            store.effective_at(0, at(0)).map(|t| t.id.clone()),
            Some(TariffId("today".into()))
        );
        assert_eq!(
            store.effective_at(0, at(86_400)).map(|t| t.id.clone()),
            Some(TariffId("tomorrow".into()))
        );
    }

    // I07.FR.23: A is not yet valid and takes over *after* B, so installing B must not remove it.
    #[test]
    fn a_tariff_scheduled_after_the_incoming_one_survives_it() {
        let mut store = TariffStore::with_limit(10);
        let mut late = tariff("late");
        late.valid_from = Some(at(2_000));
        store.set_default(TariffScope::Evse(0), late).unwrap();
        let mut early = tariff("early");
        early.valid_from = Some(at(1_000));
        store.set_default(TariffScope::Evse(0), early).unwrap();

        assert_eq!(store.len(), 2);
        assert_eq!(
            store.effective_at(0, at(1_500)).map(|t| t.id.clone()),
            Some(TariffId("early".into()))
        );
        assert_eq!(
            store.effective_at(0, at(2_500)).map(|t| t.id.clone()),
            Some(TariffId("late".into()))
        );
    }

    // I07.FR.21: A becomes valid before B, so B takes over the moment its own `validFrom`
    // arrives - and not a second earlier, which is what would leave the station unpriced.
    #[test]
    fn a_tariff_scheduled_before_the_incoming_one_hands_over_at_its_time() {
        let mut store = TariffStore::with_limit(10);
        let mut early = tariff("early");
        early.valid_from = Some(at(1_000));
        store.set_default(TariffScope::Evse(0), early).unwrap();
        let mut late = tariff("late");
        late.valid_from = Some(at(2_000));
        store.set_default(TariffScope::Evse(0), late).unwrap();

        assert_eq!(
            store.effective_at(0, at(1_999)).map(|t| t.id.clone()),
            Some(TariffId("early".into()))
        );
        assert_eq!(
            store.effective_at(0, at(2_000)).map(|t| t.id.clone()),
            Some(TariffId("late".into()))
        );
    }

    // Two tariffs that take over at the same instant cannot both be the default, so the newer
    // instruction replaces the older outright - I07.FR.22 for the undated case.
    #[test]
    fn a_tariff_taking_over_at_the_same_instant_replaces_the_other() {
        let mut store = TariffStore::with_limit(10);
        let mut first = tariff("first");
        first.valid_from = Some(at(1_000));
        store.set_default(TariffScope::Evse(0), first).unwrap();
        let mut second = tariff("second");
        second.valid_from = Some(at(1_000));
        store.set_default(TariffScope::Evse(0), second).unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].tariff.id, TariffId("second".into()));
    }

    #[test]
    fn a_tariff_that_is_not_yet_valid_prices_nothing() {
        let mut store = TariffStore::with_limit(10);
        let mut future = tariff("future");
        future.valid_from = Some(at(1_000));
        store.set_default(TariffScope::ChargePoint, future).unwrap();

        assert!(store.effective_at(0, at(999)).is_none());
        assert!(store.effective_at(0, at(1_000)).is_some());
    }

    #[test]
    fn an_evse_tariff_wins_over_the_charge_point_default() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("wide"))
            .unwrap();
        store
            .set_default(TariffScope::Evse(1), tariff("mine"))
            .unwrap();

        assert_eq!(
            store.effective_at(1, at(0)).map(|t| t.id.clone()),
            Some(TariffId("mine".into()))
        );
        assert_eq!(
            store.effective_at(0, at(0)).map(|t| t.id.clone()),
            Some(TariffId("wide".into()))
        );
    }

    #[test]
    fn the_same_id_at_a_different_scope_is_refused_as_a_duplicate() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("t1"))
            .unwrap();

        assert_eq!(
            store.set_default(TariffScope::Evse(0), tariff("t1")),
            Err(TariffSetRejection::DuplicateTariffId)
        );
    }

    #[test]
    fn the_tariff_bound_refuses_a_new_tariff_but_never_a_replacement() {
        let mut store = TariffStore::with_limit(1);
        store
            .set_default(TariffScope::Evse(0), tariff("t1"))
            .unwrap();

        assert_eq!(
            store.set_default(TariffScope::Evse(1), tariff("t2")),
            Err(TariffSetRejection::TooManyTariffs)
        );
        assert_eq!(
            store.set_default(TariffScope::Evse(0), tariff("t1-again")),
            Ok(())
        );
    }

    #[test]
    fn clearing_by_id_removes_only_the_matching_tariff() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::Evse(0), tariff("t1"))
            .unwrap();
        store
            .set_default(TariffScope::Evse(1), tariff("t2"))
            .unwrap();

        assert_eq!(
            store.clear(&TariffClearCriteria {
                id: Some(TariffId("t1".into())),
                ..Default::default()
            }),
            1
        );
        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].tariff.id, TariffId("t2".into()));
    }

    #[test]
    fn clearing_with_no_criteria_at_all_clears_everything() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::Evse(0), tariff("t1"))
            .unwrap();
        store
            .set_default(TariffScope::ChargePoint, tariff("t2"))
            .unwrap();

        assert_eq!(store.clear(&TariffClearCriteria::default()), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn clearing_something_absent_removes_nothing() {
        let mut store = TariffStore::with_limit(10);
        assert_eq!(
            store.clear(&TariffClearCriteria {
                id: Some(TariffId("nope".into())),
                ..Default::default()
            }),
            0
        );
    }

    #[test]
    fn getting_evse_zero_returns_every_installed_tariff() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("t1"))
            .unwrap();
        store
            .set_default(TariffScope::Evse(0), tariff("t2"))
            .unwrap();
        store
            .set_default(TariffScope::Evse(1), tariff("t3"))
            .unwrap();

        assert_eq!(store.selected_by_evse(None).len(), 3);
    }

    #[test]
    fn getting_one_evse_returns_its_own_tariff_and_the_charge_point_default() {
        let mut store = TariffStore::with_limit(10);
        store
            .set_default(TariffScope::ChargePoint, tariff("t1"))
            .unwrap();
        store
            .set_default(TariffScope::Evse(0), tariff("t2"))
            .unwrap();
        store
            .set_default(TariffScope::Evse(1), tariff("t3"))
            .unwrap();

        let ids: Vec<String> = store
            .selected_by_evse(Some(0))
            .iter()
            .map(|installed| installed.tariff.id.0.clone())
            .collect();
        assert_eq!(ids, alloc::vec!["t1".to_string(), "t2".to_string()]);
    }
}
