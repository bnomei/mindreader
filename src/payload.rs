//! Shared agent-facing result dialect: handles, mutability, rateability, detail.
//!
//! Visibility is not mutability: a global record is visible in every `scope`
//! but mutable only when the request is `scope: []`. `handles` is a paste bag
//! of unused-role-safe `{kind, iri}` values. `concise` thins endpoints and
//! drops `about`; `detailed` keeps the full envelope.

use crate::domain::DomainError;
use crate::error::Result;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// Recall payload verbosity. Default is the current full envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    Detailed,
    Concise,
}

impl Detail {
    /// Parse recall verbosity; omitted or empty defaults to `detailed`.
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("detailed") => Ok(Self::Detailed),
            Some("concise") => Ok(Self::Concise),
            Some(other) => Err(DomainError::InvalidInput(format!(
                "detail must be concise or detailed, not {other:?}"
            ))
            .into()),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detailed => "detailed",
            Self::Concise => "concise",
        }
    }
}

/// Named memberships are mutable only in a named request that intersects them.
/// Global records (`[]`) are mutable only when the request is `scope: []`.
pub fn record_mutable(memberships: &[String], request_scope: &[String]) -> bool {
    if memberships.is_empty() {
        request_scope.is_empty()
    } else if request_scope.is_empty() {
        false
    } else {
        memberships
            .iter()
            .any(|layer| request_scope.iter().any(|requested| requested == layer))
    }
}

