//! Hold an actual Modbus reconnect past the scan thread's two-second grace.
//! No replacement may connect until the old worker's last write has finished.
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::http::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::run_handover_tests::{app_with, modbus_device, post_run, post_stop};

async fn check_handover(cancel_start: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let counts = connections.clone();
    let observed = writes.clone();
    let slave = tokio::spawn(async move {
        // Dropping the task also aborts its accepted connections.
        let mut clients = tokio::task::JoinSet::new();
        let mut held = Some(held);
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let index = counts.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                continue; // Fail the initial seed read and force a reconnect.
            }
            let hold = if index == 1 { held.take() } else { None };
            let writes = observed.clone();
            clients.spawn(async move {
                if let Some(hold) = hold {
                    let _ = hold.await;
                }
                loop {
                    let mut header = [0; 7];
                    if socket.read_exact(&mut header).await.is_err() {
                        break;
                    }
                    let mut request =
                        vec![0; u16::from_be_bytes([header[4], header[5]]) as usize - 1];
                    if socket.read_exact(&mut request).await.is_err() {
                        break;
                    }
                    let response = match request[0] {
                        1 => vec![1, 1, 0], // Seed read: coil is initially off.
                        5 => {
                            writes.lock().push((index, request[3] == 0xff));
                            request // Write-single-coil replies echo the request.
                        }
                        fc => panic!("unexpected Modbus function {fc}"),
                    };
                    header[4..6].copy_from_slice(&((response.len() + 1) as u16).to_be_bytes());
                    if socket.write_all(&header).await.is_err()
                        || socket.write_all(&response).await.is_err()
                    {
                        break;
                    }
                }
            });
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let mut device = modbus_device("field", addr);
    let project::ProtocolConfig::Modbus(config) = &mut device.config else {
        unreachable!()
    };
    config.timeout_ms = Some(15_000);
    let (app, state) = app_with(
        dir.path(),
        "PROGRAM main VAR x : BOOL; END_VAR x := TRUE; END_PROGRAM",
        device,
        Duration::from_millis(2500),
    );
    state
        .projects
        .lock()
        .active()
        .unwrap()
        .write_iomap(&project::IoMap {
            mappings: vec![project::Mapping {
                application: "main".into(),
                variable: "x".into(),
                direction: project::Direction::Output,
                device: "field".into(),
                channel: "c0".into(),
                unit: None,
                min: None,
                max: None,
                description: None,
            }],
        })
        .unwrap();
    assert_eq!(post_run(&app).await.0, StatusCode::OK);
    let first = super::run_handover_tests::current(&state);
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("old reconnect entered its seed read");
    tokio::time::timeout(Duration::from_millis(200), post_stop(&app))
        .await
        .expect("stop remains immediate");

    if cancel_start {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), post_run(&app))
                .await
                .is_err()
        );
    }
    // The scan thread is gone, but its detached reconnect worker still owns I/O.
    tokio::time::timeout(Duration::from_secs(5), first.shutdown())
        .await
        .unwrap();
    let (status, body) = post_run(&app).await;
    let count_before_release = connections.load(Ordering::SeqCst);
    let tracked = state.draining.lock().iter().any(|run| run.same_run(&first));
    // Always release and stop before asserting, including the unfixed case.
    release.send(()).unwrap();
    state
        .wait_until_drained(Duration::from_secs(5))
        .await
        .unwrap();
    post_stop(&app).await;
    state
        .wait_until_drained(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        count_before_release, 2,
        "replacement connected before old cleanup"
    );
    assert!(tracked, "a timeout/cancel must not forget the old worker");

    assert_eq!(post_run(&app).await.0, StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !writes.lock().iter().any(|&(run, on)| run >= 2 && on) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("replacement wrote the coil ON");
    post_stop(&app).await;
    state
        .wait_until_drained(Duration::from_secs(5))
        .await
        .unwrap();
    slave.abort();
    let writes = writes.lock();
    assert!(
        writes.contains(&(1, false)),
        "old worker performed its failsafe write"
    );
    let first_new = writes.iter().position(|&(run, on)| run >= 2 && on).unwrap();
    assert!(
        writes[first_new..].iter().all(|&(run, _)| run >= 2),
        "old worker wrote after the new run: {writes:?}"
    );
}

#[tokio::test]
async fn late_reconnect_finishes_before_the_next_run_can_write() {
    check_handover(false).await;
}

#[tokio::test]
async fn cancelled_start_does_not_release_a_late_reconnect() {
    check_handover(true).await;
}
