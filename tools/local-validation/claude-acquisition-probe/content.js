(() => {
  const core = globalThis.QuotaBarProbeCore, send = (value) => chrome.runtime.sendMessage(value);
  const post = (type) => window.postMessage({ type, buildId: core.BUILD_ID }, location.origin);
  send({ type: "stage", stage: "bridge_loaded" });
  window.addEventListener("message", (event) => { const data = core.validWindowMessage(event, window, location.origin); if (!data) return; if (data.type === "quotabar-local-probe-main-ready") send({ type: "stage", stage: "bridge_received_main_ready" }); else if (data.type === "quotabar-local-probe-stage") send({ type: "stage", stage: data.stage }); else if (data.type === "quotabar-local-probe-observation") send({ type: "observation", output: data.output }); });
  post("quotabar-local-probe-ready-request");
})();
