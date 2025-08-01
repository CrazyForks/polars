use std::sync::Arc;

use crossbeam_channel::Sender;
use polars_core::POOL;
use polars_core::frame::DataFrame;
use polars_error::PolarsResult;
use polars_expr::state::ExecutionState;
use polars_utils::aliases::PlHashSet;
use polars_utils::relaxed_cell::RelaxedCell;
use slotmap::{SecondaryMap, SparseSecondaryMap};
use tokio::task::JoinHandle;

use crate::async_executor;
use crate::graph::{Graph, GraphNode, GraphNodeKey, LogicalPipeKey, PortState};
use crate::pipe::PhysicalPipe;

#[derive(Clone)]
pub struct StreamingExecutionState {
    /// The number of parallel pipelines we have within each stream.
    pub num_pipelines: usize,

    /// The ExecutionState passed to any non-streaming operations.
    pub in_memory_exec_state: ExecutionState,

    query_tasks_send: Sender<JoinHandle<PolarsResult<()>>>,
    subphase_tasks_send: Sender<JoinHandle<PolarsResult<()>>>,
}

impl StreamingExecutionState {
    /// Spawns a task which is awaited at the end of the query.
    #[expect(unused)]
    pub fn spawn_query_task<F: Future<Output = PolarsResult<()>> + Send + 'static>(&self, fut: F) {
        self.query_tasks_send.send(tokio::task::spawn(fut)).unwrap();
    }

    /// Spawns a task which is awaited at the end of the current subphase. That is
    /// if called inside `update_state` it is awaited after the state update, and
    /// if called inside `spawn` it is awaited after the execution of that phase is
    /// complete.
    #[expect(unused)]
    pub fn spawn_subphase_task<F: Future<Output = PolarsResult<()>> + Send + 'static>(
        &self,
        fut: F,
    ) {
        self.subphase_tasks_send
            .send(tokio::task::spawn(fut))
            .unwrap();
    }
}

/// Finds all runnable pipeline blockers in the graph, that is, nodes which:
///  - Only have blocked output ports.
///  - Have at least one ready input port connected to a ready output port.
fn find_runnable_pipeline_blockers(graph: &Graph) -> Vec<GraphNodeKey> {
    let mut blockers = Vec::new();
    for (node_key, node) in graph.nodes.iter() {
        // TODO: how does the multiplexer fit into this?
        let only_has_blocked_outputs = node
            .outputs
            .iter()
            .all(|o| graph.pipes[*o].send_state == PortState::Blocked);
        if !only_has_blocked_outputs {
            continue;
        }

        let has_input_ready = node.inputs.iter().any(|i| {
            graph.pipes[*i].send_state == PortState::Ready
                && graph.pipes[*i].recv_state == PortState::Ready
        });
        if has_input_ready {
            blockers.push(node_key);
        }
    }
    blockers
}

/// Given a set of nodes expand this set with all nodes which are inputs to the
/// set and whose connecting pipe is ready on both sides, recursively.
///
/// Returns the set of nodes as well as the pipes connecting them.
fn expand_ready_subgraph(
    graph: &Graph,
    mut nodes: Vec<GraphNodeKey>,
) -> (PlHashSet<GraphNodeKey>, Vec<LogicalPipeKey>) {
    let mut in_subgraph: PlHashSet<GraphNodeKey> = nodes.iter().copied().collect();
    let mut pipes = Vec::with_capacity(nodes.len());
    while let Some(node_key) = nodes.pop() {
        let node = &graph.nodes[node_key];
        for input_pipe_key in &node.inputs {
            let input_pipe = &graph.pipes[*input_pipe_key];
            if input_pipe.send_state == PortState::Ready
                && input_pipe.recv_state == PortState::Ready
            {
                pipes.push(*input_pipe_key);
                if in_subgraph.insert(input_pipe.sender) {
                    nodes.push(input_pipe.sender);
                }
            }
        }
    }

    (in_subgraph, pipes)
}

/// Finds a part of the graph which we can run.
fn find_runnable_subgraph(graph: &mut Graph) -> (PlHashSet<GraphNodeKey>, Vec<LogicalPipeKey>) {
    // Find pipeline blockers, choose a subset with at most one memory intensive
    // pipeline blocker, and return the subgraph needed to feed them.
    let blockers = find_runnable_pipeline_blockers(graph);
    let (mut expensive, cheap): (Vec<_>, Vec<_>) = blockers.into_iter().partition(|b| {
        graph.nodes[*b]
            .compute
            .is_memory_intensive_pipeline_blocker()
    });

    // TODO: choose which expensive pipeline blocker to run more intelligently.
    expensive.sort_by_key(|node_key| {
        // Prefer to run nodes whose outputs are ready to be consumed.
        // outputs_ready_to_receive
        graph.nodes[*node_key]
            .outputs
            .iter()
            .filter(|o| graph.pipes[**o].recv_state == PortState::Ready)
            .count()
    });

    let mut to_run = cheap;
    if let Some(node) = expensive.pop() {
        to_run.push(node);
    }
    expand_ready_subgraph(graph, to_run)
}

