//! Producer-owned metadata for one rendered snapshot. IDs are observation-local,
//! never element selectors. Text ownership survives the existing line filters;
//! consumers must not reconstruct nodes or ancestry by parsing snapshot text.

use std::collections::HashMap;
use std::ops::Range;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{SnapshotOptions, TreeNode};

#[cfg(test)]
mod tests;

const MAX_PROJECTION_NODES: usize = 4_096;
const MAX_PROJECTION_BYTES: usize = 256 * 1_024;

pub struct SnapshotObservation {
    pub snapshot: String,
    pub projection: SnapshotProjection,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotProjection {
    pub schema_version: u32,
    pub snapshot_sha256: String,
    pub source_byte_length: usize,
    pub coverage: ProjectionCoverage,
    pub nodes: Vec<ProjectionNode>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionCoverage {
    /// Complete refers only to nodes in the final, filtered snapshot, not the
    /// DOM, omitted AX nodes, hidden content, or unobserved frames.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub options: ProjectionOptions,
    pub observed_frame_count: usize,
    pub unavailable_frame_count: usize,
    pub unexpanded_frame_count: usize,
    pub unknown_ancestry: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionOptions {
    pub selector_applied: bool,
    pub interactive: bool,
    pub compact: bool,
    pub depth: Option<usize>,
    pub urls: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionNode {
    pub id: String,
    /// Nearest emitted ancestor, including the owning iframe when observed.
    /// Null can mean a snapshot root or unavailable outer ancestry; consult
    /// coverage.unknownAncestry and coverage.options before interpreting it.
    pub parent_id: Option<String>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<String>,
    /// Half-open UTF-8 byte offsets into the exact snapshot string. This covers
    /// the node's own rendered text (possibly multiline), never its descendants.
    pub source_byte_range: SourceByteRange,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<i64>,
    pub states: ProjectionStates,
    /// Snapshot-local frame occurrence, not a durable CDP target or selector.
    pub frame_id: String,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceByteRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Serialize)]
pub struct ProjectionStates {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
}

struct RecordedNode {
    node: ProjectionNode,
    parent: Option<usize>,
}

pub(super) struct ProjectionCollector {
    enabled: bool,
    unavailable: Option<&'static str>,
    nodes: Vec<RecordedNode>,
    node_bytes: usize,
    next_frame: usize,
    pub observed_frames: usize,
    pub unavailable_frames: usize,
    pub unexpanded_frames: usize,
    pub unknown_ancestry: bool,
}

impl ProjectionCollector {
    pub fn new(enabled: bool, unknown_ancestry: bool) -> Self {
        Self {
            enabled,
            unavailable: None,
            nodes: Vec::new(),
            node_bytes: 0,
            next_frame: 0,
            observed_frames: 0,
            unavailable_frames: 0,
            unexpanded_frames: 0,
            unknown_ancestry,
        }
    }

    fn disable(&mut self, reason: &'static str) {
        self.unavailable.get_or_insert(reason);
        self.nodes.clear();
    }

    fn record(
        &mut self,
        node: &TreeNode,
        display_name: &str,
        parent: Option<usize>,
        frame: usize,
    ) -> Option<usize> {
        if !self.enabled || self.unavailable.is_some() {
            return None;
        }
        if self.nodes.len() >= MAX_PROJECTION_NODES {
            self.disable("node_limit");
            return None;
        }
        // Reject a single oversized semantic field before cloning it. Exact
        // serialized sizes below account for JSON escaping and metadata too.
        let field_bytes = [
            node.role.as_str(),
            node.name.as_str(),
            if display_name == node.name {
                ""
            } else {
                display_name
            },
            node.value_text.as_deref().unwrap_or_default(),
            node.checked.as_deref().unwrap_or_default(),
            node.ref_id.as_deref().unwrap_or_default(),
        ]
        .iter()
        .fold(0usize, |total, value| total.saturating_add(value.len()));
        if field_bytes > MAX_PROJECTION_BYTES {
            self.disable("byte_limit");
            return None;
        }

        let id = self.nodes.len();
        let projected = ProjectionNode {
            id: format!("n{id}"),
            parent_id: None,
            ref_id: node.ref_id.clone(),
            source_byte_range: SourceByteRange::default(),
            role: node.role.clone(),
            name: node.name.clone(),
            display_name: (display_name != node.name).then(|| display_name.to_string()),
            value: node.value_text.clone(),
            level: node.level,
            states: ProjectionStates {
                checked: node.checked.clone(),
                disabled: node.disabled,
                required: node.required,
                selected: node.selected,
                expanded: node.expanded,
            },
            frame_id: format!("f{frame}"),
        };
        let Ok(bytes) = serde_json::to_vec(&projected) else {
            self.disable("serialization_failed");
            return None;
        };
        self.node_bytes += bytes.len();
        if self.node_bytes > MAX_PROJECTION_BYTES {
            self.disable("byte_limit");
            return None;
        }
        self.nodes.push(RecordedNode {
            node: projected,
            parent,
        });
        Some(id)
    }

    pub fn finish(
        mut self,
        rendered: ProjectedText,
        options: &SnapshotOptions,
    ) -> SnapshotObservation {
        let mut ranges: Vec<Option<Range<usize>>> = vec![None; self.nodes.len()];
        if self.unavailable.is_none() {
            for span in &rendered.spans {
                match &mut ranges[span.owner] {
                    Some(range) if range.end == span.range.start => range.end = span.range.end,
                    Some(_) => {
                        // The legacy iframe insertion can land inside multiline
                        // page text. Preserve that text, but never claim one
                        // contiguous own-text range includes a different node.
                        self.disable("noncontiguous_source_range");
                        break;
                    }
                    range => *range = Some(span.range.clone()),
                }
            }
        }

        let mut nodes = Vec::new();
        if self.unavailable.is_none() {
            for idx in 0..self.nodes.len() {
                let Some(range) = &ranges[idx] else {
                    continue;
                };
                let mut parent = self.nodes[idx].parent;
                while let Some(parent_idx) = parent {
                    if ranges[parent_idx].is_some() {
                        break;
                    }
                    parent = self.nodes[parent_idx].parent;
                }
                self.nodes[idx].node.parent_id = parent.map(|id| format!("n{id}"));
                self.nodes[idx].node.source_byte_range = SourceByteRange {
                    start: range.start,
                    end: range.end,
                };
            }
            nodes = self
                .nodes
                .into_iter()
                .enumerate()
                .filter_map(|(idx, record)| ranges[idx].as_ref().map(|_| record.node))
                .collect();
            nodes.sort_by_key(|node| node.source_byte_range.start);
        }

        let mut projection = SnapshotProjection {
            schema_version: 1,
            snapshot_sha256: hex::encode(Sha256::digest(rendered.text.as_bytes())),
            source_byte_length: rendered.text.len(),
            coverage: ProjectionCoverage {
                status: if self.unavailable.is_some() {
                    "unavailable"
                } else {
                    "complete"
                },
                reason: self.unavailable,
                options: ProjectionOptions {
                    selector_applied: options.selector.is_some(),
                    interactive: options.interactive,
                    compact: options.compact,
                    depth: options.depth,
                    urls: options.urls,
                },
                observed_frame_count: self.observed_frames,
                unavailable_frame_count: self.unavailable_frames,
                unexpanded_frame_count: self.unexpanded_frames,
                unknown_ancestry: self.unknown_ancestry,
            },
            nodes,
        };
        // Include ranges, parent IDs, and the envelope in the final wire bound.
        if serde_json::to_vec(&projection).map_or(true, |bytes| bytes.len() > MAX_PROJECTION_BYTES)
        {
            projection.coverage.status = "unavailable";
            projection.coverage.reason = Some("byte_limit");
            projection.nodes.clear();
        }
        SnapshotObservation {
            snapshot: rendered.text,
            projection,
        }
    }
}

pub(super) struct FrameProjection<'a> {
    pub collector: &'a mut ProjectionCollector,
    frame: usize,
    pub parent: Option<usize>,
    pub rendered_nodes: HashMap<usize, usize>,
}

impl<'a> FrameProjection<'a> {
    pub fn new(collector: &'a mut ProjectionCollector, parent: Option<usize>) -> Self {
        let frame = collector.next_frame;
        collector.next_frame += 1;
        Self {
            collector,
            frame,
            parent,
            rendered_nodes: HashMap::new(),
        }
    }

    pub fn record(
        &mut self,
        idx: usize,
        node: &TreeNode,
        display_name: &str,
        parent: Option<usize>,
    ) -> Option<usize> {
        let id = self
            .collector
            .record(node, display_name, parent, self.frame)?;
        self.rendered_nodes.insert(idx, id);
        Some(id)
    }
}

#[derive(Clone)]
struct OwnedSpan {
    range: Range<usize>,
    owner: usize,
}

#[derive(Default)]
pub(super) struct ProjectedText {
    pub text: String,
    spans: Vec<OwnedSpan>,
}

impl ProjectedText {
    fn push_span(&mut self, range: Range<usize>, owner: usize) {
        if range.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut() {
            if last.owner == owner && last.range.end == range.start {
                last.range.end = range.end;
                return;
            }
        }
        self.spans.push(OwnedSpan { range, owner });
    }

    pub fn append(&mut self, text: &str, owner: Option<usize>) {
        let start = self.text.len();
        self.text.push_str(text);
        if let Some(owner) = owner {
            self.push_span(start..self.text.len(), owner);
        }
    }

    fn owner_at(&self, offset: usize) -> Option<usize> {
        self.spans
            .get(self.spans.partition_point(|span| span.range.end <= offset))
            .filter(|span| span.range.contains(&offset))
            .map(|span| span.owner)
    }

    fn copy_range(&mut self, source: &Self, range: Range<usize>) {
        let start = self.text.len();
        self.text.push_str(&source.text[range.clone()]);
        let first = source
            .spans
            .partition_point(|span| span.range.end <= range.start);
        for span in source.spans[first..]
            .iter()
            .take_while(|span| span.range.start < range.end)
        {
            self.push_span(
                start + span.range.start.max(range.start) - range.start
                    ..start + span.range.end.min(range.end) - range.start,
                span.owner,
            );
        }
    }

    pub fn insert(&mut self, position: usize, inserted: &Self) {
        let mut result = Self::default();
        result.copy_range(self, 0..position);
        result.copy_range(inserted, 0..inserted.text.len());
        result.copy_range(self, position..self.text.len());
        *self = result;
    }

    /// Applies the renderer's existing `str::lines` transformations, preserving
    /// byte ownership even when a field contains newlines or snapshot-like text.
    pub fn lines(&self, keep: Option<&[bool]>, prefix: &str, trailing_newline: bool) -> Self {
        let mut result = Self::default();
        let mut offset = 0;
        let mut has_line = false;
        let mut previous_owner = None;
        for (idx, line) in self.text.lines().enumerate() {
            let end = offset + line.len();
            if keep.is_none_or(|keep| keep[idx]) {
                if !trailing_newline && has_line {
                    result.append("\n", previous_owner);
                }
                result.append(prefix, self.owner_at(offset));
                result.copy_range(self, offset..end);
                if trailing_newline {
                    result.append("\n", self.owner_at(end).or_else(|| self.owner_at(offset)));
                }
                previous_owner = self.owner_at(end).or_else(|| self.owner_at(offset));
                has_line = true;
            }
            offset = end
                + if self.text[end..].starts_with("\r\n") {
                    2
                } else if self.text[end..].starts_with('\n') {
                    1
                } else {
                    0
                };
        }
        result
    }

    pub fn trim(self) -> Self {
        let trimmed = self.text.trim();
        let start = self.text.len() - self.text.trim_start().len();
        let mut result = Self::default();
        result.copy_range(&self, start..start + trimmed.len());
        result
    }
}
