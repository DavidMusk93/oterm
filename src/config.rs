use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "oterm",
    version,
    about = "oterm: Online Terminal — SOTA web terminal in Rust with server-side TOTP auth and persistent PTY sessions"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the oterm HTTP/WebSocket server
    Serve(ServeArgs),
    /// Manage server-side TOTP secret (bound strictly on server)
    Otp(OtpArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    /// Listen address (host:port)
    #[arg(long, env = "OTERM_BIND", default_value = "127.0.0.1:8782")]
    pub bind: String,

    /// Base URL path where oterm is mounted (e.g. /oterm or /)
    #[arg(long, env = "OTERM_BASE_PATH", default_value = "/oterm")]
    pub base_path: String,

    /// Path to server-side TOTP secret file
    #[arg(long, env = "OTERM_SECRET_PATH", default_value = "/etc/oterm/totp.secret")]
    pub secret_path: PathBuf,

    /// OTP authentication session TTL in seconds (default: 300 = 5 minutes)
    #[arg(long, env = "OTERM_AUTH_TTL", default_value_t = 300)]
    pub auth_ttl: u64,

    /// Per-session scrollback ring buffer size in bytes (default: 4 MiB)
    #[arg(long, env = "OTERM_RING_BUFFER_BYTES", default_value_t = 4 * 1024 * 1024)]
    pub ring_buffer_bytes: usize,

    /// Maximum concurrent persistent PTY sessions
    #[arg(long, env = "OTERM_MAX_SESSIONS", default_value_t = 16)]
    pub max_sessions: usize,

    /// Shell executable to spawn in PTY
    #[arg(long, env = "OTERM_SHELL")]
    pub shell: Option<String>,

    /// Initial working directory for new PTY sessions
    #[arg(long, env = "OTERM_CWD")]
    pub cwd: Option<PathBuf>,

    /// Force Secure flag on session cookies (auto-detected via X-Forwarded-Proto if false)
    #[arg(long, env = "OTERM_SECURE_COOKIE", default_value_t = false)]
    pub secure_cookie: bool,

    /// TOTP issuer label
    #[arg(long, env = "OTERM_ISSUER", default_value = "oterm")]
    pub issuer: String,

    /// TOTP account label
    #[arg(long, env = "OTERM_ACCOUNT", default_value = "xyz69.top")]
    pub account: String,
}

impl ServeArgs {
    /// Returns normalized base path: either "" (root) or "/segment" (no trailing slash).
    pub fn normalized_base_path(&self) -> String {
        let trimmed = self.base_path.trim().trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "/" {
            String::new()
        } else if trimmed.starts_with('/') {
            trimmed.to_string()
        } else {
            format!("/{}", trimmed)
        }
    }

    pub fn resolved_shell(&self) -> String {
        self.shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/bash".to_string())
    }

    pub fn resolved_cwd(&self) -> PathBuf {
        self.cwd
            .clone()
            .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/root"))
    }
}

#[derive(Args, Debug)]
pub struct OtpArgs {
    #[command(subcommand)]
    pub action: OtpAction,
}

#[derive(Subcommand, Debug)]
pub enum OtpAction {
    /// Generate and bind a new server-side TOTP secret and display its QR code
    Init {
        #[arg(long, env = "OTERM_SECRET_PATH", default_value = "/etc/oterm/totp.secret")]
        secret_path: PathBuf,
        #[arg(long, env = "OTERM_ISSUER", default_value = "oterm")]
        issuer: String,
        #[arg(long, env = "OTERM_ACCOUNT", default_value = "xyz69.top")]
        account: String,
        /// Overwrite existing secret file if present
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Display the QR code and URI for the existing server-side TOTP secret
    Show {
        #[arg(long, env = "OTERM_SECRET_PATH", default_value = "/etc/oterm/totp.secret")]
        secret_path: PathBuf,
        #[arg(long, env = "OTERM_ISSUER", default_value = "oterm")]
        issuer: String,
        #[arg(long, env = "OTERM_ACCOUNT", default_value = "xyz69.top")]
        account: String,
    },
    /// Verify a 6-digit TOTP code against the server-side secret
    Verify {
        #[arg(long, env = "OTERM_SECRET_PATH", default_value = "/etc/oterm/totp.secret")]
        secret_path: PathBuf,
        #[arg(long, env = "OTERM_ISSUER", default_value = "oterm")]
        issuer: String,
        #[arg(long, env = "OTERM_ACCOUNT", default_value = "xyz69.top")]
        account: String,
        /// 6-digit TOTP code
        code: String,
    },
}
