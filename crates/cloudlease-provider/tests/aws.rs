//! The AWS adapter, region set and shared security group ingress against a stateful fake EC2

use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    future::Future,
    num::{NonZeroU16, NonZeroU32},
    task::{Context, Poll, Waker},
};

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId, TagPrefix},
    tags::{OwnerTags, TagScheme, UnrecognizedReason},
    time::UnixMillis,
};
use cloudlease_provider::{
    aws::{
        AmiId, AvailabilityZone, Aws, AwsImage, AwsIngress, AwsInstance, AwsInstanceDetails,
        AwsInstanceSpec, AwsInstanceState, AwsInstanceType, AwsKind, AwsRegion, AwsRegions,
        AwsSecurityGroup, AwsSecurityGroupIngress, AwsSpotInstanceRequest,
        AwsSpotInstanceRequestState, AwsSpotInstanceRequestType, AwsSpotInterruptionBehavior,
        AwsStateReason, AwsSubnet, AwsTag, CreateFleetRequest, CreateFleetResponse, Ec2,
        FleetError, FleetInstances, FleetOverrides, IdempotencyKey, InstanceFilter, InstanceName,
        InstanceType, LaunchPools, LaunchTemplate, LaunchTemplateId, RegionalSpec,
        RetiredPlacement, SPOT_INSTANCE_TERMINATION, SecurityGroupId, SharedSecurityGroup,
        SpotHistory, SpotMaxPrice, SpotPool, SubnetId, TcpIngress, UserData,
    },
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    firewall::{IngressPolicy, SourceCidr},
    http::LocalFuture,
    provider::{CloudProvider, Presence},
};

const GROUP: &str = "sg-01234567";
const VPC: &str = "vpc-01234567";

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Fleet { pools: Vec<String>, token: String },
    Cancel(String),
    Terminate(String),
    Authorize(String, String),
    Revoke(String, String),
}

struct FakeSpot {
    instance_id: String,
    state: AwsSpotInstanceRequestState,
    request_type: AwsSpotInstanceRequestType,
    tags: Vec<AwsTag>,
}

#[derive(Default)]
struct State {
    instances: BTreeMap<String, AwsInstance>,
    spot_requests: BTreeMap<String, FakeSpot>,
    // client token to the response it got, like EC2's idempotent CreateFleet
    tokens: BTreeMap<String, CreateFleetResponse>,
    // `type@zone` pools with no Spot capacity, refused in the fleet's error set
    no_capacity: Vec<String>,
    // pools refused with another error code, as `type@zone` to code
    pool_errors: BTreeMap<String, String>,
    // a failure of the whole CreateFleet call, as for a bad template or missing permission
    fleet_failure: Option<ProviderError>,
    fleet_requests: Vec<CreateFleetRequest>,
    // forced failures for the next describe of an instance
    describe_failures: VecDeque<ProviderError>,
    group: Option<AwsSecurityGroup>,
    // a rule another writer adds just before the next authorize lands
    racing_rule: Option<AwsSecurityGroupIngress>,
    calls: Vec<Call>,
    next_id: u32,
}

#[derive(Default)]
struct FakeEc2(RefCell<State>);

impl FakeEc2 {
    fn calls(&self) -> Vec<Call> {
        self.0.borrow().calls.clone()
    }

    fn add_instance(&self, id: &str, state: AwsInstanceState, tags: Vec<AwsTag>, spot: bool) {
        let mut instance = if state.is_retiring() {
            AwsInstance::retiring(
                id.to_owned(),
                state,
                RetiredPlacement::default(),
                tags.clone(),
            )
            .unwrap()
        } else {
            AwsInstance::present(id.to_owned(), state, details("c7a.2xlarge"), tags.clone())
                .unwrap()
        };
        let mut fake = self.0.borrow_mut();
        if spot {
            let request_id = format!("sir-{id}");
            instance = instance.with_spot_instance_request_id(request_id.clone());
            fake.spot_requests.insert(
                request_id,
                FakeSpot {
                    instance_id: id.to_owned(),
                    state: AwsSpotInstanceRequestState::Active,
                    request_type: AwsSpotInstanceRequestType::Persistent,
                    tags,
                },
            );
        }
        fake.instances.insert(id.to_owned(), instance);
    }

    // terminates an instance as EC2 does with `reason` and keeps it listed, with its type
    // and zone, as EC2 does for about an hour
    fn retire(&self, id: &str, reason: &str, at: Option<u64>) {
        let mut fake = self.0.borrow_mut();
        let instance = fake.instances[id].clone();
        let details = instance.details().cloned().unwrap();
        let retired = AwsInstance::retiring(
            id.to_owned(),
            AwsInstanceState::Terminated,
            RetiredPlacement {
                instance_type: Some(details.instance_type),
                availability_zone: Some(details.availability_zone),
            },
            instance.tags().to_vec(),
        )
        .unwrap()
        .with_state_reason(AwsStateReason {
            code: reason.to_owned(),
            at: at.and_then(UnixMillis::new),
        });
        fake.instances.insert(id.to_owned(), retired);
    }

    fn ingress(&self) -> Vec<AwsSecurityGroupIngress> {
        self.0.borrow().group.as_ref().unwrap().ingress.clone()
    }

    fn set_ingress(&self, ingress: Vec<AwsSecurityGroupIngress>) {
        self.0.borrow_mut().group.as_mut().unwrap().ingress = ingress;
    }
}

