use crate::{
    dataflow::operators::{CrossbeamExtractor, CrossbeamPusher},
    vsdg::{
        node::{Constant, FuncId, Function, Node, NodeExt, NodeId, Operation, Value},
        Edge, ProgramGraph,
    },
};
use abomonation_derive::Abomonation;
use crossbeam_channel::{Receiver, Sender};
use differential_dataflow::{difference::Semigroup, lattice::Lattice, ExchangeData};
use petgraph::{dot::Dot, Graph};
use std::{
    cmp::Ordering,
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use timely::dataflow::{
    operators::{
        capture::{Event, EventPusher},
        Capture, Concatenate, Map,
    },
    Scope, ScopeParent,
};

type RenderSender<S, R> = Sender<
    Event<<S as ScopeParent>::Timestamp, (String, (GraphNode, <S as ScopeParent>::Timestamp, R))>,
>;

impl<S, R> ProgramGraph<S, R>
where
    S: Scope,
    R: Semigroup,
{
    // TODO: Work this out better than returning a join handle and allow naming the graph
    pub fn render_graph<N>(&self, name: N, sender: RenderSender<S, R>) -> Self
    where
        N: Into<String>,
        S::Timestamp: Lattice,
        R: ExchangeData,
    {
        self.scope().region_named("debug program graph", |region| {
            self.enter_region(region)
                .consolidate()
                .capture_into(name.into(), CrossbeamPusher::new(sender));
        });

        self.clone()
    }

    pub fn capture_into<P>(&self, name: String, pusher: P)
    where
        P: EventPusher<S::Timestamp, (String, (GraphNode, S::Timestamp, R))> + Clone + 'static,
    {
        self.value_edges
            .map(GraphNode::ValueEdge)
            .concatenate(vec![
                self.effect_edges.map(GraphNode::EffectEdge),
                self.control_edges.map(GraphNode::ControlEdge),
                self.nodes.map(GraphNode::Node),
                self.functions.map(GraphNode::Function),
                self.function_nodes.map(GraphNode::FunctionNode),
            ])
            .inner
            .map(move |node| (name.clone(), node))
            .capture_into(pusher.clone());
    }
}

pub fn render_graph<T, R>(receiver: Receiver<Event<T, (String, (GraphNode, T, R))>>) {
    let mut graphs = HashMap::new();
    for event in CrossbeamExtractor::new(receiver) {
        if let Event::Messages(_, data) = event {
            for (graph_name, node) in data {
                graphs
                    .entry(graph_name)
                    .or_insert_with(|| Vec::with_capacity(512))
                    .push(node);
            }
        }
    }

    for (graph_name, mut graph_data) in graphs {
        graph_data.sort_by(|(a, _, _), (b, _, _)| {
            match (
                matches!(a, GraphNode::Node(_)),
                matches!(b, GraphNode::Node(_)),
            ) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => Ordering::Equal,
            }
        });

        let (mut graph, mut node_ids) = (Graph::new(), HashMap::new());
        for (node, _time, _diff) in graph_data {
            match node {
                GraphNode::ValueEdge((src, dest)) => {
                    let src = *node_ids.get(&src).unwrap();
                    let dest = *node_ids.get(&dest).unwrap();

                    graph.add_edge(src, dest, EdgeKind::Value);
                }

                GraphNode::EffectEdge((src, dest)) => {
                    let src = *node_ids.get(&src).unwrap();
                    let dest = *node_ids.get(&dest).unwrap();

                    graph.add_edge(src, dest, EdgeKind::Effect);
                }

                GraphNode::ControlEdge((src, dest)) => {
                    let src = *node_ids.get(&src).unwrap();
                    let dest = *node_ids.get(&dest).unwrap();

                    graph.add_edge(src, dest, EdgeKind::Control);
                }

                GraphNode::Node((node_id, node)) => {
                    let graph_id = graph.add_node(node);
                    node_ids.insert(node_id, graph_id);
                }

                // TODO
                GraphNode::FunctionNode(_) | GraphNode::Function(_) => {}
            }
        }

        let dot = Dot::with_attr_getters(
            &graph,
            &[
                petgraph::dot::Config::EdgeNoLabel,
                petgraph::dot::Config::NodeNoLabel,
            ],
            &|_graph, edge| {
                match edge.weight() {
                    EdgeKind::Control => "color = black",
                    EdgeKind::Effect => "color = cornflowerblue",
                    EdgeKind::Value => "color = forestgreen",
                }
                .to_owned()
            },
            &|_graph, (_idx, node)| match node {
                Node::Value(value) => match value {
                    Value::Constant(constant) => match constant {
                        Constant::Uint8(uint8) => {
                            format!("label = \"{}: u8\", shape = circle", uint8)
                        }
                        Constant::Bool(b) => {
                            format!("label = \"{}: bool\", shape = circle", b)
                        }
                    },
                    Value::Parameter(param) => {
                        format!("label = \"param: {}\", shape = doublecircle", param.ty)
                    }
                },
                Node::Control(control) => {
                    format!("label = \"{}\", shape = diamond", control.node_name())
                }
                Node::Operation(operation) => {
                    format!("label = \"{}\", shape = box", operation.node_name())
                }
                Node::End(_) | Node::Start(_) | Node::Merge(_) => {
                    format!(
                        "label = \"{}\", shape = box, peripheries = 2",
                        node.node_name(),
                    )
                }
            },
        );

        let system_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("target/debug/{}.dot", graph_name);

        {
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .open(&name)
                .unwrap();

            file.write_all(format!("{:?}", dot).as_bytes()).unwrap();
        }

        Command::new("dot")
            .arg(&name)
            .args(&["-Tpng", "-o"])
            .arg(name.replace(".dot", ".png"))
            .status()
            .unwrap();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Abomonation)]
pub enum GraphNode {
    ValueEdge(Edge),
    EffectEdge(Edge),
    ControlEdge(Edge),
    Node((NodeId, Node)),
    FunctionNode((NodeId, FuncId)),
    Function((FuncId, Function)),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Abomonation)]
pub enum EdgeKind {
    Value,
    Effect,
    Control,
}

// A hack because `petgraph::Dot` requires it
#[doc(hidden)]
impl std::fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}
