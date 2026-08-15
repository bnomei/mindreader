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
    if Mindreader::registered_tool_names().len() != 6 {
        return Err(anyhow!("expected 6 registered tools"));
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
                && (text.contains(&project_iri) || text.contains("worksOn") || text.contains("graph-memory"))
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
            v.get("nodes")
                .and_then(|n| n.as_array())
                .map(|arr| {
                    arr.iter().any(|n| {
                        n.get("iri")
                            .and_then(|x| x.as_str())
                            .map(|s| s == bruno_iri || s.contains("bruno"))
                            .unwrap_or(false)
                            || n.get("name")
                                .and_then(|x| x.as_str())
                                .map(|s| s.eq_ignore_ascii_case("Bruno"))
                                .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);
    r.check(5, "memory_search \"Bruno\"", found_bruno, &format!("{search:?}"));

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
    r.check(6, "memory_traverse from Bruno depth 2", trav_ok, &format!("{trav:?}"));

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
        },
    )
    .await;
    let (cur_n2, cur_objs2) =
        tools::count_current_asserts(&graph, &bruno_iri, "worksOn", "project:graph-memory").await?;
    let hist = tools::count_historical_asserts(&graph, &bruno_iri, "worksOn", "project:graph-memory")
        .await?;
    let other_iri = other
        .as_ref()
        .ok()
        .and_then(|v| v.get("o").and_then(|o| o.get("iri")).and_then(|x| x.as_str()))
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

    Ok(r.failed)
}

fn load() {
    mindreader::config::load_env();
}
