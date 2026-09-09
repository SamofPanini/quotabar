const slot = document.querySelector("#slot");
const planClass = document.querySelector("#plan-class");
const output = document.querySelector("#output");
const refresh = () => chrome.runtime.sendMessage({ type: "read-observation" }, (value) => {
  output.textContent = value ? JSON.stringify(value, null, 2) : "No sanitized observation.";
});
slot.addEventListener("change", () => chrome.runtime.sendMessage({ type: "set-slot", slot: slot.value }, refresh));
planClass.addEventListener("change", () => chrome.runtime.sendMessage({ type: "set-plan-class", planClass: planClass.value }, refresh));
refresh();
