use crate::protocol::SessionInfo;
use crate::ringbuf::{ReplaySlice, SeqRingBuffer};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use uuid::Uuid;

const BROADCAST_CAPACITY: usize = 512;
const PTY_READ_BUF_SIZE: usize = 16 * 1024;
const TMUX_SESSION_PREFIX: &str = "oterm_";
const CONTEXT_CACHE_TTL: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub end_seq: u64,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub cwd: PathBuf,
    pub fg_command: String,
    pub git_branch: Option<String>,
    pub alt_screen: bool,
    pub echo_enabled: bool,
}

impl SessionContext {
    /// True when the pane is sitting at an interactive shell prompt with echo enabled
    /// (never inside vim/htop/less/TUI or a hidden password prompt).
    pub fn is_safe_for_completion(&self) -> bool {
        if self.alt_screen || !self.echo_enabled {
            return false;
        }
        let cmd = self
            .fg_command
            .rsplit('/')
            .next()
            .unwrap_or(&self.fg_command)
            .trim_start_matches('-');
        matches!(cmd, "bash" | "zsh" | "sh" | "dash" | "fish" | "ash" | "ksh")
    }
}

pub struct TerminalSession {
    pub id: String,
    name: RwLock<String>,
    created_at_ms: u64,
    tmux_socket: Option<String>,
    tmux_session: Option<String>,
    child_pid: Option<u32>,
    fallback_cwd: PathBuf,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    ring: Mutex<SeqRingBuffer>,
    tx: broadcast::Sender<OutputChunk>,
    size: Mutex<(u16, u16)>,
    alive: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    clients: AtomicUsize,
    child_killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    cached_context: RwLock<Option<(Instant, SessionContext)>>,
}

impl TerminalSession {
    pub fn spawn_direct(
        name: String,
        shell: &str,
        cwd: &PathBuf,
        cols: u16,
        rows: u16,
        ring_capacity: usize,
    ) -> Result<Arc<Self>> {
        let id = Uuid::new_v4().simple().to_string()[..12].to_string();
        let created_at_ms = now_ms();

        let mut cmd = CommandBuilder::new(shell);
        if shell.ends_with("bash") || shell.ends_with("zsh") {
            cmd.arg("-l");
        }
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }

