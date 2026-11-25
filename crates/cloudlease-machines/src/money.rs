//! Prices, billing, accrued spend and when a run reaches its cap
//!
//! A class's price is per hour, and its [`Billing`] says how its provider rounds a machine's
//! life: every started increment is billed in full, with a minimum.
//!
//! - DigitalOcean bills droplets per second with a minimum of 60 seconds or $0.01, whichever
//!   is higher, and volumes hourly (docs.digitalocean.com droplet and volume pricing pages,
//!   checked 2026-11-24).
//! - phoenixNAP bills every started hour in full, with a minimum of one hour, from deploy until
//!   the server is deleted (phoenixNAP billing knowledge base, checked 2026-11-24).
//! - Latitude.sh documents an hourly price but not how it rounds a partial hour, so it is
//!   counted like phoenixNAP until a live test shows otherwise.
//! - AWS bills a Linux instance per second with a minimum of 60 seconds, and charges nothing
//!   for a Spot instance it interrupts in its first hour (EC2 pricing and Spot billing pages,
//!   as cited in the batch-compute-providers research of 2026-11-24). The root volume and
//!   public IPv4 address bill separately, so an AWS class's price includes them. Data sent
//!   out to the internet, about $0.09 per GB, is billed too but not counted here.
//!
//! Spend counts from the lease's creation, before the provider create, until its resources
//! are confirmed gone, so it never undercounts what the provider bills.

use std::{fmt, time::Duration};

use cloudlease_core::{capacity::PermitUnits, time::UnixMillis};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MICROS_PER_DOLLAR: u64 = 1_000_000;
const MICROS_PER_CENT: u64 = 10_000;
const MILLIS_PER_HOUR: u128 = 3_600_000;
const SECS_PER_HOUR: u64 = 3_600;

/// The longest billing increment a class may set
pub const MAX_BILLING_INCREMENT: Duration = Duration::from_secs(24 * 60 * 60);

/// A rejected amount
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MoneyError {
    /// The amount is not a plain decimal dollar figure
    #[error("{0:?} is not a dollar amount such as \"0.476\" (at most 6 decimals)")]
    Invalid(String),
    /// The hourly price is too large to weigh a capacity permit
    #[error("an hourly price above $655.35 cannot weigh a capacity permit")]
    TooLarge,
    /// A billing increment that is not a whole number of the provider's, or over a day
    #[error("the billing increment must be a whole multiple of {0} seconds, at most 86400")]
    Increment(u64),
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

/// How a provider turns a machine's life into billed time
///
/// Every started increment is billed in full, so a machine is billed from the first instant
/// of each increment; the billed time is at least the minimum, and the charge at least the
/// minimum charge. The minimum is always a whole number of increments.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Billing {
    increment_secs: u64,
    minimum_secs: u64,
    minimum_charge: MicroUsd,
}

impl Billing {
    /// DigitalOcean: per second, at least 60 seconds and $0.01
    pub const DIGITALOCEAN: Self = Self {
        increment_secs: 1,
        minimum_secs: 60,
        minimum_charge: MicroUsd(MICROS_PER_CENT),
    };

    /// phoenixNAP: every started hour, at least one hour
    pub const PHOENIXNAP: Self = Self {
        increment_secs: SECS_PER_HOUR,
        minimum_secs: SECS_PER_HOUR,
        minimum_charge: MicroUsd::ZERO,
    };

    /// Latitude.sh: every started hour, at least one hour, until its rounding is known
    pub const LATITUDE: Self = Self::PHOENIXNAP;

    /// AWS EC2 on Linux: every started second, at least 60 seconds, with no minimum charge
    pub const AWS: Self = Self {
        increment_secs: 1,
        minimum_secs: 60,
        minimum_charge: MicroUsd::ZERO,
    };

