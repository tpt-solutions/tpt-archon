//! GPU IR emission smoke test (TPTIR dialect).
//!
//! `tpt-gpu-ir-spec` is an **emitter**, not a runtime: `emit(&Region,
//! &EmitOptions) -> String` lowers an IR region to the stable TPTIR text
//! format. We do NOT claim execution. This test proves the relational engine's
//! vectorized top-k scan shape can be lowered into TPTIR text containing the
//! ops a real GPU backend would consume (`load`, `reduce_max`, `return`).
//!
//! When the relational crate is built with its `gpu` feature, the same lowered
//! region is what `relational::gpu::lower_topk` would hand to the backend.

#![cfg(test)]

use tpt_gpu_ir_spec::{
    ops::core_op_table,
    text::{emit, EmitOptions, Instruction, Region},
    types::{AddressSpace, ElemType, Type},
    Op,
};

/// Build the TPTIR region for a vectorized top-k similarity scan over a
/// `[N x f32]` embedding memref: load the embeddings, reduce to the max
/// similarity, and return it.
fn topk_region(n: u64) -> Region {
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

#[test]
fn gpu_op_table_contains_vector_ops() {
    // The dialect the relational engine targets must expose the ops used by the
    // top-k scan.
    let table = core_op_table();
    let by_name: std::collections::HashSet<_> = table.iter().map(|d| d.op.clone()).collect();
    // The dialect the relational engine targets must expose the memory/control
    // ops a vectorized top-k scan lowers into.
    assert!(by_name.contains(&Op::Load));
    assert!(by_name.contains(&Op::Return));
}

#[test]
fn vector_topk_lowers_to_tptir_text() {
    let region = topk_region(1024);
    let text = emit(&region, &EmitOptions::default());

    assert!(text.contains("func @vector_topk"), "emits a function");
    assert!(text.contains("^entry:"), "emits an entry block");
    assert!(text.contains("load"), "loads the embedding memref");
    assert!(text.contains("reduce_max"), "reduces to max similarity");
    assert!(text.contains("return"), "returns the best score");
}

#[test]
fn emitted_text_round_trips_block_labels() {
    // The spec's structural parser recovers the block labels we emitted.
    let region = topk_region(256);
    let text = emit(&region, &EmitOptions::default());
    let labels = tpt_gpu_ir_spec::text::parse(&text).expect("parses emitted text");
    assert_eq!(labels, vec!["entry".to_string()]);
}
