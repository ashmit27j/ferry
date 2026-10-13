//! Status objects from a [`Translation`](crate::translate::Translation).
//!
//! These functions are pure: given the status an object has now and what the
//! translator decided, they return the status to write, or `None` when
//! nothing changed (so a steady cluster sees no writes). Conditions keep
//! their `lastTransitionTime` while their status is unchanged.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use serde_json::{Value, json};

use crate::gateway_api::GROUP;
use crate::translate::{
    ClassStatus, Cond, GatewayOutput, ParentStatus, PolicyStatus, RouteStatus, SUPPORTED_FEATURES,
};

/// Turns translator conditions into Kubernetes conditions, reusing the
/// transition time of an existing condition of the same type and status.
#[must_use]
pub fn conditions(old: &[Condition], new: &[Cond], generation: i64, now: &Time) -> Vec<Condition> {
    new.iter()
        .map(|c| {
            let status = if c.status { "True" } else { "False" };
            let since = old
                .iter()
                .find(|o| o.type_ == c.type_ && o.status == status)
                .map_or_else(|| now.clone(), |o| o.last_transition_time.clone());
            Condition {
                type_: c.type_.to_owned(),
                status: status.to_owned(),
                reason: c.reason.clone(),
                message: c.message.clone(),
                observed_generation: Some(generation),
                last_transition_time: since,
            }
        })
        .collect()
}

fn conditions_of(status: &Value, pointer: &str) -> Vec<Condition> {
    status
        .pointer(pointer)
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default()
}

/// `desired` if any field it sets differs from `current`. Fields Ferry does
/// not manage (another controller's, or newer API fields) are not compared,
/// and `null` (a merge-patch delete) matches an absent field.
fn changed(current: &Value, desired: &Value) -> Option<Value> {
    let Some(fields) = desired.as_object() else {
        return (current != desired).then(|| desired.clone());
    };
    let differs = fields
        .iter()
        .any(|(k, v)| current.get(k).unwrap_or(&Value::Null) != v);
    differs.then(|| desired.clone())
}

/// The GatewayClass status to write.
#[must_use]
pub fn class(current: &Value, desired: &ClassStatus, now: &Time) -> Option<Value> {
    let conds = conditions(
        &conditions_of(current, "/conditions"),
        &desired.conditions,
        desired.generation,
        now,
    );
    let features: Vec<Value> = SUPPORTED_FEATURES
        .iter()
        .map(|f| json!({ "name": f }))
        .collect();
    changed(
        current,
        &json!({ "conditions": conds, "supportedFeatures": features }),
    )
}

/// The Gateway status to write.
#[must_use]
pub fn gateway(current: &Value, desired: &GatewayOutput, now: &Time) -> Option<Value> {
    let generation = desired.generation;
    let conds = conditions(
        &conditions_of(current, "/conditions"),
        &desired.conditions,
        generation,
        now,
    );
    let listeners: Vec<Value> = desired
        .listeners
        .iter()
        .map(|l| {
            let old: Vec<Condition> = current
                .get("listeners")
                .and_then(Value::as_array)
                .and_then(|ls| {
                    ls.iter()
                        .find(|x| x.get("name").and_then(Value::as_str) == Some(&l.name))
                })
                .map(|x| conditions_of(x, "/conditions"))
                .unwrap_or_default();
            let kinds: Vec<Value> = l
                .supported_kinds
                .iter()
                .map(|k| json!({ "group": GROUP, "kind": k.as_str() }))
                .collect();
            json!({
                "name": l.name,
                "supportedKinds": kinds,
                "attachedRoutes": l.attached_routes,
                "conditions": conditions(&old, &l.conditions, generation, now),
            })
        })
        .collect();
    // No address yet: `null` removes a stale one in the merge patch.
    let addresses = if desired.addresses.is_empty() {
        Value::Null
    } else {
        desired
            .addresses
            .iter()
            .map(|(t, v)| json!({ "type": t, "value": v }))
            .collect()
    };
    let status = json!({ "conditions": conds, "listeners": listeners, "addresses": addresses });
    changed(current, &status)
}

fn parent_ref(p: &ParentStatus) -> Value {
    let mut r = json!({
        "group": p.group,
        "kind": p.kind,
        "namespace": p.namespace,
        "name": p.name,
    });
    if let Some(s) = &p.section_name {
        r["sectionName"] = json!(s);
    }
    if let Some(port) = p.port {
        r["port"] = json!(port);
    }
    r
}