/// Re-uses the memory for a vec while clearing it. Allows casting the type of
/// the vec at the same time. The stdlib specializes collect() to re-use the
/// memory.
fn reuse_vec<T, U>(v: Vec<T>) -> Vec<U> {
    v.into_iter().filter_map(|_| None).collect()
}

/// Runs the given subgraph. Assumes the set of pipes is correct for the subgraph.
fn run_subgraph(
    graph: &mut Graph,
    nodes: &PlHashSet<GraphNodeKey>,
    pipes: &[LogicalPipeKey],
    pipe_seq_offsets: &mut SecondaryMap<LogicalPipeKey, Arc<RelaxedCell<u64>>>,
    state: &StreamingExecutionState,
) -> PolarsResult<()> {
    // Construct physical pipes for the logical pipes we'll use.
    let mut physical_pipes = SecondaryMap::new();
    for pipe_key in pipes.iter().copied() {
        let seq_offset = pipe_seq_offsets
            .entry(pipe_key)
            .unwrap()
            .or_default()
            .clone();
        physical_pipes.insert(pipe_key, PhysicalPipe::new(state.num_pipelines, seq_offset));
    }

    // We do a topological sort of the graph: we want to spawn each node,
    // starting with the sinks and moving backwards. This order is important
    // for the initialization of physical pipes - the receive port must be
    // initialized first.
    let mut ready = Vec::new();
    let mut num_send_ports_not_yet_ready = SecondaryMap::new();
    for node_key in nodes {
        let node = &graph.nodes[*node_key];
        let num_outputs_in_subgraph = node
            .outputs
            .iter()
            .filter(|o| physical_pipes.contains_key(**o))
            .count();
        num_send_ports_not_yet_ready.insert(*node_key, num_outputs_in_subgraph);
        if num_outputs_in_subgraph == 0 {
            ready.push(*node_key);
        }
    }

    async_executor::task_scope(|scope| {
        // Using SlotMap::iter_mut we can get simultaneous mutable references. By storing them and
        // removing the references from the secondary map as we do our topological sort we ensure
        // they are unique.
        let mut node_refs: SecondaryMap<GraphNodeKey, &mut GraphNode> =
            graph.nodes.iter_mut().collect();

        // Initialize tasks.
        let mut join_handles = Vec::new();
        let mut input_pipes = Vec::new();
        let mut output_pipes = Vec::new();
        let mut recv_ports = Vec::new();
        let mut send_ports = Vec::new();
        while let Some(node_key) = ready.pop() {
            let node = node_refs.remove(node_key).unwrap();

            // Temporarily remove the physical pipes from the SecondaryMap so that we can mutably
            // borrow them simultaneously.
            for input in &node.inputs {
                input_pipes.push(physical_pipes.remove(*input));
            }
            for output in &node.outputs {
                output_pipes.push(physical_pipes.remove(*output));
            }

            // Construct the receive/send ports.
            for input_pipe in &mut input_pipes {
                recv_ports.push(input_pipe.as_mut().map(|p| p.recv_port()));
            }
            for output_pipe in &mut output_pipes {
                send_ports.push(output_pipe.as_mut().map(|p| p.send_port()));
            }

            // Spawn a task per pipeline.
            node.compute.spawn(
                scope,
                &mut recv_ports[..],
                &mut send_ports[..],
                state,
                &mut join_handles,
            );

            // Ensure the ports were consumed.
            assert!(recv_ports.iter().all(|p| p.is_none()));
            assert!(send_ports.iter().all(|p| p.is_none()));

            // Reuse the port vectors, clearing the borrow it has on input_/output_pipes.
            recv_ports = reuse_vec(recv_ports);
            send_ports = reuse_vec(send_ports);

            // Re-insert the physical pipes into the SecondaryMap.
            for (input, input_pipe) in node.inputs.iter().zip(input_pipes.drain(..)) {
                if let Some(pipe) = input_pipe {
                    physical_pipes.insert(*input, pipe);

                    // For all the receive ports we just initialized inside spawn(), decrement
                    // the num_send_ports_not_yet_ready for the node it was connected to and mark
                    // the node as ready to spawn if all its send ports are connected to
                    // initialized recv ports.
                    let sender = graph.pipes[*input].sender;
                    if let Some(count) = num_send_ports_not_yet_ready.get_mut(sender) {
                        if *count > 0 {
                            *count -= 1;
                            if *count == 0 {
                                ready.push(sender);
                            }
                        }
                    }
                }
            }
            for (output, output_pipe) in node.outputs.iter().zip(output_pipes.drain(..)) {
                if let Some(pipe) = output_pipe {
                    physical_pipes.insert(*output, pipe);
                }
            }

            // Reuse the pipe vectors, clearing the borrow it has for next iteration.
            input_pipes = reuse_vec(input_pipes);
            output_pipes = reuse_vec(output_pipes);
        }

        // Spawn tasks for all the physical pipes (no-op on most, but needed for
        // those with distributors or linearizers).
        for pipe in physical_pipes.values_mut() {
            pipe.spawn(scope, &mut join_handles);
        }

        // Wait until all tasks are done.
        // Only now do we turn on/off wait statistics tracking to reduce noise
        // from task startup.
        if std::env::var("POLARS_TRACK_WAIT_STATS").as_deref() == Ok("1") {
            async_executor::track_task_wait_statistics(true);
        }
        let ret = polars_io::pl_async::get_runtime().block_on(async move {
            for handle in join_handles {
                handle.await?;
            }
            PolarsResult::Ok(())
        });
        if std::env::var("POLARS_TRACK_WAIT_STATS").as_deref() == Ok("1") {
            async_executor::track_task_wait_statistics(false);
        }
        ret
    })?;

    Ok(())
}

