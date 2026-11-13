// One connection to the daemon's events, shared by every newcut tab.
// Browsers allow only a few connections to a site at once (6 over
// HTTP/1.1), and an event stream holds one for as long as it's open: one
// per tab would leave pages and images waiting once a few tabs are open.
let ports = [];
let source = null;
let open = false;

function send(message) {
  for (const port of ports) port.postMessage(message);
}

function connect() {
  source = new EventSource("/api/v1/events");
  source.onopen = function () { open = true; send({ kind: "open" }); };
  source.onerror = function () { open = false; send({ kind: "error" }); };
  source.onmessage = function (message) { send({ kind: "event", data: message.data }); };
}

onconnect = function (connection) {
  const port = connection.ports[0];
  ports.push(port);
  port.onmessage = function (message) {
    if (message.data !== "bye") return;
    ports = ports.filter(function (p) { return p !== port; });
    if (ports.length === 0 && source) {
      source.close();
      source = null;
      open = false;
    }
  };
  port.start();
  if (typeof EventSource === "undefined") {
    port.postMessage({ kind: "unsupported" });
    return;
  }
  if (!source) connect();
  else if (open) port.postMessage({ kind: "open" });
};
