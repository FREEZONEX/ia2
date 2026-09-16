//! Full-stack integration against the `_sim` bus: the adapter's SDO
//! client, PDO engine, NMT gating, failsafe contract and heartbeat
//! watchdog all run against the simulated slave — the same path a dev
//! machine or demo project uses.

use iocore::{ChannelValue, IoDevice};
use iomap_canopen::CanopenDevice;
use project::{CanopenAccess, CanopenChannel, CanopenConfig, CanopenDataType, CanopenTransport};
use std::time::Duration;

fn base_config() -> CanopenConfig {
    CanopenConfig {
        interface: "_sim".into(),
        node_id: 0x22,
        bitrate: None,
        poll_interval_ms: 40,
        heartbeat_timeout_ms: 400,
        start_on_connect: true,
        channels: vec![],
    }
}

fn ch(
    name: &str,
    index: u16,
    sub: u8,
    ty: CanopenDataType,
    access: CanopenAccess,
    transport: CanopenTransport,
) -> CanopenChannel {
    CanopenChannel {
        name: name.into(),
        index,
        sub_index: sub,
        data_type: ty,
        access,
        transport,
        failsafe: None,
    }
}

#[tokio::test]
async fn sdo_write_persists_in_the_node_and_polls_back() {
    let mut cfg = base_config();
    // Same object bound twice: a write channel and a read channel, so
    // the poll loop proves the value round-tripped through the slave's
    // object dictionary rather than just our own mirror.
    cfg.channels = vec![
        ch(
            "sp_w",
            0x2000,
            1,
            CanopenDataType::I16,
            CanopenAccess::Write,
            CanopenTransport::Sdo,
        ),
        ch(
            "sp_r",
            0x2000,
            1,
            CanopenDataType::I16,
            CanopenAccess::Read,
            CanopenTransport::Sdo,
        ),
    ];
    let mut dev = CanopenDevice::connect("servo".into(), &cfg).await.unwrap();

    dev.write_channel("sp_w", ChannelValue::I32(-1234))
        .await
        .unwrap();
    // Wait a couple of poll rounds for sp_r to refresh from the slave.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let v = dev.read_channel("sp_r").await.unwrap();
    assert_eq!(v.to_i32(), -1234);
    assert!(dev.is_healthy());
    dev.shutdown().await.unwrap();
}

#[tokio::test]
async fn rpdo_write_comes_back_on_the_tpdo() {
    let mut cfg = base_config();
    // Command rides RPDO1; the slave folds it into its dictionary and
    // its TPDO1 packs the same object back out — the classic
    // controlword/statusword shape, minus the state machine.
    cfg.channels = vec![
        ch(
            "cmd",
            0x6040,
            0,
            CanopenDataType::U16,
            CanopenAccess::Write,
            CanopenTransport::Rpdo {
                slot: 1,
                byte_offset: 0,
            },
        ),
        ch(
            "echo",
            0x6040,
            0,
            CanopenDataType::U16,
            CanopenAccess::Read,
            CanopenTransport::Tpdo {
                slot: 1,
                byte_offset: 0,
            },
        ),
    ];
    let mut dev = CanopenDevice::connect("servo".into(), &cfg).await.unwrap();
    // Give NMT start + first TPDO cycle a moment.
    tokio::time::sleep(Duration::from_millis(120)).await;

    dev.write_channel("cmd", ChannelValue::U16(0x000F))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let v = dev.read_channel("echo").await.unwrap();
    assert_eq!(v.to_i32(), 0x000F);
    dev.shutdown().await.unwrap();
}

#[tokio::test]
async fn pdos_stay_silent_without_nmt_start() {
    let mut cfg = base_config();
    cfg.start_on_connect = false; // node stays pre-operational
    cfg.channels = vec![ch(
        "echo",
        0x6041,
        0,
        CanopenDataType::U16,
        CanopenAccess::Read,
        CanopenTransport::Tpdo {
            slot: 1,
            byte_offset: 0,
        },
    )];
    let mut dev = CanopenDevice::connect("servo".into(), &cfg).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // No TPDO arrived — mirror still empty, read falls back to zero,
    // and the node's heartbeat (pre-op) keeps us healthy.
    let v = dev.read_channel("echo").await.unwrap();
    assert_eq!(v.to_i32(), 0);
    assert!(dev.is_healthy());
    dev.shutdown().await.unwrap();
}

