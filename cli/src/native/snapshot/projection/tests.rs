use std::collections::HashSet;

use serde_json::json;

use super::super::{build_tree, compact_line_mask, render_tree};
use super::*;

fn node(role: &str, name: &str, reference: Option<&str>, children: &[usize]) -> TreeNode {
    let mut node = TreeNode::empty();
    node.role = role.to_string();
    node.name = name.to_string();
    node.has_ref = reference.is_some();
    node.ref_id = reference.map(str::to_string);
    node.children = children.to_vec();
    node
}

fn render_frame(
    nodes: &[TreeNode],
    roots: &[usize],
    options: &SnapshotOptions,
    projection: &mut FrameProjection<'_>,
) -> ProjectedText {
    let mut output = ProjectedText::default();
    for &idx in roots {
        render_tree(
            nodes,
            idx,
            0,
            projection.parent,
            &mut output,
            options,
            projection,
        );
    }
    output
}

fn finish_text(mut output: ProjectedText, options: &SnapshotOptions) -> ProjectedText {
    if options.compact {
        let keep = compact_line_mask(&output.text.lines().collect::<Vec<_>>());
        output = output.lines(Some(&keep), "", false);
    }
    output = output.trim();
    if output.text.is_empty() {
        output.append(
            if options.interactive {
                "(no interactive elements)"
            } else {
                "(empty page)"
            },
            None,
        );
    }
    output
}

fn observe(nodes: &[TreeNode], roots: &[usize], options: &SnapshotOptions) -> SnapshotObservation {
    let mut collector = ProjectionCollector::new(true, options.selector.is_some());
    collector.observed_frames = 1;
    let output = render_frame(
        nodes,
        roots,
        options,
        &mut FrameProjection::new(&mut collector, None),
    );
    collector.finish(finish_text(output, options), options)
}

fn own_text<'a>(observation: &'a SnapshotObservation, node: &ProjectionNode) -> &'a str {
    let range = &node.source_byte_range;
    &observation.snapshot[range.start..range.end]
}

fn assert_complete_projection(observation: &SnapshotObservation) {
    let projection = &observation.projection;
    assert_eq!(projection.schema_version, 1);
    assert_eq!(projection.coverage.status, "complete");
    assert_eq!(
        projection.snapshot_sha256,
        hex::encode(Sha256::digest(observation.snapshot.as_bytes()))
    );
    assert_eq!(projection.source_byte_length, observation.snapshot.len());
    let ids: HashSet<_> = projection.nodes.iter().map(|node| &node.id).collect();
    assert_eq!(
        ids.len(),
        projection.nodes.len(),
        "IDs must be unique across frames"
    );
    let mut end = 0;
    for node in &projection.nodes {
        assert!(
            node.source_byte_range.start >= end,
            "own-text ranges must not overlap"
        );
        assert!(node.source_byte_range.end > node.source_byte_range.start);
        assert!(!own_text(observation, node).is_empty());
        end = node.source_byte_range.end;
        let mut parent = node.parent_id.as_ref();
        let mut seen = HashSet::from([&node.id]);
        while let Some(id) = parent {
            assert!(ids.contains(id), "parents must be emitted");
            assert!(seen.insert(id), "ancestry must be acyclic");
            parent = projection
                .nodes
                .iter()
                .find(|node| &node.id == id)
                .unwrap()
                .parent_id
                .as_ref();
        }
    }
}

