//! One bounded mailbox lane, polled beside (never inside) the PDO loop.
use async_trait::async_trait;
use ethercrab_wire::{EtherCrabWireRead, EtherCrabWireSized, WireError};
use iocore::sdo::{SdoAddress, SdoError, SdoUpload, SdoValue};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(4);

pub(crate) struct Scalar(pub Vec<u8>);
impl EtherCrabWireRead for Scalar {
    fn unpack_from_slice(bytes: &[u8]) -> Result<Self, WireError> {
        if !matches!(bytes.len(), 1 | 2 | 4 | 8) {
            return Err(WireError::InvalidValue);
        }
        Ok(Self(bytes.to_vec()))
    }
}
impl EtherCrabWireSized for Scalar {
    const PACKED_LEN: usize = 8;
    type Buffer = [u8; 8];
    fn buffer() -> Self::Buffer {
        [0; 8]
    }
}

pub(crate) fn value(
    address: SdoAddress,
    scalar: Scalar,
    id: ethercrab::SubDeviceIdentity,
) -> SdoValue {
    let mut raw = [0u8; 8];
    raw[..scalar.0.len()].copy_from_slice(&scalar.0);
    let unsigned = u64::from_le_bytes(raw);
    let shift = 64 - scalar.0.len() * 8;
    let signed = ((unsigned << shift) as i64) >> shift;
    SdoValue {
        address,
        bytes: scalar.0,
        unsigned,
        signed,
        vendor_id: id.vendor_id,
        product_id: id.product_id,
        revision: id.revision,
        serial: id.serial,
        sampled_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    }
}

pub(crate) struct Request {
    address: SdoAddress,
    deadline: Instant,
    generation: u64,
    ack: oneshot::Sender<Result<SdoValue, SdoError>>,
    // Held by the worker even if the HTTP caller disappears or times out.
    // Never overlap a second transaction with a late mailbox response.
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct Client {
    tx: smol::channel::Sender<Request>,
    gate: Arc<Semaphore>,
    healthy: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    reinitializing: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
}

pub(crate) fn channel(
    healthy: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    reinitializing: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
) -> (Arc<Client>, smol::channel::Receiver<Request>) {
    let (tx, rx) = smol::channel::bounded(1);
    (
        Arc::new(Client {
            tx,
            gate: Arc::new(Semaphore::new(1)),
            healthy,
            shutdown,
            reinitializing,
            generation,
        }),
        rx,
    )
}

#[async_trait]
impl SdoUpload for Client {
    async fn upload(&self, address: SdoAddress) -> Result<SdoValue, SdoError> {
        if self.shutdown.load(Ordering::Relaxed)
            || !self.healthy.load(Ordering::Relaxed)
            || self.reinitializing.load(Ordering::Relaxed)
        {
            return Err(SdoError::Unavailable(
                "master stopped, unhealthy or reinitializing".into(),
            ));
        }
        let permit = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| SdoError::Busy)?;
        let (ack, rx) = oneshot::channel();
        self.tx
            .try_send(Request {
                address,
                deadline: Instant::now() + RESPONSE_TIMEOUT,
                generation: self.generation.load(Ordering::Relaxed),
                ack,
                _permit: permit,
            })
            .map_err(|_| SdoError::Unavailable("mailbox worker disconnected".into()))?;
        tokio::time::timeout(RESPONSE_TIMEOUT, rx)
            .await
            .map_err(|_| SdoError::Timeout)?
            .map_err(|_| SdoError::Unavailable("mailbox session ended".into()))?
    }
}

/// The first future remains the original cyclic loop. Losing OP cancels
/// uploads before the group/master is replaced; queued work is not replayed.
pub(crate) async fn run<C, F, R>(
    rx: &smol::channel::Receiver<Request>,
    generation: &AtomicU64,
    cyclic: C,
    read: F,
) where
    C: Future<Output = ()>,
    F: Fn(SdoAddress) -> R,
    R: Future<Output = Result<SdoValue, SdoError>>,
{
    let mailbox = async {
        while let Ok(req) = rx.recv().await {
            if req.ack.is_closed() {
                continue;
            }
            let result = if req.deadline <= Instant::now() {
                Err(SdoError::Timeout)
            } else if req.generation != generation.load(Ordering::Relaxed) {
                Err(SdoError::Unavailable("master session changed".into()))
            } else {
                read(req.address).await
            };
            let _ = req.ack.send(result);
        }
        std::future::pending::<()>().await;
    };
    smol::future::race(cyclic, mailbox).await;
    while let Ok(req) = rx.try_recv() {
        let _ = req
            .ack
            .send(Err(SdoError::Unavailable("master session ended".into())));
    }
}

/// Both DC and free-run use this same upload and error mapping.
pub(crate) async fn read<S>(
    subdevice: Result<ethercrab::SubDeviceRef<'_, S>, ethercrab::error::Error>,
    address: SdoAddress,
) -> Result<SdoValue, SdoError>
where
    S: std::ops::Deref<Target = ethercrab::SubDevice>,
{
    let sd = subdevice.map_err(|_| SdoError::NotFound(format!("slave {}", address.slave)))?;
    let raw = sd
        .sdo_read::<Scalar>(address.index, address.subindex)
        .await
        .map_err(protocol_error)?;
    Ok(value(address, raw, sd.identity()))
}