        Self::spawn_pty_command(
            id,
            name,
            created_at_ms,
            None,
            None,
            cwd.clone(),
            cmd,
            cols,
            rows,
            ring_capacity,
            None,
        )
    }

    pub fn spawn_tmux_new(
        socket: &str,
        name: String,
        shell: &str,
        cwd: &PathBuf,
        cols: u16,
        rows: u16,
        ring_capacity: usize,
    ) -> Result<Arc<Self>> {
        let id = Uuid::new_v4().simple().to_string()[..12].to_string();
        let created_at_ms = now_ms();
        let tmux_name = format!("{TMUX_SESSION_PREFIX}{id}");

        let shell_cmd = if shell.ends_with("bash") || shell.ends_with("zsh") {
            format!("exec {} -l", shell_escape(shell))
        } else {
            format!("exec {}", shell_escape(shell))
        };

        ensure_tmux_server_defaults(socket);

        let status = Command::new("tmux")
            .args([
                "-L",
                socket,
                "new-session",
                "-d",
                "-s",
                &tmux_name,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
                "-c",
                &cwd.to_string_lossy(),
                &shell_cmd,
            ])
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .status()
            .context("failed to execute tmux new-session")?;

        if !status.success() {
            bail!("tmux new-session failed with status {status}");
        }

        configure_tmux_session(socket, &tmux_name, &id, &name, created_at_ms);

        Self::attach_tmux(
            socket,
            tmux_name,
            id,
            name,
            created_at_ms,
            cwd.clone(),
            cols,
            rows,
            ring_capacity,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn attach_tmux(
        socket: &str,
        tmux_name: String,
        id: String,
        name: String,
        created_at_ms: u64,
        fallback_cwd: PathBuf,
        cols: u16,
        rows: u16,
        ring_capacity: usize,
        capture_history: bool,
    ) -> Result<Arc<Self>> {
        let initial_seed = if capture_history {
            capture_tmux_pane_history(socket, &tmux_name)
        } else {
            None
        };

        let mut cmd = CommandBuilder::new("tmux");
        cmd.args(["-L", socket, "attach-session", "-t", &tmux_name]);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }

        Self::spawn_pty_command(
            id,
            name,
            created_at_ms,
            Some(socket.to_string()),
            Some(tmux_name),
            fallback_cwd,
            cmd,
            cols,
            rows,
            ring_capacity,
            initial_seed,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_pty_command(
        id: String,
        name: String,
        created_at_ms: u64,
        tmux_socket: Option<String>,
        tmux_session: Option<String>,
        fallback_cwd: PathBuf,
        cmd: CommandBuilder,
        cols: u16,
        rows: u16,
        ring_capacity: usize,
        initial_seed: Option<Bytes>,
    ) -> Result<Arc<Self>> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY pair")?;

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn PTY command")?;
        let child_pid = child.process_id();
        let killer = child.clone_killer();
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let mut ring = SeqRingBuffer::new(ring_capacity);
        if let Some(seed) = initial_seed {
            if !seed.is_empty() {
                ring.push(seed);
            }
        }

        let session = Arc::new(Self {
            id: id.clone(),
            name: RwLock::new(name),
            created_at_ms,
            tmux_socket,
            tmux_session,
            child_pid,
            fallback_cwd,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            ring: Mutex::new(ring),
            tx,
            size: Mutex::new((cols, rows)),
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            clients: AtomicUsize::new(0),
            child_killer: Mutex::new(Some(killer)),
            cached_context: RwLock::new(None),
        });

        let sess_reader = Arc::clone(&session);
        std::thread::Builder::new()
            .name(format!("oterm-pty-{id}"))
            .spawn(move || {
                let mut buf = vec![0u8; PTY_READ_BUF_SIZE];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = Bytes::copy_from_slice(&buf[..n]);
                            let end_seq = {
                                let mut ring = sess_reader.ring.lock().unwrap();
                                ring.push(data.clone())
                            };
                            let _ = sess_reader.tx.send(OutputChunk { end_seq, data });
                        }
                        Err(e) => {
                            debug!("PTY read ended for session {}: {}", sess_reader.id, e);
                            break;
                        }
                    }
                }

                let code = match child.wait() {
                    Ok(status) => Some(status.exit_code() as i32),
                    Err(_) => None,
                };
                *sess_reader.exit_code.lock().unwrap() = code;
                sess_reader.alive.store(false, Ordering::SeqCst);

                let notice = format!(
                    "\r\n\x1b[38;5;244m[oterm: session ended (code {:?})]\x1b[0m\r\n",
                    code
                );
                let data = Bytes::from(notice);
                let end_seq = {
                    let mut ring = sess_reader.ring.lock().unwrap();
                    ring.push(data.clone())
                };
                let _ = sess_reader.tx.send(OutputChunk { end_seq, data });
                info!("PTY session {} exited with {:?}", sess_reader.id, code);
            })
            .context("failed to spawn PTY reader thread")?;

        info!(
            "attached PTY session {} (tmux={}, {}x{})",
            session.id,
            session.tmux_session.is_some(),
            cols,
            rows
        );
        Ok(session)
    }

    pub fn info(&self) -> SessionInfo {
        let (cols, rows) = *self.size.lock().unwrap();
        let end_seq = self.ring.lock().unwrap().end_seq();
        SessionInfo {
            id: self.id.clone(),
            name: self.name.read().unwrap().clone(),
            cols,
            rows,
            alive: self.alive.load(Ordering::SeqCst),
            exit_code: *self.exit_code.lock().unwrap(),
            created_at_ms: self.created_at_ms,
            clients: self.clients.load(Ordering::SeqCst),
            end_seq,
        }
    }

    pub fn rename(&self, new_name: String) {
        let trimmed = new_name.trim();
        if !trimmed.is_empty() {
            let clean: String = trimmed.chars().take(48).collect();
            *self.name.write().unwrap() = clean.clone();
            if let (Some(sock), Some(tname)) = (&self.tmux_socket, &self.tmux_session) {
                let _ = Command::new("tmux")
                    .args(["-L", sock, "set-option", "-t", tname, "@oterm_name", &clean])
                    .status();
            }
        }
    }

    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        if !self.alive.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.clamp(2, 1000);
        let rows = rows.clamp(1, 500);
        {
            let mut size = self.size.lock().unwrap();
            if *size == (cols, rows) {
                return Ok(());
            }
            *size = (cols, rows);
        }
        let master = self.master.lock().unwrap();
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("PTY resize failed: {e}"))?;
        Ok(())
    }

    /// Subscribe to live output BEFORE taking a snapshot to guarantee zero gaps.
    pub fn subscribe_and_slice(&self, last_seq: u64) -> (broadcast::Receiver<OutputChunk>, ReplaySlice) {
        let rx = self.tx.subscribe();
        let slice = self.ring.lock().unwrap().slice_from(last_seq);
        (rx, slice)
    }

    pub fn slice_from(&self, last_seq: u64) -> ReplaySlice {
        self.ring.lock().unwrap().slice_from(last_seq)
    }

    /// Capture a clean, collapsed visible screen + recent scrollback snapshot directly from tmux
    /// with exact cursor alignment. Collapses megabytes of intermediate TUI/progress redraws
    /// into ~15-35 KiB so unlock/reconnect converges to the latest screen state instantaneously.
    pub fn capture_fast_snapshot(&self, max_history: usize) -> Option<Bytes> {
        let (sock, tname) = (self.tmux_socket.as_deref()?, self.tmux_session.as_deref()?);
        capture_tmux_pane_snapshot(sock, tname, max_history)
    }

    pub fn capture_full_snapshot(&self) -> Option<Bytes> {
        self.capture_fast_snapshot(350)
    }

    pub fn inc_clients(&self) {
        self.clients.fetch_add(1, Ordering::SeqCst);
    }

    pub fn dec_clients(&self) {
        self.clients.fetch_sub(1, Ordering::SeqCst);
    }

    /// Return cached shell context if fresh (within CONTEXT_CACHE_TTL), otherwise probe live.
    /// Avoids spawning a `tmux display-message` subprocess on every autocomplete keystroke.
    pub fn inspect_context_cached(&self) -> SessionContext {
        if let Ok(guard) = self.cached_context.read() {
            if let Some((ts, ref ctx)) = *guard {
                if ts.elapsed() < CONTEXT_CACHE_TTL {
                    return ctx.clone();
                }
            }
        }
        self.inspect_context()
    }

    /// Invalidate cached context (e.g., when Enter/Ctrl+C is pressed) so next probe refreshes immediately.
    pub fn invalidate_context_cache(&self) {
        if let Ok(mut guard) = self.cached_context.write() {
            *guard = None;
        }
    }

    /// Inspect live shell context (cwd, foreground process, alt-screen, termios ECHO)
    /// for safe, zero-hallucination autocomplete.
    pub fn inspect_context(&self) -> SessionContext {
        let ctx = self.inspect_context_uncached();
        if let Ok(mut guard) = self.cached_context.write() {
            *guard = Some((Instant::now(), ctx.clone()));
        }
        ctx
    }

    fn inspect_context_uncached(&self) -> SessionContext {
        if let (Some(sock), Some(tname)) = (&self.tmux_socket, &self.tmux_session) {
            if let Some(ctx) = inspect_tmux_context(sock, tname) {
                return ctx;
            }
        }
        if let Some(pid) = self.child_pid {
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
                .unwrap_or_else(|_| self.fallback_cwd.clone());
            let fg_command = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "bash".to_string());
            let git_branch = read_git_branch(&cwd);
            return SessionContext {
                cwd,
                fg_command,
                git_branch,
                alt_screen: false,
                echo_enabled: true,
            };
        }
        let git_branch = read_git_branch(&self.fallback_cwd);
        SessionContext {
            cwd: self.fallback_cwd.clone(),
            fg_command: "bash".to_string(),
            git_branch,
            alt_screen: false,
            echo_enabled: true,
        }
    }

    pub fn kill(&self) {
        if let (Some(sock), Some(tname)) = (&self.tmux_socket, &self.tmux_session) {
            let _ = Command::new("tmux")
                .args(["-L", sock, "kill-session", "-t", tname])
                .status();
        }
        if let Some(mut killer) = self.child_killer.lock().unwrap().take() {
            if let Err(e) = killer.kill() {
                warn!("failed to kill session {}: {}", self.id, e);
            }
        }
        self.alive.store(false, Ordering::SeqCst);
    }
}

