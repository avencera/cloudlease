//! The typed EC2 seam: one method per EC2 action, and the values they exchange
//!
//! [`Ec2`] keeps signing and XML out of lifecycle code, and lets tests drive the adapter
//! with a stateful fake instead of canned HTTP responses.

use std::{collections::HashSet, num::NonZeroU16};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

use super::{
    client::{AwsRequestSafety, Ec2Client},
    xml::{
        parse_image, parse_instance_page, parse_instance_type, parse_run_instances,
        parse_security_group_page, parse_spot_instance_requests, parse_subnet,
    },
};
use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    firewall::SourceCidr,
    http::{HttpTransport, LocalFuture, Runtime},
    validated::validated,
};

const MAX_INVENTORY_PAGES: usize = 100;
/// The largest user data EC2 accepts, before base64 encoding
pub const MAX_USER_DATA_BYTES: usize = 16 * 1024;

/// One EC2 tag
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AwsTag {
    /// The case-sensitive key
    pub key: String,
    /// The value
    pub value: String,
}

impl AwsTag {
    /// Creates a tag
    #[must_use]
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// The state EC2 reports for an instance
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AwsInstanceState {
    /// Starting for the first time
    Pending,
    /// Running
    Running,
    /// Stopping; the root volume is kept
    Stopping,
    /// Stopped; the root volume is kept
    Stopped,
    /// Shutting down before termination
    ShuttingDown,
    /// Terminated
    Terminated,
    /// A state this adapter does not know
    Unknown,
}

impl AwsInstanceState {
    /// Reports whether the instance no longer exists for cleanup purposes
    ///
    /// Only `terminated` counts: a `shutting-down` instance still holds its Spot request and
    /// root volume.
    #[must_use]
    pub const fn is_absent(self) -> bool {
        matches!(self, Self::Terminated)
    }

    /// Reports whether EC2 is retiring the instance, so it has no live details
    #[must_use]
    pub const fn is_retiring(self) -> bool {
        matches!(self, Self::ShuttingDown | Self::Terminated)
    }
}

/// One security group ingress rule, one per source CIDR
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsSecurityGroupIngress {
    /// The IP protocol, normally `tcp`
    pub protocol: String,
    /// The first allowed port
    pub from_port: Option<u16>,
    /// The last allowed port
    pub to_port: Option<u16>,
    /// The source IPv4 or IPv6 CIDR
    pub cidr: String,
    /// The rule description, which names the generation that owns the rule
    pub description: Option<String>,
}

impl AwsSecurityGroupIngress {
    /// Reports whether the rule allows exactly TCP on `port`
    #[must_use]
    pub fn is_tcp_port(&self, port: u16) -> bool {
        self.protocol == "tcp" && self.from_port == Some(port) && self.to_port == Some(port)
    }

    /// Reports whether the rule covers the same protocol, port and source as `rule`
    ///
    /// EC2 identifies a rule by these alone; the description is only metadata.
    #[must_use]
    pub fn has_source(&self, rule: &TcpIngress) -> bool {
        self.is_tcp_port(rule.port) && self.cidr == rule.source.as_str()
    }

    /// Reports whether the rule is exactly `rule`, description included
    #[must_use]
    pub fn matches(&self, rule: &TcpIngress) -> bool {
        self.has_source(rule) && self.description.as_deref() == Some(rule.description.as_str())
    }
}

/// One security group
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsSecurityGroup {
    /// The ID, such as `sg-0123`
    pub id: String,
    /// The name
    pub name: String,
    /// The VPC, for a VPC group
    pub vpc_id: Option<String>,
    /// The inbound rules
    pub ingress: Vec<AwsSecurityGroupIngress>,
    /// The tags
    pub tags: Vec<AwsTag>,
}

/// One TCP ingress rule to add or remove
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcpIngress {
    /// The port, used as both ends of the range
    pub port: u16,
    /// The one source CIDR
    pub source: SourceCidr,
    /// The description that names the owning generation
    pub description: String,
}

impl TcpIngress {
    /// Returns the rule as EC2 reports it
    #[must_use]
    pub fn to_ingress(&self) -> AwsSecurityGroupIngress {
        AwsSecurityGroupIngress {
            protocol: "tcp".to_owned(),
            from_port: Some(self.port),
            to_port: Some(self.port),
            cidr: self.source.as_str().to_owned(),
            description: Some(self.description.clone()),
        }
    }
}

/// One EBS volume attached to an instance
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsInstanceVolume {
    /// The volume ID
    pub volume_id: String,
    /// The device name
    pub device: String,
    /// The attachment state
    pub state: String,
}

/// The details EC2 reports for an instance that is not retiring
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsInstanceDetails {
    /// The instance type
    pub instance_type: String,
    /// The AMI
    pub image_id: String,
    /// The Availability Zone
    pub availability_zone: String,
    /// The subnet
    pub subnet_id: String,
    /// The public IPv4 address, once assigned
    pub public_ipv4: Option<String>,
    /// The attached EBS volumes
    pub volumes: Vec<AwsInstanceVolume>,
    /// The attached security groups
    pub security_group_ids: Vec<String>,
    /// Whether the instance runs on Spot capacity
    pub spot: bool,
    /// Whether the instance metadata service requires IMDSv2 tokens
    pub imds_v2_required: bool,
}

/// One instance
///
/// A retiring instance (shutting down or terminated) may lack its network and placement
/// fields, so its details are absent by construction rather than by convention.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsInstance {
    id: String,
    lifecycle: Lifecycle,
    tags: Vec<AwsTag>,
    spot_instance_request_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Lifecycle {
    Present {
        state: AwsInstanceState,
        details: AwsInstanceDetails,
    },
    Retiring {
        state: AwsInstanceState,
    },
}

impl AwsInstance {
    /// Creates an instance that is not retiring
    pub fn present(
        id: String,
        state: AwsInstanceState,
        details: AwsInstanceDetails,
        tags: Vec<AwsTag>,
    ) -> ProviderResult<Self> {
        if state.is_retiring() {
            return Err(ProviderError::invalid_response(
                "AWS retiring instance cannot have live instance details",
            ));
        }

        Ok(Self {
            id,
            lifecycle: Lifecycle::Present { state, details },
            tags,
            spot_instance_request_id: None,
        })
    }

