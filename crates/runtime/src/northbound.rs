//! Northbound MQTT publisher — edge runtime → plant platform (supOS /
//! Tier0).
//!
//! MQTT only, by design: the platform ingests MQTT natively, and the
//! northbound link is a *data* path. Southbound control stays on the
//! device layer (OPC UA → DCS, EtherCAT, Modbus).
//!
//! Topics (prefix defaults to `ia2/<project>`):
//!   - `<prefix>/status`    retained `online` / `offline`, with an MQTT
//!     Last-Will so the platform sees crashes too, not just clean exits.
//!   - `<prefix>/snapshot`  every `publish_interval_ms`:
//!     `{"ts_us":…,"scan":…,"values":{"FT0202":12.7,"alarm_h":true,…}}`
//!     Values are decoded JSON numbers/bools (not the IDE's display
//!     strings), so platform-side JSON-path mapping is one hop.
//!   - `<prefix>/write`     subscribed only when `allow_write = true`:
//!     `{"name":"sp_flow","value":12.5}` → one-shot variable write,
//!     same semantics as POST /write (the program can overwrite next
//!     scan; use program logic to latch setpoints).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironplc_bridge::{ProgramHandle, VarSnapshot};
use project::MqttNorthbound;
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};

pub struct NorthboundCtx {
    pub config: MqttNorthbound,
    pub project_name: String,
    pub latest: Arc<Mutex<Option<VarSnapshot>>>,
    pub handle: ProgramHandle,
    /// Write-audit ring shared with the HTTP handlers — northbound
    /// writes are recorded with origin `mqtt` (ADR-0002 attribution).
    pub audit: crate::AuditLog,
}

/// Spawn the northbound task. Returns immediately; the task owns the
/// MQTT connection (rumqttc's event loop reconnects with backoff on its
/// own, so a broker restart just pauses publishing).
pub fn spawn(ctx: NorthboundCtx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(ctx))
}

fn qos_of(q: u8) -> QoS {
    match q {
        0 => QoS::AtMostOnce,
        _ => QoS::AtLeastOnce,
    }
}

async fn run(ctx: NorthboundCtx) {
    let cfg = &ctx.config;
    let client_id = if cfg.client_id.is_empty() {
        format!("ia2-{}", ctx.project_name)
    } else {
        cfg.client_id.clone()
    };
    let prefix = if cfg.topic_prefix.is_empty() {
        format!("ia2/{}", ctx.project_name)
    } else {
        cfg.topic_prefix.trim_end_matches('/').to_string()
    };
    let qos = qos_of(cfg.qos);

    let mut options = MqttOptions::new(client_id, &cfg.broker_host, cfg.broker_port);
    options.set_keep_alive(Duration::from_secs(15));
    options.set_last_will(LastWill::new(
        format!("{prefix}/status"),
        "offline",
        qos,
        true,
    ));
    if !cfg.username.is_empty() {
        options.set_credentials(&cfg.username, &cfg.password);
    }

    let (client, mut event_loop) = AsyncClient::new(options, 32);

    // Publisher half — periodic snapshot + retained birth message. Owned
    // by the runtime process; dies with it (no explicit join needed).
    let _publisher = {
        let client = client.clone();
        let prefix = prefix.clone();
        let latest = ctx.latest.clone();
        let interval = Duration::from_millis(cfg.publish_interval_ms.max(100) as u64);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Birth: retained online (delivery rides on the event loop).
            let _ = client
                .publish(format!("{prefix}/status"), qos, true, "online")
                .await;
            loop {
                tick.tick().await;
                let snap = latest.lock().expect("latest mutex").clone();
                let Some(snap) = snap else { continue };
                let payload = snapshot_json(&snap);
                if let Err(e) = client
                    .publish(format!("{prefix}/snapshot"), qos, false, payload)
                    .await
                {
                    tracing::debug!(%e, "northbound publish queue error");
                }
            }
        })
    };

    if cfg.allow_write {
        if let Err(e) = client.subscribe(format!("{prefix}/write"), qos).await {
            tracing::warn!(%e, "northbound write-topic subscribe failed");
        } else {
            tracing::info!(topic = %format!("{prefix}/write"), "northbound write topic enabled");
        }
    }

    tracing::info!(
        broker = %format!("{}:{}", cfg.broker_host, cfg.broker_port),
        prefix = %prefix,
        interval_ms = cfg.publish_interval_ms,
        allow_write = cfg.allow_write,
        "northbound mqtt starting"
    );

    // Event loop half — drives the connection; handles inbound writes.
    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Packet::Publish(p))) => {
                if cfg.allow_write && p.topic == format!("{prefix}/write") {
                    handle_write(&ctx, &p.payload).await;
                }
            }
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                tracing::info!("northbound mqtt connected");
                // (Re)assert retained online after every (re)connect.
                let _ = client
                    .publish(format!("{prefix}/status"), qos, true, "online")
                    .await;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(%e, "northbound mqtt connection error; retrying");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// `{"name": "...", "value": 12.5}` → one-shot variable write. The
