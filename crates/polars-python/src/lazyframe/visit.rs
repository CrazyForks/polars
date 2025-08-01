use std::sync::{Arc, Mutex};

use polars::prelude::PolarsError;
use polars::prelude::python_dsl::PythonScanSource;
use polars_plan::plans::{Context, ExprToIRContext, IR, to_expr_ir};
use polars_plan::prelude::expr_ir::ExprIR;
use polars_plan::prelude::{AExpr, PythonOptions};
use polars_utils::arena::{Arena, Node};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use super::PyLazyFrame;
use super::visitor::{expr_nodes, nodes};
use crate::error::PyPolarsErr;
use crate::{PyExpr, Wrap, raise_err};

#[derive(Clone)]
#[pyclass]
pub struct PyExprIR {
    #[pyo3(get)]
    node: usize,
    #[pyo3(get)]
    output_name: String,
}

impl From<ExprIR> for PyExprIR {
    fn from(value: ExprIR) -> Self {
        Self {
            node: value.node().0,
            output_name: value.output_name().to_string(),
        }
    }
}

impl From<&ExprIR> for PyExprIR {
    fn from(value: &ExprIR) -> Self {
        Self {
            node: value.node().0,
            output_name: value.output_name().to_string(),
        }
    }
}

type Version = (u16, u16);

#[pyclass]
pub struct NodeTraverser {
    root: Node,
    lp_arena: Arc<Mutex<Arena<IR>>>,
    expr_arena: Arc<Mutex<Arena<AExpr>>>,
    scratch: Vec<Node>,
    expr_scratch: Vec<ExprIR>,
    expr_mapping: Option<Vec<Node>>,
}

impl NodeTraverser {
    // Versioning for IR, (major, minor)
    // Increment major on breaking changes to the IR (e.g. renaming
    // fields, reordering tuples), minor on backwards compatible
    // changes (e.g. exposing a new expression node).
    const VERSION: Version = (9, 0);

    pub fn new(root: Node, lp_arena: Arena<IR>, expr_arena: Arena<AExpr>) -> Self {
        Self {
            root,
            lp_arena: Arc::new(Mutex::new(lp_arena)),
            expr_arena: Arc::new(Mutex::new(expr_arena)),
            scratch: vec![],
            expr_scratch: vec![],
            expr_mapping: None,
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn get_arenas(&self) -> (Arc<Mutex<Arena<IR>>>, Arc<Mutex<Arena<AExpr>>>) {
        (self.lp_arena.clone(), self.expr_arena.clone())
    }

    fn fill_inputs(&mut self) {
        let lp_arena = self.lp_arena.lock().unwrap();
        let this_node = lp_arena.get(self.root);
        self.scratch.clear();
        this_node.copy_inputs(&mut self.scratch);
    }

    fn fill_expressions(&mut self) {
        let lp_arena = self.lp_arena.lock().unwrap();
        let this_node = lp_arena.get(self.root);
        self.expr_scratch.clear();
        this_node.copy_exprs(&mut self.expr_scratch);
    }

    fn scratch_to_list<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(py, self.scratch.drain(..).map(|node| node.0))
    }

    fn expr_to_list<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(
            py,
            self.expr_scratch
                .drain(..)
                .map(|e| PyExprIR::from(e).into_pyobject(py).unwrap()),
        )
    }
}

