//! Typed decoding of EC2 Query API responses

use chrono::NaiveDateTime;
use cloudlease_core::time::UnixMillis;
use quick_xml::de::from_reader;
use serde::{Deserialize, de::DeserializeOwned};

use super::ec2::{
    AwsImage, AwsInstance, AwsInstanceDetails, AwsInstanceState, AwsInstanceType,
    AwsInstanceVolume, AwsSecurityGroup, AwsSecurityGroupIngress, AwsSpotInstanceRequest,
    AwsSpotInstanceRequestState, AwsSpotInstanceRequestType, AwsSpotInterruptionBehavior,
    AwsStateReason, AwsSubnet, AwsTag, CreateFleetResponse, FleetError, FleetInstances,
    FleetOverrides, RetiredPlacement,
};
use crate::error::{ProviderError, ProviderErrorCode, ProviderResult};

const MAX_XML_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// One decoded page from an EC2 inventory operation
pub(crate) struct Ec2Page<T> {
    /// Resources returned on this page
    pub(crate) items: Vec<T>,
    /// Opaque token for the next page
    pub(crate) next_token: Option<String>,
}

/// Decodes and validates one `DescribeSubnets` response
pub(crate) fn parse_subnet(body: &[u8], expected_id: &str) -> ProviderResult<AwsSubnet> {
    let response: DescribeSubnetsResponse = parse_response(body)?;
    let subnet = response
        .subnets
        .items
        .into_iter()
        .next()
        .ok_or_else(|| not_found("AWS subnet is absent"))?;
    let subnet = AwsSubnet {
        id: required(subnet.id, "subnetId")?,
        availability_zone: required(subnet.availability_zone, "availabilityZone")?,
        vpc_id: required(subnet.vpc_id, "vpcId")?,
        map_public_ip_on_launch: subnet.map_public_ip_on_launch,
    };
    if subnet.id != expected_id {
        return Err(invalid_response(
            "AWS subnet response ID does not match request",
        ));
    }

    Ok(subnet)
}

/// Decodes and validates one `DescribeImages` response
pub(crate) fn parse_image(body: &[u8], expected_id: &str) -> ProviderResult<AwsImage> {
    let response: DescribeImagesResponse = parse_response(body)?;
    let image = response
        .images
        .items
        .into_iter()
        .next()
        .ok_or_else(|| not_found("AWS AMI is absent"))?;
    let image = AwsImage {
        id: required(image.id, "imageId")?,
        available: required(image.state, "imageState")? == "available",
        root_device_name: required(image.root_device_name, "rootDeviceName")?,
        ebs_backed: required(image.root_device_type, "rootDeviceType")? == "ebs",
        architecture: required(image.architecture, "architecture")?,
        virtualization_type: required(image.virtualization_type, "virtualizationType")?,
        ena_supported: image.ena_supported,
    };
    if image.id != expected_id {
        return Err(invalid_response(
            "AWS AMI response ID does not match request",
        ));
    }

    Ok(image)
}

/// Decodes and validates one `DescribeInstanceTypes` response
pub(crate) fn parse_instance_type(
    body: &[u8],
    expected_name: &str,
) -> ProviderResult<AwsInstanceType> {
    let response: DescribeInstanceTypesResponse = parse_response(body)?;
    let instance_type = response
        .instance_types
        .items
        .into_iter()
        .next()
        .ok_or_else(|| not_found("AWS instance type is absent"))?;
    let name = required(instance_type.name, "instanceType")?;
    if name != expected_name {
        return Err(invalid_response(
            "AWS instance type response does not match request",
        ));
    }

    Ok(AwsInstanceType {
        name,
        available: true,
    })
}

/// Decodes one `DescribeSecurityGroups` page
pub(crate) fn parse_security_group_page(body: &[u8]) -> ProviderResult<Ec2Page<AwsSecurityGroup>> {
    let response: DescribeSecurityGroupsResponse = parse_response(body)?;
    let items = response
        .security_groups
        .items
        .into_iter()
        .map(TryInto::try_into)
        .collect::<ProviderResult<_>>()?;

    Ok(Ec2Page {
        items,
        next_token: non_empty(response.next_token),
    })
}

/// Decodes one `DescribeInstances` page
pub(crate) fn parse_instance_page(body: &[u8]) -> ProviderResult<Ec2Page<AwsInstance>> {
    let response: DescribeInstancesResponse = parse_response(body)?;
    let items = response
        .reservations
        .items
        .into_iter()
        .flat_map(|reservation| reservation.instances.items)
        .map(TryInto::try_into)
        .collect::<ProviderResult<_>>()?;

    Ok(Ec2Page {
        items,
        next_token: non_empty(response.next_token),
    })
}