fn details(instance_type: &str) -> AwsInstanceDetails {
    AwsInstanceDetails {
        instance_type: instance_type.to_owned(),
        image_id: "ami-01234567".to_owned(),
        availability_zone: "us-east-1a".to_owned(),
        subnet_id: "subnet-01234567".to_owned(),
        public_ipv4: None,
        volumes: Vec::new(),
        security_group_ids: vec![GROUP.to_owned()],
        spot: true,
        imds_v2_required: true,
    }
}

fn ready<T: 'static>(value: T) -> LocalFuture<'static, T> {
    Box::pin(async move { value })
}

fn not_found(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorCode::NotFound, false, message)
}

impl Ec2 for FakeEc2 {
    fn describe_subnet(&self, _subnet_id: String) -> LocalFuture<'_, ProviderResult<AwsSubnet>> {
        unreachable!("the adapter never reads subnets")
    }

    fn describe_image(&self, _image_id: String) -> LocalFuture<'_, ProviderResult<AwsImage>> {
        unreachable!("the adapter never reads images")
    }

    fn describe_instance_type(
        &self,
        _instance_type: String,
    ) -> LocalFuture<'_, ProviderResult<AwsInstanceType>> {
        unreachable!("the adapter never reads instance types")
    }

    fn describe_security_group(
        &self,
        security_group_id: String,
    ) -> LocalFuture<'_, ProviderResult<AwsSecurityGroup>> {
        let group = self
            .0
            .borrow()
            .group
            .clone()
            .filter(|group| group.id == security_group_id)
            .ok_or_else(|| not_found("no group"));
        ready(group)
    }

    fn authorize_tcp_ingress(
        &self,
        security_group_id: String,
        rule: TcpIngress,
    ) -> LocalFuture<'_, ProviderResult<()>> {
        let mut fake = self.0.borrow_mut();
        fake.calls.push(Call::Authorize(
            rule.source.to_string(),
            rule.description.clone(),
        ));
        let racing_rule = fake.racing_rule.take();
        let group = fake.group.as_mut().unwrap();
        assert_eq!(group.id, security_group_id);
        group.ingress.extend(racing_rule);
        if group
            .ingress
            .iter()
            .any(|existing| existing.has_source(&rule))
        {
            return ready(Err(ProviderError::new(
                ProviderErrorCode::Conflict,
                false,
                "AWS EC2 AuthorizeSecurityGroupIngress request failed with error code InvalidPermission.Duplicate",
            )));
        }
        group.ingress.push(rule.to_ingress());
        ready(Ok(()))
    }

    fn revoke_tcp_ingress(
        &self,
        _security_group_id: String,
        rule: TcpIngress,
    ) -> LocalFuture<'_, ProviderResult<()>> {
        let mut fake = self.0.borrow_mut();
        fake.calls.push(Call::Revoke(
            rule.source.to_string(),
            rule.description.clone(),
        ));
        // EC2 removes a rule by protocol, port and source; the description does not matter
        let group = fake.group.as_mut().unwrap();
        group.ingress.retain(|existing| !existing.has_source(&rule));
        ready(Ok(()))
    }

    fn list_instances(
        &self,
        filter: InstanceFilter,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsInstance>>> {
        let matches = |tags: &[AwsTag]| match &filter {
            InstanceFilter::TagKey(key) => tags.iter().any(|tag| &tag.key == key),
            InstanceFilter::Tag(wanted) => tags.contains(wanted),
        };
        let instances = self
            .0
            .borrow()
            .instances
            .values()
            .filter(|instance| matches(instance.tags()))
            .cloned()
            .collect();
        ready(Ok(instances))
    }

    fn describe_instance(
        &self,
        instance_id: String,
    ) -> LocalFuture<'_, ProviderResult<AwsInstance>> {
        let mut fake = self.0.borrow_mut();
        if let Some(error) = fake.describe_failures.pop_front() {
            return ready(Err(error));
        }
        let instance = fake
            .instances
            .get(&instance_id)
            .cloned()
            .ok_or_else(|| not_found("no instance"));
        ready(instance)
    }

    fn stop_instance(&self, _instance_id: String) -> LocalFuture<'_, ProviderResult<()>> {
        unreachable!("the adapter never stops instances")
    }

    fn start_instance(&self, _instance_id: String) -> LocalFuture<'_, ProviderResult<()>> {
        unreachable!("the adapter never starts instances")
    }

    fn cancel_spot_instance_request(
        &self,
        spot_instance_request_id: String,
    ) -> LocalFuture<'_, ProviderResult<()>> {
        let mut fake = self.0.borrow_mut();
        fake.calls
            .push(Call::Cancel(spot_instance_request_id.clone()));
        if let Some(request) = fake.spot_requests.get_mut(&spot_instance_request_id) {
            request.state = AwsSpotInstanceRequestState::Cancelled;
        }
        ready(Ok(()))
    }

    fn modify_instance_user_data(
        &self,
        _instance_id: String,
        _user_data: String,
    ) -> LocalFuture<'_, ProviderResult<()>> {
        unreachable!("the adapter never changes user data")
    }

    fn describe_spot_instance_requests(
        &self,
        spot_instance_request_id: String,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsSpotInstanceRequest>>> {
        let fake = self.0.borrow();
        let requests = fake
            .spot_requests
            .get(&spot_instance_request_id)
            .map(|request| spot(&spot_instance_request_id, request))
            .into_iter()
            .collect();
        ready(Ok(requests))
    }

    fn describe_spot_instance_requests_for_instance(
        &self,
        instance_id: String,
    ) -> LocalFuture<'_, ProviderResult<Vec<AwsSpotInstanceRequest>>> {
        let fake = self.0.borrow();
        let requests = fake
            .spot_requests
            .iter()
            .filter(|(_, request)| request.instance_id == instance_id)
            .map(|(id, request)| spot(id, request))
            .collect();
        ready(Ok(requests))
    }

    // launches in the first pool, by the order sent, that has capacity; EC2 would pick by
    // price and capacity instead, which the adapter does not depend on
    fn create_fleet(
        &self,
        request: CreateFleetRequest,
    ) -> LocalFuture<'_, ProviderResult<CreateFleetResponse>> {
        let mut fake = self.0.borrow_mut();
        fake.calls.push(Call::Fleet {
            pools: request.pools.iter().map(pool_name).collect(),
            token: request.client_token.clone(),
        });
        fake.fleet_requests.push(request.clone());
        if let Some(error) = fake.fleet_failure.clone() {
            return ready(Err(error));
        }
        if let Some(response) = fake.tokens.get(&request.client_token) {
            return ready(Ok(response.clone()));
        }

        let mut response = CreateFleetResponse {
            fleet_id: Some(format!("fleet-{:08x}", fake.next_id + 1)),
            ..CreateFleetResponse::default()
        };
        for pool in &request.pools {
            let name = pool_name(pool);
            let refusal = if fake.no_capacity.contains(&name) {
                Some("InsufficientInstanceCapacity".to_owned())
            } else {
                fake.pool_errors.get(&name).cloned()
            };
            if let Some(code) = refusal {
                response.errors.push(FleetError {
                    code,
                    message: Some("refused".to_owned()),
                    overrides: overrides(pool),
                });
                continue;
            }

            fake.next_id += 1;
            let id = format!("i-{:08x}", fake.next_id);
            let mut details = details(pool.instance_type.as_str());
            details.availability_zone = pool.availability_zone.as_str().to_owned();
            details.subnet_id = pool.subnet_id.as_str().to_owned();
            let instance = AwsInstance::present(
                id.clone(),
                AwsInstanceState::Pending,
                details,
                request.tags.clone(),
            )
            .unwrap();
            // a fleet's one-time Spot request carries no tags
            fake.spot_requests.insert(
                format!("sir-{id}"),
                FakeSpot {
                    instance_id: id.clone(),
                    state: AwsSpotInstanceRequestState::Active,
                    request_type: AwsSpotInstanceRequestType::OneTime,
                    tags: Vec::new(),
                },
            );
            fake.instances.insert(id.clone(), instance);
            response.instances.push(FleetInstances {
                instance_ids: vec![id],
                instance_type: Some(pool.instance_type.as_str().to_owned()),
                overrides: overrides(pool),
            });
            break;
        }
        fake.tokens.insert(request.client_token, response.clone());
        ready(Ok(response))
    }

    fn terminate_instance(&self, instance_id: String) -> LocalFuture<'_, ProviderResult<()>> {
        let mut fake = self.0.borrow_mut();
        fake.calls.push(Call::Terminate(instance_id.clone()));
        let Some(instance) = fake.instances.get(&instance_id).cloned() else {
            return ready(Err(not_found("no instance")));
        };
        let terminated = AwsInstance::retiring(
            instance_id.clone(),
            AwsInstanceState::Terminated,
            RetiredPlacement::default(),
            instance.tags().to_vec(),
        )
        .unwrap();
        fake.instances.insert(instance_id, terminated);
        ready(Ok(()))
    }
}

