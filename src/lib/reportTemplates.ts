import { LucideIcon, Sunrise, Scale, HelpCircle, Compass } from 'lucide-react';
import { CreateReportInput } from '../stores/reports';

/// One curated starter report. The body pre-fills the editor when the
/// user picks the template; they can rename, retag, and tweak before
/// saving. Tag arrays stay empty in v1 templates because tags are
/// per-DB and the gallery has no way to know what tags exist where.
export interface ReportTemplate {
  /// Stable slug, used as the gallery card's React key and as a
  /// future analytics id. Don't rename without thinking about it.
  id: string;
  /// One-line gallery card description.
  description: string;
  /// Tiny schedule + scope hint shown under the description.
  /// Independently derived from `body` so the gallery doesn't have to
  /// re-implement schedule humanization.
  scheduleHint: string;
  /// Lucide icon component rendered in the card header.
  icon: LucideIcon;
  /// Body the editor receives via `initialBody`. `name` here is the
  /// suggested default; the user almost always renames before saving.
  body: CreateReportInput;
}

/// The four templates that ship in v1. Ordered by approximate
/// "first thing a new user should adopt" (briefing) → "most
/// distinctive use of the primitive" (contradiction scan) → utility
/// reports.
export const REPORT_TEMPLATES: ReportTemplate[] = [
  {
    id: 'daily-briefing',
    description:
      "Today's most notable atoms, synthesized into a 2–3 paragraph briefing with citations. Scope this to a specific topic — your database already has a general one.",
    scheduleHint: 'Daily 9am · cites source',
    icon: Sunrise,
    body: {
      name: 'Daily Briefing',
      description: 'Daily recap of new atoms within a chosen topic.',
      research_prompt:
        "Synthesize today's new source atoms into a 2-3 paragraph briefing that highlights what's noteworthy, what themes emerge, and where these new notes connect to existing knowledge. Skip atoms that aren't noteworthy. Write in the user's voice: concise, direct, mildly analytical, no filler.",
      schedule: '0 0 9 * * *',
      schedule_tz: null,
      enabled: true,
      source_scope_tag_ids: [],
      source_scope_window: 'since_last_run',
      source_include_kinds: ['captured'],
      context_scope_mode: 'same_as_source',
      context_scope_tag_ids: [],
      context_scope_window: null,
      context_include_kinds: ['captured'],
      citation_policy: 'source_only',
      output_atom_tags: [],
    },
  },
  {
    id: 'weekly-contradictions',
    description:
      "Each week, find statements in this week's new atoms that contradict, complicate, or qualify claims in your older notes. The highest-leverage use of the primitive.",
    scheduleHint: 'Monday 9am · cites source + context',
    icon: Scale,
    body: {
      name: 'Weekly Contradiction Scan',
      description: "Surfaces tensions between new captures and the corpus's prior claims.",
      research_prompt:
        'For each atom captured this week, look for places where it contradicts, complicates, or qualifies statements found in older notes. For each contradiction:\n- State the new claim with a [N] citation.\n- State the older claim it conflicts with, also cited.\n- Note whether this is a clean contradiction (direct logical conflict) or a tension (the new claim weakens or adds nuance to the older one).\nIf no contradictions exist this week, say so plainly and move on. Keep it tight — fewer well-found contradictions beat a long list of forced ones.',
      schedule: '0 0 9 * * 1',
      schedule_tz: null,
      enabled: true,
      source_scope_tag_ids: [],
      source_scope_window: 'since_last_run',
      source_include_kinds: ['captured'],
      context_scope_mode: 'all',
      context_scope_tag_ids: [],
      context_scope_window: 'older_than_source',
      context_include_kinds: ['captured'],
      citation_policy: 'source_and_context',
      output_atom_tags: [],
    },
  },
  {
    id: 'open-questions',
    description:
      'Weekly review of unresolved questions across your notes — what was asked, what got answered since, what is still open.',
    scheduleHint: 'Friday 4pm · cites source + context',
    icon: HelpCircle,
    body: {
      name: 'Open Questions Status',
      description: "Tracks unresolved questions across the corpus and reconciles them weekly.",
      research_prompt:
        "Walk this week's new atoms. For each open question you find:\n- Restate the question.\n- Cite the atom where it was raised.\n- Search the corpus for any answer or partial answer captured since the question was raised.\n- If you find an answer, cite it and mark \"resolved\" or \"partially resolved\".\n- If not, mark \"still open\".\nGroup output by topic when natural. End with a short list of the longest-standing still-open questions you can find.",
      schedule: '0 0 16 * * 5',
      schedule_tz: null,
      enabled: true,
      source_scope_tag_ids: [],
      source_scope_window: 'since_last_run',
      source_include_kinds: ['captured'],
      context_scope_mode: 'same_as_source',
      context_scope_tag_ids: [],
      context_scope_window: null,
      context_include_kinds: ['captured'],
      citation_policy: 'source_and_context',
      output_atom_tags: [],
    },
  },
  {
    id: 'monthly-themes',
    description:
      "End-of-month synthesis: 3–5 themes that dominated the last 30 days, what's new, what's continuing, and where to dig in next.",
    scheduleHint: '1st of month 10am · cites source',
    icon: Compass,
    body: {
      name: 'Themes This Month',
      description: 'Thematic synthesis of the last 30 days of capture.',
      research_prompt:
        'Synthesize the last 30 days of atoms into 3-5 themes. For each theme:\n- Name it in 3-5 words.\n- Describe the cluster of atoms it represents, with [N] citations.\n- Note whether this theme is new this month or continuing from prior coverage.\n- Call out the strongest individual atom (most-substantive or most-cited) for the theme.\nEnd with a "worth a deep dive" line picking the one theme you would recommend prioritizing.',
      // Monthly schedule — first of the month at 10:00. The preset
      // chooser only models daily/weekdays/weekly/hourly, so this
      // cron lands in the editor's "Custom" mode automatically.
      schedule: '0 0 10 1 * *',
      schedule_tz: null,
      enabled: true,
      source_scope_tag_ids: [],
      source_scope_window: { duration: 'P30D' },
      source_include_kinds: ['captured'],
      context_scope_mode: 'same_as_source',
      context_scope_tag_ids: [],
      context_scope_window: null,
      context_include_kinds: ['captured'],
      citation_policy: 'source_only',
      output_atom_tags: [],
    },
  },
];
