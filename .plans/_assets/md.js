/*
 * md.js — tiny, dependency-free markdown→HTML. Output is RAW (unsafe) HTML;
 * always pass through sanitize.js before inserting into the DOM. No CDN.
 * Plan task P0-2.
 */
(function (root, factory) {
  if (typeof module === "object" && module.exports) module.exports = factory();
  else root.EngPlanMd = factory();
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  function esc(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function inline(text) {
    // escape first, then re-introduce a small safe set of markup
    var out = esc(text);
    // inline code
    out = out.replace(/`([^`]+)`/g, function (_, c) { return "<code>" + c + "</code>"; });
    // bold then italic
    out = out.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
    out = out.replace(/\b_([^_]+)_\b/g, "<em>$1</em>");
    out = out.replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>");
    // links [text](url)
    out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, function (_, label, url) {
      return '<a href="' + url + '">' + label + "</a>";
    });
    return out;
  }

  function mdToHtml(src) {
    if (src == null) return "";
    var lines = String(src).replace(/\r\n?/g, "\n").split("\n");
    var html = [];
    var i = 0;
    var listOpen = null; // 'ul' | 'ol'

    function closeList() { if (listOpen) { html.push("</" + listOpen + ">"); listOpen = null; } }

    while (i < lines.length) {
      var line = lines[i];

      // fenced code block
      var fence = line.match(/^```(.*)$/);
      if (fence) {
        closeList();
        var buf = [];
        i++;
        while (i < lines.length && !/^```/.test(lines[i])) { buf.push(lines[i]); i++; }
        i++; // skip closing fence
        html.push("<pre><code>" + esc(buf.join("\n")) + "</code></pre>");
        continue;
      }

      // heading
      var h = line.match(/^(#{1,6})\s+(.*)$/);
      if (h) {
        closeList();
        var lvl = h[1].length;
        html.push("<h" + lvl + ">" + inline(h[2]) + "</h" + lvl + ">");
        i++;
        continue;
      }

      // unordered list
      var ul = line.match(/^\s*[-*]\s+(.*)$/);
      if (ul) {
        if (listOpen !== "ul") { closeList(); html.push("<ul>"); listOpen = "ul"; }
        html.push("<li>" + inline(ul[1]) + "</li>");
        i++;
        continue;
      }
      // ordered list
      var ol = line.match(/^\s*\d+\.\s+(.*)$/);
      if (ol) {
        if (listOpen !== "ol") { closeList(); html.push("<ol>"); listOpen = "ol"; }
        html.push("<li>" + inline(ol[1]) + "</li>");
        i++;
        continue;
      }

      // blank line
      if (/^\s*$/.test(line)) { closeList(); i++; continue; }

      // paragraph (accumulate consecutive non-empty, non-special lines)
      closeList();
      var para = [line];
      i++;
      while (i < lines.length && !/^\s*$/.test(lines[i]) &&
             !/^(#{1,6})\s/.test(lines[i]) && !/^```/.test(lines[i]) &&
             !/^\s*[-*]\s+/.test(lines[i]) && !/^\s*\d+\.\s+/.test(lines[i])) {
        para.push(lines[i]);
        i++;
      }
      html.push("<p>" + inline(para.join(" ")) + "</p>");
    }
    closeList();
    return html.join("\n");
  }

  return { mdToHtml: mdToHtml, escapeHtml: esc };
});