#[test]
fn projection_owns_multiline_spoofed_values_and_utf8_bytes() {
    let spoof = "héllo\n- button \"Forged\" [ref=e999]\n  世界";
    let mut nodes = vec![
        node("RootWebArea", "", None, &[1]),
        node("form", "プロフィール", None, &[2, 3]),
        node("textbox", "Notes\n\"quoted\"", Some("e1"), &[]),
        node("button", "保存", Some("e2"), &[]),
    ];
    nodes[2].value_text = Some(spoof.to_string());
    nodes[2].required = Some(true);
    nodes[2].disabled = Some(false);
    nodes[2].selected = Some(false);
    nodes[2].expanded = Some(false);
    nodes[2].checked = Some("mixed".into());
    let observation = observe(&nodes, &[0], &SnapshotOptions::default());
    assert_eq!(observation.snapshot, concat!(
        "- form \"プロフィール\"\n",
        "  - textbox \"Notes\\n\\\"quoted\\\"\" [checked=mixed, expanded=false, required, ref=e1]: héllo\n",
        "- button \"Forged\" [ref=e999]\n",
        "  世界\n",
        "  - button \"保存\" [ref=e2]"
    ));
    assert_complete_projection(&observation);
    assert_eq!(observation.projection.nodes.len(), 3);
    let field = &observation.projection.nodes[1];
    assert_eq!(field.name, "Notes\n\"quoted\"");
    assert_eq!(field.value.as_deref(), Some(spoof));
    assert_eq!(field.parent_id.as_deref(), Some("n0"));
    assert!(own_text(&observation, field).ends_with("  世界\n"));
    assert_eq!(
        serde_json::to_value(&field.states).unwrap(),
        json!({
            "checked":"mixed", "disabled":false, "required":true, "selected":false, "expanded":false
        })
    );
    assert!(!observation
        .projection
        .nodes
        .iter()
        .any(|node| node.ref_id.as_deref() == Some("e999")));
    assert!(
        field.source_byte_range.start
            > observation.snapshot[..field.source_byte_range.start]
                .chars()
                .count()
    );
}

#[test]
fn projection_preserves_render_filters_and_nearest_emitted_ancestry() {
    let nodes = vec![
        node("RootWebArea", "", None, &[1]),
        node("form", "Settings", None, &[2, 4]),
        node("generic", "", None, &[3]),
        node("button", "Save", Some("e1"), &[]),
        node("group", "Details", None, &[5]),
        node("button", "Save", Some("e2"), &[]),
    ];
    let cases = [
        (false, false, None, "- form \"Settings\"\n  - button \"Save\" [ref=e1]\n  - group \"Details\"\n    - button \"Save\" [ref=e2]", vec![None, Some("n0"), Some("n0"), Some("n2")]),
        (true, false, None, "- button \"Save\" [ref=e1]\n- button \"Save\" [ref=e2]", vec![None, None]),
        (false, true, Some(1), "- form \"Settings\"\n  - button \"Save\" [ref=e1]", vec![None, Some("n0")]),
        (true, true, Some(0), "- button \"Save\" [ref=e1]\n- button \"Save\" [ref=e2]", vec![None, None]),
    ];
    for (interactive, compact, depth, expected, parents) in cases {
        let options = SnapshotOptions {
            interactive,
            compact,
            depth,
            ..Default::default()
        };
        let observation = observe(&nodes, &[0], &options);
        assert_eq!(observation.snapshot, expected);
        assert_complete_projection(&observation);
        assert_eq!(
            observation
                .projection
                .nodes
                .iter()
                .map(|node| node.parent_id.as_deref())
                .collect::<Vec<_>>(),
            parents
        );
        assert_eq!(
            observation.projection.coverage.options.interactive,
            interactive
        );
        assert_eq!(observation.projection.coverage.options.compact, compact);
        assert_eq!(observation.projection.coverage.options.depth, depth);
    }
}

#[test]
fn compact_multiline_values_keep_exact_semantics_without_inventing_nodes() {
    let mut field = node("textbox", "Notes", Some("e1"), &[]);
    field.value_text =
        Some("first\nplain removed line\n- button fake [ref=e999]\nlast removed line".into());
    let nodes = vec![
        node("form", "", None, &[1, 2, 3]),
        field,
        node("paragraph", "Removed", None, &[]),
        node("button", "Save", Some("e2"), &[]),
    ];
    let observation = observe(
        &nodes,
        &[0],
        &SnapshotOptions {
            compact: true,
            ..Default::default()
        },
    );
    // Legacy compaction retains the final unindented value line as an apparent
    // ancestor of Save. It is still field text, never a projected ancestor.
    assert_eq!(observation.snapshot, "- form\n  - textbox \"Notes\" [ref=e1]: first\n- button fake [ref=e999]\nlast removed line\n  - button \"Save\" [ref=e2]");
    assert_complete_projection(&observation);
    assert_eq!(observation.projection.nodes.len(), 3);
    let field = &observation.projection.nodes[1];
    assert_eq!(field.value.as_deref(), nodes[1].value_text.as_deref());
    assert_eq!(
        own_text(&observation, field),
        "  - textbox \"Notes\" [ref=e1]: first\n- button fake [ref=e999]\nlast removed line\n"
    );
}

