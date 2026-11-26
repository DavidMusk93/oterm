use anyhow::{anyhow, bail, Context, Result};
use qrcode::{render::unicode, QrCode};
use rand::RngCore;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use totp_rs::{Algorithm, TOTP};

pub const COOKIE_NAME: &str = "oterm_sid";
const TOTP_STEP_SECS: u64 = 30;
const TOTP_DIGITS: usize = 6;
const MAX_FAILED_ATTEMPTS: usize = 5;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct AuthSession {
    pub token: String,
    pub expires_at: Instant,
}

#[derive(Debug)]
struct RateLimitState {
    failures: Vec<Instant>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("too many failed attempts; retry in {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("invalid or already-used OTP code")]
    InvalidCode,
}

pub struct AuthManager {
    totp: TOTP,
    auth_ttl: Duration,
    sessions: RwLock<HashMap<String, AuthSession>>,
    rate_limits: RwLock<HashMap<String, RateLimitState>>,
    last_used_step: RwLock<Option<u64>>,
}

impl AuthManager {
    pub fn from_secret_file(
        path: &Path,
        issuer: &str,
        account: &str,
        auth_ttl_secs: u64,
    ) -> Result<Self> {
        let secret_bytes = load_secret_bytes(path)?;
        Self::new(secret_bytes, issuer, account, auth_ttl_secs)
    }

    pub fn new(
        secret_bytes: Vec<u8>,
        issuer: &str,
        account: &str,
        auth_ttl_secs: u64,
    ) -> Result<Self> {
        let totp = build_totp(secret_bytes, issuer, account)?;
        Ok(Self {
            totp,
            auth_ttl: Duration::from_secs(auth_ttl_secs),
            sessions: RwLock::new(HashMap::new()),
            rate_limits: RwLock::new(HashMap::new()),
            last_used_step: RwLock::new(None),
        })
    }

    pub fn auth_ttl_secs(&self) -> u64 {
        self.auth_ttl.as_secs()
    }

    /// Verify a 6-digit TOTP code and issue a new session valid for `auth_ttl`.
    pub fn authenticate(&self, code: &str, client_ip: &str) -> std::result::Result<AuthSession, AuthError> {
        let now = Instant::now();
        self.check_rate_limit(client_ip, now)?;

        let trimmed = code.trim();
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let matched_step = self.verify_code_with_step(trimmed, unix_now);
        let Some(step) = matched_step else {
            self.record_failure(client_ip, now);
            return Err(AuthError::InvalidCode);
        };

        // Replay protection: reject if this step was already used
        {
            let mut last = self.last_used_step.write().unwrap();
            if let Some(prev) = *last {
                if step <= prev {
                    self.record_failure(client_ip, now);
                    return Err(AuthError::InvalidCode);
                }
            }
            *last = Some(step);
        }

        // Clear rate limit failures on success
        {
            let mut rl = self.rate_limits.write().unwrap();
            rl.remove(client_ip);
        }

        let mut token_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut token_bytes);
        let token = hex::encode(token_bytes);
        let session = AuthSession {
            token: token.clone(),
            expires_at: now + self.auth_ttl,
        };

        {
            let mut sessions = self.sessions.write().unwrap();
            sessions.retain(|_, s| s.expires_at > now);
            sessions.insert(token, session.clone());
        }

        Ok(session)
    }

    /// Check if session token is valid; returns remaining TTL if valid, None if expired/unknown.
    pub fn validate(&self, token: &str) -> Option<Duration> {
        let now = Instant::now();
        let sessions = self.sessions.read().unwrap();
        let session = sessions.get(token)?;
        if session.expires_at > now {
            Some(session.expires_at.saturating_duration_since(now))
        } else {
            None
        }
    }

