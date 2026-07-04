use std::fs;

use e2v_sync::{
    RemoteDiagnosticsOptions, RemoteDiagnosticsReport, RemoteDiagnosticsScenario,
    run_remote_diagnostics,
};
use tempfile::tempdir;

fn file_remote_spec(path: &std::path::Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    };
    let mut normalized = absolute.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = normalized.strip_prefix("//?/") {
        normalized = stripped.to_string();
    }
    format!("file:///{normalized}")
}

#[test]
fn remote_diagnostics_full_reports_phase_timings_request_counts_and_bytes() {
    let temp = tempdir().unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&remote_root).unwrap();

    let report = run_remote_diagnostics(RemoteDiagnosticsOptions {
        remote_spec: file_remote_spec(&remote_root),
        scenario: RemoteDiagnosticsScenario::Full,
        password: "correct horse battery staple".to_string(),
        file_count: 1,
        payload_bytes: 32,
        force_single_writer_risk: false,
    })
    .unwrap();

    assert_eq!(report.scenario, RemoteDiagnosticsScenario::Full);
    assert_eq!(
        report
            .phases
            .iter()
            .map(|phase| phase.name.as_str())
            .collect::<Vec<_>>(),
        vec!["push_v1", "clone", "push_v2", "fetch"]
    );
    assert!(
        report.total_metrics.total_requests > 0,
        "full diagnostics should record remote requests"
    );
    assert!(
        report.total_metrics.bytes_sent > 0 || report.total_metrics.bytes_received > 0,
        "full diagnostics should record remote bytes"
    );
    assert!(
        report.total_metrics.unique_path_count() > 0,
        "full diagnostics should record touched remote paths"
    );

    for phase in &report.phases {
        assert!(
            phase.elapsed_ms <= report.elapsed_ms,
            "phase elapsed time should fit within total elapsed time"
        );
        assert!(
            phase.metrics.total_requests > 0,
            "phase {} should record remote requests",
            phase.name
        );
        assert!(
            !phase.summary.is_empty(),
            "phase {} should include a human-readable summary",
            phase.name
        );
    }
}

#[test]
fn remote_diagnostics_fetch_scenario_focuses_on_fetch_phase_only() {
    let temp = tempdir().unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&remote_root).unwrap();

    let report = run_remote_diagnostics(RemoteDiagnosticsOptions {
        remote_spec: file_remote_spec(&remote_root),
        scenario: RemoteDiagnosticsScenario::Fetch,
        password: "correct horse battery staple".to_string(),
        file_count: 1,
        payload_bytes: 16,
        force_single_writer_risk: false,
    })
    .unwrap();

    assert_eq!(report.scenario, RemoteDiagnosticsScenario::Fetch);
    assert_eq!(report.phases.len(), 1);
    assert_eq!(report.phases[0].name, "fetch");
    assert!(report.phases[0].metrics.total_requests > 0);
}

#[test]
fn remote_diagnostics_report_round_trips_as_json() {
    let temp = tempdir().unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&remote_root).unwrap();

    let report = run_remote_diagnostics(RemoteDiagnosticsOptions {
        remote_spec: file_remote_spec(&remote_root),
        scenario: RemoteDiagnosticsScenario::Push,
        password: "correct horse battery staple".to_string(),
        file_count: 1,
        payload_bytes: 8,
        force_single_writer_risk: false,
    })
    .unwrap();

    let json = serde_json::to_string_pretty(&report).unwrap();
    let decoded: RemoteDiagnosticsReport = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.scenario, RemoteDiagnosticsScenario::Push);
    assert_eq!(decoded.phases.len(), 1);
    assert!(decoded.phases[0].metrics.total_requests > 0);
}
