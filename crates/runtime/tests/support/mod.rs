use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct TestRuntime {
    pub child: Child,
    pub port: u16,
    log: PathBuf,
}

impl TestRuntime {
    pub fn spawn(command: &mut Command, bind_ip: IpAddr, log: &Path) -> Self {
        let output = fs::File::create(log).expect("create runtime log");
        let child = command
            .arg("--bind")
            .arg(SocketAddr::new(bind_ip, 0).to_string())
            .env("IA2_PRINT_LISTEN_ADDR", "1")
            .stdin(Stdio::null())
            .stderr(output.try_clone().expect("clone runtime log handle"))
            .stdout(output)
            .spawn()
            .expect("start ia2-runtime");
        let mut runtime = Self {
            child,
            port: 0,
            log: log.to_path_buf(),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let output = runtime.output();
            // Wait for a complete line so a partial port number cannot pass.
            let address = output.rsplit_once('\n').and_then(|(complete, _)| {
                complete
                    .lines()
                    .find_map(|line| line.strip_prefix("IA2_LISTEN_ADDR="))
            });
            if let Some(address) = address {
                let address: SocketAddr = address.parse().expect("valid runtime listen address");
                assert!(
                    address.ip() == bind_ip && address.port() != 0,
                    "unexpected listen address {address} for bind IP {bind_ip}"
                );
                runtime.port = address.port();
                return runtime;
            }
            if let Some(status) = runtime.child.try_wait().expect("check runtime startup") {
                panic!(
                    "ia2-runtime exited {status} before reporting its address:\n{}",
                    runtime.output()
                );
            }
            assert!(
                Instant::now() < deadline,
                "ia2-runtime did not report its listen address:\n{output}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn output(&self) -> String {
        match fs::read(&self.log) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => format!("reading runtime log: {error}"),
        }
    }
}

impl Drop for TestRuntime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
