import { readStorageValue, writeStorageItem } from './storage';

export type MenuBarQuotaWindow = 'weekly' | 'five_hour';

export const MENU_BAR_QUOTA_WINDOW_STORAGE_KEY = 'menuBarQuotaWindow';

export function getSavedMenuBarQuotaWindow(): MenuBarQuotaWindow {
  const result = readStorageValue(MENU_BAR_QUOTA_WINDOW_STORAGE_KEY, (raw) => {
    if (raw !== 'weekly' && raw !== 'five_hour') {
      throw new Error('Invalid saved Codex menu-bar quota window');
    }
    return raw;
  }, { notifyUser: true });
  return result.status === 'value' ? result.value : 'weekly';
}

export function saveMenuBarQuotaWindow(window: MenuBarQuotaWindow): boolean {
  return writeStorageItem(MENU_BAR_QUOTA_WINDOW_STORAGE_KEY, window, {
    preserveSessionValue: true,
    notifyUser: true,
  });
}
