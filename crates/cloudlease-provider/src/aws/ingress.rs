//! Inbound access through one shared, externally owned security group
//!
//! Terraform (or another owner) creates the group, its egress and its tags; this adapter only
//! adds and removes TCP rules on one port. Each generation owns at most one rule, named by
//! its description `{prefix}-ssh:{owner}:{generation}`, so concurrent generations share the
//! group without touching each other's access.

use std::{collections::HashSet, num::NonZeroU16};

use cloudlease_core::tags::{OwnerTags, TagScheme};

use super::ec2::{
    AwsSecurityGroup, AwsSecurityGroupIngress, AwsTag, Ec2, SecurityGroupId, TcpIngress,
    is_duplicate_ingress,
};
use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    firewall::{IngressPolicy, SourceCidr},
};

/// The shared security group and what it must look like before any rule is changed
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SharedSecurityGroup {
    /// The group ID
    pub id: SecurityGroupId,
    /// The VPC the group must be in, normally the launch subnet's
    pub vpc_id: String,
    /// The exact tag set its owner gives the group
    pub tags: Vec<AwsTag>,
}

/// [`IngressPolicy`] over one shared security group
pub struct AwsIngress<'a, C> {
    ec2: &'a C,
    group: SharedSecurityGroup,
    port: NonZeroU16,
    description_prefix: String,
}

impl<'a, C: Ec2> AwsIngress<'a, C> {
    /// Manages TCP `port` rules on `group`, described with `scheme`'s prefix
    #[must_use]
    pub fn new(
        ec2: &'a C,
        scheme: &TagScheme,
        group: SharedSecurityGroup,
        port: NonZeroU16,
    ) -> Self {
        Self {
            ec2,
            group,
            port,
            description_prefix: format!("{}-ssh:", scheme.prefix()),
        }
    }

    /// Returns the description of the rule one generation owns
    #[must_use]
    pub fn rule_description(&self, owner: &OwnerTags) -> String {
        format!(
            "{}{}:{}",
            self.description_prefix, owner.owner, owner.generation
        )
    }

    /// Describes the shared group and checks its ID, exact tags and VPC
    ///
    /// Callers that launch into the group check it with this before using it, without
    /// changing any rule.
    pub async fn verified_group(&self) -> ProviderResult<AwsSecurityGroup> {
        let group = self
            .ec2
            .describe_security_group(self.group.id.as_str().to_owned())
            .await?;
        self.verify(&group)?;

        Ok(group)
    }

    fn verify(&self, group: &AwsSecurityGroup) -> ProviderResult<()> {
        if group.id != self.group.id.as_str() {
            return Err(ProviderError::invalid_response(
                "AWS security group response does not match the configured shared ID",
            ));
        }

        let actual: HashSet<_> = group.tags.iter().collect();
        let expected: HashSet<_> = self.group.tags.iter().collect();
        if group.tags.len() != self.group.tags.len()
            || actual.len() != group.tags.len()
            || actual != expected
        {
            return Err(invariant_violation(
                "AWS shared security group does not have the exact owner tag set",
            ));
        }

        if group.vpc_id.as_deref() != Some(self.group.vpc_id.as_str()) {
            return Err(ProviderError::new(
                ProviderErrorCode::Conflict,
                false,
                "AWS shared security group is outside the configured subnet VPC",
            ));
        }

        Ok(())
    }

    fn rule(&self, source: SourceCidr, description: String) -> TcpIngress {
        TcpIngress {
            port: self.port.get(),
            source,
            description,
        }
    }