fn spot(id: &str, request: &FakeSpot) -> AwsSpotInstanceRequest {
    AwsSpotInstanceRequest {
        id: id.to_owned(),
        instance_id: Some(request.instance_id.clone()),
        state: request.state,
        request_type: request.request_type,
        interruption_behavior: match request.request_type {
            AwsSpotInstanceRequestType::OneTime => AwsSpotInterruptionBehavior::Terminate,
            _ => AwsSpotInterruptionBehavior::Stop,
        },
        tags: request.tags.clone(),
    }
}

fn pool_name(pool: &SpotPool) -> String {
    format!(
        "{}@{}",
        pool.instance_type.as_str(),
        pool.availability_zone.as_str()
    )
}

fn overrides(pool: &SpotPool) -> FleetOverrides {
    FleetOverrides {
        instance_type: Some(pool.instance_type.as_str().to_owned()),
        availability_zone: Some(pool.availability_zone.as_str().to_owned()),
        subnet_id: Some(pool.subnet_id.as_str().to_owned()),
    }
}

fn pool(instance_type: &str, zone: &str) -> SpotPool {
    SpotPool {
        instance_type: InstanceType::parse(instance_type).unwrap(),
        availability_zone: AvailabilityZone::parse(zone).unwrap(),
        subnet_id: SubnetId::parse(format!("subnet-0000000{}", &zone[zone.len() - 1..])).unwrap(),
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}

fn scheme() -> TagScheme {
    TagScheme::new(TagPrefix::parse("rb").unwrap())
}

fn owner(project: &str, generation: u64) -> OwnerTags {
    OwnerTags {
        owner: OwnerId::parse(project).unwrap(),
        generation: Generation::new(generation).unwrap(),
        ceiling: None,
    }
}

// every type in us-east-1a and us-east-1b, in that order
fn spec(types: &[&str]) -> AwsInstanceSpec {
    AwsInstanceSpec {
        name: InstanceName::parse("rb-builder-prj_1-1").unwrap(),
        pools: LaunchPools::new(
            types
                .iter()
                .flat_map(|name| [pool(name, "us-east-1a"), pool(name, "us-east-1b")])
                .collect(),
        )
        .unwrap(),
        launch_template: LaunchTemplate {
            id: LaunchTemplateId::parse("lt-01234567").unwrap(),
            version: NonZeroU32::new(4).unwrap(),
        },
        image_id: AmiId::parse("ami-01234567").unwrap(),
        root_device_name: "/dev/sda1".to_owned(),
        root_volume_gib: NonZeroU16::new(50).unwrap(),
        user_data: UserData::parse("#cloud-config").unwrap(),
        idempotency_key: IdempotencyKey::parse("op-1").unwrap(),
        max_price: Some(SpotMaxPrice::parse("0.45").unwrap()),
    }
}

fn tag(key: &str, value: &str) -> AwsTag {
    AwsTag::new(key, value)
}

fn pairs(owner: &OwnerTags) -> Vec<AwsTag> {
    vec![
        tag("rb-managed", "true"),
        tag("rb-owner", owner.owner.as_str()),
        tag("rb-gen", &owner.generation.to_string()),
        tag("rb-kind", "instance"),
        tag("Name", "rb-builder"),
    ]
}

fn legacy_tags() -> Vec<AwsTag> {
    vec![
        tag("rb-managed", "rembuild"),
        tag("rb-project", "aws"),
        tag("rb-lane", "lane_1"),
        tag("rb-generation", "1"),
        tag("rb-kind", "builder"),
        tag("Name", "rb-builder-aws-lane_1-1"),
    ]
}

fn id(value: &str) -> ResourceId {
    ResourceId::parse(value).unwrap()
}

fn fleet_calls(aws: &Aws<FakeEc2>) -> Vec<(Vec<String>, String)> {
    aws.ec2()
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Fleet { pools, token } => Some((pools, token)),
            _ => None,
        })
        .collect()
}

