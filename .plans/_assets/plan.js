/*
 * plan.js — engplan/1 renderer + Plan Inbox UI. Vanilla JS, no deps, no CDN.
 * Universal module: in the browser it auto-boots from #plan JSON; in Node tests
 * it is required and given an injected `document`/`sanitize`/`fetch`.
 * Plan P0-3, P0-4, P0-5, P2-3, P3-2, P3-6, P3-7, P4-1.
 */
(function (root, factory) {
  if (typeof module === "object" && module.exports) {
    module.exports = factory(require("./engplan.js"), require("./sanitize.js"));
  } else {
    root.PlanRenderer = factory(root.EngPlan, root.EngPlanSanitize);
  }
})(typeof self !== "undefined" ? self : this, function (EngPlan, Sanitize) {
  "use strict";

  // The 14 (+comment) section actions (spec §3.2).
  var ACTIONS = EngPlan.EVENT_TYPES;

  function doc(opts) {
    if (opts && opts.document) return opts.document;
    if (typeof document !== "undefined") return document;
    throw new Error("no document available");
  }

  function el(d, tag, attrs, text) {
    var n = d.createElement(tag);
    if (attrs) Object.keys(attrs).forEach(function (k) {
      if (k === "class") n.className = attrs[k];
      else if (k === "dataset") Object.keys(attrs[k]).forEach(function (dk) { n.dataset[dk] = attrs[k][dk]; });
      else n.setAttribute(k, attrs[k]);
    });
    if (text != null) n.textContent = String(text);
    return n;
  }

  function badge(d, cls, label) { return el(d, "span", { class: "badge badge-" + cls }, label); }

  function renderSection(d, section, ctx) {
    ctx = ctx || {};
    var wrap = el(d, "section", { class: "plan-section type-" + section.type, dataset: { sectionId: section.id } });
    wrap.setAttribute("data-section-id", section.id);

    var head = el(d, "div", { class: "section-head" });
    head.appendChild(el(d, "h2", { class: "section-heading" }, section.heading));

    if (section.type === "task" && section.state) head.appendChild(badge(d, "state-" + section.state, section.state));
    if (section.approval && section.approval !== "none") head.appendChild(badge(d, "approval", section.approval));
    if (section.risk && section.risk !== "none") head.appendChild(badge(d, "risk", section.risk));
    if (section.halted || (ctx.halted && ctx.halted.indexOf(section.id) !== -1)) head.appendChild(badge(d, "halted", "halted"));
    if (ctx.legacy) head.appendChild(badge(d, "legacy", "legacy / degraded"));
    wrap.appendChild(head);

    var bodyHtml = (Sanitize && Sanitize.renderMarkdown) ? Sanitize.renderMarkdown(section.md || "") : "";
    var body = el(d, "div", { class: "section-body" });
    body.innerHTML = bodyHtml; // sanitized
    wrap.appendChild(body);

    if (section.acceptance && section.acceptance.length) {
      wrap.appendChild(el(d, "h3", { class: "list-title" }, "Acceptance"));
      var ul = el(d, "ul", { class: "acceptance" });
      section.acceptance.forEach(function (a) { ul.appendChild(el(d, "li", null, a)); });
      wrap.appendChild(ul);
    }
    if (section.verification && section.verification.length) {
      wrap.appendChild(el(d, "h3", { class: "list-title" }, "Verification"));
      var vul = el(d, "ul", { class: "verification" });
      section.verification.forEach(function (v) { vul.appendChild(el(d, "li", null, v)); });
      wrap.appendChild(vul);
    }

    // Inbox / action UI (P3-2)
    if (!ctx.noActions) wrap.appendChild(renderActions(d, section, ctx));
    // Note thread
    wrap.appendChild(renderThread(d, section, ctx));
    return wrap;
  }

  function renderActions(d, section, ctx) {
    var box = el(d, "div", { class: "section-actions" });
    var sel = el(d, "select", { class: "action-type", dataset: { sectionId: section.id } });
    ACTIONS.forEach(function (a) { sel.appendChild(el(d, "option", { value: a }, a)); });
    var ta = el(d, "textarea", { class: "action-text", placeholder: "note / rationale…" });
    var btn = el(d, "button", { class: "action-submit", type: "button" }, "Send");
    if (btn.addEventListener) btn.addEventListener("click", function () {
      submitEvent(section, sel.value, ta.value, ctx);
      ta.value = "";
    });
    box.appendChild(sel); box.appendChild(ta); box.appendChild(btn);
    return box;
  }

  function renderThread(d, section, ctx) {
    var thread = el(d, "div", { class: "note-thread", dataset: { sectionId: section.id } });
    var events = (ctx.events || []).filter(function (e) { return e.section_id === section.id; });
    events.forEach(function (ev) {
      var item = el(d, "div", { class: "note-item status-" + (ev.status || "open"), dataset: { eventId: ev.id || "" } });
      item.appendChild(el(d, "span", { class: "note-actor" }, (ev.actor || "?") + "/" + (ev.author || "?")));
      item.appendChild(el(d, "span", { class: "note-type" }, ev.type));
      // note text is sanitized on display (no stored-XSS)
      var txt = el(d, "div", { class: "note-text" });
      txt.textContent = ev.text || ""; // textContent → inert
      item.appendChild(txt);
      if (ev.agent_response) {
        var ar = el(d, "div", { class: "agent-response" });
        ar.textContent = ev.agent_response;
        item.appendChild(ar);
      }
      thread.appendChild(item);
    });
    return thread;
  }

  function submitEvent(section, type, text, ctx) {
    var payload = {
      plan_id: ctx.slug, section_id: section.id, type: type,
      actor: "human", author: ctx.author || "operator", text: text || "",
    };
    var f = (ctx.fetch) || (typeof fetch !== "undefined" ? fetch : null);
    if (f) {
      var tk = resolveToken(ctx);
      return f("/api/notes" + (tk ? "?token=" + encodeURIComponent(tk) : ""), {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Plan-Token": tk },
        body: JSON.stringify(payload),
      });
    }
    // file:// fallback → localStorage
    if (typeof localStorage !== "undefined") {
      var key = "engplan:" + ctx.slug + ":" + section.id;
      var arr = [];
      try { arr = JSON.parse(localStorage.getItem(key) || "[]"); } catch (_) {}
      payload.id = "local_" + Date.now();
      payload.status = "open";
      arr.push(payload);
      localStorage.setItem(key, JSON.stringify(arr));
    }
    return Promise.resolve(payload);
  }

  function loadLocalNotes(slug, sectionId) {
    if (typeof localStorage === "undefined") return [];
    try { return JSON.parse(localStorage.getItem("engplan:" + slug + ":" + sectionId) || "[]"); } catch (_) { return []; }
  }

  function renderPlan(appEl, plan, opts) {
    opts = opts || {};
    var d = doc(opts);
    var p;
    try { p = plan.schema ? EngPlan.parseEngPlan(plan) : plan; }
    catch (e) {
      appEl.innerHTML = "";
      appEl.appendChild(el(d, "div", { class: "plan-error" }, "Plan failed to load: " + e.message));
      return;
    }
    var ctx = {
      slug: p.slug, events: opts.events || [], halted: opts.halted || [],
      legacy: opts.legacy || false, fetch: opts.fetch, token: opts.token, author: opts.author,
      noActions: opts.noActions,
    };
    appEl.innerHTML = "";
    var header = el(d, "header", { class: "plan-header" });
    header.appendChild(el(d, "h1", null, p.title));
    header.appendChild(badge(d, "status", p.status));
    if (p.convergence && p.convergence !== "none") header.appendChild(badge(d, "convergence", "convergence: " + p.convergence));
    appEl.appendChild(header);

    var container = el(d, "div", { class: "sections", dataset: { slug: p.slug } });
    p.sections.forEach(function (s) {
      // merge localStorage notes when offline
      if (!ctx.fetch && typeof localStorage !== "undefined") {
        ctx.events = ctx.events.concat(loadLocalNotes(p.slug, s.id));
      }
      container.appendChild(renderSection(d, s, ctx));
    });
    appEl.appendChild(container);
    appEl._plan = p; appEl._ctx = ctx;
    return p;
  }

  // P0-4: section-id keyed patch (append/replace) without full reload
  function applySectionPatch(plan, patch, appEl, opts) {
    opts = opts || {};
    var d = doc(opts);
    if (!patch || typeof patch.id !== "string") return false;
    var section;
    try { section = EngPlan.parseSection(patch); } catch (_) { return false; }
    var container = appEl.querySelector ? appEl.querySelector(".sections") : null;
    if (!container) return false;
    var ctx = appEl._ctx || { slug: plan.slug, events: [] };
    var existingIdx = plan.sections.findIndex(function (s) { return s.id === section.id; });
    var newNode = renderSection(d, section, ctx);
    if (existingIdx === -1) {
      plan.sections.push(section);
      container.appendChild(newNode);
    } else {
      plan.sections[existingIdx] = section;
      var old = container.querySelector('[data-section-id="' + cssEsc(section.id) + '"]');
      if (old && old.parentNode) old.parentNode.replaceChild(newNode, old);
      else container.appendChild(newNode);
    }
    return true;
  }

  function cssEsc(s) { return String(s).replace(/["\\]/g, "\\$&"); }

  // Token resolver shared by notes fetch + SSE subscribe (CSP-safe, no inline globals needed).
  // Order: injected global → URL ?token= → ctx/opts.token → "".
  function resolveToken(ctx) {
    if (typeof window !== "undefined") {
      if (window.__PLAN_TOKEN__) return window.__PLAN_TOKEN__;
      try {
        if (window.location && window.location.search) {
          var qt = new URLSearchParams(window.location.search).get("token");
          if (qt) return qt;
        }
      } catch (_) {}
    }
    return (ctx && ctx.token) || "";
  }

  // P2-3: SSE live subscription
  function subscribeLive(slug, onPatch, opts) {
    opts = opts || {};
    var ES = opts.EventSource || (typeof EventSource !== "undefined" ? EventSource : null);
    if (!ES) return { close: function () {} };
    var tk = resolveToken(opts);
    var es = new ES("/api/stream?plan=" + encodeURIComponent(slug) + (tk ? "&token=" + encodeURIComponent(tk) : ""));
    es.onmessage = function (e) {
      var data; try { data = JSON.parse(e.data); } catch (_) { return; }
      onPatch(data);
    };
    return { close: function () { try { es.close(); } catch (_) {} }, es: es };
  }

  // ---- legacy .md parsing (P4-1) ----
  function parseLegacyMarkdown(mdText, slug) {
    var lines = String(mdText).replace(/\r\n?/g, "\n").split("\n");
    var sections = [];
    var seen = {};
    var cur = null;
    function slugify(h) {
      var base = h.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "section";
      var id = base, n = 1;
      while (seen[id]) { id = base + "-" + (++n); }
      seen[id] = true;
      return id;
    }
    lines.forEach(function (line) {
      var h = line.match(/^(#{1,6})\s+(.*)$/);
      if (h) {
        if (cur) sections.push(cur);
        cur = { id: slugify(h[2]), heading: h[2], type: "prose", md: "" };
      } else if (cur) {
        cur.md += (cur.md ? "\n" : "") + line;
      } else {
        cur = { id: slugify("intro"), heading: "Introduction", type: "prose", md: line };
      }
    });
    if (cur) sections.push(cur);
    return { schema: "engplan/1", kind: "plan", slug: slug || "legacy", title: (sections[0] && sections[0].heading) || "Legacy plan", status: "drafting", convergence: "none", sections: sections };
  }

  function boot(opts) {
    opts = opts || {};
    var d = doc(opts);
    var appEl = (d.getElementById && d.getElementById("app")) || opts.appEl;
    var raw = d.getElementById && d.getElementById("plan");
    if (!appEl || !raw) return;
    var data; try { data = JSON.parse(raw.textContent || "{}"); } catch (_) { data = {}; }
    var plan = renderPlan(appEl, data, opts);
    if (plan && plan.slug) {
      subscribeLive(plan.slug, function (patch) {
        if (patch && patch.type === "filechange") {
          // re-fetch artifact on filechange in real browser; tests inject patches directly
        }
      }, opts);
    }
  }

  if (typeof document !== "undefined" && typeof window !== "undefined") {
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", function () { boot(); });
    else boot();
  }

  return {
    ACTIONS: ACTIONS,
    renderPlan: renderPlan,
    renderSection: renderSection,
    applySectionPatch: applySectionPatch,
    subscribeLive: subscribeLive,
    resolveToken: resolveToken,
    submitEvent: submitEvent,
    parseLegacyMarkdown: parseLegacyMarkdown,
    boot: boot,
  };
});
