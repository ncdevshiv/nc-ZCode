import io

PATH = "crates/zcode-workflow/src/bin/workflow_engine_bench.rs"
s = io.open(PATH, encoding="utf-8").read()

ANCHOR = """    // Compile-once row: the Rust host drives the SAME TypeScript compiler through a
    // persistent Node kernel"""

ROW = """    // Authoring-surface row: the cost of the preview card's projection on top of a compile. This is
    // the operation `CreateWorkflow` performs per script, so it belongs beside the compile rows.
    // The TypeScript baseline for the same question is `analyze` through the resident kernel, and
    // specs/tool-benchmark.md carries it as a frozen pre-cutover number (warm 59 ms, one-time spawn
    // 390 ms) that cannot be re-measured here; it is also not the same operation — that analyze runs
    // a JavaScript taint analysis, which is precisely the work this tree walk replaces.
    {
        use zcode_workflow::script::graph::project_analysis;
        let mut samples: Vec<f64> = Vec::with_capacity(runs * 2);
        let mut step_counts: Vec<usize> = Vec::with_capacity(runs * 2);
        for name in ["bench_linear", "bench_fanout"] {
            let text =
                std::fs::read_to_string(fixtures_dir.join(format!("{name}.nws"))).expect("fixture");
            for _ in 0..runs {
                let started = Instant::now();
                let plan = Plan::compile(&text).expect("NWS fixture compiles");
                let analysis = project_analysis(&plan);
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                step_counts.push(
                    analysis["causality"]["steps"]
                        .as_array()
                        .map_or(0, |steps| steps.len()),
                );
            }
        }
        // The wide case is what the caps are for: one lane, seventy awaits.
        let mut wide = String::from("workflow \\"wide\\" {\\n  let a = agent(\\"a\\");\\n");
        for round in 1..=70 {
            wide.push_str(&format!(
                "  let x{round} = await a.ask(\\"question {round}\\");\\n"
            ));
        }
        wide.push_str("}\\n");
        let wide_started = Instant::now();
        let wide_plan = Plan::compile(&wide).expect("wide compiles");
        let wide_analysis = project_analysis(&wide_plan);
        let wide_ms = wide_started.elapsed().as_secs_f64() * 1000.0;
        compile_results.push(json!({
            "runtime": "nws-authoring-projection",
            "fixtures": ["bench_linear", "bench_fanout"],
            "stepsPerFixture": step_counts,
            "compilePlusProjectMedianMs": (median(&mut samples.clone()) * 1000.0).round() / 1000.0,
            "compilePlusProjectMinMs": (samples.first().copied().unwrap_or(0.0) * 1000.0).round()
                / 1000.0,
            "wideSeventyAwaitsMs": (wide_ms * 1000.0).round() / 1000.0,
            "wideSteps": wide_analysis["causality"]["steps"]
                .as_array()
                .map_or(0, |steps| steps.len()),
            "needsNode": false,
        }));
    }

""" + ANCHOR

assert s.count(ANCHOR) == 1
io.open(PATH, "w", encoding="utf-8", newline="\n").write(s.replace(ANCHOR, ROW))
print("ok")