#[test]
fn create_offers_every_pool_in_one_fleet_and_repeats_idempotently() {
    let ec2 = FakeEc2::default();
    let aws = Aws::new(ec2, scheme());
    let owner = owner("prj_1", 3);

    let first = block_on(aws.create(&owner, &spec(&["c7a.2xlarge", "c7i.2xlarge"]))).unwrap();
    let second = block_on(aws.create(&owner, &spec(&["c7a.2xlarge", "c7i.2xlarge"]))).unwrap();

    // the repeat returns the first launch's instance and launches nothing more
    assert_eq!(first, second);
    assert_eq!(aws.ec2().0.borrow().instances.len(), 1);
    let expected_tags = [
        tag("rb-managed", "true"),
        tag("rb-owner", "prj_1"),
        tag("rb-gen", "3"),
        tag("rb-kind", "instance"),
        tag("Name", "rb-builder-prj_1-1"),
    ];
    let instance = aws.ec2().0.borrow().instances[first.as_str()].clone();
    assert_eq!(instance.tags(), expected_tags);

    let fake = aws.ec2().0.borrow();
    let request = &fake.fleet_requests[0];
    assert_eq!(
        request.pools.iter().map(pool_name).collect::<Vec<_>>(),
        [
            "c7a.2xlarge@us-east-1a",
            "c7a.2xlarge@us-east-1b",
            "c7i.2xlarge@us-east-1a",
            "c7i.2xlarge@us-east-1b",
        ]
    );
    assert_eq!(request.pools[1].subnet_id.as_str(), "subnet-0000000b");
    assert_eq!(request.tags, expected_tags);
    assert_eq!(request.user_data.as_str(), "#cloud-config");
    assert_eq!(request.launch_template.id.as_str(), "lt-01234567");
    assert_eq!(request.launch_template.version.get(), 4);
    assert_eq!(request.image_id.as_str(), "ami-01234567");
    assert_eq!(request.root_volume_gib.get(), 50);
    assert_eq!(
        request.max_price.as_ref().map(SpotMaxPrice::as_str),
        Some("0.45")
    );
    drop(fake);

    // one token per launch, whatever its pools
    let calls = fleet_calls(&aws);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, calls[1].1);
    assert!(calls[0].1.starts_with("rb-instance-") && calls[0].1.len() == 44);

    // another generation or idempotency key is another launch
    let mut other_key = spec(&["c7a.2xlarge"]);
    other_key.idempotency_key = IdempotencyKey::parse("op-2").unwrap();
    let third = block_on(aws.create(&owner, &other_key)).unwrap();
    let fourth = block_on(aws.create(&self::owner("prj_1", 4), &spec(&["c7a.2xlarge"]))).unwrap();
    assert_ne!(third, first);
    assert_ne!(fourth, first);
    assert_ne!(fourth, third);
}

#[test]
fn launch_reports_the_pool_ec2_chose_after_others_refused() {
    let ec2 = FakeEc2::default();
    ec2.0.borrow_mut().no_capacity = vec![
        "c7a.2xlarge@us-east-1a".to_owned(),
        "c7a.2xlarge@us-east-1b".to_owned(),
    ];
    let aws = Aws::new(ec2, scheme());

    let launched =
        block_on(aws.launch(&owner("prj_1", 1), &spec(&["c7a.2xlarge", "c7i.2xlarge"]))).unwrap();

    assert_eq!(launched.pool, pool("c7i.2xlarge", "us-east-1a"));
    let details = launched.instance.details().unwrap();
    assert_eq!(details.instance_type, "c7i.2xlarge");
    assert_eq!(details.availability_zone, "us-east-1a");
    assert_eq!(details.subnet_id, "subnet-0000000a");
    assert!(details.spot && details.imds_v2_required);
    // one call, not one per pool
    assert_eq!(fleet_calls(&aws).len(), 1);
}

