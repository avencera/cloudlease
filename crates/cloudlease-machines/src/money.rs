//! Prices, accrued spend and when a run reaches its cap
//!
//! DigitalOcean bills droplets per second with a minimum of 60 seconds or $0.01, whichever
//! is higher, and volumes hourly (docs.digitalocean.com droplet and volume pricing pages,
//! checked 2026-11-24). A class's price is its droplet and volume together per hour, and
//! spend is billed per millisecond from the lease's creation until its resources are
//! confirmed gone, with the same minimum. That counts from before the provider create until after the delete, so it never
//! undercounts what DigitalOcean bills.

use std::{fmt, time::Duration};

use cloudlease_core::{capacity::PermitUnits, time::UnixMillis};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The shortest billed lifetime of a droplet
pub const MINIMUM_BILLED: Duration = Duration::from_secs(60);

/// The least DigitalOcean charges for a droplet, whatever its price and life
pub const MINIMUM_CHARGE: MicroUsd = MicroUsd(MICROS_PER_CENT);

const MICROS_PER_DOLLAR: u64 = 1_000_000;
const MICROS_PER_CENT: u64 = 10_000;
const MILLIS_PER_HOUR: u128 = 3_600_000;

/// A rejected amount
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MoneyError {
    /// The amount is not a plain decimal dollar figure
    #[error("{0:?} is not a dollar amount such as \"0.476\" (at most 6 decimals)")]
    Invalid(String),
    /// The hourly price is too large to weigh a capacity permit
    #[error("an hourly price above $655.35 cannot weigh a capacity permit")]
    TooLarge,
}

/// An amount in millionths of a US dollar
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct MicroUsd(u64);

impl MicroUsd {
    /// Nothing
    pub const ZERO: Self = Self(0);

    /// Creates an amount from millionths of a dollar
    #[must_use]
    pub const fn new(micros: u64) -> Self {
        Self(micros)
    }

    /// Returns millionths of a dollar
    #[must_use]
    pub const fn micros(self) -> u64 {
        self.0
    }

    /// Parses a decimal dollar figure such as `"25"` or `"0.476"`
    pub fn parse_dollars(value: &str) -> Result<Self, MoneyError> {
        let invalid = || MoneyError::Invalid(value.to_owned());
        let (whole, fraction) = match value.split_once('.') {
            Some((_, "")) => return Err(invalid()),
            Some(parts) => parts,
            None => (value, ""),
        };
        let digits = |part: &str| part.chars().all(|character| character.is_ascii_digit());
        if whole.is_empty() || !digits(whole) || !digits(fraction) || fraction.len() > 6 {
            return Err(invalid());
        }

        let whole: u64 = whole.parse().map_err(|_| invalid())?;
        let fraction: u64 = format!("{fraction:0<6}").parse().map_err(|_| invalid())?;

        whole
            .checked_mul(MICROS_PER_DOLLAR)
            .and_then(|micros| micros.checked_add(fraction))
            .map(Self)
            .ok_or_else(invalid)
    }

    /// Creates an amount from whole US cents, or `None` past what fits
    #[must_use]
    pub const fn from_cents(cents: u64) -> Option<Self> {
        match cents.checked_mul(MICROS_PER_CENT) {
            Some(micros) => Some(Self(micros)),
            None => None,
        }
    }

    /// Returns the whole cents in the amount, rounded down
    #[must_use]
    pub const fn whole_cents(self) -> u64 {
        self.0 / MICROS_PER_CENT
    }

    /// Adds without overflow
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// Returns the weight of an hourly price in a spend gate whose units are cents per hour,
    /// rounded up so the gate never undercounts
    pub fn cents_per_hour(self) -> Result<PermitUnits, MoneyError> {
        let cents = self.0.div_ceil(MICROS_PER_CENT).max(1);
        let cents = u16::try_from(cents).map_err(|_| MoneyError::TooLarge)?;

        PermitUnits::new(cents).map_err(|_| MoneyError::TooLarge)
    }
}

impl fmt::Display for MicroUsd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cents = self.0.div_ceil(MICROS_PER_CENT);
        write!(formatter, "${}.{:02}", cents / 100, cents % 100)
    }
}

/// What a resource at `price_per_hour` costs from `from` to `to`: at least
/// [`MINIMUM_BILLED`] of its time, and at least [`MINIMUM_CHARGE`]
#[must_use]
pub fn billed(price_per_hour: MicroUsd, from: UnixMillis, to: UnixMillis) -> MicroUsd {
    let elapsed = to.as_millis().saturating_sub(from.as_millis());
    let minimum = u64::try_from(MINIMUM_BILLED.as_millis()).unwrap_or(u64::MAX);
    let billed_millis = u128::from(elapsed.max(minimum));
    let micros = (u128::from(price_per_hour.0) * billed_millis).div_ceil(MILLIS_PER_HOUR);

    MicroUsd(u64::try_from(micros).unwrap_or(u64::MAX)).max(MINIMUM_CHARGE)
}

/// One machine's billed life: from its lease's creation until its resources are gone
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BilledLife {
    /// When the lease was created, before the provider create
    pub from: UnixMillis,
    /// When every resource was confirmed gone, or `None` while any may exist
    pub gone_at: Option<UnixMillis>,
}