    /// Returns the owner description of a rule, refusing any rule this adapter did not add
    ///
    /// The group is shared and Terraform keeps no ingress, so an undescribed rule or one on
    /// another port means someone changed the group by hand; stop rather than guess.
    fn owner_of<'r>(&self, rule: &'r AwsSecurityGroupIngress) -> ProviderResult<&'r str> {
        let Some(owner) = rule.description.as_deref() else {
            return Err(invariant_violation(
                "AWS shared security group contains an ingress rule without an owner",
            ));
        };
        if !owner.starts_with(&self.description_prefix) {
            return Err(invariant_violation(
                "AWS shared security group contains unmanaged ingress",
            ));
        }
        if !rule.is_tcp_port(self.port.get()) {
            return Err(invariant_violation(
                "AWS shared security group contains managed ingress on another port",
            ));
        }

        Ok(owner)
    }

    async fn revoke_owned(
        &self,
        group: &AwsSecurityGroup,
        description: &str,
    ) -> ProviderResult<()> {
        let owned = group
            .ingress
            .iter()
            .filter(|rule| rule.description.as_deref() == Some(description));
        for rule in owned {
            if !rule.is_tcp_port(self.port.get()) {
                return Err(invariant_violation(
                    "AWS shared security group contains an owned rule on another port",
                ));
            }
            let source = parse_source(rule)?;
            self.revoke(self.rule(source, description.to_owned()))
                .await?;
        }

        Ok(())
    }

    async fn ensure_rule(
        &self,
        group: AwsSecurityGroup,
        desired: TcpIngress,
    ) -> ProviderResult<()> {
        let mut desired_exists = false;
        let mut same_source_owner = None;

        for rule in &group.ingress {
            let owner = self.owner_of(rule)?;
            let source = parse_source(rule)?;
            if owner == desired.description {
                if source == desired.source {
                    desired_exists = true;
                } else {
                    // the generation's previous source loses access
                    self.revoke(self.rule(source, owner.to_owned())).await?;
                }
                continue;
            }

            // EC2 keys a rule by protocol, port and source, so a rule with the same source
            // owned by another generation must go before this one can be added
            if source == desired.source {
                same_source_owner = Some(owner.to_owned());
            }
        }

        if let Some(owner) = same_source_owner {
            self.revoke(self.rule(desired.source.clone(), owner))
                .await?;
        }
        if desired_exists {
            return Ok(());
        }

        self.authorize(desired).await
    }

    async fn authorize(&self, desired: TcpIngress) -> ProviderResult<()> {
        let group_id = self.group.id.as_str().to_owned();
        match self
            .ec2
            .authorize_tcp_ingress(group_id, desired.clone())
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if is_duplicate_ingress(&error) => {
                self.adopt_duplicate(desired, error).await
            }
            Err(error) => Err(error),
        }
    }

    /// Replaces a rule that appeared with the same source between the read and the add
    async fn adopt_duplicate(
        &self,
        desired: TcpIngress,
        original: ProviderError,
    ) -> ProviderResult<()> {
        let group = self.verified_group().await?;
        if group.ingress.iter().any(|rule| rule.matches(&desired)) {
            return Ok(());
        }

        let Some(existing) = group.ingress.iter().find(|rule| rule.has_source(&desired)) else {
            return Err(original);
        };
        let Ok(owner) = self.owner_of(existing) else {
            return Err(original);
        };
        self.revoke(self.rule(desired.source.clone(), owner.to_owned()))
            .await?;

        let group_id = self.group.id.as_str().to_owned();
        let error = match self
            .ec2
            .authorize_tcp_ingress(group_id, desired.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) if is_duplicate_ingress(&error) => error,
            Err(error) => return Err(error),
        };

        // another writer won the race with the same source; the source has access either way
        let current = self.verified_group().await?;
        if current.ingress.iter().any(|rule| rule.has_source(&desired)) {
            Ok(())
        } else {
            Err(error)
        }
    }

    async fn revoke(&self, rule: TcpIngress) -> ProviderResult<()> {
        let group_id = self.group.id.as_str().to_owned();
        match self.ec2.revoke_tcp_ingress(group_id, rule).await {
            Ok(())
            | Err(ProviderError {
                code: ProviderErrorCode::NotFound,
                ..
            }) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl<C: Ec2> IngressPolicy for AwsIngress<'_, C> {
    type PolicyId = SecurityGroupId;

    async fn ensure_closed(&self, owner: &OwnerTags) -> ProviderResult<SecurityGroupId> {
        let group = self.verified_group().await?;
        self.revoke_owned(&group, &self.rule_description(owner))
            .await?;

        Ok(self.group.id.clone())
    }

    async fn open(
        &self,
        owner: &OwnerTags,
        source: &SourceCidr,
    ) -> ProviderResult<SecurityGroupId> {
        let group = self.verified_group().await?;
        let desired = self.rule(source.clone(), self.rule_description(owner));
        self.ensure_rule(group, desired).await?;

        Ok(self.group.id.clone())
    }

    async fn close(&self, owner: &OwnerTags) -> ProviderResult<()> {
        let group = match self
            .ec2
            .describe_security_group(self.group.id.as_str().to_owned())
            .await
        {
            Ok(group) => group,
            Err(error) if error.code == ProviderErrorCode::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        self.verify(&group)?;

        self.revoke_owned(&group, &self.rule_description(owner))
            .await
    }
}

fn parse_source(rule: &AwsSecurityGroupIngress) -> ProviderResult<SourceCidr> {
    SourceCidr::parse(rule.cidr.clone())
        .map_err(|_| invariant_violation("AWS shared security group contains an invalid CIDR"))
}

fn invariant_violation(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorCode::InvariantViolation, false, message)
}
