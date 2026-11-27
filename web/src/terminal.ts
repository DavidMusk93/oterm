import { Terminal, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import { ClipboardAddon } from "@xterm/addon-clipboard";

export const THEMES: Record<string, ITheme> = {
  tokyo: {
    background: "#131620",
    foreground: "#c0caf5",
    cursor: "#7aa2f7",
    cursorAccent: "#131620",
    selectionBackground: "rgba(122, 162, 247, 0.30)",
    black: "#15161e",
    red: "#f7768e",
    green: "#9ece6a",
    yellow: "#e0af68",
    blue: "#7aa2f7",
    magenta: "#bb9af7",
    cyan: "#7dcfff",
    white: "#a9b1d6",
    brightBlack: "#414868",
    brightRed: "#f7768e",
    brightGreen: "#9ece6a",
    brightYellow: "#e0af68",
    brightBlue: "#7aa2f7",
    brightMagenta: "#bb9af7",
    brightCyan: "#7dcfff",
    brightWhite: "#c0caf5",
  },
  catppuccin: {
    background: "#1e1e2e",
    foreground: "#cdd6f4",
    cursor: "#f5e0dc",
    cursorAccent: "#1e1e2e",
    selectionBackground: "rgba(137, 180, 250, 0.28)",
    black: "#45475a",
    red: "#f38ba8",
    green: "#a6e3a1",
    yellow: "#f9e2af",
    blue: "#89b4fa",
    magenta: "#f5c2e7",
    cyan: "#94e2d5",
    white: "#bac2de",
    brightBlack: "#585b70",
    brightRed: "#f38ba8",
    brightGreen: "#a6e3a1",
    brightYellow: "#f9e2af",
    brightBlue: "#89b4fa",
    brightMagenta: "#f5c2e7",
    brightCyan: "#94e2d5",
    brightWhite: "#a6adc8",
  },
  dracula: {
    background: "#1e1f29",
    foreground: "#f8f8f2",
    cursor: "#ff79c6",
    cursorAccent: "#1e1f29",
    selectionBackground: "rgba(189, 147, 249, 0.32)",
    black: "#21222c",
    red: "#ff5555",
    green: "#50fa7b",
    yellow: "#f1fa8c",
    blue: "#bd93f9",
    magenta: "#ff79c6",
    cyan: "#8be9fd",
    white: "#f8f8f2",
    brightBlack: "#6272a4",
    brightRed: "#ff6e6e",
    brightGreen: "#69ff94",
    brightYellow: "#ffffa5",
    brightBlue: "#d6acff",
    brightMagenta: "#ff92df",
    brightCyan: "#a4ffff",
    brightWhite: "#ffffff",
  },
  gruvbox: {
    background: "#1d2021",
    foreground: "#ebdbb2",
    cursor: "#fe8019",
    cursorAccent: "#1d2021",
    selectionBackground: "rgba(254, 128, 25, 0.28)",
    black: "#282828",
    red: "#cc241d",
    green: "#98971a",
    yellow: "#d79921",
    blue: "#458588",
    magenta: "#b16286",
    cyan: "#689d6a",
    white: "#a89984",
    brightBlack: "#928374",
    brightRed: "#fb4934",
    brightGreen: "#b8bb26",
    brightYellow: "#fabd2f",
    brightBlue: "#83a598",
    brightMagenta: "#d3869b",
    brightCyan: "#8ec07c",
    brightWhite: "#ebdbb2",
  },
  light: {
    background: "#fdf6e3",
    foreground: "#586e75",
    cursor: "#268bd2",
    cursorAccent: "#fdf6e3",
    selectionBackground: "rgba(38, 139, 210, 0.22)",
    black: "#073642",
    red: "#dc322f",
    green: "#859900",
    yellow: "#b58900",
    blue: "#268bd2",
    magenta: "#d33682",
    cyan: "#2aa198",
    white: "#eee8d5",
    brightBlack: "#002b36",
    brightRed: "#cb4b16",
    brightGreen: "#586e75",
    brightYellow: "#657b83",
    brightBlue: "#839496",
    brightMagenta: "#6c71c4",
    brightCyan: "#93a1a1",
    brightWhite: "#fdf6e3",
  },
};

export interface ScrollState {
  atBottom: boolean;
  linesAbove: number;
}

export interface TerminalCallbacks {
  onInput: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
  onAck: (endSeq: bigint, byteCount: number) => void;
  onOpenSearch: () => void;
  onScrollStateChange: (state: ScrollState) => void;
  transformStickyModifiers: (data: string) => string;
  onKeyDown?: (ev: KeyboardEvent) => boolean;
  onRendered?: () => void;
  onLayoutChanged?: () => void;
}

export class OtermView {
  public readonly term: Terminal;
  private readonly fitAddon = new FitAddon();
  private readonly searchAddon = new SearchAddon();
  private webglAddon: WebglAddon | null = null;
  private readonly container: HTMLElement;
  private readonly callbacks: TerminalCallbacks;
  private resizeTimer: number | null = null;
  private resizeObserver: ResizeObserver;
  private lastAtBottom: boolean | null = null;
  private lastLinesAbove = -1;
  private writeQueue: Array<{ data: Uint8Array; endSeq: bigint; totalBytes: number; isLast: boolean }> = [];
  private pumpScheduled = false;

  constructor(container: HTMLElement, callbacks: TerminalCallbacks) {
    this.container = container;
    this.callbacks = callbacks;

    const savedFont = Number(localStorage.getItem("oterm_font_size") || "14");
    const savedTheme = localStorage.getItem("oterm_theme") || "tokyo";

    this.term = new Terminal({
      allowProposedApi: true,
      cursorBlink: true,
      cursorStyle: "bar",
      fontFamily:
        '"JetBrains Mono", "Fira Code", "SF Mono", Menlo, Monaco, Consolas, "Liberation Mono", "Noto Sans Mono CJK SC", monospace',
      fontSize: Math.min(24, Math.max(10, savedFont)),
      lineHeight: 1.22,
      scrollback: 50000,
      smoothScrollDuration: 90,
      macOptionIsMeta: true,
      rightClickSelectsWord: true,
      theme: THEMES[savedTheme] ?? THEMES.tokyo,
    });

    this.term.loadAddon(this.fitAddon);
    this.term.loadAddon(new Unicode11Addon());
    this.term.unicode.activeVersion = "11";
    this.term.loadAddon(new WebLinksAddon());
    this.term.loadAddon(this.searchAddon);
    this.term.loadAddon(new ClipboardAddon());

    this.term.open(container);
    this.enableWebGL();
    this.hardenHelperTextarea();
    this.installKeyHandlers();

    this.term.onData((data) => {
      const transformed = this.callbacks.transformStickyModifiers(data);
      this.callbacks.onInput(transformed);
    });

    this.term.onBinary((data) => {
      this.callbacks.onInput(data);
    });

    this.term.onScroll(() => {
      this.emitScrollState();
    });

    this.resizeObserver = new ResizeObserver(() => this.scheduleFit());
    this.resizeObserver.observe(container);
    this.fitNow();
  }

  private enableWebGL(): void {
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => {
        webgl.dispose();
        this.webglAddon = null;
      });
      this.term.loadAddon(webgl);
      this.webglAddon = webgl;
    } catch {
      // Automatic fallback to standard DOM/Canvas renderer
      this.webglAddon = null;
    }
  }

  private hardenHelperTextarea(): void {
    const ta = this.container.querySelector("textarea.xterm-helper-textarea");
    if (ta instanceof HTMLTextAreaElement) {
      ta.setAttribute("autocorrect", "off");
      ta.setAttribute("autocapitalize", "off");
      ta.setAttribute("autocomplete", "off");
      ta.setAttribute("spellcheck", "false");
    }
  }

  private installKeyHandlers(): void {
    this.term.attachCustomKeyEventHandler((ev: KeyboardEvent) => {
      if (ev.type !== "keydown") return true;
      // Never intercept IME composition keys (Chinese/Japanese/Korean input methods)
      if (ev.isComposing || ev.keyCode === 229) return true;

      // Ctrl+Shift+F -> Open scrollback search
      if ((ev.ctrlKey || ev.metaKey) && ev.shiftKey && ev.key.toLowerCase() === "f") {
        ev.preventDefault();
        this.callbacks.onOpenSearch();
        return false;
      }

      // Ctrl+Shift+C -> Copy selection
      if ((ev.ctrlKey || ev.metaKey) && ev.shiftKey && ev.key.toLowerCase() === "c") {
        const sel = this.term.getSelection();
        if (sel) {
          navigator.clipboard?.writeText(sel).catch(() => {});
        }
        ev.preventDefault();
        return false;
      }

      // Smart Ctrl+C: copy if selection exists, else send SIGINT
      if (ev.ctrlKey && !ev.shiftKey && !ev.altKey && ev.key.toLowerCase() === "c") {
        if (this.term.hasSelection()) {
          const sel = this.term.getSelection();
          if (sel) {
            navigator.clipboard?.writeText(sel).catch(() => {});
            this.term.clearSelection();
          }
          ev.preventDefault();
          return false;
        }
      }

      // Ctrl+Shift+V -> Bracketed paste
      if ((ev.ctrlKey || ev.metaKey) && ev.shiftKey && ev.key.toLowerCase() === "v") {
        ev.preventDefault();
        this.pasteFromClipboard();
        return false;
      }

      // Ctrl+= / Ctrl+- / Ctrl+0 -> Font zoom
      if ((ev.ctrlKey || ev.metaKey) && !ev.altKey) {
        if (ev.key === "=" || ev.key === "+") {
          ev.preventDefault();
          this.adjustFontSize(1);
          return false;
        }
        if (ev.key === "-") {
          ev.preventDefault();
          this.adjustFontSize(-1);
          return false;
        }
        if (ev.key === "0") {
          ev.preventDefault();
          this.setFontSize(14);
          return false;
        }
      }

      if (this.callbacks.onKeyDown) {
        return this.callbacks.onKeyDown(ev);
      }

      return true;
    });
  }

  public async pasteFromClipboard(): Promise<void> {
    try {
      const text = await navigator.clipboard.readText();
      if (!text) return;
      // Use xterm's native paste() so bracketed paste (\x1b[200~...\x1b[201~),
      // CRLF normalization, and escape sanitization are handled identically to Ctrl+V/Cmd+V.
      this.term.paste(text);
    } catch {
      // Clipboard permission denied
    }
  }

  public writeChunk(data: Uint8Array, endSeq: bigint, fullReset = false): void {
    const SLICE_SIZE = 16 * 1024;
    if (fullReset) {
      // Preserve bracketed paste mode across fullReset so multi-line paste never breaks
      const hadBracketedPaste = this.term.modes.bracketedPasteMode;
      // Drop any stale queued slices when converging to a fresh collapsed snapshot
      this.writeQueue.length = 0;
      this.term.reset();
      if (hadBracketedPaste) {
        this.term.write("\x1b[?2004h");
      }
    }

    // Fast path for interactive typing / small chunks: zero-delay direct write
    if (this.writeQueue.length === 0 && data.length <= SLICE_SIZE) {
      const len = data.length;
      this.term.write(data, () => {
        this.callbacks.onAck(endSeq, len);
        this.emitScrollState();
        this.callbacks.onRendered?.();
      });
      return;
    }

    // Adaptive frame-budgeted chunked rendering for large bursts/snapshots:
    // prevents main-thread jank and keeps input responsive at 60fps.
    const totalBytes = data.length;
    for (let offset = 0; offset < totalBytes; offset += SLICE_SIZE) {
      const end = Math.min(offset + SLICE_SIZE, totalBytes);
      const isLast = end >= totalBytes;
      this.writeQueue.push({
        data: data.subarray(offset, end),
        endSeq,
        totalBytes: isLast ? totalBytes : 0,
        isLast,
      });
    }
    this.schedulePump();
  }

  private schedulePump(): void {
    if (this.pumpScheduled) return;
    this.pumpScheduled = true;
    window.requestAnimationFrame(() => {
      this.pumpScheduled = false;
      this.pumpWriteQueue();
    });
  }

  private pumpWriteQueue(): void {
    const FRAME_BUDGET_MS = 8;
    const start = performance.now();
    while (this.writeQueue.length > 0) {
      const item = this.writeQueue.shift()!;
      if (item.isLast) {
        const { endSeq, totalBytes } = item;
        this.term.write(item.data, () => {
          this.callbacks.onAck(endSeq, totalBytes);
          this.emitScrollState();
          this.callbacks.onRendered?.();
        });
      } else {
        this.term.write(item.data);
      }
      if (performance.now() - start >= FRAME_BUDGET_MS) {
        break;
      }
    }
    if (this.writeQueue.length > 0) {
      this.schedulePump();
    }
  }

  public scheduleFit(): void {
    if (this.resizeTimer !== null) {
      window.clearTimeout(this.resizeTimer);
    }
    this.resizeTimer = window.setTimeout(() => {
      this.resizeTimer = null;
      this.fitNow();
    }, 60);
  }

  public fitNow(): { cols: number; rows: number } {
    try {
      this.fitAddon.fit();
    } catch {
      // Container might be hidden during layout transition
    }
    const cols = this.term.cols;
    const rows = this.term.rows;
    this.callbacks.onResize(cols, rows);
    this.callbacks.onLayoutChanged?.();
    return { cols, rows };
  }

  public setTheme(name: string): void {
    const theme = THEMES[name];
    if (!theme) return;
    this.term.options.theme = theme;
    localStorage.setItem("oterm_theme", name);
    document.documentElement.style.setProperty("--term-bg", theme.background || "#131620");
    this.callbacks.onLayoutChanged?.();
  }

  public adjustFontSize(delta: number): void {
    const current = this.term.options.fontSize ?? 14;
    this.setFontSize(current + delta);
  }

  public setFontSize(size: number): void {
    const clamped = Math.min(24, Math.max(10, size));
    this.term.options.fontSize = clamped;
    localStorage.setItem("oterm_font_size", String(clamped));
    this.fitNow();
  }

  public searchNext(query: string, opts: { regex: boolean; caseSensitive: boolean }): void {
    if (!query) {
      this.searchAddon.clearDecorations();
      return;
    }
    this.searchAddon.findNext(query, {
      regex: opts.regex,
      caseSensitive: opts.caseSensitive,
      incremental: false,
    });
  }

  public searchPrev(query: string, opts: { regex: boolean; caseSensitive: boolean }): void {
    if (!query) return;
    this.searchAddon.findPrevious(query, {
      regex: opts.regex,
      caseSensitive: opts.caseSensitive,
    });
  }

  public clearSearch(): void {
    this.searchAddon.clearDecorations();
  }

  public scrollToBottom(): void {
    this.term.scrollToBottom();
    this.emitScrollState();
  }

  private emitScrollState(): void {
    const buf = this.term.buffer.active;
    const linesAbove = Math.max(0, buf.baseY - buf.viewportY);
    const atBottom = linesAbove <= 1;
    if (atBottom === this.lastAtBottom && linesAbove === this.lastLinesAbove) {
      return;
    }
    this.lastAtBottom = atBottom;
    this.lastLinesAbove = linesAbove;
    this.callbacks.onScrollStateChange({
      atBottom,
      linesAbove,
    });
  }

  private findPromptRows(): number[] {
    const buf = this.term.buffer.active;
    const rows: number[] = [];
    const promptRe = /^[^#$%❯➜\n]{1,80}[#$%❯➜]\s/;
    const total = buf.length;
    for (let i = 0; i < total; i++) {
      const line = buf.getLine(i);
      if (!line || line.isWrapped) continue;
      const text = line.translateToString(true);
      if (promptRe.test(text)) {
        rows.push(i);
      }
    }
    return rows;
  }

  public jumpPrevCommand(): void {
    const buf = this.term.buffer.active;
    const prompts = this.findPromptRows();
    const curTop = buf.viewportY;
    for (let i = prompts.length - 1; i >= 0; i--) {
      if (prompts[i] < curTop - 1) {
        this.term.scrollToLine(Math.max(0, prompts[i] - 1));
        this.emitScrollState();
        return;
      }
    }
    if (prompts.length > 0) {
      this.term.scrollToLine(Math.max(0, prompts[0] - 1));
      this.emitScrollState();
    }
  }

  public jumpNextCommand(): void {
    const buf = this.term.buffer.active;
    const prompts = this.findPromptRows();
    const curTop = buf.viewportY;
    for (let i = 0; i < prompts.length; i++) {
      if (prompts[i] > curTop + 2) {
        this.term.scrollToLine(Math.max(0, prompts[i] - 1));
        this.emitScrollState();
        return;
      }
    }
    this.scrollToBottom();
  }

  public async copyLastBlock(): Promise<boolean> {
    const buf = this.term.buffer.active;
    const prompts = this.findPromptRows();
    const endRow = buf.baseY + buf.cursorY;
    let startRow = Math.max(0, endRow - 200);
    let stopRow = endRow;

    if (prompts.length >= 2) {
      startRow = prompts[prompts.length - 2];
      stopRow = prompts[prompts.length - 1] - 1;
    } else if (prompts.length === 1) {
      startRow = prompts[0];
    }

    const lines: string[] = [];
    for (let r = startRow; r <= stopRow; r++) {
      const line = buf.getLine(r);
      if (!line) continue;
      const text = line.translateToString(true);
      if (line.isWrapped && lines.length > 0) {
        lines[lines.length - 1] += text;
      } else {
        lines.push(text);
      }
    }

    const block = lines.join("\n").trim();
    if (!block) return false;
    try {
      await navigator.clipboard.writeText(block);
      return true;
    } catch {
      return false;
    }
  }

  public focus(): void {
    this.term.focus();
  }

  public dispose(): void {
    this.resizeObserver.disconnect();
    this.webglAddon?.dispose();
    this.term.dispose();
  }
}
