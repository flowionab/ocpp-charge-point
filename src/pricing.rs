//! Local cost calculation: turning a [`Tariff`](crate::state::Tariff) and what the meter saw into
//! money, which is OCPP 2.1's I12 (and the running cost I02 displays).
//!
//! Earlier OCPP versions left every cost to the CSMS. 2.1 lets the CSMS ship the *tariff* instead
//! and have the station price the session itself, which is what makes a real-time running cost on
//! the display - and a prepaid stop at a fixed amount - possible without a round trip per kWh.
//! This module is that calculation. It knows nothing about the wire or the actor: give it a
//! tariff and a sequence of readings and it produces totals, usage and the charging-period
//! breakdown `CostDetailsType` reports.
//!
//! # Arithmetic
//!
//! No floating point anywhere. Prices arrive as fixed-point [`Money`] (see
//! [`crate::state::tariff`]), every product is computed in `i128` and every division **truncates
//! toward zero**. Truncation rather than rounding is deliberate and consistent: where the
//! specification does not say which way a fraction of a micro-unit goes, this crate takes the
//! reading that charges the driver less. The largest error that can accumulate is one micro-unit
//! (10⁻⁶ of a currency unit) per priced interval per dimension.
//!
//! # Where the specification is ambiguous
//!
//! Three places, each resolved the way that cannot overcharge, and each marked in the code:
//!
//! - **A condition that cannot be evaluated** (an `evseKind` on a station that does not know
//!   whether its EVSE is AC or DC, a `paymentBrand` for a token with no payment information) is
//!   treated as *not met*. The price is then skipped in favour of the next candidate, and if none
//!   applies the dimension costs nothing - OCPP's own advice is to put an unconditional fallback
//!   at the bottom of the list.
//! - **Energy between two meter readings** is apportioned across the sub-intervals a tariff
//!   boundary splits them into, in proportion to time. Nothing finer is knowable, and any other
//!   split would have to invent a load curve.
//! - **`totalUsage.chargingTime`** is reported as the time actually spent charging, following
//!   I12.FR.10/.11 (which pair it with `totalCost.chargingTime`), not the `TotalUsageType` field
//!   note that describes it as the whole session. Usage does not enter the cost, so this choice
//!   cannot change what anyone is charged; it is called out because the two readings disagree.

use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike, Utc};

use crate::state::{
    EnergyComponent, EvseKind, FixedComponent, Money, Price, Tariff, TariffConditions,
    TariffConditionsFixed, TariffId, TaxRate, TimeComponent,
};

/// The most sub-intervals one [`TransactionCost::advance`] will split itself into before giving
/// up on splitting and pricing the remainder in one go.
///
/// A guard, not a tuning knob: a station whose clock jumps forward by a year would otherwise walk
/// every tariff boundary in that year, on a device with no operating system to interrupt it. With
/// a tariff at the specification's own granularity this bound is thousands of days away from any
/// real session.
const MAX_SUB_INTERVALS: usize = 4096;

/// What the station can see at one instant, for deciding which prices apply.
///
/// Every physical reading is in thousandths of its OCPP unit, matching
/// [`TariffConditions`](crate::state::TariffConditions) and
/// [`MeterSample`](crate::state::MeterSample).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingContext {
    /// The station's clock, in UTC.
    pub now: DateTime<Utc>,
    /// Minutes to add to UTC to get the local time a tariff's `startTimeOfDay`/`endTimeOfDay`/
    /// `validFromDate`/`validToDate` are stated in (I07.FR.17). `0` means the station runs on UTC,
    /// which is what it reports until an operator configures `ClockCtrlr.TimeOffset`.
    pub local_offset_minutes: i32,
    /// Cumulative energy this transaction has transferred, in mWh.
    pub energy_mwh: i64,
    /// Instantaneous power, in mW. `None` when the meter does not report it, which makes any
    /// `minPower`/`maxPower` condition unevaluable - see this module's docs.
    pub power_mw: Option<i64>,
    /// Instantaneous current summed over the phases, in mA. `None` when unreported.
    pub current_ma: Option<i64>,
    /// Whether energy is flowing right now. Decides whether elapsed time is charged as charging
    /// time or as idle time.
    pub charging: bool,
    /// Which kind of EVSE this is, for an `evseKind` condition. `None` when the station does not
    /// know.
    pub evse_kind: Option<EvseKind>,
    /// The payment brand the session was authorized with, for a `paymentBrand` condition.
    pub payment_brand: Option<String>,
    /// The payment recognition (`"CC"`, `"Debit"`, ...) the session was authorized with, for a
    /// `paymentRecognition` condition.
    pub payment_recognition: Option<String>,
}

impl PricingContext {
    /// A context with nothing known but the clock - the starting point for a test or for a
    /// station whose meter reports energy only.
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now,
            local_offset_minutes: 0,
            energy_mwh: 0,
            power_mw: None,
            current_ma: None,
            charging: false,
            evse_kind: None,
            payment_brand: None,
            payment_recognition: None,
        }
    }

    fn local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        (at + TimeDelta::minutes(self.local_offset_minutes as i64)).naive_utc()
    }
}

/// One dimension's cost, with and without tax (OCPP `PriceType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DimensionCost {
    /// The net cost.
    pub excl_tax: Money,
    /// The cost with this dimension's taxes applied - see [`apply_taxes`].
    pub incl_tax: Money,
}

impl DimensionCost {
    fn saturating_add(self, other: Self) -> Self {
        Self {
            excl_tax: self.excl_tax.saturating_add(other.excl_tax),
            incl_tax: self.incl_tax.saturating_add(other.incl_tax),
        }
    }

    fn is_zero(self) -> bool {
        self == Self::default()
    }
}

/// Which of a tariff's three total kinds a [`CostTotals`] is (OCPP `TariffCostEnumType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TotalCostKind {
    /// The calculated cost (I12.FR.41).
    Normal,
    /// The tariff's `minCost` replaced the calculated cost (I12.FR.38).
    Min,
    /// The tariff's `maxCost` was reached and calculation froze (I12.FR.39).
    Max,
}

/// The cost of a transaction, broken down per dimension (OCPP `TotalCostType`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CostTotals {
    /// ISO 4217 currency of the tariff that priced this (I12.FR.16).
    pub currency: String,
    /// Whether this is the calculated cost or a `minCost`/`maxCost` substitution.
    pub kind: TotalCostKind,
    /// Fixed fees (I12.FR.05). `None` when the tariff has none.
    pub fixed: Option<DimensionCost>,
    /// Energy (I12.FR.09).
    pub energy: Option<DimensionCost>,
    /// Time spent charging (I12.FR.10).
    pub charging_time: Option<DimensionCost>,
    /// Time spent connected but not charging (I12.FR.11).
    pub idle_time: Option<DimensionCost>,
    /// Time spent under reservation (I12.FR.07).
    pub reservation_time: Option<DimensionCost>,
    /// A fixed reservation fee (I12.FR.08).
    pub reservation_fixed: Option<DimensionCost>,
    /// The sum of every dimension above (I12.FR.17).
    pub total: DimensionCost,
}

