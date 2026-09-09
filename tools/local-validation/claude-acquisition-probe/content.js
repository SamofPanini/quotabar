window.addEventListener("quotabar-local-probe-observation", (event) => {
  const output = globalThis.QuotaBarProbeValidator.safeEnvelope(event.detail);
  if (output) chrome.runtime.sendMessage({ type: "observation", output });
});
