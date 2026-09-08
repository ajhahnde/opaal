//! Distinct wall-clock timestamps and monotonic observations.

use std::fmt;

use opaal_platform::Platform;
use opaal_platform::operational::OperationalAdapter;

use crate::authority::{CapabilityRequest, EffectSet};
use crate::context::OperationalContext;

use super::{ModuleError, adapter_error, authorize};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(i128);

impl Timestamp {
    pub fn from_unix_nanos(value: i128) -> Result<Self, ModuleError> {
        let seconds = value.div_euclid(1_000_000_000);
        let days = seconds.div_euclid(86_400);
        let (year, _, _) = civil_from_days(days).ok_or_else(|| {
            ModuleError::invalid("TIME001", "wall timestamp is outside the RFC 3339 range")
        })?;
        if !(0..=9999).contains(&year) {
            return Err(ModuleError::invalid(
                "TIME001",
                "wall timestamp is outside the RFC 3339 range",
            ));
        }
        Ok(Self(value))
    }
    #[must_use]
    pub const fn unix_nanos(self) -> i128 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.div_euclid(1_000_000_000);
        let nanos = self.0.rem_euclid(1_000_000_000);
        let days = seconds.div_euclid(86_400);
        let seconds_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days).ok_or(fmt::Error)?;
        let hour = seconds_of_day / 3_600;
        let minute = seconds_of_day % 3_600 / 60;
        let second = seconds_of_day % 60;
        write!(
            formatter,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{nanos:09}Z"
        )
    }
}

fn civil_from_days(days: i128) -> Option<(i128, i128, i128)> {
    let shifted = days.checked_add(719_468)?;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.checked_sub(era.checked_mul(146_097)?)?;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era.checked_add(era.checked_mul(400)?)?;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i128::from(month <= 2);
    Some((year, month, day))
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MonotonicInstant(u128);

impl MonotonicInstant {
    #[must_use]
    pub const fn from_nanos(value: u128) -> Self {
        Self(value)
    }
    #[must_use]
    pub const fn as_nanos(self) -> u128 {
        self.0
    }
}

pub fn wall_now(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
) -> Result<Timestamp, ModuleError> {
    let request = CapabilityRequest::clock_wall();
    authorize(context, effects, &request, platform)?;
    let value = adapter
        .wall_time_unix_nanos()
        .map_err(|error| adapter_error(context, error))?;
    if let Some(reason) = context.poll_cancellation() {
        return Err(ModuleError::Cancelled(reason));
    }
    Timestamp::from_unix_nanos(value)
}

pub fn monotonic_now(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
) -> Result<MonotonicInstant, ModuleError> {
    let request = CapabilityRequest::clock_monotonic();
    authorize(context, effects, &request, platform)?;
    let value = adapter
        .monotonic_nanos()
        .map_err(|error| adapter_error(context, error))?;
    if let Some(reason) = context.poll_cancellation() {
        return Err(ModuleError::Cancelled(reason));
    }
    Ok(MonotonicInstant(value))
}
