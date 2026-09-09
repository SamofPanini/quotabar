import { PROBE_SLOTS, SlotStore, safeEnvelope } from "./shared.js";

let selectedSlot = "profile-a";
let selectedPlanClass = "unknown";
const observations = new SlotStore();

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "set-slot" && PROBE_SLOTS.includes(message.slot)) {
    selectedSlot = message.slot;
    sendResponse({ ok: true });
    return;
  }
  if (message?.type === "set-plan-class" && ["paid", "free", "unknown"].includes(message.planClass)) {
    selectedPlanClass = message.planClass;
    sendResponse({ ok: true });
    return;
  }
  if (message?.type === "observation") {
    const candidate = safeEnvelope({ ...message.output, probeSlot: selectedSlot, planClass: selectedPlanClass });
    sendResponse({ ok: candidate !== null && observations.write(candidate) });
    return;
  }
  if (message?.type === "read-observation") {
    sendResponse(observations.read(selectedSlot));
  }
});
