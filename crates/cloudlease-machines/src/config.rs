//! Classes, prices, caps and the lease policy
//!
//! The Worker reads one JSON object, `MACHINES_CONFIG`, and checks all of it before serving a
//! request, so a bad price or class fails loudly instead of creating a machine. The hourly
//! spend cap is the capacity gate's limit (`HOURLY_SPEND_CAP_CENTS`), not part of this
//! object, so it can be lowered with the gate's override without a deploy.

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
use cloudlease_provider::digitalocean::{DropletImage, Slug};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    boot::MountPath,
    ids::{AppName, ClassName},
    money::MicroUsd,
};

/// How long a lease lives without a renew once its machine has renewed once
pub const HEARTBEAT_TTL: Duration = Duration::from_secs(10 * 60);

/// How long a new lease waits for its machine's first renew
pub const BOOT_GRACE: Duration = Duration::from_secs(15 * 60);

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
    classes: BTreeMap<ClassName, RawClass>,
    apps: BTreeMap<AppName, RawApp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClass {
    region: String,
    size: String,
    image: String,
    price_per_hour_usd: String,
    max_hours: u64,
    #[serde(default)]
    volume: Option<RawVolume>,
    #[serde(default)]
    ssh_key_ids: Vec<NonZeroU64>,
    #[serde(default)]
    vpc_uuid: Option<String>,
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

/// What one class of machine is and costs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineClass {
    /// The region slug
    pub region: Slug,
    /// The droplet size slug
    pub size: Slug,
    /// The boot image
    pub image: DropletImage,
    /// The droplet and its volume together, per hour
    pub price_per_hour: MicroUsd,
    /// The price in whole cents per hour, the machine's weight in the hourly spend gate
    pub gate_units: PermitUnits,
    /// The longest a machine of this class may live
    pub max_lifetime: Duration,
    /// The volume each machine gets, if any; local disk is preferred
    pub volume: Option<VolumeClass>,
    /// SSH keys installed for break-glass access
    pub ssh_key_ids: Vec<NonZeroU64>,
    /// The VPC, when not the region default
    pub vpc_uuid: Option<ResourceId>,
}

impl MachineClass {
    /// Returns the lease policy of this class: the contract's heartbeat and boot grace, and
    /// the class's maximum lifetime
    pub fn lease_policy(&self) -> Result<LeasePolicy, LeaseError> {
        LeasePolicy::new(HEARTBEAT_TTL, self.max_lifetime)?.with_boot_grace(BOOT_GRACE)
    }
}

/// Returns the policy renews are checked against
///
/// A renew uses only the heartbeat TTL, so it needs no class: a lease keeps working even if
/// its class was removed from the configuration after the lease began.
pub fn renew_policy() -> Result<LeasePolicy, LeaseError> {
    LeasePolicy::new(HEARTBEAT_TTL, MAX_RUN_LIFETIME)?.with_boot_grace(BOOT_GRACE)
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

        let classes = raw
            .classes
            .into_iter()
            .map(|(name, class)| {
                let class = machine_class(&name, class)?;
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
            classes,
            apps,
        })
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

fn machine_class(name: &ClassName, raw: RawClass) -> Result<MachineClass, ConfigError> {
    let field = |key: &str| format!("classes.{name}.{key}");
    let slug = |key: &str, value: String| {
        Slug::parse(value).map_err(|error| ConfigError::invalid(field(key), error))
    };

    let image = match raw.image.parse::<u64>().ok().and_then(NonZeroU64::new) {
        Some(id) => DropletImage::Id(id),
        None => DropletImage::Slug(slug("image", raw.image)?),
    };
    let price_per_hour = MicroUsd::parse_dollars(&raw.price_per_hour_usd)
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

    let class = MachineClass {
        region: slug("region", raw.region)?,
        size: slug("size", raw.size)?,
        image,
        price_per_hour,
        gate_units,
        max_lifetime: hours(&field("max_hours"), raw.max_hours)?,
        volume,
        ssh_key_ids: raw.ssh_key_ids,
        vpc_uuid,
    };
    class
        .lease_policy()
        .map_err(|error| ConfigError::invalid(field("max_hours"), error))?;

    Ok(class)
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{Value, json};

    use super::{ConfigError, MachinesConfig, UnknownClass};
    use crate::ids::{AppName, ClassName};

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
                    "region": "nyc3",
                    "size": "c-16",
                    "image": "212345678",
                    "price_per_hour_usd": "0.514",
                    "max_hours": 12,
                    "volume": { "size_gib": 100, "mount": "/mnt/data" }
                }
            },
            "apps": { "homebased": { "classes": ["do-c16", "do-c16-disk"] } }
        })
    }

    #[test]
    fn a_valid_config_names_the_classes_each_app_may_use() {
        let config = MachinesConfig::parse(&sample().to_string()).unwrap();
        let homebased = AppName::parse("homebased").unwrap();
        let class = ClassName::parse("do-c16-disk").unwrap();

        let disk = config.class_for(&homebased, &class).unwrap();
        assert_eq!(disk.gate_units.get(), 52);
        assert_eq!(disk.volume.as_ref().unwrap().size_gib.get(), 100);
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
    }
}