/// The route status to write: other controllers' parent entries are kept as
/// they are, ours are replaced. `None` when nothing changes.
#[must_use]
pub fn route(
    current: &Value,
    desired: &RouteStatus,
    controller: &str,
    now: &Time,
) -> Option<Value> {
    let existing: Vec<Value> = current
        .get("parents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut parents: Vec<Value> = existing
        .iter()
        .filter(|p| p.get("controllerName").and_then(Value::as_str) != Some(controller))
        .cloned()
        .collect();
    for p in &desired.parents {
        let r = parent_ref(p);
        let old = existing
            .iter()
            .find(|x| {
                x.get("controllerName").and_then(Value::as_str) == Some(controller)
                    && x.get("parentRef") == Some(&r)
            })
            .map(|x| conditions_of(x, "/conditions"))
            .unwrap_or_default();
        parents.push(json!({
            "parentRef": r,
            "controllerName": controller,
            "conditions": conditions(&old, &p.conditions, desired.generation, now),
        }));
    }
    let empty = current.get("parents").is_none() && parents.is_empty();
    if empty {
        return None;
    }
    changed(current, &json!({ "parents": parents }))
}

/// The FerryPolicy status to write.
#[must_use]
pub fn policy(current: &Value, desired: &PolicyStatus, now: &Time) -> Option<Value> {
    let conds = conditions(
        &conditions_of(current, "/conditions"),
        &desired.conditions,
        desired.generation,
        now,
    );
    changed(current, &json!({ "conditions": conds }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::RouteKind;
    use k8s_openapi::jiff::Timestamp;

    fn at(secs: i64) -> Time {
        Time(Timestamp::from_second(secs).unwrap())
    }

    fn cond(type_: &'static str, status: bool, reason: &str) -> Cond {
        Cond {
            type_,
            status,
            reason: reason.into(),
            message: String::new(),
        }
    }

    #[test]
    fn transition_time_survives_unchanged_status() {
        let first = conditions(&[], &[cond("Accepted", true, "Accepted")], 1, &at(100));
        let again = conditions(&first, &[cond("Accepted", true, "Accepted")], 2, &at(200));
        assert_eq!(again[0].last_transition_time, at(100));
        assert_eq!(again[0].observed_generation, Some(2));
        let flipped = conditions(&again, &[cond("Accepted", false, "Invalid")], 3, &at(300));
        assert_eq!(flipped[0].last_transition_time, at(300));
    }

    #[test]
    fn steady_state_writes_nothing() {
        let desired = RouteStatus {
            generation: 1,
            parents: vec![ParentStatus {
                group: GROUP.into(),
                kind: "Gateway".into(),
                namespace: "ns".into(),
                name: "gw".into(),
                section_name: None,
                port: None,
                conditions: vec![cond("Accepted", true, "Accepted")],
            }],
        };
        let first = route(&Value::Null, &desired, "me", &at(1)).unwrap();
        assert!(route(&first, &desired, "me", &at(2)).is_none());
    }

    #[test]
    fn other_controllers_keep_their_parents() {
        let current = json!({ "parents": [
            { "controllerName": "other", "parentRef": { "name": "x" }, "conditions": [] },
            { "controllerName": "me", "parentRef": { "name": "stale" }, "conditions": [] },
        ]});
        let next = route(&current, &RouteStatus::default(), "me", &at(1)).unwrap();
        let parents = next["parents"].as_array().unwrap();
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0]["controllerName"], "other");
        assert!(route(&Value::Null, &RouteStatus::default(), "me", &at(1)).is_none());
        let _ = RouteKind::Http;
    }

    #[test]
    fn unmanaged_fields_do_not_cause_writes() {
        let desired = PolicyStatus {
            generation: 1,
            conditions: vec![cond("Accepted", true, "Accepted")],
        };
        let mut current = policy(&Value::Null, &desired, &at(1)).unwrap();
        current["somethingNewer"] = json!(3);
        assert!(policy(&current, &desired, &at(2)).is_none());
    }

    #[test]
    fn class_status_lists_supported_features() {
        let s = class(
            &Value::Null,
            &ClassStatus {
                generation: 1,
                conditions: vec![cond("Accepted", true, "Accepted")],
            },
            &at(1),
        )
        .unwrap();
        let features = s["supportedFeatures"].as_array().unwrap();
        assert!(features.iter().any(|f| f["name"] == "GRPCRoute"));
    }
}