pub struct SessionManager {
    sessions: RwLock<HashMap<String, Arc<TerminalSession>>>,
    order: RwLock<Vec<String>>,
    shell: String,
    cwd: PathBuf,
    ring_capacity: usize,
    max_sessions: usize,
    tmux_socket: Option<String>,
    counter: AtomicUsize,
}

impl SessionManager {
    pub fn new(
        shell: String,
        cwd: PathBuf,
        ring_capacity: usize,
        max_sessions: usize,
        tmux_socket: Option<String>,
    ) -> Self {
        let active_tmux = tmux_socket.filter(|_| is_tmux_available());
        if let Some(ref sock) = active_tmux {
            info!("tmux persistence backend enabled (socket={sock})");
        } else {
            info!("tmux unavailable or disabled; using direct PTY sessions");
        }

        let mgr = Self {
            sessions: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            shell,
            cwd,
            ring_capacity,
            max_sessions,
            tmux_socket: active_tmux,
            counter: AtomicUsize::new(1),
        };
        mgr.recover_tmux_sessions();
        mgr
    }

    /// Re-discover and re-attach any surviving `oterm_*` tmux sessions after a server restart.
    fn recover_tmux_sessions(&self) {
        let Some(ref sock) = self.tmux_socket else {
            return;
        };
        let output = match Command::new("tmux")
            .args([
                "-L",
                sock,
                "list-sessions",
                "-F",
                "#{session_name}\t#{@oterm_id}\t#{@oterm_name}\t#{@oterm_created_ms}\t#{window_width}\t#{window_height}",
            ])
            .output()
        {
            Ok(out) if out.status.success() => out,
            _ => return,
        };

        let text = String::from_utf8_lossy(&output.stdout);
        let mut recovered = 0usize;
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 6 {
                continue;
            }
            let tmux_name = parts[0].trim();
            if !tmux_name.starts_with(TMUX_SESSION_PREFIX) {
                continue;
            }
            let id = if !parts[1].trim().is_empty() {
                parts[1].trim().to_string()
            } else {
                tmux_name.trim_start_matches(TMUX_SESSION_PREFIX).to_string()
            };
            let name = if !parts[2].trim().is_empty() {
                parts[2].trim().to_string()
            } else {
                format!("term-{}", recovered + 1)
            };
            let created_at_ms = parts[3].trim().parse::<u64>().unwrap_or_else(|_| now_ms());
            let cols = parts[4].trim().parse::<u16>().unwrap_or(120).clamp(2, 1000);
            let rows = parts[5].trim().parse::<u16>().unwrap_or(36).clamp(1, 500);

            configure_tmux_session(sock, tmux_name, &id, &name, created_at_ms);

            match TerminalSession::attach_tmux(
                sock,
                tmux_name.to_string(),
                id.clone(),
                name,
                created_at_ms,
                self.cwd.clone(),
                cols,
                rows,
                self.ring_capacity,
                true,
            ) {
                Ok(sess) => {
                    self.sessions.write().unwrap().insert(id.clone(), sess);
                    self.order.write().unwrap().push(id);
                    recovered += 1;
                }
                Err(e) => {
                    warn!("failed to reattach tmux session {tmux_name}: {e}");
                }
            }
        }
        if recovered > 0 {
            self.counter.fetch_add(recovered, Ordering::SeqCst);
            info!("recovered {recovered} persistent tmux session(s)");
        }
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().unwrap();
        let order = self.order.read().unwrap();
        order
            .iter()
            .filter_map(|id| sessions.get(id).map(|s| s.info()))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<TerminalSession>> {
        self.sessions.read().unwrap().get(id).cloned()
    }

