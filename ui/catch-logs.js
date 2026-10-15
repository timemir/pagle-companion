// The window's Catch logs list (CMP-R41, CMP UI): one row per logId with its
// characters, account folder, the Upload switch, Change folder (CMP-R42) and
// its state line: CMP-R40's question, CMP-R42's question, "Not found on this
// computer", or CMP-R30's or CMP-R31's text; or the last run's result, which
// goes in the row's `.run-line`: "Uploaded 5 minutes ago" or "Up to date"
// with CMP-R53's lines under it, a divergence with Replace and its question
// (CMP-R52), deleted (CMP-R54), log cap, failed and the rest. Every text
// comes from the Companion.
(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;

  const section = document.getElementById("catch-logs-section");
  const list = document.getElementById("catch-logs");
  const error = document.getElementById("catch-logs-error");

  // The state lines that are the last run's result (CMP UI, Catch logs).
  const RUN_KINDS = new Set([
    "uploaded", "up-to-date", "divergence", "stale", "deleted", "dev-log",
    "waiting-for-pagle", "log-cap", "too-large", "failed",
  ]);
  // The lines that carry a link, its text and the command that opens it.
  const LINKS = {
    deleted: ["My Anglers", "open_my_anglers"],
    "log-cap": ["My Anglers", "open_my_anglers"],
    "left-off": ["My Anglers", "open_my_anglers"],
    "held-characters": ["Sign in", "open_sign_in"],
  };

  let view = null;
  let readOnly = false;
  // logIds whose Change folder question is open.
  const changing = new Set();
  // logIds switched off in this window, which say CMP-R41's text.
  const switchedOff = new Set();
  // logIds whose Replace question is open (CMP-R52).
  const replacing = new Set();

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function button(text, onClick, className) {
    const node = element("button", className, text);
    node.type = "button";
    node.disabled = readOnly;
    node.addEventListener("click", onClick);
    return node;
  }

  function showError(message) {
    error.textContent = String(message);
    error.hidden = false;
  }

  // `fromSwitch`: the Upload switch, whose turning off says CMP-R41's text;
  // otherwise the question's buttons.
  function answer(logId, upload, fromSwitch = false) {
    error.hidden = true;
    if (upload) switchedOff.delete(logId);
    else if (fromSwitch) switchedOff.add(logId);
    invoke("answer_catch_log", { logId, upload }).catch(showError);
  }

  // A link that opens a Pagle page in the browser: the webview never
  // navigates away.
  function pageLink(text, command) {
    const link = element("a", "", text);
    link.href = "#";
    link.addEventListener("click", (event) => {
      event.preventDefault();
      invoke(command);
    });
    return link;
  }

  // A line of the last run, with its link when it has one.
  function runText(kind, text, className) {
    const node = element("p", className, text);
    const link = LINKS[kind];
    if (link) node.append(" ", pageLink(...link));
    return node;
  }

  function replace(logId) {
    error.hidden = true;
    replacing.delete(logId);
    invoke("replace_catch_log", { logId }).catch(showError);
  }

  // The last run's result, then CMP-R53's lines in their order.
  function runLine(row) {
    const block = element("div", "run-line");
    const line = row.line;
    if (!line || !RUN_KINDS.has(line.kind)) return block;
    block.append(runText(line.kind, line.text, line.kind));
    for (const note of line.notes ?? []) {
      block.append(runText(note.kind, note.text, `note ${note.kind}`));
    }
    if (line.kind === "divergence") {
      if (replacing.has(row.logId)) {
        // CMP-R52: asks first, with Replace and Cancel.
        const ask = element("div", "confirm");
        ask.append(element("p", "", line.question));
        const actions = element("p", "actions");
        actions.append(
          button("Replace", () => replace(row.logId)),
          button("Cancel", () => {
            replacing.delete(row.logId);
            render();
          }),
        );
        ask.append(actions);
        block.append(ask);
      } else {
        const actions = element("p", "actions");
        actions.append(button("Replace Pagle's copy with this computer's", () => {
          replacing.add(row.logId);
          render();
        }));
        block.append(actions);
      }
    }
    return block;
  }

  function pick(logId, folder) {
    error.hidden = true;
    changing.delete(logId);
    invoke("pick_folder", { logId, folder }).catch(showError);
  }

  // CMP-R42's question: one button per folder.
  function pickButtons(row) {
    const actions = element("p", "actions");
    for (const choice of row.choices) {
      actions.append(button(choice.label, () => pick(row.logId, choice.path)));
    }
    return actions;
  }

  function stateLine(row) {
    const line = row.line;
    if (!line || RUN_KINDS.has(line.kind)) return null;
    const block = element("div", `state-line ${line.kind}`);
    block.append(element("p", "", line.text));
    if (line.kind === "awaiting") {
      const actions = element("p", "actions");
      actions.append(
        button("Upload", () => answer(row.logId, true)),
        button("Don't upload", () => answer(row.logId, false)),
      );
      block.append(actions);
    } else if (line.kind === "several-folders") {
      const actions = pickButtons(row);
      // CMP-R40: a logId answered Don't upload needs no folder pick.
      if (line.dontUpload) actions.append(button("Don't upload", () => answer(row.logId, false)));
      block.append(actions);
    }
    return block;
  }

  function uploadSwitch(row) {
    const label = element("label", "switch");
    const input = document.createElement("input");
    input.type = "checkbox";
    input.setAttribute("role", "switch");
    input.checked = row.upload;
    input.disabled = readOnly;
    input.addEventListener("change", () => answer(row.logId, input.checked, true));
    label.append(input, element("span", "", "Upload"));
    return label;
  }

  function rowItem(row) {
    const item = element("li", "log");
    const head = element("div", "log-head");
    head.append(element("span", "characters", row.characters), uploadSwitch(row));
    item.append(head);

    if (row.folder) {
      const folder = element("div", "log-folder muted", `Account folder ${row.folder}`);
      if (row.changeFolder) {
        folder.append(
          " · ",
          button("Change folder", () => {
            changing.add(row.logId);
            render();
          }, "link"),
        );
      }
      item.append(folder);
    }

    // Change folder asks CMP-R42's question again, here in the row.
    if (row.changeFolder && changing.has(row.logId)) {
      const block = element("div", "state-line several-folders");
      block.append(element("p", "", row.pickText));
      const actions = pickButtons(row);
      actions.append(button("Cancel", () => {
        changing.delete(row.logId);
        render();
      }));
      block.append(actions);
      item.append(block);
    }

    const line = stateLine(row);
    if (line) item.append(line);
    // The last run's result (uploaded, up to date, …) shows here.
    item.append(runLine(row));

    if (!row.upload && switchedOff.has(row.logId)) {
      const note = element("p", "notice", `${view.switchedOff} `);
      note.append(pageLink("My Anglers", "open_my_anglers"));
      item.append(note);
    }
    return item;
  }

  function render() {
    list.replaceChildren();
    const logs = view ? view.logs : [];
    section.hidden = logs.length === 0;
    for (const row of logs) list.append(rowItem(row));
  }

  function setView(next) {
    view = next;
    render();
  }

  invoke("catch_logs").then(setView);
  listen("catch-logs", (event) => setView(event.payload));
  invoke("status").then((status) => {
    readOnly = status.readOnly;
    render();
  });
  listen("status", (event) => {
    readOnly = event.payload.readOnly;
    render();
  });
})();
