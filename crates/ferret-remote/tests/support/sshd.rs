// Included by more than one test binary, and each uses a different part of
// it: `r1_sshd` replaces the host key, `r6_sftp` never does. Without this,
// every helper one binary happens not to call is reported as dead.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ferret_remote::Connection;

const ENABLE_ENV: &str = "FERRET_REMOTE_SSHD_TEST";

pub struct Sshd {
    server: ServerProcess,
    fixture: PathBuf,
    port: u16,
    user: String,
}

enum ServerProcess {
    Docker(String),
    Local(Child),
}

pub fn trusted_connection_from_env() -> Option<Connection> {
    let host = std::env::var("FERRET_TEST_SSH_HOST").ok()?;
    let user = std::env::var("FERRET_TEST_SSH_USER").ok()?;
    let key_path = std::env::var_os("FERRET_TEST_SSH_KEY").map(PathBuf::from)?;
    let port = std::env::var("FERRET_TEST_SSH_PORT")
        .ok()
        .map(|value| value.parse().expect("FERRET_TEST_SSH_PORT must be a u16"));
    Some(Connection {
        host,
        user,
        port,
        key_path: Some(key_path),
        ..Connection::default()
    })
}

impl Sshd {
    pub fn start_if_enabled() -> Option<Self> {
        if std::env::var_os(ENABLE_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        assert_installed("ssh-keygen");
        let fixture = std::env::temp_dir().join(format!(
            "ferret-r1-sshd-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&fixture).expect("create sshd fixture directory");
        generate_key(&fixture.join("id_ed25519"));
        std::fs::copy(
            fixture.join("id_ed25519.pub"),
            fixture.join("authorized_keys"),
        )
        .expect("write authorized_keys");

        let (server, port, user) = if command_available("docker", &["version"]) {
            let (server, port) = start_docker(&fixture);
            (server, port, "ferret".into())
        } else {
            let user = std::env::var("USER").expect("current user");
            let (server, port) = start_local(&fixture, &user);
            (server, port, user)
        };
        wait_until_listening(port);
        std::fs::write(fixture.join("known_hosts"), []).expect("create isolated known_hosts");
        Some(Self {
            server,
            fixture,
            port,
            user,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn user(&self) -> &str {
        &self.user
    }
    pub fn private_key(&self) -> PathBuf {
        self.fixture.join("id_ed25519")
    }
    pub fn known_hosts(&self) -> PathBuf {
        self.fixture.join("known_hosts")
    }

    pub fn replace_with_different_host_key(&self) {
        let fake = self.fixture.join("different_host_ed25519");
        generate_key(&fake);
        let public = std::fs::read_to_string(fake.with_extension("pub")).expect("read fake key");
        let mut fields = public.split_whitespace();
        let algorithm = fields.next().expect("key algorithm");
        let encoded = fields.next().expect("key bytes");
        std::fs::write(
            self.known_hosts(),
            format!("[127.0.0.1]:{} {algorithm} {encoded}\n", self.port),
        )
        .expect("replace isolated known_hosts");
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        match &mut self.server {
            ServerProcess::Docker(container) => {
                let _ = command("docker")
                    .args(["rm", "--force", container])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            ServerProcess::Local(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = std::fs::remove_dir_all(&self.fixture);
    }
}

fn start_docker(fixture: &Path) -> (ServerProcess, u16) {
    let container = format!("ferret-r1-{}-{}", std::process::id(), unique_suffix());
    let output = command("docker")
        .args([
            "run",
            "--detach",
            "--rm",
            "--name",
            &container,
            "--publish",
            "127.0.0.1::2222",
            "--volume",
        ])
        .arg(format!(
            "{}:/home/ferret/.ssh/authorized_keys:ro",
            fixture.join("authorized_keys").display()
        ))
        .args([
            "--env",
            "USER_NAME=ferret",
            "--env",
            "PUBLIC_KEY_FILE=/home/ferret/.ssh/authorized_keys",
            "lscr.io/linuxserver/openssh-server:latest",
        ])
        .output()
        .expect("start sshd container");
    assert!(
        output.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let port = published_port(&container);
    (ServerProcess::Docker(container), port)
}

fn start_local(fixture: &Path, user: &str) -> (ServerProcess, u16) {
    assert_installed("/usr/sbin/sshd");
    let host_key = fixture.join("ssh_host_ed25519_key");
    generate_key(&host_key);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve port");
    let port = listener.local_addr().expect("reserved address").port();
    drop(listener);
    let config = fixture.join("sshd_config");
    std::fs::write(&config, format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nPubkeyAuthentication yes\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nUsePAM no\nAllowUsers {user}\nLogLevel ERROR\nSubsystem sftp internal-sftp\n",
        host_key.display(), fixture.join("sshd.pid").display(), fixture.join("authorized_keys").display()
    )).expect("write sshd config");
    let child = command("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(config)
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start local sshd");
    (ServerProcess::Local(child), port)
}

fn generate_key(path: &Path) {
    assert!(
        command("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)
            .status()
            .expect("run ssh-keygen")
            .success()
    );
}

fn command(program: &str) -> Command {
    Command::new(program)
}

fn command_available(program: &str, args: &[&str]) -> bool {
    command(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn assert_installed(program: &str) {
    assert!(
        command(program)
            .arg("-?")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok(),
        "{program} is required when {ENABLE_ENV}=1"
    );
}

fn published_port(container: &str) -> u16 {
    let output = command("docker")
        .args(["port", container, "2222/tcp"])
        .output()
        .expect("read port");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .expect("published port")
}

fn wait_until_listening(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("sshd did not listen on port {port}");
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