    pub fn create(
        &self,
        name: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<TerminalSession>> {
        {
            let sessions = self.sessions.read().unwrap();
            if sessions.len() >= self.max_sessions {
                bail!("maximum session limit ({}) reached", self.max_sessions);
            }
        }
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let session_name = name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("term-{n}"));
        let cols = cols.clamp(2, 1000);
        let rows = rows.clamp(1, 500);

        let session = if let Some(ref sock) = self.tmux_socket {
            match TerminalSession::spawn_tmux_new(
                sock,
                session_name.clone(),
                &self.shell,
                &self.cwd,
                cols,
                rows,
                self.ring_capacity,
            ) {
                Ok(s) => s,
                Err(e) => {
                    warn!("tmux spawn failed ({e}); falling back to direct PTY");
                    TerminalSession::spawn_direct(
                        session_name,
                        &self.shell,
                        &self.cwd,
                        cols,
                        rows,
                        self.ring_capacity,
                    )?
                }
            }
        } else {
            TerminalSession::spawn_direct(
                session_name,
                &self.shell,
                &self.cwd,
                cols,
                rows,
                self.ring_capacity,
            )?
        };

        let id = session.id.clone();
        self.sessions
            .write()
            .unwrap()
            .insert(id.clone(), Arc::clone(&session));
        self.order.write().unwrap().push(id);
        Ok(session)
    }

