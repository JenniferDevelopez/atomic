import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

function read(path: string): string {
  return readFileSync(join(root, path), 'utf8');
}

function assert(condition: unknown, message: string): void {
  if (!condition) {
    throw new Error(message);
  }
}

const mainView = read('src/components/layout/MainView.tsx');
const themeHook = read('src/hooks/useTheme.ts');
const css = read('src/index.css');

const logoIndex = mainView.indexOf('<PaperboyLogoLink />');
const tabIndex = mainView.indexOf('data-atomic-tab-strip-anchor', logoIndex);
const toolbarIndex = mainView.indexOf('data-atomic-toolbar-actions', tabIndex);
const themeToggleIndex = mainView.indexOf('<AtomicThemeToggle />', toolbarIndex);

assert(existsSync(join(root, 'public/paperboy-logo.svg')), 'Paperboy logo asset is missing.');
assert(mainView.includes('data-paperboy-logo-link'), 'Paperboy logo link marker is missing.');
assert(mainView.includes('aria-label="Back to Paperboy"'), 'Paperboy logo accessible label is missing.');
assert(
  mainView.includes('https://paperboy.internal.yassineraddahi.com/paperboy'),
  'Paperboy return URL fallback is missing.',
);
assert(
  mainView.includes('import.meta.env.VITE_PAPERBOY_RETURN_URL'),
  'Paperboy return URL is not configurable via Vite env.',
);
assert(logoIndex >= 0, 'Paperboy logo is not rendered in the main titlebar.');
assert(tabIndex > logoIndex, 'Tab strip is not rendered after the Paperboy logo.');
assert(toolbarIndex > tabIndex, 'Toolbar actions are not rendered to the right of the tab strip.');
assert(themeToggleIndex > toolbarIndex, 'Theme toggle is not rendered inside the moved toolbar group.');
assert(mainView.includes('aria-label="Toggle theme"'), 'Theme toggle accessible label is missing.');
assert(mainView.includes("setSetting('theme', nextTheme)"), 'Theme toggle does not persist through existing settings.');

assert(themeHook.includes('Paperboy Dark'), 'Theme labels do not expose Paperboy Dark.');
assert(themeHook.includes('Paperboy Light'), 'Theme labels do not expose Paperboy Light.');

for (const token of [
  '--paperboy-bg',
  '--paperboy-surface',
  '--paperboy-border',
  '--paperboy-purple',
  '--paperboy-purple-bright',
  '--paperboy-text',
  '--paperboy-muted',
  '#090a0f',
  '#8b5cf6',
  '[data-theme="liquid-glass"]',
  '#f8fafc',
  '#5e0ed7',
]) {
  assert(css.includes(token), `Expected theme token/value missing: ${token}`);
}

const changedUiSources = [mainView, themeHook, css].join('\n');
for (const forbidden of [
  'localStorage.setItem',
  'sessionStorage.setItem',
  '?token=',
  'Bearer ',
  'encryptedToken',
  'atomicTokenId',
]) {
  assert(!changedUiSources.includes(forbidden), `Forbidden credential pattern added: ${forbidden}`);
}

console.log('PASS atomic UI Paperboy theme smoke');
