import { readStorageValue, writeStorageItem } from './storage';

export type MenuBarQuotaWindow = 'weekly' | 'five_hour';

export const MENU_BAR_QUOTA_WINDOW_STORAGE_KEY = 'menuBarQuotaWindow';
export const CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY = 'claudeMenuBarQuotaWindow';

function getSavedMenuBarQuotaWindowForKey(
  key: string,
  providerLabel: string,
): MenuBarQuotaWindow {
  const result = readStorageValue(key, (raw) => {
    if (raw !== 'weekly' && raw !== 'five_hour') {
      throw new Error(`Invalid saved ${providerLabel} menu-bar quota window`);
    }
    return raw;
  }, { notifyUser: true });
  return result.status === 'value' ? result.value : 'weekly';
}

function saveMenuBarQuotaWindowForKey(key: string, window: MenuBarQuotaWindow): boolean {
  return writeStorageItem(key, window, {
    preserveSessionValue: true,
    notifyUser: true,
  });
}

export function getSavedMenuBarQuotaWindow(): MenuBarQuotaWindow {
  return getSavedMenuBarQuotaWindowForKey(MENU_BAR_QUOTA_WINDOW_STORAGE_KEY, 'Codex');
}

export function saveMenuBarQuotaWindow(window: MenuBarQuotaWindow): boolean {
  return saveMenuBarQuotaWindowForKey(MENU_BAR_QUOTA_WINDOW_STORAGE_KEY, window);
}

export function getSavedClaudeMenuBarQuotaWindow(): MenuBarQuotaWindow {
  return getSavedMenuBarQuotaWindowForKey(CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY, 'Claude');
}

export function saveClaudeMenuBarQuotaWindow(window: MenuBarQuotaWindow): boolean {
  return saveMenuBarQuotaWindowForKey(CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY, window);
}
