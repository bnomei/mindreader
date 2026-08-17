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

    /// Wire token stamped on recall results (`detailed` or `concise`).
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

/// Read the agent-facing `scope` array (graph storage still uses `layers`).
fn memberships_of(value: &Value) -> Result<Vec<String>> {
    let scope = value
        .get("scope")
        .and_then(Value::as_array)
        .ok_or_else(|| crate::operation_error!("record is missing its scope array"))?;
    scope
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| crate::operation_error!("record scope contains a non-string value"))
        })
        .collect()
}

/// Mark a fact envelope as current/historical, rateable, and mutable in this request.
pub fn decorate_fact(fact: &mut Value, request_scope: &[String], current: bool) -> Result<()> {
    fact_target(fact)
        .ok_or_else(|| crate::operation_error!("fact is missing its canonical target handle"))?;
    for endpoint in ["s", "o"] {
        let value = fact
            .get(endpoint)
            .ok_or_else(|| crate::operation_error!("fact is missing its {endpoint} endpoint"))?;
        validate_endpoint(value)?;
    }
    nonempty_string(fact, "p", "fact")?;
    let memberships = memberships_of(fact)?;
    fact.get("weight")
        .and_then(Value::as_i64)
        .ok_or_else(|| crate::operation_error!("fact is missing its integer weight"))?;
    fact["current"] = json!(current);
    fact["rateable"] = json!(current);
    fact["mutable"] = json!(current && record_mutable(&memberships, request_scope));
    Ok(())
}

/// Mark a non-literal node as rateable and mutable in this request.
pub fn decorate_node(node: &mut Value, request_scope: &[String]) -> Result<()> {
    validate_endpoint(node)?;
    if node.get("kind").and_then(Value::as_str) == Some("literal") {
        return Ok(());
    }
    let memberships = memberships_of(node)?;
    node["rateable"] = json!(true);
    node["mutable"] = json!(record_mutable(&memberships, request_scope));
    Ok(())
}

/// Fail closed on a node or literal endpoint that is missing identity, memberships, or weight.
fn validate_endpoint(value: &Value) -> Result<()> {
    match value.get("kind").and_then(Value::as_str) {
        Some("literal") => {
            nonempty_string(value, "iri", "literal endpoint")?;
            value.get("value").and_then(Value::as_str).ok_or_else(|| {
                crate::operation_error!("literal endpoint is missing its string value")
            })?;
            nonempty_string(value, "datatype", "literal endpoint")?;
            memberships_of(value)?;
            value.get("weight").and_then(Value::as_i64).ok_or_else(|| {
                crate::operation_error!("literal endpoint is missing its integer weight")
            })?;
        }
        Some("node") => {
            let iri = nonempty_string(value, "iri", "node endpoint")?;
            match value.get("name") {
                Some(Value::String(_)) | Some(Value::Null) => {}
                _ => {
                    return Err(crate::operation_error!(
                        "node endpoint is missing its string-or-null name"
                    ));
                }
            }
            string_array(value, "labels", "node endpoint")?;
            memberships_of(value)?;
            value.get("weight").and_then(Value::as_i64).ok_or_else(|| {
                crate::operation_error!("node endpoint is missing its integer weight")
            })?;
            let target = value.get("target").ok_or_else(|| {
                crate::operation_error!("node endpoint is missing its canonical target handle")
            })?;
            let target_iri = exact_handle_iri(target, "node", "node target")?;
            if target_iri != iri {
                return Err(crate::operation_error!(
                    "node target IRI does not match the node IRI"
                ));
            }
        }
        _ => return Err(crate::operation_error!("endpoint has an invalid kind")),
    }
    Ok(())
}

/// Require a non-empty string field; used to keep handle envelopes pasteable.
fn nonempty_string<'a>(value: &'a Value, field: &str, context: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| crate::operation_error!("{context} is missing non-empty {field}"))
}

/// Require an array of strings (labels); mixed types fail closed.
fn string_array(value: &Value, field: &str, context: &str) -> Result<()> {
    let values = value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| crate::operation_error!("{context} is missing its {field} array"))?;
    if values.iter().any(|value| value.as_str().is_none()) {
        return Err(crate::operation_error!(
            "{context} {field} contains a non-string value"
        ));
    }
    Ok(())
}

