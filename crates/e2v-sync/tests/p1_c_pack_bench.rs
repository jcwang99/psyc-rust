use std::process::Command;

#[test]
fn p1_c_pack_bench_reports_p1_c_critical_paths() {
    let output = Command::new(env!("CARGO_BIN_EXE_p1_c_pack_bench"))
        .env("E2V_P1_C_BENCH_FILE_COUNT", "64")
        .env("E2V_P1_C_BENCH_MAX_COMPACTION_VERSIONS", "10")
        .output()
        .expect("benchmark binary should launch");

    assert!(
        output.status.success(),
        "benchmark command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("benchmark output should be utf-8");
    for metric in [
        "packed_push_ms=",
        "packed_clone_ms=",
        "pack_index_warmup_ms=",
        "cached_fetch_ms=",
        "compaction_path_ms=",
        "pack_range_reads=",
        "distinct_pack_paths=",
        "pack_range_reuse_verified=",
        "pack_remote_written=",
        "clone_head_verified=",
        "cached_fetch_without_remote_segments=",
        "root_segments=",
        "compaction_triggered=",
        "cache_reuse_verified=",
    ] {
        assert!(
            stdout.contains(metric),
            "expected benchmark output to contain {metric}, got:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("pack_remote_written=true"),
        "expected benchmark to verify packed push wrote remote pack paths, got:\n{stdout}"
    );
    assert!(
        stdout.contains("clone_head_verified=true"),
        "expected benchmark to verify packed clone restored the head object, got:\n{stdout}"
    );
    assert!(
        stdout.contains("cached_fetch_without_remote_segments=true"),
        "expected benchmark to verify cached fetch succeeds without remote pack-index segments, got:\n{stdout}"
    );
    assert!(
        stdout.contains("compaction_triggered=true"),
        "expected benchmark to trigger bounded L0 compaction, got:\n{stdout}"
    );
    assert!(
        stdout.contains("cache_reuse_verified=true"),
        "expected benchmark to verify cached pack-index reuse, got:\n{stdout}"
    );
    assert!(
        stdout.contains("pack_range_reuse_verified=true"),
        "expected benchmark to verify packed fetch reuses pack range reads, got:\n{stdout}"
    );
    assert!(
        extract_metric(&stdout, "pack_index_warmup_ms")
            .and_then(|value| value.parse::<u128>().ok())
            .is_some(),
        "expected benchmark to report a numeric pack_index_warmup_ms, got:\n{stdout}"
    );
    let pack_range_reads = extract_metric(&stdout, "pack_range_reads")
        .expect("benchmark should report pack_range_reads")
        .parse::<usize>()
        .expect("pack_range_reads should be numeric");
    let distinct_pack_paths = extract_metric(&stdout, "distinct_pack_paths")
        .expect("benchmark should report distinct_pack_paths")
        .parse::<usize>()
        .expect("distinct_pack_paths should be numeric");
    assert!(
        pack_range_reads >= distinct_pack_paths,
        "expected pack range reads to be at least the number of distinct pack paths, got reads={pack_range_reads} distinct={distinct_pack_paths}\n{stdout}"
    );

    let root_segments = extract_metric(&stdout, "root_segments")
        .expect("benchmark should report root_segments")
        .parse::<usize>()
        .expect("root_segments should be numeric");
    assert!(
        root_segments <= 4,
        "expected benchmark root segment count to stay bounded, got {root_segments}\n{stdout}"
    );
}

fn extract_metric<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout.split_whitespace().find_map(|field| {
        let (field_key, field_value) = field.split_once('=')?;
        (field_key == key).then_some(field_value)
    })
}