#[test]
fn a_fleet_that_launches_nothing_names_the_refused_pools() {
    let ec2 = FakeEc2::default();
    ec2.0.borrow_mut().no_capacity = vec![
        "c7a.2xlarge@us-east-1a".to_owned(),
        "c7a.2xlarge@us-east-1b".to_owned(),
    ];
    let aws = Aws::new(ec2, scheme());

    let error = block_on(aws.launch(&owner("prj_1", 1), &spec(&["c7a.2xlarge"]))).unwrap_err();

    assert_eq!(error.code, ProviderErrorCode::NoCapacity);
    assert!(!error.retryable);
    assert_eq!(
        error.message(),
        "AWS has no Spot capacity under the price cap for c7a.2xlarge in us-east-1a, \
         c7a.2xlarge in us-east-1b"
    );

    // a pool refused for its price is no capacity under the cap too, and a pool that fails
    // otherwise does not hide the others' refusals
    let ec2 = FakeEc2::default();
    ec2.0.borrow_mut().pool_errors = BTreeMap::from([
        (
            "c7a.2xlarge@us-east-1a".to_owned(),
            "SpotMaxPriceTooLow".to_owned(),
        ),
        (
            "c7a.2xlarge@us-east-1b".to_owned(),
            "Unsupported".to_owned(),
        ),
    ]);
    let aws = Aws::new(ec2, scheme());
    let error = block_on(aws.launch(&owner("prj_1", 1), &spec(&["c7a.2xlarge"]))).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::NoCapacity);
    assert!(error.message().contains("for c7a.2xlarge in us-east-1a;"));
    assert!(
        error
            .message()
            .contains("other pools failed: c7a.2xlarge in us-east-1b (Unsupported: refused)")
    );
}

#[test]
fn a_hard_fleet_failure_keeps_its_class() {
    // a fleet whose every pool fails with something other than capacity launched nothing,
    // so it is a definite failure of that class
    let ec2 = FakeEc2::default();
    ec2.0.borrow_mut().pool_errors = BTreeMap::from([
        (
            "c7a.2xlarge@us-east-1a".to_owned(),
            "InvalidParameterValue".to_owned(),
        ),
        (
            "c7a.2xlarge@us-east-1b".to_owned(),
            "InvalidParameterValue".to_owned(),
        ),
    ]);
    let aws = Aws::new(ec2, scheme());
    let error = block_on(aws.launch(&owner("prj_1", 1), &spec(&["c7a.2xlarge"]))).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::Validation);
    assert!(!error.retryable);
    assert!(
        error
            .message()
            .starts_with("EC2 CreateFleet launched nothing: c7a.2xlarge in")
    );

    // an API error, such as a missing permission, passes through as it is
    let ec2 = FakeEc2::default();
    ec2.0.borrow_mut().fleet_failure = Some(ProviderError::new(
        ProviderErrorCode::PermissionDenied,
        false,
        "AWS EC2 CreateFleet request failed with error code UnauthorizedOperation",
    ));
    let aws = Aws::new(ec2, scheme());
    let error = block_on(aws.launch(&owner("prj_1", 1), &spec(&["c7a.2xlarge"]))).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::PermissionDenied);
    assert!(aws.ec2().0.borrow().instances.is_empty());
}

#[test]
fn deleting_a_fleet_instance_accepts_its_untagged_one_time_spot_request() {
    let ec2 = FakeEc2::default();
    let aws = Aws::new(ec2, scheme());
    let owner = owner("prj_1", 1);
    let id = block_on(aws.create(&owner, &spec(&["c7a.2xlarge"]))).unwrap();
    let instance = aws.ec2().0.borrow().instances[id.as_str()].clone();
    let request = block_on(aws.spot_request(&instance, &owner.with_kind(AwsKind::Instance)))
        .unwrap()
        .unwrap();
    assert!(request.tags.is_empty());

    block_on(aws.delete_owned(&owner, &id)).unwrap();

    // a one-time request closes with its instance, so only the terminate is sent
    assert_eq!(
        aws.ec2().calls().last(),
        Some(&Call::Terminate(id.as_str().to_owned()))
    );
    assert!(
        !aws.ec2()
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Cancel(_)))
    );

    // a request that carries another owner's tags is still refused
    let other = id.as_str().to_owned();
    aws.ec2()
        .0
        .borrow_mut()
        .spot_requests
        .get_mut(&format!("sir-{other}"))
        .unwrap()
        .tags = pairs(&self::owner("prj_2", 1));
    let error = block_on(aws.delete_owned(&owner, &id)).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::InvariantViolation);
}