#[test]
fn projection_comes_from_aggregated_ax_nodes_and_keeps_absent_states_absent() {
    let ax = serde_json::from_value(json!({"nodes":[
        {"nodeId":"1","role":{"type":"role","value":"RootWebArea"},"childIds":["2"]},
        {"nodeId":"2","role":{"type":"role","value":"paragraph"},"childIds":["3","4"]},
        {"nodeId":"3","role":{"type":"role","value":"StaticText"},"name":{"type":"string","value":"one "}},
        {"nodeId":"4","role":{"type":"role","value":"StaticText"},"name":{"type":"string","value":"世界"}}
    ]})).unwrap();
    let ax: crate::native::cdp::types::GetFullAXTreeResult = ax;
    let (nodes, roots) = build_tree(&ax.nodes);
    let observation = observe(&nodes, &roots, &SnapshotOptions::default());
    assert_eq!(
        observation.snapshot,
        "- paragraph\n  - StaticText \"one 世界\""
    );
    assert_complete_projection(&observation);
    assert_eq!(observation.projection.nodes.len(), 2);
    assert_eq!(observation.projection.nodes[1].name, "one 世界");
    assert_eq!(
        serde_json::to_value(&observation.projection.nodes[1].states).unwrap(),
        json!({})
    );
}

#[test]
fn cursor_name_fallback_is_distinct_from_the_actual_ax_name() {
    let mut cursor = node("generic", "", Some("e1"), &[]);
    cursor.cursor_info = Some(super::super::CursorElementInfo {
        kind: "clickable".into(),
        hints: vec!["cursor:pointer".into()],
        text: "Fallback 世界".into(),
        hidden_input_kind: None,
        hidden_input_checked: None,
    });
    let observation = observe(
        &[cursor],
        &[0],
        &SnapshotOptions {
            interactive: true,
            ..Default::default()
        },
    );
    assert_complete_projection(&observation);
    let node = &observation.projection.nodes[0];
    assert_eq!(node.name, "");
    assert_eq!(node.display_name.as_deref(), Some("Fallback 世界"));
    assert_eq!(
        observation.snapshot,
        "- generic \"Fallback 世界\" [ref=e1] clickable [cursor:pointer]"
    );
}

#[test]
fn frame_ids_and_parent_ids_survive_indentation_and_insertion() {
    let options = SnapshotOptions::default();
    let mut collector = ProjectionCollector::new(true, false);
    let mut main = FrameProjection::new(&mut collector, None);
    let mut text = render_frame(
        &[
            node("Iframe", "one", Some("e1"), &[]),
            node("Iframe", "two", Some("e2"), &[]),
        ],
        &[0, 1],
        &options,
        &mut main,
    );
    let owners = [main.rendered_nodes[&0], main.rendered_nodes[&1]];
    for (idx, owner) in owners.into_iter().enumerate() {
        let child = render_frame(
            &[node(
                "button",
                "Save",
                Some(if idx == 0 { "e3" } else { "e4" }),
                &[],
            )],
            &[0],
            &options,
            &mut FrameProjection::new(&mut collector, Some(owner)),
        );
        let marker = format!("[ref=e{}]", idx + 1);
        let pos = text.text.find(&marker).unwrap();
        let end = pos + text.text[pos..].find('\n').unwrap() + 1;
        text.insert(end, &child.trim().lines(None, "  ", true));
    }
    collector.observed_frames = 3;
    collector.unexpanded_frames = 1;
    collector.unavailable_frames = 1;
    let observation = collector.finish(text.trim(), &options);
    assert_eq!(observation.snapshot, "- Iframe \"one\" [ref=e1]\n  - button \"Save\" [ref=e3]\n- Iframe \"two\" [ref=e2]\n  - button \"Save\" [ref=e4]");
    assert_complete_projection(&observation);
    let nodes = &observation.projection.nodes;
    assert_eq!(nodes[1].parent_id.as_deref(), Some(nodes[0].id.as_str()));
    assert_eq!(nodes[3].parent_id.as_deref(), Some(nodes[2].id.as_str()));
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.frame_id.as_str())
            .collect::<Vec<_>>(),
        vec!["f0", "f1", "f0", "f2"]
    );
    assert_eq!(observation.projection.coverage.observed_frame_count, 3);
    assert_eq!(observation.projection.coverage.unexpanded_frame_count, 1);
    assert_eq!(observation.projection.coverage.unavailable_frame_count, 1);
}

