// The window's Settings (CMP UI): "Start at login" (CMP-R3), on by default.
// Turning it off deletes the autostart entry; the Companion keeps the setting
// and says when it couldn't change the entry. On SteamOS as an AppImage,
// "Keep uploading in Game Mode (experimental)" (CMP-R90), off by default,
// with the Steam Deck report link (DST-R71). Quit, and on Linux Remove Pagle
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

  // Keep uploading in Game Mode (CMP-R90): turning it on or off runs
  // systemctl, which may take a moment; a failure leaves it as it was and
  // shows the Companion's text.
  const gameModeSetting = document.getElementById("game-mode-setting");
  const gameMode = document.getElementById("game-mode");
  let gameModeKnown = false;
  let gameModeBusy = false;

  function renderGameMode() {
    gameMode.disabled = readOnly || !gameModeKnown || gameModeBusy;
  }

  gameMode.addEventListener("change", () => {
    const on = gameMode.checked;
    gameModeBusy = true;
    error.hidden = true;
    renderGameMode();
    invoke("set_game_mode", { on })
      .then((now) => {
        gameMode.checked = now;
      })
      .catch((message) => {
        gameMode.checked = !on;
        error.textContent = String(message);
        error.hidden = false;
      })
      .finally(() => {
        gameModeBusy = false;
        renderGameMode();
      });
  });
  document.getElementById("steam-deck-report").addEventListener("click", () => {
    invoke("open_steam_deck_report");
  });
  invoke("game_mode").then((view) => {
    gameMode.checked = view.on;
    gameModeSetting.hidden = !view.shown;
    gameModeKnown = true;
    renderGameMode();
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

  // Quit (CMP UI): the tray's Quit, also here for desktops without a tray.
  // It works from a read-only window too.
  document.getElementById("quit").addEventListener("click", () => {
    invoke("quit_companion");
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
    renderGameMode();
  });
  listen("status", (event) => {
    readOnly = event.payload.readOnly;
    showRemove(event.payload);
    render();
    renderGameMode();
  });
})();
