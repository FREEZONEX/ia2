//! OPC UA client IoDevice adapter — southbound link to an existing
//! DCS / PLC / gateway.
//!
//! Role: IA2 is the *supervisory* layer. The DCS below keeps base
//! regulatory control; IA2 reads PV tags and writes SP / command tags
//! over OPC UA. Classic OPC DA servers (COM/DCOM, Windows-only) are
//! reached through a DA→UA gateway (KEPServerEX, Matrikon UA Proxy…) —
//! this crate speaks UA only.
//!
//! Shape: one background poll task owns the tag mirror — every
//! `poll_interval_ms` it issues ONE bulk Read service call for all
//! readable channels (OPC UA reads N nodes per call, so 200 tags is
//! still one round-trip — subject to the server's `MaxNodesPerRead`,
//! which this adapter does not chunk against; a response that does not
//! match the request one-for-one is rejected rather than zipped, see
//! `pair_results`). `read_channel` returns the mirrored value;
//! `write_channel` performs a direct Write service call so command
//! errors surface immediately at the scan loop.
//!
//! Failsafe: deliberately *opt-in per channel*. On a supervisory layer
//! the safe default is to leave DCS tags untouched on shutdown (the DCS
//! holds authority); only channels with an explicit `failsafe` value
//! get written.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use iocore::{ChannelValue, HealthTracker, HealthTransition, IoDevice, IoError};
use opcua::client::{Client, ClientBuilder, IdentityToken, Session};
use opcua::crypto::SecurityPolicy;
use opcua::types::{
    AttributeId, DataValue, EndpointDescription, MessageSecurityMode, NodeId, ReadValueId,
    StatusCode, TimestampsToReturn, UserTokenPolicy, Variant, WriteValue,
};
use project::{OpcuaAccess, OpcuaAuth, OpcuaChannel, OpcuaConfig, OpcuaDataType};

/// Upper bound on the initial connect + seed read. `session_retry_limit`
/// is `-1` (reconnect forever — the DCS outlives us), so
/// `wait_for_connection()` never returns while the endpoint is down. We
/// must NOT block the runtime's scan-loop startup on one southbound link:
/// if the DCS / gateway isn't up within this window, `connect` returns a
/// device with an empty mirror and the background poll task + session
/// retry populate it once the endpoint comes up. Bounded so a momentarily
/// unreachable endpoint can't wedge the whole runtime.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive failed mirror refreshes before the device is flagged
/// unhealthy (one ERROR log per outage, not one per poll) — same
/// contract as the Modbus adapter. The session keeps retrying forever
/// either way; this only makes the outage *visible* to the bridge's
/// per-device health surface instead of silently serving stale tags.
const UNHEALTHY_AFTER_FAILURES: u32 = 3;

/// Resolved channel — NodeId parsed once at connect.
#[derive(Clone)]
struct ResolvedChannel {
    meta: OpcuaChannel,
    node: NodeId,
}

pub struct OpcuaDevice {
    name: String,
    channels: HashMap<String, ResolvedChannel>,
    /// Last-known value per readable channel, refreshed by the poll task.
    mirror: Arc<RwLock<HashMap<String, ChannelValue>>>,
    session: Arc<Session>,
    /// `false` once the poll task has seen `UNHEALTHY_AFTER_FAILURES`
    /// consecutive failed refreshes, `true` again on the first success.
    healthy: Arc<AtomicBool>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
    event_loop_task: Option<tokio::task::JoinHandle<StatusCode>>,
}

