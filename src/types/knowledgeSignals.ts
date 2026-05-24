export interface KnowledgeSignalTarget {
  kind: string;
  id: string;
  label: string;
}

export interface KnowledgeSignalReason {
  kind: string;
  label: string;
  value: unknown;
  contribution: number;
}

export interface KnowledgeSignalAction {
  id: string;
  label: string;
  kind: string;
}

export interface KnowledgeSignal<Evidence = Record<string, unknown>> {
  id: string;
  provider_id: string;
  target: KnowledgeSignalTarget;
  score: number;
  confidence: number;
  severity?: string;
  title: string;
  summary: string;
  reasons: KnowledgeSignalReason[];
  evidence?: Evidence;
  suggested_actions?: KnowledgeSignalAction[];
  created_at?: string;
  expires_at?: string | null;
}

export interface WikiCandidateEvidence {
  schema?: string;
  schema_version?: number;
  tag_id?: string;
  tag_name?: string;
  atom_count?: number;
  mention_count?: number;
  source_count?: number;
  recent_count?: number;
}

export interface WikiUpdateEvidence {
  schema?: string;
  schema_version?: number;
  article_id?: string;
  tag_id?: string;
  tag_name?: string;
  article_atom_count?: number;
  current_atom_count?: number;
  new_atom_count?: number;
  new_source_count?: number;
  new_substantive_count?: number;
  new_recent_count?: number;
  inbound_link_count?: number;
  updated_at?: string;
}

export interface TagCleanupTagEvidence {
  id: string;
  name: string;
  parent_id?: string | null;
  path: string[];
  atom_count: number;
  child_count: number;
  has_wiki: boolean;
  is_autotag_target: boolean;
}

export interface TagRedundancyEvidence {
  schema?: string;
  schema_version?: number;
  primary_tag: TagCleanupTagEvidence;
  secondary_tag: TagCleanupTagEvidence;
  shared_atom_count: number;
  primary_unique_atom_count: number;
  secondary_unique_atom_count: number;
  jaccard_overlap: number;
  containment_overlap: number;
  centroid_similarity?: number | null;
  name_similarity: number;
  hierarchy_relationship: string;
  review_posture: string;
}

export interface EmptyTagEvidence {
  schema?: string;
  schema_version?: number;
  tag: TagCleanupTagEvidence;
}

export interface MissingTagOverlapEvidence {
  schema?: string;
  schema_version?: number;
  atom_id: string;
  atom_title: string;
  current_tag_count: number;
  suggested_tag: TagCleanupTagEvidence;
  nearby_tagged_atom_count: number;
  strongest_similarity: number;
  average_similarity: number;
}

export type TagCleanupEvidence = TagRedundancyEvidence | EmptyTagEvidence;