/// Decodes one `CreateFleet` response, which lists both what launched and what failed
pub(crate) fn parse_create_fleet(body: &[u8]) -> ProviderResult<CreateFleetResponse> {
    let response: CreateFleetXml = parse_response(body)?;
    let instances = response
        .instances
        .items
        .into_iter()
        .map(|item| FleetInstances {
            instance_ids: item
                .instance_ids
                .items
                .into_iter()
                .filter_map(non_empty_value)
                .collect(),
            instance_type: non_empty(item.instance_type),
            availability_zone: non_empty(item.availability_zone),
            subnet_id: non_empty(item.subnet_id),
            overrides: item.launch.into(),
        })
        .collect();
    let errors = response
        .errors
        .items
        .into_iter()
        .map(|item| {
            Ok(FleetError {
                code: required(item.code, "errorCode")?,
                message: non_empty(item.message),
                overrides: item.launch.into(),
            })
        })
        .collect::<ProviderResult<_>>()?;

    Ok(CreateFleetResponse {
        fleet_id: non_empty(response.fleet_id),
        instances,
        errors,
    })
}

/// Decodes one `DescribeSpotInstanceRequests` response
pub(crate) fn parse_spot_instance_requests(
    body: &[u8],
    expected_id: Option<&str>,
) -> ProviderResult<Vec<AwsSpotInstanceRequest>> {
    let response: DescribeSpotInstanceRequestsResponse = parse_response(body)?;
    let requests: Vec<AwsSpotInstanceRequest> = response
        .requests
        .items
        .into_iter()
        .map(TryInto::try_into)
        .collect::<ProviderResult<Vec<_>>>()?;
    if let Some(expected_id) = expected_id
        && !requests.iter().any(|request| request.id == expected_id)
    {
        return Err(not_found("AWS Spot Instance request is absent"));
    }

    Ok(requests)
}

fn parse_response<T: DeserializeOwned>(body: &[u8]) -> ProviderResult<T> {
    if body.len() > MAX_XML_RESPONSE_BYTES {
        return Err(invalid_response("AWS XML response is too large"));
    }

    from_reader(body)
        .map_err(|error| invalid_response(&format!("AWS XML response is invalid: {error}")))
}

fn required(value: Option<String>, field: &str) -> ProviderResult<String> {
    value
        .and_then(non_empty_value)
        .ok_or_else(|| invalid_response(&format!("AWS XML response is missing {field}")))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(non_empty_value)
}

fn non_empty_value(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn not_found(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorCode::NotFound, true, message)
}

fn invalid_response(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorCode::InvalidResponse, false, message)
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct ItemSet<T> {
    #[serde(rename = "item", default)]
    items: Vec<T>,
}

impl<T> Default for ItemSet<T> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

#[derive(Deserialize)]
struct DescribeSubnetsResponse {
    #[serde(rename = "subnetSet", default)]
    subnets: ItemSet<SubnetResponse>,
}

#[derive(Deserialize)]
struct SubnetResponse {
    #[serde(rename = "subnetId")]
    id: Option<String>,
    #[serde(rename = "availabilityZone")]
    availability_zone: Option<String>,
    #[serde(rename = "vpcId")]
    vpc_id: Option<String>,
    #[serde(rename = "mapPublicIpOnLaunch")]
    map_public_ip_on_launch: bool,
}

#[derive(Deserialize)]
struct DescribeImagesResponse {
    #[serde(rename = "imagesSet", default)]
    images: ItemSet<ImageResponse>,
}

#[derive(Deserialize)]
struct ImageResponse {
    #[serde(rename = "imageId")]
    id: Option<String>,
    #[serde(rename = "imageState")]
    state: Option<String>,
    #[serde(rename = "rootDeviceName")]
    root_device_name: Option<String>,
    #[serde(rename = "rootDeviceType")]
    root_device_type: Option<String>,
    architecture: Option<String>,
    #[serde(rename = "virtualizationType")]
    virtualization_type: Option<String>,
    #[serde(rename = "enaSupport")]
    ena_supported: bool,
}

#[derive(Deserialize)]
struct DescribeInstanceTypesResponse {
    #[serde(rename = "instanceTypeSet", default)]
    instance_types: ItemSet<InstanceTypeResponse>,
}

