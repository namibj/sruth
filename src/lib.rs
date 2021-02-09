pub mod dataflow;
pub mod optimize;
pub mod repr;
pub mod verify;

#[cfg(test)]
mod tests {
    use crate::{
        dataflow::{
            self,
            operators::{CrossbeamExtractor, CrossbeamPusher},
            Diff, Time, TraceManager,
        },
        optimize,
        repr::{
            basic_block::BasicBlockMeta,
            instruction::{Add, Assign, Div, Mul, Sub},
            terminator::{Branch, Label},
            utils::{DisplayCtx, IRDisplay},
            BasicBlock, BasicBlockId, Constant, FuncId, Function, Instruction, Terminator, Type,
            Value, ValueKind, VarId,
        },
        verify::{verify, ValidityError},
    };
    use dataflow::InputManager;
    use differential_dataflow::{
        input::Input,
        operators::{
            arrange::{ArrangeByKey, ArrangeBySelf, TraceAgent},
            iterate::Variable,
            Consolidate, Join, JoinCore, Reduce, Threshold,
        },
        trace::implementations::ord::OrdKeySpine,
    };
    use lasso::ThreadedRodeo;
    use pretty::{BoxAllocator, RefDoc};
    use std::{io, iter, num::NonZeroU64, sync::Arc};
    use timely::{
        dataflow::{
            operators::{capture::Extract, Capture},
            ProbeHandle, Scope,
        },
        order::Product,
        progress::frontier::AntichainRef,
        Config,
    };
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    #[test]
    fn optimization_test() {
        let _ = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(tracing_subscriber::fmt::layer())
            .try_init();

        let interner = Arc::new(ThreadedRodeo::new());
        let intern = interner.clone();

        let ir = vec![Function {
            name: None,
            id: FuncId::new(NonZeroU64::new(1).unwrap()),
            params: vec![(VarId::new(NonZeroU64::new(13).unwrap()), Type::Uint)],
            entry: BasicBlockId::new(NonZeroU64::new(1).unwrap()),
            basic_blocks: vec![
                BasicBlock {
                    name: None,
                    id: BasicBlockId::new(NonZeroU64::new(1).unwrap()),
                    instructions: vec![],
                    terminator: Terminator::Branch(Branch::new(
                        Constant::Bool(true).into(),
                        Label::new(BasicBlockId::new(NonZeroU64::new(2).unwrap())),
                        Label::new(BasicBlockId::new(NonZeroU64::new(2).unwrap())),
                    )),
                },
                BasicBlock {
                    name: None,
                    id: BasicBlockId::new(NonZeroU64::new(2).unwrap()),
                    instructions: vec![
                        Instruction::Assign(Assign {
                            value: Value::new(ValueKind::Const(Constant::Uint(100)), Type::Uint),
                            dest: VarId::new(NonZeroU64::new(6).unwrap()),
                        }),
                        Instruction::Add(Add {
                            lhs: Value::new(ValueKind::Const(Constant::Uint(300)), Type::Uint),
                            rhs: Value::new(
                                ValueKind::Var(VarId::new(NonZeroU64::new(6).unwrap())),
                                Type::Uint,
                            ),
                            dest: VarId::new(NonZeroU64::new(1).unwrap()),
                        }),
                        Instruction::Mul(Mul {
                            lhs: Value::new(ValueKind::Const(Constant::Uint(10)), Type::Uint),
                            rhs: Value::new(
                                ValueKind::Var(VarId::new(NonZeroU64::new(1).unwrap())),
                                Type::Uint,
                            ),
                            dest: VarId::new(NonZeroU64::new(2).unwrap()),
                        }),
                        Instruction::Div(Div {
                            lhs: Value::new(ValueKind::Const(Constant::Uint(10)), Type::Uint),
                            rhs: Value::new(
                                ValueKind::Var(VarId::new(NonZeroU64::new(2).unwrap())),
                                Type::Uint,
                            ),
                            dest: VarId::new(NonZeroU64::new(3).unwrap()),
                        }),
                        Instruction::Sub(Sub {
                            lhs: Value::new(ValueKind::Const(Constant::Uint(5)), Type::Uint),
                            rhs: Value::new(
                                ValueKind::Var(VarId::new(NonZeroU64::new(3).unwrap())),
                                Type::Uint,
                            ),
                            dest: VarId::new(NonZeroU64::new(4).unwrap()),
                        }),
                        Instruction::Div(Div {
                            lhs: Value::new(ValueKind::Const(Constant::Uint(10)), Type::Uint),
                            rhs: Value::new(
                                ValueKind::Var(VarId::new(NonZeroU64::new(2).unwrap())),
                                Type::Uint,
                            ),
                            dest: VarId::new(NonZeroU64::new(5).unwrap()),
                        }),
                        Instruction::Mul(Mul {
                            lhs: Value::new(ValueKind::Const(Constant::Uint(0)), Type::Uint),
                            rhs: Value::new(
                                ValueKind::Var(VarId::new(NonZeroU64::new(13).unwrap())),
                                Type::Uint,
                            ),
                            dest: VarId::new(NonZeroU64::new(11).unwrap()),
                        }),
                        Instruction::Mul(Mul {
                            lhs: Value::new(
                                ValueKind::Var(VarId::new(NonZeroU64::new(13).unwrap())),
                                Type::Uint,
                            ),
                            rhs: Value::new(ValueKind::Const(Constant::Uint(0)), Type::Uint),
                            dest: VarId::new(NonZeroU64::new(12).unwrap()),
                        }),
                        Instruction::Sub(Sub {
                            lhs: Value::new(
                                ValueKind::Var(VarId::new(NonZeroU64::new(13).unwrap())),
                                Type::Uint,
                            ),
                            rhs: Value::new(ValueKind::Const(Constant::Uint(0)), Type::Uint),
                            dest: VarId::new(NonZeroU64::new(14).unwrap()),
                        }),
                    ],
                    terminator: Terminator::Return(Some(Value::new(
                        ValueKind::Var(VarId::new(NonZeroU64::new(14).unwrap())),
                        Type::Uint,
                    ))),
                },
            ],
        }];

        let (sender, receiver) = crossbeam_channel::unbounded();

        timely::execute(Config::thread(), move |worker| {
            let (mut probe, mut trace_manager) = (ProbeHandle::new(), TraceManager::new());

            let mut input_manager = worker.dataflow_named("inputs", |scope| {
                let mut input = InputManager::new(scope);

                let (instructions, basic_blocks, functions) = (
                    input
                        .instruction_trace
                        .import(scope)
                        .as_collection(|&inst_id, inst| (inst_id, inst.clone())),
                    input
                        .basic_block_trace
                        .import(scope)
                        .as_collection(|&block, meta| (block, meta.clone())),
                    input
                        .function_trace
                        .import(scope)
                        .as_collection(|&func, meta| (func, meta.clone())),
                );

                let errors = verify(scope, &instructions, &basic_blocks, &functions)
                    .probe_with(&mut probe)
                    .arrange_by_self();

                trace_manager.insert_trace::<TraceAgent<OrdKeySpine<ValidityError, Time, Diff>>>(
                    interner.get_or_intern_static("input/errors"),
                    errors.trace,
                );

                input
            });

            let (mut instructions, mut terminators) =
                worker.dataflow_named::<Time, _, _>("constant propagation", |scope| {
                    let (instructions, terminators) = (
                        input_manager
                            .instruction_trace
                            .import(scope)
                            .as_collection(|&id, inst| (id, inst.clone())),
                        input_manager
                            .basic_block_trace
                            .import(scope)
                            .as_collection(|&id, meta| (id, meta.terminator.clone())),
                    );

                    let (instructions, terminators) = scope.scoped::<Product<_, Time>, _, _>(
                        "constant folding and peephole optimization",
                        |scope| {
                            let instructions = Variable::new_from(
                                instructions.enter(scope),
                                Product::new(Default::default(), 1),
                            );
                            let terminators = Variable::new_from(
                                terminators.enter(scope),
                                Product::new(Default::default(), 1),
                            );

                            let (folded_instructions, folded_terminators) =
                                optimize::constant_folding::<_, Diff>(
                                    scope,
                                    &instructions,
                                    &terminators,
                                );

                            let peeped_instructions =
                                optimize::peephole(scope, &folded_instructions).consolidate();

                            (
                                instructions.set(&peeped_instructions).leave(),
                                terminators.set(&folded_terminators.consolidate()).leave(),
                            )
                        },
                    );

                    let basic_blocks = input_manager.basic_block_trace.import(scope).join_map(
                        &terminators,
                        |&id, meta, term| {
                            (
                                id,
                                BasicBlockMeta {
                                    terminator: term.clone(),
                                    ..meta.clone()
                                },
                            )
                        },
                    );

                    let functions = input_manager
                        .function_trace
                        .import(scope)
                        .as_collection(|&func, meta| (func, meta.clone()));

                    let errors = verify(scope, &instructions, &basic_blocks, &functions)
                        .probe_with(&mut probe)
                        .arrange_by_self();

                    trace_manager
                        .insert_trace::<TraceAgent<OrdKeySpine<ValidityError, Time, Diff>>>(
                            interner.get_or_intern_static("constant-prop/errors"),
                            errors.trace,
                        );

                    let instructions = instructions.probe_with(&mut probe).arrange_by_key().trace;
                    trace_manager.insert_trace(
                        interner.get_or_intern_static("constant-prop/instructions"),
                        instructions.clone(),
                    );

                    let terminators = terminators.probe_with(&mut probe).arrange_by_key().trace;
                    trace_manager.insert_trace(
                        interner.get_or_intern_static("constant-prop/terminators"),
                        terminators.clone(),
                    );

                    (instructions, terminators)
                });

            let (mut instructions, mut basic_blocks) =
                worker.dataflow_named("cull dead code", |scope| {
                    let (instructions, terminators) = (
                        instructions.import_named(scope, "instructions (post constant folding)"),
                        terminators.import_named(scope, "terminators (post constant folding)"),
                    );

                    let (instructions, basic_blocks) =
                        optimize::dead_code(scope, &mut input_manager, &instructions, &terminators);

                    let instructions = instructions
                        .consolidate()
                        .probe_with(&mut probe)
                        .arrange_by_key();
                    let basic_blocks = basic_blocks
                        .consolidate()
                        .probe_with(&mut probe)
                        .arrange_by_key();

                    (instructions.trace, basic_blocks.trace)
                });

            worker.dataflow_named("reconstruct ir", |scope| {
                let (instructions, terminators, basic_blocks, block_trace) = (
                    instructions.import(scope),
                    terminators.import(scope),
                    basic_blocks.import(scope),
                    input_manager.basic_block_trace.import(scope),
                );

                let live_blocks = basic_blocks.as_collection(|_func, &block| block);
                let live_basic_blocks =
                    block_trace
                        .semijoin(&live_blocks)
                        .flat_map(|(block_id, meta)| {
                            meta.instructions
                                .into_iter()
                                .enumerate()
                                .map(move |(idx, inst)| (inst, (block_id, idx)))
                        });

                let mut basic_blocks = live_basic_blocks
                    .join_core(&instructions, |_inst, &(block, inst_idx), inst| {
                        iter::once((block, (inst.to_owned(), inst_idx)))
                    })
                    .reduce(|_, input, output| {
                        let mut instructions: Vec<_> = input
                            .iter()
                            .copied()
                            .map(|((inst, idx), _diff)| (inst.clone(), idx))
                            .collect();
                        instructions.sort_unstable_by_key(|&(_, idx)| idx);

                        output.push((
                            instructions
                                .into_iter()
                                .map(|(inst, _idx)| inst)
                                .collect::<Vec<_>>(),
                            1,
                        ));
                    })
                    .join_core(&terminators, |&block_id, instructions, term| {
                        iter::once((
                            block_id,
                            BasicBlock {
                                // TODO: Retain this info
                                name: None,
                                id: block_id,
                                instructions: instructions.to_owned(),
                                terminator: term.to_owned(),
                            },
                        ))
                    });

                let terminated_blocks = terminators.semijoin(
                    &block_trace
                        .as_collection(|&block_id, _| block_id)
                        .distinct_core::<Diff>(),
                );

                // Add back in the blocks which have no instructions
                basic_blocks = basic_blocks.concat(
                    &terminated_blocks
                        .antijoin(&basic_blocks.map(|(block_id, _)| block_id))
                        .map(|(block_id, terminator)| {
                            (
                                block_id,
                                BasicBlock {
                                    // TODO: Retain this info
                                    name: None,
                                    id: block_id,
                                    instructions: Vec::new(),
                                    terminator,
                                },
                            )
                        }),
                );

                let functions = input_manager
                    .function_trace
                    .import(scope)
                    .flat_map_ref(|&func_id, meta| {
                        let meta = meta.clone();
                        meta.basic_blocks
                            .clone()
                            .into_iter()
                            .map(move |block| (block, (func_id, meta.clone())))
                    })
                    .join_core(
                        &basic_blocks.arrange_by_key(),
                        |&block_id, &(func_id, ref meta), instructions| {
                            iter::once(((func_id, meta.clone()), (block_id, instructions.clone())))
                        },
                    )
                    .reduce(|&(func_id, ref meta), input, output| {
                        let basic_blocks: Vec<_> = input
                            .iter()
                            .copied()
                            .map(|((_, instructions), _diff)| instructions.to_owned())
                            .collect();

                        output.push((
                            Function {
                                name: meta.name,
                                id: func_id,
                                params: meta.params.clone(),
                                entry: meta.entry,
                                basic_blocks,
                            },
                            1,
                        ));
                    })
                    .map(|((id, _meta), func)| (id, func))
                    .probe_with(&mut probe);

                trace_manager.insert_trace(
                    interner.get_or_intern_static("reconstruct/functions"),
                    functions.arrange_by_key().trace,
                );

                let (_, mut errors) = scope.new_collection();
                let error_traces = &[
                    interner.get_or_intern_static("input/errors"),
                    interner.get_or_intern_static("constant-prop/errors"),
                ];

                for &trace in error_traces {
                    let trace = trace_manager
                        .get_trace::<TraceAgent<OrdKeySpine<ValidityError, Time, Diff>>>(trace)
                        .unwrap()
                        .import(scope)
                        .as_collection(|error, _| Err(error.clone()));

                    errors = errors.concat(&trace);
                }

                functions
                    .map(Ok)
                    .concat(&errors)
                    .distinct_core::<Diff>()
                    .inner
                    .capture_into(CrossbeamPusher::new(sender.clone()));
            });

            if worker.index() == 0 {
                dataflow::translate(&mut input_manager, ir.clone(), &*interner);
            }

            input_manager.advance_to(1);

            let frontier = AntichainRef::new(&[1]);
            trace_manager.advance_by(frontier);
            trace_manager.distinguish_since(frontier);

            worker.step_while(|| !probe.less_than(input_manager.time()));
        })
        .unwrap();

        let alloc = BoxAllocator;
        for (time, data) in CrossbeamExtractor::new(receiver).extract() {
            println!("Data from timestamp {}:", time);

            for (data, _time, _diff) in data {
                match data {
                    Ok((_id, func)) => {
                        func.display::<BoxAllocator, RefDoc, _>(DisplayCtx::new(&alloc, &*intern))
                            .1
                            .render(70, &mut io::stdout())
                            .unwrap();
                    }

                    Err(err) => println!("Error: {:?}", err),
                }
            }
        }
    }
}
