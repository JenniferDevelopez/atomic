import { useEffect } from 'react';
import { useSettingsStore } from '../stores/settings';

export type Theme = 'obsidian' | 'liquid-glass';

export const THEMES: { value: Theme; label: string }[] = [
  { value: 'obsidian', label: 'Paperboy Dark' },
  { value: 'liquid-glass', label: 'Paperboy Light' },
];

export function useTheme() {
  const settings = useSettingsStore(s => s.settings);
  const theme = (settings.theme as Theme) || 'obsidian';

  useEffect(() => {
    document.documentElement.setAttribute('data-theme', theme);
  }, [theme]);

  return theme;
}
