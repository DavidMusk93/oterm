mod assets;
mod auth;
mod complete;
mod config;
mod protocol;
mod ringbuf;
mod server;
mod session;

use anyhow::Result;
use clap::Parser;
use config::{Cli, Commands, OtpAction};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("oterm=info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Serve(args) => server::run_server(args).await,
        Commands::Otp(otp) => match otp.action {
            OtpAction::Init {
                secret_path,
                issuer,
                account,
                force,
            } => {
                let secret = auth::generate_secret_bytes();
                let b32 = auth::save_secret_file(&secret_path, &secret, force)?;
                let totp = auth::build_totp(secret, &issuer, &account)?;
                let url = totp.get_url();
                let qr = auth::render_qr_terminal(&url)?;
                println!("TOTP secret generated and saved to {}", secret_path.display());
                println!("Permissions set to 0600 (server-side binding only).\n");
                println!("{qr}\n");
                println!("Manual secret (Base32): {b32}");
                println!("otpauth URI:            {url}");
                Ok(())
            }
            OtpAction::Show {
                secret_path,
                issuer,
                account,
            } => {
                let secret = auth::load_secret_bytes(&secret_path)?;
                let b32 = auth::encode_base32(&secret);
                let totp = auth::build_totp(secret, &issuer, &account)?;
                let url = totp.get_url();
                let qr = auth::render_qr_terminal(&url)?;
                println!("Server-side TOTP secret ({}):\n", secret_path.display());
                println!("{qr}\n");
                println!("Manual secret (Base32): {b32}");
                println!("otpauth URI:            {url}");
                Ok(())
            }
            OtpAction::Verify {
                secret_path,
                issuer,
                account,
                code,
            } => {
                let ok = auth::verify_secret_file_code(&secret_path, &issuer, &account, &code)?;
                if ok {
                    println!("OK: valid TOTP code");
                    Ok(())
                } else {
                    anyhow::bail!("INVALID: TOTP code rejected");
                }
            }
        },
    }
}
