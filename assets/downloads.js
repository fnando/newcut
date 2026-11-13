// Downloads change state in the background (slskd finishes, the importer
// files them or leaves them for you). Pages showing them follow the
// daemon's events and redraw those parts: each listens for
// `download-changed` on the body (its `detail.key` is the album).
(function () {
  if (!window.newcutEvents || !window.htmx) return;
  if (!document.querySelector("[data-download-live]")) return;
  let lost = false;
  window.newcutEvents.listen({
    event: function (event) {
      if (event.type !== "download") return;
      htmx.trigger(document.body, "download-changed", { key: event.key, state: event.state });
    },
    error: function () { lost = true; },
    open: function () {
      // Connected again: what changed meanwhile, everything redraws.
      if (lost) htmx.trigger(document.body, "download-changed", { key: "*" });
      lost = false;
    },
  });
})();
