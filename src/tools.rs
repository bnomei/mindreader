//! Private operation-focused facade for graph mutations and closed-world recall.

mod facts;
mod judge;
mod place;
mod recall;

pub use facts::{
    memory_revise, memory_withdraw, memory_write, JudgeArgs, JudgeRating, PlaceArgs, PlaceEdit,
    ReviseArgs, TargetArgs, WithdrawArgs, WriteArgs, WriteFact,
};
pub use judge::memory_judge;
pub use place::memory_place;
pub use recall::{
    list_schema_catalog, memory_recall_around, memory_recall_history, memory_recall_iris,
};

#[cfg(test)]
use facts::{
    effective_weight, merge_memberships, plan_fact_membership_changes, prepare_write_fact,
    reject_system_owned_predicate, remove_memberships, revision_fact_lock_requests,
    select_revision_current, validate_write_args, withdrawal_fact_lock_requests,
    write_fact_lock_requests, CurrentFact, FactMembershipChange, LAYERS_PROPERTY, MAX_WRITE_FACTS,
    PREDICATE_USAGE_PROPERTY,
};
#[cfg(test)]
use judge::validate_judge_args;
#[cfg(test)]
use place::normalize_place_args;
#[cfg(test)]
use recall::{
    recall_around_query, RecallDirection, HUB_DEGREE_THRESHOLD, RECALL_IRI_FACTS_QUERY,
    RECALL_IRI_NODES_QUERY,
};
#[cfg(test)]
mod tests {
    use super::{
        effective_weight, merge_memberships, plan_fact_membership_changes, prepare_write_fact,
        recall_around_query, reject_system_owned_predicate, remove_memberships,
        revision_fact_lock_requests, select_revision_current, validate_judge_args,
        validate_write_args, withdrawal_fact_lock_requests, write_fact_lock_requests, CurrentFact,
        FactMembershipChange, JudgeArgs, JudgeRating, PlaceArgs, PlaceEdit, RecallDirection,
        TargetArgs, WriteArgs, WriteFact, HUB_DEGREE_THRESHOLD, LAYERS_PROPERTY, MAX_WRITE_FACTS,
        PREDICATE_USAGE_PROPERTY, RECALL_IRI_FACTS_QUERY, RECALL_IRI_NODES_QUERY,
    };
    use crate::domain::{DomainError, EntityInput, ObjectInput};
    use crate::error::Error;
    use crate::graph::{fact_lock_specs, spike_rank};
    use crate::vocabulary::CONTRADICTS_PROPERTY_IRI;

    #[test]
    fn spike_rank_order() {
        assert!(spike_rank(Some("Knowledge")) > spike_rank(Some("Insight")));
        assert!(spike_rank(Some("Insight")) > spike_rank(Some("Pattern")));
        assert!(spike_rank(Some("Pattern")) > spike_rank(Some("Signal")));
        assert!(spike_rank(Some("Signal")) > spike_rank(None));
    }

    #[test]
    fn recall_queries_are_set_oriented_bounded_and_deterministic() {
        assert!(RECALL_IRI_NODES_QUERY.contains("UNWIND range(0, size($iris) - 1)"));
        assert!(RECALL_IRI_NODES_QUERY.contains("ORDER BY inputIndex ASC"));
        assert!(RECALL_IRI_FACTS_QUERY.contains("CALL {"));
        assert!(!RECALL_IRI_FACTS_QUERY.contains("collect("));
        assert!(RECALL_IRI_FACTS_QUERY.contains("ORDER BY inputIndex ASC"));
        assert!(RECALL_IRI_FACTS_QUERY.contains("LIMIT $limit"));
        assert!(RECALL_IRI_FACTS_QUERY.contains("type(r) <> 'ABOUT'"));

        let around = recall_around_query(3, RecallDirection::Both);
        let predicate_filter = around.find("pathRel.propertyIri IN $predicates").unwrap();
        let limit = around.find("LIMIT $limit").unwrap();
        assert!(around.contains("pathRels*1..3"));
        assert!(predicate_filter < limit);
        assert!(around.contains("pathNodes ASC, pathEdgeIris ASC"));
        assert!(around.contains("length(path) + hubCount AS routeCost"));
        assert!(around.contains(&format!("eligibleDegree > {HUB_DEGREE_THRESHOLD}")));
        assert!(around.contains("RETURN distance, path, witnessNodes AS pathNodes"));
        assert!(around.contains("ORDER BY routeCost ASC, hubCount ASC, distance ASC"));
        assert!(recall_around_query(1, RecallDirection::Outgoing).contains("-[pathRels*1..1]->"));
        assert!(recall_around_query(1, RecallDirection::Incoming).contains("<-[pathRels*1..1]-"));
        assert!(RecallDirection::parse("sideways").is_err());
    }

    #[test]
    fn membership_merge_honors_global_dominance() {
        assert_eq!(
            merge_memberships(&[], &["project:a".into()]),
            Vec::<String>::new()
        );
        assert_eq!(
            merge_memberships(&["project:a".into()], &[]),
            Vec::<String>::new()
        );
        assert_eq!(
            merge_memberships(&["project:b".into()], &["project:a".into()]),
            vec!["project:a".to_string(), "project:b".to_string()]
        );
    }

