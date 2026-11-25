# AWS operator grants and service-user boundaries

This Terraform configuration lets the operator IAM user, `local-automation` by default (the Mac
mini's default AWS profile), manage two service users without the account's root credentials,
and without being able to turn either into an administrator:

- `user/service-accounts/cloudlease-machines`, the machines Worker's user (`infra/aws-machines`)
- `user/service-accounts/rembuild-worker`, rembuild's control-plane user (rembuild's
  `infra/aws-spot-iam`)

It creates one permissions boundary per service user and three managed policies attached to the
operator. Only an administrator can apply it.

## Permissions boundaries

A boundary is the most a user's policies can grant: whatever the operator writes into the user's
policies, the user can do only what the boundary also allows. Both boundaries live under
`policy/boundaries/` and allow EC2 only, in their own regions, on their own app's tags:

- **`boundaries/cloudlease-machines`:** `DescribeInstances` and `DescribeSpotInstanceRequests`
  in `machines_regions`; `RunInstances` of instances and Spot requests tagged `clm-managed=true`,
  with the images, snapshots, subnets, network interfaces, security groups and volumes a launch
  touches; `CreateTags` only as part of that launch; `TerminateInstances` and
  `CancelSpotInstanceRequests` only on `clm-managed=true` resources
- **`boundaries/rembuild-worker`:** the seven describes rembuild uses in `rembuild_regions`;
  ingress changes only on the security group tagged `rb-managed=rembuild` and `rb-kind=firewall`;
  `RunInstances` and `CreateTags` on launch with `rb-managed` set to `rembuild` (legacy shape) or
  `true` (current shape); start, stop, terminate, user-data changes and Spot-request
  cancellation only on resources with one of those `rb-managed` values

The users' own policies keep their finer limits (pinned AMI, subnets, instance types, root
volume size and exact tag sets). The boundaries leave those out on purpose, so moving to a new
AMI or subnet stays an operator change and does not need an administrator.

Neither boundary allows IAM, STS, S3 or any service besides EC2, another region, or the other
app's instances. Changing what a service user may do beyond that means changing its boundary
here, with administrator credentials.

When rembuild drops its legacy tag shape, remove `rembuild` from `rembuild_managed_values` in
`boundaries.tf`. A new region goes into `machines_regions` or `rembuild_regions` before the
service user's own configuration uses it, or the user's calls there fail with
`UnauthorizedOperation`.

## The operator's grants

Three managed policies under `policy/operators/`, attached to the operator:

- **`cloudlease-iam-operator`** and **`rembuild-iam-operator`**, one per service user, on that
  user and its policies (`policy/service-accounts/<user>-*`) only:
  - create the user, or set its permissions boundary, only to its own boundary
    (`iam:PermissionsBoundary` condition)
  - attach a policy only while the user has its boundary, and only one of the user's own
    policies (`iam:PolicyARN` condition), so attaching `AdministratorAccess` or the other app's
    policy is refused
  - delete the user, detach its policies, tag, untag and update it
  - create, version, tag and delete the user's own policies
  - create, list, deactivate and delete the user's access keys
- **`iam-operator-guardrails`:** account-wide IAM reads (`Get*`, `List*`) and the policy
  simulator, plus explicit denies, so a grant added later by mistake still cannot lift them:
  no deleting any permissions boundary; no new versions, deletion or tagging of policies under
  `policy/boundaries/` or `policy/operators/`; nothing but reads on the operator user itself

The grants hold no `iam:PutUserPolicy`, so the operator cannot give either user an inline
policy. It can still write a broad statement into one of a user's own managed policies, and
that user's boundary caps it.

The grants are managed policies because AWS limits a user's inline policies to 2,048
characters in total. The operator also holds AWS's `PowerUserAccess`, which this configuration
does not manage.

## Why a configuration of its own

The grants' principal is the operator, not the service users, and only an administrator can
apply them. Inside `infra/aws-machines` they would make every change to it, and a destroy,
need administrator credentials. Kept here, `infra/aws-machines` and rembuild's
`infra/aws-spot-iam` stay operable by the operator.

Both of those configurations set `permissions_boundary` on their user from the `boundary_arns`
output here, as `permissions_boundary_arn`. A plan of either one without it would remove the
boundary, which the operator may not do, so the apply fails with `AccessDenied` instead.

## Applying

Apply with an administrator profile (on the Mac mini, `infraops-admin`, behind `aws login`),
and only this configuration:

```sh
cd infra/aws-operator
cp terraform.tfvars.example terraform.tfvars   # set aws_account_id and the regions
AWS_PROFILE=infraops-admin terraform init
AWS_PROFILE=infraops-admin terraform plan -out operator.plan
AWS_PROFILE=infraops-admin terraform apply operator.plan
terraform output -json boundary_arns
```

Then check, as the operator, that `infra/aws-machines` and rembuild's `infra/aws-spot-iam` plan
with no changes and no access errors.

To check a grant or boundary change before applying it, run `aws iam simulate-custom-policy`
with the planned documents (`terraform show -json` of the plan). The operator may run the
simulator itself.