#[pymethods]
impl NodeTraverser {
    /// Get expression nodes
    fn get_exprs<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        self.fill_expressions();
        self.expr_to_list(py)
    }

    /// Get input nodes
    fn get_inputs<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        self.fill_inputs();
        self.scratch_to_list(py)
    }

    /// The current version of the IR
    fn version(&self) -> Version {
        NodeTraverser::VERSION
    }

    /// Get Schema of current node as python dict<str, pl.DataType>
    fn get_schema<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let lp_arena = self.lp_arena.lock().unwrap();
        let schema = lp_arena.get(self.root).schema(&lp_arena);
        Wrap((**schema).clone()).into_pyobject(py)
    }

    /// Get expression dtype of expr_node, the schema used is that of the current root node
    fn get_dtype<'py>(&self, expr_node: usize, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let expr_node = Node(expr_node);
        let lp_arena = self.lp_arena.lock().unwrap();
        let schema = lp_arena.get(self.root).schema(&lp_arena);
        let expr_arena = self.expr_arena.lock().unwrap();
        let field = expr_arena
            .get(expr_node)
            .to_field(&schema, Context::Default, &expr_arena)
            .map_err(PyPolarsErr::from)?;
        Wrap(field.dtype).into_pyobject(py)
    }

    /// Set the current node in the plan.
    fn set_node(&mut self, node: usize) {
        self.root = Node(node);
    }

    /// Get the current node in the plan.
    fn get_node(&mut self) -> usize {
        self.root.0
    }

    /// Set a python UDF that will replace the subtree location with this function src.
    fn set_udf(&mut self, function: PyObject) {
        let mut lp_arena = self.lp_arena.lock().unwrap();
        let schema = lp_arena.get(self.root).schema(&lp_arena).into_owned();
        let ir = IR::PythonScan {
            options: PythonOptions {
                scan_fn: Some(function.into()),
                schema,
                output_schema: None,
                with_columns: None,
                python_source: PythonScanSource::Cuda,
                predicate: Default::default(),
                n_rows: None,
                validate_schema: false,
            },
        };
        lp_arena.replace(self.root, ir);
    }

    fn view_current_node(&self, py: Python<'_>) -> PyResult<PyObject> {
        let lp_arena = self.lp_arena.lock().unwrap();
        let lp_node = lp_arena.get(self.root);
        nodes::into_py(py, lp_node)
    }

    fn view_expression(&self, py: Python<'_>, node: usize) -> PyResult<PyObject> {
        let expr_arena = self.expr_arena.lock().unwrap();
        let n = match &self.expr_mapping {
            Some(mapping) => *mapping.get(node).unwrap(),
            None => Node(node),
        };
        let expr = expr_arena.get(n);
        expr_nodes::into_py(py, expr)
    }

    /// Add some expressions to the arena and return their new node ids as well
    /// as the total number of nodes in the arena.
    fn add_expressions(&mut self, expressions: Vec<PyExpr>) -> PyResult<(Vec<usize>, usize)> {
        let lp_arena = self.lp_arena.lock().unwrap();
        let schema = lp_arena.get(self.root).schema(&lp_arena);
        let mut expr_arena = self.expr_arena.lock().unwrap();
        Ok((
            expressions
                .into_iter()
                .map(|e| {
                    let mut ctx = ExprToIRContext::new(&mut expr_arena, &schema);
                    ctx.allow_unknown = true;
                    // NOTE: Probably throwing away the output names here is not okay?
                    to_expr_ir(e.inner, &mut ctx)
                        .map_err(PyPolarsErr::from)
                        .map(|v| v.node().0)
                })
                .collect::<Result<_, PyPolarsErr>>()?,
            expr_arena.len(),
        ))
    }

    /// Set up a mapping of expression nodes used in `view_expression_node``.
    /// With a mapping set, `view_expression_node(i)` produces the node for
    /// `mapping[i]`.
    fn set_expr_mapping(&mut self, mapping: Vec<usize>) -> PyResult<()> {
        if mapping.len() != self.expr_arena.lock().unwrap().len() {
            raise_err!("Invalid mapping length", ComputeError);
        }
        self.expr_mapping = Some(mapping.into_iter().map(Node).collect());
        Ok(())
    }

    /// Unset the expression mapping (reinstates the identity map)
    fn unset_expr_mapping(&mut self) {
        self.expr_mapping = None;
    }
}

#[pymethods]
#[allow(clippy::should_implement_trait)]
impl PyLazyFrame {
    fn visit(&self) -> PyResult<NodeTraverser> {
        let mut lp_arena = Arena::with_capacity(16);
        let mut expr_arena = Arena::with_capacity(16);
        let root = self
            .ldf
            .clone()
            .optimize(&mut lp_arena, &mut expr_arena)
            .map_err(PyPolarsErr::from)?;
        Ok(NodeTraverser {
            root,
            lp_arena: Arc::new(Mutex::new(lp_arena)),
            expr_arena: Arc::new(Mutex::new(expr_arena)),
            scratch: vec![],
            expr_scratch: vec![],
            expr_mapping: None,
        })
    }
}
