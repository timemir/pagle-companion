// The window's Status and Linking parts (CMP UI): the link status, Link to
// Pagle, Link again…, Unlink with its confirmation (CMP-R63), "Last
// uploaded …", during a stop its stop message (CMP-R50), and the device
// flow's user code, URL and QR code (CMP-R60). The credential never reaches
// the window.
(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const $ = (id) => document.getElementById(id);

  let readOnly = false;
  let linkStatus = null;
  let linking = { phase: "idle" };

  const linkingUnderWay = () =>
    linking.phase === "starting" || linking.phase === "waiting";

  function renderStatus() {
    if (!linkStatus) return;
    $("link-status").textContent = linkStatus.text;
    $("keyring-locked").hidden = !linkStatus.keyringLocked;
    $("link-start").hidden = !linkStatus.offerLink;
    $("link-again").hidden = linkStatus.offerLink;
    $("unlink").hidden = linkStatus.offerLink;
    if (linkStatus.offerLink) $("unlink-confirm").hidden = true;
    $("stop-message").hidden = !linkStatus.stop;
    $("stop-message").textContent = linkStatus.stop ?? "";
    // The seal and the leading line (style.css, Status): stopped, keyring
    // locked, not linked, or uploading as it should.
    $("status").dataset.health = linkStatus.stop
      ? "stopped"
      : linkStatus.keyringLocked
        ? "attention"
        : linkStatus.offerLink
          ? "unlinked"
          : "ok";
    for (const id of ["link-start", "link-again", "unlink", "unlink-yes"]) {
      $(id).disabled = readOnly || linkingUnderWay();
    }
    // Disabled while update required: DST-R56 allows no device flow.
    for (const id of ["link-start", "link-again"]) {
      if (linkStatus.updateRequired) $(id).disabled = true;
    }
  }

  function renderLinking() {
    $("linking").hidden = linking.phase === "idle";
    $("linking-starting").hidden = linking.phase !== "starting";
    $("linking-waiting").hidden = linking.phase !== "waiting";
    $("linking-outcome").hidden = linking.phase !== "ended";
    $("link-cancel").hidden = !linkingUnderWay();
    if (linking.phase === "waiting") {
      $("user-code").textContent = linking.userCode;
      $("verification-uri").textContent = linking.verificationUri;
      // The QR code is an SVG element the Companion drew itself.
      $("qr").innerHTML = linking.qrSvg ?? "";
    }
    if (linking.phase === "ended") $("linking-outcome").textContent = linking.text;
    renderStatus();
  }

  function showError(error) {
    linking = { phase: "ended", text: String(error) };
    renderLinking();
  }

  function startLink() {
    $("unlink-confirm").hidden = true;
    invoke("start_link").catch(showError);
  }

  $("link-start").addEventListener("click", startLink);
  $("link-again").addEventListener("click", startLink);
  $("link-cancel").addEventListener("click", () => invoke("cancel_link"));
  $("verification-uri").addEventListener("click", (event) => {
    // The webview never navigates away: the page opens in the browser.
    event.preventDefault();
    invoke("open_link_page");
  });
  $("unlink").addEventListener("click", () => {
    $("unlink-confirm").hidden = false;
  });
  $("unlink-no").addEventListener("click", () => {
    $("unlink-confirm").hidden = true;
  });
  $("unlink-yes").addEventListener("click", () => {
    $("unlink-confirm").hidden = true;
    invoke("unlink").catch(showError);
  });

  listen("link-status", (event) => {
    linkStatus = event.payload;
    renderStatus();
  });
  listen("linking", (event) => {
    linking = event.payload;
    renderLinking();
  });
  listen("status", (event) => {
    readOnly = event.payload.readOnly;
    renderStatus();
  });

  invoke("status").then((status) => {
    readOnly = status.readOnly;
    renderStatus();
  });
  invoke("link_status").then((status) => {
    linkStatus = status;
    renderStatus();
  });
  // "Last uploaded {relative time}" or "Nothing uploaded yet" (CMP UI,
  // Status).
  const showLastUploaded = (text) => {
    $("last-uploaded").textContent = text;
  };
  invoke("last_uploaded").then(showLastUploaded);
  listen("last-uploaded", (event) => showLastUploaded(event.payload));

  invoke("linking_view").then((view) => {
    linking = view;
    renderLinking();
  });
})();
