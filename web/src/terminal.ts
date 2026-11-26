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

export interface TerminalCallbacks {
  onInput: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
  onAck: (endSeq: bigint) => void;
  onOpenSearch: () => void;
  onScrollStateChange: (atBottom: boolean) => void;
  transformStickyModifiers: (data: string) => string;
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
      scrollback: 10000,
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
      const buf = this.term.buffer.active;
      const atBottom = buf.viewportY >= buf.baseY - 1;
      this.callbacks.onScrollStateChange(atBottom);
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

      return true;
    });
  }

  public async pasteFromClipboard(): Promise<void> {
    try {
      const text = await navigator.clipboard.readText();
      if (!text) return;
      // Wrap in bracketed paste sequences so shells/vim treat paste safely
      if (this.term.modes.bracketedPasteMode) {
        this.callbacks.onInput(`\x1b[200~${text}\x1b[201~`);
      } else {
        this.callbacks.onInput(text);
      }
    } catch {
      // Clipboard permission denied
    }
  }

  public writeChunk(data: Uint8Array, endSeq: bigint, fullReset = false): void {
    if (fullReset) {
      this.term.reset();
    }
    // SOTA flow-control: send ACK only after xterm.js finishes parsing & rendering chunk
    this.term.write(data, () => {
      this.callbacks.onAck(endSeq);
      const buf = this.term.buffer.active;
      this.callbacks.onScrollStateChange(buf.viewportY >= buf.baseY - 1);
    });
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
    return { cols, rows };
  }

  public setTheme(name: string): void {
    const theme = THEMES[name];
    if (!theme) return;
    this.term.options.theme = theme;
    localStorage.setItem("oterm_theme", name);
    document.documentElement.style.setProperty("--term-bg", theme.background || "#131620");
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
    this.callbacks.onScrollStateChange(true);
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
