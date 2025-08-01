use std::sync::Arc;

use polars_core::frame::DataFrame;
use polars_core::prelude::{IdxSize, InitHashMaps, PlHashMap, SortMultipleOptions};
use polars_core::schema::{Schema, SchemaRef};
use polars_error::PolarsResult;
use polars_io::RowIndex;
use polars_io::cloud::CloudOptions;
use polars_ops::frame::JoinArgs;
use polars_plan::dsl::deletion::DeletionFilesList;
use polars_plan::dsl::{
    CastColumnsPolicy, JoinTypeOptionsIR, MissingColumnsPolicy, PartitionTargetCallback,
    PartitionVariantIR, ScanSources, SinkFinishCallback, SinkOptions, SinkTarget, SortColumnIR,
};
use polars_plan::plans::hive::HivePartitionsDf;
use polars_plan::plans::{AExpr, DataFrameUdf, IR};
use polars_plan::prelude::expr_ir::ExprIR;

mod fmt;
mod io;
mod lower_expr;
mod lower_group_by;
mod lower_ir;
mod to_graph;

pub use fmt::visualize_plan;
use polars_plan::prelude::FileType;
use polars_utils::arena::{Arena, Node};
use polars_utils::pl_str::PlSmallStr;
use polars_utils::plpath::PlPath;
use polars_utils::slice_enum::Slice;
use slotmap::{SecondaryMap, SlotMap};
pub use to_graph::physical_plan_to_graph;

pub use self::lower_ir::StreamingLowerIRContext;
use crate::nodes::io_sources::multi_scan::components::forbid_extra_columns::ForbidExtraColumns;
use crate::nodes::io_sources::multi_scan::components::projection::builder::ProjectionBuilder;
use crate::nodes::io_sources::multi_scan::reader_interface::builder::FileReaderBuilder;
use crate::physical_plan::lower_expr::ExprCache;

slotmap::new_key_type! {
    /// Key used for physical nodes.
    pub struct PhysNodeKey;
}

/// A node in the physical plan.
///
/// A physical plan is created when the `IR` is translated to a directed
/// acyclic graph of operations that can run on the streaming engine.
#[derive(Clone, Debug)]
pub struct PhysNode {
    output_schema: Arc<Schema>,
    kind: PhysNodeKind,
}

impl PhysNode {
    pub fn new(output_schema: Arc<Schema>, kind: PhysNodeKind) -> Self {
        Self {
            output_schema,
            kind,
        }
    }

    pub fn kind(&self) -> &PhysNodeKind {
        &self.kind
    }
}

/// A handle representing a physical stream of data with a fixed schema in the
/// physical plan. It consists of a reference to a physical node as well as the
/// output port on that node to connect to receive the stream.
#[derive(Clone, Debug, Copy, PartialEq, Eq, Hash)]
pub struct PhysStream {
    pub node: PhysNodeKey,
    pub port: usize,
}

impl PhysStream {
    #[expect(unused)]
    pub fn new(node: PhysNodeKey, port: usize) -> Self {
        Self { node, port }
    }

    // Convenience method to refer to the first output port of a physical node.
    pub fn first(node: PhysNodeKey) -> Self {
        Self { node, port: 0 }
    }
}

#[derive(Clone, Debug)]
pub enum PhysNodeKind {
    InMemorySource {
        df: Arc<DataFrame>,
    },

    Select {
        input: PhysStream,
        selectors: Vec<ExprIR>,
        extend_original: bool,
    },

    WithRowIndex {
        input: PhysStream,
        name: PlSmallStr,
        offset: Option<IdxSize>,
    },

    InputIndependentSelect {
        selectors: Vec<ExprIR>,
    },

    Reduce {
        input: PhysStream,
        exprs: Vec<ExprIR>,
    },

    StreamingSlice {
        input: PhysStream,
        offset: usize,
        length: usize,
    },

    NegativeSlice {
        input: PhysStream,
        offset: i64,
        length: usize,
    },

    DynamicSlice {
        input: PhysStream,
        offset: PhysStream,
        length: PhysStream,
    },

    Filter {
        input: PhysStream,
        predicate: ExprIR,
    },

    SimpleProjection {
        input: PhysStream,
        columns: Vec<PlSmallStr>,
    },

    InMemorySink {
        input: PhysStream,
    },

    FileSink {
        target: SinkTarget,
        sink_options: SinkOptions,
        file_type: FileType,
        input: PhysStream,
        cloud_options: Option<CloudOptions>,
    },

