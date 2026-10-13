//! Leader election over a `coordination.k8s.io/v1` Lease (ADR 0035).
//!
//! Every replica translates and serves snapshots; only the leader writes
//! status and provisions proxies, so two replicas never fight over an
//! object. The protocol is client-go's: the holder renews the Lease every
//! [`RENEW_EVERY`]; a candidate takes it over once it has gone unrenewed for
//! [`LEASE_DURATION`]. Updates carry the Lease's resourceVersion, so of two
//! candidates racing for an expired Lease exactly one wins. A leader that
//! cannot renew within [`RENEW_DEADLINE`] steps down before another replica
//! can take over.

use std::time::Duration;

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::Api;
use kube::api::{PostParams, ResourceExt};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// How long a Lease stays valid without renewal.
pub const LEASE_DURATION: Duration = Duration::from_secs(15);
/// How long a leader keeps leading while renewals fail.
pub const RENEW_DEADLINE: Duration = Duration::from_secs(10);
/// How often the holder renews (and candidates retry).
pub const RENEW_EVERY: Duration = Duration::from_secs(2);

/// What to do with the Lease as it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// No Lease exists: create it, holding it.
    Create,
    /// We hold it: renew.
    Renew,
    /// It is free or expired: take it over.
    Acquire,
    /// Someone else holds a live Lease.
    Follow,
}

/// Decides from the Lease spec (if the Lease exists) and the time now.
#[must_use]
pub fn decide(lease: Option<&LeaseSpec>, me: &str, now: Timestamp) -> Action {
    let Some(spec) = lease else {
        return Action::Create;
    };
    match spec.holder_identity.as_deref() {
        Some(h) if h == me => Action::Renew,
        None | Some("") => Action::Acquire,
        Some(_) => {
            let duration = spec.lease_duration_seconds.map_or(LEASE_DURATION, |s| {
                Duration::from_secs(u64::try_from(s).unwrap_or(0))
            });
            let renewed = spec.renew_time.as_ref().map(|t| t.0);
            let expired = renewed.is_none_or(|t| {
                now.duration_since(t)
                    > SignedDuration::try_from(duration).unwrap_or(SignedDuration::MAX)
            });
            if expired {
                Action::Acquire
            } else {
                Action::Follow
            }
        }
    }
}

fn spec(me: &str, now: Timestamp, prev: Option<&LeaseSpec>, acquire: bool) -> LeaseSpec {
    let transitions = prev.and_then(|p| p.lease_transitions).unwrap_or(0);
    LeaseSpec {
        holder_identity: Some(me.to_owned()),
        lease_duration_seconds: Some(i32::try_from(LEASE_DURATION.as_secs()).unwrap_or(15)),
        acquire_time: if acquire {
            Some(MicroTime(now))
        } else {
            prev.and_then(|p| p.acquire_time.clone())
        },
        renew_time: Some(MicroTime(now)),
        lease_transitions: Some(if acquire && prev.is_some() {
            transitions + 1
        } else {
            transitions
        }),
        ..LeaseSpec::default()
    }
}

/// One election round: returns whether we hold the Lease afterwards.
async fn round(api: &Api<Lease>, name: &str, me: &str) -> Result<bool, kube::Error> {
    let now = Timestamp::now();
    let current = api.get_opt(name).await?;
    let action = decide(current.as_ref().and_then(|l| l.spec.as_ref()), me, now);
    debug!(?action, "leader election round");
    match (action, current) {
        (Action::Create, _) => {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(name.to_owned()),
                    ..ObjectMeta::default()
                },
                spec: Some(spec(me, now, None, true)),
            };
            api.create(&PostParams::default(), &lease).await?;
            Ok(true)
        }
        (Action::Follow, _) => Ok(false),
        (action, Some(mut lease)) => {
            lease.spec = Some(spec(
                me,
                now,
                lease.spec.as_ref(),
                action == Action::Acquire,
            ));
            // The resourceVersion in `lease` makes this a compare-and-swap.
            api.replace(name, &PostParams::default(), &lease).await?;
            Ok(true)
        }
        (_, None) => Ok(false),
    }
}

/// Runs the election until `stop`, publishing leadership on `tx`. On stop,
/// a leader releases the Lease so another replica takes over at once.
pub async fn run(
    api: Api<Lease>,
    name: String,
    me: String,
    tx: watch::Sender<bool>,
    stop: CancellationToken,
) {
    let mut last_renewal = tokio::time::Instant::now();
    loop {
        let leading = *tx.borrow();
        match round(&api, &name, &me).await {
            Ok(now_leading) => {
                if now_leading {
                    last_renewal = tokio::time::Instant::now();
                }
                if now_leading != leading {
                    info!(identity = %me, leading = now_leading, "leadership changed");
                    tx.send_replace(now_leading);
                }
            }
            Err(e) => {
                // A conflict means another replica won the race.
                debug!(error = %e, "leader election round failed");
                if leading && last_renewal.elapsed() > RENEW_DEADLINE {
                    warn!(identity = %me, "cannot renew the lease; stepping down");
                    tx.send_replace(false);
                }
            }
        }
        tokio::select! {
            () = stop.cancelled() => break,
            () = tokio::time::sleep(RENEW_EVERY) => {}
        }
    }
    if *tx.borrow() {
        tx.send_replace(false);
        if let Ok(Some(mut lease)) = api.get_opt(&name).await
            && lease
                .spec
                .as_ref()
                .and_then(|s| s.holder_identity.as_deref())
                == Some(me.as_str())
        {
            if let Some(s) = lease.spec.as_mut() {
                s.holder_identity = None;
            }
            match api
                .replace(&lease.name_any(), &PostParams::default(), &lease)
                .await
            {
                Ok(_) => info!(identity = %me, "released the lease"),
                Err(e) => warn!(error = %e, "could not release the lease"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn held(by: &str, renewed: i64) -> LeaseSpec {
        LeaseSpec {
            holder_identity: Some(by.into()),
            lease_duration_seconds: Some(15),
            renew_time: Some(MicroTime(at(renewed))),
            ..LeaseSpec::default()
        }
    }

    #[test]
    fn decisions() {
        assert_eq!(decide(None, "a", at(100)), Action::Create);
        assert_eq!(decide(Some(&held("a", 99)), "a", at(100)), Action::Renew);
        assert_eq!(decide(Some(&held("b", 90)), "a", at(100)), Action::Follow);
        assert_eq!(
            decide(Some(&held("b", 80)), "a", at(100)),
            Action::Acquire,
            "expired"
        );
        assert_eq!(
            decide(Some(&LeaseSpec::default()), "a", at(100)),
            Action::Acquire,
            "released"
        );
    }

    #[test]
    fn takeover_counts_a_transition() {
        let prev = held("b", 80);
        let s = spec("a", at(100), Some(&prev), true);
        assert_eq!(s.holder_identity.as_deref(), Some("a"));
        assert_eq!(s.lease_transitions, Some(1));
        let renewed = spec("a", at(102), Some(&s), false);
        assert_eq!(renewed.lease_transitions, Some(1));
        assert_eq!(renewed.acquire_time, s.acquire_time);
    }
}