#[test]
fn interruptions_name_the_pools_that_took_an_owners_instances_back() {
    let ec2 = FakeEc2::default();
    let aws = Aws::new(ec2, scheme());
    let mut ids = Vec::new();
    for (generation, instance_type) in [(1, "c7a.2xlarge"), (2, "c7i.2xlarge"), (3, "m7a.2xlarge")]
    {
        ids.push(
            block_on(aws.create(&owner("prj_1", generation), &spec(&[instance_type]))).unwrap(),
        );
    }
    let other_owner = block_on(aws.create(&owner("prj_2", 1), &spec(&["c7a.2xlarge"]))).unwrap();
    aws.ec2()
        .retire(ids[0].as_str(), SPOT_INSTANCE_TERMINATION, Some(1_000));
    aws.ec2()
        .retire(ids[1].as_str(), "Client.UserInitiatedShutdown", None);
    aws.ec2()
        .retire(other_owner.as_str(), SPOT_INSTANCE_TERMINATION, None);
    let regions = AwsRegions::new([(AwsRegion::parse("us-east-1").unwrap(), &aws)]).unwrap();

    let interruptions = block_on(regions.spot_interruptions(
        &AwsRegion::parse("us-east-1").unwrap(),
        &OwnerId::parse("prj_1").unwrap(),
    ))
    .unwrap();

    // every generation of the owner counts, a user's own termination does not
    assert_eq!(interruptions.len(), 1);
    assert_eq!(interruptions[0].instance_type.as_str(), "c7a.2xlarge");
    assert_eq!(interruptions[0].availability_zone.as_str(), "us-east-1a");
    assert_eq!(interruptions[0].at, UnixMillis::new(1_000));
    assert!(
        block_on(regions.spot_interruptions(
            &AwsRegion::parse("us-west-2").unwrap(),
            &OwnerId::parse("prj_1").unwrap(),
        ))
        .is_err()
    );
}

#[test]
fn inventory_classifies_and_never_deletes_an_old_scheme_instance() {
    let ec2 = FakeEc2::default();
    let current = owner("prj_1", 2);
    ec2.add_instance(
        "i-current",
        AwsInstanceState::Stopped,
        pairs(&current),
        true,
    );
    ec2.add_instance("i-legacy", AwsInstanceState::Running, legacy_tags(), true);
    ec2.add_instance(
        "i-gone",
        AwsInstanceState::Terminated,
        pairs(&current),
        false,
    );
    ec2.add_instance(
        "i-foreign",
        AwsInstanceState::Running,
        vec![tag("team", "x")],
        false,
    );
    let mut extra = pairs(&current);
    extra.push(tag("rb-lane", "lane_1"));
    ec2.add_instance("i-extra", AwsInstanceState::Running, extra, false);
    let aws = Aws::new(ec2, scheme());

    let inventory = block_on(aws.inventory()).unwrap();

    assert_eq!(inventory.managed.len(), 1);
    assert_eq!(inventory.managed[0].id, id("i-current"));
    assert_eq!(
        inventory.managed[0].tags,
        current.with_kind(AwsKind::Instance)
    );
    let unrecognized: BTreeMap<_, _> = inventory
        .unrecognized
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    assert_eq!(unrecognized.len(), 2);
    let legacy = unrecognized["i-legacy"];
    assert_eq!(legacy.reason, UnrecognizedReason::InvalidValue);
    assert_eq!(legacy.provider_kind, "instance");
    assert_eq!(legacy.name, "rb-builder-aws-lane_1-1");
    assert!(legacy.tags.contains(&"rb-managed=rembuild".to_owned()));
    assert_eq!(unrecognized["i-extra"].reason, UnrecognizedReason::ExtraTag);

    // a delete by ID still refuses the old-scheme instance and its Spot request
    let error = block_on(aws.delete(AwsKind::Instance, &id("i-legacy"))).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::InvariantViolation);
    assert!(aws.ec2().calls().is_empty());
}

#[test]
fn delete_cancels_the_spot_request_before_terminating() {
    let ec2 = FakeEc2::default();
    let current = owner("prj_1", 1);
    ec2.add_instance("i-1", AwsInstanceState::Running, pairs(&current), true);
    let aws = Aws::new(ec2, scheme());

    block_on(aws.delete(AwsKind::Instance, &id("i-1"))).unwrap();

    assert_eq!(
        aws.ec2().calls(),
        [
            Call::Cancel("sir-i-1".to_owned()),
            Call::Terminate("i-1".to_owned())
        ]
    );
    assert_eq!(
        block_on(aws.presence(AwsKind::Instance, &id("i-1"))).unwrap(),
        Presence::Absent
    );

    // deleting again cancels nothing: the request is cancelled and the instance terminated
    block_on(aws.delete(AwsKind::Instance, &id("i-1"))).unwrap();
    assert_eq!(aws.ec2().calls().len(), 2);

    // an instance that never existed is already deleted
    block_on(aws.delete(AwsKind::Instance, &id("i-missing"))).unwrap();
}

#[test]
fn delete_owned_refuses_another_generation() {
    let ec2 = FakeEc2::default();
    ec2.add_instance(
        "i-1",
        AwsInstanceState::Running,
        pairs(&owner("prj_1", 2)),
        true,
    );
    let aws = Aws::new(ec2, scheme());

    let error = block_on(aws.delete_owned(&owner("prj_1", 1), &id("i-1"))).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::InvariantViolation);
    assert!(aws.ec2().calls().is_empty());

    block_on(aws.delete_owned(&owner("prj_1", 2), &id("i-1"))).unwrap();
    assert_eq!(aws.ec2().calls().len(), 2);
}

#[test]
fn presence_reads_terminated_and_missing_as_absent_and_keeps_errors() {
    let ec2 = FakeEc2::default();
    let current = owner("prj_1", 1);
    ec2.add_instance(
        "i-stopping",
        AwsInstanceState::ShuttingDown,
        pairs(&current),
        false,
    );
    ec2.add_instance(
        "i-stopped",
        AwsInstanceState::Stopped,
        pairs(&current),
        false,
    );
    ec2.add_instance(
        "i-gone",
        AwsInstanceState::Terminated,
        pairs(&current),
        false,
    );
    let aws = Aws::new(ec2, scheme());
    let presence = |value: &str| block_on(aws.presence(AwsKind::Instance, &id(value)));

    assert_eq!(presence("i-stopping").unwrap(), Presence::Present);
    assert_eq!(presence("i-stopped").unwrap(), Presence::Present);
    assert_eq!(presence("i-gone").unwrap(), Presence::Absent);
    assert_eq!(presence("i-missing").unwrap(), Presence::Absent);

    aws.ec2()
        .0
        .borrow_mut()
        .describe_failures
        .push_back(ProviderError::new(
            ProviderErrorCode::ServerError,
            true,
            "down",
        ));
    assert_eq!(
        presence("i-stopped").unwrap_err().code,
        ProviderErrorCode::ServerError
    );
}