    #[test]
    fn fact_handle_selection_never_falls_through_to_a_reasserted_identity() {
        let replacement = CurrentFact {
            rel_id: 2,
            iri: "fact:new".into(),
            layers: vec!["project:a".into()],
            spike: None,
        };
        assert!(select_revision_current(
            std::slice::from_ref(&replacement),
            &["project:a".into()],
            Some("fact:retired"),
        )
        .is_none());
        assert_eq!(
            select_revision_current(
                std::slice::from_ref(&replacement),
                &["project:a".into()],
                Some("fact:new"),
            )
            .map(|current| current.iri),
            Some("fact:new".into())
        );
    }

    #[test]
    fn judge_batch_prevalidates_modes_and_duplicate_targets() {
        let target = TargetArgs {
            kind: "fact".into(),
            iri: "mindreader:relationship/one".into(),
        };
        let valid = JudgeArgs {
            scope: vec!["project:x".into()],
            ratings: vec![JudgeRating {
                target: target.clone(),
                mode: "strengthen".into(),
            }],
        };
        assert!(validate_judge_args(&valid).is_ok());
        let duplicate = JudgeArgs {
            scope: valid.scope.clone(),
            ratings: vec![valid.ratings[0].clone(), valid.ratings[0].clone()],
        };
        assert!(validate_judge_args(&duplicate).is_err());
        let invalid_mode = JudgeArgs {
            scope: valid.scope,
            ratings: vec![JudgeRating {
                target,
                mode: "maybe".into(),
            }],
        };
        assert!(validate_judge_args(&invalid_mode).is_err());
    }

    #[test]
    fn place_batch_rejects_duplicates_and_normalizes_each_edit() {
        let target = TargetArgs {
            kind: "node".into(),
            iri: "mindreader:element/alice".into(),
        };
        let args = PlaceArgs {
            scope: vec!["project:x".into()],
            edits: vec![PlaceEdit {
                target: target.clone(),
                add: vec!["project:b".into(), "project:a".into(), "project:b".into()],
                remove: Vec::new(),
            }],
        };
        let (scope, edits) = super::normalize_place_args(args).expect("valid place batch");
        assert_eq!(scope, vec!["project:x"]);
        assert_eq!(edits[0].add, vec!["project:a", "project:b"]);

        let duplicate = PlaceArgs {
            scope: vec![],
            edits: vec![
                PlaceEdit {
                    target: target.clone(),
                    add: vec!["project:a".into()],
                    remove: Vec::new(),
                },
                PlaceEdit {
                    target,
                    add: vec!["project:b".into()],
                    remove: Vec::new(),
                },
            ],
        };
        assert!(super::normalize_place_args(duplicate).is_err());
    }

    #[test]
    fn selected_memberships_are_removed_without_globalizing_facts() {
        assert_eq!(
            remove_memberships(
                &["project:a".into(), "project:b".into()],
                &["project:a".into()]
            ),
            Some(vec!["project:b".to_string()])
        );
        assert_eq!(
            remove_memberships(&["project:a".into()], &["project:a".into()]),
            Some(Vec::new())
        );
        assert_eq!(remove_memberships(&[], &["project:a".into()]), None);
        assert_eq!(remove_memberships(&[], &[]), Some(Vec::new()));
    }

    #[test]
    fn write_and_revision_plan_all_known_locks_in_one_batch() {
        let write_locks = write_fact_lock_requests("subject", "property", "new", true);
        assert_eq!(write_locks.len(), 5);
        assert!(write_locks.contains(&(
            "new".into(),
            CONTRADICTS_PROPERTY_IRI.into(),
            "@fact".into()
        )));
        assert!(write_locks.contains(&(
            "property".into(),
            PREDICATE_USAGE_PROPERTY.into(),
            "@fact".into()
        )));

        let revision_locks = revision_fact_lock_requests("subject", "property", "old", "new", true);
        assert_eq!(revision_locks.len(), 6);
        assert!(revision_locks.contains(&("old".into(), LAYERS_PROPERTY.into(), "@fact".into())));
        assert!(revision_locks.contains(&(
            "new".into(),
            CONTRADICTS_PROPERTY_IRI.into(),
            "@fact".into()
        )));
    }

    #[test]
    fn withdrawal_plans_subject_and_predicate_guards_together() {
        assert_eq!(withdrawal_fact_lock_requests("subject", None).len(), 1);
        let locks = withdrawal_fact_lock_requests("subject", Some("property"));
        assert_eq!(locks.len(), 2);
        assert!(locks.contains(&(
            "property".into(),
            PREDICATE_USAGE_PROPERTY.into(),
            "@fact".into()
        )));
    }

    #[test]
    fn broad_withdrawal_plans_only_matching_named_memberships() {
        let currents = vec![
            current_fact(1, &[]),
            current_fact(2, &["project:a"]),
            current_fact(3, &["project:a", "project:b"]),
            current_fact(4, &["project:b"]),
        ];

        assert_eq!(
            plan_fact_membership_changes(&currents, &["project:a".into()]),
            vec![
                FactMembershipChange {
                    rel_id: 2,
                    remaining: Vec::new(),
                },
                FactMembershipChange {
                    rel_id: 3,
                    remaining: vec!["project:b".into()],
                },
            ]
        );
    }

