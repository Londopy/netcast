"use strict";

const token = document.querySelector('meta[name="netcast-token"]').content;
const $ = (id) => document.getElementById(id);
let rosterShown = false;
let outboxKey = "";

async function api(path, body) {
  const res = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    headers: { "X-Netcast-Token": token, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw Object.assign(new Error(data.error || `netcast answered ${res.status}`), { status: res.status });
  return data;
}

function el(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === "class") e.className = v;
    else e.setAttribute(k, v);
  }
  for (const k of kids) if (k !== null && k !== undefined) e.append(k);
  return e;
}

function stateClass(s) {
  return "state-" + s.split(" ")[0];
}

function trustClass(t) {
  if (t === "verified") return "trust-verified";
  if (t === "unsigned") return "trust-unsigned";
  return "trust-unknown";
}

function showRoster(s) {
  if (rosterShown) return;
  rosterShown = true;
  const box = $("members");
  if (!s.roster) {
    $("to-hint").textContent = "No roster loaded, so type the callsigns.";
    return;
  }
  const add = (name, group) => {
    const id = "to-" + name;
    box.append(
      el("label", { class: group ? "pick group" : "pick", for: id },
        el("input", { type: "checkbox", id, value: name }),
        el("span", {}, name)));
  };
  s.roster.members.filter((m) => m !== s.call).forEach((m) => add(m, false));
  s.roster.groups.forEach((g) => add(g, true));
  $("to-hint").textContent = "Pick none to send to every member of the roster.";
}

function showOutbox(files) {
  const key = JSON.stringify(files);
  if (key === outboxKey) return;
  outboxKey = key;
  $("source").hidden = files === null;
  if (files === null) return;
  const sel = $("file");
  const keep = sel.value;
  sel.replaceChildren();
  const usable = files.filter((f) => !f.problem);
  for (const f of usable) {
    sel.append(el("option", { value: f.file }, `${f.file}: ${f.subject || f.mid}`));
  }
  if (usable.some((f) => f.file === keep)) sel.value = keep;
  const bad = files.filter((f) => f.problem);
  $("file-hint").textContent =
    (usable.length === 0 ? "No messages in the outbox yet. " : "") +
    bad.map((f) => `${f.file} can't go: ${f.problem}.`).join(" ");
}

function showSigning(canSign) {
  const sign = $("sign");
  sign.disabled = !canSign;
  if (!canSign) sign.checked = false;
  $("sign-hint").textContent = canSign ? "" : "No station key yet. Run `netcast key` to make one.";
}

function showAir(snd) {
  const air = $("air");
  if (!snd) {
    air.replaceChildren(el("p", { class: "empty" }, "Nothing sent yet."));
    return;
  }
  const bits = [snd.mode];
  if (snd.signed) bits.push("signed");
  if (snd.k) bits.push(`${snd.k} symbols`);
  if (snd.round) bits.push(`status round ${snd.round}`);
  const phase = { sending: "on the air", done: "finished", failed: "failed" }[snd.phase];
  const head = el("p", { class: "headline" },
    el("strong", {}, snd.mid || "preparing"), " ",
    el("span", { class: "meta" }, `${bits.join(", ")}, ${phase}`));
  const rows = snd.recipients.map(([call, st]) =>
    el("tr", {}, el("td", {}, call), el("td", { class: stateClass(st) }, st)));
  const table = el("table", {},
    el("thead", {}, el("tr", {}, el("th", { scope: "col" }, "Station"), el("th", { scope: "col" }, "State"))),
    el("tbody", {}, ...rows));
  const parts = [head, table];
  if (snd.error) parts.push(el("p", { class: "note" }, snd.error));
  air.replaceChildren(...parts);
}