    /// Bills in a coarser increment than the provider's, such as a class priced by the hour
    ///
    /// The increment is a whole number of the provider's increments, at most
    /// [`MAX_BILLING_INCREMENT`]. Only a whole multiple never undercounts: a finer one bills
    /// less than the provider, and so does 90 minutes over hourly billing at minute 61. The
    /// minimum is rounded up to a whole number of the new increments.
    pub fn with_increment(self, increment: Duration) -> Result<Self, MoneyError> {
        let secs = increment.as_secs();
        let whole =
            increment.subsec_nanos() == 0 && secs > 0 && secs.is_multiple_of(self.increment_secs);
        if !whole || increment > MAX_BILLING_INCREMENT {
            return Err(MoneyError::Increment(self.increment_secs));
        }

        Ok(Self {
            increment_secs: secs,
            minimum_secs: self.minimum_secs.div_ceil(secs) * secs,
            ..self
        })
    }

    /// The length of one billed increment
    #[must_use]
    pub const fn increment(self) -> Duration {
        Duration::from_secs(self.increment_secs)
    }

    const fn increment_millis(self) -> u64 {
        self.increment_secs * 1_000
    }

    /// What a resource at `price_per_hour` costs from `from` to `to`
    #[must_use]
    pub fn billed(self, price_per_hour: MicroUsd, from: UnixMillis, to: UnixMillis) -> MicroUsd {
        let elapsed = to.as_millis().saturating_sub(from.as_millis());
        let increment = self.increment_millis();
        let started = elapsed / increment + 1;
        let billed_millis = (started.saturating_mul(increment)).max(self.minimum_secs * 1_000);
        let micros =
            (u128::from(price_per_hour.0) * u128::from(billed_millis)).div_ceil(MILLIS_PER_HOUR);

        MicroUsd(u64::try_from(micros).unwrap_or(u64::MAX)).max(self.minimum_charge)
    }

    /// What a machine at `price_per_hour` costs however short its life
    #[must_use]
    pub fn minimum(self, price_per_hour: MicroUsd) -> MicroUsd {
        let now = UnixMillis::new(0).unwrap_or_else(|| unreachable!("zero is a valid time"));
        self.billed(price_per_hour, now, now)
    }

    /// How long after creation a machine's bill first grows past its minimum, or earlier
    ///
    /// It is never later than the real moment, so a spend check armed for it is never late.
    #[must_use]
    pub fn accrues_after(self, price_per_hour: MicroUsd) -> Duration {
        let minimum = Duration::from_secs(self.minimum_secs);
        if price_per_hour == MicroUsd::ZERO {
            return minimum;
        }

        // the bill reaches the minimum charge at this time; it grows at the start of the
        // increment that passes it, which is no earlier than the increment it falls in
        let charge_millis =
            u128::from(self.minimum_charge.0) * MILLIS_PER_HOUR / u128::from(price_per_hour.0);
        let increment = u128::from(self.increment_millis());
        let charge_millis = charge_millis / increment * increment;
        let charge = Duration::from_millis(u64::try_from(charge_millis).unwrap_or(u64::MAX));

        charge.max(minimum)
    }

    /// Returns when the increment after the one running at `now` starts, for a machine
    /// created at `from`
    #[must_use]
    pub fn next_increment(self, from: UnixMillis, now: UnixMillis) -> UnixMillis {
        let elapsed = now.as_millis().saturating_sub(from.as_millis());
        let increment = self.increment_millis();
        let started = elapsed / increment + 1;

        from.saturating_add(Duration::from_millis(started.saturating_mul(increment)))
    }
}

/// How long before its paid time ends an idle machine asks for its release
///
/// The provider must stop billing before the next increment starts. phoenixNAP took about 2 to
/// 2.5 minutes to deprovision a server in the live test of 2026-11-24; whether its billing
/// stops at the delete request or once the server is gone is unconfirmed, as it is for
/// Latitude.sh, so this covers the whole deletion, the machine's renew round and a retried
/// release.
pub const IDLE_RELEASE_MARGIN: Duration = Duration::from_secs(5 * 60);