    /// Renew an active session's idle expiration back to `auth_ttl` on user activity.
    /// Returns the renewed TTL if the session is still valid, or `None` if expired/unknown.
    pub fn touch(&self, token: &str) -> Option<Duration> {
        if self.auth_ttl.is_zero() {
            return None;
        }
        let now = Instant::now();
        let mut sessions = self.sessions.write().unwrap();
        let session = sessions.get_mut(token)?;
        if session.expires_at > now {
            session.expires_at = now + self.auth_ttl;
            Some(self.auth_ttl)
        } else {
            sessions.remove(token);
            None
        }
    }

    /// Revoke an active session (manual lock).
    pub fn revoke(&self, token: &str) {
        let mut sessions = self.sessions.write().unwrap();
        sessions.remove(token);
    }

    fn verify_code_with_step(&self, code: &str, unix_secs: u64) -> Option<u64> {
        if code.len() != TOTP_DIGITS || !code.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let current_step = unix_secs / TOTP_STEP_SECS;
        // Check -1, 0, +1 step window for clock skew tolerance
        for delta in [-1i64, 0, 1] {
            let candidate_step = (current_step as i64 + delta) as u64;
            let candidate_time = candidate_step * TOTP_STEP_SECS;
            let expected = self.totp.generate(candidate_time);
            if constant_time_eq(code.as_bytes(), expected.as_bytes()) {
                return Some(candidate_step);
            }
        }
        None
    }

    fn check_rate_limit(&self, ip: &str, now: Instant) -> std::result::Result<(), AuthError> {
        let mut rl = self.rate_limits.write().unwrap();
        if let Some(state) = rl.get_mut(ip) {
            state.failures.retain(|t| now.duration_since(*t) < RATE_LIMIT_WINDOW);
            if state.failures.len() >= MAX_FAILED_ATTEMPTS {
                let oldest = state.failures[0];
                let retry_after = RATE_LIMIT_WINDOW
                    .saturating_sub(now.duration_since(oldest))
                    .as_secs()
                    .max(1);
                return Err(AuthError::RateLimited {
                    retry_after_secs: retry_after,
                });
            }
        }
        Ok(())
    }