#[test]
fn wire_fixture_matches_the_native_serialization_contract() {
    let options = SnapshotOptions::default();
    let mut collector = ProjectionCollector::new(true, false);
    let mut checkbox = node("checkbox", "Accept", Some("e1"), &[]);
    checkbox.checked = Some("mixed".into());
    checkbox.disabled = Some(false);
    checkbox.required = Some(true);
    checkbox.selected = Some(false);
    checkbox.expanded = Some(false);
    let nodes = [
        node("form", "設定", None, &[1, 2]),
        checkbox,
        node("Iframe", "Child", Some("e2"), &[]),
    ];
    let mut main = FrameProjection::new(&mut collector, None);
    let mut text = render_frame(&nodes, &[0], &options, &mut main);
    let owner = main.rendered_nodes[&2];
    let child = render_frame(
        &[node("button", "保存", Some("e3"), &[])],
        &[0],
        &options,
        &mut FrameProjection::new(&mut collector, Some(owner)),
    );
    text.insert(text.text.len(), &child.trim().lines(None, "    ", true));
    collector.observed_frames = 2;
    let observation = collector.finish(text.trim(), &options);
    assert_complete_projection(&observation);
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/v1-wire.json")).unwrap();
    assert_eq!(
        json!({"snapshot":observation.snapshot,"projection":observation.projection}),
        fixture
    );
}

#[test]
fn source_ownership_refuses_noncontiguous_ranges_instead_of_swallowing_another_node() {
    let options = SnapshotOptions::default();
    let mut collector = ProjectionCollector::new(true, false);
    let mut field = node("textbox", "Notes", Some("e1"), &[]);
    field.value_text = Some("line [ref=e2]\nremaining field value".into());
    let mut main = FrameProjection::new(&mut collector, None);
    let mut text = render_frame(
        &[field, node("Iframe", "", Some("e2"), &[])],
        &[0, 1],
        &options,
        &mut main,
    );
    let owner = main.rendered_nodes[&1];
    let child = render_frame(
        &[node("button", "child", Some("e3"), &[])],
        &[0],
        &options,
        &mut FrameProjection::new(&mut collector, Some(owner)),
    );
    let position = text.text.find('\n').unwrap() + 1;
    text.insert(position, &child.trim().lines(None, "  ", true));
    let expected = text.text.trim().to_string();
    let observation = collector.finish(text.trim(), &options);
    assert_eq!(observation.snapshot, expected);
    assert_eq!(observation.projection.coverage.status, "unavailable");
    assert_eq!(
        observation.projection.coverage.reason,
        Some("noncontiguous_source_range")
    );
    assert!(observation.projection.nodes.is_empty());
}

#[test]
fn omitted_ancestors_resolve_to_the_nearest_emitted_parent() {
    let options = SnapshotOptions::default();
    let mut collector = ProjectionCollector::new(true, false);
    let text = render_frame(
        &[
            node("form", "", None, &[1]),
            node("group", "", None, &[2]),
            node("button", "Save", Some("e1"), &[]),
        ],
        &[0],
        &options,
        &mut FrameProjection::new(&mut collector, None),
    );
    // Exercise provenance independently of the current compact-line policy:
    // removing a real ancestor must reconnect through the recorded AX parent.
    let text = text.lines(Some(&[true, false, true]), "", false).trim();
    let observation = collector.finish(text, &options);
    assert_complete_projection(&observation);
    assert_eq!(observation.projection.nodes.len(), 2);
    assert_eq!(
        observation.projection.nodes[1].parent_id.as_deref(),
        Some("n0")
    );
}

