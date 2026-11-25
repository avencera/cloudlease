# AWS for the cloudlease machines Worker

This Terraform configuration creates the network and the IAM user the machines Worker
(`apps/machines`) launches EC2 Spot instances with, in one region. It does not create an access
key: an `aws_iam_access_key` resource would keep the secret in Terraform state. Create the key
with the AWS CLI and put it straight into the Worker's secrets (the deploy runbook in
`private-workbench/ops/cloudlease/machines-deploy.md` has the commands).

The Worker never uses rembuild's IAM user, and rembuild never uses this one. The two policies
match disjoint tags (`clm-managed=true` here, `rb-managed` there), so neither key can launch,
terminate or cancel the other app's instances.

## What it creates

- one VPC, `10.66.0.0/16` by default
- one public `/24` subnet in each of `availability_zones`, with automatic public IPv4
- one internet gateway and a public IPv4 route
- one security group, `cloudlease-machines`, with all egress and no ingress: machines reach
  their coordinator outbound over iroh, so nothing connects in
- one launch template, `cloudlease-machines`: the security group (a fleet cannot override it),
  no network interface block (so the subnets' automatic public IPv4 applies), IMDSv2 required,
  the pinned AMI and terminate on shutdown. Each launch overrides the type, subnet, zone, AMI,
  root volume, IMDSv2 and max price and brings its own user data, so one version serves every
  machine
- one IAM user, `cloudlease-machines`, with four managed policies, named with the region

The `class_fields` output holds the region, AMI, root device, launch template ID and version,
and zones with their subnet IDs that an AWS class in `MACHINES_CONFIG` names. A class pins the
template version as a number, so a template change takes effect only once the classes name its
new `latest_version`.

**Applied 2026-11-25** with the default profile, after `infra/aws-operator` widened the boundary:
the launch template (`lt-010f6147c2e816163`, version 1), the `fleet` policy and its attachment,
and the launch policy's template grant. The AWS classes of `MACHINES_CONFIG` name that template
and version.

## What the policies allow

- **inventory:** `DescribeInstances` and `DescribeSpotInstanceRequests` in the region. The
  sweep lists by the `clm-managed` tag key; describes cannot be scoped by tag. Before each
  launch the Worker also reads which of a run's live Spot requests are
  `marked-for-termination`, to leave out the pools taking a machine back.
- **fleet:** `CreateFleet`, which the Worker launches with
  - from this configuration's launch template, pinned AMI and subnets, in the region
  - with the exact managed tag keys, `clm-managed=true` and `clm-kind=instance` on the
    instance, its root volume and the fleet, and `clm-owner`, `clm-gen`, `clm-until` and
    `Name` on the instance; `CreateTags` of the fleet, instance and volume only as part of
    that `CreateFleet`
- **launch:** `RunInstances` only, kept because whether `CreateFleet` also authorizes
  `RunInstances` against the caller is unconfirmed
  - of the pinned AMI (and its root snapshot) and the launch template (EC2's launch template
    permissions guide asks for `RunInstances` on a template to launch from it), into this
    configuration's subnets, VPC and security group, with a public IPv4 address
  - in the listed zones, of the allowed instance-type patterns, on the Spot market, with
    IMDSv2 required and default tenancy
  - with a root volume no larger than `max_root_volume_gib`
  - with the exact managed tag keys (`Name`, `clm-managed`, `clm-owner`, `clm-gen`,
    `clm-kind`, `clm-until`), `clm-managed=true` and `clm-kind=instance` on the instance and
    its Spot request; `CreateTags` only as part of that launch
- **cleanup:** `TerminateInstances` and `CancelSpotInstanceRequests` only on resources tagged
  `clm-managed=true` and `clm-kind=instance`

IAM has no condition keys for the Spot request type, interruption behavior or max price. The
adapter always sends a one-time request that terminates on interruption, with the class's max
price, and the Worker's tests check the request it sends.

### What EC2 checks on `CreateFleet`

The Service Authorization Reference lists more `CreateFleet` condition keys than EC2 sends.
On 2026-11-25 the Worker's own user sent the adapter's request with `DryRun`, while the fleet
policy held temporary statements that denied the call if a given key was absent. EC2 checked:

| Resource | Keys present | Keys absent |
| --- | --- | --- |
| `instance/*` | `ec2:Region`, `ec2:InstanceID` (`*`), `aws:RequestTag/*`, `aws:TagKeys` | `ec2:AvailabilityZone`, `ec2:InstanceType`, `ec2:Tenancy`, `ec2:RootDeviceType`, `ec2:EbsOptimized`, `ec2:PlacementGroup` |
| `fleet/*` | `ec2:Region`, `aws:RequestTag/*`, `aws:TagKeys` | `ec2:FleetID` |
| `volume/*` | `ec2:Region`, `ec2:VolumeID`, `aws:RequestTag/*`, `aws:TagKeys` | `ec2:VolumeSize`, `ec2:VolumeType`, `ec2:Encrypted`, `ec2:AvailabilityZone`, `ec2:ParentSnapshot`, `ec2:KmsKeyId` |
| each subnet | `ec2:Region`, `ec2:Vpc`, `ec2:AvailabilityZone`, `ec2:SubnetID` | |
| the image | `ec2:Region`, `ec2:ImageID`, `ec2:Owner`, `ec2:Public`, `ec2:RootDeviceType` | |
| the launch template | `ec2:Region`, `ec2:ResourceTag/*` | |

The security group, network interface, snapshot and Spot request are not checked at all. The
first fleet policy required `ec2:AvailabilityZone` and `ec2:InstanceType` on the instance, so
no request could match it and every launch failed with `UnauthorizedOperation`. A failure's
encoded message names only the first resource refused, and its decoded context leaves out
`ec2:Region` and the request tags even when EC2 sends them, so read it with that in mind; the
Worker logs it (never in the ntfy alert) for `aws sts decode-authorization-message`.

So the fleet policy pins what EC2 checks and cannot express the rest. Each guarantee rests on:

- **Zone:** the subnets. The policy names only this configuration's subnets, one per listed
  zone, and the template's security group belongs to this VPC, so a launch cannot leave it.
- **Instance type:** the adapter, which sends only the class's types from `MACHINES_CONFIG`,
  and the Worker's spend caps. IAM cannot cap the type of a fleet's instance.
- **Spot only:** the adapter, which sends `DefaultTargetCapacityType=spot` with a total
  capacity of 1 and no On-Demand capacity; the launch template sets no market options. A
  caller with this key could launch an On-Demand instance through a fleet.
- **IMDSv2:** the adapter, whose every override sends `HttpTokens=required`. The launch
  template requires IMDSv2 too, but a fleet's override can change that, so for a caller with
  this key it is only a default.
- **Default tenancy:** the adapter, which never sets one; the template names none. A fleet's
  override can set a tenancy.
- **Root volume size:** the class's `root_volume_gib`. `max_root_volume_gib` caps only
  `RunInstances`.
- **Public IPv4 and the security group:** the template, which has no network interface block
  and names the outbound-only group; a fleet cannot override the group, and the subnets assign
  a public IPv4 on launch.
- **Tags:** IAM refuses a wrong or missing managed tag on the fleet, and a wrong one on the
  instance or volume. A request that leaves out the instance's or the volume's tag
  specification entirely is not refused (as with rembuild's Spot request below), so that the
  instance carries its tags rests on the adapter, which always sends all three. An untagged
  instance would be invisible to the sweep, and the cleanup policy would refuse to terminate
  it; only an operator could find and end it.

An instant fleet needs no `AWSServiceRoleForEC2Fleet` (EC2 Fleet prerequisites guide), and the
account has none.

A fleet cannot tag the Spot request it opens. Whether that request carries any tags is
unconfirmed, so the adapter accepts an untagged one-time request as its instance's own and
never cancels it (the cleanup policy would refuse a cancel of an untagged request): it closes
with its instance. `CreateFleet` keeps a `ClientToken` for an unconfirmed time; the adapter
never replays a launch whose answer was lost, and finds its instance by tags instead.

An instant fleet has no fleet-count quota, and is deleted
by EC2 some time after its instance terminates, so nothing here or in the Worker cleans fleets
up.

As for rembuild's policy, AWS does not evaluate the Spot-request resource when a caller omits its
tag specification, so IAM cannot make an untagged Spot request impossible. The instance itself
must still pass every condition above, and a one-time request closes with its instance.

## AMI

The AMI is pinned, so the policy names one image. Read Canonical's current Ubuntu 24.04 image
from the SSM public parameter:

```sh
aws ssm get-parameter --region us-east-1 \
  --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
  --query Parameter.Value --output text
```

To move to a newer image, set `ami_id`, apply, and put the same ID in the AWS classes of
`MACHINES_CONFIG` before deploying the Worker. A launch of an image the policy does not name is
refused with `UnauthorizedOperation`, which ends the create as `create_failed`.

## Apply

```sh
cp terraform.tfvars.example terraform.tfvars   # fill in the account, zones and AMI
terraform init
terraform plan
terraform apply
terraform output -json class_fields
```

Apply it once per account, for one region. The IAM user is account-wide, so a second state for
another region fails on the existing user, and a second user would reach only its own region
while the Worker signs every region in `AWS_REGIONS` with one key. A second region needs this
configuration extended (its network, and that region in the policies' `ec2:Region`
conditions) rather than applied again; until then `AWS_REGIONS` lists only this region.

Account-wide prerequisites this does not manage:

- the Spot service-linked role `AWSServiceRoleForEC2Spot`, which the account already has
  because rembuild runs Spot builders
- the "All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance Requests" vCPU quota in the
  region (quota code `L-34B43A08`), which must cover the largest pool: 16 machines of 16 vCPUs
  need 256, which was us-east-1's quota on 2026-11-24, with no headroom

The apply needs an identity that may create IAM users, policies and access keys under
`/service-accounts/`. The first apply, on 2026-11-24, made the IAM half and the access key with
the account's admin profile, because the Mac mini's default `local-automation` user could then
manage only rembuild's IAM user. `infra/aws-operator` now grants `local-automation` exactly the
IAM writes this configuration and the access key need, so it applies the whole configuration
and rotates the key on its own.

The user carries the permissions boundary `policy/boundaries/cloudlease-machines` from
`infra/aws-operator`, passed in as `permissions_boundary_arn`. It caps the user at the EC2
calls above, in this region, on `clm-managed=true` resources, whatever its policies say. The
operator may change the user and attach policies only while that boundary is attached, and may
not remove it, so a plan without `permissions_boundary_arn` fails rather than dropping it. A
policy change that needs more than the boundary allows (another service, another region) needs
the boundary widened in `infra/aws-operator` first, with administrator credentials.

Applied on 2026-11-24 in us-east-1, with zones `us-east-1f`, `us-east-1b` and `us-east-1d` and
AMI `ami-0a951420c495183dc`: 18 resources. The state and `terraform.tfvars` live on the Mac mini
in `~/code/cloudlease/infra/aws-machines`, and git ignores both. The state holds no access key.