/// Accept only `{kind, iri}` — extra fields make a handle unsafe to paste back.
fn exact_handle_iri<'a>(value: &'a Value, kind: &str, context: &str) -> Result<&'a str> {
    let object = value
        .as_object()
        .ok_or_else(|| crate::operation_error!("{context} is not an object"))?;
    if object.len() != 2 || value.get("kind").and_then(Value::as_str) != Some(kind) {
        return Err(crate::operation_error!(
            "{context} must contain exactly kind={kind:?} and iri"
        ));
    }
    nonempty_string(value, "iri", context)
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
    let target = fact.get("target")?;
    exact_handle_iri(target, "fact", "fact target").ok()?;
    Some(target.clone())
}

/// Pasteable node handle; literals have no unify/judge target.
pub fn node_target(node: &Value) -> Option<Value> {
    if node.get("kind").and_then(Value::as_str) == Some("literal") {
        return None;
    }
    if exact_handle_iri(node, "node", "node handle").is_ok() {
        return Some(node.clone());
    }
    let iri = nonempty_string(node, "iri", "node").ok()?;
    let target = node.get("target")?;
    (exact_handle_iri(target, "node", "node target").ok()? == iri).then(|| target.clone())
}

/// Deduplicate advisory unify pairs while requiring exact pasteable node handles.
fn unify_pairs(review_unify: &[Value]) -> Result<Vec<Value>> {
    let mut seen = HashSet::new();
    let mut pairs = Vec::new();
    for item in review_unify {
        let source = item
            .get("source")
            .ok_or_else(|| crate::operation_error!("unify review item is missing source"))?;
        let source_iri = exact_handle_iri(source, "node", "unify source")?;
        let target = item
            .get("target")
            .ok_or_else(|| crate::operation_error!("unify review item is missing target"))?;
        let target_iri = exact_handle_iri(target, "node", "unify target")?;
        if !seen.insert((source_iri.to_string(), target_iri.to_string())) {
            continue;
        }
        pairs.push(json!({
            "source": source,
            "target": target,
        }));
    }
    Ok(pairs)
}

/// Append a handle once per IRI so the paste bag stays first-seen order.
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
) -> Result<Value> {
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
    Ok(json!({
        "facts": fact_handles,
        "nodes": node_handles,
        "current": current.unwrap_or(Value::Null),
        "retired": retired.unwrap_or(Value::Null),
        "unify": unify_pairs(unify)?,
    }))
}