impl OpcuaDevice {
    pub async fn connect(name: String, config: &OpcuaConfig) -> Result<Self, IoError> {
        // The channel table below is keyed by name, so a name declared twice
        // does not collide — it overwrites, and the earlier channel ceases to
        // exist while every mapping onto that name silently moves to the
        // survivor. Refuse: a config that cannot be read unambiguously is not
        // one to run a plant on. `/api/project/validate` reports the same
        // thing at author time, but the edge runtime validates nothing — this
        // is the only check that always runs.
        let dups =
            project::duplicate_channel_names(config.channels.iter().map(|c| c.name.as_str()));
        if !dups.is_empty() {
            return Err(IoError::Connect(format!(
                "device '{name}': channel name declared more than once: {}",
                dups.join(", ")
            )));
        }
        let mut channels = HashMap::new();
        for ch in &config.channels {
            let node = NodeId::from_str(&ch.node_id).map_err(|e| {
                IoError::Connect(format!(
                    "channel '{}': bad node_id '{}': {e}",
                    ch.name, ch.node_id
                ))
            })?;
            channels.insert(
                ch.name.clone(),
                ResolvedChannel {
                    meta: ch.clone(),
                    node,
                },
            );
        }

        let mut client: Client = ClientBuilder::new()
            .application_name("IA2 runtime")
            .application_uri("urn:ia2:runtime")
            .product_uri("urn:ia2:runtime")
            // Site policy: UA endpoints on control networks (or DA→UA
            // gateway hops) commonly run SecurityPolicy None; certs are
            // a later iteration alongside Sign&Encrypt.
            .create_sample_keypair(false)
            .trust_server_certs(true)
            .session_retry_limit(-1) // reconnect forever; the DCS outlives us
            .client()
            .map_err(|e| IoError::Connect(format!("opcua client builder invalid: {e:?}")))?;

        let endpoint: EndpointDescription = (
            config.endpoint_url.as_str(),
            SecurityPolicy::None.to_str(),
            MessageSecurityMode::None,
            UserTokenPolicy::anonymous(),
        )
            .into();

        let identity = match &config.auth {
            OpcuaAuth::Anonymous => IdentityToken::Anonymous,
            OpcuaAuth::UserPassword { username, password } => {
                IdentityToken::UserName(username.clone(), password.clone().into())
            }
        };

        // Bounded initial connect. `session_retry_limit` is -1 (reconnect
        // forever — the DCS outlives us), so `connect_to_matching_endpoint`
        // blocks indefinitely while the endpoint is down, retrying the
        // transport internally. We must NOT let one unreachable southbound
        // link wedge the whole runtime's scan-loop startup, so bound it:
        // on timeout (or a hard connect error) we return `IoError::Connect`
        // and `connect_devices` skips this device and starts the scan loop
        // with the rest — exactly how an unreachable Modbus slave is
        // handled. A reachable endpoint connects well within the window.
        let (session, event_loop) = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            client.connect_to_matching_endpoint(endpoint, identity),
        )
        .await
        {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                return Err(IoError::Connect(format!(
                    "opcua connect {}: {e}",
                    config.endpoint_url
                )))
            }
            Err(_) => {
                return Err(IoError::Connect(format!(
                    "opcua endpoint {} unreachable within {}s",
                    config.endpoint_url,
                    CONNECT_TIMEOUT.as_secs()
                )))
            }
        };

        let event_loop_task = tokio::spawn(event_loop.run());
        let mirror = Arc::new(RwLock::new(HashMap::new()));
        let readable: Vec<ResolvedChannel> = channels.values().cloned().collect();

        // Confirm the session is live and seed the mirror with one bulk
        // read (so the first scan round sees real values and missing
        // NodeIds surface now). Bounded too — connect_to_matching_endpoint
        // returning OK means the transport is up, so this is fast.
        let seed = tokio::time::timeout(CONNECT_TIMEOUT, async {
            if !session.wait_for_connection().await {
                return Err(IoError::Connect(format!(
                    "opcua session to {} failed",
                    config.endpoint_url
                )));
            }
            bulk_read(&session, &readable).await
        })
        .await
        .map_err(|_| {
            IoError::Connect(format!(
                "opcua session to {} did not become ready within {}s",
                config.endpoint_url,
                CONNECT_TIMEOUT.as_secs()
            ))
        })??;
        {
            let mut m = mirror.write().expect("mirror poisoned");
            for (name, value) in seed {
                m.insert(name, value);
            }
        }
        tracing::info!(
            device = %name,
            endpoint = %config.endpoint_url,
            tags = channels.len(),
            poll_ms = config.poll_interval_ms,
            "opcua connected; tag mirror seeded"
        );

        // Background poll task — one bulk read per interval. It also owns
        // the health tracking: the session retries reconnection forever on
        // its own, but without this the bridge (and the monitor) would keep
        // reporting a dead DCS link as healthy while serving stale tags.
        let healthy = Arc::new(AtomicBool::new(true));
        let poll_task = {
            let session = session.clone();
            let mirror = mirror.clone();
            let device = name.clone();
            let interval = Duration::from_millis(config.poll_interval_ms.max(50) as u64);
            let mut health = HealthTracker::with_flag(UNHEALTHY_AFTER_FAILURES, healthy.clone());
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(interval);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    match bulk_read(&session, &readable).await {
                        Ok(values) => {
                            if health.record_success() == HealthTransition::Recovered {
                                tracing::info!(device = %device, "opcua device recovered; tag mirror refreshing again");
                            }
                            let mut m = mirror.write().expect("mirror poisoned");
                            for (name, value) in values {
                                m.insert(name, value);
                            }
                        }
                        Err(e) => {
                            // Session retry handles reconnection; keep
                            // last-known values and keep trying.
                            match health.record_failure() {
                                HealthTransition::BecameUnhealthy => {
                                    tracing::error!(
                                        device = %device,
                                        consecutive_failures = health.consecutive_failures(),
                                        error = %e,
                                        "opcua device unhealthy; serving last-known values until it recovers"
                                    );
                                }
                                _ if health.is_healthy() => {
                                    tracing::warn!(device = %device, %e, "opcua poll failed; serving last-known values");
                                }
                                _ => {
                                    tracing::debug!(device = %device, %e, "opcua poll still failing");
                                }
                            }
                        }
                    }
                }
            })
        };

        Ok(Self {
            name,
            channels,
            mirror,
            session,
            healthy,
            poll_task: Some(poll_task),
            event_loop_task: Some(event_loop_task),
        })
    }

    fn channel(&self, name: &str) -> Result<&ResolvedChannel, IoError> {
        self.channels
            .get(name)
            .ok_or_else(|| IoError::UnknownChannel(name.to_string()))
    }

    async fn write_node(&self, ch: &ResolvedChannel, value: ChannelValue) -> Result<(), IoError> {
        let variant = to_variant(value, ch.meta.data_type);
        let write = WriteValue {
            node_id: ch.node.clone(),
            attribute_id: AttributeId::Value as u32,
            index_range: Default::default(),
            value: DataValue::value_only(variant),
        };
        let results = self
            .session
            .write(&[write])
            .await
            .map_err(|e| IoError::Transport(format!("opcua write: {e}")))?;
        match results.first() {
            Some(code) if code.is_good() => Ok(()),
            Some(code) => Err(IoError::Transport(format!(
                "opcua write '{}' rejected: {code}",
                ch.meta.name
            ))),
            None => Err(IoError::Transport("opcua write: empty result".into())),
        }
    }
}

