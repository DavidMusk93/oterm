use crate::protocol::SessionInfo;
use crate::ringbuf::{ReplaySlice, SeqRingBuffer};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use uuid::Uuid;

const BROADCAST_CAPACITY: usize = 512;
const PTY_READ_BUF_SIZE: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub end_seq: u64,
    pub data: Bytes,
}

pub struct TerminalSession {
    pub id: String,
    name: RwLock<String>,
    created_at_ms: u64,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    ring: Mutex<SeqRingBuffer>,
    tx: broadcast::Sender<OutputChunk>,
    size: Mutex<(u16, u16)>,
    alive: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    clients: AtomicUsize,
    child_killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
}

impl TerminalSession {
    pub fn spawn(
        name: String,
        shell: &str,
        cwd: &PathBuf,
        cols: u16,
        rows: u16,
        ring_capacity: usize,
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

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn shell `{shell}`"))?;
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
        let id = Uuid::new_v4().simple().to_string()[..12].to_string();
        let created_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let session = Arc::new(Self {
            id: id.clone(),
            name: RwLock::new(name),
            created_at_ms,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            ring: Mutex::new(SeqRingBuffer::new(ring_capacity)),
            tx,
            size: Mutex::new((cols, rows)),
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            clients: AtomicUsize::new(0),
            child_killer: Mutex::new(Some(killer)),
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
                    "\r\n\x1b[38;5;244m[oterm: process exited (code {:?})]\x1b[0m\r\n",
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

        info!("spawned PTY session {} ({shell} {}x{})", session.id, cols, rows);
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
            *self.name.write().unwrap() = trimmed.chars().take(48).collect();
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

    pub fn inc_clients(&self) {
        self.clients.fetch_add(1, Ordering::SeqCst);
    }

    pub fn dec_clients(&self) {
        self.clients.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn kill(&self) {
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
    counter: AtomicUsize,
}

impl SessionManager {
    pub fn new(shell: String, cwd: PathBuf, ring_capacity: usize, max_sessions: usize) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            shell,
            cwd,
            ring_capacity,
            max_sessions,
            counter: AtomicUsize::new(1),
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
        let session = TerminalSession::spawn(
            session_name,
            &self.shell,
            &self.cwd,
            cols.clamp(2, 1000),
            rows.clamp(1, 500),
            self.ring_capacity,
        )?;
        let id = session.id.clone();
        self.sessions.write().unwrap().insert(id.clone(), Arc::clone(&session));
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
