import {
  decodeServerFrame,
  encodeAck,
  encodeActivity,
  encodeAttach,
  encodeCmdRecord,
  encodeCompleteReq,
  encodeInput,
  encodePing,
  encodeResize,
  type SessionInfo,
} from "./protocol";
import { OtermView } from "./terminal";
import { initMobileSupport, type MobileController } from "./mobile";
import { AutocompleteController } from "./autocomplete";
import { LocalEchoOverlay, isInteractiveShell } from "./local_echo";

const WS_CLOSE_AUTH_EXPIRED = 4401;
const PING_INTERVAL_MS = 10_000;
const PONG_TIMEOUT_MS = 5_000;
const ACTIVITY_THROTTLE_MS = 2_000;
const ACK_COALESCE_MS = 80;
const ACK_IMMEDIATE_BYTES = 32 * 1024;
const PENDING_INPUT_MAX_BYTES = 2048;
const PENDING_INPUT_MAX_AGE_MS = 10_000;
const ACTIVE_SESSION_STORAGE_KEY = "oterm_active_session";

class OtermApp {
  private view!: OtermView;
  private mobile!: MobileController;
  private autocomplete!: AutocompleteController;
  private localEcho!: LocalEchoOverlay;
  private ws: WebSocket | null = null;
  private sessions: SessionInfo[] = [];
  private activeSessionId: string | null = localStorage.getItem(ACTIVE_SESSION_STORAGE_KEY);
  // Monotonic sequence numbers preserved across reconnects AND idle OTP lock/unlock!
  private lastSeqBySession = new Map<string, bigint>();
  private pendingAckSeq: bigint | null = null;
  private pendingAckBytes = 0;
  private ackFlushTimer: number | null = null;
  private pendingInputs: Array<{ data: string; ts: number }> = [];
  private pendingInputBytes = 0;
  private wsEverOpenedThisAttempt = false;

  private authenticated = false;
  private hasEverUnlocked = false;

  private lastTouchSentAtMs = 0;
  private activityFlushTimer: number | null = null;

  private reconnectAttempt = 0;
  private reconnectTimer: number | null = null;
  private pingTimer: number | null = null;
  private pongWatchdog: number | null = null;

  private searchRegex = false;
  private searchCase = false;

  // DOM elements
  private connDot = document.getElementById("conn-dot")!;
  private rttBadge = document.getElementById("rtt-badge")!;
  private sizeBadge = document.getElementById("size-badge")!;
  private tabsBar = document.getElementById("tabs-bar")!;
  private btnLock = document.getElementById("btn-lock")!;
  private btnAcToggle = document.getElementById("btn-ac-toggle")!;
  private stickyPill = document.getElementById("sticky-history-pill")!;
  private stickyLinesText = document.getElementById("sticky-lines-text")!;
  private scrollFab = document.getElementById("btn-scroll-bottom")!;
  private ctxCwd = document.getElementById("ctx-cwd")!;
  private ctxBranch = document.getElementById("ctx-branch")!;
  private ctxProc = document.getElementById("ctx-proc")!;
  private btnCopyBlock = document.getElementById("btn-copy-block")!;

  private authOverlay = document.getElementById("auth-overlay")!;
  private authTitle = document.getElementById("auth-title")!;
  private authSubtitle = document.getElementById("auth-subtitle")!;
  private authError = document.getElementById("auth-error")!;
  private otpBoxes = document.getElementById("otp-boxes")!;
  private otpSlots = Array.from(document.querySelectorAll<HTMLElement>("#otp-boxes .otp-slot"));
  private otpStatus = document.getElementById("otp-status")!;
  private otpInput = document.getElementById("otp-input") as HTMLInputElement;
  private authForm = document.getElementById("auth-form") as HTMLFormElement;
  private otpSubmitting = false;

  private searchBar = document.getElementById("search-bar")!;
  private searchInput = document.getElementById("search-input") as HTMLInputElement;