#[derive(Deserialize)]
struct InstanceTypeResponse {
    #[serde(rename = "instanceType")]
    name: Option<String>,
}

#[derive(Deserialize)]
struct DescribeSecurityGroupsResponse {
    #[serde(rename = "securityGroupInfo", default)]
    security_groups: ItemSet<SecurityGroupResponse>,
    #[serde(rename = "nextToken")]
    next_token: Option<String>,
}

#[derive(Deserialize)]
struct SecurityGroupResponse {
    #[serde(rename = "groupId")]
    id: Option<String>,
    #[serde(rename = "groupName")]
    name: Option<String>,
    #[serde(rename = "vpcId")]
    vpc_id: Option<String>,
    #[serde(rename = "ipPermissions", default)]
    ingress: ItemSet<IngressResponse>,
    #[serde(rename = "tagSet", default)]
    tags: ItemSet<TagResponse>,
}

impl TryFrom<SecurityGroupResponse> for AwsSecurityGroup {
    type Error = ProviderError;

    fn try_from(value: SecurityGroupResponse) -> Result<Self, Self::Error> {
        let mut ingress = Vec::new();
        for response in value.ingress.items {
            ingress.extend(response.into_rules()?);
        }
        let tags = value
            .tags
            .items
            .into_iter()
            .map(TryInto::try_into)
            .collect::<ProviderResult<_>>()?;

        Ok(Self {
            id: required(value.id, "groupId")?,
            name: required(value.name, "groupName")?,
            vpc_id: non_empty(value.vpc_id),
            ingress,
            tags,
        })
    }
}

#[derive(Deserialize)]
struct IngressResponse {
    #[serde(rename = "ipProtocol")]
    protocol: Option<String>,
    #[serde(rename = "fromPort")]
    from_port: Option<i32>,
    #[serde(rename = "toPort")]
    to_port: Option<i32>,
    #[serde(rename = "ipRanges", default)]
    ipv4_ranges: ItemSet<Ipv4RangeResponse>,
    #[serde(rename = "ipv6Ranges", default)]
    ipv6_ranges: ItemSet<Ipv6RangeResponse>,
}

