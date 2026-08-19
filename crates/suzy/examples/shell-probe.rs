//! Shell-session probe (ops/e2e.sh): connects to a live agent's shell
//! over the iroh operator channel, runs `echo <marker>`, and asserts the
//! marker comes back through the real pipeline — microVM pty →
//! gondolin-driver → castellan relay → suzerain operator channel → client.
//!
//! Usage: shell-probe [--key-file PATH] [--print-id] <suzerain-endpoint-id> [agent] [marker]
//!   --key-file  persist the probe's iroh identity (default: ephemeral)
//!   --print-id  print the operator id and exit (for allowlisting)
//! Exit 0 on success, 1 on failure (transcript printed either way).

use suzerain_client::{Client, ShellMessage};

fn load_or_create_key(path: &std::path::Path) -> anyhow::Result<suzerain_client::iroh::SecretKey> {
    use suzerain_client::iroh::SecretKey;
    if let Ok(bytes) = std::fs::read(path) {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("bad key length in {}", path.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, key.to_bytes())?;
    Ok(key)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut positional = Vec::new();
    let mut key_file: Option<std::path::PathBuf> = None;
    let mut print_id = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--key-file" => key_file = Some(args.next().expect("--key-file needs a value").into()),
            "--print-id" => print_id = true,
            _ => positional.push(arg),
        }
    }

    let key = match &key_file {
        Some(path) => load_or_create_key(path)?,
        None => suzerain_client::iroh::SecretKey::generate(),
    };
    let key = key; // (single binding keeps clippy quiet about the match arms)
    if print_id {
        println!("{}", key.public());
        return Ok(());
    }

    let endpoint_id = positional.first().expect(
        "usage: shell-probe [--key-file PATH] [--print-id] <suzerain-endpoint-id> [agent] [marker]",
    );
    let agent = positional
        .get(1)
        .map(String::as_str)
        .unwrap_or("researcher-1");
    let marker = positional
        .get(2)
        .map(String::as_str)
        .unwrap_or("e2e-shell-ok");

    let client = Client::new(endpoint_id, key);
    eprintln!("probe: connecting to {agent} shell on {endpoint_id}…");
    let mut conn = client.shell_connect(agent).await?;

    // Initial resize (also proves the Resize frame path).
    conn.resize(100, 30).await?;
    // The expected string must NOT appear in the command itself — otherwise
    // the pty's input echo would false-positive the assertion.
    let expect = format!("{marker}-42");
    conn.send_input(format!("echo {marker}-$((40+2))\n").as_bytes())
        .await?;

    let mut transcript: Vec<u8> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        let Some(msg) = conn.next().await else { break };
        match msg {
            Ok(ShellMessage::Data { data }) => {
                let bytes = suzerain_client::b64_decode(&data)?;
                transcript.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&transcript).contains(&expect) {
                    ok = true;
                    break;
                }
            }
            Ok(ShellMessage::Notice { message }) => eprintln!("probe: notice: {message}"),
            Ok(ShellMessage::Exit { code }) => {
                eprintln!("probe: shell exited (code {code})");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("probe: stream error: {e}");
                break;
            }
        }
    }

    eprintln!(
        "--- transcript ---\n{}\n------------------",
        String::from_utf8_lossy(&transcript)
    );
    if ok {
        eprintln!("probe: SHELL PROBE PASSED (computed output received)");
        Ok(())
    } else {
        anyhow::bail!("expected output '{expect}' never arrived")
    }
}
