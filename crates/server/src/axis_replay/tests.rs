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

#[tokio::test]
async fn v2_exports_are_scored_or_excluded_by_the_v1_api() {
    use ironplc_bridge::axis_capture::{
        Capture, Command, EndReason, Exchange, Feedback, InputQuality, PdoField, ReplayImport,
        Scope, SlaveIdentity,
    };

    let input = request(vec![run("source", 30, 1)], vec![]);
    let original: super::ReplayRequest = serde_json::from_value(input).unwrap();
    let sampling = original.sampling;
    let run = original.runs.into_iter().next().unwrap();
    let scope = Scope {
        runtime_id: "sim-run".into(),
        bus_generation: 7,
        device: "sim-bus".into(),
        slave: 0,
    };
    let mut capture = Capture {
        capture_schema_version: 1,
        capture_id: "sim-run-7-1".into(),
        scope: scope.clone(),
        slave: SlaveIdentity {
            index: 0,
            name: "sim-axis".into(),
            vendor_id: 0,
            product_id: 0,
            revision: None,
            serial: None,
            input_bytes: 14,
            output_bytes: 6,
        },
        // The EtherCAT recorder tests cover the mapping. Replay takes
        // its already decoded numbers, so the metadata need not match.
        mapping: [PdoField {
            role: "test",
            direction: "tx_pdo",
            object: 0,
            sub_index: 0,
            offset_bytes: 0,
            width_bits: 0,
            signed: false,
        }; 7],
        sampling,
        simulated: true,
        max_samples: 130,
        duration_ms: 3_000,
        dropped_samples: 0,
        end_reason: Some(EndReason::SampleLimit),
        samples: run
            .samples
            .into_iter()
            .enumerate()
            .map(|(i, sample)| Exchange {
                attempt: i as u64 + 1,
                dc_ns: Some(sample.dc_ns),
                wkc: Some(sample.wkc),
                all_op: Some(true),
                input_quality: InputQuality::Fresh,
                command: Some(Command {
                    controlword: sample.controlword,
                    target_raw: (i32::MAX - 6).wrapping_add(sample.target_counts as i32),
                }),
                feedback: Some(Feedback {
                    statusword: sample.statusword,
                    position_raw: (i32::MAX - 6).wrapping_add(sample.position_counts as i32),
                    following_error_counts: sample.following_error_counts,
                    torque_permille: sample.torque_permille,
                    error_code: sample.error_code,
                }),
                ..Exchange::default()
            })
            .collect(),
    };
    let capture_id = capture.capture_id.clone();
    let import = |started: bool, normal_completion| {
        serde_json::from_value::<ReplayImport>(json!({
            "run_id": "offline-sim",
            "plc": {
                "capture_id": capture_id,
                "scope": scope,
                "request_consumed": true,
                "plc_started": started,
                "normal_completion": normal_completion,
                "scan_count_before": 1,
                "scan_count_after": 131,
                "terminal_reason": (!started).then_some("PLC declined the request"),
            },
            "parameter_readback": { "scope": scope, "values": run.parameter_readback },
            "profile": run.profile
        }))
        .unwrap()
    };
    let go = |capture: &Capture, import| {
        let payload =
            serde_json::to_vec(&capture.replay(import, run.source.clone()).unwrap()).unwrap();
        async move {
            let response = Router::new()
                .route("/api/axis-replay", route())
                .oneshot(post(Body::from(payload)))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 100_000).await.unwrap())
                .unwrap()
        }
    };
    let valid_import = import(true, true);
    let mut missing_data = import(true, true);
    missing_data.parameter_readback = None;
    assert!(capture.replay(missing_data, run.source.clone()).is_err());
    let mut other_run = import(true, true);
    other_run.plc.scope.bus_generation += 1;
    assert!(capture.replay(other_run, run.source.clone()).is_err());
    let mut other_axis = import(true, true);
    other_axis.parameter_readback.as_mut().unwrap().scope.slave += 1;
    assert!(capture.replay(other_axis, run.source.clone()).is_err());

    let result = go(&capture, valid_import).await;
    assert_eq!(result["runs"][0]["status"], "analyzed");
    assert_eq!(
        result["runs"][0]["metrics"]["legs"][0]["steady_speed_mean_rpm"],
        5.0
    );

    let mut reverse = import(true, true);
    reverse.profile = Some(ironplc_bridge::axis_replay::Profile::Single {
        command_rpm: -5.0,
        steady_slots: 120,
        settle_ns: 1_000_000_000,
        encoder_span: None,
    });
    let samples = capture.samples.clone();
    for row in &mut capture.samples {
        let command = row.command.as_mut().unwrap();
        command.target_raw = !command.target_raw;
        let feedback = row.feedback.as_mut().unwrap();
        feedback.position_raw = command.target_raw;
    }
    // Descending through the signed wrap has the same 5 rpm magnitude.
    let result = go(&capture, reverse).await;
    assert_eq!(result["runs"][0]["status"], "analyzed");
    assert_eq!(
        result["runs"][0]["metrics"]["legs"][0]["steady_speed_mean_rpm"],
        -5.0
    );
    capture.samples = samples;
    let mut variants: Vec<(_, &str)> = Vec::new();
    let mut gap = capture.samples.clone();
    for row in &mut gap[4..] {
        row.dc_ns = row.dc_ns.map(|dc| dc + u64::from(sampling.cycle_ns.get()));
    }
    variants.push((gap, "dc_discontinuity"));
    let mut clockless = capture.samples.clone();
    clockless[4].dc_ns = None;
    variants.push((clockless, "invalid_capture"));
    let mut low_wkc = capture.samples.clone();
    low_wkc[4].wkc = Some(1);
    low_wkc[4].input_quality = InputQuality::UnexpectedWkc;
    low_wkc[4].feedback = None;
    variants.push((low_wkc, "invalid_wkc"));
    let mut stale = capture.samples.clone();
    stale[4].input_quality = InputQuality::NotOperational;
    stale[4].feedback = None;
    variants.push((stale, "invalid_capture"));
    let mut missing_sample = capture.samples.clone();
    missing_sample.remove(4);
    variants.push((missing_sample, "invalid_capture"));
    for (samples, expected) in variants {
        capture.samples = samples;
        let result = go(&capture, import(true, true)).await;
        assert_eq!(result["runs"][0]["status"], expected);
        assert!(result["runs"][0]["metrics"].is_null());
    }

    capture.samples.truncate(4);
    for row in &mut capture.samples {
        row.command = Some(Command::default());
        row.feedback = Some(Feedback::default());
    }
    let result = go(&capture, import(false, false)).await;
    assert_eq!(result["runs"][0]["status"], "pre_enable_abort");
    assert_eq!(
        result["runs"][0]["terminal_reason"],
        "PLC declined the request"
    );
    assert!(result["runs"][0]["metrics"].is_null());
}
