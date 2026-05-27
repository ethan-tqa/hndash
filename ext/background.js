const DEFAULTS = [
  "doubleclick.net",
  "googleadservices.com",
  "example-tracker.com",
];

let blockedStrings = [...DEFAULTS];

async function loadBlockedStrings() {
  const { blocked } = await chrome.storage.sync.get("blocked");
  if (blocked && Array.isArray(blocked) && blocked.length > 0) {
    blockedStrings = blocked;
  } else {
    blockedStrings = [...DEFAULTS];
  }
}

function closeIfBlocked(tabId, url) {
  if (!url) return;
  const lower = url.toLowerCase();
  for (const pattern of blockedStrings) {
    if (lower.includes(pattern.toLowerCase())) {
      chrome.tabs.remove(tabId);
      return;
    }
  }
}

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (changeInfo.url) {
    closeIfBlocked(tabId, changeInfo.url);
  }
});

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (msg.action === "scanAll") {
    chrome.tabs.query({}, (tabs) => {
      for (const tab of tabs) {
        closeIfBlocked(tab.id, tab.url);
      }
    });
  }
});

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === "sync" && changes.blocked) {
    const newList = changes.blocked.newValue;
    if (Array.isArray(newList)) {
      blockedStrings = newList;
    }
  }
});

loadBlockedStrings();