#[test]
fn find_returns_only_the_live_exact_generation() {
    let ec2 = FakeEc2::default();
    let wanted = owner("prj_1", 2);
    ec2.add_instance("i-wanted", AwsInstanceState::Running, pairs(&wanted), false);
    ec2.add_instance(
        "i-older",
        AwsInstanceState::Running,
        pairs(&owner("prj_1", 1)),
        false,
    );
    ec2.add_instance(
        "i-other",
        AwsInstanceState::Running,
        pairs(&owner("prj_2", 2)),
        false,
    );
    ec2.add_instance(
        "i-gone",
        AwsInstanceState::Terminated,
        pairs(&wanted),
        false,
    );
    let mut extra = pairs(&wanted);
    extra.push(tag("rb-lane", "lane_1"));
    ec2.add_instance("i-extra", AwsInstanceState::Running, extra, false);
    let aws = Aws::new(ec2, scheme());

    let found = block_on(aws.find(&wanted.owner, wanted.generation, AwsKind::Instance)).unwrap();

    assert_eq!(found, [id("i-wanted")]);
}

#[test]
fn a_region_set_merges_and_routes_without_reading_errors_as_absence() {
    let east = Aws::new(FakeEc2::default(), scheme());
    let west = Aws::new(FakeEc2::default(), scheme());
    let current = owner("prj_1", 1);
    east.ec2()
        .add_instance("i-east", AwsInstanceState::Running, pairs(&current), true);
    west.ec2()
        .add_instance("i-west", AwsInstanceState::Running, pairs(&current), true);
    west.ec2()
        .add_instance("i-legacy", AwsInstanceState::Running, legacy_tags(), false);
    let east_region = AwsRegion::parse("us-east-1").unwrap();
    let west_region = AwsRegion::parse("us-west-2").unwrap();
    let regions =
        AwsRegions::new([(east_region.clone(), &east), (west_region.clone(), &west)]).unwrap();

    let inventory = block_on(regions.inventory()).unwrap();
    let mut managed: Vec<_> = inventory
        .managed
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    managed.sort();
    assert_eq!(managed, [id("i-east"), id("i-west")]);
    assert_eq!(inventory.unrecognized.len(), 1);
    let mut found =
        block_on(regions.find(&current.owner, current.generation, AwsKind::Instance)).unwrap();
    found.sort();
    assert_eq!(found, [id("i-east"), id("i-west")]);

    // delete reaches the region that holds the instance
    block_on(regions.delete(AwsKind::Instance, &id("i-west"))).unwrap();
    assert_eq!(
        west.ec2().calls(),
        [
            Call::Cancel("sir-i-west".to_owned()),
            Call::Terminate("i-west".to_owned())
        ]
    );
    assert!(east.ec2().calls().is_empty());
    assert_eq!(
        block_on(regions.presence(AwsKind::Instance, &id("i-west"))).unwrap(),
        Presence::Absent
    );
    assert_eq!(
        block_on(regions.presence(AwsKind::Instance, &id("i-east"))).unwrap(),
        Presence::Present
    );

    // a region that fails is never read as absence
    west.ec2()
        .0
        .borrow_mut()
        .describe_failures
        .push_back(ProviderError::new(ProviderErrorCode::Network, true, "down"));
    assert_eq!(
        block_on(regions.presence(AwsKind::Instance, &id("i-missing")))
            .unwrap_err()
            .code,
        ProviderErrorCode::Network
    );

    // create goes to the named region only
    let created = block_on(regions.create(
        &owner("prj_2", 1),
        &RegionalSpec {
            region: east_region,
            instance: spec(&["c7a.2xlarge"]),
        },
    ))
    .unwrap();
    assert!(
        east.ec2()
            .0
            .borrow()
            .instances
            .contains_key(created.as_str())
    );
    assert!(AwsRegions::new([(west_region.clone(), &east), (west_region, &west)]).is_err());
}

fn shared_group_tags() -> Vec<AwsTag> {
    vec![tag("ManagedBy", "Terraform"), tag("rb-scope", "shared")]
}

fn ingress_fake() -> FakeEc2 {
    let ec2 = FakeEc2::default();
    ec2.0.borrow_mut().group = Some(AwsSecurityGroup {
        id: GROUP.to_owned(),
        name: "shared".to_owned(),
        vpc_id: Some(VPC.to_owned()),
        ingress: Vec::new(),
        tags: shared_group_tags(),
    });
    ec2
}

fn ingress(ec2: &FakeEc2) -> AwsIngress<'_, FakeEc2> {
    AwsIngress::new(
        ec2,
        &scheme(),
        SharedSecurityGroup {
            id: SecurityGroupId::parse(GROUP).unwrap(),
            vpc_id: VPC.to_owned(),
            tags: shared_group_tags(),
        },
        NonZeroU16::new(22).unwrap(),
    )
}

fn rule(cidr: &str, description: &str) -> AwsSecurityGroupIngress {
    TcpIngress {
        port: 22,
        source: SourceCidr::parse(cidr).unwrap(),
        description: description.to_owned(),
    }
    .to_ingress()
}