    /// Creates a shutting-down or terminated instance
    pub fn retiring(
        id: String,
        state: AwsInstanceState,
        tags: Vec<AwsTag>,
    ) -> ProviderResult<Self> {
        if !state.is_retiring() {
            return Err(ProviderError::invalid_response(
                "AWS live instance must include live instance details",
            ));
        }

        Ok(Self {
            id,
            lifecycle: Lifecycle::Retiring { state },
            tags,
            spot_instance_request_id: None,
        })
    }

    /// Records the Spot request that launched the instance
    #[must_use]
    pub fn with_spot_instance_request_id(mut self, id: String) -> Self {
        self.spot_instance_request_id = Some(id);
        self
    }

    /// Returns the instance ID
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the state
    #[must_use]
    pub const fn state(&self) -> AwsInstanceState {
        match self.lifecycle {
            Lifecycle::Present { state, .. } | Lifecycle::Retiring { state } => state,
        }
    }

    /// Returns the details of an instance that is not retiring
    #[must_use]
    pub const fn details(&self) -> Option<&AwsInstanceDetails> {
        match &self.lifecycle {
            Lifecycle::Present { details, .. } => Some(details),
            Lifecycle::Retiring { .. } => None,
        }
    }

    /// Returns the tags
    #[must_use]
    pub fn tags(&self) -> &[AwsTag] {
        &self.tags
    }

    /// Returns the value of the `Name` tag
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.tags
            .iter()
            .find(|tag| tag.key == NAME_TAG_KEY)
            .map(|tag| tag.value.as_str())
    }

    /// Returns the Spot request that launched the instance, when EC2 reported one
    #[must_use]
    pub fn spot_instance_request_id(&self) -> Option<&str> {
        self.spot_instance_request_id.as_deref()
    }
}

/// The tag key EC2 shows as a resource's name
pub const NAME_TAG_KEY: &str = "Name";

/// A subnet
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsSubnet {
    /// The ID
    pub id: String,
    /// The Availability Zone
    pub availability_zone: String,
    /// The VPC
    pub vpc_id: String,
    /// Whether instances launched in it get a public IPv4 address
    pub map_public_ip_on_launch: bool,
}

/// An AMI
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsImage {
    /// The ID
    pub id: String,
    /// Whether it can be launched
    pub available: bool,
    /// The root device name
    pub root_device_name: String,
    /// Whether the root device is EBS
    pub ebs_backed: bool,
    /// The CPU architecture
    pub architecture: String,
    /// The virtualization type
    pub virtualization_type: String,
    /// Whether enhanced networking is enabled
    pub ena_supported: bool,
}

/// An instance type the account can use in the region
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsInstanceType {
    /// The name
    pub name: String,
    /// Whether the account can use it
    pub available: bool,
}

/// The state of a Spot Instance request
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AwsSpotInstanceRequestState {
    /// Waiting for capacity
    Open,
    /// Fulfilled by a running instance
    Active,
    /// Finished
    Closed,
    /// Cancelled
    Cancelled,
    /// Could not be fulfilled
    Failed,
    /// Paused because its instance is stopped
    Disabled,
    /// A state this adapter does not know
    Unknown,
}

impl AwsSpotInstanceRequestState {
    /// Reports whether the request could still launch or restart an instance
    ///
    /// An unknown state counts, so cleanup cancels rather than leaves it.
    #[must_use]
    pub const fn may_launch(self) -> bool {
        matches!(
            self,
            Self::Open | Self::Active | Self::Disabled | Self::Unknown
        )
    }
}

/// How long a Spot Instance request lives
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AwsSpotInstanceRequestType {
    /// Fulfilled at most once
    OneTime,
    /// Stays open across interruptions and stops
    Persistent,
    /// A type this adapter does not know
    Unknown,
}

/// What EC2 does to a Spot instance when it reclaims capacity
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AwsSpotInterruptionBehavior {
    /// Stop it, keeping its root volume
    Stop,
    /// Hibernate it
    Hibernate,
    /// Terminate it
    Terminate,
    /// A behavior this adapter does not know
    Unknown,
}

/// One Spot Instance request
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsSpotInstanceRequest {
    /// The ID, such as `sir-0123`
    pub id: String,
    /// The instance fulfilling it
    pub instance_id: Option<String>,
    /// The state
    pub state: AwsSpotInstanceRequestState,
    /// The lifetime policy
    pub request_type: AwsSpotInstanceRequestType,
    /// The interruption behavior
    pub interruption_behavior: AwsSpotInterruptionBehavior,
    /// The tags
    pub tags: Vec<AwsTag>,
}

