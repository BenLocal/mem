create table if not exists graph_edges (
  from_node_id text not null,
  to_node_id   text not null,
  relation     text not null,
  valid_from   text not null,
  valid_to     text,
  primary key (from_node_id, to_node_id, relation, valid_from)
);

create index if not exists idx_graph_edges_from on graph_edges (from_node_id, relation);
create index if not exists idx_graph_edges_to on graph_edges (to_node_id, relation);
create index if not exists idx_graph_edges_history on graph_edges (from_node_id, valid_from);
