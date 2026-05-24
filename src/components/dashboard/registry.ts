import type { FC } from 'react';
import { BriefingWidget } from './widgets/BriefingWidget';
import { ActivityWidget } from './widgets/ActivityWidget';
import { NewWikisWidget } from './widgets/NewWikisWidget';
import { RevisionsWidget } from './widgets/RevisionsWidget';
import { TagCleanupWidget } from './widgets/TagCleanupWidget';
import { IdeasToConnectWidget } from './widgets/IdeasToConnectWidget';

export type WidgetSpan = 'full' | 'half';

export interface DashboardWidget {
  id: string;
  span: WidgetSpan;
  Component: FC;
}

export const dashboardWidgets: DashboardWidget[] = [
  { id: 'briefing', span: 'full', Component: BriefingWidget },
  { id: 'activity', span: 'half', Component: ActivityWidget },
  { id: 'new-wikis', span: 'half', Component: NewWikisWidget },
  { id: 'tag-cleanup', span: 'half', Component: TagCleanupWidget },
  { id: 'ideas-to-connect', span: 'half', Component: IdeasToConnectWidget },
  { id: 'revisions', span: 'full', Component: RevisionsWidget },
];
