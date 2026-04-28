use mem::domain::memory::GraphEdge;

#[test]
fn graph_edge_carries_valid_from_and_valid_to() {
    let edge = GraphEdge {
        from_node_id: "memory:abc".into(),
        to_node_id: "project:foo".into(),
        relation: "applies_to".into(),
        valid_from: "00000001761662918634".into(),
        valid_to: None,
    };
    assert_eq!(edge.valid_to, None);
    assert!(edge.valid_from.starts_with("000000"));

    let s = serde_json::to_string(&edge).unwrap();
    let back: GraphEdge = serde_json::from_str(&s).unwrap();
    assert_eq!(back.valid_to, None);
    assert_eq!(back.valid_from, "00000001761662918634");
}
