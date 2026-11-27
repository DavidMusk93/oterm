use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_RESULTS: usize = 8;
const MAX_DIR_ENTRIES: usize = 400;
const PATH_CACHE_TTL: Duration = Duration::from_secs(60);
const HALF_LIFE_DAYS: f64 = 14.0;

#[derive(Debug, Clone, Serialize)]
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub replace_len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ghost_suffix: Option<String>,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip)]
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompletionResponse {
    pub seq: u32,
    pub items: Vec<CompletionItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedHistoryRecord {
    cmd: String,
    cwd: String,
    ts: u64,
}

#[derive(Debug, Clone)]
struct HistoryEntry {
    cmd: String,
    count: u32,
    last_ts: u64,
    cwds: HashSet<String>,
}

struct HistoryStore {
    entries: HashMap<String, HistoryEntry>,
    /// Markov bigram transition frequencies: prev_cmd -> (next_cmd -> count)
    transitions: HashMap<String, HashMap<String, u32>>,
    last_cmd_by_session: HashMap<String, String>,
    store_path: PathBuf,
}

impl HistoryStore {
    fn load() -> Self {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/root"));
        let store_path = home.join(".local/share/oterm/history.jsonl");

        let mut store = Self {
            entries: HashMap::new(),
            transitions: HashMap::new(),
            last_cmd_by_session: HashMap::new(),
            store_path,
        };

        let now = now_unix_secs();
        store.ingest_shell_history(&home.join(".bash_history"), now);
        store.ingest_shell_history(&home.join(".zsh_history"), now);
        store.ingest_oterm_history();
        store
    }

    fn ingest_shell_history(&mut self, path: &Path, now: u64) {
        let Ok(raw) = fs::read(path) else {
            return;
        };
        let text = String::from_utf8_lossy(&raw);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len().max(1);
        let mut prev: Option<String> = None;

        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            // Handle zsh extended history: `: 1700000000:0;command`
            let (ts, cmd_str) = if let Some(rest) = trimmed.strip_prefix(": ") {
                if let Some((meta, cmd)) = rest.split_once(';') {
                    let parsed_ts = meta
                        .split(':')
                        .next()
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .unwrap_or_else(|| estimate_ts(now, idx, total));
                    (parsed_ts, cmd.trim())
                } else {
                    (estimate_ts(now, idx, total), trimmed)
                }
            } else {
                (estimate_ts(now, idx, total), trimmed)
            };

            if is_noise_command(cmd_str) {
                continue;
            }
            let cmd = cmd_str.to_string();
            self.insert_entry(&cmd, None, ts);
            if let Some(p) = prev.as_ref() {
                if p != &cmd {
                    *self
                        .transitions
                        .entry(p.clone())
                        .or_default()
                        .entry(cmd.clone())
                        .or_insert(0) += 1;
                }
            }
            prev = Some(cmd);
        }
    }

    fn ingest_oterm_history(&mut self) {
        let Ok(raw) = fs::read_to_string(&self.store_path) else {
            return;
        };
        let mut prev: Option<String> = None;
        for line in raw.lines() {
            let Ok(rec) = serde_json::from_str::<PersistedHistoryRecord>(line) else {
                continue;
            };
            if is_noise_command(&rec.cmd) {
                continue;
            }
            self.insert_entry(&rec.cmd, Some(&rec.cwd), rec.ts);
            if let Some(p) = prev.as_ref() {
                if p != &rec.cmd {
                    *self
                        .transitions
                        .entry(p.clone())
                        .or_default()
                        .entry(rec.cmd.clone())
                        .or_insert(0) += 1;
                }
            }
            prev = Some(rec.cmd);
        }
    }

    fn insert_entry(&mut self, cmd: &str, cwd: Option<&str>, ts: u64) {
        let entry = self
            .entries
            .entry(cmd.to_string())
            .or_insert_with(|| HistoryEntry {
                cmd: cmd.to_string(),
                count: 0,
                last_ts: ts,
                cwds: HashSet::new(),
            });
        entry.count = entry.count.saturating_add(1);
        if ts > entry.last_ts {
            entry.last_ts = ts;
        }
        if let Some(dir) = cwd {
            if !dir.is_empty() {
                entry.cwds.insert(dir.to_string());
            }
        }
    }

    fn record(&mut self, session_id: &str, cmd: &str, cwd: &Path) {
        let trimmed = cmd.trim();
        if is_noise_command(trimmed) {
            return;
        }
        let ts = now_unix_secs();
        let cwd_str = cwd.to_string_lossy().to_string();
        self.insert_entry(trimmed, Some(&cwd_str), ts);

        if let Some(prev) = self.last_cmd_by_session.get(session_id) {
            if prev != trimmed {
                *self
                    .transitions
                    .entry(prev.clone())
                    .or_default()
                    .entry(trimmed.to_string())
                    .or_insert(0) += 1;
            }
        }
        self.last_cmd_by_session
            .insert(session_id.to_string(), trimmed.to_string());

        if let Some(parent) = self.store_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.store_path)
        {
            let rec = PersistedHistoryRecord {
                cmd: trimmed.to_string(),
                cwd: cwd_str,
                ts,
            };
            if let Ok(line) = serde_json::to_string(&rec) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

pub struct CompletionEngine {
    history: RwLock<HistoryStore>,
    path_bins: RwLock<(Instant, Arc<Vec<String>>)>,
}

impl CompletionEngine {
    pub fn new() -> Self {
        let bins = Arc::new(scan_path_executables());
        Self {
            history: RwLock::new(HistoryStore::load()),
            path_bins: RwLock::new((Instant::now(), bins)),
        }
    }

    pub fn record_command(&self, session_id: &str, cmd: &str, cwd: &Path) {
        if let Ok(mut hist) = self.history.write() {
            hist.record(session_id, cmd, cwd);
        }
    }

    pub fn complete(&self, session_id: &str, line: &str, cwd: &Path, seq: u32) -> CompletionResponse {
        if line.trim().is_empty() {
            return CompletionResponse {
                seq,
                items: Vec::new(),
            };
        }

        let mut candidates: Vec<CompletionItem> = Vec::new();
        let now = now_unix_secs();
        let cwd_str = cwd.to_string_lossy().to_string();

        // 1. Whole-line history completion (Frecency + CWD Affinity + Markov Bigram)
        self.collect_history_candidates(session_id, line, &cwd_str, now, &mut candidates);

        // 2. Tokenize active command segment (after &&, ||, ;, |)
        let segment = active_command_segment(line);
        let (tokens, trailing_space) = split_tokens(segment);

        if tokens.is_empty() {
            return finalize_candidates(seq, candidates);
        }

        let first = tokens[0];
        let is_first_token = tokens.len() == 1 && !trailing_space;

        if is_first_token {
            if first.starts_with("./")
                || first.starts_with('/')
                || first.starts_with("~/")
                || first.starts_with("..")
            {
                collect_path_candidates(first, cwd, false, true, &mut candidates);
            } else {
                self.collect_executable_candidates(first, &mut candidates);
            }
        } else {
            let current_arg = if trailing_space {
                ""
            } else {
                tokens.last().copied().unwrap_or("")
            };
            let prev_arg = if trailing_space {
                tokens.last().copied().unwrap_or("")
            } else if tokens.len() >= 2 {
                tokens[tokens.len() - 2]
            } else {
                ""
            };
            let subcmd = if tokens.len() >= 2 && !(tokens.len() == 2 && !trailing_space) {
                Some(tokens[1])
            } else {
                None
            };

            // 2a. Curated CLI knowledge graph (subcommands & flags)
            collect_cli_spec_candidates(first, subcmd, current_arg, &mut candidates);

            // 2b. Pure-Rust Git context (branches & remotes read directly from .git/)
            if first == "git" {
                let sub = subcmd.unwrap_or(prev_arg);
                if matches!(
                    sub,
                    "checkout" | "switch" | "merge" | "rebase" | "diff" | "branch" | "push" | "pull" | "reset" | "log"
                ) && !current_arg.starts_with('-')
                {
                    collect_git_ref_candidates(cwd, current_arg, &mut candidates);
                }
            }

            // 2c. Real-time filesystem path completion
            let dirs_only = matches!(first, "cd" | "pushd" | "rmdir");
            if !current_arg.starts_with('-') || current_arg.is_empty() {
                collect_path_candidates(current_arg, cwd, dirs_only, false, &mut candidates);
            }
        }

        finalize_candidates(seq, candidates)
    }

    fn collect_history_candidates(
        &self,
        session_id: &str,
        line: &str,
        cwd_str: &str,
        now: u64,
        out: &mut Vec<CompletionItem>,
    ) {
        let Ok(hist) = self.history.read() else {
            return;
        };
        let prev_cmd = hist.last_cmd_by_session.get(session_id);
        let trans_map = prev_cmd.and_then(|p| hist.transitions.get(p));
        let line_len = line.chars().count();

        for entry in hist.entries.values() {
            if entry.cmd.len() <= line.len() {
                continue;
            }
            // Prefer exact prefix for crisp ghost text; allow fuzzy only for multi-word or >=5-char queries
            let (match_score, is_prefix) = if entry.cmd.starts_with(line) {
                (1150.0, true)
            } else if line_len >= 5 || line.contains(' ') {
                match fuzzy_score(line, &entry.cmd) {
                    Some(s) if s >= 560.0 => (s * 0.52, false),
                    _ => continue,
                }
            } else {
                continue;
            };

            let age_days = now.saturating_sub(entry.last_ts) as f64 / 86400.0;
            let decay = 0.5_f64.powf(age_days / HALF_LIFE_DAYS);
            let frecency = (entry.count as f64).ln_1p() * 160.0 * decay + 40.0 * decay;

            let mut cwd_boost = 0.0;
            let mut in_same_dir = false;
            if entry.cwds.contains(cwd_str) {
                cwd_boost = 420.0;
                in_same_dir = true;
            } else if entry
                .cwds
                .iter()
                .any(|d| cwd_str.starts_with(d) || d.starts_with(cwd_str))
            {
                cwd_boost = 150.0;
            }

            let trans_boost = trans_map
                .and_then(|m| m.get(&entry.cmd))
                .map(|&c| 210.0 * (c as f64).ln_1p())
                .unwrap_or(0.0);

            let total_score = match_score + frecency + cwd_boost + trans_boost;

            let (insert_text, replace_len, ghost_suffix) = if is_prefix {
                let suffix = entry.cmd[line.len()..].to_string();
                (suffix.clone(), 0, Some(suffix))
            } else {
                (entry.cmd.clone(), line_len, None)
            };

            let detail = if trans_boost > 0.0 {
                Some("next step".to_string())
            } else if in_same_dir {
                Some(format!("this dir · {}×", entry.count))
            } else if entry.count > 1 {
                Some(format!("history · {}×", entry.count))
            } else {
                Some("history".to_string())
            };

            out.push(CompletionItem {
                label: entry.cmd.clone(),
                insert_text,
                replace_len,
                ghost_suffix,
                kind: "history",
                detail,
                score: total_score,
            });
        }
    }

    fn collect_executable_candidates(&self, prefix: &str, out: &mut Vec<CompletionItem>) {
        // Refresh PATH cache if stale (O(1) Arc clone on cache hit)
        let bins: Arc<Vec<String>> = {
            let needs_refresh = self
                .path_bins
                .read()
                .map(|g| g.0.elapsed() > PATH_CACHE_TTL)
                .unwrap_or(false);
            if needs_refresh {
                let fresh = Arc::new(scan_path_executables());
                if let Ok(mut g) = self.path_bins.write() {
                    *g = (Instant::now(), Arc::clone(&fresh));
                }
                fresh
            } else {
                self.path_bins
                    .read()
                    .map(|g| Arc::clone(&g.1))
                    .unwrap_or_else(|_| Arc::new(Vec::new()))
            }
        };

        let prefix_len = prefix.chars().count();
        for bin in BUILTIN_COMMANDS.iter().copied().chain(bins.iter().map(|s| s.as_str())) {
            if bin == prefix {
                continue;
            }
            let Some(score) = fuzzy_score(prefix, bin) else {
                continue;
            };
            if score < 380.0 {
                continue;
            }
            let is_prefix = bin.starts_with(prefix);
            let (insert_text, replace_len, ghost_suffix) = if is_prefix {
                let suffix = bin[prefix.len()..].to_string();
                (suffix.clone(), 0, Some(suffix))
            } else {
                (bin.to_string(), prefix_len, None)
            };
            let curated_bonus = if CLI_SPECS.iter().any(|s| s.cmd == bin) {
                120.0
            } else {
                0.0
            };
            out.push(CompletionItem {
                label: bin.to_string(),
                insert_text,
                replace_len,
                ghost_suffix,
                kind: "cmd",
                detail: cli_summary(bin).map(str::to_string),
                score: score * 0.68 + curated_bonus,
            });
        }
    }
}

fn finalize_candidates(seq: u32, mut candidates: Vec<CompletionItem>) -> CompletionResponse {
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.label.len().cmp(&b.label.len()))
    });

    let mut seen = HashSet::new();
    let mut items = Vec::with_capacity(MAX_RESULTS);
    for item in candidates {
        if item.insert_text.is_empty() {
            continue;
        }
        if seen.insert(item.label.clone()) {
            items.push(item);
            if items.len() >= MAX_RESULTS {
                break;
            }
        }
    }
    CompletionResponse { seq, items }
}