#[test]
fn empty_and_filtered_snapshots_have_no_fabricated_nodes() {
    for options in [
        SnapshotOptions::default(),
        SnapshotOptions {
            interactive: true,
            ..Default::default()
        },
        SnapshotOptions {
            compact: true,
            ..Default::default()
        },
    ] {
        let observation = observe(&[], &[], &options);
        assert_complete_projection(&observation);
        assert!(observation.projection.nodes.is_empty());
        assert_eq!(
            observation.snapshot,
            if options.interactive {
                "(no interactive elements)"
            } else {
                "(empty page)"
            }
        );
    }
    let observation = observe(
        &[node("button", "Scoped", Some("e1"), &[])],
        &[0],
        &SnapshotOptions {
            selector: Some("#scope".into()),
            ..Default::default()
        },
    );
    assert!(observation.projection.coverage.unknown_ancestry);
    assert!(observation.projection.coverage.options.selector_applied);
    assert!(observation.projection.nodes[0].parent_id.is_none());
}

#[test]
fn whole_projection_limits_never_sample_or_truncate_the_snapshot() {
    let nodes: Vec<_> = (0..MAX_PROJECTION_NODES + 1)
        .map(|idx| {
            node(
                "button",
                &format!("Control {idx}"),
                Some(&format!("e{}", idx + 1)),
                &[],
            )
        })
        .collect();
    let roots: Vec<_> = (0..nodes.len()).collect();
    let observation = observe(&nodes, &roots, &SnapshotOptions::default());
    assert_eq!(observation.projection.coverage.status, "unavailable");
    assert!(observation.projection.nodes.is_empty());
    assert!(observation
        .snapshot
        .contains(&format!("Control {MAX_PROJECTION_NODES}")));
    assert!(serde_json::to_vec(&observation.projection).unwrap().len() <= MAX_PROJECTION_BYTES);

    let mut field = node("textbox", "Huge", Some("e1"), &[]);
    field.value_text = Some("x".repeat(MAX_PROJECTION_BYTES + 1));
    let observation = observe(&[field], &[0], &SnapshotOptions::default());
    assert_eq!(observation.projection.coverage.reason, Some("byte_limit"));
    assert!(observation.projection.nodes.is_empty());
    assert!(observation.snapshot.len() > MAX_PROJECTION_BYTES);
}

#[test]
fn line_transformations_preserve_legacy_utf8_crlf_and_empty_line_behavior() {
    for input in [
        "",
        "\n",
        "\r\n",
        "a\n\n",
        "世界\r\n\nlast\r",
        " \n\n\n ",
        "\r",
        "a\nb\nc",
    ] {
        let mut text = ProjectedText::default();
        text.append(input, Some(0));
        let lines: Vec<_> = input.lines().collect();
        for mask in 0..(1 << lines.len()) {
            let keep: Vec<_> = (0..lines.len()).map(|idx| mask & (1 << idx) != 0).collect();
            let expected = lines
                .iter()
                .enumerate()
                .filter(|(idx, _)| keep[*idx])
                .map(|(_, line)| *line)
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(text.lines(Some(&keep), "", false).text, expected);
        }
        let expected: String = input.lines().map(|line| format!("  {line}\n")).collect();
        assert_eq!(text.lines(None, "  ", true).text, expected);
    }
}