/// What a transaction used, independent of what it cost (OCPP `TotalUsageType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Energy transferred, in mWh.
    pub energy_mwh: i64,
    /// Seconds spent charging - see this module's docs on which of two readings this is.
    pub charging_secs: i64,
    /// Seconds spent connected but not charging.
    pub idle_secs: i64,
    /// Seconds the reservation that preceded this transaction lasted, if any (I12.FR.07).
    pub reservation_secs: Option<i64>,
}

/// One stretch of a transaction over which the same price elements applied (OCPP
/// `ChargingPeriodType`).
///
/// A new period starts whenever the "used element list" changes - a different price becomes the
/// first applicable one for energy, charging time or idle time - or when the transaction's tariff
/// itself is replaced (I12.FR.35/.36). The dimensions recorded here are what lets a CSMS
/// recalculate the cost from the tariff and check this station's arithmetic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CostPeriod {
    /// The tariff in force during this period (I12.FR.04).
    pub tariff_id: TariffId,
    /// When the period started.
    pub start: DateTime<Utc>,
    /// Energy transferred during the period, in mWh.
    pub energy_mwh: i64,
    /// Seconds spent charging during the period.
    pub charging_secs: i64,
    /// Seconds spent idle during the period.
    pub idle_secs: i64,
    /// Lowest power seen during the period, in mW, if the meter reported power.
    pub min_power_mw: Option<i64>,
    /// Highest power seen during the period, in mW.
    pub max_power_mw: Option<i64>,
    /// Lowest current seen during the period, in mA.
    pub min_current_ma: Option<i64>,
    /// Highest current seen during the period, in mA.
    pub max_current_ma: Option<i64>,
}

/// One dimension's running cost, kept as a *sealed* part and a *live* part.
///
/// Tax is a percentage of the net, so applying it once to a whole dimension truncates once
/// instead of once per meter sample. That is only valid while the tax rates stay the same, which
/// they do until the CSMS replaces the transaction's tariff (I11) - so a tariff change seals the
/// live part at the old rates and starts a new one. The alternative, taxing every increment,
/// would lose up to a micro-unit per sample for no gain.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
struct Accumulator {
    sealed: DimensionCost,
    live_excl_tax: Money,
    rates: Vec<TaxRate>,
}

impl Accumulator {
    fn add(&mut self, amount: Money, rates: &[TaxRate]) {
        if self.rates != rates {
            self.seal();
            self.rates = rates.to_vec();
        }
        self.live_excl_tax = self.live_excl_tax.saturating_add(amount);
    }

    fn seal(&mut self) {
        if self.live_excl_tax != Money::ZERO {
            self.sealed = self.sealed.saturating_add(DimensionCost {
                excl_tax: self.live_excl_tax,
                incl_tax: apply_taxes(self.live_excl_tax, &self.rates),
            });
            self.live_excl_tax = Money::ZERO;
        }
    }

    fn total(&self) -> DimensionCost {
        self.sealed.saturating_add(DimensionCost {
            excl_tax: self.live_excl_tax,
            incl_tax: apply_taxes(self.live_excl_tax, &self.rates),
        })
    }
}

/// Applies a tax stack to a net amount (I12.FR.13).
///
/// Stack `0` (or absent) is charged on the net price; stack `1` on the result of stack `0`; and so
/// on. Several rates sharing a stack level all apply to that level's base and are summed - the
/// specification says there can be more than one, and charging them one after another would tax
/// the tax. Each stack level truncates once, downwards.
pub fn apply_taxes(net: Money, rates: &[TaxRate]) -> Money {
    if rates.is_empty() {
        return net;
    }
    let mut base = net;
    let mut stack = 0u8;
    let highest = rates.iter().map(|rate| rate.stack).max().unwrap_or(0);
    loop {
        let percent: i128 = rates
            .iter()
            .filter(|rate| rate.stack == stack)
            .map(|rate| rate.percent.0 as i128)
            .sum();
        if percent != 0 {
            // `percent` is micro-percent, so a full 100% is 100 * TaxPercent::SCALE.
            let tax = base.0 as i128 * percent / (100 * crate::state::TaxPercent::SCALE as i128);
            base = Money(saturating_i64(base.0 as i128 + tax));
        }
        if stack == highest {
            return base;
        }
        stack += 1;
    }
}

fn saturating_i64(value: i128) -> i64 {
    value.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// The cost of `energy_mwh` at `price` per kWh, truncated toward zero. 1 kWh is 10⁶ mWh, which is
/// also [`Money::SCALE`], so the two cancel exactly.
fn energy_cost(price: Money, energy_mwh: i64) -> Money {
    Money(saturating_i64(
        price.0 as i128 * energy_mwh as i128 / 1_000_000,
    ))
}

/// The cost of `secs` at `price` per minute, truncated toward zero.
fn time_cost(price: Money, secs: i64) -> Money {
    Money(saturating_i64(price.0 as i128 * secs as i128 / 60))
}

/// Whether a value satisfies a `minXXX` threshold.
///
/// OCPP's note on `TariffConditionsType`: "minXXX must be read as 'closest to zero', and maxXXX as
/// 'furthest from zero'", so a discharging range from -10 kW to -50 kW is `minPower = -10`,
/// `maxPower = -50`. A negative threshold therefore reverses the comparison rather than meaning
/// what it would arithmetically.
fn min_satisfied(value: i64, min: i64) -> bool {
    if min < 0 { value <= min } else { value >= min }
}

/// Whether a value satisfies a `maxXXX` threshold - exclusive, and sign-symmetric with
/// [`min_satisfied`].
fn max_satisfied(value: i64, max: i64) -> bool {
    if max < 0 { value > max } else { value < max }
}

/// Whether `local` falls inside a daily window, which may wrap around midnight.
///
/// An absent bound is midnight, so `endTimeOfDay = "00:00"` means "to the end of the day" as OCPP
/// says, and a window whose start equals its end covers the whole day. The start is inclusive and
/// the end exclusive, which is what makes a transaction that runs across 18:00 pay the old price
/// up to 18:00 and the new one from 18:00 with no minute charged twice or not at all.
fn in_daily_window(
    local: NaiveDateTime,
    start: Option<crate::state::TimeOfDay>,
    end: Option<crate::state::TimeOfDay>,
) -> bool {
    if start.is_none() && end.is_none() {
        return true;
    }
    let minutes = local.hour() as u16 * 60 + local.minute() as u16;
    let start = start.map_or(0, |time| time.minutes_since_midnight());
    let end = end.map_or(0, |time| time.minutes_since_midnight());
    if start == end {
        true
    } else if start < end {
        minutes >= start && minutes < end
    } else {
        minutes >= start || minutes < end
    }
}

fn in_date_range(date: NaiveDate, from: Option<NaiveDate>, to: Option<NaiveDate>) -> bool {
    from.is_none_or(|from| date >= from) && to.is_none_or(|to| date < to)
}

/// What the running transaction looks like to a condition, at the instant one is evaluated.
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    at: DateTime<Utc>,
    energy_mwh: i64,
    elapsed_secs: i64,
    charging_secs: i64,
    idle_secs: i64,
}