/// Facts plus lookup-local facts, first-seen by handle IRI, for the paste bag.
fn collect_recall_facts(result: &Value) -> Result<Vec<Value>> {
    let mut facts = result
        .get("facts")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| crate::operation_error!("recall result is missing its facts array"))?;
    let mut seen = facts
        .iter()
        .map(|fact| {
            fact_target(fact)
                .and_then(|target| {
                    target
                        .get("iri")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .ok_or_else(|| crate::operation_error!("recall fact has an invalid target handle"))
        })
        .collect::<Result<HashSet<_>>>()?;
    let lookups = result
        .get("lookups")
        .and_then(Value::as_array)
        .ok_or_else(|| crate::operation_error!("recall result is missing its lookups array"))?;
    for lookup in lookups {
        let lookup_facts = lookup
            .get("facts")
            .and_then(Value::as_array)
            .ok_or_else(|| crate::operation_error!("recall lookup is missing its facts array"))?;
        for fact in lookup_facts {
            let iri = fact
                .pointer("/target/iri")
                .and_then(Value::as_str)
                .filter(|iri| !iri.is_empty())
                .ok_or_else(|| crate::operation_error!("recall fact is missing its target IRI"))?;
            if seen.insert(iri.to_string()) {
                facts.push(fact.clone());
            }
        }
    }
    Ok(facts)
}

/// Stamp `current`/`rateable`/`mutable` on every fact in a result or lookup envelope.
fn decorate_facts_in(value: &mut Value, request_scope: &[String]) -> Result<()> {
    let facts = value
        .get_mut("facts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| crate::operation_error!("recall payload is missing its facts array"))?;
    for fact in facts {
        let current = fact
            .get("current")
            .and_then(Value::as_bool)
            .ok_or_else(|| crate::operation_error!("fact is missing its current flag"))?;
        decorate_fact(fact, request_scope, current)?;
    }
    Ok(())
}

/// Clone a required envelope field; missing keys fail closed rather than thinning.
fn required_field(value: &Value, field: &str, context: &str) -> Result<Value> {
    value
        .get(field)
        .cloned()
        .ok_or_else(|| crate::operation_error!("{context} is missing {field}"))
}

/// Concise endpoint: keep identity and pasteable target; drop labels, weight, and memberships.
fn thin_endpoint(value: &Value) -> Result<Value> {
    validate_endpoint(value)?;
    if value.get("kind").and_then(Value::as_str) == Some("literal") {
        return Ok(json!({
            "kind": "literal",
            "iri": required_field(value, "iri", "literal endpoint")?,
            "value": required_field(value, "value", "literal endpoint")?,
            "datatype": required_field(value, "datatype", "literal endpoint")?,
        }));
    }
    let mut out = Map::from_iter([
        ("kind".into(), json!("node")),
        ("iri".into(), required_field(value, "iri", "node endpoint")?),
        (
            "target".into(),
            required_field(value, "target", "node endpoint")?,
        ),
    ]);
    if let Some(name) = value.get("name") {
        out.insert("name".into(), name.clone());
    }
    Ok(Value::Object(out))
}

/// Concise fact: keep the handle, `s`/`p`/`o`, mutability flags, and optional Spike/weight/`validTo`.
fn thin_fact(fact: &Value) -> Result<Value> {
    let mut out = Map::new();
    for key in ["target", "p", "scope", "current", "rateable", "mutable"] {
        out.insert(key.to_string(), required_field(fact, key, "fact")?);
    }
    for key in ["spike", "weight", "validTo"] {
        if let Some(value) = fact.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    let subject = fact
        .get("s")
        .ok_or_else(|| crate::operation_error!("fact is missing s"))?;
    out.insert("s".into(), thin_endpoint(subject)?);
    let object = fact
        .get("o")
        .ok_or_else(|| crate::operation_error!("fact is missing o"))?;
    out.insert("o".into(), thin_endpoint(object)?);
    Ok(Value::Object(out))
}

/// Concise node: keep the handle, labels, memberships, and mutability flags.
fn thin_node(node: &Value) -> Result<Value> {
    if node.get("kind").and_then(Value::as_str) == Some("literal") {
        return thin_endpoint(node);
    }
    validate_endpoint(node)?;
    let mut out = Map::from_iter([
        ("kind".into(), json!("node")),
        ("iri".into(), required_field(node, "iri", "node")?),
        ("labels".into(), required_field(node, "labels", "node")?),
        ("scope".into(), required_field(node, "scope", "node")?),
        ("target".into(), required_field(node, "target", "node")?),
        ("rateable".into(), required_field(node, "rateable", "node")?),
        ("mutable".into(), required_field(node, "mutable", "node")?),
    ]);
    if let Some(name) = node.get("name") {
        out.insert("name".into(), name.clone());
    }
    Ok(Value::Object(out))
}

/// Stamp `detail` and, for `concise`, thin facts/nodes and clear `about`.
fn apply_detail(result: &mut Value, detail: Detail) -> Result<()> {
    result["detail"] = json!(detail.as_str());
    if detail != Detail::Concise {
        return Ok(());
    }
    let facts = result
        .get("facts")
        .and_then(Value::as_array)
        .ok_or_else(|| crate::operation_error!("recall result is missing its facts array"))?;
    result["facts"] = Value::Array(facts.iter().map(thin_fact).collect::<Result<Vec<_>>>()?);
    let nodes = result
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| crate::operation_error!("recall result is missing its nodes array"))?;
    result["nodes"] = Value::Array(nodes.iter().map(thin_node).collect::<Result<Vec<_>>>()?);
    let lookups = result
        .get_mut("lookups")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| crate::operation_error!("recall result is missing its lookups array"))?;
    for lookup in lookups {
        let facts = lookup
            .get("facts")
            .and_then(Value::as_array)
            .ok_or_else(|| crate::operation_error!("recall lookup is missing its facts array"))?;
        lookup["facts"] = Value::Array(facts.iter().map(thin_fact).collect::<Result<Vec<_>>>()?);
        if let Some(node) = lookup.get("node") {
            lookup["node"] = thin_node(node)?;
        }
    }
    result["about"] = json!([]);
    result["paths"] = json!([]);
    Ok(())
}

/// Drop top-level iris hops=1 `facts[]` for concise recall; keep lookup facts.
pub fn omit_iris_top_level_facts(result: &mut Value) {
    result["facts"] = json!([]);
}

/// Decorate a closed-world or semantic recall result and attach `handles`.
pub fn finish_recall(mut result: Value, request_scope: &[String], detail: Detail) -> Result<Value> {
    decorate_facts_in(&mut result, request_scope)?;
    let lookups = result
        .get_mut("lookups")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| crate::operation_error!("recall result is missing its lookups array"))?;
    for lookup in lookups {
        decorate_facts_in(lookup, request_scope)?;
        if let Some(node) = lookup.get_mut("node") {
            decorate_node(node, request_scope)?;
        }
    }
    let facts = result
        .get("facts")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| crate::operation_error!("recall result is missing its facts array"))?;
    let nodes_empty = result
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| crate::operation_error!("recall result is missing its nodes array"))?
        .is_empty();
    if nodes_empty && !facts.is_empty() {
        result["nodes"] = json!(nodes_from_facts(&facts));
    }
    let nodes = result
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| crate::operation_error!("recall result is missing its nodes array"))?;
    for node in nodes {
        decorate_node(node, request_scope)?;
    }
    let collected = collect_recall_facts(&result)?;
    let nodes = result
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| crate::operation_error!("recall result is missing its nodes array"))?;
    result["handles"] = handles_bag(&collected, &nodes, None, None, &[])?;
    apply_detail(&mut result, detail)?;
    Ok(result)
}

