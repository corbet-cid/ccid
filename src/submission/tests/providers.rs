use super::*;
use github::Decision;

fn reference() -> Value {
    json!({"repository":"owner/repo","workflow":"verify.yml","source_ref":"main","source_commit":SHA,"check":"linux","request_id":"fixture","platform":"ubuntu-24.04","workflow_digest":"workflow-sha256","manifest_digest":"manifest-sha256","config_digest":"config-sha256","dependency_snapshot":"deps-lock-sha256","toolchain":"rust-stable","environment":"linux-v1","secret_free":true,"free_eligible":true})
}
fn observation() -> Value {
    let mut run = reference();
    run["run_id"] = json!(7);
    run["status"] = json!("completed");
    run["conclusion"] = json!("success");
    run
}
#[test]
fn eligibility_requires_explicit_both_flags() {
    let mut r = reference();
    assert!(github::eligible(&r));
    r["secret_free"] = Value::Null;
    assert!(!github::eligible(&r));
    r["secret_free"] = json!(true);
    r["free_eligible"] = json!(false);
    assert!(!github::eligible(&r));
}
#[test]
fn exact_match_rejects_changed_commit_check_or_native_platform() {
    let reference = reference();
    for key in [
        "source_commit",
        "check",
        "dependency_snapshot",
        "toolchain",
        "request_id",
        "platform",
    ] {
        let mut run = observation();
        run[key] = json!("different");
        assert_eq!(
            github::find_exact(&reference, &[run]),
            Decision::Unavailable
        );
    }
    assert_eq!(
        github::find_exact(&reference, &[observation()]),
        Decision::Eligible
    );
}
#[test]
fn multiple_exact_runs_are_ambiguous() {
    assert_eq!(
        github::find_exact(&reference(), &[observation(), observation()]),
        Decision::Ambiguous
    );
}
#[test]
fn provider_availability_has_no_post_ambiguity() {
    for code in [None, Some(503), Some(599)] {
        assert_eq!(github::classify(code, false), Decision::Unavailable);
        assert_eq!(github::classify(code, true), Decision::Ambiguous);
    }
    assert_eq!(github::classify(Some(404), true), Decision::Ineligible);
}
#[test]
fn dispatch_accepts_only_run_id_bearing_200() {
    assert_eq!(
        github::dispatch_response(&reference(), &json!({"status_code":204})),
        Decision::Ambiguous
    );
    assert_eq!(
        github::dispatch_response(
            &reference(),
            &json!({"status_code":503,"workflow_run_id":9})
        ),
        Decision::Ambiguous
    );
    assert_eq!(
        github::dispatch_response(
            &reference(),
            &json!({"status_code":200,"workflow_run_id":8})
        ),
        Decision::Eligible
    );
    for id in [json!(true), json!(0), json!("8")] {
        assert_eq!(
            github::dispatch_response(
                &reference(),
                &json!({"status_code":200,"workflow_run_id":id})
            ),
            Decision::Ambiguous
        );
    }
}
#[test]
fn dispatch_rejects_mismatched_returned_metadata() {
    let mut run = observation();
    run["source_commit"] = json!("f".repeat(40));
    assert_eq!(
        github::dispatch_response(
            &reference(),
            &json!({"status_code":200,"workflow_run_id":8,"run":run})
        ),
        Decision::Ambiguous
    );
}
// Historical mutation tests now assert the stronger frozen boundary: neither
// dispatch nor cancellation can reach any network transport at all.
#[test]
fn dispatch_requests_details_and_never_retries_unknown_post() {
    assert!(github::GitHub::fixture()
        .request(
            "POST",
            "/repos/owner/repo/actions/workflows/verify.yml/dispatches"
        )
        .unwrap_err()
        .to_string()
        .contains("frozen"));
    assert_eq!(
        github::dispatch_response(&reference(), &json!({})),
        Decision::Ambiguous
    );
}
#[test]
fn cancel_server_error_is_ambiguous() {
    assert_eq!(github::classify(Some(503), true), Decision::Ambiguous);
    assert!(github::GitHub::fixture()
        .request("POST", "/repos/owner/repo/actions/runs/7/cancel")
        .is_err());
}
#[test]
fn active_run_must_be_terminally_cancelled_before_crow() {
    let state = json!({"phase":"cancel-intent"});
    let queued = json!({"status":"queued"});
    assert_eq!(
        routing::next_action(&state, Some(&queued), 1000.0, 120.0),
        "wait"
    );
    let cancelled = json!({"status":"completed","conclusion":"cancelled"});
    assert_eq!(
        routing::next_action(&state, Some(&cancelled), 1000.0, 120.0),
        "crow"
    );
}
#[test]
fn cancel_timeout_is_ambiguous_and_blocks_fallback() {
    assert_eq!(
        routing::next_action(&json!({"phase":"cancel-intent"}), None, 1000.0, 120.0),
        "uncertain"
    );
}
#[test]
fn cancel_identity_change_is_ambiguous() {
    let mut run = observation();
    run["source_commit"] = json!("a".repeat(40));
    assert!(!github::exact_match(&reference(), &run));
}
#[test]
fn genuine_failure_is_not_provider_fallback() {
    let mut run = observation();
    run["conclusion"] = json!("failure");
    assert_eq!(github::kind(&run), "failure");
    assert_eq!(
        routing::next_action(&json!({"phase":"gha"}), Some(&run), 1000.0, 120.0),
        "result"
    );
}
#[test]
fn terminal_success_is_reused_without_cancel() {
    assert_eq!(github::kind(&observation()), "success");
    assert_eq!(
        routing::next_action(&json!({"phase":"gha"}), Some(&observation()), 1000.0, 120.0),
        "result"
    );
}
#[test]
fn genuine_failure_and_external_cancellation_never_fall_back() {
    for conclusion in ["failure", "cancelled"] {
        assert_eq!(
            routing::next_action(
                &json!({"phase":"observed"}),
                Some(&json!({"status":"completed","conclusion":conclusion})),
                1000.0,
                120.0
            ),
            "result"
        );
    }
}
#[test]
fn only_confirmed_owned_cancel_can_fall_back() {
    for phase in ["cancel-intent", "gha-retired"] {
        assert_eq!(
            routing::next_action(
                &json!({"phase":phase}),
                Some(&json!({"status":"completed","conclusion":"cancelled"})),
                1000.0,
                120.0
            ),
            "crow"
        );
    }
}
#[test]
fn unknown_dispatch_cannot_be_repeated() {
    for phase in ["gha-intent", "crow-intent", "cancel-intent"] {
        assert_eq!(
            routing::next_action(&json!({"phase":phase}), None, 1000.0, 120.0),
            "uncertain"
        );
    }
}
#[test]
fn running_jobs_are_not_queue_timed_out() {
    assert_eq!(
        routing::next_action(
            &json!({"phase":"gha","submitted_at":0}),
            Some(&json!({"status":"in_progress"})),
            1000.0,
            120.0
        ),
        "wait"
    );
}
#[test]
fn cancelled_run_requires_jobs_without_execution_evidence() {
    assert!(routing::jobs_may_have_started(&json!([]), 7, SHA));
    let mut jobs = json!([{"run_id":7,"head_sha":SHA,"status":"completed","conclusion":"cancelled","started_at":null,"steps":[{"status":"completed","conclusion":"skipped","started_at":null,"completed_at":null}]}]);
    assert!(!routing::jobs_may_have_started(&jobs, 7, SHA));
    jobs[0]["steps"][0]["started_at"] = json!("timestamp");
    assert!(routing::jobs_may_have_started(&jobs, 7, SHA));
}
fn contract() -> Value {
    json!({"github":{"workflow":"verify.yml"}})
}
fn hosted() -> Value {
    json!({"id":7,"head_sha":SHA,"display_title":"ccid/key","path":".github/workflows/verify.yml","event":"workflow_dispatch","head_branch":"main"})
}
#[test]
fn wrong_event_or_ref_cannot_be_reused() {
    assert!(routing::validate_run(&hosted(), &contract(), SHA, "key", "main").is_ok());
    for key in ["event", "head_branch"] {
        let mut run = hosted();
        run[key] = json!("wrong");
        assert!(routing::validate_run(&run, &contract(), SHA, "key", "main").is_err());
    }
}
#[test]
fn wrong_source_does_not_publish_result_or_fall_back() {
    let mut run = hosted();
    run["head_sha"] = json!("a".repeat(40));
    assert!(routing::validate_run(&run, &contract(), SHA, "key", "main").is_err());
}
#[test]
fn malformed_extra_variable_is_not_silently_dropped() {
    assert!(submit::variables(&["malformed".into()]).is_err());
}
#[test]
fn unsupported_or_duplicate_inputs_cannot_bypass_mapped_routing() {
    for vars in [
        vec!["CHECKS=linux".into(), "CHECKS=windows".into()],
        vec!["CI_COMMIT_SHA=forged".into()],
    ] {
        assert!(submit::variables(&vars).is_err());
    }
}
#[test]
fn plan_has_no_network_mutation() {
    let d = tempfile::tempdir().unwrap();
    let args = cli::SubmitArgs {
        repo: d.path().into(),
        branch: "main".into(),
        expect_commit: None,
        workflows: vec!["ccid".into()],
        variables: vec!["CHECKS=linux".into()],
        provider: "github".into(),
        provider_wait: 0,
        queue_timeout: 120,
        rerun: false,
        cached_rerun: false,
    };
    assert!(routing::route(&config(d.path()), &args, true)
        .unwrap_err()
        .to_string()
        .contains("frozen"));
}

