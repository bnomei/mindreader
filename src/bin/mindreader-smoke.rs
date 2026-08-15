use anyhow::{anyhow, Result};
use mindreader::config::Config;
use mindreader::graph::{self};
use mindreader::tools::{
    self, AssertArgs, GetArgs, RetractArgs, SchemaArgs, SearchArgs, TraverseArgs,
};
use mindreader::Mindreader;
use serde_json::{json, Value};
use std::process::ExitCode;

struct Report {
    failed: u32,
}

impl Report {
    fn new() -> Self {
        Self { failed: 0 }
    }

    fn check(&mut self, step: u32, name: &str, ok: bool, detail: &str) {
        if ok {
            println!("PASS {step} {name}");
        } else {
            println!("FAIL {step} {name}");
            self.failed += 1;
        }
        if !detail.is_empty() {
            println!("     {detail}");
        }
    }
}

fn iri_of(v: &Value) -> Option<String> {
    v.get("iri")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            v.get("node")
                .and_then(|n| n.get("iri"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            v.get("s")
                .and_then(|n| n.get("iri"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(0) => {
            println!("ALL PASS");
            ExitCode::SUCCESS
        }
        Ok(_) => {
            println!("SOME STEPS FAILED");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("SMOKE ABORT: {e:?}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<u32> {
    load();
    let cfg = Config::from_env()?;
    println!("mindreader-smoke: uri={} project={}", cfg.uri, cfg.project);
    println!(
        "registered tools: {}",
        Mindreader::registered_tool_names().join(", ")
    );
    if Mindreader::registered_tool_names().len() != 7 {
        return Err(anyhow!("expected 7 registered tools"));
    }

    let graph = graph::connect(&cfg).await?;
    graph::bootstrap(&graph).await?;
    let project = cfg.project.as_str();
    let mut r = Report::new();

    // 1. schema
    let person = tools::memory_schema(
        &graph,
        project,
        SchemaArgs {
            kind: "class".into(),
            name: Some("Person".into()),
            iri: None,
            sub_class_of: None,
            sub_property_of: None,
            domain: None,
            range: None,
        },
    )
    .await;
    let project_cls = tools::memory_schema(
        &graph,
        project,
        SchemaArgs {
            kind: "class".into(),
            name: Some("Project".into()),
            iri: None,
            sub_class_of: None,
            sub_property_of: None,
            domain: None,
            range: None,
        },
    )
    .await;
    let works = tools::memory_schema(
        &graph,
        project,
        SchemaArgs {
            kind: "property".into(),
            name: Some("worksOn".into()),
            iri: None,
            sub_class_of: None,
            sub_property_of: None,
            domain: Some("Person".into()),
            range: Some("Project".into()),
        },
    )
    .await;
    let schema_ok = person.is_ok() && project_cls.is_ok() && works.is_ok();
    r.check(
        1,
        "memory_schema Class Person, Class Project, Property worksOn",
        schema_ok,
        &format!(
            "person={} project={} worksOn={}",
            person
                .as_ref()
                .ok()
                .and_then(iri_of)
                .unwrap_or_else(|| format!("{person:?}")),
            project_cls
                .as_ref()
                .ok()
                .and_then(iri_of)
                .unwrap_or_default(),
            works.as_ref().ok().and_then(iri_of).unwrap_or_default()
        ),
    );

    // 2. assert Bruno worksOn graph-memory
    let asserted = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"name": "Bruno", "labels": ["Element"]}),
            p: "worksOn".into(),
            o: json!({"name": "graph-memory", "labels": ["Element"]}),
            layer: Some("project:graph-memory".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let (bruno_iri, project_iri) = match &asserted {
        Ok(v) => (
            v.get("s")
                .and_then(|s| s.get("iri"))
                .and_then(|x| x.as_str())
                .unwrap_or("mindreader:element/bruno")
                .to_string(),
            v.get("o")
                .and_then(|s| s.get("iri"))
                .and_then(|x| x.as_str())
                .unwrap_or("mindreader:element/graph-memory")
                .to_string(),
        ),
        Err(_) => (
            "mindreader:element/bruno".into(),
            "mindreader:element/graph-memory".into(),
        ),
    };
    r.check(
        2,
        "memory_assert Element Bruno worksOn graph-memory Project",
        asserted.is_ok(),
        &format!("bruno={bruno_iri} project={project_iri} raw={asserted:?}"),
    );

    // 3. Signal ABOUT Bruno, layer global
    let signal = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"name": "bruno-observed", "labels": ["Signal"]}),
            p: "ABOUT".into(),
            o: json!({"iri": bruno_iri}),
            layer: Some("global".into()),
            spike: Some("Signal".into()),
            contradicts: false,
        },
    )
    .await;
    r.check(
        3,
        "memory_assert Signal ABOUT Bruno layer global",
        signal.is_ok(),
        &format!("{signal:?}"),
    );

    // 4. get Bruno hops=1 sees project fact
    let got = tools::memory_get(
        &graph,
        project,
        GetArgs {
            iri: bruno_iri.clone(),
            hops: Some(1),
        },
    )
    .await;
    let sees_project = got
        .as_ref()
        .ok()
        .map(|v| {
            let text = v.to_string();
            v.get("found").and_then(|x| x.as_bool()).unwrap_or(false)
                && (text.contains(&project_iri)
                    || text.contains("worksOn")
                    || text.contains("graph-memory"))
        })
        .unwrap_or(false);
    r.check(
        4,
        "memory_get Bruno hops=1 sees the project fact",
        sees_project,
        &format!("{got:?}"),
    );

    // 5. search Bruno
    let search = tools::memory_search(
        &graph,
        project,
        SearchArgs {
            text: Some("Bruno".into()),
            labels: None,
            limit: Some(20),
        },
    )
    .await;
    let found_bruno = search
        .as_ref()
        .ok()
        .map(|v| {
            let mode_ok = v.get("mode").and_then(|m| m.as_str()) == Some("wakeup");
            let facts = v
                .get("facts")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            let hit = facts.iter().any(|f| {
                let s = f.get("s");
                s.and_then(|n| n.get("iri"))
                    .and_then(|x| x.as_str())
                    .map(|s| s == bruno_iri || s.contains("bruno"))
                    .unwrap_or(false)
                    || s.and_then(|n| n.get("name"))
                        .and_then(|x| x.as_str())
                        .map(|s| s.eq_ignore_ascii_case("Bruno"))
                        .unwrap_or(false)
                    || f.to_string().to_ascii_lowercase().contains("bruno")
            });
            mode_ok && hit
        })
        .unwrap_or(false);
    r.check(
        5,
        "memory_search \"Bruno\" returns facts (wakeup)",
        found_bruno,
        &format!("{search:?}"),
    );

    // 6. traverse from Bruno depth 2
    let trav = tools::memory_traverse(
        &graph,
        project,
        TraverseArgs {
            from: bruno_iri.clone(),
            rels: None,
            depth: Some(2),
            limit: Some(50),
        },
    )
    .await;
    let trav_ok = trav
        .as_ref()
        .ok()
        .map(|v| {
            v.get("found").and_then(|x| x.as_bool()).unwrap_or(false)
                && v.get("nodes")
                    .and_then(|n| n.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    r.check(
        6,
        "memory_traverse from Bruno depth 2",
        trav_ok,
        &format!("{trav:?}"),
    );

    // 7. re-assert same worksOn — idempotent
    let again = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"iri": bruno_iri}),
            p: "worksOn".into(),
            o: json!({"iri": project_iri}),
            layer: Some("project:graph-memory".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let (cur_n, cur_objs) =
        tools::count_current_asserts(&graph, &bruno_iri, "worksOn", "project:graph-memory").await?;
    let idem = again
        .as_ref()
        .ok()
        .and_then(|v| v.get("noop").and_then(|x| x.as_bool()))
        .unwrap_or(false)
        && cur_n == 1;
    r.check(
        7,
        "re-assert same worksOn is idempotent (one current ASSERTS)",
        idem,
        &format!("count={cur_n} objects={cur_objs:?} again={again:?}"),
    );

    // 8. assert worksOn different o — supersedes
    let other = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"iri": bruno_iri}),
            p: "worksOn".into(),
            o: json!({"name": "other-desk", "labels": ["Element"]}),
            layer: Some("project:graph-memory".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let (cur_n2, cur_objs2) =
        tools::count_current_asserts(&graph, &bruno_iri, "worksOn", "project:graph-memory").await?;
    let hist =
        tools::count_historical_asserts(&graph, &bruno_iri, "worksOn", "project:graph-memory")
            .await?;
    let other_iri = other
        .as_ref()
        .ok()
        .and_then(|v| {
            v.get("o")
                .and_then(|o| o.get("iri"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or("")
        .to_string();
    let superseded = other
        .as_ref()
        .ok()
        .and_then(|v| v.get("superseded"))
        .map(|s| !s.is_null())
        .unwrap_or(false)
        && cur_n2 == 1
        && hist >= 1
        && cur_objs2.iter().any(|o| o == &other_iri);
    r.check(
        8,
        "assert worksOn different o supersedes (old validTo set)",
        superseded,
        &format!("current={cur_n2} hist={hist} objs={cur_objs2:?} other={other:?}"),
    );

    // 9. retract — soft; node still gettable
    let retracted = tools::memory_retract(
        &graph,
        project,
        RetractArgs {
            iri: None,
            s: Some(bruno_iri.clone()),
            p: Some("worksOn".into()),
            o: Some(json!(other_iri)),
            layer: Some("project:graph-memory".into()),
            reason: Some("smoke retract".into()),
        },
    )
    .await;
    let still = tools::memory_get(
        &graph,
        project,
        GetArgs {
            iri: bruno_iri.clone(),
            hops: Some(0),
        },
    )
    .await;
    let (cur_n3, _) =
        tools::count_current_asserts(&graph, &bruno_iri, "worksOn", "project:graph-memory").await?;
    let retract_ok = retracted
        .as_ref()
        .ok()
        .and_then(|v| v.get("soft").and_then(|x| x.as_bool()))
        .unwrap_or(false)
        && still
            .as_ref()
            .ok()
            .and_then(|v| v.get("found").and_then(|x| x.as_bool()))
            .unwrap_or(false)
        && cur_n3 == 0;
    r.check(
        9,
        "retract is soft; node still gettable",
        retract_ok,
        &format!("retract={retracted:?} get={still:?} current_asserts={cur_n3}"),
    );

    // 10. layer project:other rejected
    let rejected = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"iri": bruno_iri}),
            p: "worksOn".into(),
            o: json!({"iri": project_iri}),
            layer: Some("project:other".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let reject_ok = match &rejected {
        Err(e) => {
            let msg = e.to_string();
            msg.contains("not allowed") || msg.contains("layer")
        }
        Ok(_) => false,
    };
    r.check(
        10,
        "layer project:other is rejected",
        reject_ok,
        &format!("{rejected:?}"),
    );

    // --- v1.1: conflicts, CONTRADICTS, wakeup rank ---
    // Unique names so a second smoke run does not inherit leftover CONTRADICTS.
    let tag = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "x".into());
    let v11_s = json!({"name": format!("v11-subj-{tag}"), "labels": ["Element"]});
    let desk_a = format!("v11-alpha-{tag}");
    let desk_b = format!("v11-beta-{tag}");
    let desk_c = format!("v11-gamma-{tag}");

    let a = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: v11_s.clone(),
            p: "worksOn".into(),
            o: json!({"name": desk_a, "labels": ["Element"]}),
            layer: Some("global".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let a_iri = a
        .as_ref()
        .ok()
        .and_then(|v| {
            v.get("o")
                .and_then(|o| o.get("iri"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or("")
        .to_string();
    let v11_iri = a
        .as_ref()
        .ok()
        .and_then(|v| {
            v.get("s")
                .and_then(|s| s.get("iri"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or("")
        .to_string();
    r.check(
        11,
        "v1.1 assert subject worksOn A on global",
        a.is_ok() && !a_iri.is_empty(),
        &format!("{a:?}"),
    );

    let b = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: v11_s.clone(),
            p: "worksOn".into(),
            o: json!({"name": desk_b, "labels": ["Element"]}),
            layer: Some("project:graph-memory".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let b_iri = b
        .as_ref()
        .ok()
        .and_then(|v| {
            v.get("o")
                .and_then(|o| o.get("iri"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or("")
        .to_string();
    let conflicts = b
        .as_ref()
        .ok()
        .and_then(|v| v.get("conflicts").and_then(|c| c.as_array()))
        .cloned()
        .unwrap_or_default();
    let mentions_global_a = conflicts.iter().any(|c| {
        let layer = c.get("layer").and_then(|x| x.as_str()).unwrap_or("");
        let o = c
            .get("o")
            .and_then(|o| o.get("iri"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        layer == "global" && (o == a_iri || o.contains("v11-alpha-"))
    });
    let contradicts_before = count_contradicts(&graph, &b_iri).await.unwrap_or(-1);
    r.check(
        12,
        "v1.1 project worksOn B returns conflicts[] for global/A; no CONTRADICTS without flag",
        b.is_ok() && mentions_global_a && contradicts_before == 0,
        &format!("conflicts={conflicts:?} contradicts={contradicts_before} b={b:?}"),
    );

    let b2 = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"iri": v11_iri}),
            p: "worksOn".into(),
            o: json!({"iri": b_iri}),
            layer: Some("project:graph-memory".into()),
            spike: None,
            contradicts: true,
        },
    )
    .await;
    let contradicts_after = count_contradicts(&graph, &b_iri).await.unwrap_or(-1);
    r.check(
        13,
        "v1.1 contradicts:true writes CONTRADICTS to conflicting o",
        b2.is_ok() && contradicts_after == 1,
        &format!("contradicts={contradicts_after} b2={b2:?}"),
    );

    let a2 = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"iri": v11_iri}),
            p: "worksOn".into(),
            o: json!({"name": desk_c, "labels": ["Element"]}),
            layer: Some("global".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let a2_iri = a2
        .as_ref()
        .ok()
        .and_then(|v| {
            v.get("o")
                .and_then(|o| o.get("iri"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or("")
        .to_string();
    let b3 = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"iri": v11_iri}),
            p: "worksOn".into(),
            o: json!({"iri": b_iri}),
            layer: Some("project:graph-memory".into()),
            spike: None,
            contradicts: true,
        },
    )
    .await;
    let contradicts_multi = count_contradicts(&graph, &b_iri).await.unwrap_or(-1);
    r.check(
        14,
        "v1.1 later fight adds another CONTRADICTS (does not supersede first)",
        a2.is_ok() && b3.is_ok() && contradicts_multi == 2 && !a2_iri.is_empty(),
        &format!("contradicts={contradicts_multi} a2={a2:?} b3={b3:?}"),
    );

    let knowledge = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"name": "bruno-known", "labels": ["Knowledge"]}),
            p: "ABOUT".into(),
            o: json!({"iri": bruno_iri}),
            layer: Some("global".into()),
            spike: Some("Knowledge".into()),
            contradicts: false,
        },
    )
    .await;
    r.check(
        15,
        "v1.1 Knowledge ABOUT Bruno",
        knowledge.is_ok(),
        &format!("{knowledge:?}"),
    );

    let wake = tools::memory_search(
        &graph,
        project,
        SearchArgs {
            text: Some("Bruno".into()),
            labels: None,
            limit: Some(30),
        },
    )
    .await;
    let wake_ok = wake
        .as_ref()
        .ok()
        .map(|v| {
            let mode = v.get("mode").and_then(|m| m.as_str()) == Some("wakeup");
            let facts = v
                .get("facts")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            let spikes = v
                .get("spike")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            let has_fact = !facts.is_empty()
                && facts.iter().any(|f| {
                    f.get("s")
                        .and_then(|s| s.get("iri"))
                        .and_then(|x| x.as_str())
                        == Some(bruno_iri.as_str())
                        || f.get("p")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .contains("worksOn")
                        || f.to_string().to_ascii_lowercase().contains("bruno")
                });
            let no_nodes_dir = v.get("nodes").is_none();
            let first_spike = spikes
                .first()
                .and_then(|s| s.get("rank").and_then(|x| x.as_str()));
            let knowledge_first = first_spike == Some("Knowledge");
            let bruno_fact_spike = facts.iter().find(|f| {
                f.get("s")
                    .and_then(|s| s.get("iri"))
                    .and_then(|x| x.as_str())
                    == Some(bruno_iri.as_str())
            });
            let fact_ranked = bruno_fact_spike
                .and_then(|f| f.get("spike").and_then(|x| x.as_str()))
                == Some("Knowledge");
            mode && has_fact && no_nodes_dir && knowledge_first && fact_ranked
        })
        .unwrap_or(false);
    r.check(
        16,
        "v1.1 memory_search Bruno returns facts+ABOUT SPIKE; Knowledge ranks above Signal",
        wake_ok,
        &format!("{wake:?}"),
    );

    let lit = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"iri": bruno_iri}),
            p: "note".into(),
            o: json!({"value": "v11-unique-literal-token"}),
            layer: Some("project:graph-memory".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let lit_search = tools::memory_search(
        &graph,
        project,
        SearchArgs {
            text: Some("v11-unique-literal-token".into()),
            labels: None,
            limit: Some(20),
        },
    )
    .await;
    let lit_ok = lit.is_ok()
        && lit_search
            .as_ref()
            .ok()
            .map(|v| {
                v.get("mode").and_then(|m| m.as_str()) == Some("wakeup")
                    && v.get("facts")
                        .and_then(|n| n.as_array())
                        .map(|arr| {
                            arr.iter()
                                .any(|f| f.to_string().contains("v11-unique-literal-token"))
                        })
                        .unwrap_or(false)
            })
            .unwrap_or(false);
    r.check(
        17,
        "v1.1 memory_search finds literal/factText (not node directory)",
        lit_ok,
        &format!("assert={lit:?} search={lit_search:?}"),
    );

    // 18. retract without layer uses default_write_layer only
    let retract_s = json!({"name": format!("v11-retract-s-{tag}"), "labels": ["Element"]});
    let g_assert = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: retract_s.clone(),
            p: "desk".into(),
            o: json!({"name": format!("v11-retract-g-{tag}"), "labels": ["Element"]}),
            layer: Some("global".into()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let p_assert = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: retract_s.clone(),
            p: "desk".into(),
            o: json!({"name": format!("v11-retract-p-{tag}"), "labels": ["Element"]}),
            layer: None,
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let retract_s_iri = g_assert
        .as_ref()
        .ok()
        .and_then(|v| {
            v.get("s")
                .and_then(|s| s.get("iri"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or("")
        .to_string();
    let retract_nolayer = tools::memory_retract(
        &graph,
        project,
        RetractArgs {
            iri: None,
            s: Some(retract_s_iri.clone()),
            p: Some("desk".into()),
            o: None,
            layer: None,
            reason: Some("smoke retract no layer".into()),
        },
    )
    .await;
    let (g_left, _) =
        tools::count_current_asserts(&graph, &retract_s_iri, "desk", "global").await?;
    let (p_left, _) = tools::count_current_asserts(&graph, &retract_s_iri, "desk", project).await?;
    let used_layer = retract_nolayer
        .as_ref()
        .ok()
        .and_then(|v| v.get("layer").and_then(|x| x.as_str()))
        .unwrap_or("");
    r.check(
        18,
        "retract without layer uses default_write_layer only; other layers untouched",
        g_assert.is_ok()
            && p_assert.is_ok()
            && retract_nolayer.is_ok()
            && g_left == 1
            && p_left == 0
            && used_layer == project,
        &format!(
            "global_left={g_left} project_left={p_left} used_layer={used_layer} retract={retract_nolayer:?}"
        ),
    );

    // 19. NULL-layer edges stay invisible (search + get + ABOUT spike)
    let null_token = format!("v11-null-layer-token-{tag}");
    let null_s = format!("mindreader:element/v11-null-s-{tag}");
    let null_o = format!("mindreader:element/v11-null-o-{tag}");
    let null_sp = format!("mindreader:signal/v11-null-spike-{tag}");
    graph
        .run(
            neo4rs::query(
                r#"
                MERGE (s:Entity:Element {iri: $s})
                ON CREATE SET s.name = $sname, s.createdAt = datetime(), s.searchText = $token
                MERGE (o:Entity:Element {iri: $o})
                ON CREATE SET o.name = $oname, o.createdAt = datetime()
                CREATE (s)-[r:ASSERTS {
                    propertyIri: 'mindreader:property/note',
                    factText: $token,
                    validFrom: datetime()
                }]->(o)
                "#,
            )
            .param("s", null_s.clone())
            .param("sname", format!("v11-null-s-{tag}"))
            .param("o", null_o.clone())
            .param("oname", format!("v11-null-o-{tag}"))
            .param("token", null_token.clone()),
        )
        .await?;
    graph
        .run(
            neo4rs::query(
                r#"
                MERGE (sp:Entity:Signal {iri: $iri})
                ON CREATE SET sp.name = $name, sp.createdAt = datetime()
                WITH sp
                MATCH (el:Entity {iri: $bruno})
                CREATE (sp)-[a:ABOUT {
                    propertyIri: 'mindreader:property/ABOUT',
                    factText: $ft,
                    validFrom: datetime()
                }]->(el)
                "#,
            )
            .param("iri", null_sp.clone())
            .param("name", format!("v11-null-spike-{tag}"))
            .param("bruno", bruno_iri.clone())
            .param("ft", format!("null spike about bruno {tag}")),
        )
        .await?;
    let null_search = tools::memory_search(
        &graph,
        project,
        SearchArgs {
            text: Some(null_token.clone()),
            labels: None,
            limit: Some(20),
        },
    )
    .await;
    let null_get = tools::memory_get(
        &graph,
        project,
        GetArgs {
            iri: bruno_iri.clone(),
            hops: Some(1),
        },
    )
    .await;
    let null_wake = tools::memory_search(
        &graph,
        project,
        SearchArgs {
            text: Some("Bruno".into()),
            labels: None,
            limit: Some(30),
        },
    )
    .await;
    let search_hides = null_search
        .as_ref()
        .ok()
        .map(|v| {
            let facts = v
                .get("facts")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            facts.iter().all(|f| !f.to_string().contains(&null_token))
        })
        .unwrap_or(false);
    let get_hides = null_get
        .as_ref()
        .ok()
        .map(|v| !v.to_string().contains(&null_sp))
        .unwrap_or(false);
    let spike_hides = null_wake
        .as_ref()
        .ok()
        .map(|v| {
            let spikes = v
                .get("spike")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            spikes.iter().all(|s| !s.to_string().contains(&null_sp))
        })
        .unwrap_or(false);
    r.check(
        19,
        "NULL-layer edges stay invisible (search facts, get hops, ABOUT spike)",
        search_hides && get_hides && spike_hides,
        &format!(
            "search_hides={search_hides} get_hides={get_hides} spike_hides={spike_hides} search={null_search:?}"
        ),
    );

    // 20. find_current closes ALL current (s,p,layer) on supersede
    let multi_s = format!("mindreader:element/v11-multi-s-{tag}");
    let multi_a = format!("mindreader:element/v11-multi-a-{tag}");
    let multi_b = format!("mindreader:element/v11-multi-b-{tag}");
    graph
        .run(
            neo4rs::query(
                r#"
                MERGE (s:Entity:Element {iri: $s}) ON CREATE SET s.name = $sname, s.createdAt = datetime()
                MERGE (a:Entity:Element {iri: $a}) ON CREATE SET a.name = $aname, a.createdAt = datetime()
                MERGE (b:Entity:Element {iri: $b}) ON CREATE SET b.name = $bname, b.createdAt = datetime()
                CREATE (s)-[:ASSERTS {
                    propertyIri: 'mindreader:property/worksOn',
                    layer: $layer,
                    validFrom: datetime(),
                    factText: 'multi a'
                }]->(a)
                CREATE (s)-[:ASSERTS {
                    propertyIri: 'mindreader:property/worksOn',
                    layer: $layer,
                    validFrom: datetime(),
                    factText: 'multi b'
                }]->(b)
                "#,
            )
            .param("s", multi_s.clone())
            .param("sname", format!("v11-multi-s-{tag}"))
            .param("a", multi_a.clone())
            .param("aname", format!("v11-multi-a-{tag}"))
            .param("b", multi_b.clone())
            .param("bname", format!("v11-multi-b-{tag}"))
            .param("layer", project.to_string()),
        )
        .await?;
    let (before_n, before_objs) =
        tools::count_current_asserts(&graph, &multi_s, "worksOn", project).await?;
    let multi_c = tools::memory_assert(
        &graph,
        project,
        AssertArgs {
            s: json!({"iri": multi_s}),
            p: "worksOn".into(),
            o: json!({"name": format!("v11-multi-c-{tag}"), "labels": ["Element"]}),
            layer: Some(project.to_string()),
            spike: None,
            contradicts: false,
        },
    )
    .await;
    let c_iri = multi_c
        .as_ref()
        .ok()
        .and_then(|v| {
            v.get("o")
                .and_then(|o| o.get("iri"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or("")
        .to_string();
    let (after_n, after_objs) =
        tools::count_current_asserts(&graph, &multi_s, "worksOn", project).await?;
    let hist_multi = tools::count_historical_asserts(&graph, &multi_s, "worksOn", project).await?;
    r.check(
        20,
        "assert supersede closes ALL current (s,p,layer) matches, not LIMIT 1",
        before_n == 2
            && multi_c.is_ok()
            && after_n == 1
            && hist_multi >= 2
            && after_objs.iter().any(|o| o == &c_iri)
            && !after_objs.iter().any(|o| o == &multi_a || o == &multi_b),
        &format!(
            "before={before_n} {before_objs:?} after={after_n} {after_objs:?} hist={hist_multi} c={multi_c:?}"
        ),
    );

    Ok(r.failed)
}

async fn count_contradicts(graph: &neo4rs::Graph, from_iri: &str) -> Result<i64> {
    let row = mindreader::graph::fetch_one(
        graph,
        neo4rs::query(
            r#"
            MATCH (n:Entity {iri: $iri})-[r:CONTRADICTS]->(o)
            WHERE r.validTo IS NULL
            RETURN count(r) AS n
            "#,
        )
        .param("iri", from_iri.to_string()),
    )
    .await?;
    Ok(row.and_then(|r| r.get::<i64>("n").ok()).unwrap_or(0))
}

fn load() {
    mindreader::config::load_env();
}
