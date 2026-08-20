//! F-017 slice 2a LIVE smoke: prove the SSH stdio transport end-to-end against a REAL host. Ignored by default
//! (needs a reachable host with `ruse` on its PATH + working key auth); the deterministic command-assembly is
//! unit-tested in `remote::transport`, this only exercises the real `ssh` wire.
//!
//! Run with: `RUSE_SSH_TEST_HOST=user@host cargo test -p ruse-tui --test agent_ssh -- --ignored`.
//! The remote path defaults to `/etc/hostname`; override with `RUSE_SSH_TEST_PATH`.

use ruse_tui::remote::client::AgentClient;
use ruse_tui::remote::transport;

#[test]
#[ignore = "needs a reachable SSH host with `ruse` on PATH (set RUSE_SSH_TEST_HOST)"]
fn ssh_handshakes_and_reads_a_remote_file() {
    let Ok(host) = std::env::var("RUSE_SSH_TEST_HOST") else {
        // Without a target this smoke cannot prove anything — skip rather than assert on nothing.
        return;
    };
    let remote_path =
        std::env::var("RUSE_SSH_TEST_PATH").unwrap_or_else(|_| "/etc/hostname".into());

    let cmd = transport::ssh_command(&host, transport::DEFAULT_REMOTE_AGENT_CMD);
    let mut client = AgentClient::spawn(cmd, &["fs.readFile"]).expect("ssh spawn + handshake");

    assert_eq!(
        client.protocol_version(),
        1,
        "remote agent speaks protocol v1"
    );
    assert!(client.has("fs.readFile"), "fs.readFile negotiated over SSH");

    let content = client.read_file(&remote_path).expect("read a remote file");
    assert!(
        !content.is_empty(),
        "read {remote_path} through the remote agent"
    );
}