fn zip(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in files {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
fn receipt() -> Value {
    json!({"source_revision":SHA,"binary_sha256":sha(b"binary"),"target":"x86_64-unknown-linux-gnu","rustc":"rustc stable"})
}
fn tool_zip(receipt: &Value) -> Vec<u8> {
    zip(&[
        ("ccid", b"binary".to_vec()),
        ("receipt.json", encode(receipt).unwrap().into_bytes()),
    ])
}
#[test]
fn valid_exact_binary_receipt() {
    assert_eq!(
        github::tool_receipt(&tool_zip(&receipt()), SHA).unwrap(),
        receipt()
    );
}
#[test]
fn changed_source_platform_binary_and_missing_compiler_rejected() {
    for key in ["source_revision", "target", "binary_sha256", "rustc"] {
        let mut r = receipt();
        r[key] = json!("");
        assert!(github::tool_receipt(&tool_zip(&r), SHA).is_err());
    }
}
#[test]
fn extra_paths_rejected_without_extraction() {
    let data = zip(&[
        ("ccid", b"binary".to_vec()),
        ("receipt.json", encode(&receipt()).unwrap().into_bytes()),
        ("../escape", b"bad".to_vec()),
    ]);
    assert!(github::tool_receipt(&data, SHA).is_err());
}
#[test]
fn result_requires_exact_dependency_snapshot_and_successful_guard() {
    let expected = json!({"source_commit":SHA,"dependency_snapshot":"lock"});
    let r = json!({"source_commit":SHA,"dependency_snapshot":"lock","exit_code":0,"guard_status":0,"rustc":"stable","node":"","bun":""});
    assert!(github::result_receipt(
        &zip(&[("result.json", encode(&r).unwrap().into_bytes())]),
        &expected
    )
    .is_ok());
    for (key, value) in [
        ("dependency_snapshot", json!("wrong")),
        ("guard_status", json!(1)),
        ("exit_code", json!(false)),
    ] {
        let mut bad = r.clone();
        bad[key] = value;
        assert!(github::result_receipt(
            &zip(&[("result.json", encode(&bad).unwrap().into_bytes())]),
            &expected
        )
        .is_err());
    }
}
#[test]
fn public_release_url_rejects_credentials_and_untrusted_redirect_hosts() {
    for url in [
        "http://github.com/asset",
        "https://token@github.com/asset",
        "https://github.com.evil/asset",
        "https://github.com:444/asset",
        "https://evil.invalid/asset",
    ] {
        assert!(github::public_url(url).is_err());
    }
    assert!(github::public_url("https://release-assets.githubusercontent.com/asset").is_ok());
}
