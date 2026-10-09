import { useCallback, useEffect, useRef, useState } from 'react';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { hasTauriBackend } from '../services/backend';
import { readStorageValue, writeStorageItem } from '../services/storage';

export const UI_SCALE_KEY = 'quotabar-ui-scale';
export const UI_SCALES = [1, 1.25, 1.5] as const;
export type UiScale = typeof UI_SCALES[number];
export const UI_SCALE_ERROR = "Couldn't change interface size. Try again.";

export function getSavedUiScale(): UiScale {
  const result = readStorageValue(UI_SCALE_KEY, (raw) => {
    const scale = Number(raw);
    if (!UI_SCALES.includes(scale as UiScale)) throw new Error('Invalid interface size');
    return scale as UiScale;
  }, { notifyUser: true });
  return result.status === 'value' ? result.value : 1;
}

export function useUiScale(onError: (message: string) => void) {
  const [scale, setScale] = useState<UiScale>(1);
  const generation = useRef(0);
  const mounted = useRef(true);
  const confirmedScale = useRef<UiScale>(1);
  const queue = useRef(Promise.resolve());

  const applyNativeScale = useCallback(async (next: UiScale) => {
    if (hasTauriBackend()) {
      await getCurrentWebview().setZoom(next);
    } else if (typeof document !== 'undefined') {
      document.documentElement.style.zoom = String(next);
    }
  }, []);

  const enqueueScale = useCallback((next: UiScale, persist: boolean) => {
    const request = ++generation.current;
    const job = async () => {
      if (!mounted.current || request !== generation.current) return;
      try {
        await applyNativeScale(next);
        if (!mounted.current || request !== generation.current) return;
        confirmedScale.current = next;
        setScale(next);
        if (persist) {
          writeStorageItem(UI_SCALE_KEY, String(next), {
            preserveSessionValue: true,
            notifyUser: true,
          });
        }
      } catch {
        if (!mounted.current || request !== generation.current) return;
        onError(UI_SCALE_ERROR);
        try {
          await applyNativeScale(confirmedScale.current);
        } catch {
          // The original failure has already been reported; recovery is best-effort.
        }
      }
    };
    const pending = queue.current.then(job, job);
    queue.current = pending.catch(() => undefined);
    return pending;
  }, [applyNativeScale, onError]);

  useEffect(() => {
    mounted.current = true;
    const saved = getSavedUiScale();
    if (saved !== 1) void enqueueScale(saved, false);
    return () => {
      mounted.current = false;
      ++generation.current;
    };
  }, [enqueueScale]);

  return { scale, changeScale: (next: UiScale) => enqueueScale(next, true) };
}
