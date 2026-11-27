import type { Terminal } from "@xterm/xterm";

const EXPIRE_MS = 1500;
const SHELL_NAMES = new Set(["bash", "zsh", "sh", "dash", "fish", "ash", "ksh"]);

interface CellMetrics {
  offsetX: number;
  offsetY: number;
  cellWidth: number;
  cellHeight: number;
  cols: number;
  rows: number;
}

export function isInteractiveShell(fgCommand: string): boolean {
  const base = fgCommand.split("/").pop()?.replace(/^-/, "") ?? "";
  return SHELL_NAMES.has(base);
}

/**
 * Mosh-style conservative predictive local echo overlay.
 *
 * Renders printable ASCII keystrokes immediately at the exact cursor cell
 * in a non-invasive DOM overlay (never mutating xterm.js buffer state),
 * and reconciles against actual PTY output as each frame renders.
 *
 * Automatically disabled in alt-screen (vim/htop/TUI), password prompts
 * (echo_enabled === false), non-shell foreground commands, scrolled-up views,
 * or mid-line edits.
 */
export class LocalEchoOverlay {
  private readonly term: Terminal;
  private readonly wrap: HTMLElement;
  private readonly el: HTMLElement;

  private shellSafe = false;
  private rttMs = 0;
  private predicted = "";
  private baseLineText = "";
  private baseRow = -1;
  private expireTimer: number | null = null;
  private metrics: CellMetrics | null = null;

  constructor(term: Terminal, wrap: HTMLElement, el: HTMLElement) {
    this.term = term;
    this.wrap = wrap;
    this.el = el;
  }

  public updateRtt(rttMs: number): void {
    this.rttMs = rttMs;
  }

  public updateContext(altScreen: boolean, echoEnabled: boolean, fgCommand: string): void {
    const safe = !altScreen && echoEnabled && isInteractiveShell(fgCommand);
    this.shellSafe = safe;
    if (!safe) {
      this.clear();
    }
  }

  public invalidateMetrics(): void {
    this.metrics = null;
    if (this.predicted.length > 0) {
      this.paint();
    }
  }

  public getPredictedLength(): number {
    return this.predicted.length;
  }

  /** Called synchronously right as user input is sent over WebSocket. */
  public onUserInput(data: string): void {
    // Only engage speculative overlay on noticeably high-RTT links (>= 60ms);
    // on fast connections, native WebGL echo is already instant with zero subpixel jitter.
    if (!this.shellSafe || this.rttMs < 60) {
      this.clear();
      return;
    }

    // Handle Backspace smoothly while predicted characters are in flight
    if (data === "\x7f" || data === "\b") {
      if (this.predicted.length > 0) {
        this.predicted = this.predicted.slice(0, -1);
        if (this.predicted.length === 0) {
          this.clear();
        } else {
          this.armExpireTimer();
          this.paint();
        }
      }
      return;
    }

    if (data.length !== 1) {
      this.clear();
      return;
    }

    const code = data.charCodeAt(0);
    // Only predict printable ASCII (0x20..=0x7e)
    if (code < 0x20 || code > 0x7e) {
      this.clear();
      return;
    }

    const buf = this.term.buffer.active;
    if (buf.type !== "normal" || buf.viewportY < buf.baseY) {
      this.clear();
      return;
    }

    const absRow = buf.baseY + buf.cursorY;
    const curLine = buf.getLine(absRow);
    if (!curLine) {
      this.clear();
      return;
    }

    // Avoid line-wrap edge cases near right margin
    if (buf.cursorX + this.predicted.length >= this.term.cols - 2) {
      this.clear();
      return;
    }

    if (this.predicted.length === 0) {
      // Only start predicting when cursor is at EOL
      const fullText = curLine.translateToString(true);
      const beforeCursor = curLine.translateToString(false, 0, buf.cursorX);
      if (fullText.length > beforeCursor.trimEnd().length) {
        return;
      }
      this.baseLineText = beforeCursor;
      this.baseRow = absRow;
    } else if (absRow !== this.baseRow) {
      this.clear();
      return;
    }

    this.predicted += data;
    this.armExpireTimer();
    this.paint();
  }

