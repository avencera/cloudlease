//! The typed EC2 seam: one method per EC2 action, and the values they exchange
//!
//! [`Ec2`] keeps signing and XML out of lifecycle code, and lets tests drive the adapter
//! with a stateful fake instead of canned HTTP responses.

use std::{
    collections::HashSet,
    num::{NonZeroU16, NonZeroU32},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use cloudlease_core::time::UnixMillis;

use super::{
    client::{AwsRequestSafety, Ec2Client},
    xml::{
        parse_create_fleet, parse_image, parse_instance_page, parse_instance_type,
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

/// The state reason code EC2 gives an instance it terminated to take its Spot capacity back
pub const SPOT_INSTANCE_TERMINATION: &str = "Server.SpotInstanceTermination";

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

/// Why EC2 last changed an instance's state
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsStateReason {
    /// The code, such as [`SPOT_INSTANCE_TERMINATION`]
    pub code: String,
    /// When the change happened, when EC2's transition reason names a time
    pub at: Option<UnixMillis>,
}

/// The type and zone EC2 still reports for a retiring instance
///
/// A terminated instance keeps them while EC2 lists it, for about an hour, though it loses
/// its network fields.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetiredPlacement {
    /// The instance type
    pub instance_type: Option<String>,
    /// The Availability Zone
    pub availability_zone: Option<String>,
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
    state_reason: Option<AwsStateReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Lifecycle {
    Present {
        state: AwsInstanceState,
        details: AwsInstanceDetails,
    },
    Retiring {
        state: AwsInstanceState,
        placement: RetiredPlacement,
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
            state_reason: None,
        })
    }

    /// Creates a shutting-down or terminated instance
    pub fn retiring(
        id: String,
        state: AwsInstanceState,
        placement: RetiredPlacement,
        tags: Vec<AwsTag>,
    ) -> ProviderResult<Self> {
        if !state.is_retiring() {
            return Err(ProviderError::invalid_response(
                "AWS live instance must include live instance details",
            ));
        }

        Ok(Self {
            id,
            lifecycle: Lifecycle::Retiring { state, placement },
            tags,
            spot_instance_request_id: None,
            state_reason: None,
        })
    }

    /// Records the Spot request that launched the instance
    #[must_use]
    pub fn with_spot_instance_request_id(mut self, id: String) -> Self {
        self.spot_instance_request_id = Some(id);
        self
    }

    /// Records why EC2 last changed the instance's state
    #[must_use]
    pub fn with_state_reason(mut self, reason: AwsStateReason) -> Self {
        self.state_reason = Some(reason);
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
            Lifecycle::Present { state, .. } | Lifecycle::Retiring { state, .. } => state,
        }
    }

    /// Returns the instance type, which EC2 reports even for a terminated instance
    #[must_use]
    pub fn instance_type(&self) -> Option<&str> {
        match &self.lifecycle {
            Lifecycle::Present { details, .. } => Some(&details.instance_type),
            Lifecycle::Retiring { placement, .. } => placement.instance_type.as_deref(),
        }
    }

    /// Returns the Availability Zone, which EC2 reports even for a terminated instance
    #[must_use]
    pub fn availability_zone(&self) -> Option<&str> {
        match &self.lifecycle {
            Lifecycle::Present { details, .. } => Some(&details.availability_zone),
            Lifecycle::Retiring { placement, .. } => placement.availability_zone.as_deref(),
        }
    }

    /// Returns why EC2 last changed the instance's state, when it said
    #[must_use]
    pub const fn state_reason(&self) -> Option<&AwsStateReason> {
        self.state_reason.as_ref()
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

fn is_resource_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        matches!(suffix.len(), 8 | 17)
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

validated!(
    /// A launch template ID such as `lt-0123456789abcdef0`
    LaunchTemplateId,
    "a launch template ID is `lt-` and 8 or 17 lowercase hex digits",
    |value| is_resource_id(value, "lt-")
);

/// One pinned version of a launch template
///
/// The version is a number, never `$Latest` or `$Default`, so a template edit cannot change
/// what a running deployment launches until its configuration names the new version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchTemplate {
    /// The template
    pub id: LaunchTemplateId,
    /// The version
    pub version: NonZeroU32,
}

/// One Spot capacity pool: an instance type in one Availability Zone, with the subnet an
/// instance is launched into there
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotPool {
    /// The instance type
    pub instance_type: InstanceType,
    /// The Availability Zone, which must be the subnet's
    pub availability_zone: AvailabilityZone,
    /// The subnet
    pub subnet_id: SubnetId,
}

impl SpotPool {
    /// Reports whether the pool is `instance_type` in `availability_zone`
    #[must_use]
    pub fn is(&self, instance_type: &str, availability_zone: &str) -> bool {
        self.instance_type.as_str() == instance_type
            && self.availability_zone.as_str() == availability_zone
    }
}

/// An instant `CreateFleet` request for one Spot instance
///
/// The adapter always asks for one instance of capacity type spot, the
/// `price-capacity-optimized` allocation strategy, termination on interruption, IMDSv2 and
/// an encrypted gp3 root volume deleted with the instance, so none of those are options.
/// The security group and the public IPv4 address come from the launch template and the
/// subnets, since a fleet cannot override them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateFleetRequest {
    /// The launch template every pool shares
    pub launch_template: LaunchTemplate,
    /// The pools EC2 may choose from, one launch template override each
    pub pools: Vec<SpotPool>,
    /// The AMI
    pub image_id: AmiId,
    /// The AMI's root device name
    pub root_device_name: String,
    /// The root volume size
    pub root_volume_gib: NonZeroU16,
    /// The most the instance may cost per hour in every pool; `None` caps it at the
    /// On-Demand price
    pub max_price: Option<SpotMaxPrice>,
    /// The boot user data
    pub user_data: UserData,
    /// The tags for the fleet, the instance and its root volume
    pub tags: Vec<AwsTag>,
    /// The token that makes a repeated launch return the first result
    pub client_token: String,
}

