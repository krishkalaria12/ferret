# R-1 OpenSSH integration harness

The regular `ferret-remote` test suite does not require Docker and skips the
real-server test. To start a disposable OpenSSH container and exercise the
harness explicitly, run:

```sh
FERRET_REMOTE_SSHD_TEST=1 cargo test -p ferret-remote --test r1_sshd -- --nocapture
```

The harness prefers Docker when it is available. Without Docker it uses the
system `/usr/sbin/sshd`, still on a random loopback port with an isolated
configuration, client key, host key, and `known_hosts` file. It removes the
container or daemon and fixture directory when the test ends. The sequence
refuses an unknown key, accepts only the exact fingerprint just presented,
authenticates, disconnects, replaces the recorded key, and refuses the changed
key. The local fallback is useful on a stock macOS development machine; CI can
exercise the container path.

The clean authenticated-connect and disconnect case deliberately does not
modify the developer's real `known_hosts`. Point it at a disposable server
whose key is already recorded there:

```sh
FERRET_TEST_SSH_HOST=127.0.0.1 \
FERRET_TEST_SSH_PORT=2222 \
FERRET_TEST_SSH_USER=ferret \
FERRET_TEST_SSH_KEY=/absolute/path/to/id_ed25519 \
cargo test -p ferret-remote --test r1_sshd -- --nocapture
```

R-1's session-engine connection, authentication, and changed-key assertions
belong in `r1_sshd.rs`; keeping them in this opt-in binary makes normal CI
deterministic on hosts without a container runtime.
