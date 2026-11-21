// qa-judge-smoke.mjs — the runtime smoke layer of the bench judge
// (`showdown judge`). Zero-dependency (node 22+ global WebSocket + fetch),
// same CDP transport as qa-smoke.mjs and qa-flow.mjs.
//
// It opens each static route of a running app in headless Chrome/Edge,
// records uncaught exceptions and console errors, then runs the
// requirement-driven flows of an objective (`smoke.flows` in
// `bench/showdown/objectives/<name>.reqs.json`). The flows do not know the
// app's markup: every control is found by role, label, placeholder, aria
// name or visible text, through the heuristics below. A step that cannot
// find its control reports `not_found`; a step that finds it but sees the
// wrong result reports `failed`. The two are reported apart.
//
// Usage:
//   node qa-judge-smoke.mjs --url http://127.0.0.1:3000 --routes /,/board
//        --flows flows.json --out smoke.json [--budget-ms 90000]
//        [--cdp-port 9340] [--step-ms 4000]
//
// Exit codes: 0 report written (whatever the flows did), 2 no browser or
// a usage error (the report then has "error").
//
// Time: every CDP call has a timeout (a page that loops forever fails its
// step, and its script is terminated), JS dialogs are answered (alert: OK,
// confirm: OK, prompt: cancel; an alert that reads like an error fails the
// flow), the report is rewritten after every route and flow, and a watchdog
// writes it and exits 10 s after the budget. Flows the budget cuts off are
// `skipped` and routes it cuts off are `ok: false`: the judge counts both as
// failed.
//
// Step kinds (one key per step object; `{n}` is a per-run nonce, `{today}`
// is today's date as YYYY-MM-DD, `{name}` is a value an earlier step stored):
//   goto      "/path" or ["/path", "/other"] (the first that is a route)
//   reload    true
//   click     {"control": "regex", "within": "text", "optional": true}
//             (`within`: only the controls on that item's row or card)
//   form      {"open": "regex", "fields": [{"label": "regex", "value": "text",
//              "option": "regex", "optional": true}], "submit": "regex"}
//   expectText "text"
//   clickText {"text": "text", "control": "regex"}
//   pickItem  {"as": "name", "prefer": "text", "column": "regex", "columns": "regex"}
//             or {"as": "name", "prefer": "text", "rows": "css", "has": "regex",
//             "expense": true, "amountAs": "name"} (a row with a control matching
//             `has`, or one that shows an expense; `amountAs` stores its signed amount)
//   pickLink  {"as": "name", "href": "regex", "prefer": "text"}
//   move      {"item": "text", "to": "regex", "columns": "regex", "verify": true}
//             (`columns`: the board's column names, to find the columns)
//   read      {"near": "regex", "as": "name"}
//   expect    {"near": "regex", "ref": "name", "by": -12.34, "op": ">", "value": 0}
//             or {"near": "regex", "ref": "name", "delta": {"from": "name", "to": -23.45}}
//             (a change of to - {from}); a `ref` that no earlier step read fails
//   snapshot  {"as": "name", "near": "regex"} or {"as": "name", "item": "text"}
//   changed   {"ref": "name", "near": "regex", "item": "text", "noError": true}
//   nonzero   {"min": 1}
//   expectNear {"item": "text", "text": "text"} (the item's row or card shows it)

import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CdpClient, findChrome, newPageTarget, sendRetry, sleep, waitForDebugger } from "./qa-cdp.mjs";

function parseArgs(argv) {
  const args = { url: "", routes: "/", flows: null, out: "smoke.json", "budget-ms": 90000, "cdp-port": 9340, "step-ms": 4000 };
  for (let i = 2; i < argv.length; i += 1) {
    const key = argv[i];
    if (!key.startsWith("--")) continue;
    const name = key.slice(2);
    const value = argv[i + 1];
    if (name in args) {
      args[name] = typeof args[name] === "number" ? Number(value) : value;
      i += 1;
    }
  }
  return args;
}

// ---- In-page helpers --------------------------------------------------------
// Evaluated inside the page with each step; `S` is the step's argument.

