//! GPU lowering for the relational engine (feature-gated).
//!
//! This module does **not** execute anything on a GPU. It lowers the engine's
//! vectorized scans into the stable TPTIR text dialect via [`tpt_gpu_ir_spec`],
//! the canonical emitter shared across the TPT compute suite. The emitted text
//! is what an external GPU backend (tpt-gpu, tpt-crucible, …) would consume;
//! this crate only produces it.
//!
//! Two shapes are lowered today:
//!
//! - [`lower_topk`] / [`emit_topk`] — a vectorized top-k similarity scan
//!   (`executor::vector_topk`) reduced to the max similarity.
//! - [`lower_aggregate`] / [`emit_aggregate`] — a single-column aggregate
//!   (`SUM`/`MIN`/`MAX`/`AVG`/`COUNT`) reduced over the column.
//!
//! [`emit_for_plan`] picks the right shape for a planned query so `EXPLAIN`'s
//! GPU view shows the emitted IR for the dominant operation, not just top-k.
//!
//! The `gpu` feature on the engine only enables *emission*. The CPU path
//! (`vector_topk`, the executor aggregates) remains the runtime fallback, and
//! the planner still only dispatches to `Dispatch::Gpu` when built with this
//! feature and the scan is large enough (`planner::GPU_ROW_THRESHOLD`).

use tpt_gpu_ir_spec::{
    text::{emit, Block, EmitOptions, Instruction, Region},
    types::{AddressSpace, ElemType, Type},
    Op,
};

use crate::parser::AggregateFunc;
use crate::planner::{Plan, PlanNode};

/// Load a column memref into a tensor with the conventional `%vals` result.
fn load_col(n: u64) -> (Type, Type, Instruction) {
    let f32_ty = Type::scalar(ElemType::F32);
    let mem = Type::memref(vec![n as i64], f32_ty.clone(), AddressSpace::Global);
    let tensor = Type::tensor(vec![n as i64], f32_ty, AddressSpace::Global);
    let instr = Instruction {
        result: Some("vals".to_string()),
        op: Op::Load,
        operands: vec!["col".to_string()],
        attrs: vec![],
        result_type: Some(tensor.clone()),
    };
    (mem, tensor, instr)
}

/// A single reduced result: `%<result> = <op> %vals : <ty>`.
fn reduce(result: &str, op: Op, ty: Type) -> Instruction {
    Instruction {
        result: Some(result.to_string()),
        op,
        operands: vec!["vals".to_string()],
        attrs: vec![],
        result_type: Some(ty),
    }
}

/// `return %<val>`.
fn ret(val: &str) -> Instruction {
    Instruction {
        result: None,
        op: Op::Return,
        operands: vec![val.to_string()],
        attrs: vec![],
        result_type: None,
    }
}

/// Lower a vectorized top-k similarity scan over an `[n x f32]` embedding
/// memref into a TPTIR [`Region`]: load the embeddings, reduce to the max
/// similarity, and return it. `n` is the embedding table length.
pub fn lower_topk(n: u64) -> Region {
    let (mem, _tensor, load) = load_col(n);
    let f32_ty = Type::scalar(ElemType::F32);
    Region {
        name: "vector_topk".to_string(),
        args: vec![("embeddings".to_string(), mem)],
        return_types: vec![f32_ty.clone()],
        blocks: vec![Block {
            label: "entry".to_string(),
            instructions: vec![
                load,
                reduce("best", Op::ReduceMax, f32_ty.clone()),
                ret("best"),
            ],
        }],
    }
}

/// Emit the TPTIR text for a top-k scan of `n` embeddings.
pub fn emit_topk(n: u64) -> String {
    emit(&lower_topk(n), &EmitOptions::default())
}

/// Lower a single-column aggregate over an `[n x f32]` column into a TPTIR
/// [`Region`].
///
/// The lowering maps each [`AggregateFunc`] to the dialect op the engine would
/// target:
///
/// - `SUM` → `reduce_sum`
/// - `MIN` → `reduce_min`
/// - `MAX` → `reduce_max`
/// - `AVG` → `reduce_sum` + `divf` (by the row count)
/// - `COUNT` → `count` (a [`Op::Custom`] escape — *not* `reduce_sum`, which
///   would silently sum the values instead of counting rows)
///
/// `n` is the number of rows (the column length).
pub fn lower_aggregate(func: AggregateFunc, n: u64) -> Region {
    let (mem, _tensor, load) = load_col(n);
    let f32_ty = Type::scalar(ElemType::F32);
    let i64_ty = Type::scalar(ElemType::I64);

    let (name, ret_ty, tail) = match func {
        AggregateFunc::Sum => (
            "aggregate_sum",
            f32_ty.clone(),
            vec![reduce("sum", Op::ReduceSum, f32_ty.clone()), ret("sum")],
        ),
        AggregateFunc::Min => (
            "aggregate_min",
            f32_ty.clone(),
            vec![reduce("min", Op::ReduceMin, f32_ty.clone()), ret("min")],
        ),
        AggregateFunc::Max => (
            "aggregate_max",
            f32_ty.clone(),
            vec![reduce("max", Op::ReduceMax, f32_ty.clone()), ret("max")],
        ),
        AggregateFunc::Count => (
            "aggregate_count",
            i64_ty.clone(),
            vec![
                Instruction {
                    result: Some("cnt".to_string()),
                    op: Op::Custom("count".to_string()),
                    operands: vec!["vals".to_string()],
                    attrs: vec![],
                    result_type: Some(i64_ty.clone()),
                },
                ret("cnt"),
            ],
        ),
        AggregateFunc::Avg => (
            "aggregate_avg",
            f32_ty.clone(),
            vec![
                reduce("sum", Op::ReduceSum, f32_ty.clone()),
                Instruction {
                    result: Some("n".to_string()),
                    op: Op::Constant,
                    operands: vec![],
                    attrs: vec![("value".to_string(), n.to_string())],
                    result_type: Some(f32_ty.clone()),
                },
                Instruction {
                    result: Some("avg".to_string()),
                    op: Op::Divf,
                    operands: vec!["sum".to_string(), "n".to_string()],
                    attrs: vec![],
                    result_type: Some(f32_ty.clone()),
                },
                ret("avg"),
            ],
        ),
    };

    let mut instrs = vec![load];
    instrs.extend(tail);
    Region {
        name: name.to_string(),
        args: vec![("col".to_string(), mem)],
        return_types: vec![ret_ty],
        blocks: vec![Block {
            label: "entry".to_string(),
            instructions: instrs,
        }],
    }
}

