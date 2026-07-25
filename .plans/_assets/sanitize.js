/*
 * sanitize.js — mandatory HTML sanitizer (allowlist) + renderMarkdown wrapper.
 * Strips <script>/<style>, event handlers (on*), javascript:/data: URLs,
 * and any tag not in the allowlist. Applied unconditionally (spec §7.3).
 * Plan task P0-2.
 */
(function (root, factory) {
  if (typeof module === "object" && module.exports) {
    module.exports = factory(require("./md.js"));
  } else {
    root.EngPlanSanitize = factory(root.EngPlanMd);
  }
})(typeof self !== "undefined" ? self : this, function (Md) {
  "use strict";

  var ALLOWED_TAGS = {
    p: 1, br: 1, hr: 1, h1: 1, h2: 1, h3: 1, h4: 1, h5: 1, h6: 1,
    ul: 1, ol: 1, li: 1, pre: 1, code: 1, strong: 1, em: 1, b: 1, i: 1,
    blockquote: 1, a: 1, span: 1, div: 1, table: 1, thead: 1, tbody: 1,
    tr: 1, th: 1, td: 1
  };
  var ALLOWED_ATTRS = { a: { href: 1, title: 1 }, "*": { class: 1 } };

  function safeUrl(url) {
    var u = String(url).trim();
    // reject anything with a dangerous scheme
    var lower = u.toLowerCase().replace(/[\s\u0000-\u001f]+/g, "");
    if (/^javascript:/.test(lower) || /^vbscript:/.test(lower) || /^data:/.test(lower) || /^file:/.test(lower)) {
      return null;
    }
    return u;
  }

  function escAttr(s) {
    return String(s).replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  // String-based sanitizer (works without a DOM). Tokenizes tags; drops
  // disallowed tags entirely (incl. their content for script/style); filters
  // attributes; neutralizes dangerous URLs.
  function sanitizeHtml(html) {
    if (html == null) return "";
    var input = String(html);
    // Remove script/style blocks with their content outright.
    input = input.replace(/<\s*(script|style)[\s\S]*?<\s*\/\s*\1\s*>/gi, "");
    // Remove HTML comments.
    input = input.replace(/<!--[\s\S]*?-->/g, "");

    var out = "";
    var re = /<\/?([a-zA-Z][a-zA-Z0-9]*)((?:[^>"']|"[^"]*"|'[^']*')*)>/g;
    var last = 0;
    var m;
    while ((m = re.exec(input)) !== null) {
      // text before this tag — already-escaped entities are fine; raw < handled by regex boundaries
      out += input.slice(last, m.index);
      last = re.lastIndex;
      var full = m[0];
      var tag = m[1].toLowerCase();
      var isClose = /^<\s*\//.test(full);

      if (!ALLOWED_TAGS[tag]) {
        // drop the tag entirely (content, if any, remains as text)
        continue;
      }
      if (isClose) { out += "</" + tag + ">"; continue; }

      // self-closing?
      var selfClose = /\/\s*>$/.test(full);
      var attrsStr = m[2] || "";
      var attrs = "";
      var attrRe = /([a-zA-Z_:][-a-zA-Z0-9_:.]*)\s*=\s*("([^"]*)"|'([^']*)'|([^\s"'>]+))/g;
      var am;
      while ((am = attrRe.exec(attrsStr)) !== null) {
        var name = am[1].toLowerCase();
        var val = am[3] !== undefined ? am[3] : (am[4] !== undefined ? am[4] : am[5]);
        if (/^on/.test(name)) continue;            // event handlers
        if (name === "style") continue;            // inline style → drop
        if (name === "srcset" || name === "src") continue;
        var allow = (ALLOWED_ATTRS[tag] && ALLOWED_ATTRS[tag][name]) || (ALLOWED_ATTRS["*"] && ALLOWED_ATTRS["*"][name]);
        if (!allow) continue;
        if (name === "href") {
          var safe = safeUrl(val);
          if (safe === null) continue;
          val = safe;
        }
        attrs += " " + name + '="' + escAttr(val) + '"';
      }
      out += "<" + tag + attrs + (selfClose ? " />" : ">");
    }
    out += input.slice(last);
    return out;
  }

  function renderMarkdown(src) {
    return sanitizeHtml(Md.mdToHtml(src));
  }

  return { sanitizeHtml: sanitizeHtml, renderMarkdown: renderMarkdown, safeUrl: safeUrl };
});