const HELPERS = String.raw`
const vis = (el) => !!el && (el.offsetWidth > 0 || el.offsetHeight > 0 || el.getClientRects().length > 0)
  && getComputedStyle(el).visibility !== "hidden";
const txt = (el) => ((el && (el.innerText || el.textContent)) || "").replace(/\s+/g, " ").trim();
const rx = (s) => new RegExp(s, "i");
const CONTROLS = "button, a[href], [role=button], [role=tab], [role=menuitem], input[type=submit], input[type=button], summary";
const controlName = (el) => [el.getAttribute("aria-label"), el.getAttribute("title"),
  el.tagName === "INPUT" ? el.value : txt(el)].filter(Boolean).join(" ").trim();
const controls = (root) => [...(root || document).querySelectorAll(CONTROLS)].filter(vis);
const findControl = (re, root) => {
  const found = controls(root).filter((el) => re.test(controlName(el)));
  found.sort((a, b) => controlName(a).length - controlName(b).length);
  return found[0] || null;
};
const FIELDS = "input:not([type=hidden]):not([type=checkbox]):not([type=radio]):not([type=submit]):not([type=button]):not([type=file]), textarea, select";
const fieldLabel = (el) => {
  const parts = [el.getAttribute("aria-label"), el.getAttribute("placeholder"), el.getAttribute("name"), el.id];
  if (el.labels) for (const label of el.labels) parts.push(txt(label));
  const wrap = el.closest("label");
  if (wrap) parts.push(txt(wrap));
  const by = el.getAttribute("aria-labelledby");
  if (by) for (const id of by.split(/\s+/)) parts.push(txt(document.getElementById(id)));
  const prev = el.previousElementSibling;
  if (prev && txt(prev).length < 40) parts.push(txt(prev));
  return parts.filter(Boolean).join(" | ");
};
const fields = (root, wantSelect) => [...(root || document).querySelectorAll(FIELDS)].filter(vis)
  .filter((el) => !el.disabled && !el.readOnly)
  .filter((el) => wantSelect === undefined || (el.tagName === "SELECT") === wantSelect);
// A field whose label matches. Search and filter boxes only match a
// pattern that asks for them, and a field in the page's own content wins
// over one in the header, the navigation or a sidebar.
const SEARCHY = /search|filter/i;
const findField = (re, root, wantSelect) => {
  const found = fields(root, wantSelect).filter((el) => re.test(fieldLabel(el)))
    .filter((el) => SEARCHY.test(re.source) || !(el.type === "search" || SEARCHY.test(fieldLabel(el))));
  return found.find((el) => !el.closest("header, nav, aside, [role=search], [role=banner]")) || found[0] || null;
};
const setValue = (el, value) => {
  el.scrollIntoView({ block: "center" });
  el.focus();
  if (el.tagName === "SELECT") {
    const re = rx(value);
    const option = [...el.options].find((o) => re.test(o.text) || re.test(o.value));
    if (!option) return false;
    const setter = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value").set;
    setter.call(el, option.value);
  } else {
    const proto = el.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value").set.call(el, value);
  }
  el.dispatchEvent(new Event("input", { bubbles: true }));
  el.dispatchEvent(new Event("change", { bubbles: true }));
  el.dispatchEvent(new Event("blur", { bubbles: true }));
  return true;
};
const today = () => { const d = new Date(); const p = (n) => String(n).padStart(2, "0"); return d.getFullYear() + "-" + p(d.getMonth() + 1) + "-" + p(d.getDate()); };
const NUM = /([-−–]|\()?\s*(?:[$€£¥]|[A-Z]{3}\s)?\s*([-−–])?\s*(\d[\d,\s]*(?:\.\d+)?)\s*(%|\))?/g;
const numbers = (text) => {
  const out = [];
  for (const m of String(text).matchAll(NUM)) {
    if (m[4] === "%") continue;
    const value = Number(m[3].replace(/[,\s]/g, ""));
    if (!Number.isFinite(value)) continue;
    const negative = !!(m[1] || m[2]);
    out.push({ value: negative ? -value : value, raw: m[0].trim() });
  }
  return out;
};
// The number shown next to a label: the label's smallest element, then the
// closest container that holds a number besides the label's own text.
const numberNear = (re) => {
  const all = [...document.querySelectorAll("body *")].filter(vis)
    .filter((el) => { const t = txt(el); return t.length > 0 && t.length < 80 && re.test(t); });
  const labels = all.filter((el) => ![...el.children].some((child) => all.includes(child)));
  for (const label of labels) {
    let box = label;
    for (let depth = 0; depth < 4 && box; depth += 1) {
      const rest = txt(box).replace(re, " ");
      const found = numbers(rest);
      if (found.length) return { value: found[0].value, raw: found[0].raw, label: txt(label) };
      box = box.parentElement;
    }
  }
  return labels.length ? { label: txt(labels[0]) } : null;
};
// A board column: the closest ancestor of a heading that matches the
// column name whose parent also holds another column (a sibling whose text
// starts with one of the flow's column names).
const COLUMN_NAMES = "backlog|to ?do|in progress|review|doing|done|open|closed";
const columnOf = (re, names) => {
  const peerRe = new RegExp("^(" + (names || COLUMN_NAMES) + ")\\b", "i");
  const all = [...document.querySelectorAll("body *")].filter(vis)
    .filter((el) => !/^(OPTION|SELECT|BUTTON|A|INPUT|LABEL|TEXTAREA)$/.test(el.tagName) && !el.closest("select, nav, button, a"))
    .filter((el) => { const t = txt(el); return t.length < 40 && re.test(t.slice(0, 24)) && t.search(re) === 0; });
  const heads = all.filter((el) => ![...el.children].some((child) => all.includes(child)));
  for (const head of heads) {
    let box = head;
    while (box && box.parentElement && box.parentElement !== document.body) {
      const parent = box.parentElement;
      // Another column: a sibling of the same kind whose text starts with a
      // column name.
      const peers = [...parent.children].filter((child) => child !== box && vis(child)
        && child.tagName === box.tagName && child.className === box.className
        && peerRe.test(txt(child)));
      if (peers.length >= 1) return box;
      box = parent;
    }
  }
  return null;
};
const smallestWith = (text) => {
  const lower = String(text).toLowerCase();
  const hits = [...document.querySelectorAll("body *")].filter(vis).filter((el) => txt(el).toLowerCase().includes(lower));
  return hits.filter((el) => ![...el.children].some((child) => hits.includes(child)));
};
// The signed amount a row shows: its money-like number, negative when it is
// written negative or the row says it is an expense.
const EXPENSE = /\b(expense|debit|outflow|spent|withdrawal)\b/i;
const rowAmount = (row) => {
  const found = numbers(txt(row)).filter((n) => /[$€£¥]|\.\d{2}\b/.test(n.raw) && !/^\d{4}$/.test(n.raw));
  if (!found.length) return null;
  const n = found[0];
  return n.value < 0 ? n.value : EXPENSE.test(txt(row)) ? -Math.abs(n.value) : n.value;
};
// The card of an item: the closest ancestor (below the column) that is
// draggable or holds a control, else one that looks like a card.
const cardOf = (el, stop) => {
  const up = [];
  for (let box = el; up.length < 6 && box && box !== stop && box !== document.body; box = box.parentElement) up.push(box);
  return up.find((box) => txt(box).length < 400 && (box.getAttribute("draggable") === "true" || box.querySelector("select, button")))
    || up.find((box) => box.matches("li, article, tr, [role=listitem], [role=row], .card, [class*=card], [class*=task], [class*=item]"))
    || el;
};
// The row or card of the item with this text (the first that shows it).
// A name read with a badge ("Netflix Due soon") loses the badge once the
// item changes: the leading words are tried too, down to the first one.
const rowOf = (text) => {
  const words = String(text).split(" ");
  for (let n = words.length; n >= 1; n -= 1) {
    const part = words.slice(0, n).join(" ");
    if (part.length < 3) break;
    const hits = smallestWith(part);
    if (hits.length) return hits[0].closest("tr, li, article, [role=row]") || cardOf(hits[0]);
  }
  return null;
};
`;

