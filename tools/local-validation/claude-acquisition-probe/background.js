/* All mutable state is session-scoped; worker globals hold neither attribution nor observations. */
importScripts("probe-core.js");
(() => {
  const handle = globalThis.QuotaBarProbeCore.createSessionHandler(chrome.storage.session);
  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => { handle(message).then(sendResponse, () => sendResponse({ ok: false })); return true; });
})();
