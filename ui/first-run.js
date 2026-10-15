// The first-run window (CMP-R100, CMP UI): 1. the found list with ticks, the
// CMP-R42 pick in the list, the no-install state with Add a WoW folder…,
// Link to Pagle (or Continue) and Not now; 2. the linking part, which is
// linking.js's; 3. "Uploading…"; 4. the status and Close. While it shows,
// the everyday parts of the window are hidden. Every text but the headings
// and buttons comes from the Companion.
(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const $ = (id) => document.getElementById(id);

  const HEADINGS = {
    scan: "Your Catch logs",
    link: "Link to Pagle",
    uploading: "Uploading",
    done: "All set",
  };

  let view = null;
  let readOnly = false;
  let linking = { phase: "idle" };
  let busy = false;

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function button(text, onClick) {
    const node = element("button", "", text);
    node.type = "button";
    node.disabled = readOnly || busy;
    node.addEventListener("click", onClick);
    return node;
  }

  function showError(message) {
    $("fr-error").textContent = String(message);
    $("fr-error").hidden = false;
  }

  // Runs a command; its error is the text to show.
  function act(command, args) {
    $("fr-error").hidden = true;
    busy = true;
    render();
    return invoke(command, args)
      .catch(showError)
      .finally(() => {
        busy = false;
        refresh();
      });
  }

  function rowItem(row) {
    const item = element("li", "log");
    const label = element("label", "tick");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = row.ticked;
    // CMP-R100: a logId in two folders can't be ticked until a folder is
    // picked.
    box.disabled = readOnly || !row.canTick;
    box.addEventListener("change", () =>
      act("tick_catch_log", { logId: row.logId, ticked: box.checked }));
    label.append(box, element("span", "characters", row.characters));
    item.append(label);
    if (row.folder) {
      item.append(element("div", "log-folder muted", `Account folder ${row.folder}`));
    }
    const line = row.line;
    if (line && line.kind === "several-folders") {
      // CMP-R42's question, part of the list.
      const block = element("div", "state-line several-folders");
      block.append(element("p", "", line.text));
      const actions = element("p", "actions");
      for (const choice of row.choices) {
        actions.append(button(choice.label, () =>
          act("pick_folder", { logId: row.logId, folder: choice.path })));
      }
      block.append(actions);
      item.append(block);
    } else if (line && (line.kind === "not-found" || line.kind === "problem")) {
      item.append(element("p", "read-problem", line.text));
    }
    return item;
  }

  function renderScan() {
    const logs = view.logs;
    $("fr-reading").hidden = view.read || view.noInstall;
    $("fr-no-install").hidden = !view.noInstall;
    $("fr-add-folder").disabled = readOnly || busy;
    $("fr-none").hidden = !view.read || view.noInstall || logs.length > 0;
    $("fr-intro").hidden = logs.length === 0;
    $("fr-logs").replaceChildren(...logs.map(rowItem));
    $("fr-primary").textContent = view.primary;
  }

  function renderStatus() {
    $("fr-status").replaceChildren(...view.status.map((line) => element("p", "", line)));
  }

  function render() {
    const open = view && view.step !== "over";
    document.body.classList.toggle("first-run", Boolean(open));
    $("first-run").hidden = !open;
    if (!open) return;
    const step = view.step;
    document.body.classList.toggle("first-run-link", step === "link");
    $("first-run-heading").textContent = HEADINGS[step];
    $("fr-scan").hidden = step !== "scan";
    $("fr-uploading").hidden = step !== "uploading";
    $("fr-done").hidden = step !== "done";
    // Step 2: linking.js shows the code; here, linking again or not now
    // once an attempt has failed or was cancelled.
    const underWay = linking.phase === "starting" || linking.phase === "waiting";
    $("fr-link-actions").hidden = step !== "link" || underWay;
    for (const id of ["fr-primary", "fr-not-now", "fr-link-again", "fr-link-not-now", "fr-close"]) {
      $(id).disabled = readOnly || busy;
    }
    if (step === "scan") renderScan();
    if (step === "done") renderStatus();
  }

  function setView(next) {
    view = next;
    render();
  }

  function refresh() {
    invoke("first_run").then(setView);
  }

  $("fr-primary").addEventListener("click", () => act("first_run_link"));
  $("fr-not-now").addEventListener("click", () => act("first_run_not_now"));
  $("fr-link-again").addEventListener("click", () => act("first_run_link"));
  $("fr-link-not-now").addEventListener("click", () => act("first_run_not_now"));
  $("fr-close").addEventListener("click", () => act("first_run_close"));
  $("fr-add-folder").addEventListener("click", () => act("add_wow_folder"));

  listen("first-run", (event) => setView(event.payload));
  // The list, the installs and the status follow the rest of the window.
  for (const name of ["catch-logs", "installs", "link-status", "last-uploaded"]) {
    listen(name, refresh);
  }
  listen("linking", (event) => {
    linking = event.payload;
    render();
  });
  listen("status", (event) => {
    readOnly = event.payload.readOnly;
    render();
  });
  invoke("status").then((status) => {
    readOnly = status.readOnly;
    render();
  });
  invoke("linking_view").then((next) => {
    linking = next;
    render();
  });
  refresh();
})();