/// Custom fzf-inspired subsequence + prefix + word-boundary fuzzy scorer.
pub fn fuzzy_score(pattern: &str, candidate: &str) -> Option<f64> {
    if pattern.is_empty() {
        return Some(300.0);
    }
    if candidate.starts_with(pattern) {
        let len_penalty = (candidate.len().saturating_sub(pattern.len()) as f64) * 1.2;
        return Some((1000.0 - len_penalty).max(650.0));
    }

    // Fast zero-allocation ASCII subsequence pre-check to reject 95%+ non-matches immediately
    if pattern.is_ascii() && candidate.is_ascii() {
        let pb = pattern.as_bytes();
        let cb = candidate.as_bytes();
        if pb.len() > cb.len() {
            return None;
        }
        let mut pi = 0usize;
        let mut target = pb[0].to_ascii_lowercase();
        for &b in cb {
            if b.to_ascii_lowercase() == target {
                pi += 1;
                if pi == pb.len() {
                    break;
                }
                target = pb[pi].to_ascii_lowercase();
            }
        }
        if pi < pb.len() {
            return None;
        }
    }

    let pat_lower: Vec<char> = pattern.to_lowercase().chars().collect();
    let cand_chars: Vec<char> = candidate.chars().collect();
    let cand_lower: Vec<char> = cand_chars.iter().map(|c| c.to_ascii_lowercase()).collect();

    if cand_lower.starts_with(&pat_lower) {
        let len_penalty = (cand_lower.len().saturating_sub(pat_lower.len()) as f64) * 1.2;
        return Some((920.0 - len_penalty).max(600.0));
    }

    let mut pi = 0usize;
    let mut score = 0.0_f64;
    let mut prev_match: Option<usize> = None;
    let mut consecutive = 0usize;

    for (ci, &ch) in cand_lower.iter().enumerate() {
        if pi < pat_lower.len() && ch == pat_lower[pi] {
            let mut bonus = 30.0;
            if ci == 0 {
                bonus += 45.0;
            } else {
                let prev = cand_chars[ci - 1];
                if matches!(prev, '/' | '-' | '_' | '.' | ' ') {
                    bonus += 35.0;
                } else if prev.is_ascii_lowercase() && cand_chars[ci].is_ascii_uppercase() {
                    bonus += 28.0;
                }
            }
            if let Some(pm) = prev_match {
                if ci == pm + 1 {
                    consecutive += 1;
                    bonus += 22.0 + (consecutive as f64 * 6.0).min(30.0);
                } else {
                    consecutive = 0;
                    let gap = (ci - pm - 1) as f64;
                    score -= (gap * 3.5).min(35.0);
                }
            }
            score += bonus;
            prev_match = Some(ci);
            pi += 1;
        }
    }

    if pi < pat_lower.len() {
        return None;
    }

    // Normalize into 0..850 range, favoring tighter matches
    let density = (pat_lower.len() as f64) / (cand_lower.len().max(1) as f64);
    let final_score = 320.0 + score + density * 140.0;
    Some(final_score.min(860.0))
}

