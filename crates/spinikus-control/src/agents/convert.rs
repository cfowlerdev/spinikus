//! Conversions from protocol messages to core types. Values an agent should never send,
//! such as unspecified enums or malformed IDs, are reported as errors to log.

use jiff::Timestamp;
use spinikus_core::server::{AgentInfo, InstanceHealth, InstanceStatus, ObservedInstance, ServerFacts};
use spinikus_proto::agent::v1 as pb;

pub(crate) fn agent_info(hello: &pb::Hello) -> AgentInfo {
    let facts = hello.facts.clone().unwrap_or_default();
    AgentInfo {
        agent_version: hello.agent_version.clone(),
        protocol_version: hello.protocol_version,
        facts: ServerFacts {
            hostname: facts.hostname,
            os: facts.os,
            arch: facts.arch,
            cpu_count: facts.cpu_count,
            memory_bytes: facts.memory_bytes,
        },
    }
}

pub(crate) fn instance(instance: pb::Instance) -> Result<ObservedInstance, String> {
    let status = match pb::InstanceStatus::try_from(instance.status) {
        Ok(pb::InstanceStatus::Created) => InstanceStatus::Created,
        Ok(pb::InstanceStatus::Running) => InstanceStatus::Running,
        Ok(pb::InstanceStatus::Paused) => InstanceStatus::Paused,
        Ok(pb::InstanceStatus::Restarting) => InstanceStatus::Restarting,
        Ok(pb::InstanceStatus::Removing) => InstanceStatus::Removing,
        Ok(pb::InstanceStatus::Exited) => InstanceStatus::Exited,
        Ok(pb::InstanceStatus::Dead) => InstanceStatus::Dead,
        Ok(pb::InstanceStatus::Unspecified) | Err(_) => return Err(format!("unknown status {}", instance.status)),
    };
    let health = match pb::InstanceHealth::try_from(instance.health) {
        Ok(pb::InstanceHealth::None) => InstanceHealth::None,
        Ok(pb::InstanceHealth::Starting) => InstanceHealth::Starting,
        Ok(pb::InstanceHealth::Healthy) => InstanceHealth::Healthy,
        Ok(pb::InstanceHealth::Unhealthy) => InstanceHealth::Unhealthy,
        Ok(pb::InstanceHealth::Unspecified) | Err(_) => return Err(format!("unknown health {}", instance.health)),
    };
    let started_at = match instance.started_at {
        Some(at) => Some(timestamp(at)?),
        None => None,
    };
    Ok(ObservedInstance {
        service_id: instance.service_id.parse().map_err(|error| format!("{error}"))?,
        deployment_id: instance.deployment_id.parse().map_err(|error| format!("{error}"))?,
        container_id: instance.container_id,
        status,
        health,
        restart_count: instance.restart_count,
        started_at,
    })
}

fn timestamp(at: prost_types::Timestamp) -> Result<Timestamp, String> {
    Timestamp::new(at.seconds, at.nanos).map_err(|error| format!("invalid timestamp: {error}"))
}

#[cfg(test)]
mod tests {
    use spinikus_core::id::{DeploymentId, ServiceId};

    use super::*;

    fn reported(status: pb::InstanceStatus) -> pb::Instance {
        pb::Instance {
            container_id: "c1".to_string(),
            service_id: ServiceId::new().to_string(),
            deployment_id: DeploymentId::new().to_string(),
            status: status.into(),
            health: pb::InstanceHealth::Healthy.into(),
            restart_count: 2,
            started_at: Some(prost_types::Timestamp { seconds: 1_800_000_000, nanos: 0 }),
        }
    }

    #[test]
    fn converts_reported_instances() {
        let instance = instance(reported(pb::InstanceStatus::Running)).unwrap();

        assert_eq!(
            (instance.status, instance.health, instance.restart_count),
            (InstanceStatus::Running, InstanceHealth::Healthy, 2)
        );
        assert_eq!(instance.started_at.unwrap().as_second(), 1_800_000_000);
    }

    #[test]
    fn rejects_what_an_agent_should_never_send() {
        let unspecified = reported(pb::InstanceStatus::Unspecified);
        let bad_id = pb::Instance { service_id: "web".to_string(), ..reported(pb::InstanceStatus::Running) };
        let future_status = pb::Instance { status: 99, ..reported(pb::InstanceStatus::Running) };

        assert!(instance(unspecified).is_err());
        assert!(instance(bad_id).is_err());
        assert!(instance(future_status).is_err());
    }
}