    fn record_failure(&self, ip: &str, now: Instant) {
        let mut rl = self.rate_limits.write().unwrap();
        let entry = rl
            .entry(ip.to_string())
            .or_insert_with(|| RateLimitState { failures: Vec::new() });
        entry.failures.retain(|t| now.duration_since(*t) < RATE_LIMIT_WINDOW);
        entry.failures.push(now);
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn build_totp(secret_bytes: Vec<u8>, issuer: &str, account: &str) -> Result<TOTP> {
    TOTP::new(
        Algorithm::SHA1,
        TOTP_DIGITS,
        1,
        TOTP_STEP_SECS,
        secret_bytes,
        Some(issuer.to_string()),
        account.to_string(),
    )
    .map_err(|e| anyhow!("failed to construct TOTP: {e}"))
}

pub fn generate_secret_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 20]; // 160-bit RFC 4226 recommended secret
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

pub fn encode_base32(bytes: &[u8]) -> String {
    base32::encode(base32::Alphabet::Rfc4648 { padding: false }, bytes)
}

pub fn decode_base32(encoded: &str) -> Result<Vec<u8>> {
    let cleaned: String = encoded
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_uppercase();
    base32::decode(base32::Alphabet::Rfc4648 { padding: false }, &cleaned)
        .or_else(|| base32::decode(base32::Alphabet::Rfc4648 { padding: true }, &cleaned))
        .ok_or_else(|| anyhow!("invalid Base32 TOTP secret"))
}

pub fn save_secret_file(path: &Path, secret_bytes: &[u8], force: bool) -> Result<String> {
    if path.exists() && !force {
        bail!(
            "secret file already exists at {} (use --force to overwrite)",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let b32 = encode_base32(secret_bytes);
    fs::write(path, format!("{b32}\n"))
        .with_context(|| format!("failed to write secret file {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod 0600 {}", path.display()))?;
    Ok(b32)
}

pub fn load_secret_bytes(path: &Path) -> Result<Vec<u8>> {
    if !path.exists() {
        bail!(
            "TOTP secret not found at {}. Run `oterm otp init` on the server first.",
            path.display()
        );
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read TOTP secret from {}", path.display()))?;
    let bytes = decode_base32(raw.trim())?;
    if bytes.len() < 16 {
        bail!("TOTP secret must be at least 128 bits (16 bytes)");
    }
    Ok(bytes)
}

pub fn render_qr_terminal(otpauth_url: &str) -> Result<String> {
    let code = QrCode::new(otpauth_url.as_bytes())?;
    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    Ok(image)
}

pub fn verify_secret_file_code(
    path: &Path,
    issuer: &str,
    account: &str,
    code: &str,
) -> Result<bool> {
    let bytes = load_secret_bytes(path)?;
    let totp = build_totp(bytes, issuer, account)?;
    Ok(totp.check_current(code.trim()).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totp_auth_replay_and_expiry() {
        let secret = generate_secret_bytes();
        let mgr = AuthManager::new(secret.clone(), "oterm", "test", 300).unwrap();
        let totp = build_totp(secret, "oterm", "test").unwrap();
        let code = totp.generate_current().unwrap();

        // First auth succeeds
        let session = mgr.authenticate(&code, "127.0.0.1").unwrap();
        let ttl = mgr.validate(&session.token).unwrap();
        assert!(ttl.as_secs() <= 300 && ttl.as_secs() >= 298);

        // Replay of same code in same step fails
        assert!(matches!(
            mgr.authenticate(&code, "127.0.0.1"),
            Err(AuthError::InvalidCode)
        ));

        // Revoke invalidates session
        mgr.revoke(&session.token);
        assert!(mgr.validate(&session.token).is_none());
    }

    #[test]
    fn rate_limit_blocks_after_five_failures() {
        let secret = generate_secret_bytes();
        let mgr = AuthManager::new(secret, "oterm", "test", 300).unwrap();
        for _ in 0..MAX_FAILED_ATTEMPTS {
            assert!(matches!(
                mgr.authenticate("000000", "10.0.0.1"),
                Err(AuthError::InvalidCode)
            ));
        }
        assert!(matches!(
            mgr.authenticate("000000", "10.0.0.1"),
            Err(AuthError::RateLimited { .. })
        ));
    }

    #[test]
    fn short_ttl_expires() {
        let secret = generate_secret_bytes();
        let mgr = AuthManager::new(secret.clone(), "oterm", "test", 0).unwrap();
        let totp = build_totp(secret, "oterm", "test").unwrap();
        let code = totp.generate_current().unwrap();
        let session = mgr.authenticate(&code, "127.0.0.1").unwrap();
        assert!(mgr.validate(&session.token).is_none());
        assert!(mgr.touch(&session.token).is_none());
    }

    #[test]
    fn touch_renews_active_session_and_rejects_expired() {
        let secret = generate_secret_bytes();
        let mgr = AuthManager::new(secret.clone(), "oterm", "test", 2).unwrap();
        let totp = build_totp(secret, "oterm", "test").unwrap();
        let code = totp.generate_current().unwrap();
        let session = mgr.authenticate(&code, "127.0.0.1").unwrap();

        // Simulate time passing so remaining TTL drops below 1.2s
        {
            let mut sessions = mgr.sessions.write().unwrap();
            let entry = sessions.get_mut(&session.token).unwrap();
            entry.expires_at = Instant::now() + Duration::from_millis(500);
        }
        assert!(mgr.validate(&session.token).unwrap() <= Duration::from_millis(500));

        // User activity renews full 2s TTL
        let renewed = mgr.touch(&session.token).unwrap();
        assert_eq!(renewed, Duration::from_secs(2));
        assert!(mgr.validate(&session.token).unwrap() > Duration::from_millis(1500));

        // Once expired, touch must not resurrect session
        {
            let mut sessions = mgr.sessions.write().unwrap();
            let entry = sessions.get_mut(&session.token).unwrap();
            entry.expires_at = Instant::now() - Duration::from_millis(10);
        }
        assert!(mgr.touch(&session.token).is_none());
        assert!(mgr.validate(&session.token).is_none());
    }
}