/// One Read service call for every channel; returns (name, value) pairs
/// for the ones that came back Good.
async fn bulk_read(
    session: &Arc<Session>,
    channels: &[ResolvedChannel],
) -> Result<Vec<(String, ChannelValue)>, IoError> {
    if channels.is_empty() {
        return Ok(Vec::new());
    }
    let reads: Vec<ReadValueId> = channels
        .iter()
        .map(|c| ReadValueId {
            node_id: c.node.clone(),
            attribute_id: AttributeId::Value as u32,
            index_range: Default::default(),
            data_encoding: Default::default(),
        })
        .collect();
    let results = session
        .read(&reads, TimestampsToReturn::Neither, 0.0)
        .await
        .map_err(|e| IoError::Transport(format!("opcua bulk read: {e}")))?;
    pair_results(channels, results)
}

/// Pair a Read response with the channels that asked for it, positionally —
/// which is the only correspondence the service defines.
///
/// The count is checked first. A `zip` alone would have accepted a short
/// response as a *success*: the tags past the cut keep whatever the mirror
/// held (0 for any tag whose very first read was already short), health stays
/// green because the call returned `Ok`, and nothing is logged. A PV frozen at
/// zero that the system insists is healthy is the worst shape a supervisory
/// read can take, so a response that does not line up with the request is
/// treated as a failed refresh instead: last-known values are served and the
/// health tracker flips after `UNHEALTHY_AFTER_FAILURES`.
///
/// The realistic cause is a server `MaxNodesPerRead` smaller than the tag
/// count — this adapter issues one Read for every readable channel — so the
/// message names it.
fn pair_results(
    channels: &[ResolvedChannel],
    results: Vec<DataValue>,
) -> Result<Vec<(String, ChannelValue)>, IoError> {
    if results.len() != channels.len() {
        return Err(IoError::Transport(format!(
            "opcua bulk read: asked for {} nodes, got {} results — the response cannot be \
             matched to the request (server MaxNodesPerRead below the tag count?)",
            channels.len(),
            results.len()
        )));
    }
    let mut out = Vec::with_capacity(results.len());
    for (ch, dv) in channels.iter().zip(results) {
        let good = dv.status.map(|s| s.is_good()).unwrap_or(true);
        if !good {
            tracing::debug!(tag = %ch.meta.name, status = ?dv.status, "opcua tag read not good; keeping last value");
            continue;
        }
        if let Some(variant) = dv.value {
            if let Some(value) = from_variant(&variant, ch.meta.data_type) {
                out.push((ch.meta.name.clone(), value));
            } else {
                tracing::debug!(tag = %ch.meta.name, got = ?variant.type_id(), "opcua tag type mismatch; skipping");
            }
        }
    }
    Ok(out)
}

