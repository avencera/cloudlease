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
    num::{NonZeroU32, NonZeroU64},
    time::Duration,
};

use cloudlease_core::{
    capacity::PermitUnits,
    ids::ResourceId,
    lease::{LeaseError, LeasePolicy},
};
use cloudlease_provider::{
    digitalocean::{DropletImage, Slug},
    phoenixnap::{Location, OsImage, ServerType},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;

use crate::{
    boot::MountPath,
    ids::{AppName, ClassName},
    money::{Billing, MicroUsd},
    provider::{Locations, ProviderName},
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
    volume: Option<RawVolume>,
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
struct RawVolume {
    size_gib: NonZeroU32,
    mount: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    classes: BTreeSet<ClassName>,
}

/// A block storage volume each machine of a class gets, formatted and mounted at boot
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeClass {
    /// The size in GiB
    pub size_gib: NonZeroU32,
    /// Where the boot script mounts it
    pub mount: MountPath,
}

/// A DigitalOcean droplet, with an optional volume
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigitalOceanClass {
    /// The region slug
    pub region: Slug,
    /// The droplet size slug
    pub size: Slug,
    /// The boot image
    pub image: DropletImage,
    /// The volume each machine gets, if any; local disk is preferred
    pub volume: Option<VolumeClass>,
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
    /// IDs of account SSH keys installed for break-glass access
    pub ssh_key_ids: Vec<ResourceId>,
}

/// The provider a class uses and the machine it asks that provider for
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderClass {
    /// A DigitalOcean droplet
    DigitalOcean(DigitalOceanClass),
    /// A phoenixNAP server
    PhoenixNap(PhoenixNapClass),
}

impl ProviderClass {
    /// Returns the provider's name
    #[must_use]
    pub const fn name(&self) -> ProviderName {
        match self {
            Self::DigitalOcean(_) => ProviderName::DigitalOcean,
            Self::PhoenixNap(_) => ProviderName::PhoenixNap,
        }
    }

    /// Returns where the boot script mounts the class's volume, if it has one
    #[must_use]
    pub fn volume_mount(&self) -> Option<&MountPath> {
        match self {
            Self::DigitalOcean(class) => class.volume.as_ref().map(|volume| &volume.mount),
            Self::PhoenixNap(_) => None,
        }
    }
}

/// What one class of machine is and costs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineClass {
    /// The provider and its machine
    pub provider: ProviderClass,
    /// The machine and its volume together, per hour
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
            ConfigError::invalid(field("provider"), "must be digitalocean or phoenixnap")
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
    };

    let price_per_hour = MicroUsd::parse_dollars(&common.price_per_hour_usd)
        .map_err(|error| ConfigError::invalid(field("price_per_hour_usd"), error))?;
    if price_per_hour == MicroUsd::ZERO {
        return Err(ConfigError::invalid(
            field("price_per_hour_usd"),
            "must be positive",
        ));
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
    let volume = raw
        .volume
        .map(|volume| {
            Ok::<_, ConfigError>(VolumeClass {
                size_gib: volume.size_gib,
                mount: MountPath::parse(&volume.mount)
                    .map_err(|error| ConfigError::invalid(field("volume.mount"), error))?,
            })
        })
        .transpose()?;
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
        volume,
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

    Ok(PhoenixNapClass {
        server_type: ServerType::parse(raw.server_type)
            .map_err(|error| ConfigError::invalid(field("server_type"), error))?,
        locations: Locations::new(locations)
            .map_err(|error| ConfigError::invalid(field("locations"), error))?,
        os: OsImage::parse(raw.os).map_err(|error| ConfigError::invalid(field("os"), error))?,
        ssh_key_ids,
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
                "do-c16-disk": {
                    "provider": "digitalocean",
                    "region": "nyc3",
                    "size": "c-16",
                    "image": "212345678",
                    "price_per_hour_usd": "0.514",
                    "max_hours": 12,
                    "volume": { "size_gib": 100, "mount": "/mnt/data" }
                },
                "pnap-s2": {
                    "provider": "phoenixnap",
                    "server_type": "s2.c2.large",
                    "locations": ["SEA", "NLD"],
                    "os": "ubuntu/jammy",
                    "price_per_hour_usd": "0.40",
                    "max_hours": 12
                }
            },
            "apps": { "homebased": { "classes": ["do-c16", "do-c16-disk", "pnap-s2"] } }
        })
    }

    #[test]
    fn a_valid_config_names_the_classes_each_app_may_use() {
        let config = MachinesConfig::parse(&sample().to_string()).unwrap();
        let homebased = AppName::parse("homebased").unwrap();
        let class = ClassName::parse("do-c16-disk").unwrap();

        assert_eq!(config.heartbeat_ttl, Duration::from_secs(180));

        let disk = config.class_for(&homebased, &class).unwrap();
        assert_eq!(disk.gate_units.get(), 52);
        assert_eq!(disk.heartbeat.boot_grace(), Duration::from_secs(480));
        assert_eq!(disk.billing, Billing::DIGITALOCEAN);
        let ProviderClass::DigitalOcean(droplet) = &disk.provider else {
            panic!("do-c16-disk is a DigitalOcean class");
        };
        assert_eq!(droplet.volume.as_ref().unwrap().size_gib.get(), 100);

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
        assert_eq!(pnap.provider.volume_mount(), None);
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
        assert!(
            broken(
                &["classes", "do-c16-disk", "volume"],
                json!({ "size_gib": 1, "mount": "../x" })
            )
            .is_err()
        );
        assert!(broken(&["apps", "homebased", "classes"], json!(["do-gpu"])).is_err());
        assert!(broken(&["classes", "do-c16", "provider"], json!("aws")).is_err());
        // a provider's fields do not fit another's class
        assert!(broken(&["classes", "do-c16", "provider"], json!("phoenixnap")).is_err());
        assert!(broken(&["classes", "pnap-s2", "region"], json!("nyc3")).is_err());
        assert!(broken(&["classes", "pnap-s2", "locations"], json!([])).is_err());
        assert!(broken(&["classes", "pnap-s2", "locations"], json!(["SEA", "SEA"])).is_err());
        assert!(broken(&["classes", "pnap-s2", "locations"], json!(["sea"])).is_err());
        assert!(broken(&["classes", "pnap-s2", "server_type"], json!("S2 C2")).is_err());
        // billing finer than phoenixNAP's hour would undercount
        assert!(broken(&["classes", "pnap-s2", "billing_increment_secs"], json!(60)).is_err());
        assert!(broken(&["classes", "pnap-s2", "boot_grace_secs"], json!(30)).is_err());
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
