//! Temporary loopback MQTT 3.1.1 broker. READY follows this server's successful
//! bind; bind/serve errors exit nonzero. STOP or EOF on stdin shuts it down.

use std::env;
use std::io::{self, BufRead, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut port: u16 = 0;
    let mut host: IpAddr = "127.0.0.1".parse()?;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().ok_or("--port needs a value")?.parse()?,
            "--bind" => host = args.next().ok_or("--bind needs a value")?.parse()?,
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if !host.is_loopback() {
        return Err("test broker must bind to loopback".into());
    }
    if port == 0 {
        let probe = TcpListener::bind(SocketAddr::new(host, 0))?;
        port = probe.local_addr()?.port();
    }
    let address = SocketAddr::new(host, port);
    let router = rumqttd::Router::new(
        0,
        rumqttd::RouterConfig {
            max_connections: 64,
            max_outgoing_packet_count: 500,
            max_segment_size: 1024 * 1024,
            max_segment_count: 10,
            custom_segment: None,
            initialized_filters: None,
            shared_subscriptions_strategy: Default::default(),
        },
    );
    // Broker::start hides individual listener errors in detached threads.
    // Drive Server::start directly so its bind/serve result remains observable.
    let mut server = rumqttd::Server::new(
        rumqttd::ServerSettings {
            name: "v4".into(),
            listen: address,
            tls: None,
            next_connection_delay_ms: 1,
            connections: rumqttd::ConnectionSettings {
                connection_timeout_ms: 5000,
                max_payload_size: 65536,
                max_inflight_count: 100,
                auth: None,
                external_auth: None,
                dynamic_filters: true,
            },
        },
        router.spawn(),
        rumqttd::protocol::v4::V4,
    );
    let serving = server.start(rumqttd::LinkType::Remote);
    tokio::pin!(serving);
    let listening = async {
        loop {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        // Poll our bind/serve future FIRST. If another process owns the port,
        // our bind error wins even when its TCP probe could also succeed.
        biased;
        result = &mut serving => {
            result?;
            return Err("broker listener stopped before readiness".into());
        }
        result = tokio::time::timeout(Duration::from_secs(5), listening) => { result?; }
    }
    println!("READY port={port}");
    io::stdout().flush()?;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    // A plain thread does not block Tokio shutdown on stdin if serving fails.
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) if line.trim() != "STOP" => continue,
                _ => break,
            }
        }
        let _ = stop_tx.send(());
    });
    tokio::select! {
        biased;
        result = &mut serving => {
            result?;
            Err("broker listener stopped unexpectedly".into())
        }
        _ = stop_rx => Ok(()),
    }
}
