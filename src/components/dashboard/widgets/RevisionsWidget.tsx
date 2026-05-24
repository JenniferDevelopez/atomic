import { useEffect, useState } from 'react';
import { X } from 'lucide-react';
import { Section } from '../Section';
import { useUIStore } from '../../../stores/ui';
import { getTransport } from '../../../lib/transport';
import type { KnowledgeSignal, WikiUpdateEvidence } from '../../../types/knowledgeSignals';

const MAX_ITEMS = 5;

export function RevisionsWidget() {
  const openWikiReader = useUIStore(s => s.openWikiReader);
  const [signals, setSignals] = useState<KnowledgeSignal<WikiUpdateEvidence>[]>([]);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function fetchSignals() {
      setIsLoading(true);
      setError(null);
      try {
        const result = await getTransport().invoke<KnowledgeSignal<WikiUpdateEvidence>[]>('list_knowledge_signals', {
          providerId: 'wiki_update',
          limit: MAX_ITEMS,
        });
        if (!cancelled) {
          setSignals(result);
          setIsLoading(false);
        }
      } catch (err) {
        if (!cancelled) {
          console.error('Failed to load wiki revision suggestions:', err);
          setError(String(err));
          setIsLoading(false);
        }
      }
    }

    fetchSignals();
    return () => {
      cancelled = true;
    };
  }, []);

  const dismissSignal = async (signalKey: string) => {
    const previous = signals;
    setSignals(current => current.filter(signal => signal.id !== signalKey));
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
    } catch (err) {
      console.error('Failed to dismiss wiki revision suggestion:', err);
      setSignals(previous);
    }
  };

  return (
    <Section label="Revision suggestions">
      {isLoading ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          Loading revision suggestions...
        </div>
      ) : error ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          Could not load revision suggestions.
        </div>
      ) : signals.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          All wikis are up to date.
        </div>
      ) : (
        <ul className="-mx-2">
          {signals.map(signal => {
            const tagId = signal.evidence?.tag_id ?? signal.target.id;
            const tagName = signal.evidence?.tag_name ?? signal.target.label;
            const newAtomCount = signal.evidence?.new_atom_count ?? 0;

            return (
              <li key={signal.id} className="group flex items-start gap-1 px-2 py-1.5 rounded hover:bg-[var(--color-bg-hover)]/60">
                <button
                  onClick={() => openWikiReader(tagId, tagName)}
                  className="min-w-0 flex-1 text-left"
                >
                  <span className="flex items-baseline gap-3">
                    <span className="flex-1 min-w-0 truncate text-sm text-[var(--color-text-secondary)] group-hover:text-[var(--color-text-primary)]">
                      {tagName}
                    </span>
                    {newAtomCount > 0 && (
                      <span className="text-[11px] text-amber-400/90 tabular-nums shrink-0">
                        +{newAtomCount}
                      </span>
                    )}
                  </span>
                  {signal.reasons.length > 0 && (
                    <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                      {signal.reasons.slice(0, 2).map(reason => reason.label).join(' / ')}
                    </span>
                  )}
                </button>
                <button
                  onClick={() => dismissSignal(signal.id)}
                  title="Dismiss suggestion"
                  className="mt-0.5 shrink-0 text-[var(--color-text-tertiary)] opacity-0 transition-opacity hover:text-[var(--color-text-primary)] group-hover:opacity-100 focus:opacity-100"
                >
                  <X className="w-3.5 h-3.5" strokeWidth={2} />
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </Section>
  );
}