// Each step is one function evaluated in the page: (S, store) => result.
const PAGE = {
  click: `(S) => {
    let root = null;
    if (S.within) {
      root = rowOf(S.within);
      if (!root) return { status: "not_found", detail: "no item " + JSON.stringify(S.within) };
    }
    const el = findControl(rx(S.control), root) || (root && findControl(rx(S.control), root.parentElement));
    if (!el) return { status: S.optional ? "ok" : "not_found", detail: "no control matching /" + S.control + "/" };
    el.scrollIntoView({ block: "center" }); el.click();
    return { status: "ok", detail: controlName(el).slice(0, 60) };
  }`,
  form: `(S) => {
    const specs = S.fields || [];
    const first = specs.find((f) => !f.optional) || specs[0];
    const wantSelect = (f) => f.option !== undefined ? true : f.select ? undefined : false;
    let anchor = first ? findField(rx(first.label), null, wantSelect(first)) : null;
    if (!anchor) return { status: "not_found", detail: "no field matching /" + (first && first.label) + "/", retry: !!S.open };
    // The container: the field's form, else the closest ancestor that holds a
    // control matching the submit pattern.
    const submitRe = rx(S.submit || "save|add|create|submit");
    let box = anchor.closest("form");
    // In a form, a submit button that matches wins over another control
    // that matches ("Add split" against "Add transaction").
    const submits = box ? controls(box).filter((el) => el.type === "submit") : [];
    let button = box ? (submits.find((el) => submitRe.test(controlName(el))) || findControl(submitRe, box) || submits[0] || null) : null;
    if (!button) {
      box = anchor.parentElement;
      while (box && box !== document.body && !findControl(submitRe, box)) box = box.parentElement;
      button = box ? findControl(submitRe, box) : null;
    }
    if (!button) return { status: "not_found", detail: "no submit control matching /" + S.submit + "/ near the field" };
    const filled = [];
    for (const spec of specs) {
      const el = findField(rx(spec.label), box, wantSelect(spec)) || findField(rx(spec.label), null, wantSelect(spec));
      if (!el) {
        if (spec.optional) continue;
        return { status: "not_found", detail: "no field matching /" + spec.label + "/" };
      }
      let value = spec.option !== undefined ? spec.option : spec.value;
      if (el.type === "date" && !/^\\d{4}-\\d{2}-\\d{2}$/.test(value)) value = today();
      // "Amount (negative = expense)": an expense is typed with a minus.
      if (spec.sign === "expense" && /negative|minus|\\(-\\)|use -|- ?(for|=)/i.test(fieldLabel(el)) && !String(value).startsWith("-")) value = "-" + value;
      if (!setValue(el, value)) {
        if (spec.optional) continue;
        return { status: "not_found", detail: "no option /" + value + "/ in the field /" + spec.label + "/" };
      }
      filled.push(fieldLabel(el).slice(0, 30));
    }
    // A toggle (an Expense/Income button pair or radio) inside the form.
    if (S.toggle) {
      const re = rx(S.toggle);
      const radio = [...box.querySelectorAll("input[type=radio], input[type=checkbox]")].find((el) => re.test(fieldLabel(el)) || re.test(el.value));
      const control = radio || controls(box).find((el) => el !== button && re.test(controlName(el)));
      if (control && !control.checked) { control.click(); filled.push("toggle " + (controlName(control) || control.value).slice(0, 20)); }
    }
    // Required fields the flow does not name get a plain value.
    for (const el of fields(box)) {
      if (!el.required || el.value) continue;
      if (el.tagName === "SELECT") { if (el.options.length > 1) setValue(el, el.options[1].value); continue; }
      setValue(el, el.type === "date" ? today() : el.type === "number" ? "1" : el.type === "email" ? "smoke@example.com" : "Smoke");
    }
    // The press is a second call: React applies the typed values before it.
    document.querySelectorAll("[data-smoke-submit]").forEach((el) => el.removeAttribute("data-smoke-submit"));
    button.setAttribute("data-smoke-submit", "1");
    return { status: "ok", detail: "filled " + filled.join(", ") + "; pressed " + controlName(button).slice(0, 30) };
  }`,
  press: `() => {
    const button = document.querySelector("[data-smoke-submit]");
    if (!button) return { status: "failed", detail: "the submit control went away" };
    button.scrollIntoView({ block: "center" }); button.click();
    return { status: "ok" };
  }`,
  bodyText: `() => txt(document.body)`,
  hasText: `(S) => ({ status: (document.body ? document.body.innerText : "").toLowerCase().includes(String(S.text).toLowerCase()) ? "ok" : "failed" })`,
  clickText: `(S) => {
    const hits = smallestWith(S.text);
    if (!hits.length) return { status: "not_found", detail: "no element with the text " + JSON.stringify(S.text) };
    const link = hits.map((el) => el.closest("a[href]")).find(Boolean)
      || [...document.querySelectorAll("a[href]")].filter(vis).find((a) => txt(a).toLowerCase().includes(S.text.toLowerCase()));
    if (link) { link.scrollIntoView({ block: "center" }); link.click(); return { status: "ok", detail: "link " + link.getAttribute("href") }; }
    if (S.control) {
      const row = cardOf(hits[0]);
      const control = findControl(rx(S.control), row) || findControl(rx(S.control), row.parentElement);
      if (control) { control.click(); return { status: "ok", detail: "control " + controlName(control).slice(0, 30) }; }
    }
    hits[0].scrollIntoView({ block: "center" }); hits[0].click();
    return { status: "ok", detail: "clicked the text" };
  }`,
  pickItem: `(S) => {
    const has = S.has ? rx(S.has) : null;
    const fits = (row) => (!has || !!findControl(has, row)) && (!S.expense || (rowAmount(row) ?? 0) < 0);
    const withAmount = (out, row) => {
      if (S.amountAs && row) out.store = { [S.amountAs]: rowAmount(row) };
      return out;
    };
    if (S.prefer && smallestWith(S.prefer).length) {
      const row = rowOf(S.prefer);
      if (!S.rows || (row && fits(row))) return withAmount({ status: "ok", value: S.prefer, detail: "the created item" }, row);
    }
    if (S.rows) {
      // A table or list row (one with a control matching S.has, or one that
      // shows an expense): its link text, else its longest cell that is not a
      // number, a date or a control.
      // nameOf: a name of the row, text that is not a number, an amount or
      // a date, nor a control's label; link texts first, then the longest cells; one that
      // occurs once on the page wins (an account name repeats on every row).
      const nameOf = (row, unique = true) => {
      const controlText = controls(row).filter((el) => el.tagName !== "A").map(controlName);
      const plain = (t) => t.length > 2 && t.length < 60 && !controlText.includes(t) && !(has && has.test(t))
        && !/^[-−+$€£¥(]?\\s*[-−]?\\s*[$€£¥]?\\s*[\\d.,\\s]+\\)?$/.test(t) && !/^\\d{4}-\\d{2}-\\d{2}$/.test(t)
        && !/\\d{1,2}[ /.-]\\w{3,}[ /.-]\\d{2,4}|\\w{3} \\d{1,2},? \\d{4}|^\\d{1,2}\\/\\d{1,2}\\/\\d{2,4}$/.test(t);
      const links = [...row.querySelectorAll("a[href]")].filter(vis).map(txt).filter(plain);
      const cells = [...row.querySelectorAll("td, [role=cell], span, div, h2, h3, h4, strong, a")].filter(vis)
        .filter((el) => !el.closest("button, select, [role=button]") && !el.querySelector("button, select, input, [role=button]")).map(txt)
        .filter(plain);
      cells.sort((a, b) => b.length - a.length);
      const names = [...links, ...cells];
      return (unique && names.find((t) => smallestWith(t).length === 1)) || names[0] || null;
      };
      // The innermost fitting element with a name, first in document order
      // (a card that holds the whole table also "has" the control; a wrapper
      // of the buttons alone has no name).
      const fitting = [...document.querySelectorAll(S.rows)].filter(vis).filter((el) => txt(el).length > 3).filter(fits).filter((el) => !!nameOf(el, false));
      const row = fitting.find((el) => !fitting.some((other) => other !== el && el.contains(other)));
      if (!row) return { status: "not_found", detail: "no row " + S.rows + (has ? " with a control /" + S.has + "/" : "") + (S.expense ? " that shows an expense" : "") };
      const name = nameOf(row);
      return name ? withAmount({ status: "ok", value: name, detail: "a seeded row" }, row) : { status: "not_found", detail: "no text in the row" };
    }
    const column = S.column ? columnOf(rx(S.column), S.columns) : null;
    if (!column) return { status: "not_found", detail: "no column /" + S.column + "/ and no item " + JSON.stringify(S.prefer) };
    const link = [...column.querySelectorAll("a[href]")].filter(vis).find((a) => txt(a).length > 2);
    const card = link || [...column.querySelectorAll("[draggable=true], li, article, .card, [class*=card]")].filter(vis)[0];
    if (!card) return { status: "not_found", detail: "the column /" + S.column + "/ is empty" };
    const name = link ? txt(link) : txt(card).split(" ").slice(0, 4).join(" ");
    return { status: "ok", value: name, detail: "a seeded item" };
  }`,
  pickLink: `(S) => {
    const re = rx(S.href);
    const links = [...document.querySelectorAll("a[href]")].filter(vis).filter((a) => re.test(a.getAttribute("href")));
    if (!links.length) return { status: "not_found", detail: "no link matching /" + S.href + "/" };
    const preferred = S.prefer ? links.find((a) => txt(a.closest("tr, li, article, div") || a).toLowerCase().includes(S.prefer.toLowerCase())) : null;
    return { status: "ok", value: (preferred || links[0]).getAttribute("href"), detail: preferred ? "the created item" : "a seeded item" };
  }`,
  near: `(S) => {
    const hits = smallestWith(S.item);
    if (!hits.length) return { status: "not_found", detail: "no item " + JSON.stringify(S.item) };
    const want = String(S.text).toLowerCase();
    for (const hit of hits) {
      const row = hit.closest("tr, li, article, [role=row]") || cardOf(hit);
      if (txt(row).toLowerCase().includes(want)) return { status: "ok" };
    }
    return { status: "failed", detail: "the row of " + JSON.stringify(S.item) + " does not show " + JSON.stringify(S.text) };
  }`,
  inColumn: `(S) => {
    const column = columnOf(rx(S.to), S.columns);
    if (!column) return { status: "not_found", detail: "no column /" + S.to + "/" };
    return { status: txt(column).toLowerCase().includes(S.item.toLowerCase()) ? "ok" : "failed", detail: "column " + txt(column).slice(0, 20) };
  }`,
  // One move strategy at a time: n is the attempt number.
  move: `(S) => {
    const hits = smallestWith(S.item);
    if (!hits.length) return { status: "not_found", detail: "no item " + JSON.stringify(S.item) };
    const column = columnOf(rx(S.to), S.columns);
    const card = cardOf(hits[0], column);
    const to = rx(S.to);
    const n = S.attempt || 0;
    if (n === 0) {
      const select = [...card.querySelectorAll("select")].find((el) => [...el.options].some((o) => to.test(o.text) || to.test(o.value)));
      if (select) { setValue(select, S.to); return { status: "ok", detail: "status select", more: true }; }
      return { status: "skip" };
    }
    if (n === 1) {
      const button = findControl(to, card);
      if (button) { button.click(); return { status: "ok", detail: "button " + controlName(button).slice(0, 20), more: true }; }
      return { status: "skip" };
    }
    if (n >= 2 && n <= 5) {
      const forward = controls(card).find((el) => /→|›|»|>|right|next|forward|advance|move/i.test(controlName(el)) && !/←|‹|«|<|left|back|prev/i.test(controlName(el)));
      if (forward) { forward.click(); return { status: "ok", detail: "forward control", more: true }; }
      return { status: "skip" };
    }
    if (n === 6 && column) {
      const source = card.closest("[draggable=true]") || card;
      const data = new DataTransfer();
      const fire = (el, type) => el.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: data }));
      fire(source, "dragstart");
      const target = column.querySelector("[class*=drop], ul, ol, [class*=list], [class*=cards]") || column;
      for (const el of [target, column]) { fire(el, "dragenter"); fire(el, "dragover"); }
      fire(target, "drop");
      fire(source, "dragend");
      return { status: "ok", detail: "drag and drop", more: false };
    }
    return { status: "exhausted", detail: column ? "no move control worked" : "no column /" + S.to + "/" };
  }`,
  read: `(S) => {
    const found = numberNear(rx(S.near));
    if (!found) return { status: "not_found", detail: "no label /" + S.near + "/" };
    if (found.value === undefined) return { status: "not_found", detail: "no number next to " + JSON.stringify(found.label) };
    return { status: "ok", value: found.value, detail: found.label + " = " + found.raw };
  }`,
  snapshot: `(S) => {
    if (S.item) {
      const row = rowOf(S.item);
      if (!row) return { status: "not_found", detail: "no item " + JSON.stringify(S.item) };
      return { status: "ok", value: txt(row), page: txt(document.body) };
    }
    if (!S.near) return { status: "ok", value: txt(document.body), page: txt(document.body) };
    const hits = [...document.querySelectorAll("section, table, ul, ol, .card, [class*=card], main")].filter(vis).filter((el) => rx(S.near).test(txt(el)));
    if (!hits.length) return { status: "not_found", detail: "no section /" + S.near + "/" };
    hits.sort((a, b) => txt(a).length - txt(b).length);
    return { status: "ok", value: txt(hits[0]), page: txt(document.body) };
  }`,
  nonzero: `(S) => {
    const main = document.querySelector("main") || document.body;
    const found = numbers(txt(main)).filter((n) => Math.abs(n.value) >= (S.min || 0.01) && /[$€£¥.]/.test(n.raw));
    const bars = [...main.querySelectorAll("svg rect, svg path, svg circle, svg polyline")].filter((el) => {
      const h = Number(el.getAttribute("height") || 0); const d = el.getAttribute("d") || el.getAttribute("points") || "";
      return h > 0 || d.length > 10;
    });
    return { status: found.length || bars.length > 2 ? "ok" : "failed", detail: found.length + " amounts, " + bars.length + " chart marks" };
  }`,
};