/// Server variant → channel lane, honouring the *declared* channel type
/// (the server may legitimately report a wider/narrower numeric).
fn from_variant(v: &Variant, ty: OpcuaDataType) -> Option<ChannelValue> {
    fn as_f64(v: &Variant) -> Option<f64> {
        match v {
            Variant::Boolean(b) => Some(*b as i32 as f64),
            Variant::SByte(x) => Some(*x as f64),
            Variant::Byte(x) => Some(*x as f64),
            Variant::Int16(x) => Some(*x as f64),
            Variant::UInt16(x) => Some(*x as f64),
            Variant::Int32(x) => Some(*x as f64),
            Variant::UInt32(x) => Some(*x as f64),
            Variant::Int64(x) => Some(*x as f64),
            Variant::UInt64(x) => Some(*x as f64),
            Variant::Float(x) => Some(*x as f64),
            Variant::Double(x) => Some(*x),
            // Some servers hand back container-wrapped scalars; unwrap
            // rather than skipping the tag (seen in the wild and from
            // naive `set_value(DataValue)` implementations).
            Variant::Variant(inner) => as_f64(inner),
            Variant::DataValue(dv) => dv.value.as_ref().and_then(as_f64),
            _ => None,
        }
    }
    let n = as_f64(v)?;
    Some(match ty {
        OpcuaDataType::Bool => ChannelValue::Bool(n != 0.0),
        OpcuaDataType::U16 => ChannelValue::U16(n as i64 as u16),
        // Int16 is signed: carry it on the I32 lane so a negative tag
        // survives. As `U16` a -40 setpoint arrived as 65496 in any
        // variable wider than 16 bits.
        OpcuaDataType::I16 => ChannelValue::I32(n as i64 as i16 as i32),
        OpcuaDataType::I32 | OpcuaDataType::U32 => ChannelValue::I32(n as i64 as i32),
        OpcuaDataType::F32 => ChannelValue::Real(n as f32),
        // Double tags ride the 64-bit lane end to end (→ LREAL vars).
        OpcuaDataType::F64 => ChannelValue::F64(n),
    })
}

/// Channel lane → server variant of the declared type.
fn to_variant(value: ChannelValue, ty: OpcuaDataType) -> Variant {
    match ty {
        OpcuaDataType::Bool => Variant::Boolean(value.to_i32() != 0),
        OpcuaDataType::I16 => Variant::Int16(value.to_i32() as i16),
        OpcuaDataType::U16 => Variant::UInt16(value.to_i32() as u16),
        OpcuaDataType::I32 => Variant::Int32(value.to_i32()),
        OpcuaDataType::U32 => Variant::UInt32(value.to_i32() as u32),
        OpcuaDataType::F32 => Variant::Float(value.to_f32()),
        OpcuaDataType::F64 => Variant::Double(value.to_f64()),
    }
}

#[async_trait]
impl IoDevice for OpcuaDevice {
    fn name(&self) -> &str {
        &self.name
    }

    async fn read_channel(&mut self, channel: &str) -> Result<ChannelValue, IoError> {
        let ch = self.channel(channel)?;
        let zero = default_for(ch.meta.data_type);
        Ok(self
            .mirror
            .read()
            .expect("mirror poisoned")
            .get(channel)
            .copied()
            .unwrap_or(zero))
    }

    async fn write_channel(&mut self, channel: &str, value: ChannelValue) -> Result<(), IoError> {
        let ch = self.channel(channel)?.clone();
        if ch.meta.access != OpcuaAccess::Write {
            return Err(IoError::TypeMismatch {
                channel: channel.into(),
                value,
            });
        }
        self.write_node(&ch, value).await
    }

