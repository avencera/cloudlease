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

Account-wide prerequisites this does not manage:

- the Spot service-linked role `AWSServiceRoleForEC2Spot`, which the account already has
  because rembuild runs Spot builders
- the "All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance Requests" vCPU quota in the
  region, which must cover the largest pool: 16 machines of 16 vCPUs need 256

`terraform validate` passed on this configuration; it has not been planned or applied against
the account.