fn collect_path_candidates(
    token: &str,
    cwd: &Path,
    dirs_only: bool,
    exec_only: bool,
    out: &mut Vec<CompletionItem>,
) {
    let (dir_part, name_prefix) = match token.rfind('/') {
        Some(pos) => (&token[..=pos], &token[pos + 1..]),
        None => ("", token),
    };

    let search_dir = if dir_part.is_empty() {
        cwd.to_path_buf()
    } else if let Some(rest) = dir_part.strip_prefix("~/") {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/root"));
        home.join(rest)
    } else if dir_part == "~/" {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/root"))
    } else if dir_part.starts_with('/') {
        PathBuf::from(dir_part)
    } else {
        cwd.join(dir_part)
    };

    let Ok(entries) = fs::read_dir(&search_dir) else {
        return;
    };

    let show_hidden = name_prefix.starts_with('.');
    let name_prefix_len = name_prefix.chars().count();

    for entry in entries.take(MAX_DIR_ENTRIES).flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && file_name.starts_with('.') {
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let is_dir = meta.is_dir();
        if dirs_only && !is_dir {
            continue;
        }
        if exec_only && !is_dir {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
        }

        let Some(score) = fuzzy_score(name_prefix, &file_name) else {
            continue;
        };
        if !name_prefix.is_empty() && score < 380.0 {
            continue;
        }

        let escaped = escape_path_token(&file_name);
        let display = if is_dir {
            format!("{dir_part}{file_name}/")
        } else {
            format!("{dir_part}{file_name}")
        };
        let full_escaped = if is_dir {
            format!("{escaped}/")
        } else {
            escaped
        };

        let is_prefix = full_escaped.starts_with(name_prefix);
        let (insert_text, replace_len, ghost_suffix) = if is_prefix {
            let suffix = full_escaped[name_prefix.len()..].to_string();
            (suffix.clone(), 0, Some(suffix))
        } else {
            (full_escaped, name_prefix_len, None)
        };

        let kind_boost = if is_dir && dirs_only {
            160.0
        } else if is_dir {
            60.0
        } else {
            20.0
        };

        out.push(CompletionItem {
            label: display,
            insert_text,
            replace_len,
            ghost_suffix,
            kind: if is_dir { "dir" } else { "file" },
            detail: Some(if is_dir { "directory" } else { "file" }.to_string()),
            score: score * 0.78 + kind_boost,
        });
    }
}

