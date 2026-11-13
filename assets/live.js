// The dashboard follows the daemon's events: work in progress redraws the
// "In progress" panel; anything else also reloads the status block. After a
// reconnect, both catch up.
(function () {
  const connection = document.getElementById("connection");
  let timer = null;
  let statusToo = false;
  function connected(yes) {
    if (!connection) return;
    connection.classList.toggle("stale", !yes);
    connection.textContent = yes ? "Live" : "Reconnecting";
  }
  function refresh(all) {
    statusToo = statusToo || all;
    if (timer) return;
    timer = setTimeout(function () {
      const status = statusToo;
      timer = null;
      statusToo = false;
      htmx.ajax("GET", "/fragments/live", { target: "#live", swap: "outerHTML" });
      if (status) htmx.ajax("GET", "/fragments/status", { target: "#status", swap: "outerHTML" });
    }, 300);
  }
  if (!window.newcutEvents) {
    connected(false);
    return;
  }
  window.newcutEvents.listen({
    open: function () { connected(true); refresh(true); },
    error: function () { connected(false); },
    event: function (event) { refresh(event.type !== "working"); },
  });
})();
