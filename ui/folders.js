// The window's WoW folders section (CMP UI): each install with its account
// folders, "Add a WoW folder…" and Remove (CMP-R13), and each account
// folder's read problem (CMP-R30, CMP-R31) and polling note (CMP-R22).
(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;

  const list = document.getElementById("installs");
  const noInstall = document.getElementById("no-install");
  const error = document.getElementById("folder-error");
  const add = document.getElementById("add-folder");

  let view = null;
  let readings = null;
  let readOnly = false;

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function split(path) {
    const i = path.lastIndexOf("/");
    return i > 0 ? [path.slice(0, i), path.slice(i + 1)] : [path, path];
  }

  // "Checked every {poll interval}" (CMP-R22).
  function pollText(seconds) {
    if (seconds % 60 === 0) {
      const minutes = seconds / 60;
      return `Checked every ${minutes === 1 ? "minute" : `${minutes} minutes`}`;
    }
    return `Checked every ${seconds} seconds`;
  }

  function removeButton(flavor) {
    const button = element("button", "link", "Remove");
    button.type = "button";
    button.disabled = readOnly;
    button.addEventListener("click", () => {
      button.disabled = true;
      invoke("remove_wow_folder", { flavor }).catch(showError);
    });
    return button;
  }

  function folderLines(item, flavorPath, label, removable) {
    const [root, flavor] = split(flavorPath);
    item.append(element("div", "install-root", root));
    const line = element("div", "install-flavor");
    line.append(element("span", "", flavor), element("span", "muted", ` · ${label}`));
    if (removable) line.append(" ", removeButton(flavorPath));
    item.append(line);
  }

  // An account folder, with "Checked every …" where its SavedVariables/ is
  // polled (CMP-R22) and CMP-R30's or CMP-R31's text where it has one.
  function accountRow(account) {
    const item = element("li", "", account.name);
    const folder = readings && readings.folders.find((f) => f.path === account.path);
    if (folder && folder.polled) {
      item.append(element("div", "muted", pollText(readings.pollSeconds)));
    }
    if (folder && folder.problem) {
      item.append(element("div", "read-problem", folder.problem));
    }
    return item;
  }

  function installRow(install) {
    const item = element("li", "install");
    const manual = install.source === "manual";
    folderLines(item, install.flavor, manual ? "added by you" : "found by Battle.net", manual);
    if (install.accounts.length > 0) {
      const accounts = element("ul", "accounts");
      for (const account of install.accounts) {
        accounts.append(accountRow(account));
      }
      item.append(accounts);
    }
    if (install.accountsPolled) {
      item.append(element("p", "muted", pollText(view.pollSeconds)));
    }
    return item;
  }

  // A manual install that no longer qualifies: greyed, "Not found" (CMP-R13).
  function notFoundRow(flavorPath) {
    const item = element("li", "install not-found");
    folderLines(item, flavorPath, "Not found", true);
    return item;
  }

  function render() {
    const snapshot = view && view.snapshot;
    list.replaceChildren();
    // CMP-R131: with no install, the no-install text and Add a WoW folder….
    noInstall.hidden = !snapshot || snapshot.installs.length > 0;
    if (snapshot) {
      for (const install of snapshot.installs) list.append(installRow(install));
      for (const path of snapshot.notFound) list.append(notFoundRow(path));
    }
    // CMP-R2: nothing changes from a read-only window.
    add.disabled = readOnly || !snapshot;
  }

  function showError(message) {
    error.textContent = String(message);
    error.hidden = false;
  }

  add.addEventListener("click", () => {
    error.hidden = true;
    add.disabled = true;
    invoke("add_wow_folder")
      .catch(showError)
      .finally(render);
  });

  function setView(next) {
    view = next;
    render();
  }

  function setReadings(next) {
    readings = next;
    render();
  }

  invoke("installs").then(setView);
  invoke("readings").then(setReadings);
  listen("readings", (event) => setReadings(event.payload));
  listen("installs", (event) => setView(event.payload));
  invoke("status").then((status) => {
    readOnly = status.readOnly;
    render();
  });
  listen("status", (event) => {
    readOnly = event.payload.readOnly;
    render();
  });
})();