    PartitionSink {
        input: PhysStream,
        base_path: Arc<PlPath>,
        file_path_cb: Option<PartitionTargetCallback>,
        sink_options: SinkOptions,
        variant: PartitionVariantIR,
        file_type: FileType,
        cloud_options: Option<CloudOptions>,
        per_partition_sort_by: Option<Vec<SortColumnIR>>,
        finish_callback: Option<SinkFinishCallback>,
    },

    SinkMultiple {
        sinks: Vec<PhysNodeKey>,
    },

    /// Generic fallback for (as-of-yet) unsupported streaming mappings.
    /// Fully sinks all data to an in-memory data frame and uses the in-memory
    /// engine to perform the map.
    InMemoryMap {
        input: PhysStream,
        map: Arc<dyn DataFrameUdf>,

        /// A formatted explain of what the in-memory map. This usually calls format on the IR.
        format_str: Option<String>,
    },

    Map {
        input: PhysStream,
        map: Arc<dyn DataFrameUdf>,
    },

    Sort {
        input: PhysStream,
        by_column: Vec<ExprIR>,
        slice: Option<(i64, usize)>,
        sort_options: SortMultipleOptions,
    },

    Repeat {
        value: PhysStream,
        repeats: PhysStream,
    },

    OrderedUnion {
        inputs: Vec<PhysStream>,
    },

    Zip {
        inputs: Vec<PhysStream>,
        /// If true shorter inputs are extended with nulls to the longest input,
        /// if false all inputs must be the same length, or have length 1 in
        /// which case they are broadcast.
        null_extend: bool,
    },

    #[allow(unused)]
    Multiplexer {
        input: PhysStream,
    },

    MultiScan {
        scan_sources: ScanSources,

        file_reader_builder: Arc<dyn FileReaderBuilder>,
        cloud_options: Option<Arc<CloudOptions>>,

        /// Columns to project from the file.
        file_projection_builder: ProjectionBuilder,
        /// Final output schema of morsels being sent out of MultiScan.
        output_schema: SchemaRef,

        row_index: Option<RowIndex>,
        pre_slice: Option<Slice>,
        predicate: Option<ExprIR>,

        hive_parts: Option<HivePartitionsDf>,
        include_file_paths: Option<PlSmallStr>,
        cast_columns_policy: CastColumnsPolicy,
        missing_columns_policy: MissingColumnsPolicy,
        forbid_extra_columns: Option<ForbidExtraColumns>,

        deletion_files: Option<DeletionFilesList>,

        /// Schema of columns contained in the file. Does not contain external columns (e.g. hive / row_index).
        file_schema: SchemaRef,
    },

    #[cfg(feature = "python")]
    PythonScan {
        options: polars_plan::plans::python::PythonOptions,
    },

    GroupBy {
        input: PhysStream,
        key: Vec<ExprIR>,
        // Must be a 'simple' expression, a singular column feeding into a single aggregate, or Len.
        aggs: Vec<ExprIR>,
    },

    EquiJoin {
        input_left: PhysStream,
        input_right: PhysStream,
        left_on: Vec<ExprIR>,
        right_on: Vec<ExprIR>,
        args: JoinArgs,
    },

    SemiAntiJoin {
        input_left: PhysStream,
        input_right: PhysStream,
        left_on: Vec<ExprIR>,
        right_on: Vec<ExprIR>,
        args: JoinArgs,
        output_bool: bool,
    },

    CrossJoin {
        input_left: PhysStream,
        input_right: PhysStream,
        args: JoinArgs,
    },

    /// Generic fallback for (as-of-yet) unsupported streaming joins.
    /// Fully sinks all data to in-memory data frames and uses the in-memory
    /// engine to perform the join.
    InMemoryJoin {
        input_left: PhysStream,
        input_right: PhysStream,
        left_on: Vec<ExprIR>,
        right_on: Vec<ExprIR>,
        args: JoinArgs,
        options: Option<JoinTypeOptionsIR>,
    },

    #[cfg(feature = "merge_sorted")]
    MergeSorted {
        input_left: PhysStream,
        input_right: PhysStream,

        key: PlSmallStr,
    },
}

