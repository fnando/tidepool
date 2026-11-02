// TOML colors. In the config editor the textarea stays on top (typing,
// selection, undo, the form all work as usual) with transparent text, over
// a <pre> showing the same text highlighted; `pre.toml` blocks are just
// highlighted.
(function () {
  const PATTERNS = [
    ["comment", /#[^\n]*/y],
    ["string", /"""[\s\S]*?(?:"""|$)/y],
    ["string", /'''[\s\S]*?(?:'''|$)/y],
    ["string", /"(?:[^"\\\n]|\\.)*"?/y],
    ["string", /'[^'\n]*'?/y],
    ["number", /\d{4}-\d{2}-\d{2}(?:[Tt ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:[Zz]|[+-]\d{2}:\d{2})?)?(?![\w-])/y],
    ["number", /\d{2}:\d{2}:\d{2}(?:\.\d+)?(?![\w-])/y],
    ["number", /[+-]?(?:0x[\da-fA-F_]+|0o[0-7_]+|0b[01_]+|inf|nan|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?)(?![\w-])/y],
    ["bool", /(?:true|false)(?![\w-])/y],
  ];
  const BARE = /[A-Za-z0-9_-]+/y;
  const QUOTED = /"(?:[^"\\\n]|\\.)*"?|'[^'\n]*'?/y;
  const HEADER = /\[\[?[^\]\n]*\]?\]?/y;

  function escape(text) {
    return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  function match(re, text, at) {
    re.lastIndex = at;
    const m = re.exec(text);
    return m && m[0].length ? m[0] : null;
  }

  function highlight(text) {
    let out = "";
    let at = 0;
    let lineStart = true; // only blanks since the last newline
    let keyPos = true; // a key may come next
    const open = []; // "[" arrays and "{" inline tables
    const emit = (kind, part) => {
      out += kind ? '<span class="t-' + kind + '">' + escape(part) + "</span>" : escape(part);
      at += part.length;
    };
    while (at < text.length) {
      const c = text[at];
      if (c === "\n") {
        emit(null, c);
        lineStart = true;
        if (!open.length) keyPos = true;
        continue;
      }
      if (c === " " || c === "\t" || c === "\r") { emit(null, c); continue; }
      const wasLineStart = lineStart;
      lineStart = false;
      let part;
      if (c === "#") { emit("comment", match(PATTERNS[0][1], text, at)); continue; }
      if (wasLineStart && !open.length && c === "[" && (part = match(HEADER, text, at))) {
        emit("table", part);
        keyPos = false;
        continue;
      }
      if (keyPos && (part = match(BARE, text, at) || match(QUOTED, text, at))) {
        emit("key", part);
        continue;
      }
      if (c === "=") { emit("punct", c); keyPos = false; continue; }
      if (c === "." && keyPos) { emit("punct", c); continue; }
      if (c === "[" || c === "{") { open.push(c); emit("punct", c); keyPos = c === "{"; continue; }
      if (c === "]" || c === "}") { open.pop(); emit("punct", c); continue; }
      if (c === ",") { emit("punct", c); keyPos = open[open.length - 1] === "{"; continue; }
      let done = false;
      for (const [kind, re] of PATTERNS) {
        if ((part = match(re, text, at))) { emit(kind, part); done = true; break; }
      }
      if (!done) emit(null, c);
    }
    // A trailing newline needs a line to show in the <pre>.
    return out + "\n";
  }

  document.querySelectorAll(".editor-wrap").forEach(function (wrap) {
    const textarea = wrap.querySelector("textarea");
    const code = wrap.querySelector("pre code");
    const pre = code.parentElement;
    const render = () => { code.innerHTML = highlight(textarea.value); };
    const sync = () => { pre.scrollTop = textarea.scrollTop; pre.scrollLeft = textarea.scrollLeft; };
    textarea.addEventListener("input", () => { render(); sync(); });
    textarea.addEventListener("scroll", sync);
    render();
    wrap.classList.add("ready");
  });

  document.querySelectorAll("pre.toml").forEach(function (pre) {
    pre.innerHTML = highlight(pre.textContent).slice(0, -1);
  });
})();
