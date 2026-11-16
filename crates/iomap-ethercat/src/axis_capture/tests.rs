use project::{EthercatBringup, EthercatChannel, EthercatDcSync};

use super::*;

const PERIOD: u64 = 2_000_000;

fn config() -> EthercatConfig {
    let offsets = [0, 2, 2, 4, 10, 8, 0];
    EthercatConfig {
        nic: "_sim".into(),
        bringup: EthercatBringup::Auto,
        scan_sync: true,
        cycle_us: 2_000,
        dc_sync: EthercatDcSync::Sync0,
        dc_static_sync_iterations: 0,
        slaves: Vec::new(),
        channels: FIELDS
            .iter()
            .zip(offsets)
            .map(
                |(&(name, index, direction, data_type, bit_length), pdi_byte_offset)| {
                    EthercatChannel {
                        name: name.into(),
                        slave_index: 2,
                        direction,
                        pdo_index: index,
                        sub_index: 0,
                        bit_length,
                        data_type,
                        pdi_byte_offset,
                        pdi_bit_offset: 0,
                    }
                },
            )
            .collect(),
        gear: Vec::new(),
    }
}

fn identity() -> SlaveIdentity {
    SlaveIdentity {
        index: 2,
        name: "simulated CiA 402 axis".into(),
        vendor_id: 42,
        product_id: 43,
        revision: None,
        serial: None,
        input_bytes: 14,
        output_bytes: 6,
    }
}

fn open(recorder: &Recorder) -> Session<'_> {
    recorder.open_session(3, vec![identity()]).unwrap()
}

fn request(generation: u64, max_samples: usize) -> Start {
    Start {
        runtime_id: "runtime".into(),
        bus_generation: generation,
        slave: 2,
        counts_per_revolution: NonZeroU32::new(1_000).unwrap(),
        max_samples,
        duration_ms: 10_000,
    }
}

fn good_reply(dc_ns: u64) -> Reply {
    Reply::Received {
        dc_ns,
        wkc: 3,
        all_op: true,
        feedback: Some(Feedback {
            statusword: 0x27,
            position_raw: -55,
            ..Feedback::default()
        }),
    }
}

fn record(session: &Session<'_>, sequence: u64, reply: Reply) {
    let tx = Instant::now();
    let attempt = session.begin(sequence, tx).unwrap();
    session.finish(
        attempt,
        tx,
        tx + Duration::from_micros(100),
        None,
        Some(Command::default()),
        reply,
    );
}

#[test]
fn pairs_selected_output_with_fresh_input_and_freezes_at_capacity() {
    let recorder = Recorder::new("bus", &config(), true).unwrap();
    let session = open(&recorder);
    let progress = recorder.start(request(session.generation, 2)).unwrap();
    let id = &progress.capture_id;
    assert_eq!(progress.samples, 0);
    assert!(matches!(
        recorder.frozen(id),
        Err(CaptureError::Conflict(_))
    ));
    assert!(matches!(
        recorder.start(request(session.generation, 4)),
        Err(CaptureError::Conflict(_))
    ));

    let tx = Instant::now();
    let attempt = session.begin(6, tx).unwrap();
    // An output frame different from the previously requested target,
    // with a signed position and full-width status/error words.
    let command = attempt.command(&[0x0f, 0, 0xf9, 0xff, 0xff, 0xff]).unwrap();
    let feedback = attempt
        .feedback(&[
            0x34, 0x12, 0x27, 0x80, 0, 0, 0, 0x80, 0xec, 0xff, 0xfd, 0xff, 0xff, 0xff,
        ])
        .unwrap();
    session.finish(
        attempt,
        tx,
        tx + Duration::from_micros(15),
        Some(5),
        Some(command),
        Reply::Received {
            dc_ns: 1,
            wkc: 3,
            all_op: true,
            feedback: Some(feedback),
        },
    );
    record(&session, 7, good_reply(1 + PERIOD));

    let frozen = recorder.frozen(id).unwrap();
    assert_eq!(frozen.end_reason, Some(EndReason::SampleLimit));
    assert_eq!(frozen.dropped_samples, 0);
    assert_eq!(frozen.scope.bus_generation, session.generation);
    assert_eq!(frozen.slave.index, 2);
    assert_eq!(frozen.samples.len(), 2);
    let first = &frozen.samples[0];
    assert_eq!(first.attempt, 6);
    assert_eq!(first.output_ticket, Some(5));
    assert_eq!(first.rx_monotonic_ns - first.tx_monotonic_ns, 15_000);
    assert_eq!(first.command.unwrap().target_raw, -7);
    let feedback = first.feedback.unwrap();
    assert_eq!(feedback.position_raw, i32::MIN);
    assert_eq!(feedback.torque_permille, -20);
    assert_eq!(feedback.following_error_counts, -3);
    assert_eq!(feedback.statusword, 0x8027);
    assert_eq!(feedback.error_code, 0x1234);
    assert!(session.begin(8, Instant::now()).is_none());
    assert_eq!(
        recorder.stop(id).unwrap().end_reason,
        Some(EndReason::SampleLimit)
    );
}