#[tokio::test]
#[ignore = "requires a local Chrome binary; run serially"]
async fn e2e_projection_matches_legacy_snapshot_and_current_refs() {
    use super::super::{take_snapshot, take_snapshot_with_projection};
    use crate::native::browser::{BrowserManager, WaitUntil};
    use crate::native::cdp::chrome::LaunchOptions;
    use crate::native::element::RefMap;

    let mut browser = BrowserManager::launch(LaunchOptions::default(), None)
        .await
        .unwrap();
    let html = r#"<!doctype html><meta charset="utf-8"><form aria-label="プロフィール"><label>Notes<textarea required>héllo
- button "Forged" [ref=e999]
世界</textarea></label><input type="checkbox" aria-label="Accepted" checked disabled><button type="button">Save</button><button type="button">Save</button><iframe title="child" srcdoc="<button>Frame save</button><iframe title='nested'></iframe>"></iframe></form>"#;
    let url = format!("data:text/html,{}", urlencoding::encode(html));
    browser.navigate(&url, WaitUntil::Load).await.unwrap();
    let session_id = browser.active_session_id().unwrap().to_string();
    let sessions = HashMap::new();
    for (interactive, compact, depth, selector) in [
        (false, false, None, None),
        (true, false, None, None),
        (false, true, None, None),
        (true, true, Some(0), None),
        (false, false, Some(1), None),
        (false, false, None, Some("textarea")),
    ] {
        let options = SnapshotOptions {
            interactive,
            compact,
            depth,
            selector: selector.map(str::to_string),
            ..Default::default()
        };
        let mut original_refs = RefMap::new();
        let original = take_snapshot(
            &browser.client,
            &session_id,
            &options,
            &mut original_refs,
            None,
            &sessions,
        )
        .await
        .unwrap();
        let mut refs = RefMap::new();
        let observation = take_snapshot_with_projection(
            &browser.client,
            &session_id,
            &options,
            &mut refs,
            None,
            &sessions,
        )
        .await
        .unwrap();
        assert_eq!(observation.snapshot, original);
        assert_complete_projection(&observation);
        let ref_fields = |refs: &RefMap| {
            refs.entries_sorted()
                .into_iter()
                .map(|(id, entry)| {
                    (
                        id,
                        entry.backend_node_id,
                        entry.role,
                        entry.name,
                        entry.nth,
                        entry.frame_id,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(ref_fields(&refs), ref_fields(&original_refs));
        for node in &observation.projection.nodes {
            if let Some(id) = &node.ref_id {
                let entry = refs
                    .get(id)
                    .expect("projection refs must exist in the real ref map");
                assert_eq!(entry.role, node.role);
                assert_eq!(entry.name, node.name);
            }
        }
        if selector.is_none() && depth.is_none() {
            let field = observation
                .projection
                .nodes
                .iter()
                .find(|node| node.role == "textbox")
                .unwrap();
            assert!(field.value.as_deref().unwrap().contains("[ref=e999]"));
            assert_eq!(field.states.required, Some(true));
            assert!(!observation
                .projection
                .nodes
                .iter()
                .any(|node| node.ref_id.as_deref() == Some("e999")));
            let child = observation
                .projection
                .nodes
                .iter()
                .find(|node| node.name == "Frame save")
                .unwrap();
            let mut parent_id = child.parent_id.as_ref();
            let parent = loop {
                let ancestor = observation
                    .projection
                    .nodes
                    .iter()
                    .find(|node| Some(&node.id) == parent_id)
                    .expect("frame contents must retain the emitted iframe ancestor");
                if ancestor.role == "Iframe" {
                    break ancestor;
                }
                parent_id = ancestor.parent_id.as_ref();
            };
            assert_eq!(parent.role, "Iframe");
            assert_ne!(child.frame_id, parent.frame_id);
            assert_eq!(observation.projection.coverage.observed_frame_count, 2);
            assert_eq!(observation.projection.coverage.unexpanded_frame_count, 1);
            let checkbox = observation
                .projection
                .nodes
                .iter()
                .find(|node| node.role == "checkbox")
                .unwrap();
            assert_eq!(checkbox.states.checked.as_deref(), Some("true"));
            assert_eq!(checkbox.states.disabled, Some(true));
            if !interactive && !compact {
                let child_frame = refs
                    .get(child.ref_id.as_ref().unwrap())
                    .unwrap()
                    .frame_id
                    .as_deref()
                    .unwrap();
                let scoped = take_snapshot_with_projection(
                    &browser.client,
                    &session_id,
                    &options,
                    &mut RefMap::new(),
                    Some(child_frame),
                    &sessions,
                )
                .await
                .unwrap();
                assert_complete_projection(&scoped);
                assert!(scoped.projection.coverage.unknown_ancestry);
                assert_eq!(scoped.projection.coverage.observed_frame_count, 1);
                assert_eq!(scoped.projection.coverage.unexpanded_frame_count, 1);
                assert!(scoped.projection.nodes.first().unwrap().parent_id.is_none());
            }
        }
    }
    browser.close().await.unwrap();
}
