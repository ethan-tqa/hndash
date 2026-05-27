const textarea = document.getElementById("patterns");
const saveBtn = document.getElementById("save");
const statusEl = document.getElementById("status");

async function load() {
  const { blocked } = await chrome.storage.sync.get("blocked");
  textarea.value = (blocked || []).join("\n");
}

async function save() {
  const lines = textarea.value
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
  await chrome.storage.sync.set({ blocked: lines });
  statusEl.textContent = "Saved!";
  setTimeout(() => {
    statusEl.textContent = "";
  }, 2000);
}

document.getElementById("scan").addEventListener("click", () => {
  chrome.runtime.sendMessage({ action: "scanAll" });
  statusEl.textContent = "Scanning all tabs…";
  setTimeout(() => {
    statusEl.textContent = "";
  }, 2000);
});

saveBtn.addEventListener("click", save);
document.addEventListener("DOMContentLoaded", load);