fn visit_node_inputs_mut(
    roots: Vec<PhysNodeKey>,
    phys_sm: &mut SlotMap<PhysNodeKey, PhysNode>,
    mut visit: impl FnMut(&mut PhysStream),
) {
    let mut to_visit = roots;
    let mut seen: SecondaryMap<PhysNodeKey, ()> =
        to_visit.iter().copied().map(|n| (n, ())).collect();
    macro_rules! rec {
        ($n:expr) => {
            let n = $n;
            if seen.insert(n, ()).is_none() {
                to_visit.push(n)
            }
        };
    }
    while let Some(node) = to_visit.pop() {
        match &mut phys_sm[node].kind {
            PhysNodeKind::InMemorySource { .. }
            | PhysNodeKind::MultiScan { .. }
            | PhysNodeKind::InputIndependentSelect { .. } => {},
            #[cfg(feature = "python")]
            PhysNodeKind::PythonScan { .. } => {},
            PhysNodeKind::Select { input, .. }
            | PhysNodeKind::WithRowIndex { input, .. }
            | PhysNodeKind::Reduce { input, .. }
            | PhysNodeKind::StreamingSlice { input, .. }
            | PhysNodeKind::NegativeSlice { input, .. }
            | PhysNodeKind::Filter { input, .. }
            | PhysNodeKind::SimpleProjection { input, .. }
            | PhysNodeKind::InMemorySink { input }
            | PhysNodeKind::FileSink { input, .. }
            | PhysNodeKind::PartitionSink { input, .. }
            | PhysNodeKind::InMemoryMap { input, .. }
            | PhysNodeKind::Map { input, .. }
            | PhysNodeKind::Sort { input, .. }
            | PhysNodeKind::Multiplexer { input }
            | PhysNodeKind::GroupBy { input, .. } => {
                rec!(input.node);
                visit(input);
            },

            PhysNodeKind::InMemoryJoin {
                input_left,
                input_right,
                ..
            }
            | PhysNodeKind::EquiJoin {
                input_left,
                input_right,
                ..
            }
            | PhysNodeKind::SemiAntiJoin {
                input_left,
                input_right,
                ..
            }
            | PhysNodeKind::CrossJoin {
                input_left,
                input_right,
                ..
            } => {
                rec!(input_left.node);
                rec!(input_right.node);
                visit(input_left);
                visit(input_right);
            },

            #[cfg(feature = "merge_sorted")]
            PhysNodeKind::MergeSorted {
                input_left,
                input_right,
                ..
            } => {
                rec!(input_left.node);
                rec!(input_right.node);
                visit(input_left);
                visit(input_right);
            },

            PhysNodeKind::DynamicSlice {
                input,
                offset,
                length,
            } => {
                rec!(input.node);
                rec!(offset.node);
                rec!(length.node);
                visit(input);
                visit(offset);
                visit(length);
            },

            PhysNodeKind::Repeat { value, repeats } => {
                rec!(value.node);
                rec!(repeats.node);
                visit(value);
                visit(repeats);
            },

            PhysNodeKind::OrderedUnion { inputs } | PhysNodeKind::Zip { inputs, .. } => {
                for input in inputs {
                    rec!(input.node);
                    visit(input);
                }
            },

            PhysNodeKind::SinkMultiple { sinks } => {
                for sink in sinks {
                    rec!(*sink);
                    visit(&mut PhysStream::first(*sink));
                }
            },
        }
    }
}

fn insert_multiplexers(roots: Vec<PhysNodeKey>, phys_sm: &mut SlotMap<PhysNodeKey, PhysNode>) {
    let mut refcount = PlHashMap::new();
    visit_node_inputs_mut(roots.clone(), phys_sm, |i| {
        *refcount.entry(*i).or_insert(0) += 1;
    });

    let mut multiplexer_map: PlHashMap<PhysStream, PhysStream> = refcount
        .into_iter()
        .filter(|(_stream, refcount)| *refcount > 1)
        .map(|(stream, _refcount)| {
            let input_schema = phys_sm[stream.node].output_schema.clone();
            let multiplexer_node = phys_sm.insert(PhysNode::new(
                input_schema,
                PhysNodeKind::Multiplexer { input: stream },
            ));
            (stream, PhysStream::first(multiplexer_node))
        })
        .collect();

    visit_node_inputs_mut(roots, phys_sm, |i| {
        if let Some(m) = multiplexer_map.get_mut(i) {
            *i = *m;
            m.port += 1;
        }
    });
}

pub fn build_physical_plan(
    root: Node,
    ir_arena: &mut Arena<IR>,
    expr_arena: &mut Arena<AExpr>,
    phys_sm: &mut SlotMap<PhysNodeKey, PhysNode>,
    ctx: StreamingLowerIRContext,
) -> PolarsResult<PhysNodeKey> {
    let mut schema_cache = PlHashMap::with_capacity(ir_arena.len());
    let mut expr_cache = ExprCache::with_capacity(expr_arena.len());
    let mut cache_nodes = PlHashMap::new();
    let phys_root = lower_ir::lower_ir(
        root,
        ir_arena,
        expr_arena,
        phys_sm,
        &mut schema_cache,
        &mut expr_cache,
        &mut cache_nodes,
        ctx,
    )?;
    insert_multiplexers(vec![phys_root.node], phys_sm);
    Ok(phys_root.node)
}
