import type { Terminal } from "@xterm/xterm";
import type { CompletionItem } from "./protocol";

export interface AutocompleteCallbacks {
  onRequest: (seq: number, line: string) => void;
  onAccept: (inputToSend: string) => void;
  onRecordCommand: (cmd: string) => void;
}

const DEBOUNCE_MS = 80;
const MAX_VISIBLE_ITEMS = 6;
const TYPING_WINDOW_MS = 2000;

interface CellMetrics {
  offsetX: number;
  offsetY: number;
  cellWidth: number;
  cellHeight: number;
  wrapWidth: number;
  wrapHeight: number;
  cols: number;
  rows: number;
}

const KIND_BADGE: Record<CompletionItem["kind"], { label: string; cls: string }> = {
  history: { label: "HIST", cls: "ac-k-hist" },
  dir: { label: "DIR", cls: "ac-k-dir" },
  file: { label: "FILE", cls: "ac-k-file" },
  git: { label: "GIT", cls: "ac-k-git" },
  cmd: { label: "CMD", cls: "ac-k-cmd" },
  flag: { label: "FLAG", cls: "ac-k-flag" },
};

export class AutocompleteController {
  private readonly term: Terminal;
  private readonly wrap: HTMLElement;
  private readonly ghostEl: HTMLElement;
  private readonly popupEl: HTMLElement;
  private readonly callbacks: AutocompleteCallbacks;

  private enabled = true;
  private shellSafe = false;
  private dismissed = false;
  private reqSeq = 0;
  private debounceTimer: number | null = null;
  private renderRaf: number | null = null;
  private items: CompletionItem[] = [];
  private selectedIdx = 0;
  private lastRequestedLine = "";
  private lastTypedAtMs = 0;
  private metrics: CellMetrics | null = null;

  constructor(
    term: Terminal,
    wrap: HTMLElement,
    ghostEl: HTMLElement,
    popupEl: HTMLElement,
    callbacks: AutocompleteCallbacks
  ) {
    this.term = term;
    this.wrap = wrap;
    this.ghostEl = ghostEl;
    this.popupEl = popupEl;
    this.callbacks = callbacks;

    const saved = localStorage.getItem("oterm_ac_enabled");
    this.enabled = saved === null ? true : saved === "1";
  }

  public isEnabled(): boolean {
    return this.enabled;
  }

  public setEnabled(next: boolean): void {
    this.enabled = next;
    localStorage.setItem("oterm_ac_enabled", next ? "1" : "0");
    if (!next) {
      this.clear();
    }
  }

  public toggle(): boolean {
    this.setEnabled(!this.enabled);
    return this.enabled;
  }

  public updateContext(shellSafe: boolean): void {
    this.shellSafe = shellSafe;
    if (!shellSafe) {
      this.clear();
    }
  }

  public invalidateMetrics(): void {
    this.metrics = null;
  }

  public onUserInput(): void {
    this.lastTypedAtMs = Date.now();
  }