validated!(
    /// An instance type such as `c7a.2xlarge`
    InstanceType,
    "an AWS instance type is a lowercase family and size joined by one dot",
    |value| {
        let valid_part = |part: &str| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        };
        value.len() <= 63
            && value
                .split_once('.')
                .is_some_and(|(family, size)| valid_part(family) && valid_part(size))
    }
);

validated!(
    /// An AMI ID such as `ami-0123456789abcdef0`
    AmiId,
    "an AMI ID is `ami-` and 8 or 17 lowercase hex digits",
    |value| is_resource_id(value, "ami-")
);

validated!(
    /// A subnet ID such as `subnet-0123456789abcdef0`
    SubnetId,
    "a subnet ID is `subnet-` and 8 or 17 lowercase hex digits",
    |value| is_resource_id(value, "subnet-")
);

validated!(
    /// A security group ID such as `sg-0123456789abcdef0`
    SecurityGroupId,
    "a security group ID is `sg-` and 8 or 17 lowercase hex digits",
    |value| is_resource_id(value, "sg-")
);

validated!(
    /// An Availability Zone such as `us-east-1a`
    AvailabilityZone,
    "an Availability Zone is 1 to 64 lowercase letters, digits or dashes",
    |value| {
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }
);

validated!(
    /// The `Name` tag of an instance
    InstanceName,
    "an instance name is 1 to 256 characters without control characters",
    |value| {
        !value.is_empty() && value.chars().count() <= 256 && !value.chars().any(char::is_control)
    }
);

validated!(
    /// The user data an instance runs at boot, such as a cloud-init document
    UserData,
    "user data must be non-empty and at most 16 KiB",
    |value| !value.trim().is_empty() && value.len() <= MAX_USER_DATA_BYTES
);

validated!(
    /// The most an instance may cost per hour on the Spot market, in US dollars, such as
    /// `0.4`
    SpotMaxPrice,
    "a Spot max price is a positive dollar amount with at most 6 decimals, such as 0.4",
    |value| {
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        !whole.is_empty()
            && whole.len() <= 6
            && digits(whole)
            && fraction.len() <= 6
            && digits(fraction)
            && !value.ends_with('.')
            && value.bytes().any(|byte| matches!(byte, b'1'..=b'9'))
    }
);

/// How long a Spot request lives and what EC2 does to its instance when it reclaims the
/// capacity
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpotLifetime {
    /// A persistent request that stops the instance, keeping its root volume, and starts it
    /// again when capacity returns
    PersistentStop,
    /// A one-time request that terminates the instance, so an interrupted instance is gone
    /// with its root volume and nothing relaunches it
    OneTimeTerminate,
}

impl SpotLifetime {
    const fn request_type(self) -> &'static str {
        match self {
            Self::PersistentStop => "persistent",
            Self::OneTimeTerminate => "one-time",
        }
    }

    const fn interruption_behavior(self) -> &'static str {
        match self {
            Self::PersistentStop => "stop",
            Self::OneTimeTerminate => "terminate",
        }
    }
}

/// The Spot market options of a launch
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotMarket {
    /// The request's lifetime and interruption behavior
    pub lifetime: SpotLifetime,
    /// The most the instance may cost per hour; EC2 refuses the launch when the Spot price
    /// is higher and reclaims the instance when it rises above it. `None` caps it at the
    /// On-Demand price
    pub max_price: Option<SpotMaxPrice>,
}

impl SpotMarket {
    /// A persistent request that stops on interruption, capped at the On-Demand price
    #[must_use]
    pub const fn persistent_stop() -> Self {
        Self {
            lifetime: SpotLifetime::PersistentStop,
            max_price: None,
        }
    }

    /// A one-time request that terminates on interruption
    #[must_use]
    pub const fn one_time_terminate(max_price: Option<SpotMaxPrice>) -> Self {
        Self {
            lifetime: SpotLifetime::OneTimeTerminate,
            max_price,
        }
    }
}

fn is_resource_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        matches!(suffix.len(), 8 | 17)
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

/// A Spot `RunInstances` request
///
/// The adapter always asks for a public IPv4 address, IMDSv2 and an encrypted gp3 root
/// volume deleted with the instance, so none of those are options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunInstancesRequest {
    /// The AMI
    pub image_id: AmiId,
    /// The AMI's root device name
    pub root_device_name: String,
    /// The root volume size
    pub root_volume_gib: NonZeroU16,
    /// The instance type
    pub instance_type: InstanceType,
    /// The subnet
    pub subnet_id: SubnetId,
    /// The Availability Zone
    pub availability_zone: AvailabilityZone,
    /// The security group
    pub security_group_id: SecurityGroupId,
    /// The tags for the instance and its Spot request
    pub tags: Vec<AwsTag>,
    /// The boot user data
    pub user_data: UserData,
    /// The token that makes a repeated launch return the first instance
    pub client_token: String,
    /// The Spot request's lifetime, interruption behavior and price cap
    pub market: SpotMarket,
}

/// Which instances `DescribeInstances` returns
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstanceFilter {
    /// Instances that carry a tag key with any value
    TagKey(String),
    /// Instances that carry a tag key with one value
    Tag(AwsTag),
}

/// The EC2 actions the adapter uses, one method per action
///
/// A launch is never replayed after an ambiguous failure; the caller reads state instead.
pub trait Ec2 {
    /// Describes one subnet
    fn describe_subnet(&self, subnet_id: String) -> LocalFuture<'_, ProviderResult<AwsSubnet>>;