/// Attach `handles` to a mutation result using its existing review/target fields.
pub fn finish_mutation(
    mut result: Value,
    facts: &[Value],
    nodes: &[Value],
    current: Option<Value>,
    retired: Option<Value>,
) -> Result<Value> {
    let unify = result
        .pointer("/review/unify")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    result["handles"] = handles_bag(facts, nodes, current, retired, &unify)?;
    Ok(result)
}

/// Advisory unify row with exact pasteable handles and separate display names.
pub fn unify_review_item(
    source: &str,
    source_name: &str,
    target: &str,
    target_name: &str,
    similarity: f64,
) -> Value {
    json!({
        "source": { "kind": "node", "iri": source },
        "sourceName": source_name,
        "target": { "kind": "node", "iri": target },
        "targetName": target_name,
        "similarity": similarity,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        decorate_fact, finish_recall, handles_bag, nodes_from_facts, omit_iris_top_level_facts,
        record_mutable, unify_review_item, Detail,
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
    fn unify_review_handles_are_exactly_pasteable() {
        let item = unify_review_item("element:a", "A", "element:b", "B", 0.9);
        assert_eq!(item["source"], json!({"kind": "node", "iri": "element:a"}));
        assert_eq!(item["target"], json!({"kind": "node", "iri": "element:b"}));
        assert_eq!(item["sourceName"], "A");
        assert_eq!(item["targetName"], "B");
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
                    "s": {"kind":"node","iri":"mindreader:element/alice","name":"Alice","labels":["Element"],"scope":["project:x"],"weight":0,"target":{"kind":"node","iri":"mindreader:element/alice"}},
                    "p": "worksOn",
                    "o": {"kind":"node","iri":"mindreader:element/mr","name":"mr","labels":["Element"],"scope":["project:x"],"weight":0,"target":{"kind":"node","iri":"mindreader:element/mr"}},
                    "scope": ["project:x"],
                    "weight": 0,
                    "current": true
                }],
                "nodes": [],
                "paths": [],
                "about": [{"about":"noise"}],
                "lookups": [],
                "scope": ["project:x"]
            }),
            &["project:x".into()],
            Detail::Concise,
        )
        .unwrap();
        assert_eq!(result["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(
            result["handles"]["facts"][0]["iri"],
            "mindreader:relationship/a"
        );
        assert_eq!(result["detail"], "concise");
        assert!(result["about"].as_array().unwrap().is_empty());
        assert!(result["paths"].as_array().unwrap().is_empty());
        assert_eq!(result["facts"][0]["mutable"], true);
        assert_eq!(result["facts"][0]["rateable"], true);
        assert!(result["facts"][0].get("score").is_none());
    }

    #[test]
    fn omit_iris_top_level_facts_keeps_lookups() {
        let mut result = json!({
            "facts": [{"target":{"kind":"fact","iri":"mindreader:relationship/a"}}],
            "lookups": [{"iri":"mindreader:element/a","facts":[{"target":{"kind":"fact","iri":"mindreader:relationship/a"}}]}]
        });
        omit_iris_top_level_facts(&mut result);
        assert!(result["facts"].as_array().unwrap().is_empty());
        assert_eq!(result["lookups"][0]["facts"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn handles_bag_exposes_unify_pairs() {
        let bag = handles_bag(
            &[],
            &[],
            None,
            None,
            &[json!({
                "source": {"kind":"node","iri":"mindreader:element/a"},
                "target": {"kind":"node","iri":"mindreader:element/b"},
                "similarity": 0.9
            })],
        )
        .unwrap();
        assert_eq!(bag["unify"][0]["source"]["kind"], "node");
        assert_eq!(bag["unify"][0]["source"]["iri"], "mindreader:element/a");
        assert!(bag["unify"][0].get("name").is_none());

        let malformed = handles_bag(
            &[],
            &[],
            None,
            None,
            &[json!({
                "source": {"kind":"node","iri":"mindreader:element/a","name":"legacy"},
                "target": {"kind":"node","iri":"mindreader:element/b"}
            })],
        );
        assert!(malformed.is_err());
    }

    #[test]
    fn decorate_fact_marks_global_visible_fact_immutable_under_named_scope() {
        let mut fact = json!({
            "target": {"kind":"fact","iri":"mindreader:relationship/a"},
            "s": {"kind":"node","iri":"mindreader:element/a","name":"A","labels":["Element"],"scope":[],"weight":0,"target":{"kind":"node","iri":"mindreader:element/a"}},
            "p": "value",
            "o": {"kind":"literal","iri":"mindreader:literal/one","value":"1","datatype":"xsd:string","scope":[],"weight":0},
            "scope": [],
            "weight": 0
        });
        decorate_fact(&mut fact, &["project:x".into()], true).unwrap();
        assert_eq!(fact["mutable"], false);
        assert_eq!(fact["rateable"], true);
    }

    #[test]
    fn malformed_recall_records_fail_closed() {
        let valid = json!({
            "ok": true,
            "mode": "text",
            "facts": [{
                "target": {"kind":"fact","iri":"mindreader:relationship/a"},
                "s": {"kind":"node","iri":"mindreader:element/a","name":"A","labels":["Element"],"scope":[],"weight":0,"target":{"kind":"node","iri":"mindreader:element/a"}},
                "p": "knows",
                "o": {"kind":"node","iri":"mindreader:element/b","name":"B","labels":["Element"],"scope":[],"weight":0,"target":{"kind":"node","iri":"mindreader:element/b"}},
                "scope": [],
                "current": true,
                "weight": 0
            }],
            "nodes": [],
            "paths": [],
            "about": [],
            "lookups": [],
            "scope": []
        });

        let mut missing_scope = valid.clone();
        missing_scope["facts"][0]
            .as_object_mut()
            .unwrap()
            .remove("scope");
        assert!(finish_recall(missing_scope, &[], Detail::Detailed).is_err());

        let mut missing_current = valid.clone();
        missing_current["facts"][0]
            .as_object_mut()
            .unwrap()
            .remove("current");
        assert!(finish_recall(missing_current, &[], Detail::Detailed).is_err());

        let mut missing_target = valid.clone();
        missing_target["facts"][0]["s"]
            .as_object_mut()
            .unwrap()
            .remove("target");
        assert!(finish_recall(missing_target, &[], Detail::Detailed).is_err());

        let mut mismatched_target = valid.clone();
        mismatched_target["facts"][0]["s"]["target"]["iri"] = json!("mindreader:element/wrong");
        assert!(finish_recall(mismatched_target, &[], Detail::Detailed).is_err());

        let mut decorated_fact_target = valid.clone();
        decorated_fact_target["facts"][0]["target"]["name"] = json!("legacy decoration");
        assert!(finish_recall(decorated_fact_target, &[], Detail::Detailed).is_err());

        let mut malformed_literal = valid.clone();
        malformed_literal["facts"][0]["o"] = json!({
            "kind": "literal",
            "iri": "mindreader:literal/one",
            "value": "1",
            "scope": [],
            "weight": 0
        });
        assert!(finish_recall(malformed_literal, &[], Detail::Detailed).is_err());

        let mut missing_labels = valid;
        missing_labels["facts"][0]["s"]
            .as_object_mut()
            .unwrap()
            .remove("labels");
        assert!(finish_recall(missing_labels, &[], Detail::Concise).is_err());
    }
}