/// A provider's own billing increment, which an idle machine can use up for free
///
/// It is the provider's increment, not a class's coarser counting one: holding an idle droplet
/// counted by the hour still costs DigitalOcean's per-second price.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaidTime {
    increment: Duration,
}

impl PaidTime {
    /// Every started hour is billed in full
    pub const HOURLY: Self = Self {
        increment: Duration::from_secs(SECS_PER_HOUR),
    };

    /// Returns until when an idle machine may stay without starting a new paid increment, or
    /// `None` when holding it gains nothing
    ///
    /// The provider's billing starts at a moment cloudlease is not told, such as phoenixNAP's
    /// `provisionedOn`, but it is no earlier than the lease's `created`, which is saved before
    /// the provider create, and no later than `billing_starts_by` after it. Increments are
    /// counted from `created` here, so the hold ends no later than the provider's real
    /// increment and never starts a counted one, which keeps the run's spend where it was.
    /// When the provider's increment may have turned less than `billing_starts_by` ago, the
    /// next one could start at any moment, so there is no hold. A hold that would pass
    /// `hard_deadline` is dropped, and the machine releases as it would without one.
    #[must_use]
    pub fn idle_hold_until(
        self,
        created: UnixMillis,
        billing_starts_by: Duration,
        hard_deadline: UnixMillis,
        now: UnixMillis,
    ) -> Option<UnixMillis> {
        let increment = u64::try_from(self.increment.as_millis()).ok()?;
        let starts_by = u64::try_from(billing_starts_by.as_millis()).ok()?;
        let elapsed = now.as_millis().checked_sub(created.as_millis())?;
        if elapsed % increment < starts_by {
            return None;
        }

        let paid_until = created.saturating_add(Duration::from_millis(
            (elapsed / increment + 1).saturating_mul(increment),
        ));
        let until = UnixMillis::new(
            paid_until
                .as_millis()
                .checked_sub(u64::try_from(IDLE_RELEASE_MARGIN.as_millis()).ok()?)?,
        )?;

        (now < until && until <= hard_deadline).then_some(until)
    }
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
    /// What the run's live machines past their minimum add per hour, on average
    pub per_hour: MicroUsd,
    /// When the first live machine still inside its minimum starts to add to the spend
    pub next_accrual: Option<UnixMillis>,
    /// When the next increment of a live machine past its minimum starts, the earliest the
    /// spend can grow
    pub next_increment: Option<UnixMillis>,
    /// The billing increment
    pub increment: Duration,
}

impl Spend {
    /// Adds up the machines of a run whose class costs `price_per_hour` under `billing`
    #[must_use]
    pub fn of(
        billing: Billing,
        price_per_hour: MicroUsd,
        machines: impl IntoIterator<Item = BilledLife>,
        now: UnixMillis,
    ) -> Self {
        let accrues_after = billing.accrues_after(price_per_hour);
        let earliest = |current: Option<UnixMillis>, at: UnixMillis| {
            Some(current.map_or(at, |current| current.min(at)))
        };
        let empty = Self {
            accrued: MicroUsd::ZERO,
            per_hour: MicroUsd::ZERO,
            next_accrual: None,
            next_increment: None,
            increment: billing.increment(),
        };

        machines.into_iter().fold(empty, |spend, machine| {
            let accrued = spend.accrued.saturating_add(billing.billed(
                price_per_hour,
                machine.from,
                machine.gone_at.unwrap_or(now),
            ));
            if machine.gone_at.is_some() {
                return Self { accrued, ..spend };
            }

            let accrues_from = machine.from.saturating_add(accrues_after);
            if accrues_from <= now {
                return Self {
                    accrued,
                    per_hour: spend.per_hour.saturating_add(price_per_hour),
                    next_increment: earliest(
                        spend.next_increment,
                        billing.next_increment(machine.from, now),
                    ),
                    ..spend
                };
            }

            Self {
                accrued,
                next_accrual: earliest(spend.next_accrual, accrues_from),
                ..spend
            }
        })
    }