fn memberships_of(value: &Value) -> Vec<String> {
    value
        .get("scope")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Mark a fact envelope as current/historical, rateable, and mutable in this request.
pub fn decorate_fact(fact: &mut Value, request_scope: &[String], current: bool) {
    let memberships = memberships_of(fact);
    fact["current"] = json!(current);
    fact["rateable"] = json!(current);
    fact["mutable"] = json!(current && record_mutable(&memberships, request_scope));
}

/// Mark a non-literal node as rateable and mutable in this request.
pub fn decorate_node(node: &mut Value, request_scope: &[String]) {
    if node.get("kind").and_then(Value::as_str) == Some("literal") {
        return;
    }
    let memberships = memberships_of(node);
    node["rateable"] = json!(true);
    node["mutable"] = json!(record_mutable(&memberships, request_scope));
}

/// Unique non-literal subject/object nodes, first-seen order.
pub fn nodes_from_facts(facts: &[Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    for fact in facts {
        for pointer in ["/s", "/o"] {
            let Some(endpoint) = fact.pointer(pointer) else {
                continue;
            };
            if endpoint.get("kind").and_then(Value::as_str) == Some("literal") {
                continue;
            }
            let Some(iri) = endpoint.get("iri").and_then(Value::as_str) else {
                continue;
            };
            if !seen.insert(iri.to_string()) {
                continue;
            }
            nodes.push(endpoint.clone());
        }
    }
    nodes
}

/// Pasteable fact handle from a result envelope (`target.kind=fact` plus IRI).
pub fn fact_target(fact: &Value) -> Option<Value> {
    fact.get("target").cloned().filter(|target| {
        target.get("kind").and_then(Value::as_str) == Some("fact")
            && target.get("iri").and_then(Value::as_str).is_some()
    })
}

/// Pasteable node handle; literals have no unify/judge target.
pub fn node_target(node: &Value) -> Option<Value> {
    if node.get("kind").and_then(Value::as_str) == Some("literal") {
        return None;
    }
    if let Some(target) = node.get("target").cloned().filter(|target| {
        target.get("kind").and_then(Value::as_str) == Some("node")
            && target.get("iri").and_then(Value::as_str).is_some()
    }) {
        return Some(target);
    }
    node.get("iri")
        .and_then(Value::as_str)
        .map(|iri| json!({ "kind": "node", "iri": iri }))
}

fn unify_pairs(review_unify: &[Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut pairs = Vec::new();
    for item in review_unify {
        let Some(source) = item.pointer("/source/iri").and_then(Value::as_str) else {
            continue;
        };
        let Some(target) = item.pointer("/target/iri").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert((source.to_string(), target.to_string())) {
            continue;
        }
        pairs.push(json!({
            "source": { "kind": "node", "iri": source },
            "target": { "kind": "node", "iri": target },
        }));
    }
    pairs
}

fn push_unique_handle(handles: &mut Vec<Value>, seen: &mut HashSet<String>, handle: Value) {
    let Some(iri) = handle.get("iri").and_then(Value::as_str) else {
        return;
    };
    if seen.insert(iri.to_string()) {
        handles.push(handle);
    }
}

/// Neutral paste bag. Empty arrays and nulls are unused roles, not commands.
pub fn handles_bag(
    facts: &[Value],
    nodes: &[Value],
    current: Option<Value>,
    retired: Option<Value>,
    unify: &[Value],
) -> Value {
    let mut fact_handles = Vec::new();
    let mut seen_facts = HashSet::new();
    for fact in facts {
        if let Some(handle) = fact_target(fact) {
            push_unique_handle(&mut fact_handles, &mut seen_facts, handle);
        }
    }
    let mut node_handles = Vec::new();
    let mut seen_nodes = HashSet::new();
    for node in nodes {
        if let Some(handle) = node_target(node) {
            push_unique_handle(&mut node_handles, &mut seen_nodes, handle);
        }
    }
    for fact in facts {
        for pointer in ["/s", "/o"] {
            if let Some(handle) = fact.pointer(pointer).and_then(node_target) {
                push_unique_handle(&mut node_handles, &mut seen_nodes, handle);
            }
        }
    }
    json!({
        "facts": fact_handles,
        "nodes": node_handles,
        "current": current.unwrap_or(Value::Null),
        "retired": retired.unwrap_or(Value::Null),
        "unify": unify_pairs(unify),
    })
}

/// Facts plus lookup-local facts, first-seen by handle IRI, for the paste bag.
fn collect_recall_facts(result: &Value) -> Vec<Value> {
    let mut facts = result
        .get("facts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen = facts
        .iter()
        .filter_map(|fact| {
            fact.pointer("/target/iri")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<HashSet<_>>();
    if let Some(lookups) = result.get("lookups").and_then(Value::as_array) {
        for lookup in lookups {
            if let Some(lookup_facts) = lookup.get("facts").and_then(Value::as_array) {
                for fact in lookup_facts {
                    let Some(iri) = fact.pointer("/target/iri").and_then(Value::as_str) else {
                        continue;
                    };
                    if seen.insert(iri.to_string()) {
                        facts.push(fact.clone());
                    }
                }
            }
        }
    }
    facts
}

fn decorate_facts_in(value: &mut Value, request_scope: &[String]) {
    if let Some(facts) = value.get_mut("facts").and_then(Value::as_array_mut) {
        for fact in facts {
            let current = fact.get("current").and_then(Value::as_bool).unwrap_or(true);
            decorate_fact(fact, request_scope, current);
        }
    }
}

fn thin_endpoint(value: &Value) -> Value {
    if value.get("kind").and_then(Value::as_str) == Some("literal") {
        return json!({
            "kind": "literal",
            "iri": value.get("iri").cloned().unwrap_or(Value::Null),
            "value": value.get("value").cloned().unwrap_or(Value::Null),
            "datatype": value.get("datatype").cloned().unwrap_or(Value::Null),
        });
    }
    json!({
        "kind": "node",
        "iri": value.get("iri").cloned().unwrap_or(Value::Null),
        "name": value.get("name").cloned().unwrap_or(Value::Null),
        "target": value.get("target").cloned().unwrap_or(Value::Null),
    })
}

fn thin_fact(fact: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "target", "p", "scope", "spike", "weight", "current", "rateable", "mutable", "validTo",
    ] {
        if let Some(value) = fact.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    if let Some(subject) = fact.get("s") {
        out.insert("s".into(), thin_endpoint(subject));
    }
    if let Some(object) = fact.get("o") {
        out.insert("o".into(), thin_endpoint(object));
    }
    Value::Object(out)
}

fn thin_node(node: &Value) -> Value {
    if node.get("kind").and_then(Value::as_str) == Some("literal") {
        return thin_endpoint(node);
    }
    json!({
        "kind": "node",
        "iri": node.get("iri").cloned().unwrap_or(Value::Null),
        "name": node.get("name").cloned().unwrap_or(Value::Null),
        "labels": node.get("labels").cloned().unwrap_or_else(|| json!([])),
        "scope": node.get("scope").cloned().unwrap_or_else(|| json!([])),
        "target": node.get("target").cloned().unwrap_or(Value::Null),
        "rateable": node.get("rateable").cloned().unwrap_or(json!(true)),
        "mutable": node.get("mutable").cloned().unwrap_or(json!(false)),
    })
}

/// Stamp `detail` and, for `concise`, thin facts/nodes and clear `about`.
fn apply_detail(result: &mut Value, detail: Detail) {
    result["detail"] = json!(detail.as_str());
    if detail != Detail::Concise {
        return;
    }
    if let Some(facts) = result.get("facts").and_then(Value::as_array) {
        let thinned = facts.iter().map(thin_fact).collect::<Vec<_>>();
        result["facts"] = json!(thinned);
    }
    if let Some(nodes) = result.get("nodes").and_then(Value::as_array) {
        let thinned = nodes.iter().map(thin_node).collect::<Vec<_>>();
        result["nodes"] = json!(thinned);
    }
    if let Some(lookups) = result.get_mut("lookups").and_then(Value::as_array_mut) {
        for lookup in lookups {
            if let Some(facts) = lookup.get("facts").and_then(Value::as_array) {
                let thinned = facts.iter().map(thin_fact).collect::<Vec<_>>();
                lookup["facts"] = json!(thinned);
            }
            if let Some(node) = lookup.get("node") {
                lookup["node"] = thin_node(node);
            }
        }
    }
    result["about"] = json!([]);
}

/// Decorate a closed-world or semantic recall result and attach `handles`.
pub fn finish_recall(mut result: Value, request_scope: &[String], detail: Detail) -> Value {
    decorate_facts_in(&mut result, request_scope);
    if let Some(lookups) = result.get_mut("lookups").and_then(Value::as_array_mut) {
        for lookup in lookups {
            decorate_facts_in(lookup, request_scope);
            if let Some(node) = lookup.get_mut("node") {
                decorate_node(node, request_scope);
            }
        }
    }
    let facts = result
        .get("facts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let nodes_empty = result
        .get("nodes")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if nodes_empty && !facts.is_empty() {
        result["nodes"] = json!(nodes_from_facts(&facts));
    }
    if let Some(nodes) = result.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            decorate_node(node, request_scope);
        }
    }
    let collected = collect_recall_facts(&result);
    let nodes = result
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    result["handles"] = handles_bag(&collected, &nodes, None, None, &[]);
    apply_detail(&mut result, detail);
    result
}

/// Attach `handles` to a mutation result using its existing review/target fields.
pub fn finish_mutation(
    mut result: Value,
    facts: &[Value],
    nodes: &[Value],
    current: Option<Value>,
    retired: Option<Value>,
) -> Value {
    let unify = result
        .pointer("/review/unify")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    result["handles"] = handles_bag(facts, nodes, current, retired, &unify);
    result
}

/// Advisory unify row using the same node-handle dialect as `memory_unify`.
pub fn unify_review_item(
    source: &str,
    source_name: &str,
    target: &str,
    target_name: &str,
    similarity: f64,
) -> Value {
    json!({
        "source": { "kind": "node", "iri": source, "name": source_name },
        "target": { "kind": "node", "iri": target, "name": target_name },
        "similarity": similarity,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        decorate_fact, finish_recall, handles_bag, nodes_from_facts, record_mutable, Detail,
    };
    use serde_json::json;

    #[test]
    fn global_records_are_mutable_only_in_empty_scope() {
        assert!(record_mutable(&[], &[]));
        assert!(!record_mutable(&[], &["project:x".into()]));
        assert!(!record_mutable(&["project:x".into()], &[]));
        assert!(record_mutable(
            &["project:x".into()],
            &["project:x".into(), "team:y".into()]
        ));
        assert!(!record_mutable(&["project:x".into()], &["team:y".into()]));
    }

    #[test]
    fn nodes_from_facts_skip_literals_and_dedup() {
        let facts = vec![
            json!({
                "target": {"kind":"fact","iri":"mindreader:relationship/a"},
                "s": {"kind":"node","iri":"mindreader:element/alice","name":"Alice","target":{"kind":"node","iri":"mindreader:element/alice"}},
                "o": {"kind":"literal","iri":"mindreader:literal/x","value":"1","datatype":"xsd:string"}
            }),
            json!({
                "target": {"kind":"fact","iri":"mindreader:relationship/b"},
                "s": {"kind":"node","iri":"mindreader:element/alice","name":"Alice","target":{"kind":"node","iri":"mindreader:element/alice"}},
                "o": {"kind":"node","iri":"mindreader:element/mindreader","name":"Mindreader","target":{"kind":"node","iri":"mindreader:element/mindreader"}}
            }),
        ];
        let nodes = nodes_from_facts(&facts);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["iri"], "mindreader:element/alice");
        assert_eq!(nodes[1]["iri"], "mindreader:element/mindreader");
    }

    #[test]
    fn finish_recall_fills_nodes_and_handles() {
        let result = finish_recall(
            json!({
                "ok": true,
                "mode": "text",
                "facts": [{
                    "target": {"kind":"fact","iri":"mindreader:relationship/a"},
                    "s": {"kind":"node","iri":"mindreader:element/alice","name":"Alice","scope":["project:x"],"target":{"kind":"node","iri":"mindreader:element/alice"}},
                    "p": "worksOn",
                    "o": {"kind":"node","iri":"mindreader:element/mr","name":"mr","scope":["project:x"],"target":{"kind":"node","iri":"mindreader:element/mr"}},
                    "scope": ["project:x"],
                    "weight": 0
                }],
                "nodes": [],
                "paths": [],
                "about": [{"about":"noise"}],
                "lookups": [],
                "scope": ["project:x"]
            }),
            &["project:x".into()],
            Detail::Concise,
        );
        assert_eq!(result["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(
            result["handles"]["facts"][0]["iri"],
            "mindreader:relationship/a"
        );
        assert_eq!(result["detail"], "concise");
        assert!(result["about"].as_array().unwrap().is_empty());
        assert_eq!(result["facts"][0]["mutable"], true);
        assert_eq!(result["facts"][0]["rateable"], true);
        assert!(result["facts"][0].get("score").is_none());
    }

    #[test]
    fn handles_bag_exposes_unify_pairs() {
        let bag = handles_bag(
            &[],
            &[],
            None,
            None,
            &[json!({
                "source": {"kind":"node","iri":"mindreader:element/a","name":"A"},
                "target": {"kind":"node","iri":"mindreader:element/b","name":"B"},
                "similarity": 0.9
            })],
        );
        assert_eq!(bag["unify"][0]["source"]["kind"], "node");
        assert_eq!(bag["unify"][0]["source"]["iri"], "mindreader:element/a");
        assert!(bag["unify"][0].get("name").is_none());
    }

    #[test]
    fn decorate_fact_marks_global_visible_fact_immutable_under_named_scope() {
        let mut fact = json!({
            "target": {"kind":"fact","iri":"mindreader:relationship/a"},
            "scope": []
        });
        decorate_fact(&mut fact, &["project:x".into()], true);
        assert_eq!(fact["mutable"], false);
        assert_eq!(fact["rateable"], true);
    }
}
