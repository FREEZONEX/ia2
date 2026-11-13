use std::convert::Infallible;
use std::task::Poll;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{
    analyze, route, ReplayRequest, Status, MAX_BODY_BYTES, MAX_COMPARISONS,
    MAX_CONCURRENT_REQUESTS, MAX_PARAMETERS_PER_RUN, MAX_READBACK_BYTES, MAX_RUNS,
};

fn run(id: &str, byte: u8, position_step: i64) -> Value {
    let samples: Vec<_> = (0..130)
        .map(|i| {
            let active = (3..123).contains(&i);
            let move_slots = (i - 3_i64).clamp(0, 120);
            json!({
                "dc_ns": i * 20_000_000 + 1,
                "position_counts": move_slots * position_step,
                "target_counts": move_slots,
                "following_error_counts": move_slots * (1 - position_step),
                "torque_permille": 0,
                "controlword": if active { 15 } else { 6 },
                "statusword": if active { 0x27 } else { 0x21 },
                "error_code": 0,
                "wkc": 3,
            })
        })
        .collect();
    json!({
        "id": id,
        "source": {"path": "fixture", "sha256": format!("{:064x}", id.as_bytes()[0])},
        "evidence": {
            "zero_capture_drops": true,
            "request_consumed": true,
            "plc_started": true,
            "normal_completion": true,
        },
        "cohort": "loopback",
        "profile": {"kind": "single", "command_rpm": 5, "steady_slots": 120, "settle_ns": 1_000_000_000_u64},
        "parameter_readback": {"feedforward": [byte], "another_gain": [10]},
        "samples": samples,
    })
}

fn request(runs: Vec<Value>, comparisons: Vec<Value>) -> Value {
    json!({
        "schema_version": 1,
        "sampling": {"cycle_ns": 20_000_000, "counts_per_revolution": 600, "expected_wkc": 3},
        "runs": runs,
        "comparisons": comparisons,
    })
}

fn post(body: Body) -> Request<Body> {
    Request::post("/api/axis-replay")
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn replay_reports_baseline_and_candidate_from_samples() {
    let comparison = json!({
        "id": "p",
        "parameter": "feedforward",
        "max_start_span_counts": 0,
        "max_relative_target_delta_counts": 0,
        "design": {"kind": "aba", "before": "a", "candidate": "b", "after": "c"},
    });
    let payload = request(
        vec![run("a", 30, 0), run("b", 40, 1), run("c", 30, 0)],
        vec![comparison],
    );
    let app = Router::new().route("/api/axis-replay", route());
    let res = app
        .oneshot(post(Body::from(payload.to_string())))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 100_000).await.unwrap();
    let result: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["runs"][1]["status"], "analyzed");
    assert_eq!(
        result["runs"][1]["metrics"]["legs"][0]["steady_speed_mean_rpm"],
        5.0
    );
    let direction = &result["comparisons"][0]["result"]["directions"][0];
    assert_eq!(
        direction["following_bias_counts"]["assessment"],
        "lower_than_references"
    );
    assert_eq!(direction["following_bias_counts"]["reduction_pct"], 100.0);
}