    /// Reports whether the run has spent past its cap
    ///
    /// Spending exactly the cap is allowed, as admission allows it: under hourly billing a
    /// machine admitted to fill the cap bills its hour at once, and that must not end the
    /// run whose hours it already paid for.
    #[must_use]
    pub fn has_reached(&self, cap: MicroUsd) -> bool {
        self.accrued > cap
    }

    /// Returns when the spend must next be checked against `cap`, or `None` when nothing
    /// is accruing or will
    ///
    /// The spend grows only when an increment starts, by the price of each machine whose
    /// increment it is, so it can cross the cap no earlier than the next increment, and no
    /// earlier than one increment before the hourly rate would cross it. The check is the
    /// later of those two, or when the next machine inside its minimum starts to accrue if
    /// that is sooner, so it is never later than the real crossing. Machines inside their
    /// minimum add nothing until then, so they cannot pull the check ever closer while the
    /// spend stays flat, and with hourly increments the run object wakes about once an hour.
    #[must_use]
    pub fn reaches_cap_at(&self, cap: MicroUsd, now: UnixMillis) -> Option<UnixMillis> {
        if self.has_reached(cap) {
            return Some(now);
        }

        let crossing = (self.per_hour != MicroUsd::ZERO).then(|| {
            let left = u128::from(cap.0 - self.accrued.0);
            let millis = (left * MILLIS_PER_HOUR).div_ceil(u128::from(self.per_hour.0));
            let millis = u64::try_from(millis).unwrap_or(u64::MAX);
            let increment = u64::try_from(self.increment.as_millis()).unwrap_or(u64::MAX);
            let early = now.saturating_add(Duration::from_millis(millis.saturating_sub(increment)));
            self.next_increment.map_or(early, |next| next.max(early))
        });

        match (crossing, self.next_accrual) {
            (Some(crossing), Some(next)) => Some(crossing.min(next)),
            (crossing, next) => crossing.or(next),
        }
    }
}

#[cfg(test)]
mod tests {
    use cloudlease_core::time::UnixMillis;

    use std::time::Duration;

    use super::{BilledLife, Billing, IDLE_RELEASE_MARGIN, MicroUsd, MoneyError, PaidTime, Spend};

    const DO: Billing = Billing::DIGITALOCEAN;
    const PNAP: Billing = Billing::PHOENIXNAP;
    const HOUR: u64 = 3_600_000;

    fn at(millis: u64) -> UnixMillis {
        UnixMillis::new(millis).unwrap()
    }

    fn dollars(value: &str) -> MicroUsd {
        MicroUsd::parse_dollars(value).unwrap()
    }

