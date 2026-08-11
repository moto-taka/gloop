use std::fmt::Write;

use crate::{CompiledGraph, EdgeKind, NodeKind};

impl CompiledGraph {
    pub fn explain(&self) -> String {
        let mut output = String::new();
        let graph = &self.graph;
        let _ = writeln!(output, "Graph: {}", graph.metadata.name);
        let _ = writeln!(output, "Goal: {}", graph.spec.goal);
        let _ = writeln!(
            output,
            "Policy: max_parallel={}, failure={:?}",
            graph.spec.policies.max_parallel, graph.spec.policies.failure
        );
        let _ = writeln!(output, "Execution order:");
        for (position, id) in self.order.iter().enumerate() {
            let node = self.node(id).expect("compiled node exists");
            let incoming: Vec<_> = self
                .incoming_edges(id)
                .map(|edge| format!("{}:{:?}", edge.from, edge.kind))
                .collect();
            let _ = writeln!(
                output,
                "  {}. {} ({}){}",
                position + 1,
                id,
                node_kind_name(&node.kind),
                if incoming.is_empty() {
                    String::new()
                } else {
                    format!(" <- {}", incoming.join(", "))
                }
            );
        }
        output
    }

    pub fn render_mermaid(&self) -> String {
        let mut output = String::from("flowchart TD\n");
        for node in &self.graph.spec.nodes {
            let label = node.label.as_deref().unwrap_or(node.id.as_str());
            let label = mermaid_escape(label);
            let shape = match node.kind {
                NodeKind::Gate { .. } => format!("{{\"{label}\"}}"),
                NodeKind::Loop { .. } => format!("[[\"{label}\"]]"),
                NodeKind::Verify { .. } => format!("([\"{label}\"] )"),
                _ => format!("[\"{label}\"]"),
            };
            let _ = writeln!(output, "    {}{}", mermaid_id(&node.id), shape);
        }
        for edge in &self.graph.spec.edges {
            let arrow = match edge.kind {
                EdgeKind::Data => "-->|data|",
                EdgeKind::Control => "-->",
                EdgeKind::Resource => "-.->|resource|",
                EdgeKind::Conditional => "-->|when|",
                EdgeKind::Failure => "-->|failure|",
            };
            let _ = writeln!(
                output,
                "    {} {} {}",
                mermaid_id(&edge.from),
                arrow,
                mermaid_id(&edge.to)
            );
        }
        output
    }

    pub fn render_dot(&self) -> String {
        let mut output = String::from("digraph gloop {\n  rankdir=LR;\n");
        for node in &self.graph.spec.nodes {
            let label = node.label.as_deref().unwrap_or(node.id.as_str());
            let id = dot_escape(node.id.as_str());
            let label = dot_escape(label);
            let _ = writeln!(output, r#"  "{id}" [label="{label}"];"#);
        }
        for edge in &self.graph.spec.edges {
            let edge_label = dot_escape(&format!("{0:?}", edge.kind));
            let _ = writeln!(
                output,
                "  \"{}\" -> \"{}\" [label=\"{}\"];",
                dot_escape(edge.from.as_str()),
                dot_escape(edge.to.as_str()),
                edge_label,
            );
        }
        output.push_str("}\n");
        output
    }
}

fn dot_escape(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => output.push(ch),
        }
    }
    output
}

fn mermaid_escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '"' => output.push_str("&quot;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\\' => output.push_str("&#92;"),
            '`' => output.push_str("&#96;"),
            '\n' | '\r' | '\t' => output.push(' '),
            character if character.is_control() => output.push(' '),
            character => output.push(character),
        }
    }
    output
}

fn node_kind_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Agent { .. } => "agent",
        NodeKind::Command { .. } => "command",
        NodeKind::Reduce { .. } => "reduce",
        NodeKind::Verify { .. } => "verify",
        NodeKind::Synthesize { .. } => "synthesize",
        NodeKind::Gate { .. } => "gate",
        NodeKind::Loop { .. } => "loop",
        NodeKind::Subgraph { .. } => "subgraph",
    }
}

fn mermaid_id(value: &str) -> String {
    format!("n_{}", value.replace('-', "_"))
}
