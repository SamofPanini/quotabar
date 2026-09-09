const slot = document.querySelector("#slot");
const planClass = document.querySelector("#plan-class");
const output = document.querySelector("#output");
const message = (value) => new Promise((resolve) => chrome.runtime.sendMessage(value, (reply) => resolve(reply || { ok: false })));
async function render(state) { const view = globalThis.QuotaBarProbeCore.popupView(state); slot.value = view.slot; planClass.value = view.planClass; output.textContent = view.observation ? JSON.stringify(view.observation, null, 2) : "No sanitized observation."; }
async function load() { const reply = await message({ type: "get-state" }); if (reply.ok) await render(reply.state); }
async function save() { const reply = await message({ type: "set-settings", slot: slot.value, planClass: planClass.value }); if (reply.ok) await render(reply.state); }
slot.addEventListener("change", save); planClass.addEventListener("change", save); void load();
