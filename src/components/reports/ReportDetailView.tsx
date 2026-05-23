import { useEffect, useState } from 'react';
import { ArrowLeft, Play, RefreshCw, Pencil, Trash2, MoreVertical } from 'lucide-react';
import { toast } from 'sonner';
import { ContextMenu } from '../ui/ContextMenu';
import { useReportsStore } from '../../stores/reports';
import { useUIStore } from '../../stores/ui';
import { StatusBadge } from './StatusBadge';
import { FindingsList } from './FindingsList';
import { FeaturedStarButton } from './FeaturedStarButton';
import { ReportDetailMeta } from './ReportDetailMeta';
import { ReportEditorModal } from './ReportEditorModal';
import { Modal } from '../ui/Modal';

/// How often (ms) we poll get_report while the detail view is open
/// and the report is in the running set. The poll catches the failure
/// path — successful completion is observed by the AtomCreated event.
const FAILURE_POLL_MS = 30_000;

/// Belt-and-suspenders timeout (ms) that clears optimistic running
/// state if neither the event nor the poll resolves. Covers browser
/// sleep, WebSocket hiccups, and dispatched-but-stuck workers.
const STALE_GUARD_MS = 5 * 60_000;

interface ReportDetailViewProps {
  reportId: string;
}

/// Top-level detail view for a single report. Step-1 skeleton: header
/// with back button + name + status badge, placeholder body. Findings
/// list, run-now, and the featured-star toggle land in later steps of
/// 4c.
///
/// Data flow:
/// - Reads the active report from `useReportsStore.byId[reportId]`.
/// - If the row isn't loaded (cold-start deep link, or list not yet
///   fetched), fetches it via `fetchOne`. On 404 → toast + close.
/// - Closing the view defers to `closeReportDetail`, which delegates
///   to `closeTab(activeTabId)` — same fallback-to-base-view path as
///   AtomReader.
export function ReportDetailView({ reportId }: ReportDetailViewProps) {
  const report = useReportsStore(s => s.byId[reportId]);
  const findings = useReportsStore(s => s.findingsByReport[reportId]);
  const isRunning = useReportsStore(s => s.runningReportIds.has(reportId));
  const dispatchedAt = useReportsStore(s => s.runDispatchedAt[reportId]);
  const lastErrorAtDispatch = useReportsStore(s => s.lastErrorAtDispatch[reportId]);
  const fetchOne = useReportsStore(s => s.fetchOne);
  const fetchFindings = useReportsStore(s => s.fetchFindings);
  const runNow = useReportsStore(s => s.runNow);
  const clearRunning = useReportsStore(s => s.clearRunning);
  const deleteReport = useReportsStore(s => s.delete);
  const ensureSubscription = useReportsStore(s => s.ensureSubscription);
  const closeReportDetail = useUIStore(s => s.closeReportDetail);
  const openReader = useUIStore(s => s.openReader);

  const [isInitialFetch, setIsInitialFetch] = useState(!report);
  const [editorOpen, setEditorOpen] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  /// Mobile overflow menu position. `null` when closed. The menu hosts
  /// Edit + Delete on small viewports where horizontal room for two
  /// distinct icon buttons isn't reliable.
  const [overflowMenuPos, setOverflowMenuPos] = useState<{ x: number; y: number } | null>(null);

  useEffect(() => {
    if (report) {
      setIsInitialFetch(false);
      return;
    }
    let cancelled = false;
    setIsInitialFetch(true);
    fetchOne(reportId).then((r) => {
      if (cancelled) return;
      setIsInitialFetch(false);
      if (!r) {
        toast.error('Report not found', {
          description: 'It may have been deleted in another window.',
        });
        closeReportDetail();
      }
    });
    return () => { cancelled = true; };
    // Intentionally not depending on `report` — once we have it the
    // first branch returns; we don't want to refetch on byId churn.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reportId, fetchOne, closeReportDetail]);

  // Fetch the full findings history once per report-id change. The
  // FindingsList shows a skeleton until this resolves.
  useEffect(() => {
    fetchFindings(reportId);
  }, [reportId, fetchFindings]);

  // Make sure the atom-created subscription is wired even on cold
  // deep-links to /reports/:id that bypass the list view. Idempotent
  // — the store guards against double-subscription.
  useEffect(() => {
    ensureSubscription();
  }, [ensureSubscription]);

  // Failure-detection poll. Only runs while the detail view is open
  // AND the report is in the running set — at most one report at any
  // time, so this is cheap.
  //
  // We can't use `last_run_at` to identify our dispatch's failure: the
  // runner intentionally does not update that column on failure (a
  // first-run failure would otherwise stamp an unparseable value into
  // the schedule anchor and silently wedge the report). Instead we
  // snapshot `last_error` at dispatch time and compare on each poll —
  // a value that has changed (especially: null → set) means a new
  // failure outcome was recorded by either our dispatch or a
  // concurrent scheduled tick. Either way the user gets the failure
  // surfaced, which is the desired UX.
  useEffect(() => {
    if (!isRunning || !dispatchedAt) return;
    let cancelled = false;
    const interval = window.setInterval(async () => {
      const fresh = await fetchOne(reportId);
      if (cancelled || !fresh) return;
      if (fresh.last_error && fresh.last_error !== lastErrorAtDispatch) {
        clearRunning(reportId);
        toast.error('Run failed', { description: fresh.last_error });
      }
    }, FAILURE_POLL_MS);
    return () => { cancelled = true; window.clearInterval(interval); };
  }, [isRunning, dispatchedAt, reportId, fetchOne, clearRunning, lastErrorAtDispatch]);

  // Stale guard. If neither the AtomCreated event nor the failure
  // poll resolves the running state within 5 minutes, clear it. Long
  // enough that a normal run never hits this (typical reports run in
  // seconds-to-tens-of-seconds); short enough that a hung optimistic
  // state doesn't strand the UI forever.
  useEffect(() => {
    if (!isRunning || !dispatchedAt) return;
    const remaining = Math.max(0, STALE_GUARD_MS - (Date.now() - dispatchedAt));
    const handle = window.setTimeout(() => {
      // Re-check after the timeout; another resolution path may have
      // already cleared it.
      if (useReportsStore.getState().runningReportIds.has(reportId)) {
        clearRunning(reportId);
        toast.message("Couldn't confirm completion", {
          description: 'Refresh the report list to check current state.',
        });
      }
    }, remaining);
    return () => window.clearTimeout(handle);
  }, [isRunning, dispatchedAt, reportId, clearRunning]);

  const handleRunNow = async () => {
    if (isRunning) return;
    try {
      await runNow(reportId);
    } catch {
      // store toasted
    }
  };

  const handleConfirmDelete = async () => {
    setConfirmDelete(false);
    try {
      await deleteReport(reportId);
      // Delete succeeded: the store has dropped the row; navigate back
      // to the list view. (The detail view would also auto-close on
      // next render because byId[reportId] is now undefined, but doing
      // it explicitly keeps the URL transition snappy.)
      closeReportDetail();
    } catch {
      // store toasted
    }
  };

  return (
    <div className="h-full overflow-hidden flex flex-col">
      {/* Header: back + name + status */}
      <div className="flex items-center gap-3 px-5 py-3 border-b border-[var(--color-border)] flex-shrink-0">
        <button
          onClick={closeReportDetail}
          title="Back to reports"
          aria-label="Back to reports"
          className="
            p-1.5 rounded-md text-[var(--color-text-secondary)]
            hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]
            transition-colors
          "
        >
          <ArrowLeft className="w-4 h-4" strokeWidth={2} />
        </button>

        <div className="flex items-center gap-3 min-w-0 flex-1">
          {report ? (
            <>
              <h2 className="text-base font-medium text-[var(--color-text-primary)] truncate">
                {report.name}
              </h2>
              <FeaturedStarButton reportId={report.id} />
              {/* Status badge lives on the meta band on mobile to keep
                  the header single-line. Desktop keeps it inline so
                  the run-state pulse is visible next to the name. */}
              <div className="hidden md:inline-flex">
                <StatusBadge report={report} isRunning={isRunning} />
              </div>
            </>
          ) : isInitialFetch ? (
            <div className="h-4 w-48 bg-[var(--color-border)] rounded animate-pulse" />
          ) : (
            <h2 className="text-base font-medium text-[var(--color-text-tertiary)]">
              Report unavailable
            </h2>
          )}
        </div>

        {/* Action cluster. Layout differs by viewport:
            - md+:    Run Now · Edit · Delete (three visible buttons)
            - <md:    Run Now · ⋮ overflow with Edit / Delete inside
            The Run Now button has a min-width pinned so the
            "Running…" state doesn't reflow the header. */}
        {report && (
          <div className="flex items-center gap-1.5 flex-shrink-0">
            <button
              type="button"
              onClick={handleRunNow}
              disabled={isRunning}
              title={isRunning ? 'Already running' : 'Run this report now'}
              className={`
                inline-flex items-center justify-center gap-1.5 px-3 py-1.5 rounded-md text-sm font-medium
                min-w-[112px]
                transition-colors
                ${isRunning
                  ? 'bg-[var(--color-bg-card)] border border-[var(--color-border)] text-[var(--color-text-tertiary)] cursor-not-allowed'
                  : 'bg-[var(--color-accent)] text-white hover:bg-[var(--color-accent-hover)]'
                }
              `}
            >
              {isRunning ? (
                <>
                  <RefreshCw className="w-3.5 h-3.5 animate-spin" strokeWidth={2.5} />
                  Running…
                </>
              ) : (
                <>
                  <Play className="w-3.5 h-3.5" strokeWidth={2.5} />
                  Run now
                </>
              )}
            </button>

            {/* Desktop: distinct Edit + Delete icon buttons. */}
            <button
              type="button"
              onClick={() => setEditorOpen(true)}
              title="Edit report"
              aria-label="Edit report"
              className="
                hidden md:inline-flex p-1.5 rounded-md text-[var(--color-text-secondary)]
                hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]
                transition-colors
              "
            >
              <Pencil className="w-4 h-4" strokeWidth={2} />
            </button>

            <button
              type="button"
              onClick={() => setConfirmDelete(true)}
              title="Delete report"
              aria-label="Delete report"
              className="
                hidden md:inline-flex p-1.5 rounded-md text-[var(--color-text-secondary)]
                hover:text-red-400 hover:bg-red-500/10
                transition-colors
              "
            >
              <Trash2 className="w-4 h-4" strokeWidth={2} />
            </button>

            {/* Mobile: ⋮ overflow with Edit + Delete inside. */}
            <button
              type="button"
              onClick={(e) => {
                const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
                setOverflowMenuPos({ x: rect.right - 160, y: rect.bottom + 4 });
              }}
              title="More actions"
              aria-label="More actions"
              className="
                md:hidden p-1.5 rounded-md text-[var(--color-text-secondary)]
                hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]
                transition-colors
              "
            >
              <MoreVertical className="w-4 h-4" strokeWidth={2} />
            </button>
          </div>
        )}
      </div>

      {/* Meta band: schedule strip + scope summary. Hidden while the
          report row is still loading. */}
      {report && <ReportDetailMeta report={report} />}

      {/* Findings history takes the rest of the available height. */}
      <div className="flex-1 min-h-0">
        {report ? (
          <FindingsList
            findings={findings}
            isLoading={findings === undefined}
            onFindingClick={(atomId) => openReader(atomId)}
          />
        ) : null}
      </div>

      {/* Edit modal — same surface as create. Save flows through
          useReportsStore.update, which patches byId in place. */}
      <ReportEditorModal
        isOpen={editorOpen}
        report={report ?? null}
        onClose={() => setEditorOpen(false)}
      />

      {/* Mobile overflow menu — Edit + Delete tucked here so the
          header stays single-line on small viewports. */}
      {overflowMenuPos && report && (
        <ContextMenu
          items={[
            {
              label: 'Edit…',
              icon: <Pencil className="w-3.5 h-3.5" strokeWidth={2} />,
              onClick: () => setEditorOpen(true),
            },
            {
              label: 'Delete',
              icon: <Trash2 className="w-3.5 h-3.5" strokeWidth={2} />,
              onClick: () => setConfirmDelete(true),
              danger: true,
            },
          ]}
          position={overflowMenuPos}
          onClose={() => setOverflowMenuPos(null)}
          autoFocus
        />
      )}

      {/* Delete confirm. Same copy as the list-view confirm: findings
          survive their producer by design. */}
      <Modal
        isOpen={confirmDelete}
        onClose={() => setConfirmDelete(false)}
        title={`Delete "${report?.name ?? ''}"?`}
        confirmLabel="Delete report"
        confirmVariant="danger"
        onConfirm={handleConfirmDelete}
      >
        <div className="text-sm text-[var(--color-text-secondary)] leading-relaxed">
          The schedule and report definition will be deleted. Past findings
          remain in your atoms — they're first-class notes, not owned by the
          report that produced them.
          {report?.last_finding_atom_id && (
            <span className="block mt-2 text-[var(--color-text-tertiary)] text-xs">
              The dashboard's featured report pointer is cleared if it points here.
            </span>
          )}
        </div>
      </Modal>
    </div>
  );
}