    fn live(from: u64) -> BilledLife {
        BilledLife {
            from: at(from),
            gone_at: None,
        }
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
    fn digitalocean_bills_every_started_second_with_a_minute_and_cent_minimum() {
        let price = dollars("36"); // a cent a second
        assert_eq!(DO.billed(price, at(0), at(1_000)), dollars("0.60"));
        assert_eq!(DO.billed(price, at(0), at(89_500)), dollars("0.90"));
        // the 91st second has started
        assert_eq!(DO.billed(price, at(0), at(90_000)), dollars("0.91"));
        assert_eq!(DO.billed(price, at(5_000), at(0)), dollars("0.60"));

        // a minute of a $0.125/h droplet is about $0.002, but DigitalOcean charges a cent
        let cheap = dollars("0.125");
        assert_eq!(DO.billed(cheap, at(0), at(59_000)), dollars("0.01"));
        assert_eq!(DO.billed(cheap, at(0), at(HOUR - 1)), dollars("0.125"));
        assert_eq!(DO.minimum(cheap), dollars("0.01"));
    }

    #[test]
    fn phoenixnap_bills_every_started_hour_in_full() {
        let price = dollars("0.47");
        // a server deleted after 5 minutes costs a whole hour
        assert_eq!(PNAP.billed(price, at(0), at(5 * 60_000)), price);
        assert_eq!(PNAP.billed(price, at(0), at(HOUR - 1)), price);
        assert_eq!(PNAP.billed(price, at(0), at(HOUR)), dollars("0.94"));
        assert_eq!(PNAP.billed(price, at(0), at(61 * 60_000)), dollars("0.94"));
        assert_eq!(PNAP.minimum(price), price);
        assert_eq!(PNAP.accrues_after(price), Duration::from_secs(3_600));
        assert_eq!(PNAP.next_increment(at(0), at(61 * 60_000)), at(2 * HOUR));
    }

    #[test]
    fn a_class_may_bill_in_whole_multiples_of_its_providers_increment() {
        let hourly = DO.with_increment(Duration::from_secs(3_600)).unwrap();
        // the minute minimum becomes a whole hour
        assert_eq!(hourly.billed(dollars("1"), at(0), at(1_000)), dollars("1"));
        let minutes = DO.with_increment(Duration::from_secs(90)).unwrap();
        assert_eq!(
            minutes.billed(dollars("36"), at(0), at(1_000)),
            dollars("0.90")
        );

        assert_eq!(
            PNAP.with_increment(Duration::from_secs(60)),
            Err(MoneyError::Increment(3_600))
        );
        // 90 minutes would bill $0.60 at minute 61, when phoenixNAP has billed two $0.40 hours
        assert_eq!(
            PNAP.with_increment(Duration::from_secs(5_400)),
            Err(MoneyError::Increment(3_600))
        );
        assert!(PNAP.with_increment(Duration::from_secs(7_200)).is_ok());
        assert!(DO.with_increment(Duration::from_millis(1_500)).is_err());
        assert!(
            DO.with_increment(Duration::from_secs(2 * 24 * 3_600))
                .is_err()
        );
    }

    #[test]
    fn a_run_reaches_its_cap_when_its_live_machines_have_accrued_it() {
        let price = dollars("36");
        let cap = dollars("3");
        let machines = [
            // gone after 100 s: 101 started seconds, $1.01, adds nothing per hour
            BilledLife {
                from: at(0),
                gone_at: Some(at(100_000)),
            },
            // live for 50 s: still at its $0.60 minimum
            live(150_000),
        ];

        let spend = Spend::of(DO, price, machines, at(200_000));
        assert_eq!(spend.accrued, dollars("1.61"));
        assert!(!spend.has_reached(cap));
        // the live machine adds nothing until its minute is up, so that is the next check
        assert_eq!(spend.per_hour, MicroUsd::ZERO);
        assert_eq!(spend.reaches_cap_at(cap, at(200_000)), Some(at(210_000)));

        // from then it accrues a cent a second; the remaining $1.38 takes 138 s at the rate,
        // and the check comes one increment early, since a started second bills at once
        let accruing = Spend::of(DO, price, machines, at(210_000));
        assert_eq!(accruing.per_hour, price);
        let early = accruing.reaches_cap_at(cap, at(210_000)).unwrap();
        assert_eq!(early, at(347_000));
        assert!(!Spend::of(DO, price, machines, early).has_reached(cap));
        let last = Spend::of(DO, price, machines, early)
            .reaches_cap_at(cap, early)
            .unwrap();
        assert_eq!(last, at(348_000));
        // at 348 s the run has spent exactly its cap, which it may; the next second passes it
        let exact = Spend::of(DO, price, machines, last);
        assert_eq!(exact.accrued, cap);
        assert!(!exact.has_reached(cap));
        let past = exact.reaches_cap_at(cap, last).unwrap();
        assert_eq!(past, at(349_000));
        assert!(Spend::of(DO, price, machines, past).has_reached(cap));

        let idle = Spend::of(DO, price, [machines[0]], at(200_000));
        assert_eq!(idle.reaches_cap_at(cap, at(200_000)), None);
        assert_eq!(
            idle.reaches_cap_at(dollars("1"), at(200_000)),
            Some(at(200_000))
        );
    }

    #[test]
    fn a_spend_left_just_under_the_cap_is_checked_when_it_next_grows() {
        // a cent buys 288 s of a $0.125/h machine, longer than the 60 s minimum
        let price = dollars("0.125");
        assert_eq!(DO.accrues_after(price), Duration::from_secs(288));
        assert_eq!(DO.billed(price, at(0), at(287_999)), dollars("0.01"));
        assert!(DO.billed(price, at(0), at(288_000)) > dollars("0.01"));

        let machines = [
            // released after 574.9 s, 575 started seconds: $0.019966
            BilledLife {
                from: at(0),
                gone_at: Some(at(574_900)),
            },
            // still inside its one cent minimum
            live(600_000),
        ];
        let spend = Spend::of(DO, price, machines, at(601_000));
        assert_eq!(spend.accrued, MicroUsd::new(29_966));

        // a few millionths short of a 3 cent cap, the next check is when the live machine
        // starts to accrue, not a few milliseconds away again and again
        assert_eq!(
            spend.reaches_cap_at(dollars("0.03"), at(601_000)),
            Some(at(888_000))
        );
    }

    #[test]
    fn an_hourly_run_is_checked_at_the_hour_its_spend_steps_up() {
        let price = dollars("0.47");
        let cap = dollars("1.00");
        let machines = [live(0)];

        // inside its first hour the server has cost $0.47 and will cost no more until 1 h
        let first = Spend::of(PNAP, price, machines, at(5 * 60_000));
        assert_eq!(first.accrued, price);
        assert_eq!(first.reaches_cap_at(cap, at(5 * 60_000)), Some(at(HOUR)));

        // the second hour bills $0.94 at once; the $1.41 third hour starts at 2 h, which is
        // when the check is, not every few minutes along the hourly rate
        let second = Spend::of(PNAP, price, machines, at(HOUR));
        assert_eq!(second.accrued, dollars("0.94"));
        assert_eq!(second.reaches_cap_at(cap, at(HOUR)), Some(at(2 * HOUR)));
        assert!(Spend::of(PNAP, price, machines, at(2 * HOUR)).has_reached(cap));
    }

    #[test]
    fn an_idle_machine_holds_to_just_before_its_paid_hour_ends() {
        const MINUTE: u64 = 60_000;
        let grace = Duration::from_secs(20 * 60);
        let far = at(10 * HOUR);
        let hold = |now| PaidTime::HOURLY.idle_hold_until(at(0), grace, far, at(now));

        // billing may start up to the boot grace after the lease, so the provider's hour
        // may turn that long after the counted one, and there is no hold until it surely has
        assert_eq!(hold(19 * MINUTE), None);
        assert_eq!(hold(20 * MINUTE), Some(at(55 * MINUTE)));
        assert_eq!(hold(54 * MINUTE), Some(at(55 * MINUTE)));
        // inside the margin the machine releases at once
        assert_eq!(hold(55 * MINUTE), None);
        assert_eq!(hold(HOUR + 19 * MINUTE), None);
        assert_eq!(hold(HOUR + 30 * MINUTE), Some(at(HOUR + 55 * MINUTE)));

        // the hold never ends in a counted hour the run has not paid for, so the spend stays
        let held = hold(HOUR + 30 * MINUTE).unwrap();
        let price = dollars("0.40");
        assert_eq!(
            PNAP.billed(price, at(0), held),
            PNAP.billed(price, at(0), at(HOUR + 30 * MINUTE))
        );
        assert!(held.as_millis() + IDLE_RELEASE_MARGIN.as_millis() as u64 <= 2 * HOUR);
    }

    #[test]
    fn a_hold_that_would_pass_the_hard_deadline_is_dropped() {
        const MINUTE: u64 = 60_000;
        let grace = Duration::from_secs(15 * 60);
        let hold = |deadline| {
            PaidTime::HOURLY.idle_hold_until(at(0), grace, at(deadline), at(30 * MINUTE))
        };

        assert_eq!(hold(55 * MINUTE), Some(at(55 * MINUTE)));
        assert_eq!(hold(54 * MINUTE), None);
    }
}