    /// Describes one AMI
    fn describe_image(&self, image_id: String) -> LocalFuture<'_, ProviderResult<AwsImage>>;

    /// Describes one instance type
    fn describe_instance_type(
        &self,
        instance_type: String,
    ) -> LocalFuture<'_, ProviderResult<AwsInstanceType>>;

    /// Describes one security group
    fn describe_security_group(
        &self,
        security_group_id: String,
    ) -> LocalFuture<'_, ProviderResult<AwsSecurityGroup>>;

    /// Adds one described TCP rule, succeeding when it already exists
    fn authorize_tcp_ingress(
        &self,
        security_group_id: String,
        rule: TcpIngress,
    ) -> LocalFuture<'_, ProviderResult<()>>;

    /// Removes one TCP rule, succeeding when it is already gone
    fn revoke_tcp_ingress(
        &self,
        security_group_id: String,
        rule: TcpIngress,
    ) -> LocalFuture<'_, ProviderResult<()>>;

    /// Lists every instance matching a tag filter, across all pages
    fn list_instances(
        &self,
        filter: InstanceFilter,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsInstance>>>;

    /// Describes one instance
    fn describe_instance(
        &self,
        instance_id: String,
    ) -> LocalFuture<'_, ProviderResult<AwsInstance>>;

    /// Stops one EBS-backed instance
    fn stop_instance(&self, instance_id: String) -> LocalFuture<'_, ProviderResult<()>>;

    /// Starts one stopped instance
    fn start_instance(&self, instance_id: String) -> LocalFuture<'_, ProviderResult<()>>;

    /// Cancels one Spot request
    fn cancel_spot_instance_request(
        &self,
        spot_instance_request_id: String,
    ) -> LocalFuture<'_, ProviderResult<()>>;

    /// Replaces one stopped instance's user data
    fn modify_instance_user_data(
        &self,
        instance_id: String,
        user_data: String,
    ) -> LocalFuture<'_, ProviderResult<()>>;

    /// Describes one Spot request by ID
    fn describe_spot_instance_requests(
        &self,
        spot_instance_request_id: String,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsSpotInstanceRequest>>>;

    /// Describes the Spot requests an instance fulfils
    fn describe_spot_instance_requests_for_instance(
        &self,
        instance_id: String,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsSpotInstanceRequest>>>;

    /// Launches one persistent Spot instance
    fn run_instances(
        &self,
        request: RunInstancesRequest,
    ) -> LocalFuture<'_, ProviderResult<AwsInstance>>;

    /// Terminates one instance
    fn terminate_instance(&self, instance_id: String) -> LocalFuture<'_, ProviderResult<()>>;
}

/// Reports whether a failed write may still have happened
///
/// The request may have reached EC2 before the transport, a throttle or a server error
/// ended it, so the caller must read state before deciding it failed.
#[must_use]
pub fn is_ambiguous_failure(error: &ProviderError) -> bool {
    matches!(
        error.code,
        ProviderErrorCode::Network
            | ProviderErrorCode::Timeout
            | ProviderErrorCode::ServerError
            | ProviderErrorCode::RateLimited
    )
}

/// Reports whether EC2 refused an ingress rule because one with the same source exists
pub(crate) fn is_duplicate_ingress(error: &ProviderError) -> bool {
    error.message().contains("InvalidPermission.Duplicate")
}

impl<T: HttpTransport, R: Runtime> Ec2 for Ec2Client<T, R> {
    fn describe_subnet(&self, subnet_id: String) -> LocalFuture<'_, ProviderResult<AwsSubnet>> {
        Box::pin(async move {
            let response = self
                .call(
                    "DescribeSubnets",
                    vec![query("SubnetId.1", &subnet_id)],
                    AwsRequestSafety::ReadOnly,
                )
                .await?;
            parse_subnet(&response.body, &subnet_id)
        })
    }

    fn describe_image(&self, image_id: String) -> LocalFuture<'_, ProviderResult<AwsImage>> {
        Box::pin(async move {
            let response = self
                .call(
                    "DescribeImages",
                    vec![query("ImageId.1", &image_id)],
                    AwsRequestSafety::ReadOnly,
                )
                .await?;
            parse_image(&response.body, &image_id)
        })
    }

    fn describe_instance_type(
        &self,
        instance_type: String,
    ) -> LocalFuture<'_, ProviderResult<AwsInstanceType>> {
        Box::pin(async move {
            let response = self
                .call(
                    "DescribeInstanceTypes",
                    vec![query("InstanceType.1", &instance_type)],
                    AwsRequestSafety::ReadOnly,
                )
                .await?;
            parse_instance_type(&response.body, &instance_type)
        })
    }

