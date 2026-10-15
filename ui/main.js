// The window's view of the tray-mode process (CMP UI).
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

function render(status) {
  document.getElementById("version").textContent = `Version ${status.version}`;
  // CMP-R2: read-only while another process holds the watcher lock.
  document.getElementById("read-only").hidden = !status.readOnly;
}

invoke("status").then(render);
listen("status", (event) => render(event.payload));
