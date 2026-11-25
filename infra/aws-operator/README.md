# AWS operator grant for the cloudlease machines Worker

This Terraform configuration gives the operator IAM user, `local-automation` by default (the
Mac mini's default AWS profile), an inline policy, `cloudlease-iam-operator`, that lets it apply
the IAM half of `infra/aws-machines` and rotate the machines Worker's access key, so neither
needs the account's root credentials.

## What the grant allows

All three statements name only the Worker's user, `user/service-accounts/cloudlease-machines`,
and its managed policies, `policy/service-accounts/cloudlease-machines-*`:

- **read:** `iam:Get*` and `iam:List*` on those names
- **manage:** create, update, tag, untag and delete the user and those policies, create and
  delete policy versions, and attach or detach policies on the user. These are the same writes
  the operator holds for rembuild's `rembuild-worker` user (its `rembuild-iam-operator` inline
  policy, which no Terraform here manages)
- **access key:** `iam:CreateAccessKey`, `iam:DeleteAccessKey` and `iam:ListAccessKeys` on the
  user only. The key stays out of Terraform state; the deploy runbook in
  `private-workbench/ops/cloudlease/machines-deploy.md` creates it with the CLI

The operator cannot change this grant: it holds no `iam:PutUserPolicy` on itself. Because it
may edit the Worker's policies, attach them and create the Worker's access key, the grant is
as strong as whatever those policies could be made to allow; treat the operator's credentials
accordingly.

## Why a configuration of its own

The grant's principal is the operator, not the Worker, and only an administrator can apply it.
Inside `infra/aws-machines` it would make any change to it, and a destroy of that configuration,
need administrator credentials. Kept here, `infra/aws-machines` stays fully operable by the
operator.

## Applying

Apply with an administrator profile (on the Mac mini, `infraops-admin`, behind `aws login`),
and only this configuration:

```sh
cd infra/aws-operator
cp terraform.tfvars.example terraform.tfvars   # set aws_account_id
AWS_PROFILE=infraops-admin terraform init
AWS_PROFILE=infraops-admin terraform plan -out operator.plan
AWS_PROFILE=infraops-admin terraform apply operator.plan
```

Then check that the operator can plan `infra/aws-machines` with no changes and no access
errors.

AWS limits the inline policies on one user to 2,048 characters in total, whitespace excluded.
On 2026-11-24 this grant used 954 and `rembuild-iam-operator` 598, leaving about 500.
