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
- one IAM user, `cloudlease-machines`, with three managed policies, named with the region

The `class_fields` output holds the region, AMI, root device, security group and zones with
their subnet IDs that an AWS class in `MACHINES_CONFIG` names.

## What the policies allow

- **inventory:** `DescribeInstances` and `DescribeSpotInstanceRequests` in the region. The
  sweep lists by the `clm-managed` tag key; describes cannot be scoped by tag.
- **launch:** `RunInstances` only
  - of the pinned AMI (and its root snapshot), into this configuration's subnets, VPC and
    security group, with a public IPv4 address
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