/// variable's IEC type comes from the latest snapshot (REAL values are
/// encoded as IEEE-754 bits for the VM, matching /write semantics).
async fn handle_write(ctx: &NorthboundCtx, payload: &[u8]) {
    #[derive(serde::Deserialize)]
    struct WriteReq {
        name: String,
        value: serde_json::Value,
    }
    let req: WriteReq = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, "northbound write: bad payload");
            return;
        }
    };
    let type_name = ctx
        .latest
        .lock()
        .expect("latest mutex")
        .as_ref()
        .and_then(|s| s.vars.iter().find(|v| v.name == req.name))
        .map(|v| v.type_name.clone());
    let Some(type_name) = type_name else {
        tracing::warn!(name = %req.name, "northbound write: unknown variable");
        return;
    };
    let is_real = type_name.eq_ignore_ascii_case("REAL");
    let bits = if is_real {
        let Some(f) = req.value.as_f64() else {
            tracing::warn!(name = %req.name, "northbound write: REAL needs a number");
            return;
        };
        (f as f32).to_bits() as i32
    } else {
        match &req.value {
            serde_json::Value::Bool(b) => *b as i32,
            v => match v.as_i64() {
                Some(i) => i as i32,
                None => {
                    tracing::warn!(name = %req.name, "northbound write: expected int/bool");
                    return;
                }
            },
        }
    };
    let result = ctx.handle.write_variable(&req.name, bits).await;
    // The broker gets no result topic (northbound is a data path), so
    // the audit entry is the ONLY record of what the platform asked
    // versus what governance applied — record both sides.
    let (applied, outcome) = crate::write_audit_outcome(bits, &result);
    crate::record_audit(
        &ctx.audit,
        "mqtt",
        "write",
        &req.name,
        Some(bits),
        applied,
        &outcome,
    );
    match result {
        Ok(_) => {
            tracing::info!(name = %req.name, value = %req.value, outcome = %outcome, "northbound write applied");
        }
        Err(e) => {
            tracing::warn!(name = %req.name, %e, "northbound write failed");
        }
    }
}

