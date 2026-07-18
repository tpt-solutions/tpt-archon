//! GPU lowering for the relational engine (feature-gated).
//!
//! This module does **not** execute anything on a GPU. It lowers the engine's
//! vectorized top-k similarity scan (`executor::vector_topk`) into the stable
//! TPTIR text dialect via [`tpt_gpu_ir_spec`], the canonical emitter shared
//! across the TPT compute suite. The emitted text is what an external GPU
//! backend (tpt-gpu, tpt-crucible, …) would consume; this crate only produces
//! it.
//!
//! The `gpu` feature on the engine only enables *emission*. The CPU path
//! (`vector_topk`) remains the runtime fallback, and the planner still only
//! dispatches to `Dispatch::Gpu` when built with this feature and the scan is
//! large enough (`planner::GPU_ROW_THRESHOLD`).

use tpt_gpu_ir_spec::{
    text::{emit, EmitOptions, Instruction, Region},
    types::{AddressSpace, ElemType, Type},
    Op,
};

/// Lower a vectorized top-k similarity scan over an `[n x f32]` embedding
/// memref into a TPTIR [`Region`]: load the embeddings, reduce to the max
/// similarity, and return it. `n` is the embedding table length.
pub fn lower_topk(n: u64) -> Region {
    let f32_ty = Type::scalar(ElemType::F32);
    let mem = Type::memref(vec![n as i64], f32_ty.clone(), AddressSpace::Global);
    let tensor = Type::tensor(vec![n as i64], f32_ty.clone(), AddressSpace::Global);
    Region {
        name: "vector_topk".to_string(),
        args: vec![("embeddings".to_string(), mem)],
        return_types: vec![f32_ty],
        blocks: vec![tpt_gpu_ir_spec::text::Block {
            label: "entry".to_string(),
            instructions: vec![
                Instruction {
                    result: Some("sim".to_string()),
                    op: Op::Load,
                    operands: vec!["embeddings".to_string()],
                    attrs: vec![],
                    result_type: Some(tensor),
                },
                Instruction {
                    result: Some("best".to_string()),
                    op: Op::ReduceMax,
                    operands: vec!["sim".to_string()],
                    attrs: vec![],
                    result_type: Some(Type::scalar(ElemType::F32)),
                },
                Instruction {
                    result: None,
                    op: Op::Return,
                    operands: vec!["best".to_string()],
                    attrs: vec![],
                    result_type: None,
                },
            ],
        }],
    }
}

/// Emit the TPTIR text for a top-k scan of `n` embeddings.
pub fn emit_topk(n: u64) -> String {
    emit(&lower_topk(n), &EmitOptions::default())
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
}