fn collect_git_ref_candidates(cwd: &Path, prefix: &str, out: &mut Vec<CompletionItem>) {
    let Some(git_dir) = find_git_dir(cwd) else {
        return;
    };
    let mut refs: Vec<(String, &'static str)> = Vec::new();

    // Local branches: .git/refs/heads/**
    collect_refs_recursive(&git_dir.join("refs/heads"), "", "branch", &mut refs);
    // Remote branches: .git/refs/remotes/**
    collect_refs_recursive(&git_dir.join("refs/remotes"), "", "remote", &mut refs);

    // Packed refs
    if let Ok(packed) = fs::read_to_string(git_dir.join("packed-refs")) {
        for line in packed.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((_, ref_name)) = line.split_once(' ') {
                if let Some(b) = ref_name.strip_prefix("refs/heads/") {
                    refs.push((b.to_string(), "branch"));
                } else if let Some(r) = ref_name.strip_prefix("refs/remotes/") {
                    if !r.ends_with("/HEAD") {
                        refs.push((r.to_string(), "remote"));
                    }
                }
            }
        }
    }

    let prefix_len = prefix.chars().count();
    for (name, kind_label) in refs {
        if name == prefix {
            continue;
        }
        let Some(score) = fuzzy_score(prefix, &name) else {
            continue;
        };
        if !prefix.is_empty() && score < 380.0 {
            continue;
        }
        let is_prefix = name.starts_with(prefix);
        let (insert_text, replace_len, ghost_suffix) = if is_prefix {
            let suffix = name[prefix.len()..].to_string();
            (suffix.clone(), 0, Some(suffix))
        } else {
            (name.clone(), prefix_len, None)
        };
        let local_boost = if kind_label == "branch" { 130.0 } else { 50.0 };
        out.push(CompletionItem {
            label: name,
            insert_text,
            replace_len,
            ghost_suffix,
            kind: "git",
            detail: Some(kind_label.to_string()),
            score: score * 0.82 + local_boost,
        });
    }
}

fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            if let Ok(content) = fs::read_to_string(&dot_git) {
                if let Some(rel) = content.trim().strip_prefix("gitdir:") {
                    let p = PathBuf::from(rel.trim());
                    return Some(if p.is_absolute() { p } else { dir.join(p) });
                }
            }
        }
        cur = dir.parent();
    }
    None
}

fn collect_refs_recursive(
    dir: &Path,
    prefix: &str,
    label: &'static str,
    out: &mut Vec<(String, &'static str)>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let full = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.path().is_dir() {
            collect_refs_recursive(&entry.path(), &full, label, out);
        } else if !full.ends_with("/HEAD") {
            out.push((full, label));
        }
    }
}

fn collect_cli_spec_candidates(
    cmd: &str,
    subcmd: Option<&str>,
    current_arg: &str,
    out: &mut Vec<CompletionItem>,
) {
    let Some(spec) = CLI_SPECS.iter().find(|s| s.cmd == cmd) else {
        return;
    };
    let arg_len = current_arg.chars().count();

    if subcmd.is_none() && !current_arg.starts_with('-') {
        for &(sub, desc) in spec.subcommands {
            if sub == current_arg {
                continue;
            }
            let Some(score) = fuzzy_score(current_arg, sub) else {
                continue;
            };
            if !current_arg.is_empty() && score < 380.0 {
                continue;
            }
            let is_prefix = sub.starts_with(current_arg);
            let (insert_text, replace_len, ghost_suffix) = if is_prefix {
                let suffix = sub[current_arg.len()..].to_string();
                (suffix.clone(), 0, Some(suffix))
            } else {
                (sub.to_string(), arg_len, None)
            };
            out.push(CompletionItem {
                label: sub.to_string(),
                insert_text,
                replace_len,
                ghost_suffix,
                kind: "cmd",
                detail: Some(desc.to_string()),
                score: score * 0.85 + 90.0,
            });
        }
    }

    if current_arg.starts_with('-') {
        for &(flag, desc) in spec.flags {
            if flag == current_arg {
                continue;
            }
            let Some(score) = fuzzy_score(current_arg, flag) else {
                continue;
            };
            if score < 380.0 {
                continue;
            }
            let is_prefix = flag.starts_with(current_arg);
            let (insert_text, replace_len, ghost_suffix) = if is_prefix {
                let suffix = flag[current_arg.len()..].to_string();
                (suffix.clone(), 0, Some(suffix))
            } else {
                (flag.to_string(), arg_len, None)
            };
            out.push(CompletionItem {
                label: flag.to_string(),
                insert_text,
                replace_len,
                ghost_suffix,
                kind: "flag",
                detail: Some(desc.to_string()),
                score: score * 0.82 + 70.0,
            });
        }
    }
}

