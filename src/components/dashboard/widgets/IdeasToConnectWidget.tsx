import { useEffect, useState } from 'react';
import { Check, FileText, X } from 'lucide-react';
import { toast } from 'sonner';
import { Section } from '../Section';
import { getTransport } from '../../../lib/transport';
import { useAtomsStore, type AtomWithTags } from '../../../stores/atoms';
import { useTagsStore } from '../../../stores/tags';
import { useUIStore } from '../../../stores/ui';
import type { KnowledgeSignal, MissingTagOverlapEvidence } from '../../../types/knowledgeSignals';

const MAX_ITEMS = 5;

type MissingTagSignal = KnowledgeSignal<MissingTagOverlapEvidence>;

function pct(value: number): string {
  return `${Math.round(value * 100)}%`;
}

export function IdeasToConnectWidget() {
  const openReader = useUIStore(s => s.openReader);
  const fetchAtoms = useAtomsStore(s => s.fetchAtoms);
  const fetchTags = useTagsStore(s => s.fetchTags);
  const [signals, setSignals] = useState<MissingTagSignal[]>([]);
  const [isLoading, setIsLoading] = useState(true);
  const [isApplying, setIsApplying] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function fetchSignals() {
      setIsLoading(true);
      setError(null);
      try {
        const result = await getTransport().invoke<MissingTagSignal[]>('list_knowledge_signals', {
          providerId: 'missing_tag_overlap',
          limit: MAX_ITEMS,
        });
        if (!cancelled) {
          setSignals(result);
          setIsLoading(false);
        }
      } catch (err) {
        if (!cancelled) {
          console.error('Failed to load connection suggestions:', err);
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

  const removeSignal = (signalKey: string) => {
    setSignals(current => current.filter(signal => signal.id !== signalKey));
  };

  const dismissSignal = async (signalKey: string) => {
    const previous = signals;
    removeSignal(signalKey);
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
    } catch (err) {
      console.error('Failed to dismiss connection suggestion:', err);
      setSignals(previous);
    }
  };

  const addTag = async (signal: MissingTagSignal) => {
    const evidence = signal.evidence;
    if (!evidence) return;
    const previous = signals;
    setIsApplying(signal.id);
    removeSignal(signal.id);
    try {
      await getTransport().invoke<AtomWithTags>('add_tag_to_atom', {
        atomId: evidence.atom_id,
        tagId: evidence.suggested_tag.id,
      });
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey: signal.id });
      await Promise.all([fetchAtoms(), fetchTags()]);
      toast.success('Tag added', {
        description: `${evidence.suggested_tag.name} added to ${evidence.atom_title}`,
      });
    } catch (err) {
      console.error('Failed to add suggested tag:', err);
      setSignals(previous);
      toast.error('Failed to add tag', { description: String(err) });
    } finally {
      setIsApplying(null);
    }
  };

  return (
    <Section label="Ideas to connect">
      {isLoading ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Loading connection ideas...</div>
      ) : error ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Could not load connection ideas.</div>
      ) : signals.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">No connection suggestions.</div>
      ) : (
        <ul className="-mx-2">
          {signals.map(signal => {
            const evidence = signal.evidence;
            if (!evidence) return null;
            return (
              <li key={signal.id} className="group flex items-start gap-2 rounded px-2 py-1.5 hover:bg-[var(--color-bg-hover)]/60">
                <div className="min-w-0 flex-1">
                  <span className="flex min-w-0 items-center gap-2 text-sm">
                    <span className="shrink-0 text-[var(--color-text-tertiary)]">Add</span>
                    <span className="min-w-0 truncate rounded border border-[var(--color-border)] bg-[var(--color-bg-tertiary)] px-1.5 py-0.5 text-xs font-medium text-[var(--color-text-primary)]">
                      {evidence.suggested_tag.name}
                    </span>
                  </span>
                  <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                    {evidence.atom_title || 'Untitled'} / {evidence.nearby_tagged_atom_count} nearby atoms / {pct(evidence.average_similarity)} avg similarity
                  </span>
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  <button
                    onClick={() => addTag(signal)}
                    disabled={isApplying === signal.id}
                    title="Add suggested tag"
                    className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)] disabled:opacity-60"
                  >
                    <Check className="h-3.5 w-3.5" strokeWidth={2} />
                  </button>
                  <button
                    onClick={() => openReader(evidence.atom_id)}
                    title="Open atom"
                    className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
                  >
                    <FileText className="h-3.5 w-3.5" strokeWidth={2} />
                  </button>
                  <button
                    onClick={() => dismissSignal(signal.id)}
                    title="Dismiss suggestion"
                    className="rounded p-1 text-[var(--color-text-tertiary)] opacity-0 transition-opacity hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)] group-hover:opacity-100 focus:opacity-100"
                  >
                    <X className="h-3.5 w-3.5" strokeWidth={2} />
                  </button>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </Section>
  );
}