/// Emit the TPTIR text for a single-column aggregate of `n` rows.
pub fn emit_aggregate(func: AggregateFunc, n: u64) -> String {
    emit(&lower_aggregate(func, n), &EmitOptions::default())
}

/// Find the first `Aggregate` node in a plan tree, if any.
fn find_aggregate(node: &PlanNode) -> Option<&PlanNode> {
    match node {
        PlanNode::Aggregate { .. } => Some(node),
        PlanNode::Scan { .. } | PlanNode::SubqueryScan { .. } => None,
        PlanNode::Filter { input, .. }
        | PlanNode::Project { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Sort { input, .. } => find_aggregate(input),
    }
}

/// Lower a planned query's dominant GPU operation into TPTIR text.
///
/// Returns `Some(text)` when the plan's top operation is a single-aggregate
/// `Aggregate` node (so it lowers via [`lower_aggregate`]); returns `None`
/// otherwise, letting the caller fall back to the top-k shape
/// ([`emit_topk`]). This keeps `EXPLAIN`'s GPU view honest about what would
/// actually be emitted rather than always showing a top-k scan.
pub fn emit_for_plan(plan: &Plan) -> Option<String> {
    if let Some(PlanNode::Aggregate { aggregates, .. }) = find_aggregate(&plan.root) {
        if aggregates.len() == 1 {
            return Some(emit_aggregate(aggregates[0].1, plan.estimated_rows.max(1)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowers_to_tptir_with_expected_ops() {
        let text = emit_topk(1024);
        assert!(text.contains("func @vector_topk"));
        assert!(text.contains("^entry:"));
        assert!(text.contains("load"));
        assert!(text.contains("reduce_max"));
        assert!(text.contains("return"));
    }

    #[test]
    fn sum_lowers_to_reduce_sum() {
        let text = emit_aggregate(AggregateFunc::Sum, 1024);
        assert!(text.contains("func @aggregate_sum"));
        assert!(text.contains("reduce_sum"));
        assert!(!text.contains("reduce_min"));
    }

    #[test]
    fn min_lowers_to_reduce_min() {
        let text = emit_aggregate(AggregateFunc::Min, 1024);
        assert!(text.contains("func @aggregate_min"));
        assert!(text.contains("reduce_min"));
    }

    #[test]
    fn max_lowers_to_reduce_max() {
        let text = emit_aggregate(AggregateFunc::Max, 1024);
        assert!(text.contains("func @aggregate_max"));
        assert!(text.contains("reduce_max"));
    }

    #[test]
    fn avg_lowers_to_reduce_sum_then_divf() {
        let text = emit_aggregate(AggregateFunc::Avg, 1024);
        assert!(text.contains("func @aggregate_avg"));
        assert!(text.contains("reduce_sum"));
        assert!(text.contains("divf"));
    }

    #[test]
    fn count_lowers_to_custom_count_not_reduce_sum() {
        let text = emit_aggregate(AggregateFunc::Count, 1024);
        assert!(text.contains("func @aggregate_count"));
        assert!(text.contains("count"));
        assert!(!text.contains("reduce_sum"));
    }

    #[test]
    fn emit_for_plan_lowers_single_aggregate() {
        use crate::planner::{Dispatch, Plan};
        let plan = Plan {
            root: PlanNode::Aggregate {
                group_by: vec![],
                aggregates: vec![(
                    "total".to_string(),
                    AggregateFunc::Sum,
                    "amount".to_string(),
                )],
                having: None,
                input: Box::new(PlanNode::Scan {
                    table: "t".to_string(),
                    vectorized: true,
                }),
            },
            estimated_rows: 4096,
            dispatch: Dispatch::Gpu,
        };
        let text = emit_for_plan(&plan).expect("single-aggregate plan lowers");
        assert!(text.contains("func @aggregate_sum"));
        assert!(text.contains("reduce_sum"));
    }

    #[test]
    fn emit_for_plan_returns_none_for_multi_aggregate() {
        use crate::planner::{Dispatch, Plan};
        let plan = Plan {
            root: PlanNode::Aggregate {
                group_by: vec![],
                aggregates: vec![
                    ("a".to_string(), AggregateFunc::Sum, "x".to_string()),
                    ("b".to_string(), AggregateFunc::Min, "y".to_string()),
                ],
                having: None,
                input: Box::new(PlanNode::Scan {
                    table: "t".to_string(),
                    vectorized: true,
                }),
            },
            estimated_rows: 100,
            dispatch: Dispatch::Gpu,
        };
        assert!(emit_for_plan(&plan).is_none());
    }
}
