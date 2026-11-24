//! Inbound access for one generation's machines, an optional provider capability
//!
//! Not every provider filters inbound traffic the same way: AWS shares one security group and
//! keeps one described rule per generation, while DigitalOcean gives each generation its own
//! firewall. [`IngressPolicy`] is the shape both fit, so it is an extension of
//! [`CloudProvider`](crate::provider::CloudProvider) rather than part of it.

use std::{fmt, future::Future, net::IpAddr, str::FromStr};

use cloudlease_core::tags::OwnerTags;

use crate::error::{ProviderError, ProviderResult};

/// The one source network a generation accepts inbound traffic from
///
/// An IPv4 or IPv6 address with a prefix length no longer than the address, written as the
/// caller gave it, such as `198.51.100.10/32` or `2001:db8::/64`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceCidr {
    text: String,
    ipv6: bool,
}

impl SourceCidr {
    /// Parses `address/prefix` with a prefix length valid for the address family
    pub fn parse(value: impl Into<String>) -> ProviderResult<Self> {
        let text = value.into();
        let Some((address, prefix)) = text.split_once('/') else {
            return Err(invalid_cidr());
        };
        if prefix.is_empty() || prefix.contains('/') {
            return Err(invalid_cidr());
        }

        let address = IpAddr::from_str(address).map_err(|_| invalid_cidr())?;
        let prefix = prefix.parse::<u8>().map_err(|_| invalid_cidr())?;
        let maximum = if address.is_ipv4() { 32 } else { 128 };
        if prefix > maximum {
            return Err(invalid_cidr());
        }

        Ok(Self {
            ipv6: address.is_ipv6(),
            text,
        })
    }

    /// Returns the CIDR as given
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Reports whether the address is IPv6
    #[must_use]
    pub const fn is_ipv6(&self) -> bool {
        self.ipv6
    }
}

impl fmt::Display for SourceCidr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

fn invalid_cidr() -> ProviderError {
    ProviderError::validation("the source must be an IPv4 or IPv6 CIDR with a valid prefix")
}

/// The inbound access policy of one owner generation's machines
///
/// A generation starts closed. While a client holds its lease, the generation accepts TCP on
/// the implementation's configured port from exactly one source; closing removes that access
/// but keeps the policy, so machines created with it stay attached.
pub trait IngressPolicy {
    /// The identifier machines are created with, such as a security group or firewall ID
    type PolicyId;

    /// Makes sure the generation's policy exists and accepts no inbound traffic
    ///
    /// Returns the identifier to create the generation's machines with.
    fn ensure_closed(
        &self,
        owner: &OwnerTags,
    ) -> impl Future<Output = ProviderResult<Self::PolicyId>>;

    /// Accepts inbound TCP on the configured port from `source` only
    ///
    /// Any other source the generation accepted before is removed.
    fn open(
        &self,
        owner: &OwnerTags,
        source: &SourceCidr,
    ) -> impl Future<Output = ProviderResult<Self::PolicyId>>;

    /// Removes all inbound access for the generation and keeps its policy
    ///
    /// A policy that no longer exists counts as closed.
    fn close(&self, owner: &OwnerTags) -> impl Future<Output = ProviderResult<()>>;
}

#[cfg(test)]
mod tests {
    use super::SourceCidr;

    #[test]
    fn cidrs_need_a_prefix_valid_for_their_family() {
        let ipv4 = SourceCidr::parse("198.51.100.10/32").unwrap();
        let ipv6 = SourceCidr::parse("2001:db8::/64").unwrap();
        assert_eq!(ipv4.as_str(), "198.51.100.10/32");
        assert!(!ipv4.is_ipv6());
        assert!(ipv6.is_ipv6());

        for invalid in [
            "198.51.100.10",
            "198.51.100.10/33",
            "2001:db8::/129",
            "198.51.100.10/",
            "/32",
            "198.51.100.10/32/1",
            "example.com/32",
        ] {
            assert!(SourceCidr::parse(invalid).is_err(), "{invalid}");
        }
    }
}
