//! CLI for exercising the protocol core end-to-end before any platform UI
//! exists (Phase 0). See `docs/protocol.md`.

mod run;

use clap::{Parser, Subcommand};
use continuity_crypto::{Identity, TrustStore};

#[derive(Parser)]
#[command(name = "continuityctl", about = "Continuity protocol core test CLI")]
struct Cli {
    /// Scopes the identity/trust store, so one machine can run multiple
    /// independent "devices" for local testing. Real usage never sets this.
    #[arg(long, global = true, default_value = "default")]
    profile: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show this device's identity, creating one on first run.
    Id,
    /// Inspect or manage the trusted-device store.
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },
    /// Run the Phase 0 daemon: advertise, discover, pair, and sync the
    /// clipboard with trusted peers on the LAN. Ctrl-C to stop.
    Run {
        /// Display name advertised to peers. Defaults to the hostname.
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum TrustAction {
    /// List currently-trusted devices.
    List,
    /// Remove a device from the trust store by its device id.
    Revoke { device_id: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Id => cmd_id(&cli.profile),
        Command::Trust { action } => cmd_trust(&cli.profile, action),
        Command::Run { name } => run::run(&cli.profile, name).await,
    }
}

fn cmd_id(profile: &str) -> anyhow::Result<()> {
    let identity = Identity::load_or_create(profile)?;
    println!("device id: {}", identity.device_id());
    Ok(())
}

fn cmd_trust(profile: &str, action: TrustAction) -> anyhow::Result<()> {
    let mut store = TrustStore::load_default(profile)?;
    match action {
        TrustAction::List => {
            let devices: Vec<_> = store.list().collect();
            if devices.is_empty() {
                println!("no trusted devices yet");
            }
            for device in devices {
                println!("{}  {}", device.id, device.name);
            }
        }
        TrustAction::Revoke { device_id } => {
            store.revoke(&device_id)?;
            println!("revoked {device_id}");
        }
    }
    Ok(())
}