impl IngressResponse {
    fn into_rules(self) -> ProviderResult<Vec<AwsSecurityGroupIngress>> {
        let protocol = required(self.protocol, "ipProtocol")?;
        let from_port = self.from_port.and_then(|port| port.try_into().ok());
        let to_port = self.to_port.and_then(|port| port.try_into().ok());
        let cidrs =
            self.ipv4_ranges
                .items
                .into_iter()
                .filter_map(|range| Some((non_empty(range.cidr)?, non_empty(range.description))))
                .chain(self.ipv6_ranges.items.into_iter().filter_map(|range| {
                    Some((non_empty(range.cidr)?, non_empty(range.description)))
                }))
                .collect::<Vec<_>>();
        if cidrs.is_empty() {
            return Err(invalid_response("AWS ingress rule has no CIDR"));
        }

        Ok(cidrs
            .into_iter()
            .map(|(cidr, description)| AwsSecurityGroupIngress {
                protocol: protocol.clone(),
                from_port,
                to_port,
                cidr,
                description,
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct Ipv4RangeResponse {
    #[serde(rename = "cidrIp")]
    cidr: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Ipv6RangeResponse {
    #[serde(rename = "cidrIpv6")]
    cidr: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct TagResponse {
    key: Option<String>,
    value: Option<String>,
}

impl TryFrom<TagResponse> for AwsTag {
    type Error = ProviderError;

    fn try_from(value: TagResponse) -> Result<Self, Self::Error> {
        Ok(Self::new(
            required(value.key, "key")?,
            required(value.value, "value")?,
        ))
    }
}

#[derive(Deserialize)]
struct DescribeInstancesResponse {
    #[serde(rename = "reservationSet", default)]
    reservations: ItemSet<ReservationResponse>,
    #[serde(rename = "nextToken")]
    next_token: Option<String>,
}

#[derive(Deserialize)]
struct ReservationResponse {
    #[serde(rename = "instancesSet", default)]
    instances: ItemSet<InstanceResponse>,
}

#[derive(Deserialize)]
struct CreateFleetXml {
    #[serde(rename = "fleetId")]
    fleet_id: Option<String>,
    #[serde(rename = "fleetInstanceSet", default)]
    instances: ItemSet<FleetInstanceXml>,
    #[serde(rename = "errorSet", default)]
    errors: ItemSet<FleetErrorXml>,
}

#[derive(Deserialize)]
struct FleetInstanceXml {
    #[serde(rename = "instanceIds", default)]
    instance_ids: ItemSet<String>,
    #[serde(rename = "instanceType")]
    instance_type: Option<String>,
    #[serde(rename = "availabilityZone")]
    availability_zone: Option<String>,
    #[serde(rename = "subnetId")]
    subnet_id: Option<String>,
    #[serde(rename = "launchTemplateAndOverrides", default)]
    launch: LaunchTemplateAndOverridesXml,
}

#[derive(Deserialize)]
struct FleetErrorXml {
    #[serde(rename = "errorCode")]
    code: Option<String>,
    #[serde(rename = "errorMessage")]
    message: Option<String>,
    #[serde(rename = "launchTemplateAndOverrides", default)]
    launch: LaunchTemplateAndOverridesXml,
}

#[derive(Default, Deserialize)]
struct LaunchTemplateAndOverridesXml {
    #[serde(default)]
    overrides: OverridesXml,
}

#[derive(Default, Deserialize)]
struct OverridesXml {
    #[serde(rename = "instanceType")]
    instance_type: Option<String>,
    #[serde(rename = "availabilityZone")]
    availability_zone: Option<String>,
    #[serde(rename = "subnetId")]
    subnet_id: Option<String>,
}

impl From<LaunchTemplateAndOverridesXml> for FleetOverrides {
    fn from(value: LaunchTemplateAndOverridesXml) -> Self {
        Self {
            instance_type: non_empty(value.overrides.instance_type),
            availability_zone: non_empty(value.overrides.availability_zone),
            subnet_id: non_empty(value.overrides.subnet_id),
        }
    }
}

#[derive(Deserialize)]
struct DescribeSpotInstanceRequestsResponse {
    #[serde(rename = "spotInstanceRequestSet", default)]
    requests: ItemSet<SpotInstanceRequestResponse>,
    #[serde(rename = "nextToken")]
    _next_token: Option<String>,
}

#[derive(Deserialize)]
struct SpotInstanceRequestResponse {
    #[serde(rename = "spotInstanceRequestId")]
    id: Option<String>,
    #[serde(rename = "instanceId")]
    instance_id: Option<String>,
    state: Option<String>,
    #[serde(rename = "type")]
    request_type: Option<String>,
    #[serde(rename = "instanceInterruptionBehavior")]
    interruption_behavior: Option<String>,
    status: Option<SpotRequestStatusResponse>,
    #[serde(rename = "tagSet", default)]
    tags: ItemSet<TagResponse>,
}

#[derive(Deserialize)]
struct SpotRequestStatusResponse {
    code: Option<String>,
}

impl TryFrom<SpotInstanceRequestResponse> for AwsSpotInstanceRequest {
    type Error = ProviderError;

    fn try_from(value: SpotInstanceRequestResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            id: required(value.id, "spotInstanceRequestId")?,
            instance_id: non_empty(value.instance_id),
            state: parse_spot_request_state(non_empty(value.state).as_deref()),
            request_type: parse_spot_request_type(non_empty(value.request_type).as_deref()),
            interruption_behavior: parse_spot_interruption_behavior(
                non_empty(value.interruption_behavior).as_deref(),
            ),
            status_code: non_empty(value.status.and_then(|status| status.code)),
            tags: value
                .tags
                .items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<ProviderResult<_>>()?,
        })
    }
}

#[derive(Deserialize)]
struct InstanceResponse {
    #[serde(rename = "instanceId")]
    id: Option<String>,
    #[serde(rename = "instanceState")]
    state: InstanceStateResponse,
    #[serde(rename = "instanceType")]
    instance_type: Option<String>,
    #[serde(rename = "imageId")]
    image_id: Option<String>,
    placement: Option<PlacementResponse>,
    #[serde(rename = "subnetId")]
    subnet_id: Option<String>,
    #[serde(rename = "ipAddress")]
    public_ipv4: Option<String>,
    #[serde(rename = "blockDeviceMapping", default)]
    volumes: ItemSet<BlockDeviceResponse>,
    #[serde(rename = "groupSet", default)]
    security_groups: ItemSet<InstanceSecurityGroupResponse>,
    #[serde(rename = "instanceLifecycle")]
    lifecycle: Option<String>,
    #[serde(rename = "spotInstanceRequestId")]
    spot_instance_request_id: Option<String>,
    #[serde(rename = "metadataOptions")]
    metadata_options: Option<MetadataOptionsResponse>,
    #[serde(rename = "stateReason")]
    state_reason: Option<StateReasonResponse>,
    #[serde(rename = "reason")]
    transition_reason: Option<String>,
    #[serde(rename = "tagSet", default)]
    tags: ItemSet<TagResponse>,
}

impl TryFrom<InstanceResponse> for AwsInstance {
    type Error = ProviderError;

    fn try_from(value: InstanceResponse) -> Result<Self, Self::Error> {
        let id = required(value.id, "instanceId")?;
        let state = parse_instance_state(&required(value.state.name, "instanceState.name")?);
        let tags = value
            .tags
            .items
            .into_iter()
            .map(TryInto::try_into)
            .collect::<ProviderResult<_>>()?;
        let spot_instance_request_id = non_empty(value.spot_instance_request_id);
        let state_reason = value
            .state_reason
            .and_then(|reason| non_empty(reason.code))
            .map(|code| AwsStateReason {
                code,
                at: value
                    .transition_reason
                    .as_deref()
                    .and_then(parse_transition_time),
            });
        if state.is_retiring() {
            let placement = RetiredPlacement {
                instance_type: non_empty(value.instance_type),
                availability_zone: value
                    .placement
                    .and_then(|placement| non_empty(placement.availability_zone)),
            };
            let instance = Self::retiring(id, state, placement, tags)?;
            return Ok(with_extras(
                instance,
                spot_instance_request_id,
                state_reason,
            ));
        }

        let volumes = value
            .volumes
            .items
            .into_iter()
            .map(TryInto::try_into)
            .collect::<ProviderResult<_>>()?;
        let security_group_ids = value
            .security_groups
            .items
            .into_iter()
            .map(|group| required(group.id, "groupId"))
            .collect::<ProviderResult<_>>()?;

        let instance = Self::present(
            id,
            state,
            AwsInstanceDetails {
                instance_type: required(value.instance_type, "instanceType")?,
                image_id: required(value.image_id, "imageId")?,
                availability_zone: required(
                    value
                        .placement
                        .and_then(|placement| placement.availability_zone),
                    "availabilityZone",
                )?,
                subnet_id: required(value.subnet_id, "subnetId")?,
                public_ipv4: non_empty(value.public_ipv4),
                volumes,
                security_group_ids,
                spot: non_empty(value.lifecycle).as_deref() == Some("spot"),
                imds_v2_required: value
                    .metadata_options
                    .and_then(|options| non_empty(options.http_tokens))
                    .as_deref()
                    == Some("required"),
            },
            tags,
        )?;

        Ok(with_extras(
            instance,
            spot_instance_request_id,
            state_reason,
        ))
    }
}

fn with_extras(
    instance: AwsInstance,
    spot_instance_request_id: Option<String>,
    state_reason: Option<AwsStateReason>,
) -> AwsInstance {
    let instance = match spot_instance_request_id {
        Some(id) => instance.with_spot_instance_request_id(id),
        None => instance,
    };
    match state_reason {
        Some(reason) => instance.with_state_reason(reason),
        None => instance,
    }
}

/// Reads the time from a transition reason such as
/// `User initiated (2026-11-24 18:30:00 GMT)`
///
/// EC2 documents this form for user-initiated changes; whether a Spot interruption's reason
/// carries a time is unconfirmed, so a reason without one yields `None`.
fn parse_transition_time(reason: &str) -> Option<UnixMillis> {
    let (_, rest) = reason.rsplit_once('(')?;
    let inner = rest.strip_suffix(')')?;
    let time = NaiveDateTime::parse_from_str(inner, "%Y-%m-%d %H:%M:%S GMT").ok()?;

    u64::try_from(time.and_utc().timestamp_millis())
        .ok()
        .and_then(UnixMillis::new)
}

fn parse_instance_state(state: &str) -> AwsInstanceState {
    match state {
        "pending" => AwsInstanceState::Pending,
        "running" => AwsInstanceState::Running,
        "stopping" => AwsInstanceState::Stopping,
        "stopped" => AwsInstanceState::Stopped,
        "shutting-down" => AwsInstanceState::ShuttingDown,
        "terminated" => AwsInstanceState::Terminated,
        _ => AwsInstanceState::Unknown,
    }
}

fn parse_spot_request_state(state: Option<&str>) -> AwsSpotInstanceRequestState {
    match state {
        Some("open") => AwsSpotInstanceRequestState::Open,
        Some("active") => AwsSpotInstanceRequestState::Active,
        Some("closed") => AwsSpotInstanceRequestState::Closed,
        Some("cancelled") => AwsSpotInstanceRequestState::Cancelled,
        Some("failed") => AwsSpotInstanceRequestState::Failed,
        Some("disabled") => AwsSpotInstanceRequestState::Disabled,
        _ => AwsSpotInstanceRequestState::Unknown,
    }
}

fn parse_spot_request_type(request_type: Option<&str>) -> AwsSpotInstanceRequestType {
    match request_type {
        Some("one-time") => AwsSpotInstanceRequestType::OneTime,
        Some("persistent") => AwsSpotInstanceRequestType::Persistent,
        _ => AwsSpotInstanceRequestType::Unknown,
    }
}

fn parse_spot_interruption_behavior(behavior: Option<&str>) -> AwsSpotInterruptionBehavior {
    match behavior {
        Some("stop") => AwsSpotInterruptionBehavior::Stop,
        Some("hibernate") => AwsSpotInterruptionBehavior::Hibernate,
        Some("terminate") => AwsSpotInterruptionBehavior::Terminate,
        _ => AwsSpotInterruptionBehavior::Unknown,
    }
}

#[derive(Deserialize)]
struct InstanceStateResponse {
    name: Option<String>,
}

#[derive(Deserialize)]
struct PlacementResponse {
    #[serde(rename = "availabilityZone")]
    availability_zone: Option<String>,
}

#[derive(Deserialize)]
struct BlockDeviceResponse {
    #[serde(rename = "deviceName")]
    device: Option<String>,
    ebs: Option<EbsAttachmentResponse>,
}

impl TryFrom<BlockDeviceResponse> for AwsInstanceVolume {
    type Error = ProviderError;

    fn try_from(value: BlockDeviceResponse) -> Result<Self, Self::Error> {
        let ebs = value
            .ebs
            .ok_or_else(|| invalid_response("AWS block-device mapping has no EBS data"))?;

        Ok(Self {
            volume_id: required(ebs.volume_id, "volumeId")?,
            device: required(value.device, "deviceName")?,
            state: required(ebs.status, "status")?,
        })
    }
}

#[derive(Deserialize)]
struct EbsAttachmentResponse {
    #[serde(rename = "volumeId")]
    volume_id: Option<String>,
    status: Option<String>,
}

#[derive(Deserialize)]
struct InstanceSecurityGroupResponse {
    #[serde(rename = "groupId")]
    id: Option<String>,
}

#[derive(Deserialize)]
struct StateReasonResponse {
    code: Option<String>,
}

#[derive(Deserialize)]
struct MetadataOptionsResponse {
    #[serde(rename = "httpTokens")]
    http_tokens: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        AwsInstanceState, AwsSpotInstanceRequestState, AwsSpotInstanceRequestType,
        AwsSpotInterruptionBehavior, parse_create_fleet, parse_image, parse_instance_page,
        parse_security_group_page, parse_spot_instance_requests,
    };
    use crate::aws::ec2::{MARKED_FOR_TERMINATION, SPOT_INSTANCE_TERMINATION};

    #[test]
    fn parses_spot_request_state_and_launch_policy() {
        let body = br#"
            <DescribeSpotInstanceRequestsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <spotInstanceRequestSet>
                    <item>
                        <spotInstanceRequestId>sir-0123456789abcdef0</spotInstanceRequestId>
                        <instanceId>i-0123456789abcdef0</instanceId>
                        <state>active</state>
                        <type>persistent</type>
                        <instanceInterruptionBehavior>stop</instanceInterruptionBehavior>
                        <status>
                            <code>marked-for-termination</code>
                            <updateTime>2026-11-24T22:00:00.000Z</updateTime>
                            <message>Your Spot Instance has been marked for termination.</message>
                        </status>
                        <tagSet>
                            <item><key>rb-managed</key><value>rembuild</value></item>
                        </tagSet>
                    </item>
                </spotInstanceRequestSet>
            </DescribeSpotInstanceRequestsResponse>
        "#;

        let requests = parse_spot_instance_requests(body, Some("sir-0123456789abcdef0")).unwrap();
        let request = &requests[0];

        assert_eq!(request.state, AwsSpotInstanceRequestState::Active);
        assert_eq!(request.request_type, AwsSpotInstanceRequestType::Persistent);
        assert_eq!(
            request.interruption_behavior,
            AwsSpotInterruptionBehavior::Stop
        );
        assert_eq!(request.instance_id.as_deref(), Some("i-0123456789abcdef0"));
        assert_eq!(request.status_code.as_deref(), Some(MARKED_FOR_TERMINATION));
    }

    #[test]
    fn parses_the_ec2_image_state_element() {
        let body = br#"
            <DescribeImagesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <imagesSet>
                    <item>
                        <imageId>ami-01234567</imageId>
                        <imageState>available</imageState>
                        <rootDeviceName>/dev/sda1</rootDeviceName>
                        <rootDeviceType>ebs</rootDeviceType>
                        <architecture>x86_64</architecture>
                        <virtualizationType>hvm</virtualizationType>
                        <enaSupport>true</enaSupport>
                    </item>
                </imagesSet>
            </DescribeImagesResponse>
        "#;

        let parsed = parse_image(body, "ami-01234567").unwrap();

        assert!(parsed.available);
    }

    #[test]
    fn preserves_security_group_rule_descriptions_for_targeted_cleanup() {
        let body = br#"
            <DescribeSecurityGroupsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <securityGroupInfo>
                    <item>
                        <groupId>sg-01234567</groupId>
                        <groupName>rembuild-builder-firewall</groupName>
                        <vpcId>vpc-01234567</vpcId>
                        <ipPermissions>
                            <item>
                                <ipProtocol>tcp</ipProtocol>
                                <fromPort>22</fromPort>
                                <toPort>22</toPort>
                                <ipRanges>
                                    <item>
                                        <cidrIp>198.51.100.10/32</cidrIp>
                                        <description>rembuild-ssh:aws:lane_1:1</description>
                                    </item>
                                </ipRanges>
                            </item>
                        </ipPermissions>
                    </item>
                </securityGroupInfo>
            </DescribeSecurityGroupsResponse>
        "#;

        let group = parse_security_group_page(body)
            .unwrap()
            .items
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            group.ingress[0].description.as_deref(),
            Some("rembuild-ssh:aws:lane_1:1")
        );
    }

    #[test]
    fn parses_the_nested_ec2_instance_state_element() {
        let body = br#"
            <DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <reservationSet>
                    <item>
                        <instancesSet>
                            <item>
                                <instanceId>i-01234567</instanceId>
                                <imageId>ami-01234567</imageId>
                                <instanceState><name>running</name></instanceState>
                                <instanceType>c7a.2xlarge</instanceType>
                                <placement><availabilityZone>us-east-2a</availabilityZone></placement>
                                <subnetId>subnet-01234567</subnetId>
                                <ipAddress>198.51.100.10</ipAddress>
                                <groupSet>
                                    <item>
                                        <groupId>sg-01234567</groupId>
                                        <groupName>rembuild-builder-firewall</groupName>
                                    </item>
                                </groupSet>
                                <instanceLifecycle>spot</instanceLifecycle>
                                <spotInstanceRequestId>sir-01234567</spotInstanceRequestId>
                                <metadataOptions><httpTokens>required</httpTokens></metadataOptions>
                            </item>
                        </instancesSet>
                    </item>
                </reservationSet>
            </DescribeInstancesResponse>
        "#;

        let page = parse_instance_page(body).unwrap();
        let instance = page.items.into_iter().next().unwrap();

        assert_eq!(instance.state(), AwsInstanceState::Running);
        let details = instance.details().unwrap();
        assert_eq!(details.public_ipv4.as_deref(), Some("198.51.100.10"));
        assert_eq!(details.security_group_ids, ["sg-01234567"]);
        assert!(details.spot);
        assert!(details.imds_v2_required);
        assert_eq!(instance.spot_instance_request_id(), Some("sir-01234567"));
    }

    #[test]
    fn accepts_a_terminated_instance_without_live_network_fields() {
        let body = br#"
            <DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <reservationSet>
                    <item>
                        <instancesSet>
                            <item>
                                <instanceId>i-01234567</instanceId>
                                <instanceState><name>terminated</name></instanceState>
                                <tagSet>
                                    <item><key>rb-managed</key><value>rembuild</value></item>
                                </tagSet>
                            </item>
                        </instancesSet>
                    </item>
                </reservationSet>
            </DescribeInstancesResponse>
        "#;

        let page = parse_instance_page(body).unwrap();
        let instance = page.items.into_iter().next().unwrap();

        assert_eq!(instance.state(), AwsInstanceState::Terminated);
        assert!(instance.details().is_none());
        assert!(instance.state_reason().is_none());
    }

    #[test]
    fn keeps_the_pool_and_state_reason_of_an_interrupted_instance() {
        let body = br#"
            <DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <reservationSet>
                    <item>
                        <instancesSet>
                            <item>
                                <instanceId>i-01234567</instanceId>
                                <instanceState><name>terminated</name></instanceState>
                                <instanceType>c8a.4xlarge</instanceType>
                                <placement><availabilityZone>us-east-1a</availabilityZone></placement>
                                <reason>Service initiated (2026-11-24 18:30:00 GMT)</reason>
                                <stateReason>
                                    <code>Server.SpotInstanceTermination</code>
                                    <message>Server.SpotInstanceTermination: Spot instance termination</message>
                                </stateReason>
                            </item>
                            <item>
                                <instanceId>i-89abcdef</instanceId>
                                <instanceState><name>terminated</name></instanceState>
                                <reason></reason>
                                <stateReason><code>Server.SpotInstanceTermination</code></stateReason>
                            </item>
                        </instancesSet>
                    </item>
                </reservationSet>
            </DescribeInstancesResponse>
        "#;

        let page = parse_instance_page(body).unwrap();

        let timed = &page.items[0];
        assert_eq!(timed.instance_type(), Some("c8a.4xlarge"));
        assert_eq!(timed.availability_zone(), Some("us-east-1a"));
        let reason = timed.state_reason().unwrap();
        assert_eq!(reason.code, SPOT_INSTANCE_TERMINATION);
        assert_eq!(reason.at.unwrap().as_millis(), 1_795_545_000_000);
        // a reason without a time still names the code
        let untimed = &page.items[1];
        assert_eq!(untimed.instance_type(), None);
        assert_eq!(untimed.state_reason().unwrap().at, None);
    }

    #[test]
    fn reads_an_empty_fleet_and_its_errors() {
        let body = br#"
            <CreateFleetResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <requestId>r-1</requestId>
                <fleetId>fleet-01234567</fleetId>
                <errorSet>
                    <item>
                        <errorCode>UnfulfillableCapacity</errorCode>
                        <errorMessage>Unable to fulfill capacity</errorMessage>
                    </item>
                </errorSet>
                <fleetInstanceSet/>
            </CreateFleetResponse>
        "#;

        let response = parse_create_fleet(body).unwrap();

        assert!(response.instances.is_empty());
        assert_eq!(response.errors[0].code, "UnfulfillableCapacity");
        assert_eq!(response.errors[0].overrides.instance_type, None);
    }

    #[test]
    fn reads_the_zone_and_subnet_an_instant_fleet_reports_outside_the_overrides() {
        let body = br#"
            <CreateFleetResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <requestId>r-1</requestId>
                <fleetId>fleet-01234567</fleetId>
                <errorSet/>
                <fleetInstanceSet>
                    <item>
                        <instanceIds><item>i-01234567</item></instanceIds>
                        <instanceType>c7a.2xlarge</instanceType>
                        <availabilityZone>us-east-1b</availabilityZone>
                        <availabilityZoneId>use1-az2</availabilityZoneId>
                        <subnetId>subnet-0000000b</subnetId>
                        <lifecycle>spot</lifecycle>
                    </item>
                </fleetInstanceSet>
            </CreateFleetResponse>
        "#;

        let response = parse_create_fleet(body).unwrap();
        let launched = &response.instances[0];

        assert_eq!(launched.instance_ids, ["i-01234567"]);
        assert_eq!(launched.instance_type.as_deref(), Some("c7a.2xlarge"));
        assert_eq!(launched.availability_zone.as_deref(), Some("us-east-1b"));
        assert_eq!(launched.subnet_id.as_deref(), Some("subnet-0000000b"));
        assert_eq!(launched.overrides.availability_zone, None);
    }

    #[test]
    fn accepts_a_shutting_down_instance_without_live_network_fields() {
        let body = br#"
            <DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
                <reservationSet>
                    <item>
                        <instancesSet>
                            <item>
                                <instanceId>i-01234567</instanceId>
                                <instanceState><name>shutting-down</name></instanceState>
                                <tagSet>
                                    <item><key>rb-managed</key><value>rembuild</value></item>
                                </tagSet>
                            </item>
                        </instancesSet>
                    </item>
                </reservationSet>
            </DescribeInstancesResponse>
        "#;

        let page = parse_instance_page(body).unwrap();
        let instance = page.items.into_iter().next().unwrap();

        assert_eq!(instance.state(), AwsInstanceState::ShuttingDown);
        assert!(instance.details().is_none());
    }
}