  /** Called from xterm attachCustomKeyEventHandler on keydown. Returns false if consumed. */
  public handleKeyDown(ev: KeyboardEvent): boolean {
    if (!this.enabled) return true;
    // Never intercept IME composition keys (Chinese/CJK input)
    if (ev.isComposing || ev.keyCode === 229) return true;

    // Enter: record command for Frecency + CWD + Markov learning, then clear UI
    if (ev.key === "Enter" && !ev.ctrlKey && !ev.altKey && !ev.metaKey) {
      const cmd = this.extractCurrentCommand();
      if (cmd && cmd.trim().length >= 2) {
        this.callbacks.onRecordCommand(cmd.trim());
      }
      this.clear();
      return true;
    }

    // Ctrl+C / Ctrl+D / Ctrl+U / Ctrl+L clear suggestions immediately
    if (ev.ctrlKey && !ev.altKey && !ev.metaKey) {
      const k = ev.key.toLowerCase();
      if (k === "c" || k === "d" || k === "u" || k === "l") {
        this.clear();
        return true;
      }
      // Ctrl+N / Ctrl+P cycle suggestions when popup is open
      if (this.items.length > 1 && (k === "n" || k === "p")) {
        ev.preventDefault();
        if (k === "n") {
          this.selectedIdx = (this.selectedIdx + 1) % this.items.length;
        } else {
          this.selectedIdx = (this.selectedIdx - 1 + this.items.length) % this.items.length;
        }
        this.render();
        return false;
      }
    }

    // Alt+ArrowDown / Alt+ArrowUp cycle suggestions without hijacking shell history Up/Down
    if (ev.altKey && !ev.ctrlKey && !ev.metaKey && this.items.length > 1) {
      if (ev.key === "ArrowDown") {
        ev.preventDefault();
        this.selectedIdx = (this.selectedIdx + 1) % this.items.length;
        this.render();
        return false;
      }
      if (ev.key === "ArrowUp") {
        ev.preventDefault();
        this.selectedIdx = (this.selectedIdx - 1 + this.items.length) % this.items.length;
        this.render();
        return false;
      }
    }

    // Esc dismisses suggestions for the current line
    if (ev.key === "Escape" && this.hasSuggestions()) {
      ev.preventDefault();
      this.dismissed = true;
      this.hide();
      return false;
    }

    // Right Arrow at end of line accepts inline ghost text immediately
    if (
      ev.key === "ArrowRight" &&
      !ev.ctrlKey &&
      !ev.altKey &&
      !ev.metaKey &&
      !ev.shiftKey &&
      this.hasGhost()
    ) {
      const top = this.items[this.selectedIdx];
      if (top?.ghost_suffix) {
        ev.preventDefault();
        this.acceptItem(top);
        return false;
      }
    }

    // Alt+F accepts the next word of the ghost suffix
    if (ev.altKey && !ev.ctrlKey && !ev.metaKey && ev.key.toLowerCase() === "f" && this.hasGhost()) {
      const top = this.items[this.selectedIdx];
      if (top?.ghost_suffix) {
        const nextWord = extractNextWord(top.ghost_suffix);
        if (nextWord) {
          ev.preventDefault();
          this.callbacks.onAccept(nextWord);
          this.scheduleQuery();
          return false;
        }
      }
    }

    // Tab accepts the currently highlighted suggestion when popup is visible
    if (
      ev.key === "Tab" &&
      !ev.ctrlKey &&
      !ev.altKey &&
      !ev.metaKey &&
      !ev.shiftKey &&
      this.hasSuggestions()
    ) {
      const item = this.items[this.selectedIdx];
      if (item) {
        ev.preventDefault();
        this.acceptItem(item);
        return false;
      }
    }

    // Any normal typing key re-enables suggestions after Esc dismiss
    if (ev.key.length === 1 || ev.key === "Backspace") {
      this.dismissed = false;
      this.lastTypedAtMs = Date.now();
      // Hide stale ghost text immediately so it never overlaps newly typed chars during RTT
      if (!this.ghostEl.classList.contains("hidden")) {
        this.ghostEl.classList.add("hidden");
      }
    }

    return true;
  }

  /** Called after xterm finishes rendering an output chunk. */
  public onTerminalRendered(): void {
    if (!this.enabled || !this.shellSafe || this.dismissed) {
      this.hide();
      return;
    }
    // Only query autocomplete after recent user typing, never on background output streams
    if (Date.now() - this.lastTypedAtMs > TYPING_WINDOW_MS) {
      return;
    }
    this.scheduleQuery();
  }

  public onResponse(seq: number, items: CompletionItem[]): void {
    if (!this.enabled || this.dismissed || seq !== this.reqSeq) {
      return;
    }
    // Verify terminal didn't enter alt-screen or change line while in-flight
    const current = this.extractCurrentCommand();
    if (!current || current !== this.lastRequestedLine) {
      this.hide();
      return;
    }
    this.items = items.slice(0, MAX_VISIBLE_ITEMS);
    this.selectedIdx = 0;
    this.render();
  }

  public clear(): void {
    this.dismissed = false;
    this.lastRequestedLine = "";
    this.items = [];
    this.selectedIdx = 0;
    if (this.debounceTimer !== null) {
      window.clearTimeout(this.debounceTimer);
      this.debounceTimer = null;
    }
    if (this.renderRaf !== null) {
      window.cancelAnimationFrame(this.renderRaf);
      this.renderRaf = null;
    }
    this.hide();
  }