/// VarSnapshot → compact JSON with typed values, decoded from the raw
/// VM slot bits by the shared monitor decoder (`monitor::typed_value`)
/// — the platform gets numbers/bools straight from the source of
/// truth, with the display string only as the STRING-type fallback.
fn snapshot_json(snap: &VarSnapshot) -> String {
    let mut values = serde_json::Map::with_capacity(snap.vars.len());
    for v in &snap.vars {
        values.insert(
            v.name.clone(),
            ironplc_bridge::monitor::typed_value(&v.type_name, v.bits, &v.value),
        );
    }
    serde_json::json!({
        "ts_us": snap.timestamp_us,
        "scan": snap.scan_count,
        "values": values,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironplc_bridge::VarValue;

    fn var(name: &str, type_name: &str, value: &str, bits: u64) -> VarValue {
        VarValue {
            name: name.into(),
            type_name: type_name.into(),
            value: value.into(),
            bits,
        }
    }

    #[test]
    fn snapshot_publishes_typed_values_from_bits() {
        let snap = VarSnapshot {
            timestamp_us: 42,
            scan_count: 7,
            vars: vec![
                var("run", "BOOL", "TRUE", 1),
                var("flow", "REAL", "12.5", 12.5f32.to_bits() as u64),
                var("mask", "WORD", "16#1637", 0x1637),
                var("delta", "DINT", "-42", (-42i32) as u32 as u64),
                var("label", "STRING", "'hello'", 0),
            ],
        };
        let json: serde_json::Value = serde_json::from_str(&snapshot_json(&snap)).unwrap();
        assert_eq!(json["ts_us"], 42);
        assert_eq!(json["scan"], 7);
        assert_eq!(json["values"]["run"], serde_json::json!(true));
        assert_eq!(json["values"]["flow"], serde_json::json!(12.5));
        assert_eq!(json["values"]["mask"], serde_json::json!(0x1637));
        assert_eq!(json["values"]["delta"], serde_json::json!(-42));
        assert_eq!(json["values"]["label"], serde_json::json!("'hello'"));
    }
}

#[cfg(test)]
mod governance_roundtrip_tests {
    use super::*;
    use ironplc_bridge::VarValue;
    use project::{WriteGovernance, WriteMode, WriteRule};
    use std::collections::HashMap;

    /// Spawn an embedded rumqttd v4 broker on a kernel-assigned port.
    /// The broker thread is leaked on purpose — the test process exits
    /// right after, same pattern as the iomap-modbus DemoSlave tests.
    fn broker_on_free_port() -> u16 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let cfg = rumqttd::Config {
            id: 0,
            router: rumqttd::RouterConfig {
                max_connections: 32,
                max_outgoing_packet_count: 200,
                max_segment_size: 1024 * 1024,
                max_segment_count: 10,
                custom_segment: None,
                initialized_filters: None,
                shared_subscriptions_strategy: Default::default(),
            },
            v4: Some(HashMap::from([(
                "v4".to_string(),
                rumqttd::ServerSettings {
                    name: "v4".into(),
                    listen: format!("127.0.0.1:{port}").parse().unwrap(),
                    tls: None,
                    next_connection_delay_ms: 1,
                    connections: rumqttd::ConnectionSettings {
                        connection_timeout_ms: 5000,
                        max_payload_size: 20480,
                        max_inflight_count: 100,
                        auth: None,
                        external_auth: None,
                        // Without this, rumqttd rejects subscriptions
                        // to filters that were not pre-initialized.
                        dynamic_filters: true,
                    },
                },
            )])),
            v5: None,
            ws: None,
            cluster: None,
            console: None,
            bridge: None,
            prometheus: None,
            metrics: None,
        };
        let mut broker = rumqttd::Broker::new(cfg);
        std::thread::spawn(move || {
            broker.start().expect("embedded MQTT broker failed");
        });
        // AsyncClient does not connect until its event loop is polled. The
        // observer stops on a connection error, so do not race broker startup.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}").parse().unwrap(),
                std::time::Duration::from_millis(100),
            )
            .is_ok()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "MQTT broker did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        port
    }

    fn f32_bits(v: f32) -> i32 {
        v.to_bits() as i32
    }

    type SeenMessages = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// The whole MQTT write path against REAL governance: an unlisted
    /// variable is denied, an out-of-range write is clamped, an
    /// in-range write applies — each recorded in the audit ring with
    /// origin "mqtt", requested and applied — and the platform gets NO
    /// reply on any topic (northbound is a data path; the audit entry
    /// is the only record). This is the one governed write path that
    /// had no automated proof (bench had no broker).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn governed_writes_over_mqtt_deny_clamp_apply_and_stay_silent() {
        let port = broker_on_free_port();

        let container = ironplc_bridge::compile(
            "PROGRAM main\n\
                VAR sp : REAL := 0.0; jog : REAL := 0.0; tick : INT := 0; END_VAR\n\
                tick := tick + 1;\n\
            END_PROGRAM",
        )
        .expect("test program compiles");
        let unit = ironplc_bridge::ProgramUnit {
            instance: "main".into(),
            task_name: "t".into(),
            interval_ms: 10,
            priority: 1,
            container,
            retain_vars: Vec::new(),
        };
        let governance = WriteGovernance {
            write_mode: WriteMode::Allowlist,
            rules: vec![WriteRule {
                variable: "sp".into(),
                min: Some(0.0),
                max: Some(10.0),
            }],
        };
        let handle =
            ironplc_bridge::spawn_units(vec![unit], Vec::new(), Vec::new(), None, governance);

        // Hand-built latest snapshot: northbound only consults it for
        // the variable's type (same construction as snapshot tests).
        let mk = |name: &str, tn: &str| VarValue {
            name: name.into(),
            type_name: tn.into(),
            value: "0".into(),
            bits: 0,
        };
        let latest = Arc::new(Mutex::new(Some(VarSnapshot {
            timestamp_us: 0,
            scan_count: 0,
            vars: vec![mk("sp", "REAL"), mk("jog", "REAL")],
        })));

        let audit: crate::AuditLog = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let ctx = NorthboundCtx {
            config: MqttNorthbound {
                enabled: true,
                broker_host: "127.0.0.1".into(),
                broker_port: port,
                client_id: String::new(),
                username: String::new(),
                password: String::new(),
                topic_prefix: String::new(),
                // Long interval: keep the snapshot topic quiet so the
                // no-reply assertion below is meaningful.
                publish_interval_ms: 120_000,
                qos: 0,
                allow_write: true,
            },
            project_name: "nbtest".into(),
            latest,
            handle: handle.clone(),
            audit: Arc::clone(&audit),
        };
        let _nb = spawn(ctx);

        // Independent test client subscribed to EVERYTHING under the
        // project prefix; its collected messages drive both the sync
        // (retained online status = northbound is up) and the
        // no-reply assertion.
        let mut opts = MqttOptions::new("nbtest-probe", "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut eventloop) = AsyncClient::new(opts, 16);
        let seen: SeenMessages = Arc::new(Mutex::new(Vec::new()));
        let seen_task = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        seen_task
                            .lock()
                            .expect("seen mutex")
                            .push((p.topic.clone(), p.payload.to_vec()));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        client
            .subscribe("ia2/nbtest/#", QoS::AtLeastOnce)
            .await
            .expect("subscribe");

        // Wait for northbound's retained "online" status.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let online = seen
                .lock()
                .expect("seen mutex")
                .iter()
                .any(|(t, p)| t == "ia2/nbtest/status" && p.as_slice() == b"online");
            if online {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "northbound never announced itself on the status topic"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        for payload in [
            r#"{"name":"jog","value":1.0}"#, // unlisted -> denied
            r#"{"name":"sp","value":50.0}"#, // out of range -> clamped to 10.0
            r#"{"name":"sp","value":5.0}"#,  // in range -> applied
        ] {
            client
                .publish("ia2/nbtest/write", QoS::AtLeastOnce, false, payload)
                .await
                .expect("publish");
        }

        // The audit ring is the contract: poll until all three land.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if audit.lock().expect("audit mutex").len() >= 3 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "northbound writes never reached the audit ring"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let entries: Vec<crate::AuditEntry> =
            audit.lock().expect("audit mutex").iter().cloned().collect();
        assert_eq!(entries.len(), 3, "{entries:?}");
        for e in &entries {
            assert_eq!(e.origin, "mqtt", "{e:?}");
            assert_eq!(e.op, "write", "{e:?}");
        }
        assert_eq!(entries[0].name, "jog");
        assert!(
            entries[0].result.contains("rejected by project governance"),
            "unlisted write must be denied: {:?}",
            entries[0]
        );
        assert_eq!(
            entries[0].applied, None,
            "denied write has no applied value"
        );
        assert_eq!(entries[1].name, "sp");
        assert_eq!(entries[1].requested, Some(f32_bits(50.0)));
        assert_eq!(
            entries[1].applied,
            Some(f32_bits(10.0)),
            "out-of-range write must clamp to the rule max"
        );
        assert_eq!(entries[1].result, "clamped");
        assert_eq!(entries[2].name, "sp");
        assert_eq!(entries[2].applied, Some(f32_bits(5.0)));
        assert_eq!(entries[2].result, "ok");

        // No reply, ever: beyond the data-path topics (retained status,
        // the periodic snapshot — one fires on connect) and the echo of
        // our own write publishes, NOTHING was published: no error
        // topic, no per-write result topic.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let stray: Vec<String> = seen
            .lock()
            .expect("seen mutex")
            .iter()
            .map(|(t, _)| t.clone())
            .filter(|t| {
                t != "ia2/nbtest/status" && t != "ia2/nbtest/snapshot" && t != "ia2/nbtest/write"
            })
            .collect();
        assert!(
            stray.is_empty(),
            "northbound must publish no reply/error topics for writes: {stray:?}"
        );

        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }
}
