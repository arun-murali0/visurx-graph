use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use visurx_wasm::adapter::{encode_json, save_json};
use visurx_wasm::core::parse::parse_all;
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
    let output_root = PathBuf::from("public").join(&repo_name);

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