function showIncoming(list, net) {
  $("incoming-empty").textContent = `Nothing received yet. Listening on ${net}.`;
  $("incoming-empty").hidden = list.length > 0;
  $("incoming").replaceChildren(...list.map((m) => {
    const top = el("div", { class: "msg-top" },
      el("span", { class: "from" }, m.from),
      el("span", {}, m.mid),
      el("span", { class: "meta" }, m.mode),
      el("span", { class: trustClass(m.trust) }, m.trust));
    const parts = [top];
    if (m.subject) parts.push(el("p", { class: "subject" }, m.subject));
    if (m.status === "receiving") {
      parts.push(el("div", { class: "bar" },
        el("progress", { max: String(m.k), value: String(m.have), "aria-label": `${m.mid} received` }),
        el("span", { class: "meta" }, `${m.have} of ${m.k}`)));
    } else if (m.status === "decoded") {
      parts.push(el("p", { class: "subject state-decoded" }, "in the Pat inbox"));
    }
    if (m.note) parts.push(el("p", { class: "note" }, m.note));
    return el("li", {}, ...parts);
  }));
}

function showLog(lines) {
  const log = $("log");
  const atEnd = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
  const text = lines.join("\n");
  if (log.textContent !== text) {
    log.textContent = text || "Nothing yet.";
    if (atEnd) log.scrollTop = log.scrollHeight;
  }
}

function render(s) {
  $("lost").hidden = true;
  $("who").textContent = `${s.call} on ${s.net}`;
  const tnc = $("tnc");
  tnc.textContent = s.board.tnc;
  tnc.classList.toggle("bad", !s.board.tnc.startsWith("connected"));
  showRoster(s);
  showOutbox(s.outbox);
  showSigning(s.can_sign);
  showAir(s.board.sending);
  showIncoming(s.board.incoming, s.net);
  showLog(s.board.log);
  const busy = s.board.sending && s.board.sending.phase === "sending";
  $("go").disabled = busy;
  $("go").textContent = busy ? "On the air..." : "Send to the net";
}

async function poll() {
  try {
    render(await api("/api/state"));
  } catch (e) {
    $("lost").textContent = e.status === 403
      ? "This page is from an earlier run of netcast. Reload it."
      : "Lost contact with netcast. Is it still running?";
    $("lost").hidden = false;
  }
  setTimeout(poll, 1000);
}

function syncMode() {
  const mode = $("mode").value;
  $("ack-wrap").hidden = mode !== "emergency";
  if (mode !== "emergency") $("ack").checked = false;
  $("repeat-wrap").hidden = mode === "addressed" || $("ack").checked;
}

function syncSource() {
  const outbox = document.querySelector('input[name="source"]:checked').value === "outbox";
  $("compose").hidden = outbox;
  $("pick").hidden = !outbox;
}

async function send(ev) {
  ev.preventDefault();
  const picked = [...document.querySelectorAll("#members input:checked")].map((i) => i.value);
  const typed = $("extra").value.split(/[\s,]+/).filter(Boolean);
  const outbox = !$("pick").hidden;
  const form = {
    to: [...picked, ...typed],
    mode: $("mode").value,
    ack: $("ack").checked,
    repeat: Number($("repeat").value) || 0,
    sign: $("sign").checked,
  };
  if (outbox) {
    form.file = $("file").value;
  } else {
    form.subject = $("subject").value;
    form.body = $("body").value;
  }
  const err = $("form-error");
  err.textContent = "";
  if (outbox && !form.file) {
    err.textContent = "Pick a message from the outbox.";
    return;
  }
  if (!outbox && !form.subject.trim()) {
    err.textContent = "The message needs a subject.";
    $("subject").focus();
    return;
  }
  $("go").disabled = true;
  try {
    await api("/api/send", form);
    if (!outbox) {
      $("subject").value = "";
      $("body").value = "";
    }
  } catch (e) {
    err.textContent = e.message;
    $("go").disabled = false;
  }
}

$("form").addEventListener("submit", send);
$("mode").addEventListener("change", syncMode);
$("ack").addEventListener("change", syncMode);
for (const r of document.querySelectorAll('input[name="source"]')) r.addEventListener("change", syncSource);
syncMode();
poll();