#[tokio::test]
async fn replay_limits_body_size_and_admits_only_two_uploads_at_a_time() {
    let app = Router::new().route("/api/axis-replay", route());
    let oversized = app
        .clone()
        .oneshot(post(Body::from(vec![b' '; MAX_BODY_BYTES + 1])))
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let mut uploads: Vec<_> =
        (0..MAX_CONCURRENT_REQUESTS)
            .map(|_| {
                Box::pin(app.clone().oneshot(post(Body::from_stream(
                    futures_util::stream::pending::<Result<Bytes, Infallible>>(),
                ))))
            })
            .collect();
    for upload in &mut uploads {
        assert!(matches!(futures_util::poll!(upload), Poll::Pending));
    }
    let res = app.clone().oneshot(post(Body::from("{}"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

    drop(uploads.pop());
    let res = app.oneshot(post(Body::from("{}"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn request_bounds_limit_run_comparison_and_readback_amplification() {
    let mut reference = run("a", 30, 0);
    let readback = |len, byte| json!(vec![byte; len]);
    reference["parameter_readback"]["feedforward"] = readback(MAX_READBACK_BYTES, 30);
    let mut candidate = run("b", 40, 1);
    candidate["parameter_readback"]["feedforward"] = readback(MAX_READBACK_BYTES, 40);
    let comparison = |id| {
        json!({
            "id": format!("pair-{id}"),
            "parameter": "feedforward",
            "max_start_span_counts": 0,
            "max_relative_target_delta_counts": 0,
            "design": {"kind": "pair", "reference": "a", "candidate": "b"},
        })
    };
    let mut runs = vec![reference, candidate];
    let mut extra = run("unused", 0, 0);
    extra["samples"] = json!([]);
    for i in 2..MAX_RUNS {
        extra["id"] = json!(format!("unused-{i}"));
        runs.push(extra.clone());
    }
    let comparisons = (0..MAX_COMPARISONS).map(comparison).collect();
    let payload = request(runs, comparisons);
    let evaluate = |value| analyze(serde_json::from_value(value).unwrap());
    let accepted = serde_json::to_value(evaluate(payload.clone()).unwrap()).unwrap();
    assert_eq!(accepted["runs"].as_array().unwrap().len(), MAX_RUNS);
    let compared = accepted["comparisons"].as_array().unwrap();
    assert_eq!(compared.len(), MAX_COMPARISONS);
    assert!(compared.iter().all(|row| row["result"].is_object()));

    let mut too_many_runs = payload.clone();
    extra["id"] = json!("one-too-many");
    too_many_runs["runs"].as_array_mut().unwrap().push(extra);
    assert!(evaluate(too_many_runs)
        .err()
        .unwrap()
        .contains("runs per request"));

    let mut too_many_comparisons = payload.clone();
    too_many_comparisons["comparisons"]
        .as_array_mut()
        .unwrap()
        .push(comparison(MAX_COMPARISONS));
    assert!(evaluate(too_many_comparisons)
        .err()
        .unwrap()
        .contains("comparisons per request"));

    let mut oversized_readback = payload.clone();
    oversized_readback["runs"][0]["parameter_readback"]["feedforward"] =
        readback(MAX_READBACK_BYTES + 1, 30);
    assert!(evaluate(oversized_readback)
        .err()
        .unwrap()
        .contains("readback exceeds"));

    let mut too_many_parameters = payload;
    let params = too_many_parameters["runs"][0]["parameter_readback"]
        .as_object_mut()
        .unwrap();
    for i in 0..MAX_PARAMETERS_PER_RUN {
        params.insert(format!("p-{i}"), json!([1]));
    }
    assert!(evaluate(too_many_parameters)
        .err()
        .unwrap()
        .contains("parameter readbacks"));
}

#[test]
fn incomplete_and_discontinuous_runs_cannot_produce_metrics() {
    let mut gap = run("g", 40, 1);
    gap["samples"].as_array_mut().unwrap().remove(10);
    let mut abort = run("a", 40, 1);
    abort["evidence"]["normal_completion"] = json!(false);
    abort["evidence"]["plc_started"] = json!(false);
    for sample in abort["samples"].as_array_mut().unwrap() {
        sample["controlword"] = json!(6);
        sample["statusword"] = json!(0x21);
    }
    let mut unknown = abort.clone();
    unknown["id"] = json!("unknown");
    unknown["evidence"]["plc_started"] = Value::Null;
    let mut bad_wkc = run("w", 40, 1);
    bad_wkc["samples"][11]["wkc"] = json!(2);
    let mut unconsumed = run("unconsumed", 40, 1);
    unconsumed["evidence"]["request_consumed"] = json!(false);
    let mut no_clock = run("no-clock", 40, 1);
    no_clock["samples"][0]["dc_ns"] = json!(0);
    let mut boundary_straddle = run("boundary-straddle", 40, 1);
    boundary_straddle["samples"][0]["dc_ns"] = json!(20_000_000 - 1);
    let r: ReplayRequest = serde_json::from_value(request(
        vec![
            gap,
            abort,
            unknown,
            bad_wkc,
            unconsumed,
            no_clock,
            boundary_straddle,
        ],
        vec![],
    ))
    .unwrap();
    let result = analyze(r).unwrap();
    for (run, status) in result.runs.iter().zip([
        Status::DcDiscontinuity,
        Status::PreEnableAbort,
        Status::Incomplete,
        Status::InvalidWkc,
        Status::Incomplete,
        Status::InvalidCapture,
        Status::DcDiscontinuity,
    ]) {
        assert_eq!(run.status, status);
        assert!(run.metrics.is_none());
    }
    assert_eq!(result.runs[0].capture.dc_missing_slots, 1);
    assert_eq!(result.runs[0].capture.wkc_mismatches, 0);
    assert_eq!(result.runs[5].capture.dc_discontinuities, 0);
    assert_eq!(result.runs[6].capture.dc_discontinuities, 1);
    assert_eq!(result.runs[6].capture.dc_missing_slots, 0);
}

#[test]
fn feedback_may_lag_but_the_enable_command_must_be_contiguous() {
    let mut delayed = run("delayed", 40, 1);
    delayed["samples"][2]["controlword"] = json!(15);
    delayed["samples"][123]["statusword"] = json!(0x27);
    let mut retried = run("retried", 40, 1);
    retried["samples"][1]["controlword"] = json!(15);
    let mut reversed_target = run("reversed-target", 40, 1);
    reversed_target["samples"][4]["target_counts"] = json!(-1);
    let payload = request(vec![delayed, retried, reversed_target], vec![]);
    let result = analyze(serde_json::from_value(payload).unwrap()).unwrap();
    assert_eq!(result.runs[0].status, Status::Analyzed);
    assert_eq!(result.runs[1].status, Status::InvalidProfile);
    assert!(result.runs[1].metrics.is_none());
    assert_eq!(result.runs[2].status, Status::InvalidProfile);
    assert!(result.runs[2].metrics.is_none());
}

#[test]
fn comparisons_exclude_mixed_profiles_and_multiple_parameter_changes() {
    let reference = run("a", 30, 0);
    let mut changed = run("b", 40, 1);
    changed["parameter_readback"]["another_gain"] = json!([20]);
    let mut mixed = run("c", 40, 1);
    mixed["profile"]["settle_ns"] = json!(500_000_000);
    let mut different_target = run("d", 40, 1);
    // Keep the 100 ms target differences identical while changing the
    // individual samples. The comparison must check the recorded slots.
    for i in (4..123).step_by(5) {
        different_target["samples"][i]["target_counts"] = json!(i - 2);
    }
    let pair = |id: &str| {
        json!({
            "id": id,
            "parameter": "feedforward",
            "max_start_span_counts": 10,
            "max_relative_target_delta_counts": 0,
            "design": {"kind": "pair", "reference": "a", "candidate": id},
        })
    };
    let payload = request(
        vec![reference, changed, mixed, different_target],
        vec![pair("b"), pair("c"), pair("d")],
    );
    let result =
        serde_json::to_value(analyze(serde_json::from_value(payload).unwrap()).unwrap()).unwrap();
    assert!(result["comparisons"][0]["result"].is_null());
    assert!(result["comparisons"][1]["result"].is_null());
    assert!(result["comparisons"][2]["result"].is_null());
    assert_eq!(result["runs"][3]["status"], "analyzed");
    assert!(result["comparisons"][0]["exclusion_reason"]
        .as_str()
        .unwrap()
        .contains("selected parameter"));
    assert!(result["comparisons"][1]["exclusion_reason"]
        .as_str()
        .unwrap()
        .contains("same cohort and motion profile"));
    assert!(result["comparisons"][2]["exclusion_reason"]
        .as_str()
        .unwrap()
        .contains("relative targets differ"));
}