/// Whether `conditions` hold. See this module's docs for why an unevaluable condition is `false`.
fn conditions_apply(
    conditions: &TariffConditions,
    context: &PricingContext,
    snapshot: &Snapshot,
) -> bool {
    let local = context.local(snapshot.at);
    if !in_daily_window(
        local,
        conditions.start_time_of_day,
        conditions.end_time_of_day,
    ) {
        return false;
    }
    if !conditions.day_of_week.is_empty() && !conditions.day_of_week.contains(&local.weekday()) {
        return false;
    }
    if !in_date_range(
        local.date(),
        conditions.valid_from_date,
        conditions.valid_to_date,
    ) {
        return false;
    }
    if conditions
        .evse_kind
        .is_some_and(|kind| context.evse_kind != Some(kind))
    {
        return false;
    }
    let thresholds = [
        (conditions.min_energy_mwh, Some(snapshot.energy_mwh), true),
        (conditions.max_energy_mwh, Some(snapshot.energy_mwh), false),
        (conditions.min_current_ma, context.current_ma, true),
        (conditions.max_current_ma, context.current_ma, false),
        (conditions.min_power_mw, context.power_mw, true),
        (conditions.max_power_mw, context.power_mw, false),
        (conditions.min_time_secs, Some(snapshot.elapsed_secs), true),
        (conditions.max_time_secs, Some(snapshot.elapsed_secs), false),
        (
            conditions.min_charging_time_secs,
            Some(snapshot.charging_secs),
            true,
        ),
        (
            conditions.max_charging_time_secs,
            Some(snapshot.charging_secs),
            false,
        ),
        (
            conditions.min_idle_time_secs,
            Some(snapshot.idle_secs),
            true,
        ),
        (
            conditions.max_idle_time_secs,
            Some(snapshot.idle_secs),
            false,
        ),
    ];
    thresholds
        .into_iter()
        .all(|(threshold, value, is_min)| match (threshold, value) {
            (None, _) => true,
            // A threshold the station cannot measure is not met - see this module's docs.
            (Some(_), None) => false,
            (Some(threshold), Some(value)) => {
                if is_min {
                    min_satisfied(value, threshold)
                } else {
                    max_satisfied(value, threshold)
                }
            }
        })
}

/// Whether a fixed fee's conditions hold. Evaluated once, at the start of the transaction
/// (I12.FR.30), so it has no access to energy, power or elapsed time - and OCPP's
/// `TariffConditionsFixedType` accordingly has no fields for them.
fn fixed_conditions_apply(
    conditions: &TariffConditionsFixed,
    context: &PricingContext,
    at: DateTime<Utc>,
) -> bool {
    let local = context.local(at);
    if !in_daily_window(
        local,
        conditions.start_time_of_day,
        conditions.end_time_of_day,
    ) {
        return false;
    }
    if !conditions.day_of_week.is_empty() && !conditions.day_of_week.contains(&local.weekday()) {
        return false;
    }
    if !in_date_range(
        local.date(),
        conditions.valid_from_date,
        conditions.valid_to_date,
    ) {
        return false;
    }
    if conditions
        .evse_kind
        .is_some_and(|kind| context.evse_kind != Some(kind))
    {
        return false;
    }
    if conditions
        .payment_brand
        .as_ref()
        .is_some_and(|brand| context.payment_brand.as_ref() != Some(brand))
    {
        return false;
    }
    if conditions
        .payment_recognition
        .as_ref()
        .is_some_and(|kind| context.payment_recognition.as_ref() != Some(kind))
    {
        return false;
    }
    true
}

/// The index of the first applicable energy price (I12.FR.33/.34), or `None` when none applies.
fn select_energy(
    component: &EnergyComponent,
    context: &PricingContext,
    snapshot: &Snapshot,
) -> Option<usize> {
    component.prices.iter().position(|price| {
        price
            .conditions
            .as_ref()
            .is_none_or(|conditions| conditions_apply(conditions, context, snapshot))
    })
}

/// The index of the first applicable time price (I12.FR.33/.34).
fn select_time(
    component: &TimeComponent,
    context: &PricingContext,
    snapshot: &Snapshot,
) -> Option<usize> {
    component.prices.iter().position(|price| {
        price
            .conditions
            .as_ref()
            .is_none_or(|conditions| conditions_apply(conditions, context, snapshot))
    })
}

/// The index of the first applicable fixed fee (I12.FR.30).
fn select_fixed(
    component: &FixedComponent,
    context: &PricingContext,
    at: DateTime<Utc>,
) -> Option<usize> {
    component.prices.iter().position(|price| {
        price
            .conditions
            .as_ref()
            .is_none_or(|conditions| fixed_conditions_apply(conditions, context, at))
    })
}

/// The distinct local times of day at which any of this tariff's conditions can change, as
/// minutes since local midnight, always including midnight itself.
///
/// I12.FR.31's note draws the line this module follows: date and time-of-day conditions "need to
/// be followed exactly (as these are predictable)", while current, power and energy are only
/// sampled every `TariffCostCtrlr.Interval[Tariff]` seconds. So an interval that spans 18:00 is
/// split at 18:00 and each half priced at its own rate, rather than the whole interval taking
/// whichever price happened to be in force when the meter last reported. Midnight is always a
/// boundary because a date or day-of-week condition changes there.
fn boundary_minutes(tariff: &Tariff) -> Vec<u16> {
    let mut minutes = alloc::vec![0u16];
    let mut push = |conditions: Option<&TariffConditions>| {
        if let Some(conditions) = conditions {
            for time in [conditions.start_time_of_day, conditions.end_time_of_day]
                .into_iter()
                .flatten()
            {
                minutes.push(time.minutes_since_midnight());
            }
        }
    };
    if let Some(component) = &tariff.energy {
        for price in &component.prices {
            push(price.conditions.as_ref());
        }
    }
    for component in [
        &tariff.charging_time,
        &tariff.idle_time,
        &tariff.reservation_time,
    ]
    .into_iter()
    .flatten()
    {
        for price in &component.prices {
            push(price.conditions.as_ref());
        }
    }
    minutes.sort_unstable();
    minutes.dedup();
    minutes
}

