// The window's update line in Status (DST-R78): while an update is waiting,
// "An important update is ready." in the act tone below the recommended
// version, else "An update is ready." in the info tone, each with Restart to
// update, which does what the tray item does (DST-R54). Nothing while no
// update waits, and nothing while update required: the stop message speaks
// then (DST-R59).
(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;

  const notice = document.getElementById("update-ready");
  const text = document.getElementById("update-ready-text");
  const restart = document.getElementById("update-restart");

  function render(view) {
    notice.hidden = !view.text;
    if (!view.text) return;
    // The notice's tone (style.css, Notices).
    notice.className = `notice ${view.tone}`;
    text.textContent = view.text;
    restart.disabled = false;
  }

  restart.addEventListener("click", () => {
    // The Companion exits once the update starts; until then, one click. An
    // update that couldn't start stays waiting, so the button comes back.
    restart.disabled = true;
    invoke("restart_to_update");
    setTimeout(() => {
      restart.disabled = false;
    }, 10000);
  });

  invoke("update_view").then(render);
  listen("update", (event) => render(event.payload));
})();