pub(crate) fn protocol_error(error: ethercrab::error::Error) -> SdoError {
    match error {
        ethercrab::error::Error::Timeout(_) => SdoError::Timeout,
        other => SdoError::Protocol(format!("{other:?}")),
    }
}

/// Retain only the final assignment to each object. PDO setup often writes
/// an assignment count of zero first and its final count at the end.
pub(crate) fn final_assignments(
    commands: &[project::EthercatSdoInit],
) -> Vec<&project::EthercatSdoInit> {
    commands
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            !commands[i + 1..]
                .iter()
                .any(|later| later.index == c.index && later.sub_index == c.sub_index)
        })
        .map(|(_, c)| c)
        .collect()
}

pub(crate) fn matches_init(scalar: &Scalar, command: &project::EthercatSdoInit) -> bool {
    let bytes = command.value.to_le_bytes();
    scalar.0.as_slice() == &bytes[..usize::from(command.bits / 8)]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (
        Arc<Client>,
        smol::channel::Receiver<Request>,
        Arc<AtomicU64>,
    ) {
        let generation = Arc::new(AtomicU64::new(0));
        let (client, rx) = channel(
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            generation.clone(),
        );
        (client, rx, generation)
    }
    fn address() -> SdoAddress {
        SdoAddress {
            slave: 1,
            index: 0x2008,
            subindex: 1,
        }
    }
    #[test]
    fn scalar_width_and_signed_views_preserve_exact_bytes() {
        let id = ethercrab::SubDeviceIdentity {
            vendor_id: 1,
            product_id: 2,
            revision: 3,
            serial: 4,
        };
        for width in [1, 2, 4, 8] {
            let raw = vec![255; width];
            let v = value(address(), Scalar::unpack_from_slice(&raw).unwrap(), id);
            assert_eq!(v.signed, -1);
            assert_eq!(v.bytes, raw);
            assert_eq!(v.unsigned, u64::MAX >> (64 - width * 8));
        }
        for width in [0, 3, 5, 9] {
            assert!(Scalar::unpack_from_slice(&vec![0; width]).is_err());
        }
    }
    #[test]
    fn verification_uses_final_values_and_exact_width() {
        let commands = vec![
            project::EthercatSdoInit {
                index: 0x1c12,
                sub_index: 0,
                value: 0,
                bits: 8,
            },
            project::EthercatSdoInit {
                index: 0x1c12,
                sub_index: 1,
                value: 0x1600,
                bits: 16,
            },
            project::EthercatSdoInit {
                index: 0x1c12,
                sub_index: 0,
                value: 1,
                bits: 8,
            },
            project::EthercatSdoInit {
                index: 0x2000,
                sub_index: 0,
                value: -2,
                bits: 16,
            },
        ];
        let final_values = final_assignments(&commands);
        assert_eq!(final_values.len(), 3);
        assert_eq!(final_values[1].value, 1);
        assert!(matches_init(&Scalar(vec![1]), final_values[1]));
        assert!(!matches_init(&Scalar(vec![1, 0]), final_values[1]));
        assert!(!matches_init(&Scalar(vec![0]), final_values[1]));
        assert!(matches_init(&Scalar(vec![254, 255]), final_values[2]));
    }
    #[tokio::test]
    async fn pending_mailbox_does_not_block_cycles_and_is_cancelled_on_exit() {
        let (client, rx, generation) = fixture();
        let cycles = Arc::new(AtomicU64::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let c = cycles.clone();
        let start = started.clone();
        let worker = tokio::spawn(async move {
            run(
                &rx,
                &generation,
                async {
                    for _ in 0..10 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                },
                |_| async {
                    start.notify_one();
                    std::future::pending::<Result<SdoValue, SdoError>>().await
                },
            )
            .await;
        });
        let caller = client.clone();
        let upload = tokio::spawn(async move { caller.upload(address()).await });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        assert!(matches!(
            client.upload(address()).await,
            Err(SdoError::Busy)
        ));
        worker.await.unwrap();
        assert_eq!(cycles.load(Ordering::Relaxed), 10);
        assert!(matches!(
            upload.await.unwrap(),
            Err(SdoError::Unavailable(_))
        ));
        assert_eq!(client.gate.available_permits(), 1);
    }
    #[tokio::test]
    async fn cancelled_expired_and_old_session_requests_never_reach_bus() {
        for case in 0..3 {
            let (client, rx, generation) = fixture();
            let (ack, reply) = oneshot::channel();
            client
                .tx
                .try_send(Request {
                    address: address(),
                    deadline: if case == 1 {
                        Instant::now() - Duration::from_secs(1)
                    } else {
                        Instant::now() + Duration::from_secs(1)
                    },
                    generation: if case == 2 { 1 } else { 0 },
                    ack,
                    _permit: client.gate.clone().try_acquire_owned().unwrap(),
                })
                .unwrap();
            if case == 0 {
                drop(reply);
            } // mimic HTTP cancellation before polling
            run(
                &rx,
                &generation,
                tokio::time::sleep(Duration::from_millis(10)),
                |_| async { panic!("stale upload reached bus") },
            )
            .await;
            assert_eq!(client.gate.available_permits(), 1);
        }
    }
    #[tokio::test]
    async fn unhealthy_master_rejects_without_enqueuing() {
        let (client, rx, _) = fixture();
        client.healthy.store(false, Ordering::Relaxed);
        assert!(matches!(
            client.upload(address()).await,
            Err(SdoError::Unavailable(_))
        ));
        assert!(rx.is_empty());
    }
}