/// The next local instant strictly after `local` at which one of `minutes` falls.
fn next_boundary(local: NaiveDateTime, minutes: &[u16]) -> Option<NaiveDateTime> {
    let mut best: Option<NaiveDateTime> = None;
    for &minute in minutes {
        let Some(time) = NaiveTime::from_hms_opt(minute as u32 / 60, minute as u32 % 60, 0) else {
            continue;
        };
        let today = local.date().and_time(time);
        let tomorrow = local.date().succ_opt().map(|date| date.and_time(time));
        for candidate in [Some(today), tomorrow].into_iter().flatten() {
            if candidate > local && best.is_none_or(|best| candidate < best) {
                best = Some(candidate);
            }
        }
    }
    best
}

/// The running cost of one transaction: what has been charged so far, what has been used, and the
/// charging-period breakdown.
///
/// Advanced by [`Self::advance`] as meter readings arrive and read by [`Self::totals`] whenever a
/// `TransactionEventRequest` needs a `costDetails`. Holds no clock and no tariff of its own: both
/// are passed in, so the same accumulator survives the CSMS replacing the transaction's tariff
/// mid-session (I11.FR.08 - the new tariff applies from that moment, never retroactively).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransactionCost {
    tariff_id: TariffId,
    currency: String,
    fixed: Accumulator,
    energy: Accumulator,
    charging_time: Accumulator,
    idle_time: Accumulator,
    reservation_time: Accumulator,
    reservation_fixed: Accumulator,
    usage: Usage,
    periods: Vec<CostPeriod>,
    last_at: DateTime<Utc>,
    last_energy_mwh: i64,
    used_elements: UsedElements,
    capped: bool,
}

/// Which price element each priced dimension is currently using - the "used element list"
/// I12.FR.34 names, and what I12.FR.35 watches for a change to close a charging period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
struct UsedElements {
    energy: Option<usize>,
    charging_time: Option<usize>,
    idle_time: Option<usize>,
}

impl TransactionCost {
    /// Starts pricing a transaction with `tariff`, charging any applicable fixed fee and
    /// reservation cost once, here and now (I12.FR.06/.30).
    ///
    /// `reservation_secs` is the time from the `ReserveNow` being received to this transaction
    /// starting (I12.FR.07); pass `None` when the session did not come from a reservation.
    pub fn start(tariff: &Tariff, context: &PricingContext, reservation_secs: Option<i64>) -> Self {
        let mut cost = Self {
            tariff_id: tariff.id.clone(),
            currency: tariff.currency.clone(),
            fixed: Accumulator::default(),
            energy: Accumulator::default(),
            charging_time: Accumulator::default(),
            idle_time: Accumulator::default(),
            reservation_time: Accumulator::default(),
            reservation_fixed: Accumulator::default(),
            usage: Usage {
                reservation_secs,
                ..Usage::default()
            },
            periods: alloc::vec![CostPeriod {
                tariff_id: tariff.id.clone(),
                start: context.now,
                energy_mwh: 0,
                charging_secs: 0,
                idle_secs: 0,
                min_power_mw: context.power_mw,
                max_power_mw: context.power_mw,
                min_current_ma: context.current_ma,
                max_current_ma: context.current_ma,
            }],
            last_at: context.now,
            last_energy_mwh: context.energy_mwh,
            used_elements: UsedElements::default(),
            capped: false,
        };
        if let Some(component) = &tariff.fixed_fee
            && let Some(index) = select_fixed(component, context, context.now)
        {
            cost.fixed
                .add(component.prices[index].price, &component.tax_rates);
        }
        if let Some(secs) = reservation_secs {
            if let Some(component) = &tariff.reservation_fixed
                && let Some(index) = select_fixed(component, context, context.now)
            {
                cost.reservation_fixed
                    .add(component.prices[index].price, &component.tax_rates);
            }
            if let Some(component) = &tariff.reservation_time
                && let Some(price) = component.prices.first()
            {
                // A reservation ended before this transaction began, so none of
                // `TariffConditionsType`'s in-transaction thresholds can be evaluated against it;
                // the first price is the only defensible choice.
                cost.reservation_time.add(
                    time_cost(price.price_per_minute, secs),
                    &component.tax_rates,
                );
            }
        }
        cost
    }

    /// The tariff currently pricing this transaction.
    pub fn tariff_id(&self) -> &TariffId {
        &self.tariff_id
    }

    /// What this transaction has used so far.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The charging periods so far, oldest first (I12.FR.35/.36).
    pub fn periods(&self) -> &[CostPeriod] {
        &self.periods
    }

    /// Whether the tariff's `maxCost` has been reached and cost calculation has frozen
    /// (I12.FR.39). Usage keeps accumulating either way (I12.FR.40).
    pub fn is_capped(&self) -> bool {
        self.capped
    }

    /// Advances the calculation to `context.now`, charging everything that happened since the last
    /// call.
    ///
    /// The interval is split at every tariff boundary it crosses (see [`boundary_minutes`]), and
    /// the energy reported by `context.energy_mwh` is apportioned across those pieces in
    /// proportion to their length - the only split available without a finer meter. A clock that
    /// went backwards contributes no time, only the energy, so a resynchronizing station cannot be
    /// charged twice for the same minute.
    pub fn advance(&mut self, tariff: &Tariff, context: &PricingContext) {
        if tariff.id != self.tariff_id {
            self.switch_tariff(tariff, context.now);
        }
        let total_secs = (context.now - self.last_at).num_seconds();
        let total_energy_mwh = context
            .energy_mwh
            .saturating_sub(self.last_energy_mwh)
            .max(0);

        if total_secs <= 0 {
            self.charge(tariff, context, self.last_at, 0, total_energy_mwh);
        } else {
            let boundaries = boundary_minutes(tariff);
            let mut from = self.last_at;
            let mut charged_energy = 0i64;
            let mut charged_secs = 0i64;
            for _ in 0..MAX_SUB_INTERVALS {
                let next = next_boundary(context.local(from), &boundaries)
                    .map(|local| from + (local - context.local(from)))
                    .filter(|next| *next < context.now)
                    .unwrap_or(context.now);
                let secs = (next - from).num_seconds();
                // Apportion by time, giving the final piece whatever rounding left over so the
                // pieces always sum to exactly what the meter reported.
                let energy = if next == context.now {
                    total_energy_mwh - charged_energy
                } else {
                    (total_energy_mwh as i128 * secs as i128 / total_secs as i128) as i64
                };
                self.charge(tariff, context, from, secs, energy);
                charged_energy += energy;
                charged_secs += secs;
                from = next;
                if from >= context.now {
                    break;
                }
            }
            // The guard above tripped: price whatever is left in one go rather than dropping it.
            if from < context.now {
                self.charge(
                    tariff,
                    context,
                    from,
                    total_secs - charged_secs,
                    total_energy_mwh - charged_energy,
                );
            }
        }

        self.record_readings(context);
        self.last_at = context.now;
        self.last_energy_mwh = context.energy_mwh;
    }

