// The window's Settings (CMP UI): "Start at login" (CMP-R3), on by default.
// Turning it off deletes the autostart entry; the Companion keeps the setting
// and says when it couldn't change the entry.
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

  invoke("start_at_login").then((on) => {
    toggle.checked = on;
    known = true;
    render();
  });
  invoke("status").then((status) => {
    readOnly = status.readOnly;
    render();
  });
  listen("status", (event) => {
    readOnly = event.payload.readOnly;
    render();
  });
})();