pub fn execute_graph(
    graph: &mut Graph,
) -> PolarsResult<SparseSecondaryMap<GraphNodeKey, DataFrame>> {
    // Get the number of threads from the rayon thread-pool as that respects our config.
    let num_pipelines = POOL.current_num_threads();
    async_executor::set_num_threads(num_pipelines);

    let (query_tasks_send, query_tasks_recv) = crossbeam_channel::unbounded();
    let (subphase_tasks_send, subphase_tasks_recv) = crossbeam_channel::unbounded();

    let state = StreamingExecutionState {
        num_pipelines,
        in_memory_exec_state: ExecutionState::default(),
        query_tasks_send,
        subphase_tasks_send,
    };

    // Ensure everything is properly connected.
    for (node_key, node) in &graph.nodes {
        for (i, input) in node.inputs.iter().enumerate() {
            assert!(graph.pipes[*input].receiver == node_key);
            assert!(graph.pipes[*input].recv_port == i);
        }
        for (i, output) in node.outputs.iter().enumerate() {
            assert!(graph.pipes[*output].sender == node_key);
            assert!(graph.pipes[*output].send_port == i);
        }
    }

    let mut pipe_seq_offsets = SecondaryMap::new();
    loop {
        // Update the states.
        if polars_core::config::verbose() {
            eprintln!("polars-stream: updating graph state");
        }
        graph.update_all_states(&state)?;
        polars_io::pl_async::get_runtime().block_on(async {
            while let Ok(handle) = subphase_tasks_recv.try_recv() {
                handle.await.unwrap()?;
            }
            PolarsResult::Ok(())
        })?;

        // Find a subgraph to run.
        let (nodes, pipes) = find_runnable_subgraph(graph);
        if polars_core::config::verbose() {
            for node in &nodes {
                eprintln!(
                    "polars-stream: running {} in subgraph",
                    graph.nodes[*node].compute.name()
                );
            }
        }

        if nodes.is_empty() {
            break;
        }

        // Run the subgraph until phase completion.
        run_subgraph(graph, &nodes, &pipes, &mut pipe_seq_offsets, &state)?;
        polars_io::pl_async::get_runtime().block_on(async {
            while let Ok(handle) = subphase_tasks_recv.try_recv() {
                handle.await.unwrap()?;
            }
            PolarsResult::Ok(())
        })?;
        if polars_core::config::verbose() {
            eprintln!("polars-stream: done running graph phase");
        }
    }

    // Ensure everything is done.
    for pipe in graph.pipes.values() {
        assert!(pipe.send_state == PortState::Done && pipe.recv_state == PortState::Done);
    }

    // Finalize query tasks.
    polars_io::pl_async::get_runtime().block_on(async {
        while let Ok(handle) = query_tasks_recv.try_recv() {
            handle.await.unwrap()?;
        }
        PolarsResult::Ok(())
    })?;

    // Extract output from in-memory nodes.
    let mut out = SparseSecondaryMap::new();
    for (node_key, node) in graph.nodes.iter_mut() {
        if let Some(df) = node.compute.get_output()? {
            out.insert(node_key, df);
        }
    }

    Ok(out)
}