    /// `false` once the poll task has seen `UNHEALTHY_AFTER_FAILURES`
    /// consecutive failed refreshes — surfaced per device on /health and
    /// /status so a dead DCS link is visible instead of silently serving
    /// stale tags.
    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Only channels with an explicit `failsafe` value are written —
    /// the DCS below keeps authority over everything else.
    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        let mut first_err = None;
        let targets: Vec<ResolvedChannel> = self
            .channels
            .values()
            .filter(|c| c.meta.access == OpcuaAccess::Write && c.meta.failsafe.is_some())
            .cloned()
            .collect();
        for ch in targets {
            let fs = ch.meta.failsafe.expect("filtered Some");
            let value = match ch.meta.data_type {
                OpcuaDataType::F64 => ChannelValue::F64(fs),
                _ => ChannelValue::Real(fs as f32),
            };
            if let Err(e) = self.write_node(&ch, value).await {
                tracing::warn!(tag = %ch.meta.name, %e, "opcua failsafe write failed");
                first_err.get_or_insert(e);
            } else {
                tracing::info!(tag = %ch.meta.name, value = fs, "opcua failsafe applied");
            }
        }
        match first_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    async fn shutdown(&mut self) -> Result<(), IoError> {
        if let Some(t) = self.poll_task.take() {
            t.abort();
        }
        // Graceful UA disconnect (CloseSession + CloseSecureChannel),
        // then stop the connection event loop.
        let _ = self.session.disconnect().await;
        if let Some(t) = self.event_loop_task.take() {
            t.abort();
        }
        tracing::info!(device = %self.name, "opcua session closed");
        Ok(())
    }
}

fn default_for(ty: OpcuaDataType) -> ChannelValue {
    match ty {
        OpcuaDataType::Bool => ChannelValue::Bool(false),
        OpcuaDataType::U16 => ChannelValue::U16(0),
        OpcuaDataType::I16 => ChannelValue::I32(0),
        OpcuaDataType::I32 | OpcuaDataType::U32 => ChannelValue::I32(0),
        OpcuaDataType::F32 => ChannelValue::Real(0.0),
        OpcuaDataType::F64 => ChannelValue::F64(0.0),
    }
}

// ---------------- Address-space browse ----------------

/// One node under the browsed parent. Plain data — the server maps this
/// into its wire/ts-rs shape, same pattern as EtherCAT's
/// `SlaveDiscovery`.
#[derive(Debug, Clone)]
pub struct BrowsedNode {
    /// Full NodeId string (`ns=2;s=Channel1.Device1.Tag1`) — exactly
    /// what an `OpcuaChannel.node_id` wants.
    pub node_id: String,
    pub display_name: String,
    /// `Object` / `Variable` / `Method` / … (UA NodeClass name).
    pub node_class: String,
    /// For Variables: the UA data type's name when it's a standard
    /// scalar (`Int32`, `Double`, …), plus the closest channel
    /// `data_type` value (`i32`, `f64`) for one-click adoption.
    pub data_type: Option<String>,
    pub suggested_channel_type: Option<OpcuaDataType>,
}

