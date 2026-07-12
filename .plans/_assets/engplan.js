/*
 * engplan.js — engplan/1 boundary parser, validators, and event lifecycle.
 * Universal module: works under Node `require` and browser `<script>`.
 * Spec §5.1, §5.2, §5.3, §3.3. Plan tasks P0-1, P3-0, P3-4.
 */
(function (root, factory) {
  if (typeof module === "object" && module.exports) module.exports = factory();
  else root.EngPlan = factory();
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  var SCHEMA = "engplan/1";

  var KINDS = ["plan", "spec"];
  var PLAN_STATUS = ["drafting", "approved", "in_progress", "done", "blocked"];
  var CONVERGENCE = ["none", "informed", "holdout"];
  var SECTION_TYPES = ["prose", "task", "risk", "gate", "criteria", "evidence"];
  var TASK_STATE = ["todo", "doing", "done", "blocked"];
  var APPROVAL = ["none", "needs-human-review", "approved"];
  var RISK = ["none", "risky", "security-sensitive"];

  // §3.2 — the 14 (15 incl. comment) section actions.
  var EVENT_TYPES = [
    "comment", "request_change", "block", "approve", "reprioritize",
    "mark_risky", "add_acceptance_criterion", "clarify", "force_verification",
    "defer", "split_task", "merge_task", "escalate_convergence",
    "require_security_review", "do_not_touch"
  ];
  var ACTORS = ["human", "orchestrator", "agent"];
  var EVENT_STATUS = ["open", "acknowledged", "incorporated", "rejected", "deferred", "blocked"];
  var AGENT_STATUS = [null, "incorporated", "rejected", "deferred", "blocked"];

  function ValidationError(msg) {
    var e = new Error(msg);
    e.name = "ValidationError";
    return e;
  }

  function isObj(v) { return v !== null && typeof v === "object" && !Array.isArray(v); }
  function isStr(v) { return typeof v === "string"; }
  function isNonEmptyStr(v) { return typeof v === "string" && v.length > 0; }
  function inEnum(v, arr) { return arr.indexOf(v) !== -1; }

  var ID_RE = /^[A-Za-z0-9][A-Za-z0-9_.-]*$/;
  function validId(v) { return isNonEmptyStr(v) && v.length <= 200 && ID_RE.test(v); }

  function parseSection(raw, seen) {
    if (!isObj(raw)) throw ValidationError("section must be an object");
    if (!validId(raw.id)) throw ValidationError("section.id invalid or missing: " + JSON.stringify(raw.id));
    if (seen) {
      if (seen[raw.id]) throw ValidationError("duplicate section id: " + raw.id);
      seen[raw.id] = true;
    }
    if (!isNonEmptyStr(raw.heading)) throw ValidationError("section.heading required (" + raw.id + ")");
    if (!inEnum(raw.type, SECTION_TYPES)) throw ValidationError("section.type invalid (" + raw.id + "): " + raw.type);

    var s = {
      id: raw.id,
      heading: raw.heading,
      type: raw.type,
      md: isStr(raw.md) ? raw.md : "",
    };
    if (raw.state !== undefined) {
      if (!inEnum(raw.state, TASK_STATE)) throw ValidationError("section.state invalid (" + raw.id + "): " + raw.state);
      s.state = raw.state;
    }
    if (raw.approval !== undefined) {
      if (!inEnum(raw.approval, APPROVAL)) throw ValidationError("section.approval invalid (" + raw.id + "): " + raw.approval);
      s.approval = raw.approval;
    }
    if (raw.risk !== undefined) {
      if (!inEnum(raw.risk, RISK)) throw ValidationError("section.risk invalid (" + raw.id + "): " + raw.risk);
      s.risk = raw.risk;
    }
    if (raw.acceptance !== undefined) {
      if (!Array.isArray(raw.acceptance) || !raw.acceptance.every(isStr)) throw ValidationError("section.acceptance must be string[] (" + raw.id + ")");
      s.acceptance = raw.acceptance.slice();
    }
    if (raw.verification !== undefined) {
      if (!Array.isArray(raw.verification) || !raw.verification.every(isStr)) throw ValidationError("section.verification must be string[] (" + raw.id + ")");
      s.verification = raw.verification.slice();
    }
    if (raw.depends_on !== undefined) {
      if (!Array.isArray(raw.depends_on) || !raw.depends_on.every(isStr)) throw ValidationError("section.depends_on must be string[] (" + raw.id + ")");
      s.depends_on = raw.depends_on.slice();
    }
    if (typeof raw.human_notes === "number") s.human_notes = raw.human_notes;
    if (typeof raw.agent_response_required === "boolean") s.agent_response_required = raw.agent_response_required;
    if (typeof raw.halted === "boolean") s.halted = raw.halted; // computed/persisted halt flag
    // preserve-and-ignore unknown fields under _ext for round-trip fidelity
    return s;
  }

  function parseEngPlan(raw) {
    if (isStr(raw)) {
      try { raw = JSON.parse(raw); } catch (e) { throw ValidationError("plan is not valid JSON: " + e.message); }
    }
    if (!isObj(raw)) throw ValidationError("plan must be an object");
    if (raw.schema !== SCHEMA) throw ValidationError("unsupported schema: " + JSON.stringify(raw.schema) + " (want " + SCHEMA + ")");
    if (!inEnum(raw.kind, KINDS)) throw ValidationError("plan.kind invalid: " + raw.kind);
    if (!isNonEmptyStr(raw.slug)) throw ValidationError("plan.slug required");
    if (!validId(raw.slug)) throw ValidationError("plan.slug invalid id: " + raw.slug);
    if (!isNonEmptyStr(raw.title)) throw ValidationError("plan.title required");
    if (!inEnum(raw.status, PLAN_STATUS)) throw ValidationError("plan.status invalid: " + raw.status);
    if (!Array.isArray(raw.sections)) throw ValidationError("plan.sections must be an array");

    var seen = {};
    var sections = raw.sections.map(function (sec) { return parseSection(sec, seen); });

    var plan = {
      schema: SCHEMA,
      kind: raw.kind,
      slug: raw.slug,
      title: raw.title,
      status: raw.status,
      convergence: inEnum(raw.convergence, CONVERGENCE) ? raw.convergence : "none",
      created_at: isStr(raw.created_at) ? raw.created_at : null,
      updated_at: isStr(raw.updated_at) ? raw.updated_at : null,
      sections: sections,
    };
    return plan;
  }

  function parseEvent(raw) {
    if (isStr(raw)) {
      try { raw = JSON.parse(raw); } catch (e) { throw ValidationError("event is not valid JSON: " + e.message); }
    }
    if (!isObj(raw)) throw ValidationError("event must be an object");
    if (!isNonEmptyStr(raw.plan_id)) throw ValidationError("event.plan_id required");
    if (!isNonEmptyStr(raw.section_id)) throw ValidationError("event.section_id required");
    if (!inEnum(raw.type, EVENT_TYPES)) throw ValidationError("event.type invalid: " + raw.type);
    var actor = inEnum(raw.actor, ACTORS) ? raw.actor : null;
    if (!actor) throw ValidationError("event.actor invalid: " + raw.actor);
    var status = raw.status === undefined ? "open" : raw.status;
    if (!inEnum(status, EVENT_STATUS)) throw ValidationError("event.status invalid: " + status);
    if (raw.text !== undefined && !isStr(raw.text)) throw ValidationError("event.text must be string");
    var ev = {
      id: isNonEmptyStr(raw.id) ? raw.id : null,
      plan_id: raw.plan_id,
      section_id: raw.section_id,
      type: raw.type,
      actor: actor,
      author: isStr(raw.author) ? raw.author : actor,
      text: isStr(raw.text) ? raw.text : "",
      status: status,
      created_at: isStr(raw.created_at) ? raw.created_at : null,
      agent_status: inEnum(raw.agent_status, AGENT_STATUS) ? raw.agent_status : null,
      agent_response: isStr(raw.agent_response) ? raw.agent_response : null,
      changed_sections: Array.isArray(raw.changed_sections) ? raw.changed_sections.filter(isStr) : [],
      responded_at: isStr(raw.responded_at) ? raw.responded_at : null,
    };
    if (raw.payload !== undefined && isObj(raw.payload)) ev.payload = raw.payload; // for add_criterion etc.
    return ev;
  }

  // A note is the lightweight form of a comment event; same store.
  function parseNote(raw) {
    var ev = parseEvent(Object.assign({ type: "comment" }, raw));
    return ev;
  }

  // ---- Lifecycle state machine (§3.3) ----
  // open → acknowledged → incorporated | rejected | deferred | blocked
  var TERMINAL = ["incorporated", "rejected", "deferred"];
  var TRANSITIONS = {
    open: ["acknowledged", "blocked"],
    acknowledged: ["incorporated", "rejected", "deferred", "blocked"],
    blocked: ["acknowledged", "incorporated", "rejected", "deferred"],
    incorporated: [],
    rejected: [],
    deferred: [],
  };

  function canTransition(from, to) {
    if (!inEnum(from, EVENT_STATUS) || !inEnum(to, EVENT_STATUS)) return false;
    return (TRANSITIONS[from] || []).indexOf(to) !== -1;
  }

  function transition(ev, to) {
    if (!isObj(ev)) throw ValidationError("transition: event required");
    var from = ev.status || "open";
    if (!canTransition(from, to)) {
      throw ValidationError("illegal transition: " + from + " → " + to);
    }
    var next = Object.assign({}, ev, { status: to });
    return next;
  }

  function isTerminal(status) { return TERMINAL.indexOf(status) !== -1; }

  return {
    SCHEMA: SCHEMA,
    KINDS: KINDS, PLAN_STATUS: PLAN_STATUS, CONVERGENCE: CONVERGENCE,
    SECTION_TYPES: SECTION_TYPES, TASK_STATE: TASK_STATE, APPROVAL: APPROVAL, RISK: RISK,
    EVENT_TYPES: EVENT_TYPES, ACTORS: ACTORS, EVENT_STATUS: EVENT_STATUS,
    TERMINAL: TERMINAL,
    ValidationError: ValidationError,
    validId: validId,
    parseEngPlan: parseEngPlan,
    parseSection: parseSection,
    parseEvent: parseEvent,
    parseNote: parseNote,
    canTransition: canTransition,
    transition: transition,
    isTerminal: isTerminal,
  };
});