  private hasSuggestions(): boolean {
    return !this.dismissed && this.items.length > 0;
  }

  private hasGhost(): boolean {
    return this.hasSuggestions() && Boolean(this.items[this.selectedIdx]?.ghost_suffix);
  }

  private scheduleQuery(): void {
    if (this.debounceTimer !== null) {
      window.clearTimeout(this.debounceTimer);
    }
    this.debounceTimer = window.setTimeout(() => {
      this.debounceTimer = null;
      this.runQuery();
    }, DEBOUNCE_MS);
  }

  private runQuery(): void {
    if (!this.enabled || this.dismissed) {
      this.hide();
      return;
    }
    const cmd = this.extractCurrentCommand();
    if (!cmd || cmd.trim().length === 0) {
      this.lastRequestedLine = "";
      this.items = [];
      this.hide();
      return;
    }
    if (cmd === this.lastRequestedLine && this.items.length > 0) {
      this.render();
      return;
    }
    this.lastRequestedLine = cmd;
    this.reqSeq = (this.reqSeq + 1) & 0x7fffffff;
    this.callbacks.onRequest(this.reqSeq, cmd);
  }

  /**
   * Reads the logical shell input line from xterm's normal buffer up to the cursor.
   * Returns null when in alternate screen (vim/htop), scrolled up, cursor not at EOL,
   * or when no shell prompt is detected.
   */
  private extractCurrentCommand(): string | null {
    const buf = this.term.buffer.active;
    if (buf.type !== "normal") return null;
    // Don't pop suggestions when user is scrolled up reading history
    if (buf.viewportY < buf.baseY) return null;

    const absRow = buf.baseY + buf.cursorY;
    const curLine = buf.getLine(absRow);
    if (!curLine) return null;

    // Ensure cursor is at the end of visible text on the current line
    const fullCurrentText = curLine.translateToString(true);
    const beforeCursor = curLine.translateToString(false, 0, buf.cursorX);
    if (fullCurrentText.length > beforeCursor.trimEnd().length) {
      return null;
    }

    // Join wrapped lines if the command spans multiple rows
    let startRow = absRow;
    while (startRow > 0) {
      const l = buf.getLine(startRow);
      if (l && l.isWrapped) {
        startRow--;
      } else {
        break;
      }
    }

    let logical = "";
    for (let r = startRow; r < absRow; r++) {
      const l = buf.getLine(r);
      if (l) logical += l.translateToString(false);
    }
    logical += beforeCursor;

    // Match standard shell prompt delimiter ($ , # , % , ❯ , ➜ )
    const match = logical.match(/^[^#$%❯➜\n]*[#$%❯➜]\s(.*)$/);
    if (!match) return null;

    const input = match[1];
    if (!input || input.length > 200) return null;
    return input;
  }

  private acceptItem(item: CompletionItem): void {
    const backspaces = item.replace_len > 0 ? "\x7f".repeat(item.replace_len) : "";
    const payload = backspaces + item.insert_text;
    this.items = [];
    this.lastRequestedLine = "";
    this.hide();
    this.callbacks.onAccept(payload);
  }

  private render(): void {
    if (this.renderRaf !== null) {
      window.cancelAnimationFrame(this.renderRaf);
    }
    this.renderRaf = window.requestAnimationFrame(() => {
      this.renderRaf = null;
      this.renderNow();
    });
  }

  private renderNow(): void {
    if (!this.hasSuggestions()) {
      this.hide();
      return;
    }

    const pos = this.getCursorPixelPosition();
    if (!pos) {
      this.hide();
      return;
    }

    const selected = this.items[this.selectedIdx];

    // 1. Inline Ghost Text at exact cursor cell
    if (selected?.ghost_suffix) {
      const availWidth = Math.max(0, pos.wrapWidth - pos.x - 16);
      this.ghostEl.textContent = selected.ghost_suffix;
      this.ghostEl.style.left = `${pos.x}px`;
      this.ghostEl.style.top = `${pos.y}px`;
      this.ghostEl.style.height = `${pos.cellHeight}px`;
      this.ghostEl.style.lineHeight = `${pos.cellHeight}px`;
      this.ghostEl.style.fontFamily = this.term.options.fontFamily || "monospace";
      this.ghostEl.style.fontSize = `${this.term.options.fontSize || 14}px`;
      this.ghostEl.style.maxWidth = `${availWidth}px`;
      this.ghostEl.classList.remove("hidden");
    } else {
      this.ghostEl.classList.add("hidden");
    }

    // 2. Compact Floating Suggestion Menu
    const listHtml = this.items
      .map((item, idx) => {
        const badge = KIND_BADGE[item.kind] ?? KIND_BADGE.cmd;
        const activeCls = idx === this.selectedIdx ? " active" : "";
        const detailHtml = item.detail
          ? `<span class="ac-detail">${escapeHtml(item.detail)}</span>`
          : "";
        return `<div class="ac-item${activeCls}" data-idx="${idx}">
          <span class="ac-kind ${badge.cls}">${badge.label}</span>
          <span class="ac-label">${escapeHtml(item.label)}</span>
          ${detailHtml}
        </div>`;
      })
      .join("");

    this.popupEl.innerHTML = `<div class="ac-list">${listHtml}</div>
      <div class="ac-footer">
        <span><kbd>→</kbd> ghost</span>
        <span><kbd>Tab</kbd> accept</span>
        <span><kbd>Ctrl+N/P</kbd> cycle</span>
        <span><kbd>Esc</kbd> hide</span>
      </div>`;

    this.popupEl.querySelectorAll<HTMLElement>(".ac-item").forEach((el) => {
      el.addEventListener("mousedown", (e) => {
        e.preventDefault();
        const idx = Number(el.dataset.idx);
        const item = this.items[idx];
        if (item) {
          this.acceptItem(item);
          this.term.focus();
        }
      });
    });

    this.popupEl.classList.remove("hidden");

    // Position popup smartly below cursor, or flip above if near bottom
    // Estimate popup height from item count to avoid forced synchronous layout reflow
    const popupWidth = Math.min(460, pos.wrapWidth - 20);
    const estPopupHeight = this.items.length * 30 + 36;
    const left = Math.max(8, Math.min(pos.x, pos.wrapWidth - popupWidth - 10));
    const spaceBelow = pos.wrapHeight - (pos.y + pos.cellHeight);
    const top =
      spaceBelow >= estPopupHeight + 10
        ? pos.y + pos.cellHeight + 4
        : Math.max(8, pos.y - estPopupHeight - 4);

    this.popupEl.style.left = `${left}px`;
    this.popupEl.style.top = `${top}px`;
    this.popupEl.style.width = `${popupWidth}px`;
  }

  private getCursorPixelPosition(): {
    x: number;
    y: number;
    cellWidth: number;
    cellHeight: number;
    wrapWidth: number;
    wrapHeight: number;
  } | null {
    let m = this.metrics;
    if (!m || m.cols !== this.term.cols || m.rows !== this.term.rows) {
      const screen = this.wrap.querySelector<HTMLElement>(".xterm-screen");
      if (!screen || this.term.cols <= 0 || this.term.rows <= 0) return null;

      const wrapRect = this.wrap.getBoundingClientRect();
      const screenRect = screen.getBoundingClientRect();
      if (screenRect.width <= 0 || screenRect.height <= 0) return null;

      m = {
        offsetX: screenRect.left - wrapRect.left,
        offsetY: screenRect.top - wrapRect.top,
        cellWidth: screenRect.width / this.term.cols,
        cellHeight: screenRect.height / this.term.rows,
        wrapWidth: wrapRect.width,
        wrapHeight: wrapRect.height,
        cols: this.term.cols,
        rows: this.term.rows,
      };
      this.metrics = m;
    }

    const buf = this.term.buffer.active;
    const x = m.offsetX + buf.cursorX * m.cellWidth;
    const y = m.offsetY + buf.cursorY * m.cellHeight;
    return {
      x,
      y,
      cellWidth: m.cellWidth,
      cellHeight: m.cellHeight,
      wrapWidth: m.wrapWidth,
      wrapHeight: m.wrapHeight,
    };
  }

  private hide(): void {
    this.ghostEl.classList.add("hidden");
    this.popupEl.classList.add("hidden");
  }
}

function extractNextWord(suffix: string): string {
  const m = suffix.match(/^(\s*\S+)/);
  return m ? m[1] : suffix;
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