/// One-shot browse of the server's address space under `from` (or
/// ObjectsFolder when `None`): connect → browse hierarchical references
/// → read each Variable's DataType attribute → disconnect. Used by the
/// IDE / CLI to pick NodeIds instead of retyping them from UaExpert.
pub async fn browse_endpoint(
    config: &OpcuaConfig,
    from: Option<&str>,
) -> Result<Vec<BrowsedNode>, IoError> {
    use opcua::types::{BrowseDescription, BrowseDirection, ReferenceTypeId};

    let root = match from {
        Some(s) => {
            NodeId::from_str(s).map_err(|e| IoError::Connect(format!("bad node id '{s}': {e}")))?
        }
        None => opcua::types::ObjectId::ObjectsFolder.into(),
    };

    let mut client: Client = ClientBuilder::new()
        .application_name("IA2 browse")
        .application_uri("urn:ia2:browse")
        .product_uri("urn:ia2:browse")
        .create_sample_keypair(false)
        .trust_server_certs(true)
        .session_retry_limit(0) // one shot — fail fast, the caller shows the error
        .client()
        .map_err(|e| IoError::Connect(format!("opcua client builder invalid: {e:?}")))?;

    let endpoint: EndpointDescription = (
        config.endpoint_url.as_str(),
        SecurityPolicy::None.to_str(),
        MessageSecurityMode::None,
        UserTokenPolicy::anonymous(),
    )
        .into();
    let identity = match &config.auth {
        OpcuaAuth::Anonymous => IdentityToken::Anonymous,
        OpcuaAuth::UserPassword { username, password } => {
            IdentityToken::UserName(username.clone(), password.clone().into())
        }
    };

    let (session, event_loop) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client.connect_to_matching_endpoint(endpoint, identity),
    )
    .await
    .map_err(|_| {
        IoError::Connect(format!(
            "opcua endpoint {} unreachable within {}s",
            config.endpoint_url,
            CONNECT_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| IoError::Connect(format!("opcua connect {}: {e}", config.endpoint_url)))?;
    let event_loop_task = tokio::spawn(event_loop.run());

    let result = async {
        if !session.wait_for_connection().await {
            return Err(IoError::Connect(format!(
                "opcua session to {} failed",
                config.endpoint_url
            )));
        }
        let desc = BrowseDescription {
            node_id: root,
            browse_direction: BrowseDirection::Forward,
            reference_type_id: ReferenceTypeId::HierarchicalReferences.into(),
            include_subtypes: true,
            node_class_mask: 0, // every class; the UI filters visually
            result_mask: 0x3F,  // all fields
        };
        let results = session
            .browse(&[desc], 1000, None)
            .await
            .map_err(|e| IoError::Transport(format!("opcua browse: {e}")))?;
        let refs = results
            .into_iter()
            .next()
            .and_then(|r| r.references)
            .unwrap_or_default();

        let mut nodes: Vec<BrowsedNode> = refs
            .into_iter()
            .map(|r| BrowsedNode {
                node_id: r.node_id.node_id.to_string(),
                display_name: r.display_name.text.to_string(),
                node_class: format!("{:?}", r.node_class),
                data_type: None,
                suggested_channel_type: None,
            })
            .collect();

        // One bulk read for every Variable's DataType attribute.
        let var_targets: Vec<(usize, NodeId)> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.node_class == "Variable")
            .filter_map(|(i, n)| NodeId::from_str(&n.node_id).ok().map(|id| (i, id)))
            .collect();
        if !var_targets.is_empty() {
            let reads: Vec<ReadValueId> = var_targets
                .iter()
                .map(|(_, id)| ReadValueId {
                    node_id: id.clone(),
                    attribute_id: AttributeId::DataType as u32,
                    index_range: Default::default(),
                    data_encoding: Default::default(),
                })
                .collect();
            if let Ok(values) = session.read(&reads, TimestampsToReturn::Neither, 0.0).await {
                for ((idx, _), dv) in var_targets.iter().zip(values) {
                    if let Some(Variant::NodeId(type_id)) = dv.value {
                        let (name, suggested) = ua_type_name(&type_id);
                        nodes[*idx].data_type = name.map(str::to_string);
                        nodes[*idx].suggested_channel_type = suggested;
                    }
                }
            }
        }

        // Folders first, then variables, alphabetical inside each — the
        // order a human expects from an address-space tree.
        nodes.sort_by(|a, b| {
            let rank = |n: &BrowsedNode| match n.node_class.as_str() {
                "Object" => 0,
                "Variable" => 1,
                _ => 2,
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.display_name.cmp(&b.display_name))
        });
        Ok(nodes)
    }
    .await;

    let _ = session.disconnect().await;
    event_loop_task.abort();
    result
}

/// Standard scalar type ids (ns=0) → display name + channel type hint.
fn ua_type_name(id: &NodeId) -> (Option<&'static str>, Option<OpcuaDataType>) {
    if id.namespace != 0 {
        return (None, None);
    }
    let numeric = match &id.identifier {
        opcua::types::Identifier::Numeric(n) => *n,
        _ => return (None, None),
    };
    match numeric {
        1 => (Some("Boolean"), Some(OpcuaDataType::Bool)),
        2 => (Some("SByte"), Some(OpcuaDataType::I16)),
        3 => (Some("Byte"), Some(OpcuaDataType::U16)),
        4 => (Some("Int16"), Some(OpcuaDataType::I16)),
        5 => (Some("UInt16"), Some(OpcuaDataType::U16)),
        6 => (Some("Int32"), Some(OpcuaDataType::I32)),
        7 => (Some("UInt32"), Some(OpcuaDataType::U32)),
        8 => (Some("Int64"), Some(OpcuaDataType::I32)),
        9 => (Some("UInt64"), Some(OpcuaDataType::U32)),
        10 => (Some("Float"), Some(OpcuaDataType::F32)),
        11 => (Some("Double"), Some(OpcuaDataType::F64)),
        12 => (Some("String"), None),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcua::types::Variant;

    /// An Int16 tag reading negative must reach the channel lane as a
    /// negative number. Decoded into `U16` the bit pattern looks right on
    /// the wire and then widens without a sign: a -40 setpoint arrives as
    /// 65496 in any PLC variable wider than 16 bits. Modbus's decoder
    /// carries the same warning in a comment; this adapter did not follow
    /// it.
    #[test]
    fn negative_int16_tag_stays_negative() {
        let v = from_variant(&Variant::Int16(-40), OpcuaDataType::I16).expect("decodes");
        assert_eq!(v, ChannelValue::I32(-40));
        assert_eq!(v.to_i32(), -40);
        assert_eq!(v.to_f64(), -40.0, "REAL/LREAL targets too");

        // The unsigned neighbour is unchanged and still unsigned.
        assert_eq!(
            from_variant(&Variant::UInt16(65496), OpcuaDataType::U16).expect("decodes"),
            ChannelValue::U16(65496)
        );
    }

    fn ch(name: &str, ty: OpcuaDataType) -> ResolvedChannel {
        ResolvedChannel {
            meta: OpcuaChannel {
                name: name.into(),
                node_id: format!("ns=2;s={name}"),
                data_type: ty,
                access: OpcuaAccess::Read,
                failsafe: None,
            },
            node: NodeId::new(2, name),
        }
    }

    #[test]
    fn a_matching_response_pairs_positionally() {
        let channels = vec![ch("pv1", OpcuaDataType::F64), ch("pv2", OpcuaDataType::F64)];
        let results = vec![
            DataValue::value_only(Variant::Double(1.5)),
            DataValue::value_only(Variant::Double(2.5)),
        ];
        let out = pair_results(&channels, results).expect("counts match");
        assert_eq!(
            out,
            vec![
                ("pv1".to_string(), ChannelValue::F64(1.5)),
                ("pv2".to_string(), ChannelValue::F64(2.5)),
            ]
        );
    }

    /// The defect this guards: a short response used to be a *success*. The
    /// tags past the cut would keep their seeded zero forever while
    /// `is_healthy()` stayed true — a dead PV the system swears is live.
    #[test]
    fn a_short_response_is_a_failed_refresh_not_a_partial_success() {
        let channels = vec![
            ch("pv1", OpcuaDataType::F64),
            ch("pv2", OpcuaDataType::F64),
            ch("pv3", OpcuaDataType::F64),
        ];
        let results = vec![DataValue::value_only(Variant::Double(1.5))];
        let err = pair_results(&channels, results).expect_err("a short response must not pass");
        let msg = err.to_string();
        assert!(msg.contains("asked for 3 nodes, got 1"), "{msg}");
        assert!(msg.contains("MaxNodesPerRead"), "{msg}");
    }

    /// A server returning MORE than asked is just as unmatchable — the extra
    /// entries mean the positional correspondence is not the one we assumed.
    #[test]
    fn an_over_long_response_is_rejected_too() {
        let channels = vec![ch("pv1", OpcuaDataType::F64)];
        let results = vec![
            DataValue::value_only(Variant::Double(1.5)),
            DataValue::value_only(Variant::Double(2.5)),
        ];
        assert!(pair_results(&channels, results).is_err());
    }

    /// A tag that comes back Bad is skipped (keeping its last value) without
    /// failing the whole refresh — that is per-tag quality, not a mismatch.
    #[test]
    fn a_bad_status_tag_is_skipped_while_the_rest_of_the_read_stands() {
        let channels = vec![ch("pv1", OpcuaDataType::F64), ch("pv2", OpcuaDataType::F64)];
        let mut bad = DataValue::value_only(Variant::Double(9.0));
        bad.status = Some(opcua::types::StatusCode::BadNoData);
        let out = pair_results(
            &channels,
            vec![bad, DataValue::value_only(Variant::Double(2.5))],
        )
        .expect("counts still match");
        assert_eq!(out, vec![("pv2".to_string(), ChannelValue::F64(2.5))]);
    }
}