    /// Replaces the tariff pricing this transaction (I11.FR.08). Everything charged so far is
    /// sealed at the old tariff's tax rates and a new charging period opens.
    fn switch_tariff(&mut self, tariff: &Tariff, at: DateTime<Utc>) {
        for accumulator in self.accumulators() {
            accumulator.seal();
        }
        self.tariff_id = tariff.id.clone();
        self.currency = tariff.currency.clone();
        self.used_elements = UsedElements::default();
        self.open_period(tariff.id.clone(), at);
    }

    fn accumulators(&mut self) -> [&mut Accumulator; 6] {
        [
            &mut self.fixed,
            &mut self.energy,
            &mut self.charging_time,
            &mut self.idle_time,
            &mut self.reservation_time,
            &mut self.reservation_fixed,
        ]
    }

    /// Charges one sub-interval: `secs` of time and `energy_mwh` of energy, at the prices whose
    /// conditions hold at `from`.
    fn charge(
        &mut self,
        tariff: &Tariff,
        context: &PricingContext,
        from: DateTime<Utc>,
        secs: i64,
        energy_mwh: i64,
    ) {
        let snapshot = Snapshot {
            at: from,
            energy_mwh: self.usage.energy_mwh,
            elapsed_secs: self.usage.charging_secs + self.usage.idle_secs,
            charging_secs: self.usage.charging_secs,
            idle_secs: self.usage.idle_secs,
        };
        let used = UsedElements {
            energy: tariff
                .energy
                .as_ref()
                .and_then(|component| select_energy(component, context, &snapshot)),
            charging_time: tariff
                .charging_time
                .as_ref()
                .and_then(|component| select_time(component, context, &snapshot)),
            idle_time: tariff
                .idle_time
                .as_ref()
                .and_then(|component| select_time(component, context, &snapshot)),
        };
        if used != self.used_elements {
            // I12.FR.35/.36 - the used element list changed, so this period ends and a new one
            // starts here. The very first evaluation of a transaction is not a change: `start`
            // already opened a period at the transaction's own start time.
            if !self.periods.is_empty() && self.used_elements != UsedElements::default() {
                self.open_period(tariff.id.clone(), from);
            }
            self.used_elements = used;
        }

        if !self.capped {
            if let (Some(component), Some(index)) = (&tariff.energy, used.energy) {
                let amount = energy_cost(component.prices[index].price_per_kwh, energy_mwh);
                self.energy.add(amount, &component.tax_rates);
            }
            let timed = if context.charging {
                tariff.charging_time.as_ref().zip(used.charging_time)
            } else {
                tariff.idle_time.as_ref().zip(used.idle_time)
            };
            if let Some((component, index)) = timed {
                let amount = time_cost(component.prices[index].price_per_minute, secs);
                if context.charging {
                    self.charging_time.add(amount, &component.tax_rates);
                } else {
                    self.idle_time.add(amount, &component.tax_rates);
                }
            }
        }

        self.usage.energy_mwh += energy_mwh;
        if context.charging {
            self.usage.charging_secs += secs;
        } else {
            self.usage.idle_secs += secs;
        }
        if let Some(period) = self.periods.last_mut() {
            period.energy_mwh += energy_mwh;
            if context.charging {
                period.charging_secs += secs;
            } else {
                period.idle_secs += secs;
            }
        }

        // I12.FR.39: checked after charging, because the requirement freezes the totals at the
        // value they had when `maxCost` was reached rather than clamping them to it.
        if !self.capped
            && let Some(max) = &tariff.max_cost
        {
            let total = self.raw_total();
            self.capped = reaches(total, max);
        }
    }

    fn open_period(&mut self, tariff_id: TariffId, at: DateTime<Utc>) {
        self.periods.push(CostPeriod {
            tariff_id,
            start: at,
            energy_mwh: 0,
            charging_secs: 0,
            idle_secs: 0,
            min_power_mw: None,
            max_power_mw: None,
            min_current_ma: None,
            max_current_ma: None,
        });
    }

    fn record_readings(&mut self, context: &PricingContext) {
        let Some(period) = self.periods.last_mut() else {
            return;
        };
        if let Some(power) = context.power_mw {
            period.min_power_mw = Some(period.min_power_mw.map_or(power, |low| low.min(power)));
            period.max_power_mw = Some(period.max_power_mw.map_or(power, |high| high.max(power)));
        }
        if let Some(current) = context.current_ma {
            period.min_current_ma = Some(
                period
                    .min_current_ma
                    .map_or(current, |low| low.min(current)),
            );
            period.max_current_ma = Some(
                period
                    .max_current_ma
                    .map_or(current, |high| high.max(current)),
            );
        }
    }

    fn raw_total(&self) -> DimensionCost {
        [
            &self.fixed,
            &self.energy,
            &self.charging_time,
            &self.idle_time,
            &self.reservation_time,
            &self.reservation_fixed,
        ]
        .into_iter()
        .map(Accumulator::total)
        .fold(DimensionCost::default(), DimensionCost::saturating_add)
    }

    /// The cost so far, with `minCost`/`maxCost` applied (I12.FR.17/.38/.39/.41).
    ///
    /// `tariff` is the one currently in force; only its `minCost`/`maxCost` are read here, since
    /// everything else has already been charged.
    pub fn totals(&self, tariff: &Tariff) -> CostTotals {
        let dimension = |accumulator: &Accumulator| {
            let total = accumulator.total();
            (!total.is_zero()).then_some(total)
        };
        let total = self.raw_total();

        // I12.FR.38: the minimum replaces the calculated cost outright, and reports it as a single
        // `fixed` amount - the specification is explicit that this is a replacement rather than a
        // top-up spread over the dimensions.
        if let Some(min) = &tariff.min_cost
            && !reaches(total, min)
        {
            let floor = DimensionCost {
                excl_tax: min.excl_tax.unwrap_or(total.excl_tax),
                incl_tax: min.incl_tax.unwrap_or(total.incl_tax),
            };
            return CostTotals {
                currency: self.currency.clone(),
                kind: TotalCostKind::Min,
                fixed: Some(floor),
                energy: None,
                charging_time: None,
                idle_time: None,
                reservation_time: None,
                reservation_fixed: None,
                total: floor,
            };
        }

        CostTotals {
            currency: self.currency.clone(),
            kind: if self.capped {
                TotalCostKind::Max
            } else {
                TotalCostKind::Normal
            },
            fixed: dimension(&self.fixed),
            energy: dimension(&self.energy),
            charging_time: dimension(&self.charging_time),
            idle_time: dimension(&self.idle_time),
            reservation_time: dimension(&self.reservation_time),
            reservation_fixed: dimension(&self.reservation_fixed),
            total,
        }
    }
}