  public async start(): Promise<void> {
    this.mobile = initMobileSupport(
      (data) => this.sendInput(data),
      () => this.view.pasteFromClipboard(),
      () => this.view?.scheduleFit()
    );

    const mount = document.getElementById("terminal-mount")!;
    this.view = new OtermView(mount, {
      onInput: (data) => this.sendInput(data),
      onResize: (cols, rows) => {
        this.sizeBadge.textContent = `${cols}×${rows}`;
        if (this.ws && this.ws.readyState === WebSocket.OPEN && this.activeSessionId) {
          this.ws.send(encodeResize(cols, rows));
        }
      },
      onAck: (endSeq, byteCount) => {
        if (this.activeSessionId) {
          this.lastSeqBySession.set(this.activeSessionId, endSeq);
        }
        this.queueAck(endSeq, byteCount);
      },
      onOpenSearch: () => this.openSearch(),
      onScrollStateChange: (state) => {
        this.stickyPill.classList.toggle("hidden", state.atBottom);
        if (!state.atBottom) {
          this.stickyLinesText.textContent = `↑ ${state.linesAbove} lines above`;
        }
      },
      transformStickyModifiers: (data) => this.mobile.transformInput(data),
      onKeyDown: (ev) => (this.autocomplete ? this.autocomplete.handleKeyDown(ev) : true),
      onRendered: () => {
        this.localEcho?.onTerminalRendered();
        this.autocomplete?.onTerminalRendered();
      },
      onLayoutChanged: () => {
        this.localEcho?.invalidateMetrics();
        this.autocomplete?.invalidateMetrics();
      },
    });

    const terminalWrap = document.getElementById("terminal-wrap")!;
    this.localEcho = new LocalEchoOverlay(
      this.view.term,
      terminalWrap,
      document.getElementById("local-echo")!
    );

    this.autocomplete = new AutocompleteController(
      this.view.term,
      terminalWrap,
      document.getElementById("ac-ghost")!,
      document.getElementById("ac-popup")!,
      {
        onRequest: (seq, line) => {
          if (this.ws && this.ws.readyState === WebSocket.OPEN && this.activeSessionId) {
            this.ws.send(encodeCompleteReq(seq, line));
          }
        },
        onAccept: (inputToSend) => {
          this.sendInput(inputToSend);
        },
        onRecordCommand: (cmd) => {
          if (this.ws && this.ws.readyState === WebSocket.OPEN && this.activeSessionId) {
            this.ws.send(encodeCmdRecord(cmd));
          }
        },
      }
    );

    this.bindUI();
    this.bindActivityListeners();

    // Instant reconnect on network recovery, tab wake, or window focus
    const ensureConnected = () => {
      if (!this.authenticated) return;
      if (!this.ws || this.ws.readyState === WebSocket.CLOSED || this.ws.readyState === WebSocket.CLOSING) {
        this.reconnectAttempt = 0;
        if (this.reconnectTimer !== null) {
          window.clearTimeout(this.reconnectTimer);
          this.reconnectTimer = null;
        }
        this.connectWs();
      } else if (this.ws.readyState === WebSocket.OPEN) {
        this.sendPing();
      }
    };
    window.addEventListener("online", ensureConnected);
    window.addEventListener("focus", ensureConnected);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") {
        ensureConnected();
      }
    });

    await this.checkAuthStatus();
  }

  private bindUI(): void {
    // Theme selector
    const themeSel = document.getElementById("theme-select") as HTMLSelectElement;
    const savedTheme = localStorage.getItem("oterm_theme") || "tokyo";
    themeSel.value = savedTheme;
    this.view.setTheme(savedTheme);
    themeSel.addEventListener("change", () => {
      this.view.setTheme(themeSel.value);
      this.view.focus();
    });

    // Font buttons
    document.getElementById("btn-font-dec")!.addEventListener("click", () => {
      this.view.adjustFontSize(-1);
      this.view.focus();
    });
    document.getElementById("btn-font-inc")!.addEventListener("click", () => {
      this.view.adjustFontSize(1);
      this.view.focus();
    });

    // Autocomplete toggle
    this.btnAcToggle.classList.toggle("active", this.autocomplete.isEnabled());
    this.btnAcToggle.addEventListener("click", () => {
      const enabled = this.autocomplete.toggle();
      this.btnAcToggle.classList.toggle("active", enabled);
      this.view.focus();
    });

    // Mobile bar toggle
    document.getElementById("btn-mobile-bar")!.addEventListener("click", () => {
      this.mobile.toggleBar();
      this.view.focus();
    });

    // Scroll to bottom & Warp-style command block navigation
    this.scrollFab.addEventListener("click", () => {
      this.view.scrollToBottom();
      this.view.focus();
    });
    document.getElementById("btn-sticky-prev")!.addEventListener("click", () => {
      this.view.jumpPrevCommand();
      this.view.focus();
    });
    document.getElementById("btn-sticky-next")!.addEventListener("click", () => {
      this.view.jumpNextCommand();
      this.view.focus();
    });
    document.getElementById("btn-prev-cmd")!.addEventListener("click", () => {
      this.view.jumpPrevCommand();
      this.view.focus();
    });
    document.getElementById("btn-next-cmd")!.addEventListener("click", () => {
      this.view.jumpNextCommand();
      this.view.focus();
    });
    this.btnCopyBlock.addEventListener("click", async () => {
      const ok = await this.view.copyLastBlock();
      if (ok) {
        this.btnCopyBlock.textContent = "✓ Copied";
        window.setTimeout(() => {
          this.btnCopyBlock.textContent = "⧉ Copy Output";
        }, 1400);
      }
      this.view.focus();
    });

    // New session tab
    document.getElementById("btn-new-tab")!.addEventListener("click", () => {
      this.createSession();
    });

    // Manual lock button
    this.btnLock.addEventListener("click", async () => {
      await fetch("./api/auth/logout", { method: "POST" }).catch(() => {});
      this.lockSession("Locked manually. Your terminal sessions continue running in the background.");
    });

    // Search bar
    document.getElementById("btn-search")!.addEventListener("click", () => this.openSearch());
    const btnRegex = document.getElementById("search-regex")!;
    const btnCase = document.getElementById("search-case")!;
    btnRegex.addEventListener("click", () => {
      this.searchRegex = !this.searchRegex;
      btnRegex.classList.toggle("active", this.searchRegex);
      this.view.searchNext(this.searchInput.value, {
        regex: this.searchRegex,
        caseSensitive: this.searchCase,
      });
    });
    btnCase.addEventListener("click", () => {
      this.searchCase = !this.searchCase;
      btnCase.classList.toggle("active", this.searchCase);
      this.view.searchNext(this.searchInput.value, {
        regex: this.searchRegex,
        caseSensitive: this.searchCase,
      });
    });
    document.getElementById("search-next")!.addEventListener("click", () => {
      this.view.searchNext(this.searchInput.value, {
        regex: this.searchRegex,
        caseSensitive: this.searchCase,
      });
    });
    document.getElementById("search-prev")!.addEventListener("click", () => {
      this.view.searchPrev(this.searchInput.value, {
        regex: this.searchRegex,
        caseSensitive: this.searchCase,
      });
    });
    document.getElementById("search-close")!.addEventListener("click", () => this.closeSearch());
    this.searchInput.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        this.closeSearch();
      } else if (e.key === "Enter") {
        e.preventDefault();
        if (e.shiftKey) {
          this.view.searchPrev(this.searchInput.value, {
            regex: this.searchRegex,
            caseSensitive: this.searchCase,
          });
        } else {
          this.view.searchNext(this.searchInput.value, {
            regex: this.searchRegex,
            caseSensitive: this.searchCase,
          });
        }
      }
    });

    // Apple-style 6-box OTP input with instant auto-login on 6th digit
    this.otpBoxes.addEventListener("click", () => {
      this.otpInput.focus();
    });
    this.otpInput.addEventListener("focus", () => this.renderOtpSlots());
    this.otpInput.addEventListener("blur", () => this.renderOtpSlots());
    this.otpInput.addEventListener("input", () => {
      const digits = this.otpInput.value.replace(/\D/g, "").slice(0, 6);
      if (this.otpInput.value !== digits) {
        this.otpInput.value = digits;
      }
      this.otpBoxes.classList.remove("error", "shake");
      this.authError.classList.add("hidden");
      this.renderOtpSlots();
      if (digits.length === 6 && !this.otpSubmitting) {
        this.submitOtp();
      }
    });
    this.otpInput.addEventListener("paste", (e) => {
      const text = e.clipboardData?.getData("text") ?? "";
      const digits = text.replace(/\D/g, "").slice(0, 6);
      if (digits.length > 0) {
        e.preventDefault();
        this.otpInput.value = digits;
        this.otpBoxes.classList.remove("error", "shake");
        this.authError.classList.add("hidden");
        this.renderOtpSlots();
        if (digits.length === 6 && !this.otpSubmitting) {
          this.submitOtp();
        }
      }
    });
    this.authForm.addEventListener("submit", (e) => {
      e.preventDefault();
      if (!this.otpSubmitting) {
        this.submitOtp();
      }
    });
  }

  private bindActivityListeners(): void {
    const onUserEvent = (ev: Event) => {
      if (!this.authenticated) return;
      const target = ev.target instanceof Element ? ev.target : null;
      if (target?.closest("#btn-lock, #auth-overlay")) return;
      this.recordUserActivity(false);
    };

    window.addEventListener("keydown", onUserEvent, { capture: true });
    window.addEventListener("pointerdown", onUserEvent, { capture: true, passive: true });
    window.addEventListener("pointermove", onUserEvent, { capture: true, passive: true });
    window.addEventListener("wheel", onUserEvent, { capture: true, passive: true });
    window.addEventListener("touchstart", onUserEvent, { capture: true, passive: true });
    window.addEventListener("paste", onUserEvent, { capture: true });
    document.addEventListener("selectionchange", onUserEvent);
  }

  private recordUserActivity(sentViaInput: boolean): void {
    if (!this.authenticated) return;
    const now = Date.now();

    if (sentViaInput) {
      this.lastTouchSentAtMs = now;
      if (this.activityFlushTimer !== null) {
        window.clearTimeout(this.activityFlushTimer);
        this.activityFlushTimer = null;
      }
      return;
    }

    const elapsed = now - this.lastTouchSentAtMs;
    if (elapsed >= ACTIVITY_THROTTLE_MS) {
      this.flushActivityTouch();
    } else if (this.activityFlushTimer === null) {
      this.activityFlushTimer = window.setTimeout(() => {
        this.activityFlushTimer = null;
        this.flushActivityTouch();
      }, ACTIVITY_THROTTLE_MS - elapsed);
    }
  }

  private flushActivityTouch(): void {
    if (!this.authenticated) return;
    this.lastTouchSentAtMs = Date.now();
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(encodeActivity());
    } else {
      fetch("./api/auth/touch", { method: "POST" }).catch(() => {});
    }
  }

  private openSearch(): void {
    this.searchBar.classList.remove("hidden");
    this.searchInput.focus();
    this.searchInput.select();
  }

  private closeSearch(): void {
    this.searchBar.classList.add("hidden");
    this.view.clearSearch();
    this.view.focus();
  }

  private async checkAuthStatus(): Promise<void> {
    try {
      const resp = await fetch("./api/auth/status", { cache: "no-store" });
      const data = (await resp.json()) as {
        authenticated: boolean;
        ttl_secs: number;
        auth_ttl_max?: number;
      };
      if (data.auth_ttl_max && data.auth_ttl_max > 0) {
        const mins = Math.max(1, Math.round(data.auth_ttl_max / 60));
        this.btnLock.title = `Lock terminal now (auto-locks after ${mins} min idle; sessions stay alive)`;
      }
      if (data.authenticated) {
        this.onAuthenticated(data.auth_ttl_max);
      } else {
        this.showAuthOverlay(
          "OTP Verification",
          "Enter the 6-digit code from your authenticator app."
        );
      }
    } catch {
      this.showAuthOverlay("Connection Error", "Cannot reach oterm server. Retrying...");
      window.setTimeout(() => this.checkAuthStatus(), 2000);
    }
  }

  private renderOtpSlots(): void {
    const val = this.otpInput.value;
    const focused = document.activeElement === this.otpInput && !this.otpInput.disabled;
    const activeIdx = focused && val.length < 6 ? val.length : -1;

    for (let i = 0; i < this.otpSlots.length; i++) {
      const slot = this.otpSlots[i];
      const ch = val[i] ?? "";
      slot.textContent = ch;
      slot.classList.toggle("filled", ch.length > 0);
      slot.classList.toggle("active", i === activeIdx);
    }
  }

  private triggerOtpError(message: string): void {
    this.authError.textContent = message;
    this.authError.classList.remove("hidden");
    this.otpBoxes.classList.remove("verifying", "shake");
    this.otpBoxes.classList.add("error");
    // Restart shake animation cleanly
    void this.otpBoxes.offsetWidth;
    this.otpBoxes.classList.add("shake");
    this.otpInput.value = "";
    this.otpInput.disabled = false;
    this.otpSubmitting = false;
    this.otpStatus.classList.add("hidden");
    this.renderOtpSlots();
    this.otpInput.focus();
  }

  private async submitOtp(): Promise<void> {
    const code = this.otpInput.value.trim();
    if (code.length !== 6 || this.otpSubmitting) return;

    this.otpSubmitting = true;
    this.authError.classList.add("hidden");
    this.otpBoxes.classList.remove("error", "shake");
    this.otpBoxes.classList.add("verifying");
    this.otpStatus.classList.remove("hidden");
    this.otpInput.disabled = true;
    this.renderOtpSlots();

    try {
      const resp = await fetch("./api/auth/verify", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code }),
      });
      const data = await resp.json();
      if (!resp.ok) {
        this.triggerOtpError(data.message || "Invalid OTP code");
        return;
      }

      this.otpInput.value = "";
      this.otpInput.disabled = false;
      this.otpSubmitting = false;
      this.otpBoxes.classList.remove("verifying", "error", "shake");
      this.otpStatus.classList.add("hidden");
      this.renderOtpSlots();
      this.onAuthenticated(data.auth_ttl_max);
    } catch {
      this.triggerOtpError("Network error verifying OTP.");
    }
  }

  private async onAuthenticated(ttlMax?: number): Promise<void> {
    if (ttlMax && ttlMax > 0) {
      const mins = Math.max(1, Math.round(ttlMax / 60));
      this.btnLock.title = `Lock terminal now (auto-locks after ${mins} min idle; sessions stay alive)`;
    }
    this.authenticated = true;
    this.lastTouchSentAtMs = Date.now();
    this.hasEverUnlocked = true;
    this.authOverlay.classList.add("hidden");
    this.reconnectAttempt = 0;
    this.view.focus();

    // Fast path: if we already know activeSessionId (e.g. unlocking from idle lock),
    // open WS immediately in parallel with refreshing session list to halve resume latency!
    const prevSessionId = this.activeSessionId;
    if (prevSessionId) {
      this.connectWs();
    }

    await this.refreshSessions();
    if (!this.authenticated) return;

    if (!prevSessionId) {
      this.connectWs();
    } else if (this.activeSessionId && this.activeSessionId !== prevSessionId) {
      this.attachActiveSession(true);
    }
  }

  private lockSession(message: string): void {
    this.authenticated = false;
    this.localEcho?.clear();
    this.autocomplete?.clear();
    this.resetPendingAck();
    this.pendingInputs = [];
    this.pendingInputBytes = 0;
    this.stopHeartbeat();
    if (this.activityFlushTimer !== null) {
      window.clearTimeout(this.activityFlushTimer);
      this.activityFlushTimer = null;
    }
    if (this.reconnectTimer !== null) {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      try {
        this.ws.close(1000, "locked");
      } catch {
        // ignore
      }
      this.ws = null;
    }
    this.setConnState("locked");
    // NOTE: We intentionally do NOT clear xterm.js or lastSeqBySession!
    // Terminal content stays intact underneath the overlay.
    this.showAuthOverlay(
      this.hasEverUnlocked ? "Session Locked (Idle Timeout)" : "OTP Verification",
      message
    );
  }

  private showAuthOverlay(title: string, subtitle: string): void {
    this.authTitle.textContent = title;
    this.authSubtitle.textContent = subtitle;
    this.authOverlay.classList.remove("hidden");
    this.authError.classList.add("hidden");
    this.otpStatus.classList.add("hidden");
    this.otpBoxes.classList.remove("verifying", "error", "shake");
    this.otpSubmitting = false;
    this.otpInput.disabled = false;
    this.otpInput.value = "";
    this.renderOtpSlots();
    window.setTimeout(() => {
      this.otpInput.focus();
      this.renderOtpSlots();
    }, 40);
  }

  private async refreshSessions(): Promise<void> {
    const resp = await fetch("./api/sessions", { cache: "no-store" });
    if (resp.status === 401) {
      this.lockSession("Session locked after inactivity. Enter OTP to resume without losing terminal state.");
      return;
    }
    const data = (await resp.json()) as { sessions: SessionInfo[] };
    this.sessions = data.sessions || [];
    if (!this.activeSessionId || !this.sessions.some((s) => s.id === this.activeSessionId)) {
      this.activeSessionId = this.sessions[0]?.id ?? null;
    }
    if (this.activeSessionId) {
      localStorage.setItem(ACTIVE_SESSION_STORAGE_KEY, this.activeSessionId);
    }
    this.renderTabs();
  }

  private async createSession(): Promise<void> {
    const { cols, rows } = this.view.fitNow();
    const resp = await fetch("./api/sessions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ cols, rows }),
    });
    if (resp.status === 401) {
      this.lockSession("OTP session expired. Re-authenticate to continue.");
      return;
    }
    if (!resp.ok) return;
    const info = (await resp.json()) as SessionInfo;
    this.sessions.push(info);
    this.switchSession(info.id);
  }

  private async deleteSession(id: string): Promise<void> {
    const resp = await fetch(`./api/sessions/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
    if (resp.status === 401) {
      this.lockSession("OTP session expired. Re-authenticate to continue.");
      return;
    }
    this.lastSeqBySession.delete(id);
    await this.refreshSessions();
    if (this.activeSessionId) {
      this.attachActiveSession(true);
    }
  }

  private switchSession(id: string): void {
    if (this.activeSessionId === id) return;
    this.activeSessionId = id;
    localStorage.setItem(ACTIVE_SESSION_STORAGE_KEY, id);
    this.localEcho?.clear();
    this.autocomplete?.clear();
    this.resetPendingAck();
    this.pendingInputs = [];
    this.pendingInputBytes = 0;
    this.renderTabs();
    // Force full snapshot when switching tabs into the shared xterm view
    this.attachActiveSession(true);
    this.view.focus();
  }

  private renderTabs(): void {
    this.tabsBar.innerHTML = "";
    for (const sess of this.sessions) {
      const tab = document.createElement("div");
      tab.className = "tab" + (sess.id === this.activeSessionId ? " active" : "");

      const dot = document.createElement("span");
      dot.className = "tab-dot " + (sess.alive ? "alive" : "exited");
      tab.appendChild(dot);

      const label = document.createElement("span");
      label.className = "tab-name";
      label.textContent = sess.name;
      tab.appendChild(label);

      tab.addEventListener("click", () => this.switchSession(sess.id));
      tab.addEventListener("dblclick", async () => {
        const next = window.prompt("Rename session:", sess.name);
        if (!next || !next.trim()) return;
        const r = await fetch(`./api/sessions/${encodeURIComponent(sess.id)}`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ name: next.trim() }),
        });
        if (r.ok) {
          sess.name = next.trim();
          this.renderTabs();
        }
      });

      if (this.sessions.length > 1) {
        const close = document.createElement("button");
        close.className = "tab-close";
        close.textContent = "×";
        close.title = "Close & kill session";
        close.addEventListener("click", (e) => {
          e.stopPropagation();
          this.deleteSession(sess.id);
        });
        tab.appendChild(close);
      }

      this.tabsBar.appendChild(tab);
    }
  }

  private connectWs(): void {
    if (!this.authenticated) return;
    if (this.ws) {
      try {
        this.ws.close();
      } catch {
        // ignore
      }
      this.ws = null;
    }

    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const base = location.pathname.endsWith("/")
      ? location.pathname
      : `${location.pathname}/`;
    const wsUrl = `${proto}//${location.host}${base}ws`;

    this.setConnState("connecting");
    this.wsEverOpenedThisAttempt = false;
    const ws = new WebSocket(wsUrl);
    ws.binaryType = "arraybuffer";
    this.ws = ws;

    ws.onopen = () => {
      this.wsEverOpenedThisAttempt = true;
      this.reconnectAttempt = 0;
      this.setConnState("online");
      this.attachActiveSession(false);
      this.flushPendingInputs();
      this.startHeartbeat();
    };

    ws.onmessage = (ev) => {
      if (!(ev.data instanceof ArrayBuffer)) return;
      const frame = decodeServerFrame(ev.data);
      if (!frame) return;

      switch (frame.type) {
        case "output":
          this.view.writeChunk(frame.data, frame.endSeq, false);
          break;
        case "snapshot":
          this.view.writeChunk(frame.data, frame.endSeq, frame.fullReset);
          break;
        case "pong": {
          const rtt = Math.max(0, Number(BigInt(Date.now()) - frame.clientTsMs));
          this.rttBadge.textContent = `${rtt}ms`;
          this.localEcho?.updateRtt(rtt);
          if (this.pongWatchdog !== null) {
            window.clearTimeout(this.pongWatchdog);
            this.pongWatchdog = null;
          }
          break;
        }
        case "complete_res":
          this.autocomplete.onResponse(frame.seq, frame.items);
          break;
        case "context": {
          const shortCwd = frame.info.cwd.replace(/^\/root(?=\/|$)/, "~");
          this.ctxCwd.textContent = shortCwd || "~";
          this.ctxCwd.title = frame.info.cwd;
          if (frame.info.git_branch) {
            this.ctxBranch.textContent = `⎇ ${frame.info.git_branch}`;
            this.ctxBranch.classList.remove("hidden");
          } else {
            this.ctxBranch.classList.add("hidden");
          }
          this.ctxProc.textContent = `● ${frame.info.fg_command || "shell"}`;
          const echoEnabled = frame.info.echo_enabled !== false;
          const shellSafe =
            !frame.info.alt_screen && echoEnabled && isInteractiveShell(frame.info.fg_command);
          this.localEcho?.updateContext(frame.info.alt_screen, echoEnabled, frame.info.fg_command);
          this.autocomplete?.updateContext(shellSafe);
          break;
        }
        case "session": {
          const idx = this.sessions.findIndex((s) => s.id === frame.info.id);
          if (idx >= 0) {
            this.sessions[idx] = frame.info;
            this.renderTabs();
          }
          break;
        }
        case "auth_lock":
          this.lockSession(
            "Session locked after inactivity. Your terminal is untouched — enter OTP to resume."
          );
          break;
        case "error":
          console.warn("[oterm server error]", frame.code, frame.message);
          break;
      }
    };

    ws.onclose = (ev) => {
      this.stopHeartbeat();
      if (this.ws !== ws) return;
      this.ws = null;
      this.resetPendingAck();
      this.localEcho?.clear();

      if (ev.code === WS_CLOSE_AUTH_EXPIRED) {
        this.lockSession(
          "Session locked after inactivity. Your terminal is untouched — enter OTP to resume."
        );
        return;
      }
      if (!this.authenticated) return;

      this.setConnState("offline");
      this.scheduleReconnect();
    };

    ws.onerror = () => {
      // onclose handles reconnect
    };
  }

  private attachActiveSession(forceFullSnapshot: boolean): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN || !this.activeSessionId) {
      return;
    }
    const { cols, rows } = this.view.fitNow();
    const lastSeq = forceFullSnapshot
      ? 0n
      : (this.lastSeqBySession.get(this.activeSessionId) ?? 0n);
    this.ws.send(encodeAttach(this.activeSessionId, lastSeq, cols, rows));
  }

  private sendInput(data: string): void {
    if (!this.authenticated) return;
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      // Buffer keystrokes across brief network blips / reconnects so typing is never dropped
      if (this.pendingInputBytes + data.length <= PENDING_INPUT_MAX_BYTES) {
        this.pendingInputs.push({ data, ts: Date.now() });
        this.pendingInputBytes += data.length;
      }
      this.localEcho?.onUserInput(data);
      return;
    }
    this.ws.send(encodeInput(data));
    this.localEcho?.onUserInput(data);
    this.autocomplete?.onUserInput();
    this.recordUserActivity(true);
  }

  private flushPendingInputs(): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN || this.pendingInputs.length === 0) {
      return;
    }
    const now = Date.now();
    const items = this.pendingInputs;
    this.pendingInputs = [];
    this.pendingInputBytes = 0;
    let combined = "";
    for (const item of items) {
      if (now - item.ts <= PENDING_INPUT_MAX_AGE_MS) {
        combined += item.data;
      }
    }
    if (combined.length > 0) {
      this.ws.send(encodeInput(combined));
      this.recordUserActivity(true);
    }
  }

  private queueAck(endSeq: bigint, byteCount: number): void {
    this.pendingAckSeq = endSeq;
    this.pendingAckBytes += byteCount;
    if (this.pendingAckBytes >= ACK_IMMEDIATE_BYTES) {
      this.flushAck();
      return;
    }
    if (this.ackFlushTimer === null) {
      this.ackFlushTimer = window.setTimeout(() => {
        this.ackFlushTimer = null;
        this.flushAck();
      }, ACK_COALESCE_MS);
    }
  }

  private flushAck(): void {
    if (this.ackFlushTimer !== null) {
      window.clearTimeout(this.ackFlushTimer);
      this.ackFlushTimer = null;
    }
    if (this.pendingAckSeq !== null && this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(encodeAck(this.pendingAckSeq));
    }
    this.pendingAckSeq = null;
    this.pendingAckBytes = 0;
  }

  private resetPendingAck(): void {
    if (this.ackFlushTimer !== null) {
      window.clearTimeout(this.ackFlushTimer);
      this.ackFlushTimer = null;
    }
    this.pendingAckSeq = null;
    this.pendingAckBytes = 0;
  }

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.sendPing();
    this.pingTimer = window.setInterval(() => this.sendPing(), PING_INTERVAL_MS);
  }

  private sendPing(): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.ws.send(encodePing(BigInt(Date.now())));
    // Keep session alive while user is actively viewing the foreground tab
    // (even without typing/clicking, e.g. watching long-running tasks).
    // When tab is hidden/minimized/locked, stop renewing so idle lock triggers normally.
    if (
      this.authenticated &&
      document.visibilityState === "visible" &&
      Date.now() - this.lastTouchSentAtMs >= 20_000
    ) {
      this.flushActivityTouch();
    }
    if (this.pongWatchdog !== null) window.clearTimeout(this.pongWatchdog);
    this.pongWatchdog = window.setTimeout(() => {
      // Half-open connection detected; force reconnect
      try {
        this.ws?.close(4000, "pong_timeout");
      } catch {
        // ignore
      }
    }, PONG_TIMEOUT_MS);
  }

  private stopHeartbeat(): void {
    if (this.pingTimer !== null) {
      window.clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
    if (this.pongWatchdog !== null) {
      window.clearTimeout(this.pongWatchdog);
      this.pongWatchdog = null;
    }
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer !== null || !this.authenticated) return;
    // Fast exponential backoff (200ms -> 400ms -> 800ms -> 1.6s -> 3s cap) for harsh networks
    const baseDelay = Math.min(3000, 200 * Math.pow(2, this.reconnectAttempt));
    const jitter = Math.floor(Math.random() * 150);
    const shouldCheckAuth = !this.wsEverOpenedThisAttempt && this.reconnectAttempt >= 1;
    this.reconnectAttempt++;
    this.reconnectTimer = window.setTimeout(async () => {
      this.reconnectTimer = null;
      if (shouldCheckAuth) {
        try {
          const r = await fetch("./api/auth/status", { cache: "no-store" });
          const d = (await r.json()) as {
            authenticated: boolean;
            ttl_secs: number;
            auth_ttl_max?: number;
          };
          if (!d.authenticated) {
            this.lockSession(
              "Session locked after inactivity. Your terminal is untouched — enter OTP to resume."
            );
            return;
          }
        } catch {
          this.scheduleReconnect();
          return;
        }
      }
      this.connectWs();
    }, baseDelay + jitter);
  }

  private setConnState(state: "online" | "connecting" | "offline" | "locked"): void {
    this.connDot.className = `dot dot-${state}`;
    if (state !== "online") {
      this.rttBadge.textContent = state === "locked" ? "locked" : "--ms";
    }
  }
}

new OtermApp().start();
