//! Native (non-wasm) test harness: point it at a zip under `public/` (or
//! any path), and it will run stages 1-3 (parse, semantic, resolve) plus
//! scan, printing a summary and saving each stage's output as JSON under
//! `public/<repo_name>/`, and mirroring the surviving (post-exclusion)
//! source files to disk so you can visually confirm what made it through
//! unzip+filter, not just trust the counts. Each phase times itself; a
//! full aggregate summary across all stages is deferred until stage 4
//! (cfg) and aggregate are built too.
//!
//! Usage: cargo run --bin test_pipeline -- public/dummy_branching.zip

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use visurx_wasm::adapter::{encode_json, save_json};
use visurx_wasm::core::cfg::analyze_all as analyze_cfg;
use visurx_wasm::core::parse::parse_all;
use visurx_wasm::core::references::collect_all as collect_references;
use visurx_wasm::core::resolve::resolve_all;
use visurx_wasm::core::semantic::extract_all;
use visurx_wasm::scan::scan;
use visurx_wasm::vfs::Vfs;

fn main() -> ExitCode {
    let pipeline_start = Instant::now();
    let mut phase_timings: Vec<(&str, Duration)> = Vec::new();

    let Some(zip_path) = env::args().nth(1) else {
        eprintln!("usage: test_pipeline <path-to-zip>");
        eprintln!("example: cargo run --bin test_pipeline -- public/dummy_branching.zip");
        return ExitCode::FAILURE;
    };

    let repo_name = Path::new(&zip_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown_repo".to_string());

    // Clean any stale output from a previous run against this same repo
    // name BEFORE anything else runs. Without this, a file that used to
    // survive exclusion/filtering but now correctly doesn't would just
    // linger from the old run — silently making a real fix look like it
    // didn't work, or a real regression look fine because old-good output
    // is still sitting there. Every run should start from a clean slate.
    let output_root = PathBuf::from("public").join(&repo_name);
    if output_root.exists() {
        if let Err(e) = fs::remove_dir_all(&output_root) {
            eprintln!(
                "warning: could not clean stale output at '{}': {e}",
                output_root.display()
            );
            eprintln!("         (continuing anyway — new output may mix with stale files)");
        }
    }

    let bytes = match fs::read(&zip_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("could not read '{zip_path}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let unzip_start = Instant::now();
    let vfs = match Vfs::from_zip_bytes(&bytes) {
        Ok(vfs) => vfs,
        Err(e) => {
            eprintln!("unzip failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    phase_timings.push(("unzip", unzip_start.elapsed()));

    println!("=== unzip ===");
    println!(
        "{} files after exclusion (.git/node_modules dropped at ingestion)",
        vfs.len()
    );
    println!("took {:?}", phase_timings.last().unwrap().1);

    // --- scan ---
    let scan_start = Instant::now();
    let scan_result = scan(&vfs, None);
    phase_timings.push(("scan", scan_start.elapsed()));

    println!("\n=== scan (json) ===");
    match encode_json(&scan_result) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("scan JSON wasn't valid UTF-8 (shouldn't happen): {e}"),
        },
        Err(e) => eprintln!("failed to encode scan result as JSON: {e}"),
    }
    println!("took {:?}", phase_timings.last().unwrap().1);

    // --- parse (stage 1) ---
    let paths: Vec<&str> = scan_result
        .parseable_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    let parse_start = Instant::now();
    let batch = parse_all(&vfs, paths.into_iter());
    let parse_elapsed = parse_start.elapsed();
    phase_timings.push(("parse (stage 1)", parse_elapsed));

    println!("\n=== parse (stage 1) ===");
    println!(
        "attempted:      {}",
        batch.outcomes.len() + batch.failures.len()
    );
    println!(
        "parsed cleanly: {}",
        batch
            .outcomes
            .iter()
            .filter(|o| !o.has_parse_errors())
            .count()
    );
    println!(
        "had diagnostics (recoverable): {}",
        batch
            .outcomes
            .iter()
            .filter(|o| o.has_parse_errors())
            .count()
    );
    println!(
        "failed outright (missing/non-utf8/panic): {}",
        batch.failures.len()
    );
    for failure in &batch.failures {
        println!("  - {failure}");
    }
    println!(
        "took {parse_elapsed:?} ({:.2}ms/file avg)",
        parse_elapsed.as_secs_f64() * 1000.0 / paths_len(&batch)
    );

    // --- write scan + parse output as real JSON files on disk ---
    match save_json(&scan_result, output_root.join("_scan.json")) {
        Ok(_) => println!("\nwrote {}", output_root.join("_scan.json").display()),
        Err(e) => eprintln!("failed to save scan result: {e}"),
    }

    match save_json(&batch, output_root.join("_parse_stage1.json")) {
        Ok(_) => println!("wrote {}", output_root.join("_parse_stage1.json").display()),
        Err(e) => eprintln!("failed to save parse batch: {e}"),
    }

    // --- semantic (stage 2) ---
    let paths: Vec<&str> = scan_result
        .parseable_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    let semantic_start = Instant::now();
    let semantic_batch = extract_all(&vfs, paths.into_iter());
    let semantic_elapsed = semantic_start.elapsed();
    phase_timings.push(("semantic (stage 2)", semantic_elapsed));

    println!("\n=== semantic (stage 2) ===");
    println!("symbols extracted: {}", semantic_batch.symbols.len());
    println!(
        "exported:          {}",
        semantic_batch.symbols.iter().filter(|s| s.exported).count()
    );
    println!("failed outright:   {}", semantic_batch.failures.len());
    for failure in &semantic_batch.failures {
        println!("  - {failure}");
    }
    let total_attempted = scan_result.parseable_files.len();
    let cfg_ok =
        total_attempted - semantic_batch.failures.len() - semantic_batch.cfg_unavailable.len();
    println!("cfg available:     {cfg_ok}/{total_attempted} files");
    for path in &semantic_batch.cfg_unavailable {
        println!("  (no cfg) {path}");
    }
    // Small per-kind breakdown so the console output is useful without
    // opening the JSON file.
    use visurx_wasm::core::semantic::symbol_classify::SymbolKind;
    for (label, kind) in [
        ("function", SymbolKind::Function),
        ("class", SymbolKind::Class),
        ("method", SymbolKind::Method),
        ("interface", SymbolKind::Interface),
        ("type_alias", SymbolKind::TypeAlias),
        ("enum", SymbolKind::Enum),
        ("variable", SymbolKind::Variable),
    ] {
        let count = semantic_batch
            .symbols
            .iter()
            .filter(|s| s.kind == kind)
            .count();
        if count > 0 {
            println!("  {label}: {count}");
        }
    }
    println!(
        "took {semantic_elapsed:?} ({:.2}ms/file avg)",
        semantic_elapsed.as_secs_f64() * 1000.0 / total_attempted.max(1) as f64
    );

    match save_json(&semantic_batch, output_root.join("_semantic_stage2.json")) {
        Ok(_) => println!(
            "wrote {}",
            output_root.join("_semantic_stage2.json").display()
        ),
        Err(e) => eprintln!("failed to save semantic batch: {e}"),
    }

    // --- resolve (stage 3) ---
    let paths: Vec<&str> = scan_result
        .parseable_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    let resolve_start = Instant::now();
    let path_aliases = scan_result
        .tsconfig
        .as_ref()
        .map(|t| t.path_aliases.clone())
        .unwrap_or_default();
    let resolve_batch = resolve_all(&vfs, &path_aliases, paths.into_iter());
    let resolve_elapsed = resolve_start.elapsed();
    phase_timings.push(("resolve (stage 3)", resolve_elapsed));

    println!("\n=== resolve (stage 3) ===");
    use visurx_wasm::core::resolve::ImportTarget;
    let local_count = resolve_batch
        .edges
        .iter()
        .filter(|e| matches!(e.target, ImportTarget::LocalFile(_)))
        .count();
    let external_count = resolve_batch
        .edges
        .iter()
        .filter(|e| matches!(e.target, ImportTarget::External(_)))
        .count();
    let unresolved_local_count = resolve_batch
        .edges
        .iter()
        .filter(|e| matches!(e.target, ImportTarget::UnresolvedLocal(_)))
        .count();
    let re_export_count = resolve_batch
        .edges
        .iter()
        .filter(|e| e.is_re_export)
        .count();
    println!("total import edges: {}", resolve_batch.edges.len());
    println!("  resolved to local file: {local_count}");
    println!("  external package:       {external_count}");
    println!("  unresolved local:       {unresolved_local_count}");
    println!("  (of which re-exports:   {re_export_count})");
    if unresolved_local_count > 0 {
        println!("  first few unresolved-local reasons (for diagnosis):");
        for edge in resolve_batch
            .edges
            .iter()
            .filter(|e| matches!(e.target, ImportTarget::UnresolvedLocal(_)))
            .take(5)
        {
            if let ImportTarget::UnresolvedLocal(reason) = &edge.target {
                println!(
                    "    {} imports '{}': {reason}",
                    edge.from_file, edge.specifier
                );
            }
        }
    }
    println!("failed outright:   {}", resolve_batch.failures.len());
    for failure in &resolve_batch.failures {
        println!("  - {failure}");
    }
    let fed_alias_count =
        visurx_wasm::core::resolve::tsconfig_aliases_to_resolver_alias(&path_aliases).len();
    if path_aliases.is_empty() {
        println!("tsconfig path_aliases: none declared for this repo");
    } else if fed_alias_count == 0 {
        println!(
            "tsconfig path_aliases: {} declared, but 0 usable (e.g. a bare '*' wildcard has no well-defined alias meaning — see resolve/mod.rs)",
            path_aliases.len()
        );
    } else {
        println!(
            "tsconfig path_aliases: {} declared, {fed_alias_count} actually fed into the resolver",
            path_aliases.len()
        );
    }
    println!(
        "took {resolve_elapsed:?} ({:.2}ms/file avg)",
        resolve_elapsed.as_secs_f64() * 1000.0 / total_attempted.max(1) as f64
    );

    match save_json(&resolve_batch, output_root.join("_resolve_stage3.json")) {
        Ok(_) => println!(
            "wrote {}",
            output_root.join("_resolve_stage3.json").display()
        ),
        Err(e) => eprintln!("failed to save resolve batch: {e}"),
    }

    // --- external dependency classification ---
    // Works correctly even without a package.json at all (package_data:
    // None) — everything non-builtin just falls into `undeclared`, which
    // in that case IS the repo's real dependency list, reverse-engineered
    // purely from what the code actually imports.
    let external_names = resolve_batch.edges.iter().filter_map(|e| match &e.target {
        ImportTarget::External(name) => Some(name.as_str()),
        _ => None,
    });
    let external_summary = visurx_wasm::core::resolve::external_classify::summarize_externals(
        external_names,
        scan_result.package_data.as_ref(),
    );

    println!("\n=== external dependency classification ===");
    println!(
        "node builtins:            {} distinct",
        external_summary.node_builtins.len()
    );
    println!(
        "declared dependencies:    {} distinct",
        external_summary.declared_dependencies.len()
    );
    println!(
        "declared devDependencies: {} distinct",
        external_summary.declared_dev_dependencies.len()
    );
    println!(
        "undeclared:               {} distinct",
        external_summary.undeclared.len()
    );
    if !external_summary.undeclared.is_empty() {
        println!("  undeclared names (real signal — not a builtin, not in package.json at all):");
        let mut undeclared: Vec<(&String, &usize)> = external_summary.undeclared.iter().collect();
        undeclared.sort_by(|a, b| b.1.cmp(a.1));
        for (name, count) in undeclared {
            println!(
                "    {name} ({count} import site{})",
                if *count == 1 { "" } else { "s" }
            );
        }
    }

    match save_json(
        &external_summary,
        output_root.join("_external_summary.json"),
    ) {
        Ok(_) => println!(
            "wrote {}",
            output_root.join("_external_summary.json").display()
        ),
        Err(e) => eprintln!("failed to save external summary: {e}"),
    }

    // --- references (stage 3b) ---
    let paths: Vec<&str> = scan_result
        .parseable_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    let references_start = Instant::now();
    let reference_batch =
        collect_references(&vfs, &semantic_batch, &resolve_batch, paths.into_iter());
    let references_elapsed = references_start.elapsed();
    phase_timings.push(("references (stage 3b)", references_elapsed));

    println!("\n=== references (stage 3b) ===");
    use visurx_wasm::core::references::ReferenceTarget;
    let same_file_calls = reference_batch
        .calls
        .iter()
        .filter(|c| matches!(c.target, ReferenceTarget::SameFile(_)))
        .count();
    let cross_file_calls = reference_batch
        .calls
        .iter()
        .filter(|c| matches!(c.target, ReferenceTarget::CrossFile(_)))
        .count();
    let unresolved_import_calls = reference_batch
        .calls
        .iter()
        .filter(|c| matches!(c.target, ReferenceTarget::UnresolvedImport(_)))
        .count();
    let unmatched_calls = reference_batch
        .calls
        .iter()
        .filter(|c| matches!(c.target, ReferenceTarget::Unmatched))
        .count();
    println!("total calls found:  {}", reference_batch.calls.len());
    println!("  same-file matched:    {same_file_calls}");
    println!("  cross-file matched:   {cross_file_calls}");
    println!("  unresolved import:    {unresolved_import_calls}  (matched an import, but its target is external/unresolved/a default import)");
    println!("  unmatched:            {unmatched_calls}  (no import or same-file symbol matched the name at all)");
    println!("total extends found: {}", reference_batch.extends.len());
    println!("failed outright:     {}", reference_batch.failures.len());
    for failure in &reference_batch.failures {
        println!("  - {failure}");
    }
    println!(
        "took {references_elapsed:?} ({:.2}ms/file avg)",
        references_elapsed.as_secs_f64() * 1000.0 / total_attempted.max(1) as f64
    );

    match save_json(
        &reference_batch,
        output_root.join("_references_stage3b.json"),
    ) {
        Ok(_) => println!(
            "wrote {}",
            output_root.join("_references_stage3b.json").display()
        ),
        Err(e) => eprintln!("failed to save reference batch: {e}"),
    }

    // --- cfg (stage 4) ---
    let paths: Vec<&str> = scan_result
        .parseable_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    let cfg_start = Instant::now();
    let cfg_batch = analyze_cfg(&vfs, &semantic_batch.symbols, paths.into_iter());
    let cfg_elapsed = cfg_start.elapsed();
    phase_timings.push(("cfg (stage 4)", cfg_elapsed));

    println!("\n=== cfg (stage 4) ===");
    println!("functions/methods analyzed: {}", cfg_batch.reports.len());
    println!("failed outright:             {}", cfg_batch.failures.len());
    for failure in &cfg_batch.failures {
        println!("  - {failure}");
    }
    if !cfg_batch.reports.is_empty() {
        let total_complexity: u64 = cfg_batch
            .reports
            .iter()
            .map(|r| r.cyclomatic_complexity as u64)
            .sum();
        let avg_complexity = total_complexity as f64 / cfg_batch.reports.len() as f64;
        let mut by_complexity: Vec<&_> = cfg_batch.reports.iter().collect();
        by_complexity.sort_by(|a, b| b.cyclomatic_complexity.cmp(&a.cyclomatic_complexity));
        println!("average complexity: {avg_complexity:.2}");
        println!("top 5 most complex:");
        for report in by_complexity.iter().take(5) {
            println!(
                "  {} — complexity {}",
                report.symbol_id, report.cyclomatic_complexity
            );
        }
    }
    println!(
        "took {cfg_elapsed:?} ({:.2}ms/file avg)",
        cfg_elapsed.as_secs_f64() * 1000.0 / total_attempted.max(1) as f64
    );

    match save_json(&cfg_batch, output_root.join("_cfg_stage4.json")) {
        Ok(_) => println!("wrote {}", output_root.join("_cfg_stage4.json").display()),
        Err(e) => eprintln!("failed to save cfg batch: {e}"),
    }

    // --- mirror surviving files to disk under public/<repo_name>/ ---
    let mirror_start = Instant::now();
    let mut written = 0usize;
    for path in vfs.paths() {
        let Some(bytes) = vfs.read(path) else {
            continue;
        };
        let out_path = output_root.join(path);
        if let Some(parent) = out_path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                eprintln!("could not create '{}': {e}", parent.display());
                continue;
            }
        }
        if let Err(e) = fs::write(&out_path, bytes) {
            eprintln!("could not write '{}': {e}", out_path.display());
            continue;
        }
        written += 1;
    }
    phase_timings.push(("mirror to disk", mirror_start.elapsed()));

    println!("\n=== mirrored to disk ===");
    println!("{written} file(s) written under {}", output_root.display());
    println!("took {:?}", phase_timings.last().unwrap().1);

    // --- aggregate timing summary across every phase ---
    let total_elapsed = pipeline_start.elapsed();
    println!("\n=== timing summary ===");
    for (label, duration) in &phase_timings {
        let pct = duration.as_secs_f64() / total_elapsed.as_secs_f64() * 100.0;
        println!("{label:<20} {duration:>10?}  ({pct:>5.1}%)");
    }
    println!("{:<20} {total_elapsed:>10?}  (100.0%)", "TOTAL");

    ExitCode::SUCCESS
}

fn paths_len(batch: &visurx_wasm::core::parse::ParseBatch) -> f64 {
    (batch.outcomes.len() + batch.failures.len()).max(1) as f64
}