#[tokio::test]
async fn writes_to_read_channels_are_rejected() {
    let mut cfg = base_config();
    cfg.channels = vec![ch(
        "pv",
        0x2001,
        0,
        CanopenDataType::F32,
        CanopenAccess::Read,
        CanopenTransport::Sdo,
    )];
    let mut dev = CanopenDevice::connect("io".into(), &cfg).await.unwrap();
    let err = dev
        .write_channel("pv", ChannelValue::Real(1.0))
        .await
        .unwrap_err();
    assert!(matches!(err, iocore::IoError::TypeMismatch { .. }));
    dev.shutdown().await.unwrap();
}

#[tokio::test]
async fn failsafe_writes_only_optin_channels() {
    let mut cfg = base_config();
    cfg.channels = vec![
        CanopenChannel {
            failsafe: Some(0.0),
            ..ch(
                "speed_cmd",
                0x60FF,
                0,
                CanopenDataType::I32,
                CanopenAccess::Write,
                CanopenTransport::Sdo,
            )
        },
        ch(
            "other_cmd",
            0x2002,
            0,
            CanopenDataType::U16,
            CanopenAccess::Write,
            CanopenTransport::Sdo,
        ),
        // Read-back bindings on the same objects.
        ch(
            "speed_rb",
            0x60FF,
            0,
            CanopenDataType::I32,
            CanopenAccess::Read,
            CanopenTransport::Sdo,
        ),
        ch(
            "other_rb",
            0x2002,
            0,
            CanopenDataType::U16,
            CanopenAccess::Read,
            CanopenTransport::Sdo,
        ),
    ];
    let mut dev = CanopenDevice::connect("servo".into(), &cfg).await.unwrap();

    dev.write_channel("speed_cmd", ChannelValue::I32(500))
        .await
        .unwrap();
    dev.write_channel("other_cmd", ChannelValue::U16(7))
        .await
        .unwrap();
    dev.enter_failsafe().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // speed came back to its failsafe 0; the non-opt-in object held.
    assert_eq!(dev.read_channel("speed_rb").await.unwrap().to_i32(), 0);
    assert_eq!(dev.read_channel("other_rb").await.unwrap().to_i32(), 7);
    dev.shutdown().await.unwrap();
}

#[tokio::test]
async fn heartbeat_loss_flips_health_and_recovery_restores_it() {
    let mut cfg = base_config();
    cfg.heartbeat_timeout_ms = 250;
    cfg.channels = vec![ch(
        "hb_mute", // the sim slave's test hook: write 1 → heartbeat stops
        0x5FFF,
        0,
        CanopenDataType::U8,
        CanopenAccess::Write,
        CanopenTransport::Sdo,
    )];
    let mut dev = CanopenDevice::connect("servo".into(), &cfg).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(dev.is_healthy(), "heartbeats flowing at connect");

    dev.write_channel("hb_mute", ChannelValue::U16(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!dev.is_healthy(), "watchdog should trip after mute");

    dev.write_channel("hb_mute", ChannelValue::U16(0))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(dev.is_healthy(), "heartbeat back → healthy again");
    dev.shutdown().await.unwrap();
}

/// A document that names one channel twice cannot be read unambiguously:
/// the adapter's table is keyed by name, so the earlier object would simply
/// cease to exist and every mapping onto that name would silently move to
/// the survivor — a different index, a different meaning, no error anywhere.
///
/// `_sim` connects without hardware, so this exercises the real connect path:
/// a config that would otherwise have come up healthy is refused instead.
#[tokio::test]
async fn a_repeated_channel_name_is_refused_at_connect() {
    let mut cfg = base_config();
    cfg.channels = vec![
        ch(
            "statusword",
            0x6041,
            0,
            CanopenDataType::U16,
            CanopenAccess::Read,
            CanopenTransport::Sdo,
        ),
        // Same name, a different object entirely.
        ch(
            "statusword",
            0x6064,
            0,
            CanopenDataType::I32,
            CanopenAccess::Read,
            CanopenTransport::Sdo,
        ),
    ];
    let msg = match CanopenDevice::connect("drive".into(), &cfg).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a repeated channel name must not connect"),
    };
    assert!(msg.contains("declared more than once"), "{msg}");
    assert!(msg.contains("statusword"), "{msg}");

    // Control: the same two objects under distinct names connect fine, so
    // the guard is about the NAME collision and nothing else.
    cfg.channels[1].name = "actual_position".into();
    let dev = CanopenDevice::connect("drive".into(), &cfg)
        .await
        .expect("distinct names connect");
    drop(dev);
}