/// Whether `total` has reached `bound` on either half. OCPP states `minCost`/`maxCost` as a
/// `PriceType` where either half may be absent, and an absent half is simply not a bound.
fn reaches(total: DimensionCost, bound: &Price) -> bool {
    bound.excl_tax.is_some_and(|limit| total.excl_tax >= limit)
        || bound.incl_tax.is_some_and(|limit| total.incl_tax >= limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{EnergyPrice, FixedPrice, TariffId, TaxPercent, TimeOfDay, TimePrice};
    use alloc::vec;
    use chrono::TimeZone;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a valid RFC 3339 instant")
    }

    fn tariff(id: &str) -> Tariff {
        Tariff::new(TariffId(id.into()), "EUR")
    }

    fn energy_only(price_per_kwh: Money) -> Tariff {
        let mut tariff = tariff("energy");
        tariff.energy = Some(EnergyComponent {
            prices: vec![EnergyPrice {
                price_per_kwh,
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });
        tariff
    }

    fn context(now: DateTime<Utc>, energy_mwh: i64, charging: bool) -> PricingContext {
        PricingContext {
            energy_mwh,
            charging,
            ..PricingContext::new(now)
        }
    }

    // The specification's own worked example: tariff 10, $0.25/kWh, 10 kWh charged.
    #[test]
    fn the_simple_kwh_example_totals_two_and_a_half() {
        let tariff = energy_only(Money(250_000));
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        cost.advance(
            &tariff,
            &context(at("2023-04-05T15:00:00Z"), 10_000_000, true),
        );

        let totals = cost.totals(&tariff);
        assert_eq!(totals.energy.unwrap().excl_tax, Money(2_500_000));
        assert_eq!(totals.total.excl_tax, Money(2_500_000));
        assert_eq!(cost.usage().energy_mwh, 10_000_000);
    }

    #[test]
    fn a_zero_energy_session_costs_nothing() {
        let tariff = energy_only(Money(250_000));
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        cost.advance(&tariff, &context(at("2023-04-05T15:00:00Z"), 0, true));

        let totals = cost.totals(&tariff);
        assert_eq!(totals.total, DimensionCost::default());
        assert!(totals.energy.is_none());
    }

    // The specification's tariff 11 example: 0.40/kWh between 08:00 and 18:00, 0.25 outside it.
    // Charging 10 kWh across 18:00 must split into 6 kWh high and 4 kWh low - here the split is
    // by time, so a session running 17:30-18:20 at a constant rate pays 30 minutes at the day
    // price and 20 at the night price.
    #[test]
    fn a_transaction_crossing_a_window_edge_is_split_at_the_edge() {
        let mut tariff = tariff("11");
        tariff.energy = Some(EnergyComponent {
            prices: vec![
                EnergyPrice {
                    price_per_kwh: Money(400_000),
                    conditions: Some(TariffConditions {
                        start_time_of_day: TimeOfDay::new(8, 0),
                        end_time_of_day: TimeOfDay::new(18, 0),
                        ..Default::default()
                    }),
                },
                EnergyPrice {
                    price_per_kwh: Money(250_000),
                    conditions: None,
                },
            ],
            tax_rates: Vec::new(),
        });
        let start = at("2023-04-05T17:30:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        // 10 kWh over 50 minutes: 30 minutes before 18:00, 20 after.
        cost.advance(
            &tariff,
            &context(at("2023-04-05T18:20:00Z"), 10_000_000, true),
        );

        // 6 kWh @ 0.40 = 2.40, 4 kWh @ 0.25 = 1.00.
        assert_eq!(
            cost.totals(&tariff).energy.unwrap().excl_tax,
            Money(3_400_000)
        );
        assert_eq!(cost.periods().len(), 2);
        assert_eq!(cost.periods()[1].start, at("2023-04-05T18:00:00Z"));
        assert_eq!(cost.periods()[0].energy_mwh, 6_000_000);
        assert_eq!(cost.periods()[1].energy_mwh, 4_000_000);
    }

    #[test]
    fn a_transaction_spanning_midnight_changes_day_of_week() {
        let mut tariff = tariff("weekend");
        tariff.energy = Some(EnergyComponent {
            prices: vec![
                EnergyPrice {
                    price_per_kwh: Money(100_000),
                    conditions: Some(TariffConditions {
                        day_of_week: vec![chrono::Weekday::Sat],
                        ..Default::default()
                    }),
                },
                EnergyPrice {
                    price_per_kwh: Money(500_000),
                    conditions: None,
                },
            ],
            tax_rates: Vec::new(),
        });
        // 2023-04-08 is a Saturday; 23:00 Saturday to 01:00 Sunday, 2 kWh at a constant rate.
        let start = at("2023-04-08T23:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        cost.advance(
            &tariff,
            &context(at("2023-04-09T01:00:00Z"), 2_000_000, true),
        );

        // 1 kWh @ 0.10 (Saturday) + 1 kWh @ 0.50 (Sunday).
        assert_eq!(
            cost.totals(&tariff).energy.unwrap().excl_tax,
            Money(600_000)
        );
    }

    #[test]
    fn a_window_that_wraps_midnight_covers_both_sides_of_it() {
        let night = TariffConditions {
            start_time_of_day: TimeOfDay::new(22, 0),
            end_time_of_day: TimeOfDay::new(6, 0),
            ..Default::default()
        };
        let context = PricingContext::new(at("2023-04-05T23:30:00Z"));
        let snapshot = Snapshot {
            at: context.now,
            energy_mwh: 0,
            elapsed_secs: 0,
            charging_secs: 0,
            idle_secs: 0,
        };
        assert!(conditions_apply(&night, &context, &snapshot));

        let morning = PricingContext::new(at("2023-04-05T05:59:00Z"));
        assert!(conditions_apply(
            &night,
            &morning,
            &Snapshot {
                at: morning.now,
                ..snapshot
            }
        ));

        let midday = PricingContext::new(at("2023-04-05T12:00:00Z"));
        assert!(!conditions_apply(
            &night,
            &midday,
            &Snapshot {
                at: midday.now,
                ..snapshot
            }
        ));
    }

    #[test]
    fn an_end_of_midnight_means_the_end_of_the_day() {
        let evening = TariffConditions {
            start_time_of_day: TimeOfDay::new(18, 0),
            end_time_of_day: TimeOfDay::new(0, 0),
            ..Default::default()
        };
        let context = PricingContext::new(at("2023-04-05T23:59:00Z"));
        let snapshot = Snapshot {
            at: context.now,
            energy_mwh: 0,
            elapsed_secs: 0,
            charging_secs: 0,
            idle_secs: 0,
        };
        assert!(conditions_apply(&evening, &context, &snapshot));
    }

    #[test]
    fn the_window_edge_itself_belongs_to_the_window_that_starts() {
        let day = TariffConditions {
            start_time_of_day: TimeOfDay::new(8, 0),
            end_time_of_day: TimeOfDay::new(18, 0),
            ..Default::default()
        };
        let snapshot = |at: DateTime<Utc>| Snapshot {
            at,
            energy_mwh: 0,
            elapsed_secs: 0,
            charging_secs: 0,
            idle_secs: 0,
        };
        let opening = PricingContext::new(at("2023-04-05T08:00:00Z"));
        assert!(conditions_apply(&day, &opening, &snapshot(opening.now)));
        let closing = PricingContext::new(at("2023-04-05T18:00:00Z"));
        assert!(!conditions_apply(&day, &closing, &snapshot(closing.now)));
    }

    #[test]
    fn local_time_shifts_the_window_off_utc() {
        let day = TariffConditions {
            start_time_of_day: TimeOfDay::new(8, 0),
            end_time_of_day: TimeOfDay::new(18, 0),
            ..Default::default()
        };
        // 07:30 UTC is 09:30 in a +02:00 zone, so the daytime price applies.
        let context = PricingContext {
            local_offset_minutes: 120,
            ..PricingContext::new(at("2023-04-05T07:30:00Z"))
        };
        let snapshot = Snapshot {
            at: context.now,
            energy_mwh: 0,
            elapsed_secs: 0,
            charging_secs: 0,
            idle_secs: 0,
        };
        assert!(conditions_apply(&day, &context, &snapshot));
    }

    #[test]
    fn an_energy_threshold_switches_price_mid_session() {
        let mut tariff = tariff("stepped");
        tariff.energy = Some(EnergyComponent {
            prices: vec![
                EnergyPrice {
                    price_per_kwh: Money(100_000),
                    conditions: Some(TariffConditions {
                        max_energy_mwh: Some(5_000_000),
                        ..Default::default()
                    }),
                },
                EnergyPrice {
                    price_per_kwh: Money(500_000),
                    conditions: None,
                },
            ],
            tax_rates: Vec::new(),
        });
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        // Sampled every minute, 1 kWh each: the first five are cheap, the next five are not.
        for minute in 1..=10 {
            let now = start + TimeDelta::minutes(minute);
            cost.advance(&tariff, &context(now, minute * 1_000_000, true));
        }

        // 5 kWh @ 0.10 + 5 kWh @ 0.50.
        assert_eq!(
            cost.totals(&tariff).energy.unwrap().excl_tax,
            Money(3_000_000)
        );
    }

    #[test]
    fn a_fixed_fee_is_charged_once_at_the_start() {
        let mut tariff = energy_only(Money(250_000));
        tariff.fixed_fee = Some(FixedComponent {
            prices: vec![FixedPrice {
                price: Money(2_500_000),
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        cost.advance(&tariff, &context(start + TimeDelta::hours(1), 0, true));
        cost.advance(&tariff, &context(start + TimeDelta::hours(2), 0, true));

        assert_eq!(
            cost.totals(&tariff).fixed.unwrap().excl_tax,
            Money(2_500_000)
        );
        assert_eq!(cost.totals(&tariff).total.excl_tax, Money(2_500_000));
    }

    #[test]
    fn idle_time_is_charged_separately_from_charging_time() {
        let mut tariff = tariff("timed");
        tariff.charging_time = Some(TimeComponent {
            prices: vec![TimePrice {
                price_per_minute: Money(1_000_000),
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });
        tariff.idle_time = Some(TimeComponent {
            prices: vec![TimePrice {
                price_per_minute: Money(2_000_000),
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        cost.advance(&tariff, &context(start + TimeDelta::minutes(10), 0, true));
        cost.advance(&tariff, &context(start + TimeDelta::minutes(15), 0, false));

        let totals = cost.totals(&tariff);
        assert_eq!(totals.charging_time.unwrap().excl_tax, Money(10_000_000));
        assert_eq!(totals.idle_time.unwrap().excl_tax, Money(10_000_000));
        assert_eq!(cost.usage().charging_secs, 600);
        assert_eq!(cost.usage().idle_secs, 300);
    }

    #[test]
    fn taxes_stack_on_the_result_of_the_stack_below() {
        // 100.00 net, 10% at stack 0 and 20% at stack 1: 100 -> 110 -> 132.
        let rates = alloc::vec![
            TaxRate {
                kind: "energy".into(),
                percent: TaxPercent(10_000_000),
                stack: 0,
            },
            TaxRate {
                kind: "vat".into(),
                percent: TaxPercent(20_000_000),
                stack: 1,
            },
        ];
        assert_eq!(apply_taxes(Money(100_000_000), &rates), Money(132_000_000));
    }

    #[test]
    fn two_taxes_at_the_same_level_both_apply_to_the_net() {
        // The specification's tariff 10: 6% federal and 4% state on 2.50 is 2.75, not 2.7515.
        let rates = alloc::vec![
            TaxRate {
                kind: "federal".into(),
                percent: TaxPercent(6_000_000),
                stack: 0,
            },
            TaxRate {
                kind: "state".into(),
                percent: TaxPercent(4_000_000),
                stack: 0,
            },
        ];
        assert_eq!(apply_taxes(Money(2_500_000), &rates), Money(2_750_000));
    }

    #[test]
    fn a_tax_stack_truncates_rather_than_rounds_up() {
        // 1 micro-unit at 50%: 1.5 micro-units, charged as 1.
        let rates = alloc::vec![TaxRate {
            kind: "vat".into(),
            percent: TaxPercent(50_000_000),
            stack: 0,
        }];
        assert_eq!(apply_taxes(Money(1), &rates), Money(1));
    }

    #[test]
    fn a_minimum_cost_replaces_a_cheaper_total() {
        let mut tariff = energy_only(Money(250_000));
        tariff.min_cost = Some(Price {
            excl_tax: Some(Money(5_000_000)),
            incl_tax: None,
        });
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);
        cost.advance(
            &tariff,
            &context(start + TimeDelta::hours(1), 1_000_000, true),
        );

        let totals = cost.totals(&tariff);
        assert_eq!(totals.kind, TotalCostKind::Min);
        assert_eq!(totals.total.excl_tax, Money(5_000_000));
        assert_eq!(totals.fixed.unwrap().excl_tax, Money(5_000_000));
        assert!(totals.energy.is_none());
        // I12.FR.38: usage is unaffected by the substitution.
        assert_eq!(cost.usage().energy_mwh, 1_000_000);
    }

    #[test]
    fn a_minimum_cost_leaves_a_dearer_total_alone() {
        let mut tariff = energy_only(Money(250_000));
        tariff.min_cost = Some(Price {
            excl_tax: Some(Money(1_000_000)),
            incl_tax: None,
        });
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);
        cost.advance(
            &tariff,
            &context(start + TimeDelta::hours(1), 10_000_000, true),
        );

        assert_eq!(cost.totals(&tariff).kind, TotalCostKind::Normal);
        assert_eq!(cost.totals(&tariff).total.excl_tax, Money(2_500_000));
    }

    #[test]
    fn a_maximum_cost_freezes_the_total_but_not_the_usage() {
        let mut tariff = energy_only(Money(1_000_000));
        tariff.max_cost = Some(Price {
            excl_tax: Some(Money(3_000_000)),
            incl_tax: None,
        });
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        for minute in 1..=10 {
            cost.advance(
                &tariff,
                &context(start + TimeDelta::minutes(minute), minute * 1_000_000, true),
            );
        }

        let totals = cost.totals(&tariff);
        assert_eq!(totals.kind, TotalCostKind::Max);
        assert_eq!(totals.total.excl_tax, Money(3_000_000));
        // I12.FR.40: usage keeps accumulating so the receipt still shows what was delivered.
        assert_eq!(cost.usage().energy_mwh, 10_000_000);
    }

    #[test]
    fn a_new_tariff_prices_only_what_follows_it() {
        let first = energy_only(Money(1_000_000));
        let mut second = energy_only(Money(2_000_000));
        second.id = TariffId("second".into());
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&first, &context(start, 0, true), None);

        cost.advance(
            &first,
            &context(start + TimeDelta::hours(1), 1_000_000, true),
        );
        cost.advance(
            &second,
            &context(start + TimeDelta::hours(2), 2_000_000, true),
        );

        // 1 kWh at 1.00 under the old tariff, 1 kWh at 2.00 under the new one - never
        // retroactively (I11.FR.08).
        assert_eq!(
            cost.totals(&second).energy.unwrap().excl_tax,
            Money(3_000_000)
        );
        assert_eq!(cost.periods().len(), 2);
        assert_eq!(cost.periods()[1].tariff_id, TariffId("second".into()));
    }

    #[test]
    fn a_clock_that_went_backwards_charges_no_time() {
        let mut tariff = tariff("timed");
        tariff.charging_time = Some(TimeComponent {
            prices: vec![TimePrice {
                price_per_minute: Money(1_000_000),
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        cost.advance(&tariff, &context(start - TimeDelta::minutes(5), 0, true));

        assert_eq!(cost.usage().charging_secs, 0);
        assert_eq!(cost.totals(&tariff).total, DimensionCost::default());
    }

    #[test]
    fn a_negative_threshold_reads_as_distance_from_zero() {
        // The specification's own example: a discharging range from -10 kW to -50 kW.
        assert!(min_satisfied(-20_000_000, -10_000_000));
        assert!(max_satisfied(-20_000_000, -50_000_000));
        assert!(!min_satisfied(-5_000_000, -10_000_000));
        assert!(!max_satisfied(-50_000_000, -50_000_000));
    }

    #[test]
    fn an_unmeasurable_condition_is_not_met() {
        let needs_power = TariffConditions {
            min_power_mw: Some(1),
            ..Default::default()
        };
        let context = PricingContext::new(at("2023-04-05T14:00:00Z"));
        let snapshot = Snapshot {
            at: context.now,
            energy_mwh: 0,
            elapsed_secs: 0,
            charging_secs: 0,
            idle_secs: 0,
        };

        assert!(!conditions_apply(&needs_power, &context, &snapshot));
    }

    #[test]
    fn a_reservation_is_charged_once_at_the_start() {
        let mut tariff = energy_only(Money(250_000));
        tariff.reservation_time = Some(TimeComponent {
            prices: vec![TimePrice {
                price_per_minute: Money(100_000),
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });
        tariff.reservation_fixed = Some(FixedComponent {
            prices: vec![FixedPrice {
                price: Money(1_000_000),
                conditions: None,
            }],
            tax_rates: Vec::new(),
        });
        let start = at("2023-04-05T14:00:00Z");

        let cost = TransactionCost::start(&tariff, &context(start, 0, true), Some(1_800));

        let totals = cost.totals(&tariff);
        // 30 minutes at 0.10 per minute.
        assert_eq!(totals.reservation_time.unwrap().excl_tax, Money(3_000_000));
        assert_eq!(totals.reservation_fixed.unwrap().excl_tax, Money(1_000_000));
        assert_eq!(cost.usage().reservation_secs, Some(1_800));
    }

    #[test]
    fn the_total_is_the_sum_of_every_dimension() {
        let mut tariff = energy_only(Money(250_000));
        tariff.fixed_fee = Some(FixedComponent {
            prices: vec![FixedPrice {
                price: Money(1_000_000),
                conditions: None,
            }],
            tax_rates: alloc::vec![TaxRate {
                kind: "vat".into(),
                percent: TaxPercent(10_000_000),
                stack: 0,
            }],
        });
        let start = at("2023-04-05T14:00:00Z");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);
        cost.advance(
            &tariff,
            &context(start + TimeDelta::hours(1), 4_000_000, true),
        );

        let totals = cost.totals(&tariff);
        assert_eq!(totals.total.excl_tax, Money(2_000_000));
        // The fixed fee carries 10% VAT; the energy carries none.
        assert_eq!(totals.total.incl_tax, Money(2_100_000));
    }

    #[test]
    fn a_tariff_with_no_priced_structure_costs_nothing() {
        let tariff = tariff("empty");
        let start = Utc.timestamp_opt(0, 0).single().expect("valid instant");
        let mut cost = TransactionCost::start(&tariff, &context(start, 0, true), None);

        cost.advance(
            &tariff,
            &context(start + TimeDelta::hours(3), 50_000_000, true),
        );

        assert_eq!(cost.totals(&tariff).total, DimensionCost::default());
    }
}