// ---- Runner -----------------------------------------------------------------

// The promise, or a rejection after ms.
const timed = (promise, ms, what) => {
  let timer;
  const timeout = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${what}: no answer in ${ms} ms (the page hangs)`)), ms); });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
};

class Page {
  constructor(client, args) {
    this.client = client;
    this.args = args;
    this.exceptions = [];
    this.console = [];
    this.dialogs = [];
    this.docStatus = null;
    this.callMs = Math.max(2000, 2 * Number(args["step-ms"] || 4000));
    client.onEvent((msg) => {
      if (msg.method === "Page.javascriptDialogOpening") {
        // Answer at once: an open dialog blocks the page and every evaluate.
        // alert: OK; confirm: OK (the flow asked for the action); prompt:
        // cancel.
        const { type, message } = msg.params;
        this.dialogs.push({ type, message: String(message || "").slice(0, 200) });
        client.send("Page.handleJavaScriptDialog", { accept: type !== "prompt" }).catch(() => {});
      } else if (msg.method === "Network.responseReceived" && msg.params.type === "Document") {
        this.docStatus = msg.params.response?.status ?? null;
      } else if (msg.method === "Runtime.consoleAPICalled" && msg.params.type === "error") {
        this.console.push((msg.params.args || []).map((a) => a.value ?? a.description ?? "").join(" ").slice(0, 300));
      } else if (msg.method === "Runtime.exceptionThrown") {
        const d = msg.params.exceptionDetails;
        this.exceptions.push(String(d.exception?.description || d.text || "exception").split("\n")[0].slice(0, 300));
      }
    });
  }

  async eval(expression) {
    let res;
    try {
      res = await timed(this.client.send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }), this.callMs, "evaluate");
    } catch (error) {
      // A script that loops forever: stop it so the next step can run.
      await timed(this.client.send("Runtime.terminateExecution", {}), 2000, "terminate").catch(() => {});
      throw error;
    }
    if (res.exceptionDetails) throw new Error(res.exceptionDetails.exception?.description || res.exceptionDetails.text || "eval failed");
    return res.result.value;
  }

  async run(kind, arg) {
    return this.eval(`(() => { ${HELPERS}; return (${PAGE[kind]})(${JSON.stringify(arg)}); })()`);
  }

  async load(url, reload = false) {
    let fired;
    const loaded = new Promise((r) => { fired = r; });
    const detach = this.client.onEvent((msg) => { if (msg.method === "Page.loadEventFired") fired(); });
    const timer = setTimeout(fired, 15000);
    this.docStatus = null;
    try {
      if (reload) await timed(sendRetry(this.client, "Page.reload", {}), 15000, "reload");
      else {
        const nav = await timed(sendRetry(this.client, "Page.navigate", { url }), 15000, "navigate");
        if (nav && nav.errorText) throw new Error(`navigate: ${nav.errorText}`);
      }
      await loaded;
    } finally {
      clearTimeout(timer);
      detach();
    }
    await this.settle();
  }

  // Wait for hydration: the document is complete and the DOM stops changing.
  async settle(ms = 700) {
    const deadline = Date.now() + 3000;
    let last = -1;
    await sleep(250);
    while (Date.now() < deadline) {
      const size = await this.eval("document.readyState === 'complete' ? document.body.innerHTML.length : -2").catch(() => -2);
      if (size === last && size >= 0) break;
      last = size;
      await sleep(ms / 3);
    }
  }

  async waitText(text, ms) {
    const deadline = Date.now() + ms;
    for (;;) {
      const res = await this.run("hasText", { text });
      if (res.status === "ok" || Date.now() > deadline) return res.status;
      await sleep(250);
    }
  }
}

const fill = (value, vars) => {
  if (typeof value === "string") return value.replace(/\{(\w+)\}/g, (all, key) => (key in vars ? String(vars[key]) : all));
  if (Array.isArray(value)) return value.map((item) => fill(item, vars));
  if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, fill(v, vars)]));
  return value;
};

const NOT_FOUND_PAGE = /404|could not be found|not found/i;
// The error pages of Next and of a crashed render: a route that shows one
// does not render.
const ERROR_PAGE = /this page could not be found|application error: a (client|server)-side exception|internal server error|^\s*(404|500)\b/i;
// An alert or a page message that reports an error.
const ERR = /\b(error|could not|cannot|must be|invalid|failed|bad|wrong|required|not allowed)\b[^.]{0,80}/i;

// The step's kind: its first key other than `optional`.
const stepKind = (step) => Object.keys(step).find((key) => key !== "optional");

async function runStep(page, step, vars, ctx) {
  const kind = stepKind(step);
  const raw = step[kind];
  const arg = fill(raw, vars);
  const base = ctx.url;
  switch (kind) {
    case "goto": {
      const options = Array.isArray(arg) ? arg : [arg];
      const route = options.find((path) => ctx.routes.includes(path)) || options[0];
      const known = ctx.routes.includes(route);
      await page.load(new URL(route, base).href);
      if (!known) {
        const text = await page.eval("document.body ? document.body.innerText.slice(0, 400) : ''");
        if (NOT_FOUND_PAGE.test(text)) return { status: "not_found", detail: `no page ${route}` };
      }
      return { status: "ok", detail: route };
    }
    case "reload":
      await page.load(null, true);
      return { status: "ok" };
    case "click": {
      const res = await page.run("click", typeof arg === "string" ? { control: arg } : arg);
      await page.settle();
      return res;
    }
    case "form": {
      let res = await page.run("form", arg);
      if (res.status === "not_found" && res.retry && arg.open) {
        const opened = await page.run("click", { control: arg.open, optional: true });
        await page.settle();
        res = await page.run("form", arg);
        if (res.status === "ok") res.detail = `opened ${opened.detail || ""}; ${res.detail}`;
      }
      if (res.status !== "ok") return res;
      await sleep(250);
      const pressed = await page.run("press", {});
      await page.settle();
      return pressed.status === "ok" ? res : pressed;
    }
    case "expectText": {
      const status = await page.waitText(arg, ctx.stepMs);
      return { status, detail: status === "ok" ? null : `no text ${JSON.stringify(arg)}` };
    }
    case "clickText": {
      const res = await page.run("clickText", typeof arg === "string" ? { text: arg } : arg);
      await page.settle();
      return res;
    }
    case "pickItem":
    case "pickLink": {
      const res = await page.run(kind, arg);
      if (res.status === "ok") {
        vars[arg.as] = res.value;
        for (const [key, value] of Object.entries(res.store || {})) if (value !== null && value !== undefined) vars[key] = value;
        if (arg.amountAs && !(arg.amountAs in vars)) return { status: "not_found", detail: `no amount on the row of ${JSON.stringify(res.value)}` };
      }
      return res;
    }
    case "move": {
      const target = { item: arg.item, to: arg.to, columns: arg.columns };
      if (arg.verify) {
        const res = await page.run("inColumn", target);
        return res;
      }
      const tried = [];
      for (let attempt = 0; attempt <= 7; attempt += 1) {
        const res = await page.run("move", { ...target, attempt });
        if (res.status === "not_found") return res;
        if (res.status === "exhausted") break;
        if (res.status !== "ok") continue;
        await page.settle(300);
        const check = await page.run("inColumn", target);
        tried.push(`${res.detail}: ${check.status}`);
        if (check.status === "ok") return { status: "ok", detail: tried.join("; ") };
        if (check.status === "not_found" && res.detail === "status select") return { status: "ok", detail: "status select (no column heading to check)" };
      }
      return tried.length ? { status: "failed", detail: `tried ${tried.join("; ")}; the item is not in /${arg.to}/` } : { status: "not_found", detail: "no move control on the item's card" };
    }
    case "read": {
      const res = await page.run("read", arg);
      if (res.status === "ok") vars[arg.as] = res.value;
      return res;
    }
    case "expect": {
      const deadline = Date.now() + ctx.stepMs;
      let res;
      // A reference that no earlier step read: the comparison would be
      // against a guess (0 for a count that seeded data already exceeds).
      if (arg.ref !== undefined && (vars[arg.ref] === undefined || Number.isNaN(Number(vars[arg.ref])))) {
        return { status: "failed", detail: `no earlier reading ${JSON.stringify(arg.ref)} to compare with` };
      }
      let by = arg.by;
      if (arg.delta) {
        const from = Number(vars[arg.delta.from]);
        if (Number.isNaN(from) || vars[arg.delta.from] === undefined) return { status: "failed", detail: `no earlier reading ${JSON.stringify(arg.delta.from)}` };
        by = Math.round((Number(arg.delta.to) - from) * 100) / 100;
      }
      for (;;) {
        res = await page.run("read", arg);
        if (res.status !== "ok") return res;
        const value = res.value;
        const ref = arg.ref !== undefined ? Number(vars[arg.ref]) : undefined;
        let ok;
        if (by !== undefined) ok = ref !== undefined && Math.abs(value - ref - Number(by)) < 0.011;
        else {
          const against = ref !== undefined && !Number.isNaN(ref) ? ref : Number(arg.value || 0);
          ok = arg.op === "<" ? value < against : arg.op === "==" ? value === against : value > against;
        }
        if (ok) return { status: "ok", detail: res.detail };
        if (Date.now() > deadline) {
          return { status: "failed", detail: `${res.detail}; expected ${by !== undefined ? `a change of ${by} from ${ref}` : `${arg.op || ">"} ${ref ?? arg.value ?? 0}`}` };
        }
        await sleep(300);
      }
    }
    case "snapshot": {
      const res = await page.run("snapshot", arg);
      if (res.status === "ok") {
        vars[arg.as] = res.value;
        vars[`${arg.as}@page`] = res.page;
      }
      return { status: res.status, detail: res.detail };
    }
    case "changed": {
      // The item's row (or the section, or the page) must differ from the
      // snapshot, and no new error may show anywhere on the page.
      const res = await page.run("snapshot", arg);
      if (res.status !== "ok") return { status: "failed", detail: res.detail || "the snapshot's item went away" };
      if (res.value === vars[arg.ref]) return { status: "failed", detail: arg.item ? `the row of ${JSON.stringify(arg.item)} did not change` : "the page did not change" };
      const before = String(vars[`${arg.ref}@page`] ?? vars[arg.ref] ?? "");
      const shown = String(res.page ?? res.value).match(ERR);
      if (arg.noError && shown && !before.includes(shown[0])) return { status: "failed", detail: `the page shows an error: ${shown[0]}` };
      return { status: "ok", detail: String(res.value).slice(0, 80) };
    }
    case "expectNear": {
      const deadline = Date.now() + ctx.stepMs;
      for (;;) {
        const res = await page.run("near", arg);
        if (res.status !== "failed" || Date.now() > deadline) return res;
        await sleep(250);
      }
    }
    case "nonzero":
      return page.run("nonzero", arg);
    case "debug": {
      // For work on the heuristics: an expression over the helpers.
      const value = await page.eval(`(() => { ${HELPERS}; return JSON.stringify(${arg}); })()`);
      return { status: "ok", detail: value };
    }
    default:
      return { status: "failed", detail: `unknown step kind ${kind}` };
  }
}

async function main() {
  const args = parseArgs(process.argv);
  const report = { browser: null, url: args.url, routes: [], flows: [], console_errors: 0, exceptions: 0, ms: 0, budget_exhausted: false };
  const started = Date.now();
  const write = () => {
    report.ms = Date.now() - started;
    try { writeFileSync(args.out, JSON.stringify(report, null, 2)); } catch { /* the next write */ }
  };
  if (!args.url || !args.flows) {
    report.error = "usage: --url URL --flows FILE --out FILE";
    write();
    process.exit(2);
  }
  let chrome;
  try {
    chrome = findChrome();
  } catch (error) {
    report.error = String(error.message || error);
    write();
    process.exit(2);
  }
  report.browser = chrome;
  const spec = JSON.parse(readFileSync(args.flows, "utf8").replace(/^﻿/, ""));
  const flows = Array.isArray(spec) ? spec : spec.flows || [];
  const routes = String(args.routes).split(",").filter(Boolean);
  const profile = mkdtempSync(join(tmpdir(), "qa-judge-smoke-"));
  const proc = spawn(chrome, [
    "--headless=new", `--remote-debugging-port=${args["cdp-port"]}`, `--user-data-dir=${profile}`,
    "--no-first-run", "--no-default-browser-check", "--disable-extensions", "--window-size=1280,900", "about:blank",
  ], { stdio: "ignore" });
  const deadline = started + args["budget-ms"];
  const ctx = { url: args.url, routes, stepMs: args["step-ms"] };
  // The budget's last resort: write what there is and stop (a page that
  // hangs the browser must not hang the judge).
  const watchdog = setTimeout(() => {
    report.budget_exhausted = true;
    report.watchdog = true;
    write();
    try { proc.kill("SIGKILL"); } catch { /* gone */ }
    try { rmSync(profile, { recursive: true, force: true }); } catch { /* still locked */ }
    process.exit(0);
  }, args["budget-ms"] + 10000);
  try {
    await waitForDebugger(args["cdp-port"], 20000);
    const target = await newPageTarget(args["cdp-port"]);
    const client = new CdpClient(target.webSocketDebuggerUrl);
    await sendRetry(client, "Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
    const page = new Page(client, args);
    await sendRetry(client, "Page.enable");
    await sendRetry(client, "Runtime.enable");
    await sendRetry(client, "Network.enable");
    // A hydration mismatch (React #418/#423/#425) is recovered by React,
    // which renders the client tree: it is reported but fails nothing.
    const fatal = (list) => list.filter((e) => !/react\.dev\/errors\/(418|423|425)\b|hydrat|did not match/i.test(String(e)));
    // Routes: each must answer 2xx/3xx, render text that is not an error
    // page, and throw no uncaught exception. Routes the budget cuts off fail.
    for (const route of routes) {
      if (Date.now() > deadline) {
        report.budget_exhausted = true;
        report.routes.push({ route, ok: false, ms: 0, chars: 0, status: null, error: "not visited: the time budget ran out", exceptions: [], console: [] });
        continue;
      }
      const [e0, c0, t0] = [page.exceptions.length, page.console.length, Date.now()];
      let error = null;
      let chars = 0;
      try {
        await page.load(new URL(route, args.url).href);
        const head = await page.eval("document.body ? document.body.innerText.trim().slice(0, 300) : ''");
        chars = await page.eval("document.body ? document.body.innerText.trim().length : 0");
        if (page.docStatus !== null && page.docStatus >= 400) error = `HTTP ${page.docStatus}`;
        else if (ERROR_PAGE.test(head)) error = `an error page: ${head.slice(0, 80)}`;
      } catch (err) {
        error = String(err.message || err);
      }
      const exceptions = page.exceptions.slice(e0);
      report.routes.push({ route, ok: !error && fatal(exceptions).length === 0 && chars > 0, ms: Date.now() - t0, chars, status: page.docStatus, error, exceptions, console: page.console.slice(c0) });
      write();
    }
    // Flows: share the browser's storage, in order; `{n}` is one nonce.
    const vars = { n: String(Date.now() % 100000), today: new Date().toISOString().slice(0, 10) };
    for (const flow of flows) {
      const entry = { id: flow.id, reqs: flow.reqs || [], status: "passed", steps: [], ms: 0 };
      const t0 = Date.now();
      const [e0, c0, d0] = [page.exceptions.length, page.console.length, page.dialogs.length];
      if (Date.now() > deadline) {
        entry.status = "skipped";
        entry.detail = "the time budget ran out";
        report.budget_exhausted = true;
        report.flows.push(entry);
        continue;
      }
      for (const [index, step] of (flow.steps || []).entries()) {
        const kind = stepKind(step);
        const s0 = Date.now();
        let res;
        if (Date.now() > deadline) {
          res = { status: "failed", detail: "the time budget ran out" };
          report.budget_exhausted = true;
        } else {
          try {
            res = await runStep(page, step, vars, ctx);
          } catch (err) {
            res = { status: "failed", detail: String(err.message || err).split("\n")[0] };
          }
        }
        entry.steps.push({ step: index + 1, kind, status: res.status, detail: res.detail ?? null, ms: Date.now() - s0 });
        // An optional step (a reading the flow can do without) never stops it.
        if (res.status !== "ok" && step.optional) continue;
        if (res.status !== "ok") {
          entry.status = res.status === "not_found" ? "not_found" : "failed";
          entry.failed_step = index + 1;
          entry.detail = `${kind}: ${res.detail ?? res.status}`;
          break;
        }
      }
      entry.exceptions = page.exceptions.slice(e0);
      entry.console = page.console.slice(c0);
      entry.dialogs = page.dialogs.slice(d0);
      const alerted = entry.dialogs.find((d) => d.type === "alert" && ERR.test(d.message));
      if (entry.status === "passed" && fatal(entry.exceptions).length > 0) {
        entry.status = "failed";
        entry.detail = `uncaught exception: ${fatal(entry.exceptions)[0]}`;
      } else if (entry.status === "passed" && alerted) {
        entry.status = "failed";
        entry.detail = `the page showed an alert: ${alerted.message}`;
      }
      entry.ms = Date.now() - t0;
      report.flows.push(entry);
      write();
    }
    report.console_errors = page.console.length;
    report.exceptions = page.exceptions.length;
    report.dialogs = page.dialogs.length;
    client.close();
  } catch (error) {
    report.error = String(error.message || error);
  } finally {
    await sleep(300);
    proc.kill("SIGKILL");
    await sleep(300);
    try { rmSync(profile, { recursive: true, force: true }); } catch { /* still locked */ }
  }
  clearTimeout(watchdog);
  write();
  process.exit(0);
}

main().catch((error) => {
  console.error(`qa-judge-smoke: ${error.message || error}`);
  process.exit(2);
});