#[test]
fn marks_clock_discontinuities_and_rejects_stale_rx_data() {
    let recorder = Recorder::new("bus", &config(), true).unwrap();
    let session = open(&recorder);
    let progress = recorder.start(request(session.generation, 10)).unwrap();
    let replies = [
        good_reply(1),
        good_reply(1 + 2 * PERIOD),
        good_reply(1 + 2 * PERIOD),
        good_reply(1 + PERIOD),
        good_reply(0),
        Reply::Received {
            dc_ns: 1 + 4 * PERIOD,
            wkc: 2,
            all_op: true,
            feedback: Some(Feedback::default()),
        },
        Reply::Received {
            dc_ns: 1 + 5 * PERIOD,
            wkc: 3,
            all_op: false,
            feedback: Some(Feedback::default()),
        },
        Reply::Timeout,
        Reply::Failed,
        Reply::Received {
            dc_ns: 1 + 9 * PERIOD,
            wkc: 3,
            all_op: true,
            feedback: None,
        },
    ];
    for (i, reply) in replies.into_iter().enumerate() {
        record(&session, i as u64 + 1, reply);
    }
    let frozen = recorder.frozen(&progress.capture_id).unwrap();
    let rows = &frozen.samples;
    assert_eq!(
        rows.iter().map(|row| row.dc_gap).collect::<Vec<_>>(),
        [
            None,
            Some(1),
            Some(0),
            Some(0),
            None,
            None,
            None,
            None,
            None,
            None,
        ]
    );
    assert!(rows[4].dc_ns.is_none());
    assert_eq!(rows[5].wkc, Some(2));
    assert_eq!(rows[6].all_op, Some(false));
    assert_eq!(rows[7].wkc, None);
    assert_eq!(rows[8].dc_ns, None);
    assert_eq!(
        rows[5..]
            .iter()
            .map(|row| row.input_quality)
            .collect::<Vec<_>>(),
        [
            InputQuality::UnexpectedWkc,
            InputQuality::NotOperational,
            InputQuality::Timeout,
            InputQuality::ExchangeFailed,
            InputQuality::MissingPdi
        ],
    );
    assert!(rows[5..].iter().all(|row| row.feedback.is_none()));
    assert_eq!(frozen.dropped_samples, 0);
}

#[test]
fn contention_counts_drops_without_blocking_or_tainting_the_next_recording() {
    let recorder = Recorder::new("bus", &config(), true).unwrap();
    let session = open(&recorder);
    let first = recorder.start(request(session.generation, 10)).unwrap();
    {
        let _busy = recorder.state.lock();
        assert!(session.begin(1, Instant::now()).is_none());
    }
    let tx = Instant::now();
    let pending = session.begin(2, tx).unwrap();
    {
        let _busy = recorder.state.lock();
        session.finish(pending, tx, tx, None, None, Reply::Timeout);
    }
    let pending = session.begin(3, tx).unwrap();
    let first = recorder.stop(&first.capture_id).unwrap();
    assert_eq!(first.dropped_samples, 2);
    assert_eq!(first.samples, 0);

    let second = recorder.start(request(session.generation, 1)).unwrap();
    session.finish(pending, tx, tx, None, None, Reply::Timeout);
    record(&session, 4, good_reply(1));
    let frozen = recorder.frozen(&second.capture_id).unwrap();
    assert_eq!(frozen.samples[0].attempt, 4);
    assert_eq!(frozen.dropped_samples, 0);
}

