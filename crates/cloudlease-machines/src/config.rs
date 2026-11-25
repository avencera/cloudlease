//! Classes, providers, prices, caps and the lease policy
//!
//! The Worker reads one JSON object, `MACHINES_CONFIG`, and checks all of it before serving a
//! request, so a bad price or class fails loudly instead of creating a machine. The hourly
//! spend cap is the capacity gate's limit (`HOURLY_SPEND_CAP_CENTS`), not part of this
//! object, so it can be lowered with the gate's override without a deploy.
//!
//! Each class names its provider (`digitalocean` when it names none) and that provider's
//! machine, its price, its billing increment and its boot grace; the last two default to the
//! provider's.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU16, NonZeroU32, NonZeroU64},
    time::Duration,
};

use cloudlease_core::{
    capacity::PermitUnits,
    ids::ResourceId,
    lease::{LeaseError, LeasePolicy},
};
use cloudlease_provider::{
    aws::{
        AmiId, AvailabilityZone, AwsRegion, InstanceType, LaunchPools, LaunchTemplate,
        LaunchTemplateId, SpotMaxPrice, SpotPool, SubnetId, UserData,
    },
    digitalocean::{BootScript, DropletImage, Slug},
    latitude,
    phoenixnap::{Location, OsImage, ServerType, SshKeyIds},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;

use crate::{
    boot::BootError,
    ids::{AppName, ClassName},
    money::{Billing, MicroUsd},
    provider::{AwsZone, InstanceTypes, Locations, ProviderName},
};

/// The heartbeat window when `MACHINES_CONFIG` sets none; machines renew every 30 seconds
pub const DEFAULT_HEARTBEAT_TTL: Duration = Duration::from_secs(3 * 60);

/// The boot grace of a DigitalOcean class when neither it nor `MACHINES_CONFIG` sets one
pub const DEFAULT_BOOT_GRACE: Duration = Duration::from_secs(8 * 60);

/// The boot grace of a phoenixNAP class that sets none
///
/// phoenixNAP quotes 60 seconds to 15 minutes to provision, by OS, and applies cloud-init
/// with a reboot on top.
pub const PHOENIXNAP_BOOT_GRACE: Duration = Duration::from_secs(20 * 60);

/// The boot grace of a Latitude.sh class that sets none
///
/// Latitude.sh deploys in under 10 seconds when the plan, site and OS are listed in the
/// plan's `deploys_instantly`, and in about 10 minutes otherwise.
pub const LATITUDE_BOOT_GRACE: Duration = Duration::from_secs(15 * 60);

/// The boot grace of an AWS class that sets none
///
/// A Spot instance from a stock Ubuntu AMI is usually running a minute after launch and
/// has run its user data a minute or two later.
pub const AWS_BOOT_GRACE: Duration = Duration::from_secs(8 * 60);

/// The root device of Canonical's Ubuntu AMIs, used when an AWS class names none
pub const UBUNTU_ROOT_DEVICE: &str = "/dev/sda1";

/// The smallest root volume an AWS class may ask for, the size of Canonical's Ubuntu AMI
/// snapshots
pub const MIN_ROOT_VOLUME_GIB: u16 = 8;

/// The shortest heartbeat window a config may set, so a few lost renews do not end a lease
pub const MIN_HEARTBEAT_TTL: Duration = Duration::from_secs(60);

/// The longest run deadline any configuration may allow
pub const MAX_RUN_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const HOUR: u64 = 60 * 60;

/// A configuration the Worker refuses to run with
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ConfigError {
    /// The JSON does not match the schema
    #[error("MACHINES_CONFIG does not parse: {0}")]
    Json(String),
    /// A value is out of range or malformed
    #[error("MACHINES_CONFIG {field}: {message}")]
    Invalid {
        /// The offending field
        field: String,
        /// What is wrong
        message: String,
    },
}

impl ConfigError {
    fn invalid(field: impl Into<String>, message: impl ToString) -> Self {
        Self::Invalid {
            field: field.into(),
            message: message.to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    api_base: String,
    max_run_hours: u64,
    max_machines_per_run: u16,
    run_spend_cap_usd: String,
    #[serde(default)]
    heartbeat_ttl_secs: Option<u64>,
    // the default of DigitalOcean classes only, which predates per-class boot graces
    #[serde(default)]
    boot_grace_secs: Option<u64>,
    classes: BTreeMap<ClassName, Value>,
    apps: BTreeMap<AppName, RawApp>,
}

/// The fields every class has, whatever its provider
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommon {
    price_per_hour_usd: String,
    max_hours: u64,
    #[serde(default)]
    boot_grace_secs: Option<u64>,
    #[serde(default)]
    billing_increment_secs: Option<u64>,
}

const COMMON_FIELDS: [&str; 4] = [
    "price_per_hour_usd",
    "max_hours",
    "boot_grace_secs",
    "billing_increment_secs",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDigitalOceanClass {
    region: String,
    size: String,
    image: String,
    #[serde(default)]
    ssh_key_ids: Vec<NonZeroU64>,
    #[serde(default)]
    vpc_uuid: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPhoenixNapClass {
    server_type: String,
    locations: Vec<String>,
    os: String,
    #[serde(default)]
    ssh_key_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLatitudeClass {
    plan: String,
    sites: Vec<String>,
    operating_system: String,
    #[serde(default)]
    ssh_key_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAwsClass {
    region: String,
    instance_types: Vec<String>,
    zones: Vec<RawAwsZone>,
    image_id: String,
    #[serde(default)]
    root_device_name: Option<String>,
    root_volume_gib: u16,
    launch_template_id: String,
    launch_template_version: u32,
    spot_max_price_usd: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAwsZone {
    zone: String,
    subnet_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    classes: BTreeSet<ClassName>,
}

/// A DigitalOcean droplet, which works on its local disk
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigitalOceanClass {
    /// The region slug
    pub region: Slug,
    /// The droplet size slug
    pub size: Slug,
    /// The boot image
    pub image: DropletImage,
    /// SSH keys installed for break-glass access
    pub ssh_key_ids: Vec<NonZeroU64>,
    /// The VPC, when not the region default
    pub vpc_uuid: Option<ResourceId>,
}

/// A phoenixNAP Bare Metal Cloud server, billed hourly
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhoenixNapClass {
    /// The server type, such as `s2.c2.large`
    pub server_type: ServerType,
    /// The locations to try in order; one without stock moves the create to the next
    pub locations: Locations,
    /// The OS image, which must support cloud-init, such as `ubuntu/jammy`
    pub os: OsImage,
    /// IDs of account SSH keys installed for break-glass access; phoenixNAP refuses a Linux
    /// server without one
    pub ssh_key_ids: SshKeyIds,
}

/// A Latitude.sh bare metal server, billed hourly
///
/// The server boots from a user data record the lease creates first, in the Worker's
/// Latitude.sh project.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatitudeClass {
    /// The plan slug, such as `m4-metal-medium`
    pub plan: latitude::Slug,
    /// The sites to try in order, such as `DAL`; one without stock moves the create on
    pub sites: Locations<latitude::Slug>,
    /// The operating system slug, which must support user data, such as
    /// `ubuntu_24_04_x64_lts`
    pub operating_system: latitude::Slug,
    /// IDs of project SSH keys installed for break-glass access
    pub ssh_key_ids: Vec<ResourceId>,
}

/// An AWS EC2 Spot instance, billed per second
///
/// Each machine is a one-time Spot request that EC2 terminates when it needs the capacity
/// back, with its root volume. One instant fleet offers every instance type in every zone,
/// and EC2 picks the pool least likely to be interrupted. The instance has a public IPv4
/// address from its subnet and the launch template's security group, which should allow no
/// inbound traffic: machines reach their coordinator outbound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsClass {
    /// The region, which the Worker's `AWS_REGIONS` must list, such as `us-east-1`
    pub region: AwsRegion,
    /// Every instance type of the class in every zone, each with the subnet to launch into,
    /// in the order the class names them
    pub pools: LaunchPools,
    /// The AMI, such as Canonical's Ubuntu 24.04 from the SSM public parameter
    pub image_id: AmiId,
    /// The AMI's root device name
    pub root_device_name: String,
    /// The gp3 root volume size, deleted with the instance
    pub root_volume_gib: NonZeroU16,
    /// The launch template every launch uses, pinned to one version; it holds the security
    /// group, which a fleet cannot override
    pub launch_template: LaunchTemplate,
    /// The most EC2 may charge for the instance per hour in any pool; at most the class's
    /// price, so the Worker's spend never counts less than EC2 bills for the instance
    pub spot_max_price: SpotMaxPrice,
}

/// The provider a class uses and the machine it asks that provider for
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderClass {
    /// A DigitalOcean droplet
    DigitalOcean(DigitalOceanClass),
    /// A phoenixNAP server
    PhoenixNap(PhoenixNapClass),
    /// A Latitude.sh server
    Latitude(LatitudeClass),
    /// An AWS Spot instance
    Aws(AwsClass),
}

impl ProviderClass {
    /// Returns the provider's name
    #[must_use]
    pub const fn name(&self) -> ProviderName {
        match self {
            Self::DigitalOcean(_) => ProviderName::DigitalOcean,
            Self::PhoenixNap(_) => ProviderName::PhoenixNap,
            Self::Latitude(_) => ProviderName::Latitude,
            Self::Aws(_) => ProviderName::Aws,
        }
    }

    /// Checks that a rendered boot script, in the form this provider sends it, fits the
    /// provider's user data limit
    ///
    /// # Errors
    ///
    /// Returns [`BootError::UserDataTooLarge`] when it does not.
    pub fn check_user_data(&self, script: &BootScript) -> Result<(), BootError> {
        match self {
            Self::DigitalOcean(_) | Self::PhoenixNap(_) => Ok(()),
            // Latitude.sh boots the script base64-encoded inside a cloud-config document
            Self::Latitude(_) => latitude::BootScript::shell(script.as_str())
                .map(drop)
                .map_err(|_| BootError::UserDataTooLarge),
            // EC2 takes at most 16 KiB of user data before its base64 encoding
            Self::Aws(_) => UserData::parse(script.as_str())
                .map(drop)
                .map_err(|_| BootError::UserDataTooLarge),
        }
    }
}

/// What one class of machine is and costs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineClass {
    /// The provider and its machine
    pub provider: ProviderClass,
    /// The machine, per hour
    pub price_per_hour: MicroUsd,
    /// The price in whole cents per hour, the machine's weight in the hourly spend gate
    pub gate_units: PermitUnits,
    /// How the provider rounds a machine's life into billed time
    pub billing: Billing,
    /// The heartbeat window and this class's boot grace
    pub heartbeat: Heartbeat,
    /// The longest a machine of this class may live
    pub max_lifetime: Duration,
}

/// How long a lease lives without a renew, fixed on a run when it is created
///
/// A run keeps the windows it was created with, so renews never read the configuration: a
/// bad config deploy cannot make every live lease lapse.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Heartbeat {
    ttl_secs: u64,
    boot_grace_secs: u64,
}

impl Heartbeat {
    /// Checks a heartbeat window and boot grace
    ///
    /// The window is at least [`MIN_HEARTBEAT_TTL`], and the boot grace at least the window
    /// and at most [`MAX_RUN_LIFETIME`].
    pub fn new(ttl: Duration, boot_grace: Duration) -> Result<Self, ConfigError> {
        if ttl < MIN_HEARTBEAT_TTL || ttl > MAX_RUN_LIFETIME {
            return Err(ConfigError::invalid(
                "heartbeat_ttl_secs",
                format!(
                    "must be from {} seconds through 168 hours",
                    MIN_HEARTBEAT_TTL.as_secs()
                ),
            ));
        }
        if boot_grace < ttl || boot_grace > MAX_RUN_LIFETIME {
            return Err(ConfigError::invalid(
                "boot_grace_secs",
                "must be from heartbeat_ttl_secs through 168 hours",
            ));
        }

        Ok(Self {
            ttl_secs: ttl.as_secs(),
            boot_grace_secs: boot_grace.as_secs(),
        })
    }

    /// How long a lease lives after a renew
    #[must_use]
    pub const fn ttl(self) -> Duration {
        Duration::from_secs(self.ttl_secs)
    }

    /// How long a new lease waits for its machine's first renew
    #[must_use]
    pub const fn boot_grace(self) -> Duration {
        Duration::from_secs(self.boot_grace_secs)
    }

    /// Returns the policy a lease of a class with `max_lifetime` starts under
    ///
    /// Both windows are cut to the class's lifetime: a run keeps the windows of the config it
    /// was created under, and a later config may shorten the class below them.
    pub fn lease_policy(self, max_lifetime: Duration) -> Result<LeasePolicy, LeaseError> {
        LeasePolicy::new(self.ttl().min(max_lifetime), max_lifetime)?
            .with_boot_grace(self.boot_grace().min(max_lifetime))
    }

    /// Returns the policy renews are checked against
    ///
    /// A renew uses only the heartbeat window, so it needs no class: a lease keeps working
    /// even if its class was removed from the configuration after the lease began.
    pub fn renew_policy(self) -> Result<LeasePolicy, LeaseError> {
        self.lease_policy(MAX_RUN_LIFETIME)
    }
}

impl Heartbeat {
    /// The fixed windows every lease had before runs stored their own: a 10 minute heartbeat
    /// window and a 15 minute boot grace
    pub const BEFORE_CONFIG: Self = Self {
        ttl_secs: 10 * 60,
        boot_grace_secs: 15 * 60,
    };
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_HEARTBEAT_TTL.as_secs(),
            boot_grace_secs: DEFAULT_BOOT_GRACE.as_secs(),
        }
    }
}

/// The Worker's validated configuration
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachinesConfig {
    /// The public base URL machines renew against, written into `lease.json`
    pub api_base: String,
    /// The latest run deadline, counted from the run's creation
    pub max_run_lifetime: Duration,
    /// The largest `max_machines` a run may ask for
    pub max_machines_per_run: u16,
    /// What one run may spend before its machines are released
    pub run_spend_cap: MicroUsd,
    /// The heartbeat window new runs take; each class adds its own boot grace
    pub heartbeat_ttl: Duration,
    classes: BTreeMap<ClassName, MachineClass>,
    apps: BTreeMap<AppName, BTreeSet<ClassName>>,
}

/// Why an app may not use a class
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownClass;

impl MachinesConfig {
    /// Parses and checks `MACHINES_CONFIG`
    pub fn parse(json: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig =
            serde_json::from_str(json).map_err(|error| ConfigError::Json(error.to_string()))?;

        if !raw.api_base.starts_with("https://") || raw.api_base.ends_with('/') {
            return Err(ConfigError::invalid(
                "api_base",
                "must be an https URL without a trailing slash",
            ));
        }
        let max_run_lifetime = hours("max_run_hours", raw.max_run_hours)?;
        if raw.max_machines_per_run == 0 {
            return Err(ConfigError::invalid(
                "max_machines_per_run",
                "must be positive",
            ));
        }
        let run_spend_cap = MicroUsd::parse_dollars(&raw.run_spend_cap_usd)
            .map_err(|error| ConfigError::invalid("run_spend_cap_usd", error))?;
        let heartbeat_ttl = raw
            .heartbeat_ttl_secs
            .map_or(DEFAULT_HEARTBEAT_TTL, Duration::from_secs);
        // checked on its own so a bad window is named even with no class to carry it
        let digitalocean_boot_grace = raw
            .boot_grace_secs
            .map_or(DEFAULT_BOOT_GRACE, Duration::from_secs);
        Heartbeat::new(heartbeat_ttl, digitalocean_boot_grace)?;
        let defaults = ClassDefaults {
            heartbeat_ttl,
            digitalocean_boot_grace,
        };

        let classes = raw
            .classes
            .into_iter()
            .map(|(name, class)| {
                let class = machine_class(&name, class, defaults)?;
                Ok((name, class))
            })
            .collect::<Result<BTreeMap<_, _>, ConfigError>>()?;
        let apps = raw
            .apps
            .into_iter()
            .map(|(app, allowed)| {
                if let Some(missing) = allowed.classes.iter().find(|c| !classes.contains_key(*c)) {
                    return Err(ConfigError::invalid(
                        format!("apps.{app}.classes"),
                        format!("{missing} is not a configured class"),
                    ));
                }
                Ok((app, allowed.classes))
            })
            .collect::<Result<_, _>>()?;

        Ok(Self {
            api_base: raw.api_base,
            max_run_lifetime,
            max_machines_per_run: raw.max_machines_per_run,
            run_spend_cap,
            heartbeat_ttl,
            classes,
            apps,
        })
    }

    /// Returns every provider some class uses
    #[must_use]
    pub fn providers(&self) -> BTreeSet<ProviderName> {
        self.classes
            .values()
            .map(|class| class.provider.name())
            .collect()
    }

    /// Returns every AWS region some class launches in
    #[must_use]
    pub fn aws_regions(&self) -> BTreeSet<&AwsRegion> {
        self.classes
            .values()
            .filter_map(|class| match &class.provider {
                ProviderClass::Aws(aws) => Some(&aws.region),
                _ => None,
            })
            .collect()
    }

    /// Returns the class when `app` may use it
    pub fn class_for(
        &self,
        app: &AppName,
        class: &ClassName,
    ) -> Result<&MachineClass, UnknownClass> {
        let allowed = self
            .apps
            .get(app)
            .is_some_and(|classes| classes.contains(class));
        if !allowed {
            return Err(UnknownClass);
        }

        self.classes.get(class).ok_or(UnknownClass)
    }
}

fn hours(field: &str, value: u64) -> Result<Duration, ConfigError> {
    let duration = Duration::from_secs(value.saturating_mul(HOUR));
    if value == 0 || duration > MAX_RUN_LIFETIME {
        return Err(ConfigError::invalid(
            field,
            "must be from 1 through 168 hours",
        ));
    }

    Ok(duration)
}

/// What a class takes from the configuration's top level
#[derive(Clone, Copy)]
struct ClassDefaults {
    heartbeat_ttl: Duration,
    digitalocean_boot_grace: Duration,
}

fn machine_class(
    name: &ClassName,
    raw: Value,
    defaults: ClassDefaults,
) -> Result<MachineClass, ConfigError> {
    let field = |key: &str| format!("classes.{name}.{key}");
    let Value::Object(mut raw) = raw else {
        return Err(ConfigError::invalid(
            format!("classes.{name}"),
            "must be an object",
        ));
    };
    let provider = match raw.remove("provider") {
        None => ProviderName::DigitalOcean,
        Some(value) => ProviderName::deserialize(value).map_err(|_| {
            ConfigError::invalid(
                field("provider"),
                "must be digitalocean, phoenixnap, latitude or aws",
            )
        })?,
    };
    // the common fields are split off, so each part is checked for unknown fields alone
    let common = COMMON_FIELDS
        .iter()
        .filter_map(|key| raw.remove_entry(*key))
        .collect();
    let common: RawCommon = parse_class(name, Value::Object(common))?;
    let raw = Value::Object(raw);

    let (provider, billing, boot_grace) = match provider {
        ProviderName::DigitalOcean => (
            ProviderClass::DigitalOcean(digitalocean_class(name, parse_class(name, raw)?)?),
            Billing::DIGITALOCEAN,
            defaults.digitalocean_boot_grace,
        ),
        ProviderName::PhoenixNap => (
            ProviderClass::PhoenixNap(phoenixnap_class(name, parse_class(name, raw)?)?),
            Billing::PHOENIXNAP,
            PHOENIXNAP_BOOT_GRACE,
        ),
        ProviderName::Latitude => (
            ProviderClass::Latitude(latitude_class(name, parse_class(name, raw)?)?),
            Billing::LATITUDE,
            LATITUDE_BOOT_GRACE,
        ),
        ProviderName::Aws => (
            ProviderClass::Aws(aws_class(name, parse_class(name, raw)?)?),
            Billing::AWS,
            AWS_BOOT_GRACE,
        ),
    };

    let price_per_hour = MicroUsd::parse_dollars(&common.price_per_hour_usd)
        .map_err(|error| ConfigError::invalid(field("price_per_hour_usd"), error))?;
    if price_per_hour == MicroUsd::ZERO {
        return Err(ConfigError::invalid(
            field("price_per_hour_usd"),
            "must be positive",
        ));
    }
    if let ProviderClass::Aws(aws) = &provider {
        let cap = MicroUsd::parse_dollars(aws.spot_max_price.as_str())
            .map_err(|error| ConfigError::invalid(field("spot_max_price_usd"), error))?;
        // a cap above the counted price would let EC2 bill more than the run's spend shows
        if cap > price_per_hour {
            return Err(ConfigError::invalid(
                field("spot_max_price_usd"),
                "must be at most price_per_hour_usd",
            ));
        }
    }
    let gate_units = price_per_hour
        .cents_per_hour()
        .map_err(|error| ConfigError::invalid(field("price_per_hour_usd"), error))?;
    let billing = match common.billing_increment_secs {
        None => billing,
        Some(secs) => billing
            .with_increment(Duration::from_secs(secs))
            .map_err(|error| ConfigError::invalid(field("billing_increment_secs"), error))?,
    };
    let boot_grace = common
        .boot_grace_secs
        .map_or(boot_grace, Duration::from_secs);
    let heartbeat = Heartbeat::new(defaults.heartbeat_ttl, boot_grace).map_err(|error| {
        // a class's own boot grace is named as the class's field
        match (error, common.boot_grace_secs) {
            (ConfigError::Invalid { message, .. }, Some(_)) => {
                ConfigError::invalid(field("boot_grace_secs"), message)
            }
            (error, _) => error,
        }
    })?;

    let class = MachineClass {
        provider,
        price_per_hour,
        gate_units,
        billing,
        heartbeat,
        max_lifetime: hours(&field("max_hours"), common.max_hours)?,
    };
    heartbeat
        .lease_policy(class.max_lifetime)
        .map_err(|error| ConfigError::invalid(field("max_hours"), error))?;

    Ok(class)
}

fn parse_class<T: DeserializeOwned>(name: &ClassName, raw: Value) -> Result<T, ConfigError> {
    serde_json::from_value(raw)
        .map_err(|error| ConfigError::invalid(format!("classes.{name}"), error))
}

fn digitalocean_class(
    name: &ClassName,
    raw: RawDigitalOceanClass,
) -> Result<DigitalOceanClass, ConfigError> {
    let field = |key: &str| format!("classes.{name}.{key}");
    let slug = |key: &str, value: String| {
        Slug::parse(value).map_err(|error| ConfigError::invalid(field(key), error))
    };

    let image = match raw.image.parse::<u64>().ok().and_then(NonZeroU64::new) {
        Some(id) => DropletImage::Id(id),
        None => DropletImage::Slug(slug("image", raw.image)?),
    };
    let vpc_uuid = raw
        .vpc_uuid
        .map(|value| {
            ResourceId::parse(value).map_err(|error| ConfigError::invalid(field("vpc_uuid"), error))
        })
        .transpose()?;

    Ok(DigitalOceanClass {
        region: slug("region", raw.region)?,
        size: slug("size", raw.size)?,
        image,
        ssh_key_ids: raw.ssh_key_ids,
        vpc_uuid,
    })
}

fn phoenixnap_class(
    name: &ClassName,
    raw: RawPhoenixNapClass,
) -> Result<PhoenixNapClass, ConfigError> {
    let field = |key: &str| format!("classes.{name}.{key}");

    let locations = raw
        .locations
        .into_iter()
        .map(|location| {
            Location::parse(location)
                .map_err(|error| ConfigError::invalid(field("locations"), error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ssh_key_ids = raw
        .ssh_key_ids
        .into_iter()
        .map(|id| {
            ResourceId::parse(id).map_err(|error| ConfigError::invalid(field("ssh_key_ids"), error))
        })
        .collect::<Result<_, _>>()?;
    let ssh_key_ids = SshKeyIds::new(ssh_key_ids)
        .map_err(|error| ConfigError::invalid(field("ssh_key_ids"), error))?;

    Ok(PhoenixNapClass {
        server_type: ServerType::parse(raw.server_type)
            .map_err(|error| ConfigError::invalid(field("server_type"), error))?,
        locations: Locations::new(locations)
            .map_err(|error| ConfigError::invalid(field("locations"), error))?,
        os: OsImage::parse(raw.os).map_err(|error| ConfigError::invalid(field("os"), error))?,
        ssh_key_ids,
    })
}

fn latitude_class(name: &ClassName, raw: RawLatitudeClass) -> Result<LatitudeClass, ConfigError> {
    let field = |key: &str| format!("classes.{name}.{key}");
    let slug = |key: &str, value: String| {
        latitude::Slug::parse(value).map_err(|error| ConfigError::invalid(field(key), error))
    };

    let sites = raw
        .sites
        .into_iter()
        .map(|site| slug("sites", site))
        .collect::<Result<Vec<_>, _>>()?;
    let ssh_key_ids = raw
        .ssh_key_ids
        .into_iter()
        .map(|id| {
            ResourceId::parse(id).map_err(|error| ConfigError::invalid(field("ssh_key_ids"), error))
        })
        .collect::<Result<_, _>>()?;

    Ok(LatitudeClass {
        plan: slug("plan", raw.plan)?,
        sites: Locations::new(sites)
            .map_err(|error| ConfigError::invalid(field("sites"), error))?,
        operating_system: slug("operating_system", raw.operating_system)?,
        ssh_key_ids,
    })
}

fn aws_class(name: &ClassName, raw: RawAwsClass) -> Result<AwsClass, ConfigError> {
    let field = |key: &str| format!("classes.{name}.{key}");
    let invalid = |key: &str| {
        let field = field(key);
        move |error: cloudlease_provider::error::ProviderError| {
            ConfigError::invalid(field, error.message())
        }
    };

    let instance_types = raw
        .instance_types
        .into_iter()
        .map(InstanceType::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid("instance_types"))?;
    let instance_types = InstanceTypes::new(instance_types)
        .map_err(|error| ConfigError::invalid(field("instance_types"), error))?;
    let zones = raw
        .zones
        .into_iter()
        .map(|zone| {
            Ok(AwsZone {
                zone: AvailabilityZone::parse(zone.zone).map_err(invalid("zones"))?,
                subnet_id: SubnetId::parse(zone.subnet_id).map_err(invalid("zones"))?,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;
    let zones =
        Locations::new(zones).map_err(|error| ConfigError::invalid(field("zones"), error))?;
    let pools = instance_types
        .as_slice()
        .iter()
        .flat_map(|instance_type| {
            zones.as_slice().iter().map(|zone| SpotPool {
                instance_type: instance_type.clone(),
                availability_zone: zone.zone.clone(),
                subnet_id: zone.subnet_id.clone(),
            })
        })
        .collect();
    let pools = LaunchPools::new(pools).map_err(invalid("instance_types"))?;
    let root_device_name = raw
        .root_device_name
        .unwrap_or_else(|| UBUNTU_ROOT_DEVICE.to_owned());
    if !root_device_name.starts_with("/dev/") {
        return Err(ConfigError::invalid(
            field("root_device_name"),
            "must be a device path such as /dev/sda1",
        ));
    }
    let root_volume_gib = NonZeroU16::new(raw.root_volume_gib)
        .filter(|size| size.get() >= MIN_ROOT_VOLUME_GIB)
        .ok_or_else(|| {
            ConfigError::invalid(
                field("root_volume_gib"),
                format!("must be at least {MIN_ROOT_VOLUME_GIB}"),
            )
        })?;

    Ok(AwsClass {
        region: AwsRegion::parse(raw.region).map_err(invalid("region"))?,
        pools,
        image_id: AmiId::parse(raw.image_id).map_err(invalid("image_id"))?,
        root_device_name,
        root_volume_gib,
        launch_template: LaunchTemplate {
            id: LaunchTemplateId::parse(raw.launch_template_id)
                .map_err(invalid("launch_template_id"))?,
            // a number only: `$Latest` would let a template edit change running deployments
            version: NonZeroU32::new(raw.launch_template_version).ok_or_else(|| {
                ConfigError::invalid(field("launch_template_version"), "must be at least 1")
            })?,
        },
        spot_max_price: SpotMaxPrice::parse(raw.spot_max_price_usd)
            .map_err(invalid("spot_max_price_usd"))?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{ConfigError, Heartbeat, MachinesConfig, ProviderClass, UnknownClass};
    use crate::{
        ids::{AppName, ClassName},
        money::Billing,
    };

    pub(crate) fn sample() -> Value {
        json!({
            "api_base": "https://machines.example.test",
            "max_run_hours": 24,
            "max_machines_per_run": 64,
            "run_spend_cap_usd": "25.00",
            "classes": {
                "do-c16": {
                    "region": "nyc3",
                    "size": "c-16",
                    "image": "ubuntu-24-04-x64",
                    "price_per_hour_usd": "0.50",
                    "max_hours": 12
                },
                "do-c16-image": {
                    "provider": "digitalocean",
                    "region": "nyc3",
                    "size": "c-16",
                    "image": "212345678",
                    "price_per_hour_usd": "0.514",
                    "max_hours": 12
                },
                "pnap-s2": {
                    "provider": "phoenixnap",
                    "server_type": "s2.c2.large",
                    "locations": ["SEA", "NLD"],
                    "os": "ubuntu/jammy",
                    "ssh_key_ids": ["5fa54d1e91867c03a0a7b4a4"],
                    "price_per_hour_usd": "0.40",
                    "max_hours": 12
                },
                "lat-m4": {
                    "provider": "latitude",
                    "plan": "m4-metal-medium",
                    "sites": ["DAL", "CHI"],
                    "operating_system": "ubuntu_24_04_x64_lts",
                    "price_per_hour_usd": "1.25",
                    "max_hours": 12
                },
                "aws-c8a": {
                    "provider": "aws",
                    "region": "us-east-1",
                    "instance_types": ["c8a.4xlarge", "c7a.4xlarge"],
                    "zones": [
                        { "zone": "us-east-1a", "subnet_id": "subnet-0123456789abcdef0" },
                        { "zone": "us-east-1b", "subnet_id": "subnet-0123456789abcdef1" }
                    ],
                    "image_id": "ami-0123456789abcdef0",
                    "root_volume_gib": 150,
                    "launch_template_id": "lt-0123456789abcdef0",
                    "launch_template_version": 2,
                    "spot_max_price_usd": "0.45",
                    "price_per_hour_usd": "0.48",
                    "max_hours": 12
                }
            },
            "apps": {
                "homebased": {
                    "classes": ["do-c16", "do-c16-image", "pnap-s2", "lat-m4", "aws-c8a"]
                }
            }
        })
    }

    #[test]
    fn a_valid_config_names_the_classes_each_app_may_use() {
        let config = MachinesConfig::parse(&sample().to_string()).unwrap();
        let homebased = AppName::parse("homebased").unwrap();
        let class = ClassName::parse("do-c16-image").unwrap();

        assert_eq!(config.heartbeat_ttl, Duration::from_secs(180));

        let droplet = config.class_for(&homebased, &class).unwrap();
        assert_eq!(droplet.gate_units.get(), 52);
        assert_eq!(droplet.heartbeat.boot_grace(), Duration::from_secs(480));
        assert_eq!(droplet.billing, Billing::DIGITALOCEAN);

        // a phoenixNAP class bills by the started hour and waits 20 minutes for its boot
        let pnap = config
            .class_for(&homebased, &ClassName::parse("pnap-s2").unwrap())
            .unwrap();
        assert_eq!(pnap.billing, Billing::PHOENIXNAP);
        assert_eq!(pnap.heartbeat.boot_grace(), Duration::from_secs(1_200));
        assert_eq!(pnap.heartbeat.ttl(), Duration::from_secs(180));
        let ProviderClass::PhoenixNap(server) = &pnap.provider else {
            panic!("pnap-s2 is a phoenixNAP class");
        };
        let locations: Vec<&str> = server
            .locations
            .as_slice()
            .iter()
            .map(|l| l.as_str())
            .collect();
        assert_eq!(locations, ["SEA", "NLD"]);

        // a Latitude.sh class bills by the hour for now and waits 15 minutes for its boot
        let lat = config
            .class_for(&homebased, &ClassName::parse("lat-m4").unwrap())
            .unwrap();
        assert_eq!(lat.billing, Billing::LATITUDE);
        assert_eq!(lat.heartbeat.boot_grace(), Duration::from_secs(900));
        assert_eq!(lat.gate_units.get(), 125);
        let ProviderClass::Latitude(server) = &lat.provider else {
            panic!("lat-m4 is a Latitude.sh class");
        };
        let sites: Vec<&str> = server.sites.as_slice().iter().map(|s| s.as_str()).collect();
        assert_eq!(sites, ["DAL", "CHI"]);

        // an AWS class bills per second, boots in 8 minutes and caps its Spot price
        let spot = config
            .class_for(&homebased, &ClassName::parse("aws-c8a").unwrap())
            .unwrap();
        assert_eq!(spot.billing, Billing::AWS);
        assert_eq!(spot.heartbeat.boot_grace(), Duration::from_secs(480));
        assert_eq!(spot.gate_units.get(), 48);
        let ProviderClass::Aws(instance) = &spot.provider else {
            panic!("aws-c8a is an AWS class");
        };
        assert_eq!(instance.root_device_name, "/dev/sda1");
        assert_eq!(instance.spot_max_price.as_str(), "0.45");
        // every type in every zone, cheaper type first
        let pools: Vec<_> = instance
            .pools
            .as_slice()
            .iter()
            .map(|pool| {
                format!(
                    "{}@{}@{}",
                    pool.instance_type.as_str(),
                    pool.availability_zone.as_str(),
                    pool.subnet_id.as_str()
                )
            })
            .collect();
        assert_eq!(
            pools,
            [
                "c8a.4xlarge@us-east-1a@subnet-0123456789abcdef0",
                "c8a.4xlarge@us-east-1b@subnet-0123456789abcdef1",
                "c7a.4xlarge@us-east-1a@subnet-0123456789abcdef0",
                "c7a.4xlarge@us-east-1b@subnet-0123456789abcdef1",
            ]
        );
        assert_eq!(instance.launch_template.id.as_str(), "lt-0123456789abcdef0");
        assert_eq!(instance.launch_template.version.get(), 2);
        let regions: Vec<&str> = config.aws_regions().iter().map(|r| r.as_str()).collect();
        assert_eq!(regions, ["us-east-1"]);
        assert_eq!(
            config.class_for(&AppName::parse("other").unwrap(), &class),
            Err(UnknownClass)
        );
        assert_eq!(
            config.class_for(&homebased, &ClassName::parse("do-gpu").unwrap()),
            Err(UnknownClass)
        );
    }

    #[test]
    fn a_config_with_any_bad_value_is_refused() {
        let broken = |path: &[&str], value: Value| {
            let mut config = sample();
            let mut target = &mut config;
            for key in &path[..path.len() - 1] {
                target = &mut target[*key];
            }
            target[path[path.len() - 1]] = value;
            MachinesConfig::parse(&config.to_string())
        };

        assert!(matches!(
            broken(&["api_base"], json!("http://plain.test")),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(broken(&["max_run_hours"], json!(0)).is_err());
        assert!(broken(&["max_run_hours"], json!(169)).is_err());
        assert!(broken(&["run_spend_cap_usd"], json!("lots")).is_err());
        assert!(broken(&["classes", "do-c16", "price_per_hour_usd"], json!("0")).is_err());
        assert!(broken(&["classes", "do-c16", "price_per_hour_usd"], json!("700")).is_err());
        assert!(broken(&["classes", "do-c16", "size"], json!("C 16")).is_err());
        assert!(broken(&["classes", "do-c16", "surprise"], json!(1)).is_err());
        assert!(broken(&["apps", "homebased", "classes"], json!(["do-gpu"])).is_err());
        assert!(broken(&["classes", "do-c16", "provider"], json!("aws")).is_err());
        // a provider's fields do not fit another's class
        assert!(broken(&["classes", "do-c16", "provider"], json!("phoenixnap")).is_err());
        assert!(broken(&["classes", "pnap-s2", "region"], json!("nyc3")).is_err());
        assert!(broken(&["classes", "pnap-s2", "locations"], json!([])).is_err());
        assert!(broken(&["classes", "pnap-s2", "locations"], json!(["SEA", "SEA"])).is_err());
        assert!(broken(&["classes", "pnap-s2", "locations"], json!(["sea"])).is_err());
        assert!(broken(&["classes", "pnap-s2", "server_type"], json!("S2 C2")).is_err());
        // phoenixNAP refuses a Linux server with no SSH key
        assert!(broken(&["classes", "pnap-s2", "ssh_key_ids"], json!([])).is_err());
        // billing finer than phoenixNAP's hour would undercount
        assert!(broken(&["classes", "pnap-s2", "billing_increment_secs"], json!(60)).is_err());
        assert!(broken(&["classes", "pnap-s2", "boot_grace_secs"], json!(30)).is_err());
        assert!(broken(&["classes", "lat-m4", "sites"], json!(["DAL", "DAL"])).is_err());
        assert!(broken(&["classes", "lat-m4", "sites"], json!([])).is_err());
        assert!(broken(&["classes", "lat-m4", "plan"], json!("m4 metal")).is_err());
        assert!(broken(&["classes", "lat-m4", "locations"], json!(["DAL"])).is_err());
        assert!(broken(&["classes", "lat-m4", "billing_increment_secs"], json!(60)).is_err());
        // a Spot cap above the counted price would let EC2 bill more than the spend shows
        assert!(broken(&["classes", "aws-c8a", "spot_max_price_usd"], json!("0.49")).is_err());
        assert!(broken(&["classes", "aws-c8a", "spot_max_price_usd"], json!("0")).is_err());
        assert!(broken(&["classes", "aws-c8a", "instance_types"], json!([])).is_err());
        assert!(
            broken(
                &["classes", "aws-c8a", "instance_types"],
                json!([
                    "c8a.4xlarge",
                    "c7a.4xlarge",
                    "m7a.4xlarge",
                    "c7i.4xlarge",
                    "c6a.4xlarge"
                ])
            )
            .is_err()
        );
        assert!(
            broken(
                &["classes", "aws-c8a", "instance_types"],
                json!(["c8a.4xlarge", "c8a.4xlarge"])
            )
            .is_err()
        );
        assert!(broken(&["classes", "aws-c8a", "zones"], json!([])).is_err());
        assert!(broken(&["classes", "aws-c8a", "root_volume_gib"], json!(4)).is_err());
        assert!(broken(&["classes", "aws-c8a", "image_id"], json!("ubuntu")).is_err());
        assert!(broken(&["classes", "aws-c8a", "region"], json!("US East")).is_err());
        assert!(broken(&["classes", "aws-c8a", "root_device_name"], json!("sda1")).is_err());
        assert!(
            broken(
                &["classes", "aws-c8a", "launch_template_id"],
                json!("sg-01234567")
            )
            .is_err()
        );
        // a version is a number: `$Latest` would follow every template edit
        assert!(broken(&["classes", "aws-c8a", "launch_template_version"], json!(0)).is_err());
        assert!(
            broken(
                &["classes", "aws-c8a", "launch_template_version"],
                json!("$Latest")
            )
            .is_err()
        );
        // the security group lives in the launch template, which a fleet cannot override
        assert!(
            broken(
                &["classes", "aws-c8a", "security_group_id"],
                json!("sg-01234567")
            )
            .is_err()
        );
        assert!(broken(&["heartbeat_ttl_secs"], json!(30)).is_err());
        // the boot grace may not be shorter than the window it precedes
        assert!(broken(&["heartbeat_ttl_secs"], json!(600)).is_err());
        assert!(broken(&["boot_grace_secs"], json!(120)).is_err());
        let tuned = {
            let mut config = sample();
            config["heartbeat_ttl_secs"] = json!(120);
            config["boot_grace_secs"] = json!(300);
            MachinesConfig::parse(&config.to_string()).unwrap()
        };
        let class = |name: &str| {
            tuned
                .class_for(
                    &AppName::parse("homebased").unwrap(),
                    &ClassName::parse(name).unwrap(),
                )
                .unwrap()
                .clone()
        };
        assert_eq!(tuned.heartbeat_ttl, Duration::from_secs(120));
        assert_eq!(class("do-c16").heartbeat.ttl(), Duration::from_secs(120));
        // the top-level boot grace is DigitalOcean's default only
        assert_eq!(
            class("do-c16").heartbeat.boot_grace(),
            Duration::from_secs(300)
        );
        assert_eq!(
            class("pnap-s2").heartbeat.boot_grace(),
            Duration::from_secs(1_200)
        );

        let mut config = sample();
        config["classes"]["pnap-s2"]["boot_grace_secs"] = json!(900);
        config["classes"]["do-c16"]["billing_increment_secs"] = json!(3_600);
        let own = MachinesConfig::parse(&config.to_string()).unwrap();
        let homebased = AppName::parse("homebased").unwrap();
        let pnap = own
            .class_for(&homebased, &ClassName::parse("pnap-s2").unwrap())
            .unwrap();
        assert_eq!(pnap.heartbeat.boot_grace(), Duration::from_secs(900));
        let hourly = own
            .class_for(&homebased, &ClassName::parse("do-c16").unwrap())
            .unwrap();
        assert_eq!(hourly.billing.increment(), Duration::from_secs(3_600));

        // a run created with a two hour boot grace still starts leases of a class a later
        // config cut to one hour
        let long = Heartbeat::new(Duration::from_secs(180), Duration::from_secs(7_200)).unwrap();
        let policy = long.lease_policy(Duration::from_secs(3_600)).unwrap();
        assert_eq!(policy.boot_grace(), Duration::from_secs(3_600));
    }
}