    fn describe_security_group(
        &self,
        security_group_id: String,
    ) -> LocalFuture<'_, ProviderResult<AwsSecurityGroup>> {
        Box::pin(async move { self.read_security_group(&security_group_id).await })
    }

    fn authorize_tcp_ingress(
        &self,
        security_group_id: String,
        rule: TcpIngress,
    ) -> LocalFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            let parameters = ingress_parameters(&security_group_id, &rule);
            let error = match self
                .call(
                    "AuthorizeSecurityGroupIngress",
                    parameters,
                    AwsRequestSafety::Ambiguous,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) if is_ambiguous_failure(&error) || is_duplicate_ingress(&error) => error,
                Err(error) => return Err(error),
            };

            // a lost response or a duplicate is success only if exactly this rule is there
            let group = self.read_security_group(&security_group_id).await?;
            if group.ingress.iter().any(|ingress| ingress.matches(&rule)) {
                Ok(())
            } else {
                Err(error)
            }
        })
    }

    fn revoke_tcp_ingress(
        &self,
        security_group_id: String,
        rule: TcpIngress,
    ) -> LocalFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            let parameters = ingress_parameters(&security_group_id, &rule);
            let error = match self
                .call(
                    "RevokeSecurityGroupIngress",
                    parameters,
                    AwsRequestSafety::Ambiguous,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) if is_ambiguous_failure(&error) => error,
                Err(error) => return Err(error),
            };

            let group = self.read_security_group(&security_group_id).await?;
            if group.ingress.iter().all(|ingress| !ingress.matches(&rule)) {
                Ok(())
            } else {
                Err(error)
            }
        })
    }

    fn list_instances(
        &self,
        filter: InstanceFilter,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsInstance>>> {
        Box::pin(async move {
            let mut instances = Vec::new();
            let mut next_token = None;
            let mut seen_tokens = HashSet::new();
            for _ in 0..MAX_INVENTORY_PAGES {
                let response = self
                    .call(
                        "DescribeInstances",
                        paginated_filter(&filter, next_token.as_deref()),
                        AwsRequestSafety::ReadOnly,
                    )
                    .await?;
                let page = parse_instance_page(&response.body)?;
                instances.extend(page.items);
                let Some(token) = page.next_token else {
                    return Ok(instances);
                };
                if !seen_tokens.insert(token.clone()) {
                    return Err(ProviderError::invalid_response(
                        "AWS instance inventory pagination repeated a token",
                    ));
                }
                next_token = Some(token);
            }

            Err(ProviderError::invalid_response(
                "AWS instance inventory exceeded its page limit",
            ))
        })
    }

    fn describe_instance(
        &self,
        instance_id: String,
    ) -> LocalFuture<'_, ProviderResult<AwsInstance>> {
        Box::pin(async move {
            let response = self
                .call(
                    "DescribeInstances",
                    vec![query("InstanceId.1", &instance_id)],
                    AwsRequestSafety::ReadOnly,
                )
                .await?;
            parse_instance_page(&response.body)?
                .items
                .into_iter()
                .find(|instance| instance.id() == instance_id)
                .ok_or_else(|| not_found("AWS EC2 instance is absent"))
        })
    }

    fn stop_instance(&self, instance_id: String) -> LocalFuture<'_, ProviderResult<()>> {
        Box::pin(self.instance_action("StopInstances", instance_id))
    }

    fn start_instance(&self, instance_id: String) -> LocalFuture<'_, ProviderResult<()>> {
        Box::pin(self.instance_action("StartInstances", instance_id))
    }

    fn cancel_spot_instance_request(
        &self,
        spot_instance_request_id: String,
    ) -> LocalFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            self.call(
                "CancelSpotInstanceRequests",
                vec![query("SpotInstanceRequestId.1", &spot_instance_request_id)],
                AwsRequestSafety::Idempotent,
            )
            .await?;
            Ok(())
        })
    }

    fn modify_instance_user_data(
        &self,
        instance_id: String,
        user_data: String,
    ) -> LocalFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            self.call(
                "ModifyInstanceAttribute",
                vec![
                    query("InstanceId", &instance_id),
                    query("UserData.Value", &BASE64.encode(user_data.as_bytes())),
                ],
                AwsRequestSafety::Idempotent,
            )
            .await?;
            Ok(())
        })
    }

    fn describe_spot_instance_requests(
        &self,
        spot_instance_request_id: String,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsSpotInstanceRequest>>> {
        Box::pin(async move {
            let response = self
                .call(
                    "DescribeSpotInstanceRequests",
                    vec![query("SpotInstanceRequestId.1", &spot_instance_request_id)],
                    AwsRequestSafety::ReadOnly,
                )
                .await?;
            parse_spot_instance_requests(&response.body, Some(&spot_instance_request_id))
        })
    }

    fn describe_spot_instance_requests_for_instance(
        &self,
        instance_id: String,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsSpotInstanceRequest>>> {
        Box::pin(async move {
            let response = self
                .call(
                    "DescribeSpotInstanceRequests",
                    vec![
                        query("Filter.1.Name", "instance-id"),
                        query("Filter.1.Value.1", &instance_id),
                    ],
                    AwsRequestSafety::ReadOnly,
                )
                .await?;
            parse_spot_instance_requests(&response.body, None)
        })
    }

    fn run_instances(
        &self,
        request: RunInstancesRequest,
    ) -> LocalFuture<'_, ProviderResult<AwsInstance>> {
        Box::pin(async move {
            let response = self
                .call(
                    "RunInstances",
                    run_instances_parameters(&request),
                    AwsRequestSafety::Ambiguous,
                )
                .await?;
            let run = parse_run_instances(&response.body)?;
            let instance = AwsInstance::present(
                run.instance_id,
                AwsInstanceState::Pending,
                AwsInstanceDetails {
                    instance_type: request.instance_type.as_str().to_owned(),
                    image_id: request.image_id.as_str().to_owned(),
                    availability_zone: request.availability_zone.as_str().to_owned(),
                    subnet_id: request.subnet_id.as_str().to_owned(),
                    public_ipv4: None,
                    volumes: Vec::new(),
                    security_group_ids: vec![request.security_group_id.as_str().to_owned()],
                    spot: true,
                    imds_v2_required: true,
                },
                request.tags,
            )?;

            Ok(instance.with_spot_instance_request_id(run.spot_instance_request_id))
        })
    }

    fn terminate_instance(&self, instance_id: String) -> LocalFuture<'_, ProviderResult<()>> {
        Box::pin(self.instance_action("TerminateInstances", instance_id))
    }
}

