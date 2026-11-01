// The dashboard follows the daemon's events: download progress redraws the
// "In progress" panel; anything else also reloads the status block. After a
// reconnect, both catch up.
(function () {
  let timer = null;
  let statusToo = false;
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
  function connect() {
    const events = new EventSource("/api/v1/events");
    events.onopen = function () { refresh(true); };
    events.onmessage = function (message) {
      let event = {};
      try { event = JSON.parse(message.data); } catch (_) {}
      refresh(!(event.type === "downloading" && event.percent !== null));
    };
  }
  if (window.EventSource) connect();
})();
