//! The scheduled tag sweep, with alerts on orphans and on failures that keep happening

use std::collections::BTreeSet;

use cloudlease_machines::{
    alert::{Alert, Priority, SWEEP_KEY, UNLISTED_KEY_PREFIX},
    provider::MachineKind,
};
use cloudlease_worker::{
    error::CloudleaseResult,
    http::call_json,
    runtime::{WorkersRuntime, now},
    sweep::{SweepDelete, SweepReport, run_sweep},
};
use worker::{Env, Method};

use crate::{
    MONITOR, MONITOR_OBJECT, SWEEP,
    app::{
        alerts::notify,
        monitor::{Due, RECORD_PATH, Record},
        settings::provider,
    },
};

/// Runs one sweep and sends the alerts it calls for
pub async fn run(env: &Env) {
    let report = sweep(env).await;
    let failing: BTreeSet<String> = match &report {
        Ok(report) => {
            alert_orphans(env, &report.deletes).await;
            let unlisted = report
                .unlisted
                .iter()
                .map(|unlisted| format!("{UNLISTED_KEY_PREFIX}{}", unlisted.provider));
            report
                .owners_failed
                .iter()
                .map(|failure| failure.owner.as_str().to_owned())
                .chain(unlisted)
                .collect()
        }
        Err(error) => {
            worker::console_error!("cloudlease machines sweep failed: {error}");
            BTreeSet::from([SWEEP_KEY.to_owned()])
        }
    };
    if let Ok(report) = &report {
        log(report);
    }

    let due = match record_failures(env, failing).await {
        Ok(due) => due,
        Err(error) => {
            worker::console_error!("the monitor did not record the sweep: {error}");
            return;
        }
    };
    for (key, count) in due {
        notify(env, &failure_alert(&key, count, report.as_ref().ok())).await;
    }
}

async fn sweep(env: &Env) -> CloudleaseResult<SweepReport<MachineKind>> {
    let provider = provider(env)?;

    run_sweep(env, &provider, &WorkersRuntime, SWEEP, now()).await
}

fn log(report: &SweepReport<MachineKind>) {
    if report.is_clean() && report.unrecognized.is_empty() {
        return;
    }
    match serde_json::to_string(report) {
        Ok(json) => worker::console_warn!("cloudlease machines sweep: {json}"),
        Err(error) => worker::console_warn!("cloudlease machines sweep report: {error}"),
    }
}

async fn alert_orphans(env: &Env, deletes: &[SweepDelete<MachineKind>]) {
    // a resource of a lease its owner already ended is routine cleanup; any other one leaked
    let orphans: Vec<String> = deletes
        .iter()
        .filter(|delete| !delete.owner_ended)
        .map(|SweepDelete { report, .. }| {
            format!(
                "{:?} {} of {} generation {} ({:?}, {:?})",
                report.resource.tags.kind,
                report.resource.id,
                report.resource.tags.owner,
                report.resource.tags.generation,
                report.reason,
                report.outcome,
            )
        })
        .collect();
    if orphans.is_empty() {
        return;
    }

    notify(
        env,
        &Alert {
            title: format!(
                "cloudlease: sweep deleted {} orphaned resources",
                orphans.len()
            ),
            message: format!(
                "No owner was releasing these, so a lease ended without cleaning them up or \
                 they were never recorded:\n{}",
                orphans.join("\n")
            ),
            priority: Priority::Default,
        },
    )
    .await;
}

async fn record_failures(
    env: &Env,
    failing: BTreeSet<String>,
) -> CloudleaseResult<Vec<(String, u32)>> {
    let stub = env.durable_object(MONITOR)?.get_by_name(MONITOR_OBJECT)?;
    let due: Due = call_json(
        &stub,
        MONITOR_OBJECT,
        Method::Post,
        &format!("https://monitor{RECORD_PATH}"),
        Some(&Record { failing }),
    )
    .await?;

    Ok(due.due)
}

fn failure_alert(key: &str, count: u32, report: Option<&SweepReport<MachineKind>>) -> Alert {
    if key == SWEEP_KEY {
        return Alert {
            title: "cloudlease: the machines sweep keeps failing".to_owned(),
            message: format!(
                "The last {count} sweeps failed before asking any owner, so nothing was \
                 deleted by tag. Leases still end on their own alarms. See the Worker logs."
            ),
            priority: Priority::High,
        };
    }
    if let Some(provider) = key.strip_prefix(UNLISTED_KEY_PREFIX) {
        let error = report
            .and_then(|report| {
                report
                    .unlisted
                    .iter()
                    .find(|unlisted| unlisted.provider == provider)
            })
            .map_or("unknown", |unlisted| unlisted.error.as_str());
        return Alert {
            title: format!("cloudlease: the sweep cannot list {provider}"),
            message: format!(
                "The last {count} sweeps could not list {provider} (latest: {error}), so its \
                 orphans are not deleted and no permit is returned. The other providers are \
                 still swept, and leases still end on their own alarms."
            ),
            priority: Priority::High,
        };
    }

    let error = report
        .and_then(|report| {
            report
                .owners_failed
                .iter()
                .find(|failure| failure.owner.as_str() == key)
        })
        .map_or("unknown", |failure| failure.error.as_str());
    Alert {
        title: format!("cloudlease: run {key} keeps failing the sweep"),
        message: format!(
            "Run object {key} failed to answer the last {count} sweeps (latest: {error}), so \
             its resources are kept until it answers."
        ),
        priority: Priority::Default,
    }
}