impl<T: HttpTransport, R: Runtime> Ec2Client<T, R> {
    async fn read_security_group(
        &self,
        security_group_id: &str,
    ) -> ProviderResult<AwsSecurityGroup> {
        let response = self
            .call(
                "DescribeSecurityGroups",
                vec![query("GroupId.1", security_group_id)],
                AwsRequestSafety::ReadOnly,
            )
            .await?;

        parse_security_group_page(&response.body)?
            .items
            .into_iter()
            .find(|group| group.id == security_group_id)
            .ok_or_else(|| not_found("AWS security group is absent"))
    }

    // stop, start and terminate move an instance toward one state, so a repeat is harmless
    async fn instance_action(&self, action: &str, instance_id: String) -> ProviderResult<()> {
        self.call(
            action,
            vec![query("InstanceId.1", &instance_id)],
            AwsRequestSafety::Idempotent,
        )
        .await?;

        Ok(())
    }
}

fn query(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

fn not_found(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorCode::NotFound, true, message)
}

fn ingress_parameters(security_group_id: &str, rule: &TcpIngress) -> Vec<(String, String)> {
    let port = rule.port.to_string();
    let (range_key, description_key) = if rule.source.is_ipv6() {
        (
            "IpPermissions.1.Ipv6Ranges.1.CidrIpv6",
            "IpPermissions.1.Ipv6Ranges.1.Description",
        )
    } else {
        (
            "IpPermissions.1.IpRanges.1.CidrIp",
            "IpPermissions.1.IpRanges.1.Description",
        )
    };

    vec![
        query("GroupId", security_group_id),
        query("IpPermissions.1.IpProtocol", "tcp"),
        query("IpPermissions.1.FromPort", &port),
        query("IpPermissions.1.ToPort", &port),
        query(range_key, rule.source.as_str()),
        query(description_key, &rule.description),
    ]
}

fn paginated_filter(filter: &InstanceFilter, next_token: Option<&str>) -> Vec<(String, String)> {
    let mut parameters = match filter {
        InstanceFilter::TagKey(key) => vec![
            query("Filter.1.Name", "tag-key"),
            query("Filter.1.Value.1", key),
        ],
        InstanceFilter::Tag(tag) => vec![
            query("Filter.1.Name", &format!("tag:{}", tag.key)),
            query("Filter.1.Value.1", &tag.value),
        ],
    };
    if let Some(next_token) = next_token {
        parameters.push(query("NextToken", next_token));
    }

    parameters
}

fn run_instances_parameters(request: &RunInstancesRequest) -> Vec<(String, String)> {
    let mut parameters = vec![
        query("ImageId", request.image_id.as_str()),
        query("BlockDeviceMapping.1.DeviceName", &request.root_device_name),
        query("BlockDeviceMapping.1.Ebs.VolumeType", "gp3"),
        query("BlockDeviceMapping.1.Ebs.Encrypted", "true"),
        query("BlockDeviceMapping.1.Ebs.DeleteOnTermination", "true"),
        query(
            "BlockDeviceMapping.1.Ebs.VolumeSize",
            &request.root_volume_gib.to_string(),
        ),
        query("InstanceType", request.instance_type.as_str()),
        query("MinCount", "1"),
        query("MaxCount", "1"),
        query(
            "Placement.AvailabilityZone",
            request.availability_zone.as_str(),
        ),
        query("NetworkInterface.1.DeviceIndex", "0"),
        query("NetworkInterface.1.SubnetId", request.subnet_id.as_str()),
        query("NetworkInterface.1.DeleteOnTermination", "true"),
        query("NetworkInterface.1.AssociatePublicIpAddress", "true"),
        query(
            "NetworkInterface.1.SecurityGroupId.1",
            request.security_group_id.as_str(),
        ),
        query("MetadataOptions.HttpTokens", "required"),
        query("InstanceMarketOptions.MarketType", "spot"),
        query(
            "InstanceMarketOptions.SpotOptions.InstanceInterruptionBehavior",
            request.market.lifetime.interruption_behavior(),
        ),
        query(
            "InstanceMarketOptions.SpotOptions.SpotInstanceType",
            request.market.lifetime.request_type(),
        ),
        query("ClientToken", &request.client_token),
        query(
            "UserData",
            &BASE64.encode(request.user_data.as_str().as_bytes()),
        ),
    ];
    if let Some(max_price) = &request.market.max_price {
        parameters.push(query(
            "InstanceMarketOptions.SpotOptions.MaxPrice",
            max_price.as_str(),
        ));
    }
    // the root volume is deleted with the instance, so only the instance and its Spot
    // request carry the tags
    append_tags(
        &mut parameters,
        "TagSpecification.1",
        "instance",
        &request.tags,
    );
    append_tags(
        &mut parameters,
        "TagSpecification.2",
        "spot-instances-request",
        &request.tags,
    );

    parameters
}