  /** Called after xterm.js finishes rendering a server output chunk. */
  public onTerminalRendered(): void {
    if (this.predicted.length === 0) return;

    const buf = this.term.buffer.active;
    if (!this.shellSafe || buf.type !== "normal" || buf.viewportY < buf.baseY) {
      this.clear();
      return;
    }

    const absRow = buf.baseY + buf.cursorY;
    if (absRow !== this.baseRow) {
      this.clear();
      return;
    }

    const curLine = buf.getLine(absRow);
    if (!curLine) {
      this.clear();
      return;
    }

    const curText = curLine.translateToString(false, 0, buf.cursorX);
    if (!curText.startsWith(this.baseLineText)) {
      this.clear();
      return;
    }

    const echoed = curText.slice(this.baseLineText.length);
    if (echoed.length === 0) {
      // Output chunk did not yet echo our characters (e.g., unrelated control seq)
      return;
    }

    if (this.predicted.startsWith(echoed)) {
      this.baseLineText = curText;
      this.predicted = this.predicted.slice(echoed.length);
      if (this.predicted.length === 0) {
        this.clear();
      } else {
        this.armExpireTimer();
        this.paint();
      }
    } else {
      // Server echoed something different (e.g. shell completion/rewrite) -> defer to server
      this.clear();
    }
  }

  public clear(): void {
    this.predicted = "";
    this.baseLineText = "";
    this.baseRow = -1;
    if (this.expireTimer !== null) {
      window.clearTimeout(this.expireTimer);
      this.expireTimer = null;
    }
    if (!this.el.classList.contains("hidden")) {
      this.el.classList.add("hidden");
      this.el.textContent = "";
    }
  }

  private armExpireTimer(): void {
    if (this.expireTimer !== null) {
      window.clearTimeout(this.expireTimer);
    }
    this.expireTimer = window.setTimeout(() => {
      this.expireTimer = null;
      this.clear();
    }, EXPIRE_MS);
  }

  private getMetrics(): CellMetrics | null {
    if (
      this.metrics &&
      this.metrics.cols === this.term.cols &&
      this.metrics.rows === this.term.rows
    ) {
      return this.metrics;
    }
    const screen = this.wrap.querySelector<HTMLElement>(".xterm-screen");
    if (!screen || this.term.cols <= 0 || this.term.rows <= 0) return null;

    const wrapRect = this.wrap.getBoundingClientRect();
    const screenRect = screen.getBoundingClientRect();
    if (screenRect.width <= 0 || screenRect.height <= 0) return null;

    this.metrics = {
      offsetX: screenRect.left - wrapRect.left,
      offsetY: screenRect.top - wrapRect.top,
      cellWidth: screenRect.width / this.term.cols,
      cellHeight: screenRect.height / this.term.rows,
      cols: this.term.cols,
      rows: this.term.rows,
    };
    return this.metrics;
  }

  private paint(): void {
    if (this.predicted.length === 0) {
      this.el.classList.add("hidden");
      return;
    }
    const m = this.getMetrics();
    if (!m) {
      this.el.classList.add("hidden");
      return;
    }
    const buf = this.term.buffer.active;
    const x = m.offsetX + buf.cursorX * m.cellWidth;
    const y = m.offsetY + buf.cursorY * m.cellHeight;
    const theme = this.term.options.theme;

    this.el.textContent = this.predicted;
    this.el.style.left = `${x}px`;
    this.el.style.top = `${y}px`;
    this.el.style.height = `${m.cellHeight}px`;
    this.el.style.lineHeight = `${m.cellHeight}px`;
    this.el.style.fontFamily = this.term.options.fontFamily || "monospace";
    this.el.style.fontSize = `${this.term.options.fontSize || 14}px`;
    this.el.style.color = theme?.foreground || "#c0caf5";
    this.el.style.backgroundColor = theme?.background || "#131620";
    this.el.classList.remove("hidden");
  }
}
