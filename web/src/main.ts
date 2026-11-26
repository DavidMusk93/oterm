import {
  decodeServerFrame,
  encodeAck,
  encodeAttach,
  encodeInput,
  encodePing,
  encodeResize,
  type SessionInfo,
} from "./protocol";
import { OtermView } from "./terminal";
import { initMobileSupport, type MobileController } from "./mobile";

const WS_CLOSE_AUTH_EXPIRED = 4401;
const PING_INTERVAL_MS = 20_000;
const PONG_TIMEOUT_MS = 10_000;

class OtermApp {
  private view!: OtermView;
  private mobile!: MobileController;
  private ws: WebSocket | null = null;
  private sessions: SessionInfo[] = [];
  private activeSessionId: string | null = null;
  // Monotonic sequence numbers preserved across reconnects AND 5-min OTP lock/unlock!
  private lastSeqBySession = new Map<string, bigint>();

  private authenticated = false;
  private authTtlSecs = 0;
  private hasEverUnlocked = false;

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
  private ttlText = document.getElementById("ttl-text")!;
  private btnLock = document.getElementById("btn-lock")!;
  private scrollFab = document.getElementById("btn-scroll-bottom")!;

  private authOverlay = document.getElementById("auth-overlay")!;
  private authTitle = document.getElementById("auth-title")!;
  private authSubtitle = document.getElementById("auth-subtitle")!;
  private authError = document.getElementById("auth-error")!;
  private otpInput = document.getElementById("otp-input") as HTMLInputElement;
  private authForm = document.getElementById("auth-form") as HTMLFormElement;

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
      onAck: (endSeq) => {
        if (this.activeSessionId) {
          this.lastSeqBySession.set(this.activeSessionId, endSeq);
        }
        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
          this.ws.send(encodeAck(endSeq));
        }
      },
      onOpenSearch: () => this.openSearch(),
      onScrollStateChange: (atBottom) => {
        this.scrollFab.classList.toggle("hidden", atBottom);
      },
      transformStickyModifiers: (data) => this.mobile.transformInput(data),
    });

    this.bindUI();
    this.startTtlTicker();

    // Instant reconnect on network recovery or tab wake
    window.addEventListener("online", () => {
      if (this.authenticated && (!this.ws || this.ws.readyState !== WebSocket.OPEN)) {
        this.reconnectAttempt = 0;
        this.connectWs();
      }
    });
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible" && this.authenticated) {
        if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
          this.reconnectAttempt = 0;
          this.connectWs();
        } else {
          this.sendPing();
        }
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

    // Mobile bar toggle
    document.getElementById("btn-mobile-bar")!.addEventListener("click", () => {
      this.mobile.toggleBar();
      this.view.focus();
    });

    // Scroll to bottom FAB
    this.scrollFab.addEventListener("click", () => {
      this.view.scrollToBottom();
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

    // OTP Form with instant 6-digit auto-submit
    this.otpInput.addEventListener("input", () => {
      this.otpInput.value = this.otpInput.value.replace(/\D/g, "").slice(0, 6);
      if (this.otpInput.value.length === 6) {
        this.submitOtp();
      }
    });
    this.authForm.addEventListener("submit", (e) => {
      e.preventDefault();
      this.submitOtp();
    });
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
      };
      if (data.authenticated) {
        this.onAuthenticated(data.ttl_secs);
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

  private async submitOtp(): Promise<void> {
    const code = this.otpInput.value.trim();
    if (code.length !== 6) return;

    this.authError.classList.add("hidden");
    this.otpInput.disabled = true;

    try {
      const resp = await fetch("./api/auth/verify", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code }),
      });
      const data = await resp.json();
      if (!resp.ok) {
        this.authError.textContent = data.message || "Invalid OTP code";
        this.authError.classList.remove("hidden");
        this.otpInput.value = "";
        this.otpInput.disabled = false;
        this.otpInput.focus();
        return;
      }

      this.otpInput.value = "";
      this.otpInput.disabled = false;
      this.onAuthenticated(data.ttl_secs);
    } catch {
      this.authError.textContent = "Network error verifying OTP.";
      this.authError.classList.remove("hidden");
      this.otpInput.disabled = false;
      this.otpInput.focus();
    }
  }

  private async onAuthenticated(ttlSecs: number): Promise<void> {
    this.authenticated = true;
    this.authTtlSecs = ttlSecs;
    this.hasEverUnlocked = true;
    this.authOverlay.classList.add("hidden");
    this.updateTtlDisplay();

    await this.refreshSessions();
    this.reconnectAttempt = 0;
    this.connectWs();
    this.view.focus();
  }

  private lockSession(message: string): void {
    this.authenticated = false;
    this.authTtlSecs = 0;
    this.updateTtlDisplay();
    this.stopHeartbeat();
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
      this.hasEverUnlocked ? "Session Locked (OTP Expired)" : "OTP Verification",
      message
    );
  }

  private showAuthOverlay(title: string, subtitle: string): void {
    this.authTitle.textContent = title;
    this.authSubtitle.textContent = subtitle;
    this.authOverlay.classList.remove("hidden");
    this.authError.classList.add("hidden");
    this.otpInput.disabled = false;
    this.otpInput.value = "";
    window.setTimeout(() => this.otpInput.focus(), 40);
  }

  private async refreshSessions(): Promise<void> {
    const resp = await fetch("./api/sessions", { cache: "no-store" });
    if (resp.status === 401) {
      this.lockSession("5-minute OTP session expired. Enter OTP to resume without losing terminal state.");
      return;
    }
    const data = (await resp.json()) as { sessions: SessionInfo[] };
    this.sessions = data.sessions || [];
    if (!this.activeSessionId || !this.sessions.some((s) => s.id === this.activeSessionId)) {
      this.activeSessionId = this.sessions[0]?.id ?? null;
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
    const ws = new WebSocket(wsUrl);
    ws.binaryType = "arraybuffer";
    this.ws = ws;

    ws.onopen = () => {
      this.reconnectAttempt = 0;
      this.setConnState("online");
      this.attachActiveSession(false);
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
          const rtt = Number(BigInt(Date.now()) - frame.clientTsMs);
          this.rttBadge.textContent = `${Math.max(0, rtt)}ms`;
          this.authTtlSecs = frame.authTtlSecs;
          this.updateTtlDisplay();
          if (this.pongWatchdog !== null) {
            window.clearTimeout(this.pongWatchdog);
            this.pongWatchdog = null;
          }
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
            "5-minute OTP session expired. Your terminal is untouched — enter OTP to resume."
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

      if (ev.code === WS_CLOSE_AUTH_EXPIRED) {
        this.lockSession(
          "5-minute OTP session expired. Your terminal is untouched — enter OTP to resume."
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
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.ws.send(encodeInput(data));
  }

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.sendPing();
    this.pingTimer = window.setInterval(() => this.sendPing(), PING_INTERVAL_MS);
  }

  private sendPing(): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.ws.send(encodePing(BigInt(Date.now())));
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
    const baseDelay = Math.min(8000, 500 * Math.pow(2, this.reconnectAttempt));
    const jitter = Math.floor(Math.random() * 250);
    this.reconnectAttempt++;
    this.reconnectTimer = window.setTimeout(async () => {
      this.reconnectTimer = null;
      // Check if auth expired while offline
      try {
        const r = await fetch("./api/auth/status", { cache: "no-store" });
        const d = (await r.json()) as { authenticated: boolean; ttl_secs: number };
        if (!d.authenticated) {
          this.lockSession(
            "5-minute OTP session expired. Your terminal is untouched — enter OTP to resume."
          );
          return;
        }
        this.authTtlSecs = d.ttl_secs;
        this.updateTtlDisplay();
      } catch {
        this.scheduleReconnect();
        return;
      }
      this.connectWs();
    }, baseDelay + jitter);
  }

  private startTtlTicker(): void {
    window.setInterval(() => {
      if (!this.authenticated) return;
      if (this.authTtlSecs > 0) {
        this.authTtlSecs--;
        this.updateTtlDisplay();
      }
    }, 1000);
  }

  private updateTtlDisplay(): void {
    const mins = Math.floor(this.authTtlSecs / 60);
    const secs = this.authTtlSecs % 60;
    this.ttlText.textContent = `${String(mins).padStart(2, "0")}:${String(secs).padStart(2, "0")}`;
    this.btnLock.classList.toggle("ttl-warn", this.authenticated && this.authTtlSecs <= 60);
  }

  private setConnState(state: "online" | "connecting" | "offline" | "locked"): void {
    this.connDot.className = `dot dot-${state}`;
    if (state !== "online") {
      this.rttBadge.textContent = state === "locked" ? "locked" : "--ms";
    }
  }
}

new OtermApp().start();