fn append_tags(
    parameters: &mut Vec<(String, String)>,
    prefix: &str,
    resource_type: &str,
    tags: &[AwsTag],
) {
    parameters.push(query(&format!("{prefix}.ResourceType"), resource_type));
    for (index, tag) in tags.iter().enumerate() {
        parameters.push(query(&format!("{prefix}.Tag.{}.Key", index + 1), &tag.key));
        parameters.push(query(
            &format!("{prefix}.Tag.{}.Value", index + 1),
            &tag.value,
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use super::{
        AmiId, AvailabilityZone, AwsInstanceState, AwsSpotInstanceRequestType, AwsTag, Ec2,
        InstanceFilter, InstanceType, RunInstancesRequest, SecurityGroupId, SpotMarket,
        SpotMaxPrice, SubnetId, TcpIngress, UserData,
    };
    use crate::{
        aws::client::tests::{FakeTransport, block_on, client, ok, status},
        error::ProviderErrorCode,
        firewall::SourceCidr,
    };

    #[test]
    fn instance_inventory_reads_every_page_with_the_returned_next_token() {
        let transport = FakeTransport::with([
            ok(br"<DescribeInstancesResponse><reservationSet/><nextToken>page-2</nextToken></DescribeInstancesResponse>"),
            ok(br"<DescribeInstancesResponse><reservationSet/></DescribeInstancesResponse>"),
        ]);
        let client = client(transport.clone());

        let instances =
            block_on(client.list_instances(InstanceFilter::TagKey("rb-managed".to_owned())))
                .unwrap();

        assert!(instances.is_empty());
        let bodies = transport.bodies();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains("Filter.1.Name=tag-key&Filter.1.Value.1=rb-managed"));
        assert!(bodies[1].contains("NextToken=page-2"));
    }

    #[test]
    fn a_repeated_page_token_is_an_invalid_response() {
        let page = br"<DescribeInstancesResponse><reservationSet/><nextToken>again</nextToken></DescribeInstancesResponse>";
        let transport = FakeTransport::with([ok(page), ok(page)]);
        let client = client(transport);

        let error =
            block_on(client.list_instances(InstanceFilter::Tag(AwsTag::new("rb-owner", "prj_1"))))
                .unwrap_err();

        assert_eq!(error.code, ProviderErrorCode::InvalidResponse);
    }

    #[test]
    fn runs_a_persistent_spot_instance_with_stop_interruption_behavior() {
        let transport = FakeTransport::with([ok(br"
            <RunInstancesResponse>
                <instancesSet>
                    <item>
                        <instanceId>i-01234567</instanceId>
                        <spotInstanceRequestId>sir-01234567</spotInstanceRequestId>
                    </item>
                </instancesSet>
            </RunInstancesResponse>
        ")]);
        let client = client(transport.clone());
        let request = RunInstancesRequest {
            image_id: AmiId::parse("ami-01234567").unwrap(),
            root_device_name: "/dev/sda1".to_owned(),
            root_volume_gib: NonZeroU16::new(50).unwrap(),
            instance_type: InstanceType::parse("c7a.2xlarge").unwrap(),
            subnet_id: SubnetId::parse("subnet-01234567").unwrap(),
            availability_zone: AvailabilityZone::parse("us-east-1a").unwrap(),
            security_group_id: SecurityGroupId::parse("sg-01234567").unwrap(),
            tags: vec![AwsTag::new("rb-managed", "true")],
            user_data: UserData::parse("#cloud-config").unwrap(),
            client_token: "rb-instance-token".to_owned(),
            market: SpotMarket::persistent_stop(),
        };

        let instance = block_on(client.run_instances(request)).unwrap();

        assert_eq!(instance.id(), "i-01234567");
        assert_eq!(instance.state(), AwsInstanceState::Pending);
        assert_eq!(instance.spot_instance_request_id(), Some("sir-01234567"));
        let body = &transport.bodies()[0];
        for expected in [
            "InstanceMarketOptions.SpotOptions.SpotInstanceType=persistent",
            "InstanceMarketOptions.SpotOptions.InstanceInterruptionBehavior=stop",
            "BlockDeviceMapping.1.Ebs.VolumeSize=50",
            "MetadataOptions.HttpTokens=required",
            "NetworkInterface.1.AssociatePublicIpAddress=true",
            "ClientToken=rb-instance-token",
            "TagSpecification.1.ResourceType=instance",
            "TagSpecification.2.ResourceType=spot-instances-request",
            "TagSpecification.2.Tag.1.Key=rb-managed&TagSpecification.2.Tag.1.Value=true",
        ] {
            assert!(body.contains(expected), "{expected} missing from {body}");
        }
        assert!(!body.contains("MaxPrice"), "{body}");
    }

    #[test]
    fn runs_a_one_time_spot_instance_that_terminates_under_a_price_cap() {
        let transport = FakeTransport::with([ok(br"
            <RunInstancesResponse>
                <instancesSet>
                    <item>
                        <instanceId>i-01234567</instanceId>
                        <spotInstanceRequestId>sir-01234567</spotInstanceRequestId>
                    </item>
                </instancesSet>
            </RunInstancesResponse>
        ")]);
        let client = client(transport.clone());
        let request = RunInstancesRequest {
            image_id: AmiId::parse("ami-01234567").unwrap(),
            root_device_name: "/dev/sda1".to_owned(),
            root_volume_gib: NonZeroU16::new(150).unwrap(),
            instance_type: InstanceType::parse("c8a.4xlarge").unwrap(),
            subnet_id: SubnetId::parse("subnet-01234567").unwrap(),
            availability_zone: AvailabilityZone::parse("us-east-1a").unwrap(),
            security_group_id: SecurityGroupId::parse("sg-01234567").unwrap(),
            tags: vec![AwsTag::new("clm-managed", "true")],
            user_data: UserData::parse("#!/bin/sh").unwrap(),
            client_token: "clm-instance-token".to_owned(),
            market: SpotMarket::one_time_terminate(Some(SpotMaxPrice::parse("0.45").unwrap())),
        };

        block_on(client.run_instances(request)).unwrap();

        let body = &transport.bodies()[0];
        for expected in [
            "InstanceMarketOptions.SpotOptions.SpotInstanceType=one-time",
            "InstanceMarketOptions.SpotOptions.InstanceInterruptionBehavior=terminate",
            "InstanceMarketOptions.SpotOptions.MaxPrice=0.45",
        ] {
            assert!(body.contains(expected), "{expected} missing from {body}");
        }
    }

    #[test]
    fn a_spot_max_price_is_a_positive_dollar_amount() {
        for valid in ["0.45", "1", "0.000001", "12.5"] {
            assert!(SpotMaxPrice::parse(valid).is_ok(), "{valid}");
        }
        for invalid in ["0", "0.0", "", ".5", "1.", "-1", "1e3", "0.0000001", "$1"] {
            assert!(SpotMaxPrice::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn sends_idempotent_spot_lifecycle_actions() {
        let transport = FakeTransport::with([
            ok(b"<StopInstancesResponse/>"),
            ok(b"<StartInstancesResponse/>"),
            ok(b"<CancelSpotInstanceRequestsResponse/>"),
            ok(b"<ModifyInstanceAttributeResponse/>"),
            ok(br"
                <DescribeSpotInstanceRequestsResponse>
                    <spotInstanceRequestSet>
                        <item>
                            <spotInstanceRequestId>sir-01234567</spotInstanceRequestId>
                            <instanceId>i-01234567</instanceId>
                            <state>disabled</state>
                            <type>persistent</type>
                            <instanceInterruptionBehavior>stop</instanceInterruptionBehavior>
                        </item>
                    </spotInstanceRequestSet>
                </DescribeSpotInstanceRequestsResponse>
            "),
        ]);
        let client = client(transport.clone());

        block_on(client.stop_instance("i-01234567".to_owned())).unwrap();
        block_on(client.start_instance("i-01234567".to_owned())).unwrap();
        block_on(client.cancel_spot_instance_request("sir-01234567".to_owned())).unwrap();
        block_on(
            client.modify_instance_user_data("i-01234567".to_owned(), "#cloud-config".to_owned()),
        )
        .unwrap();
        let requests =
            block_on(client.describe_spot_instance_requests("sir-01234567".to_owned())).unwrap();
        assert_eq!(
            requests[0].request_type,
            AwsSpotInstanceRequestType::Persistent
        );

        let bodies = transport.bodies();
        assert!(bodies[0].contains("Action=StopInstances"));
        assert!(bodies[0].contains("InstanceId.1=i-01234567"));
        assert!(bodies[1].contains("Action=StartInstances"));
        assert!(bodies[2].contains("Action=CancelSpotInstanceRequests"));
        assert!(bodies[2].contains("SpotInstanceRequestId.1=sir-01234567"));
        assert!(bodies[3].contains("Action=ModifyInstanceAttribute"));
        assert!(bodies[3].contains("UserData.Value=I2Nsb3VkLWNvbmZpZw%3D%3D"));
        assert!(bodies[4].contains("Action=DescribeSpotInstanceRequests"));
    }

    #[test]
    fn a_duplicate_ingress_is_success_only_when_the_described_rule_exists() {
        let duplicate = br"<Response><Errors><Error><Code>InvalidPermission.Duplicate</Code></Error></Errors></Response>";
        let group = |description: &str| {
            format!(
                "<DescribeSecurityGroupsResponse><securityGroupInfo><item>\
                 <groupId>sg-01234567</groupId><groupName>shared</groupName>\
                 <ipPermissions><item><ipProtocol>tcp</ipProtocol><fromPort>22</fromPort>\
                 <toPort>22</toPort><ipRanges><item><cidrIp>2001:db8::/64</cidrIp>\
                 <description>{description}</description></item></ipRanges></item></ipPermissions>\
                 </item></securityGroupInfo></DescribeSecurityGroupsResponse>"
            )
        };
        let transport = FakeTransport::with([
            status(400, duplicate),
            ok(group("rb-ssh:prj_1:1").as_bytes()),
            status(400, duplicate),
            ok(group("rb-ssh:prj_1:2").as_bytes()),
        ]);
        let client = client(transport.clone());
        let rule = TcpIngress {
            port: 22,
            source: SourceCidr::parse("2001:db8::/64").unwrap(),
            description: "rb-ssh:prj_1:1".to_owned(),
        };

        block_on(client.authorize_tcp_ingress("sg-01234567".to_owned(), rule.clone())).unwrap();
        let error =
            block_on(client.authorize_tcp_ingress("sg-01234567".to_owned(), rule)).unwrap_err();

        assert_eq!(error.code, ProviderErrorCode::Conflict);
        assert!(transport.bodies()[0].contains(
            "IpPermissions.1.Ipv6Ranges.1.CidrIpv6=2001%3Adb8%3A%3A%2F64&IpPermissions.1.Ipv6Ranges.1.Description=rb-ssh%3Aprj_1%3A1"
        ));
    }
}
