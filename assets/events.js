// The daemon's events, for pages that follow them (the dashboard, download
// statuses): `newcutEvents.listen({ event, open, error })` calls `event`
// with each parsed event, `open` when connected (again: catch up), and
// `error` when the connection is lost.
//
// Every tab shares one connection through a shared worker (see
// events-worker.js); without one, each tab connects while it's visible.
(function () {
  // Read now: later, `currentScript` is whoever's running.
  const self = document.currentScript || document.querySelector("script[data-events-worker]");
  const workerUrl = self && self.dataset.eventsWorker;
  const listeners = [];
  let started = false;

  function dispatch(kind, payload) {
    for (const listener of listeners) {
      if (listener[kind]) listener[kind](payload);
    }
  }

  function receive(data) {
    let event;
    try { event = JSON.parse(data); } catch (_) { return; }
    dispatch("event", event);
  }

  function ownConnection() {
    if (!window.EventSource) return;
    let source = null;
    function connect() {
      source = new EventSource("/api/v1/events");
      source.onopen = function () { dispatch("open"); };
      source.onerror = function () { dispatch("error"); };
      source.onmessage = function (message) { receive(message.data); };
    }
    function disconnect() {
      if (source) source.close();
      source = null;
    }
    // A hidden tab lets go: connections are scarce.
    document.addEventListener("visibilitychange", function () {
      if (document.hidden) disconnect();
      else if (!source) connect();
    });
    window.addEventListener("pagehide", disconnect);
    if (!document.hidden) connect();
  }

  function sharedConnection() {
    if (!window.SharedWorker || !workerUrl) return false;
    let worker;
    try { worker = new SharedWorker(workerUrl); } catch (_) { return false; }
    worker.port.onmessage = function (message) {
      const m = message.data;
      if (m.kind === "event") receive(m.data);
      else if (m.kind === "open") dispatch("open");
      else if (m.kind === "error") dispatch("error");
      else if (m.kind === "unsupported") ownConnection();
    };
    worker.port.start();
    window.addEventListener("pagehide", function () { worker.port.postMessage("bye"); });
    return true;
  }

  function start() {
    if (started) return;
    started = true;
    if (!sharedConnection()) ownConnection();
  }

  window.newcutEvents = {
    listen: function (listener) {
      listeners.push(listener);
      start();
    },
  };
})();