#[test]
fn opening_replaces_the_generation_source_and_closing_keeps_other_generations() {
    let ec2 = ingress_fake();
    let policy = ingress(&ec2);
    let first = owner("prj_1", 1);
    let other = owner("prj_2", 1);

    assert_eq!(
        block_on(policy.ensure_closed(&first)).unwrap().as_str(),
        GROUP
    );
    assert!(ec2.ingress().is_empty());

    let source = SourceCidr::parse("198.51.100.10/32").unwrap();
    assert_eq!(
        block_on(policy.open(&first, &source)).unwrap().as_str(),
        GROUP
    );
    block_on(policy.open(&first, &source)).unwrap();
    block_on(policy.open(&other, &SourceCidr::parse("198.51.100.11/32").unwrap())).unwrap();
    assert_eq!(
        ec2.ingress(),
        [
            rule("198.51.100.10/32", "rb-ssh:prj_1:1"),
            rule("198.51.100.11/32", "rb-ssh:prj_2:1"),
        ]
    );
    let authorizations = ec2
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::Authorize(..)))
        .count();
    assert_eq!(authorizations, 2);

    // a new source for the same generation replaces the old one
    block_on(policy.open(&first, &SourceCidr::parse("203.0.113.5/32").unwrap())).unwrap();
    assert_eq!(
        ec2.ingress(),
        [
            rule("198.51.100.11/32", "rb-ssh:prj_2:1"),
            rule("203.0.113.5/32", "rb-ssh:prj_1:1"),
        ]
    );

    block_on(policy.close(&first)).unwrap();
    assert_eq!(ec2.ingress(), [rule("198.51.100.11/32", "rb-ssh:prj_2:1")]);

    // a prewarmed generation that is closed again loses any stale rule
    block_on(policy.ensure_closed(&other)).unwrap();
    assert!(ec2.ingress().is_empty());
}

#[test]
fn a_later_generation_adopts_the_same_source() {
    let ec2 = ingress_fake();
    ec2.set_ingress(vec![rule("198.51.100.10/32", "rb-ssh:prj_1:1")]);
    let policy = ingress(&ec2);

    block_on(policy.open(
        &owner("prj_1", 2),
        &SourceCidr::parse("198.51.100.10/32").unwrap(),
    ))
    .unwrap();

    assert_eq!(ec2.ingress(), [rule("198.51.100.10/32", "rb-ssh:prj_1:2")]);
    assert_eq!(
        ec2.calls(),
        [
            Call::Revoke("198.51.100.10/32".to_owned(), "rb-ssh:prj_1:1".to_owned()),
            Call::Authorize("198.51.100.10/32".to_owned(), "rb-ssh:prj_1:2".to_owned()),
        ]
    );
}

#[test]
fn a_rule_added_between_the_read_and_the_add_is_adopted() {
    let ec2 = ingress_fake();
    ec2.0.borrow_mut().racing_rule = Some(rule("198.51.100.10/32", "rb-ssh:prj_9:4"));
    let policy = ingress(&ec2);

    block_on(policy.open(
        &owner("prj_1", 1),
        &SourceCidr::parse("198.51.100.10/32").unwrap(),
    ))
    .unwrap();

    assert_eq!(ec2.ingress(), [rule("198.51.100.10/32", "rb-ssh:prj_1:1")]);
    assert_eq!(
        ec2.calls(),
        [
            Call::Authorize("198.51.100.10/32".to_owned(), "rb-ssh:prj_1:1".to_owned()),
            Call::Revoke("198.51.100.10/32".to_owned(), "rb-ssh:prj_9:4".to_owned()),
            Call::Authorize("198.51.100.10/32".to_owned(), "rb-ssh:prj_1:1".to_owned()),
        ]
    );
}

#[test]
fn ingress_refuses_unowned_rules_and_the_wrong_group() {
    let ec2 = ingress_fake();
    ec2.set_ingress(vec![AwsSecurityGroupIngress {
        description: None,
        ..rule("198.51.100.12/32", "")
    }]);
    let policy = ingress(&ec2);
    let source = SourceCidr::parse("198.51.100.10/32").unwrap();

    let error = block_on(policy.open(&owner("prj_1", 1), &source)).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::InvariantViolation);
    assert!(ec2.calls().is_empty());

    // a rule from the old `rembuild-ssh:` scheme is not this scheme's either
    ec2.set_ingress(vec![rule("198.51.100.12/32", "rembuild-ssh:aws:lane_1:1")]);
    let error = block_on(policy.open(&owner("prj_1", 1), &source)).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::InvariantViolation);

    ec2.set_ingress(Vec::new());
    ec2.0.borrow_mut().group.as_mut().unwrap().vpc_id = Some("vpc-89abcdef".to_owned());
    let error = block_on(policy.ensure_closed(&owner("prj_1", 1))).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::Conflict);

    ec2.0.borrow_mut().group.as_mut().unwrap().vpc_id = Some(VPC.to_owned());
    ec2.0
        .borrow_mut()
        .group
        .as_mut()
        .unwrap()
        .tags
        .push(tag("extra", "x"));
    let error = block_on(policy.ensure_closed(&owner("prj_1", 1))).unwrap_err();
    assert_eq!(error.code, ProviderErrorCode::InvariantViolation);

    // a group that no longer exists is closed
    ec2.0.borrow_mut().group.as_mut().unwrap().id = "sg-89abcdef".to_owned();
    block_on(policy.close(&owner("prj_1", 1))).unwrap();
}