    pub fn ensure_default(&self) -> Result<Vec<SessionInfo>> {
        let existing = self.list();
        if !existing.is_empty() {
            return Ok(existing);
        }
        let _ = self.create(Some("main".to_string()), 120, 36)?;
        Ok(self.list())
    }

    pub fn delete(&self, id: &str) -> bool {
        let removed = self.sessions.write().unwrap().remove(id);
        self.order.write().unwrap().retain(|x| x != id);
        if let Some(sess) = removed {
            sess.kill();
            true
        } else {
            false
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn is_tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn ensure_tmux_server_defaults(socket: &str) {
    let _ = Command::new("tmux")
        .args(["-L", socket, "start-server"])
        .status();
    let _ = Command::new("tmux")
        .args(["-L", socket, "set-option", "-g", "history-limit", "100000"])
        .status();
    let _ = Command::new("tmux")
        .args([
            "-L",
            socket,
            "set-option",
            "-sg",
            "terminal-overrides",
            ",xterm*:smcup@:rmcup@:indn@:dl@:dl1@",
        ])
        .status();
}

fn configure_tmux_session(
    socket: &str,
    tmux_name: &str,
    id: &str,
    name: &str,
    created_at_ms: u64,
) {
    let server_opts: &[(&str, &str)] = &[
        ("default-terminal", "xterm-256color"),
        ("terminal-overrides", ",xterm*:smcup@:rmcup@:indn@:dl@:dl1@"),
        ("escape-time", "0"),
        ("focus-events", "on"),
        ("set-clipboard", "on"),
    ];
    for (k, v) in server_opts {
        let _ = Command::new("tmux")
            .args(["-L", socket, "set-option", "-sg", k, v])
            .status();
    }
    let _ = Command::new("tmux")
        .args(["-L", socket, "set-option", "-g", "history-limit", "100000"])
        .status();

    let created_str = created_at_ms.to_string();
    let session_opts: &[(&str, &str)] = &[
        ("status", "off"),
        ("history-limit", "100000"),
        ("mouse", "off"),
        ("aggressive-resize", "on"),
        ("@oterm_id", id),
        ("@oterm_name", name),
        ("@oterm_created_ms", &created_str),
    ];
    for (k, v) in session_opts {
        let _ = Command::new("tmux")
            .args(["-L", socket, "set-option", "-t", tmux_name, k, v])
            .status();
    }
}

fn capture_tmux_pane_history(socket: &str, tmux_name: &str) -> Option<Bytes> {
    capture_tmux_pane_snapshot(socket, tmux_name, 5000)
}

fn capture_tmux_pane_snapshot(socket: &str, tmux_name: &str, max_history: usize) -> Option<Bytes> {
    let meta = Command::new("tmux")
        .args([
            "-L",
            socket,
            "display-message",
            "-p",
            "-t",
            tmux_name,
            "#{pane_height}\t#{cursor_x}\t#{cursor_y}\t#{alternate_on}",
        ])
        .output()
        .ok()?;
    let (pane_h, cursor_x, cursor_y, alt_on) = if meta.status.success() {
        let s = String::from_utf8_lossy(&meta.stdout);
        let p: Vec<&str> = s.trim_end_matches('\n').split('\t').collect();
        if p.len() >= 4 {
            (
                p[0].trim().parse::<usize>().unwrap_or(24).max(1),
                p[1].trim().parse::<usize>().unwrap_or(0),
                p[2].trim().parse::<usize>().unwrap_or(0),
                p[3].trim() == "1",
            )
        } else {
            (24, 0, 0, false)
        }
    } else {
        (24, 0, 0, false)
    };

    let start_arg = if alt_on {
        "0".to_string()
    } else {
        format!("-{max_history}")
    };

    let out = Command::new("tmux")
        .args([
            "-L",
            socket,
            "capture-pane",
            "-p",
            "-e",
            "-S",
            &start_arg,
            "-t",
            tmux_name,
        ])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }

    // Drop leading completely empty lines only if they came before actual content
    let first_non_empty = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
    // Always preserve at least the visible pane_h rows at the bottom
    let keep_from = first_non_empty.min(lines.len().saturating_sub(pane_h));
    let slice = &lines[keep_from..];

    let mut rendered = String::with_capacity(out.stdout.len() + 64);
    for (i, line) in slice.iter().enumerate() {
        if i > 0 {
            rendered.push_str("\r\n");
        }
        rendered.push_str(line);
    }
    // Reset attributes and align cursor to exact tmux pane cursor position
    rendered.push_str("\x1b[0m");
    let bottom_row = pane_h.saturating_sub(1);
    if cursor_y < bottom_row {
        let up = bottom_row - cursor_y;
        rendered.push_str(&format!("\x1b[{up}A"));
    }
    rendered.push_str(&format!("\x1b[{}G", cursor_x + 1));
    // Re-enable bracketed paste mode after client fullReset so multi-line pastes
    // are always delivered atomically via \x1b[200~...\x1b[201~ through tmux.
    rendered.push_str("\x1b[?2004h");

    Some(Bytes::from(rendered))
}

fn inspect_tmux_context(socket: &str, tmux_name: &str) -> Option<SessionContext> {
    let out = Command::new("tmux")
        .args([
            "-L",
            socket,
            "display-message",
            "-p",
            "-t",
            tmux_name,
            "#{pane_current_command}\t#{pane_current_path}\t#{alternate_on}\t#{pane_tty}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = line.trim_end_matches('\n').split('\t').collect();
    if parts.len() < 4 {
        return None;
    }
    let fg_command = parts[0].trim().to_string();
    let cwd = PathBuf::from(parts[1].trim());
    let alt_screen = parts[2].trim() == "1";
    let pane_tty = parts[3].trim();
    let echo_enabled = check_tty_echo(pane_tty).unwrap_or(true);
    let git_branch = read_git_branch(&cwd);

    Some(SessionContext {
        cwd,
        fg_command,
        git_branch,
        alt_screen,
        echo_enabled,
    })
}

fn read_git_branch(start: &std::path::Path) -> Option<String> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let dot_git = dir.join(".git");
        let head_path = if dot_git.is_dir() {
            Some(dot_git.join("HEAD"))
        } else if dot_git.is_file() {
            std::fs::read_to_string(&dot_git).ok().and_then(|c| {
                c.trim().strip_prefix("gitdir:").map(|rel| {
                    let p = PathBuf::from(rel.trim());
                    if p.is_absolute() {
                        p.join("HEAD")
                    } else {
                        dir.join(p).join("HEAD")
                    }
                })
            })
        } else {
            None
        };
        if let Some(hp) = head_path {
            if let Ok(head) = std::fs::read_to_string(hp) {
                let h = head.trim();
                if let Some(branch) = h.strip_prefix("ref: refs/heads/") {
                    return Some(branch.to_string());
                }
                if h.len() >= 7 {
                    return Some(h[..7].to_string());
                }
            }
            return None;
        }
        cur = dir.parent();
    }
    None
}

fn check_tty_echo(tty_path: &str) -> Option<bool> {
    if tty_path.is_empty() {
        return None;
    }
    let file = OpenOptions::new().read(true).open(tty_path).ok()?;
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::tcgetattr(file.as_raw_fd(), &mut termios) };
    if rc == 0 {
        // Interactive shells (GNU Readline / ZLE) run in non-canonical mode with
        // ECHO off because they echo characters themselves. Password prompts
        // (sudo, read -s, passwd) use canonical mode with ECHO off.
        let icanon = (termios.c_lflag & libc::ICANON) != 0;
        let echo = (termios.c_lflag & libc::ECHO) != 0;
        Some(!(icanon && !echo))
    } else {
        None
    }
}
