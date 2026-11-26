// Mobile VirtualKeyboard API + visualViewport + sticky Ctrl/Alt accessory bar

export interface MobileController {
  transformInput: (data: string) => string;
  toggleBar: () => void;
}

export function initMobileSupport(
  onSendInput: (data: string) => void,
  onPaste: () => void,
  onViewportResize: () => void
): MobileController {
  const bar = document.getElementById("mobile-bar")!;
  const ctrlBtn = document.getElementById("mod-ctrl")!;
  const altBtn = document.getElementById("mod-alt")!;
  const pasteBtn = document.getElementById("btn-mobile-paste")!;

  let stickyCtrl = false;
  let stickyAlt = false;

  const isTouch = "ontouchstart" in window || navigator.maxTouchPoints > 0;
  if (isTouch) {
    bar.classList.remove("hidden");
  }

  // VirtualKeyboard API (Chromium Android)
  const navAny = navigator as Navigator & {
    virtualKeyboard?: { overlaysContent: boolean };
  };
  if (navAny.virtualKeyboard) {
    try {
      navAny.virtualKeyboard.overlaysContent = true;
    } catch {
      // Ignore if unsupported
    }
  }

  // visualViewport resize sync (iOS Safari + Android Chrome)
  const syncViewport = () => {
    const vv = window.visualViewport;
    if (vv) {
      document.documentElement.style.setProperty("--app-height", `${vv.height}px`);
    }
    onViewportResize();
  };
  if (window.visualViewport) {
    window.visualViewport.addEventListener("resize", syncViewport);
    window.visualViewport.addEventListener("scroll", syncViewport);
  }
  window.addEventListener("resize", syncViewport);

  const updateModClasses = () => {
    ctrlBtn.classList.toggle("active", stickyCtrl);
    altBtn.classList.toggle("active", stickyAlt);
  };

  const applyModifiers = (data: string): string => {
    if (!stickyCtrl && !stickyAlt) return data;
    if (data.length === 1) {
      let out = data;
      if (stickyCtrl) {
        const code = data.toUpperCase().charCodeAt(0);
        if (code >= 64 && code <= 95) {
          out = String.fromCharCode(code - 64);
        } else if (data === " ") {
          out = "\x00";
        }
      }
      if (stickyAlt) {
        out = `\x1b${out}`;
      }
      stickyCtrl = false;
      stickyAlt = false;
      updateModClasses();
      return out;
    }
    stickyCtrl = false;
    stickyAlt = false;
    updateModClasses();
    return data;
  };

  bar.addEventListener("pointerdown", (e) => {
    // Prevent losing focus from terminal textarea on tap
    e.preventDefault();
  });

  bar.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest("button");
    if (!btn) return;

    if (btn === ctrlBtn) {
      stickyCtrl = !stickyCtrl;
      updateModClasses();
      return;
    }
    if (btn === altBtn) {
      stickyAlt = !stickyAlt;
      updateModClasses();
      return;
    }
    if (btn === pasteBtn) {
      onPaste();
      return;
    }

    const key = btn.dataset.key;
    if (key === "Escape") {
      onSendInput("\x1b");
      return;
    }
    if (key === "Tab") {
      onSendInput("\t");
      return;
    }

    const rawSeq = btn.dataset.seq;
    if (rawSeq) {
      const unescaped = rawSeq.replace(/\\x1b/g, "\x1b");
      onSendInput(applyModifiers(unescaped));
    }
  });

  return {
    transformInput: applyModifiers,
    toggleBar: () => {
      bar.classList.toggle("hidden");
      onViewportResize();
    },
  };
}
