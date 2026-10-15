// The window's Settings (CMP UI): "Start at login" (CMP-R3), on by default.
// Turning it off deletes the autostart entry; the Companion keeps the setting
// and says when it couldn't change the entry. On Linux, Remove Pagle
// Companion (CMP-R71).
(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;

  const toggle = document.getElementById("start-at-login");
  const error = document.getElementById("settings-error");

  let readOnly = true;
  let known = false;
  let busy = false;

  function render() {
    // CMP-R2: nothing changes from a read-only window.
    toggle.disabled = readOnly || !known || busy;
  }

  toggle.addEventListener("change", () => {
    const on = toggle.checked;
    busy = true;
    error.hidden = true;
    render();
    invoke("set_start_at_login", { on })
      .then((now) => {
        toggle.checked = now;
      })
      .catch((message) => {
        toggle.checked = !on;
        error.textContent = String(message);
        error.hidden = false;
      })
      .finally(() => {
        busy = false;
        render();
      });
  });

  // On Linux, Remove Pagle Companion behind its question (CMP-R71). Remove
  // runs removal and the process exits; Cancel changes nothing.
  const remove = document.getElementById("remove");
  const confirm = document.getElementById("remove-confirm");
  const removing = document.getElementById("removing");
  const showRemove = (status) => {
    remove.hidden = !status.canRemove || !removing.hidden;
  };
  remove.addEventListener("click", () => {
    confirm.hidden = false;
  });
  document.getElementById("remove-no").addEventListener("click", () => {
    confirm.hidden = true;
  });
  document.getElementById("remove-yes").addEventListener("click", () => {
    confirm.hidden = true;
    remove.hidden = true;
    removing.hidden = false;
    error.hidden = true;
    invoke("remove_companion").catch((message) => {
      removing.hidden = true;
      remove.hidden = false;
      error.textContent = String(message);
      error.hidden = false;
    });
  });

  invoke("start_at_login").then((on) => {
    toggle.checked = on;
    known = true;
    render();
  });
  invoke("status").then((status) => {
    readOnly = status.readOnly;
    showRemove(status);
    render();
  });
  listen("status", (event) => {
    readOnly = event.payload.readOnly;
    showRemove(event.payload);
    render();
  });
})();