fn active_command_segment(line: &str) -> &str {
    // Take only the command after the last unquoted pipeline/chain operator
    let mut last_sep = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b';' | b'|' | b'&' if !in_single && !in_double => {
                last_sep = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    line[last_sep..].trim_start()
}

fn split_tokens(segment: &str) -> (Vec<&str>, bool) {
    let trailing_space = segment.ends_with(' ') || segment.ends_with('\t');
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    (tokens, trailing_space)
}

fn escape_path_token(name: &str) -> String {
    if name.contains([' ', '\'', '"', '(', ')', '&', ';', '$']) {
        name.replace(' ', "\\ ")
    } else {
        name.to_string()
    }
}

fn is_noise_command(cmd: &str) -> bool {
    let t = cmd.trim();
    t.len() < 2
        || t.len() > 240
        || t.contains('\n')
        || t.contains('\r')
        || matches!(t, "ls" | "ll" | "la" | "cd" | "pwd" | "clear" | "exit" | "history")
}

fn estimate_ts(now: u64, idx: usize, total: usize) -> u64 {
    let frac = 1.0 - (idx as f64 / total.max(1) as f64);
    let age_secs = (frac * 30.0 * 86400.0) as u64;
    now.saturating_sub(age_secs)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn scan_path_executables() -> Vec<String> {
    let mut set = HashSet::new();
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.is_empty() && !name.starts_with('.') {
                set.insert(name);
            }
        }
    }
    let mut vec: Vec<String> = set.into_iter().collect();
    vec.sort();
    vec
}