    #[test]
    fn broad_global_withdrawal_plans_only_global_facts() {
        let currents = vec![current_fact(1, &[]), current_fact(2, &["project:a"])];

        assert_eq!(
            plan_fact_membership_changes(&currents, &[]),
            vec![FactMembershipChange {
                rel_id: 1,
                remaining: Vec::new(),
            }]
        );
    }

    fn current_fact(rel_id: i64, layers: &[&str]) -> CurrentFact {
        CurrentFact {
            rel_id,
            iri: format!("mindreader:fact/{rel_id}"),
            layers: layers.iter().map(|layer| (*layer).to_string()).collect(),
            spike: None,
        }
    }

    #[test]
    fn effective_weight_saturates_without_panicking() {
        assert_eq!(effective_weight(1, 2, 3), 6);
        assert_eq!(effective_weight(i64::MAX, 1, 1), i64::MAX);
        assert_eq!(effective_weight(i64::MIN, -1, -1), i64::MIN);
    }

    #[test]
    fn system_owned_history_predicates_are_not_client_writable() {
        assert!(reject_system_owned_predicate("SUPERSEDES").is_err());
        assert!(reject_system_owned_predicate("mindreader:property/CONTRADICTS").is_err());
        assert!(reject_system_owned_predicate("worksOn").is_ok());
    }

    #[test]
    fn write_facts_reject_empty_and_over_max() {
        let empty = WriteArgs {
            facts: Vec::new(),
            scope: Vec::new(),
        };
        let over = WriteArgs {
            facts: (0..=MAX_WRITE_FACTS)
                .map(|index| WriteFact {
                    s: EntityInput {
                        kind: "node".into(),
                        iri: None,
                        name: Some(format!("s{index}")),
                        labels: vec!["Element".into()],
                    },
                    p: "worksOn".into(),
                    o: ObjectInput {
                        kind: "node".into(),
                        iri: None,
                        name: Some(format!("o{index}")),
                        labels: vec!["Element".into()],
                        value: None,
                        datatype: None,
                    },
                    spike: None,
                    contradicts: false,
                })
                .collect(),
            scope: Vec::new(),
        };
        assert!(matches!(
            validate_write_args(&empty),
            Err(Error::Domain(DomainError::InvalidInput(_)))
        ));
        assert!(matches!(
            validate_write_args(&over),
            Err(Error::Domain(DomainError::InvalidInput(_)))
        ));
        let one = WriteArgs {
            facts: vec![over.facts[0].clone()],
            scope: Vec::new(),
        };
        assert!(validate_write_args(&one).is_ok());
    }

    #[test]
    fn write_fact_lock_union_bounds_at_max_facts() {
        let mut locks = Vec::new();
        for index in 0..MAX_WRITE_FACTS {
            let fact = prepare_write_fact(WriteFact {
                s: EntityInput {
                    kind: "node".into(),
                    iri: Some(format!("mindreader:element/s{index}")),
                    name: None,
                    labels: vec!["Element".into()],
                },
                p: format!("prop{index}"),
                o: ObjectInput {
                    kind: "node".into(),
                    iri: Some(format!("mindreader:element/o{index}")),
                    name: None,
                    labels: vec!["Element".into()],
                    value: None,
                    datatype: None,
                },
                spike: None,
                contradicts: true,
            })
            .expect("unique contradict facts prepare");
            locks.extend(write_fact_lock_requests(
                &fact.subject_iri,
                &fact.prop_iri,
                &fact.object_iri,
                fact.contradicts,
            ));
        }
        assert_eq!(locks.len(), 100);
        let expanded = fact_lock_specs(&locks);
        assert_eq!(
            expanded.len(),
            160,
            "20 unique contradict facts expand to 160 fact-lock rows, not the unexpanded 100"
        );
    }

    #[test]
    fn target_args_accept_only_pasteable_handles() {
        let node_handle = serde_json::json!({
            "kind": "node",
            "iri": "mindreader:element/alice"
        });
        let expanded_node = serde_json::json!({
            "kind": "node",
            "iri": "mindreader:element/alice",
            "name": "Alice",
            "labels": ["Element"],
            "layers": ["project:x"],
            "weight": 0
        });
        let expanded_fact = serde_json::json!({
            "kind": "fact",
            "iri": "mindreader:relationship/abc",
            "type": "ASSERTS",
            "from": "mindreader:element/alice",
            "to": "mindreader:element/mindreader",
            "propertyIri": "mindreader:property/worksOn",
            "layers": ["project:x"],
            "weight": 0
        });
        let node_target: TargetArgs = serde_json::from_value(node_handle).unwrap();
        assert_eq!(node_target.kind, "node");
        assert_eq!(node_target.iri, "mindreader:element/alice");
        assert!(serde_json::from_value::<TargetArgs>(expanded_node).is_err());
        assert!(serde_json::from_value::<TargetArgs>(expanded_fact).is_err());
    }
}