#[test]
fn duration_limit_excludes_an_exchange_that_finishes_at_the_deadline() {
    let recorder = Recorder::new("bus", &config(), true).unwrap();
    let session = open(&recorder);
    let id = recorder
        .start(request(session.generation, 5))
        .unwrap()
        .capture_id;
    record(&session, 1, good_reply(1));
    let deadline = recorder.state.lock().recording.as_ref().unwrap().deadline;
    let tx = deadline - Duration::from_micros(1);
    let attempt = session.begin(2, tx).unwrap();
    session.finish(attempt, tx, deadline, None, None, Reply::Timeout);
    let frozen = recorder.frozen(&id).unwrap();
    assert_eq!(frozen.samples.len(), 1);
    assert_eq!(frozen.end_reason, Some(EndReason::DurationLimit));
}

#[test]
fn rewalk_retains_the_frozen_generation_and_requires_a_fresh_scope() {
    let recorder = Recorder::new("bus", &config(), true).unwrap();
    let first_bus = open(&recorder);
    let id = recorder
        .start(request(first_bus.generation, 8))
        .unwrap()
        .capture_id;
    record(&first_bus, 1, good_reply(1));
    first_bus.close(EndReason::BusSessionEnded);
    assert!(recorder.status().bus.is_none());
    let next_bus = open(&recorder);
    let frozen = recorder.frozen(&id).unwrap();
    assert_ne!(next_bus.generation, frozen.scope.bus_generation);
    assert_eq!(frozen.end_reason, Some(EndReason::BusSessionEnded));
    assert_eq!(frozen.samples.len(), 1);
    assert!(matches!(
        recorder.start(request(first_bus.generation, 1)),
        Err(CaptureError::Conflict(_))
    ));
    let next_id = recorder
        .start(request(next_bus.generation, 1))
        .unwrap()
        .capture_id;
    assert!(matches!(recorder.frozen(&id), Err(CaptureError::NotFound)));
    record(&next_bus, 2, good_reply(PERIOD + 1));
    assert_eq!(recorder.frozen(&next_id).unwrap().samples[0].dc_gap, None);
    assert_eq!(frozen.samples[0].attempt, 1);
}

#[test]
fn refuses_unsupported_maps_and_unbounded_requests() {
    let mut cfg = config();
    cfg.channels[POSITION].pdi_byte_offset = cfg.channels[STATUS].pdi_byte_offset;
    assert!(Recorder::new("bus", &cfg, true).is_none());
    let mut cfg = config();
    cfg.channels[TORQUE].data_type = EthercatDataType::U16;
    assert!(Recorder::new("bus", &cfg, true).is_none());
    let mut cfg = config();
    cfg.scan_sync = false;
    assert!(Recorder::new("bus", &cfg, true).is_none());

    let recorder = Recorder::new("bus", &config(), true).unwrap();
    let bus = open(&recorder);
    assert!(matches!(
        recorder.start(request(bus.generation, MAX_SAMPLES + 1)),
        Err(CaptureError::Invalid(_))
    ));
    let long = Start {
        duration_ms: MAX_DURATION_MS + 1,
        ..request(bus.generation, 10)
    };
    assert!(matches!(
        recorder.start(long),
        Err(CaptureError::Invalid(_))
    ));
    let too_short = SlaveIdentity {
        input_bytes: 13,
        ..identity()
    };
    let bus = recorder.open_session(3, vec![too_short]).unwrap();
    assert!(matches!(
        recorder.start(request(bus.generation, 5)),
        Err(CaptureError::Invalid(_))
    ));
}