const BUILTIN_COMMANDS: &[&str] = &[
    "cd", "export", "alias", "unalias", "source", "history", "jobs", "fg", "bg", "kill", "unset",
    "pwd", "echo", "printf", "type", "which", "set", "ulimit", "umask", "exit",
];

struct CliSpec {
    cmd: &'static str,
    summary: &'static str,
    subcommands: &'static [(&'static str, &'static str)],
    flags: &'static [(&'static str, &'static str)],
}

fn cli_summary(cmd: &str) -> Option<&'static str> {
    CLI_SPECS.iter().find(|s| s.cmd == cmd).map(|s| s.summary)
}

const CLI_SPECS: &[CliSpec] = &[
    CliSpec {
        cmd: "git",
        summary: "Distributed version control",
        subcommands: &[
            ("status", "Show working tree status"),
            ("diff", "Show changes between commits/tree"),
            ("add", "Stage file contents"),
            ("commit", "Record changes to repository"),
            ("push", "Update remote refs"),
            ("pull", "Fetch and integrate remote branch"),
            ("fetch", "Download objects and refs"),
            ("checkout", "Switch branches or restore files"),
            ("switch", "Switch branches"),
            ("branch", "List, create, or delete branches"),
            ("log", "Show commit logs"),
            ("stash", "Stash uncommitted changes"),
            ("rebase", "Reapply commits on another base"),
            ("merge", "Join development histories"),
            ("reset", "Reset current HEAD to state"),
            ("clone", "Clone a repository"),
            ("show", "Show commit or object"),
            ("cherry-pick", "Apply specific commits"),
        ],
        flags: &[
            ("--oneline", "Compact one-line commit log"),
            ("--stat", "Show diffstat summary"),
            ("--staged", "Show staged changes"),
            ("--amend", "Amend previous commit"),
            ("--force-with-lease", "Safer force push"),
            ("--no-ff", "Create merge commit"),
            ("-m", "Commit message"),
            ("-p", "Interactive patch mode"),
        ],
    },
    CliSpec {
        cmd: "cargo",
        summary: "Rust package manager",
        subcommands: &[
            ("build", "Compile the current package"),
            ("run", "Run a binary or example"),
            ("test", "Execute unit and integration tests"),
            ("check", "Analyze without producing binary"),
            ("clippy", "Run Rust lints"),
            ("fmt", "Format Rust source code"),
            ("clean", "Remove target artifacts"),
            ("add", "Add dependency to Cargo.toml"),
            ("update", "Update Cargo.lock dependencies"),
            ("doc", "Build package documentation"),
            ("bench", "Run benchmarks"),
        ],
        flags: &[
            ("--release", "Build optimized release profile"),
            ("--all-targets", "Check all targets"),
            ("--nocapture", "Show test stdout"),
            ("--locked", "Require Cargo.lock up to date"),
            ("--offline", "Run without network access"),
        ],
    },
    CliSpec {
        cmd: "systemctl",
        summary: "Control systemd services",
        subcommands: &[
            ("status", "Show service runtime status"),
            ("restart", "Restart a systemd unit"),
            ("start", "Start a systemd unit"),
            ("stop", "Stop a systemd unit"),
            ("reload", "Reload unit configuration"),
            ("daemon-reload", "Reload systemd unit files"),
            ("enable", "Enable unit on boot"),
            ("disable", "Disable unit on boot"),
            ("list-units", "List active units"),
            ("cat", "Inspect unit file contents"),
        ],
        flags: &[
            ("--no-pager", "Do not pipe into pager"),
            ("--now", "Also start/stop immediately"),
            ("--user", "Connect to user systemd"),
            ("--failed", "Show failed units"),
        ],
    },
    CliSpec {
        cmd: "journalctl",
        summary: "Query systemd journal logs",
        subcommands: &[],
        flags: &[
            ("-u", "Filter by systemd unit"),
            ("-f", "Follow new log entries"),
            ("-n", "Number of recent lines"),
            ("--since", "Show entries since timestamp"),
            ("--no-pager", "Disable pager output"),
            ("-xe", "Jump to end with explanations"),
        ],
    },
    CliSpec {
        cmd: "docker",
        summary: "Container runtime CLI",
        subcommands: &[
            ("ps", "List running containers"),
            ("logs", "Fetch container logs"),
            ("exec", "Run command in container"),
            ("compose", "Manage multi-container apps"),
            ("build", "Build image from Dockerfile"),
            ("images", "List local images"),
            ("run", "Create and run a container"),
            ("stop", "Stop running containers"),
            ("rm", "Remove containers"),
            ("inspect", "Return low-level JSON info"),
            ("system", "Manage Docker resources"),
        ],
        flags: &[
            ("-it", "Interactive TTY"),
            ("-f", "Follow log output"),
            ("--tail", "Lines from end of logs"),
            ("--rm", "Remove container on exit"),
            ("-d", "Run detached in background"),
        ],
    },
    CliSpec {
        cmd: "tmux",
        summary: "Terminal multiplexer",
        subcommands: &[
            ("ls", "List active sessions"),
            ("attach", "Attach to existing session"),
            ("new", "Create a new session"),
            ("kill-session", "Destroy a session"),
            ("capture-pane", "Dump pane contents"),
        ],
        flags: &[
            ("-L", "Specify tmux socket name"),
            ("-t", "Target session/window/pane"),
            ("-d", "Detach other clients / start detached"),
        ],
    },
    CliSpec {
        cmd: "npm",
        summary: "Node package manager",
        subcommands: &[
            ("run", "Run package script"),
            ("install", "Install dependencies"),
            ("ci", "Clean install from lockfile"),
            ("test", "Run test suite"),
            ("build", "Build project"),
            ("outdated", "Check outdated packages"),
        ],
        flags: &[
            ("--save-dev", "Save as devDependency"),
            ("--global", "Install globally"),
        ],
    },
    CliSpec {
        cmd: "kubectl",
        summary: "Kubernetes CLI",
        subcommands: &[
            ("get", "Display resources"),
            ("describe", "Show resource details"),
            ("logs", "Print container logs"),
            ("apply", "Apply manifest config"),
            ("exec", "Execute command in pod"),
            ("delete", "Delete resources"),
            ("rollout", "Manage deployments rollout"),
        ],
        flags: &[
            ("-n", "Target namespace"),
            ("-A", "All namespaces"),
            ("-o", "Output format (yaml/json/wide)"),
            ("-f", "Filename or follow logs"),
        ],
    },
    CliSpec {
        cmd: "rg",
        summary: "Ripgrep fast search",
        subcommands: &[],
        flags: &[
            ("-i", "Case-insensitive search"),
            ("-n", "Show line numbers"),
            ("-l", "Only print matching filenames"),
            ("--hidden", "Search hidden files"),
            ("--glob", "Include/exclude file glob"),
        ],
    },
    CliSpec {
        cmd: "curl",
        summary: "HTTP transfer tool",
        subcommands: &[],
        flags: &[
            ("-sS", "Silent but show errors"),
            ("-I", "Fetch response headers only"),
            ("-X", "HTTP request method"),
            ("-H", "Add request header"),
            ("-d", "POST request body"),
            ("-L", "Follow redirects"),
        ],
    },
    CliSpec {
        cmd: "ss",
        summary: "Socket statistics",
        subcommands: &[],
        flags: &[
            ("-tulpn", "Listening TCP/UDP sockets with PIDs"),
            ("-antp", "All TCP sockets with PIDs"),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_scoring_ranks_prefix_above_subsequence() {
        let p = fuzzy_score("sys", "systemctl").unwrap();
        let s = fuzzy_score("sys", "x_s_y_s_t").unwrap();
        assert!(p > s, "prefix {p} should beat scattered {s}");
        assert!(fuzzy_score("gch", "git checkout").is_some());
        assert!(fuzzy_score("xyz", "git status").is_none());
    }

    #[test]
    fn active_segment_handles_pipes_and_chains() {
        assert_eq!(active_command_segment("cargo build && git st"), "git st");
        assert_eq!(active_command_segment("cat foo | rg bar"), "rg bar");
        assert_eq!(active_command_segment("echo 'a | b'"), "echo 'a | b'");
    }
}