/// The pool fields EC2 echoes from the override a fleet instance or error used
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FleetOverrides {
    /// The instance type
    pub instance_type: Option<String>,
    /// The Availability Zone
    pub availability_zone: Option<String>,
    /// The subnet
    pub subnet_id: Option<String>,
}

/// Instances an instant fleet launched from one override
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FleetInstances {
    /// The instance IDs
    pub instance_ids: Vec<String>,
    /// The instance type
    pub instance_type: Option<String>,
    /// The override EC2 used
    pub overrides: FleetOverrides,
}

/// One pool an instant fleet could not launch in, and why
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FleetError {
    /// The EC2 error code, such as `InsufficientInstanceCapacity`
    pub code: String,
    /// EC2's message
    pub message: Option<String>,
    /// The override that failed
    pub overrides: FleetOverrides,
}

/// What an instant fleet launched and what it could not
///
/// EC2 answers a fleet that launched nothing with HTTP 200, so a capacity failure is an
/// empty instance set with errors rather than an API error.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CreateFleetResponse {
    /// The fleet, which EC2 deletes on its own once it has no running instances
    pub fleet_id: Option<String>,
    /// The instances, grouped by the override that launched them
    pub instances: Vec<FleetInstances>,
    /// The pools that failed
    pub errors: Vec<FleetError>,
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

    /// Launches at most one Spot instance with an instant fleet
    fn create_fleet(
        &self,
        request: CreateFleetRequest,
    ) -> LocalFuture<'_, ProviderResult<CreateFleetResponse>>;

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

    fn create_fleet(
        &self,
        request: CreateFleetRequest,
    ) -> LocalFuture<'_, ProviderResult<CreateFleetResponse>> {
        Box::pin(async move {
            let response = self
                .call(
                    "CreateFleet",
                    create_fleet_parameters(&request),
                    AwsRequestSafety::Ambiguous,
                )
                .await?;
            parse_create_fleet(&response.body)
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

fn create_fleet_parameters(request: &CreateFleetRequest) -> Vec<(String, String)> {
    let template = "LaunchTemplateConfigs.1.LaunchTemplateSpecification";
    let mut parameters = vec![
        query("Type", "instant"),
        query("ClientToken", &request.client_token),
        query("TargetCapacitySpecification.TotalTargetCapacity", "1"),
        query(
            "TargetCapacitySpecification.DefaultTargetCapacityType",
            "spot",
        ),
        // AWS's recommended strategy: the cheapest of the pools least likely to be
        // interrupted
        query("SpotOptions.AllocationStrategy", "price-capacity-optimized"),
        // a one-time request that terminates, so an interrupted machine is gone with its
        // root volume and nothing relaunches it after its lease ends
        query("SpotOptions.InstanceInterruptionBehavior", "terminate"),
        query(
            &format!("{template}.LaunchTemplateId"),
            request.launch_template.id.as_str(),
        ),
        query(
            &format!("{template}.Version"),
            &request.launch_template.version.to_string(),
        ),
        query(
            &format!("{template}.LaunchTemplateSpecificationUserData"),
            &BASE64.encode(request.user_data.as_str().as_bytes()),
        ),
    ];
    for (index, pool) in request.pools.iter().enumerate() {
        let prefix = format!("LaunchTemplateConfigs.1.Overrides.{}", index + 1);
        let device = format!("{prefix}.BlockDeviceMapping.1");
        parameters.extend([
            query(
                &format!("{prefix}.InstanceType"),
                pool.instance_type.as_str(),
            ),
            query(&format!("{prefix}.SubnetId"), pool.subnet_id.as_str()),
            query(
                &format!("{prefix}.AvailabilityZone"),
                pool.availability_zone.as_str(),
            ),
            query(&format!("{prefix}.ImageId"), request.image_id.as_str()),
            query(&format!("{prefix}.MetadataOptions.HttpTokens"), "required"),
            query(&format!("{device}.DeviceName"), &request.root_device_name),
            query(&format!("{device}.Ebs.VolumeType"), "gp3"),
            query(&format!("{device}.Ebs.Encrypted"), "true"),
            query(&format!("{device}.Ebs.DeleteOnTermination"), "true"),
            query(
                &format!("{device}.Ebs.VolumeSize"),
                &request.root_volume_gib.to_string(),
            ),
        ]);
        if let Some(max_price) = &request.max_price {
            parameters.push(query(&format!("{prefix}.MaxPrice"), max_price.as_str()));
        }
    }
    // the fleet's Spot request cannot be tagged here; cleanup accepts it untagged
    for (index, resource_type) in ["fleet", "instance", "volume"].into_iter().enumerate() {
        append_tags(
            &mut parameters,
            &format!("TagSpecification.{}", index + 1),
            resource_type,
            &request.tags,
        );
    }

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
    use std::num::{NonZeroU16, NonZeroU32};

    use super::{
        AmiId, AvailabilityZone, AwsSpotInstanceRequestType, AwsTag, CreateFleetRequest,
        CreateFleetResponse, Ec2, InstanceFilter, InstanceType, LaunchTemplate, LaunchTemplateId,
        SpotMaxPrice, SpotPool, SubnetId, TcpIngress, UserData,
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

    fn fleet_request() -> CreateFleetRequest {
        let pool = |instance_type: &str, zone: &str, subnet: &str| SpotPool {
            instance_type: InstanceType::parse(instance_type).unwrap(),
            availability_zone: AvailabilityZone::parse(zone).unwrap(),
            subnet_id: SubnetId::parse(subnet).unwrap(),
        };
        CreateFleetRequest {
            launch_template: LaunchTemplate {
                id: LaunchTemplateId::parse("lt-0123456789abcdef0").unwrap(),
                version: NonZeroU32::new(3).unwrap(),
            },
            pools: vec![
                pool("c8a.4xlarge", "us-east-1a", "subnet-0000000a"),
                pool("c7a.4xlarge", "us-east-1b", "subnet-0000000b"),
            ],
            image_id: AmiId::parse("ami-01234567").unwrap(),
            root_device_name: "/dev/sda1".to_owned(),
            root_volume_gib: NonZeroU16::new(150).unwrap(),
            max_price: Some(SpotMaxPrice::parse("0.45").unwrap()),
            user_data: UserData::parse("#!/bin/sh").unwrap(),
            tags: vec![AwsTag::new("clm-managed", "true")],
            client_token: "clm-instance-token".to_owned(),
        }
    }

    #[test]
    fn creates_an_instant_fleet_with_one_override_per_pool() {
        let transport = FakeTransport::with([ok(br"
            <CreateFleetResponse>
                <fleetId>fleet-01234567</fleetId>
                <errorSet>
                    <item>
                        <errorCode>InsufficientInstanceCapacity</errorCode>
                        <errorMessage>no capacity</errorMessage>
                        <launchTemplateAndOverrides>
                            <overrides>
                                <instanceType>c8a.4xlarge</instanceType>
                                <availabilityZone>us-east-1a</availabilityZone>
                                <subnetId>subnet-0000000a</subnetId>
                            </overrides>
                        </launchTemplateAndOverrides>
                        <lifecycle>spot</lifecycle>
                    </item>
                </errorSet>
                <fleetInstanceSet>
                    <item>
                        <instanceIds><item>i-01234567</item></instanceIds>
                        <instanceType>c7a.4xlarge</instanceType>
                        <launchTemplateAndOverrides>
                            <overrides>
                                <instanceType>c7a.4xlarge</instanceType>
                                <availabilityZone>us-east-1b</availabilityZone>
                                <subnetId>subnet-0000000b</subnetId>
                            </overrides>
                        </launchTemplateAndOverrides>
                        <lifecycle>spot</lifecycle>
                    </item>
                </fleetInstanceSet>
            </CreateFleetResponse>
        ")]);
        let client = client(transport.clone());

        let response = block_on(client.create_fleet(fleet_request())).unwrap();

        assert_eq!(response.fleet_id.as_deref(), Some("fleet-01234567"));
        assert_eq!(response.instances[0].instance_ids, ["i-01234567"]);
        assert_eq!(
            response.instances[0].overrides.availability_zone.as_deref(),
            Some("us-east-1b")
        );
        assert_eq!(response.errors[0].code, "InsufficientInstanceCapacity");
        assert_eq!(
            response.errors[0].overrides.subnet_id.as_deref(),
            Some("subnet-0000000a")
        );

        let body = &transport.bodies()[0];
        let template = "LaunchTemplateConfigs.1.LaunchTemplateSpecification";
        let second = "LaunchTemplateConfigs.1.Overrides.2";
        for expected in [
            "Action=CreateFleet".to_owned(),
            "Type=instant".to_owned(),
            "ClientToken=clm-instance-token".to_owned(),
            "TargetCapacitySpecification.TotalTargetCapacity=1".to_owned(),
            "TargetCapacitySpecification.DefaultTargetCapacityType=spot".to_owned(),
            "SpotOptions.AllocationStrategy=price-capacity-optimized".to_owned(),
            "SpotOptions.InstanceInterruptionBehavior=terminate".to_owned(),
            format!("{template}.LaunchTemplateId=lt-0123456789abcdef0"),
            format!("{template}.Version=3"),
            format!("{template}.LaunchTemplateSpecificationUserData=IyEvYmluL3No"),
            "LaunchTemplateConfigs.1.Overrides.1.InstanceType=c8a.4xlarge".to_owned(),
            format!("{second}.InstanceType=c7a.4xlarge"),
            format!("{second}.SubnetId=subnet-0000000b"),
            format!("{second}.AvailabilityZone=us-east-1b"),
            format!("{second}.ImageId=ami-01234567"),
            format!("{second}.MaxPrice=0.45"),
            format!("{second}.MetadataOptions.HttpTokens=required"),
            format!("{second}.BlockDeviceMapping.1.DeviceName=%2Fdev%2Fsda1"),
            format!("{second}.BlockDeviceMapping.1.Ebs.Encrypted=true"),
            format!("{second}.BlockDeviceMapping.1.Ebs.VolumeType=gp3"),
            format!("{second}.BlockDeviceMapping.1.Ebs.VolumeSize=150"),
            format!("{second}.BlockDeviceMapping.1.Ebs.DeleteOnTermination=true"),
            "TagSpecification.1.ResourceType=fleet".to_owned(),
            "TagSpecification.2.ResourceType=instance".to_owned(),
            "TagSpecification.3.ResourceType=volume".to_owned(),
            "TagSpecification.3.Tag.1.Key=clm-managed&TagSpecification.3.Tag.1.Value=true"
                .to_owned(),
        ] {
            assert!(body.contains(&expected), "{expected} missing from {body}");
        }
        // a fleet cannot override the security group or the network interface
        assert!(!body.contains("SecurityGroup"), "{body}");
        assert!(!body.contains("NetworkInterface"), "{body}");
        assert!(!body.contains("spot-instances-request"), "{body}");
    }

    #[test]
    fn a_fleet_without_a_price_cap_sends_no_max_price() {
        let transport = FakeTransport::with([ok(b"<CreateFleetResponse/>")]);
        let client = client(transport.clone());

        let response = block_on(client.create_fleet(CreateFleetRequest {
            max_price: None,
            ..fleet_request()
        }))
        .unwrap();

        assert_eq!(response, CreateFleetResponse::default());
        assert!(!transport.bodies()[0].contains("MaxPrice"));
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