/// A run's spend at one moment
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Spend {
    /// What the run's machines have cost so far
    pub accrued: MicroUsd,
    /// What the run's machines that may still exist add per hour
    pub per_hour: MicroUsd,
}

impl Spend {
    /// Adds up the machines of a run whose class costs `price_per_hour`
    #[must_use]
    pub fn of(
        price_per_hour: MicroUsd,
        machines: impl IntoIterator<Item = BilledLife>,
        now: UnixMillis,
    ) -> Self {
        machines.into_iter().fold(
            Self {
                accrued: MicroUsd::ZERO,
                per_hour: MicroUsd::ZERO,
            },
            |spend, machine| Self {
                accrued: spend.accrued.saturating_add(billed(
                    price_per_hour,
                    machine.from,
                    machine.gone_at.unwrap_or(now),
                )),
                per_hour: if machine.gone_at.is_some() {
                    spend.per_hour
                } else {
                    spend.per_hour.saturating_add(price_per_hour)
                },
            },
        )
    }

    /// Reports whether the run has spent its cap
    #[must_use]
    pub fn has_reached(&self, cap: MicroUsd) -> bool {
        self.accrued >= cap
    }

    /// Returns when the accrued spend reaches `cap` if nothing changes, or `None` when
    /// nothing is accruing
    ///
    /// Machines younger than the minimum accrue nothing until it passes, so this is never
    /// later than the real crossing; an early check finds the cap not reached and asks again.
    #[must_use]
    pub fn reaches_cap_at(&self, cap: MicroUsd, now: UnixMillis) -> Option<UnixMillis> {
        if self.has_reached(cap) {
            return Some(now);
        }
        if self.per_hour == MicroUsd::ZERO {
            return None;
        }

        let left = u128::from(cap.0 - self.accrued.0);
        let millis = (left * MILLIS_PER_HOUR).div_ceil(u128::from(self.per_hour.0));
        let millis = u64::try_from(millis).unwrap_or(u64::MAX);

        Some(now.saturating_add(Duration::from_millis(millis)))
    }
}

#[cfg(test)]
mod tests {
    use cloudlease_core::time::UnixMillis;

    use super::{BilledLife, MicroUsd, MoneyError, Spend, billed};

    fn at(millis: u64) -> UnixMillis {
        UnixMillis::new(millis).unwrap()
    }

    fn dollars(value: &str) -> MicroUsd {
        MicroUsd::parse_dollars(value).unwrap()
    }

    #[test]
    fn dollar_figures_parse_exactly_and_weigh_permits_in_whole_cents() {
        assert_eq!(dollars("25").micros(), 25_000_000);
        assert_eq!(dollars("0.476").micros(), 476_000);
        assert_eq!(dollars("0.000001").micros(), 1);
        for invalid in ["", ".5", "1.", "-1", "1e3", "0.0000001", "$1", "1,00"] {
            assert!(MicroUsd::parse_dollars(invalid).is_err(), "{invalid}");
        }

        assert_eq!(dollars("0.476").cents_per_hour().unwrap().get(), 48);
        // a price under a cent still takes a unit, so it cannot slip past the cap
        assert_eq!(dollars("0.006").cents_per_hour().unwrap().get(), 1);
        assert_eq!(
            dollars("655.36").cents_per_hour(),
            Err(MoneyError::TooLarge)
        );
        assert_eq!(dollars("12.341").to_string(), "$12.35");
    }

    #[test]
    fn a_machine_is_billed_from_creation_to_gone_with_a_minute_and_cent_minimum() {
        let price = dollars("36"); // a cent a second
        assert_eq!(billed(price, at(0), at(1_000)), dollars("0.60"));
        assert_eq!(billed(price, at(0), at(90_000)), dollars("0.90"));
        assert_eq!(billed(price, at(5_000), at(0)), dollars("0.60"));

        // a minute of a $0.125/h droplet is about $0.002, but DigitalOcean charges a cent
        let cheap = dollars("0.125");
        assert_eq!(billed(cheap, at(0), at(60_000)), dollars("0.01"));
        assert_eq!(billed(cheap, at(0), at(3_600_000)), dollars("0.125"));
    }

    #[test]
    fn a_run_reaches_its_cap_when_its_live_machines_have_accrued_it() {
        let price = dollars("36");
        let cap = dollars("3");
        let machines = [
            // gone after 100 s: $1.00, adds nothing per hour
            BilledLife {
                from: at(0),
                gone_at: Some(at(100_000)),
            },
            // live for 50 s: still at its $0.60 minimum
            BilledLife {
                from: at(150_000),
                gone_at: None,
            },
        ];

        let spend = Spend::of(price, machines, at(200_000));
        assert_eq!(spend.accrued, dollars("1.60"));
        assert_eq!(spend.per_hour, price);
        assert!(!spend.has_reached(cap));
        // the projection ignores the minimum, so it is early, never late
        let projected = spend.reaches_cap_at(cap, at(200_000)).unwrap();
        assert_eq!(projected, at(340_000));
        let later = Spend::of(price, machines, projected);
        assert!(later.accrued <= cap);
        assert!(Spend::of(price, machines, at(390_000)).has_reached(cap));

        let idle = Spend::of(price, [machines[0]], at(200_000));
        assert_eq!(idle.reaches_cap_at(cap, at(200_000)), None);
        assert_eq!(
            idle.reaches_cap_at(dollars("1"), at(200_000)),
            Some(at(200_000))
        );
    }
}
