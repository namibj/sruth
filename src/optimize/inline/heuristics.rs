use crate::{
    dataflow::{
        operators::{CountExt, FilterMap},
        Program,
    },
    repr::{
        instruction::Call,
        utils::{CastRef, InstructionExt, InstructionPurity},
        Cast, FuncId,
    },
};
use abomonation_derive::Abomonation;
use differential_dataflow::{
    difference::{Abelian, DiffPair, Semigroup},
    lattice::Lattice,
    operators::{Consolidate, Join, Reduce, Threshold},
    Collection, ExchangeData,
};
use num_traits::AsPrimitive;
use std::{iter, ops::Mul};
use timely::dataflow::Scope;

pub fn harvest_heuristics<S, R>(
    program: &Program<S, R>,
) -> Collection<S, (FuncId, InlineHeuristics), R>
where
    S: Scope,
    S::Timestamp: Lattice,
    R: Semigroup + Abelian + ExchangeData + Mul<Output = R> + AsPrimitive<usize> + From<i8> + Clone,
    isize: Mul<R, Output = isize>,
{
    let instructions = program
        .block_instructions
        .join_map(&program.instructions, |&inst_id, &block, inst| {
            (block, (inst_id, inst.clone()))
        })
        .join_map(
            &program.function_blocks,
            |_block, (_inst_id, inst), &func| (func, inst.clone()),
        );

    let mut block_lengths = program
        .function_blocks
        .map(|(_, func)| func)
        .count_core::<R>();

    block_lengths = block_lengths.concat(
        &program
            .function_descriptors
            .antijoin(&block_lengths.map(|(func, _)| func))
            .map(|(id, _)| (id, R::from(0))),
    );

    let mut ssa_inst_lengths = instructions.map(|(func, _)| func).count_core();

    ssa_inst_lengths = ssa_inst_lengths.concat(
        &program
            .function_descriptors
            .antijoin(&ssa_inst_lengths.map(|(func, _)| func))
            .map(|(id, _)| (id, R::from(0))),
    );

    let mut invocations = instructions
        .filter_map(|(_, inst)| inst.cast::<Call>().map(|call| call.func))
        .count_core();

    invocations = invocations.concat(
        &program
            .function_descriptors
            .antijoin(&invocations.map(|(func, _)| func))
            .map(|(id, _)| (id, R::from(0))),
    );

    let mut branches = program
        .block_terminators
        .filter(|(_, term)| term.is_branching())
        .join_map(&program.function_blocks, |_block, _term, &func| func)
        .count_core();

    branches = branches.concat(
        &program
            .function_descriptors
            .antijoin(&branches.map(|(func, _)| func))
            .map(|(id, _)| (id, R::from(0))),
    );

    let mut function_calls = instructions
        .filter_map(|(func, inst)| inst.cast::<Call>().map(move |_call| func))
        .count_core();

    function_calls = function_calls.concat(
        &program
            .function_descriptors
            .antijoin(&function_calls.map(|(func, _)| func))
            .map(|(id, _)| (id, R::from(0))),
    );

    let mut is_pure = instructions
        .consolidate()
        .reduce(|_func, instructions, output| {
            let is_pure = instructions
                .iter()
                .all(|(inst, _)| inst.purity() == InstructionPurity::Pure);

            output.push((is_pure, R::from(1)));
        });

    is_pure = is_pure.concat(
        &program
            .function_descriptors
            .antijoin(&is_pure.map(|(func, _)| func))
            .map(|(id, _)| (id, true)),
    );

    let mut is_recursive = instructions
        .filter_map(|(func, inst)| {
            inst.cast_ref::<Call>()
                .filter(|call| call.func == func)
                .map(|_call| (func, true))
        })
        .distinct_core();

    is_recursive = is_recursive.concat(
        &program
            .function_descriptors
            .antijoin(&is_recursive.map(|(func, _)| func))
            .map(|(id, _)| (id, false)),
    );

    // TODO: Add terminators to this
    let mut estimated_asm = instructions
        .explode(|(func, inst)| {
            let diff = DiffPair::new(R::from(1), inst.estimated_instructions() as isize);
            iter::once((func, diff))
        })
        .count_core::<R>()
        .map(|(func, diff)| (func, diff.element2 as usize));

    estimated_asm = estimated_asm.concat(
        &program
            .function_descriptors
            .antijoin(&estimated_asm.map(|(func, _)| func))
            .map(|(id, _)| (id, 0)),
    );

    block_lengths
        .join(&ssa_inst_lengths)
        .join(&invocations)
        .join(&branches)
        .join(&function_calls)
        .join(&is_pure)
        .join(&estimated_asm)
        .join_map(
            &is_recursive,
            |&func,
             &(
                (
                    ((((block_length, ssa_inst_length), invocations), branches), function_calls),
                    is_pure,
                ),
                estimated_asm,
            ),
             &is_recursive| {
                (
                    func,
                    InlineHeuristics::new(
                        branches.clone().as_(),
                        invocations.clone().as_(),
                        block_length.clone().as_(),
                        ssa_inst_length.clone().as_(),
                        function_calls.clone().as_(),
                        is_pure,
                        is_recursive,
                        estimated_asm,
                    ),
                )
            },
        )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Abomonation)]
pub struct InlineHeuristics {
    pub branches: usize,
    pub invocations: usize,
    pub block_length: usize,
    pub ssa_inst_length: usize,
    // pub promotable_branches: usize,
    pub function_calls: usize,
    // pub cheap_builtin_calls: usize,
    pub is_pure: bool,
    pub is_recursive: bool,
    pub estimated_asm: usize,
}

impl InlineHeuristics {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        branches: usize,
        invocations: usize,
        block_length: usize,
        ssa_inst_length: usize,
        // promotable_branches: usize,
        function_calls: usize,
        // cheap_builtin_calls: usize,
        is_pure: bool,
        is_recursive: bool,
        estimated_asm: usize,
    ) -> Self {
        Self {
            branches,
            invocations,
            block_length,
            ssa_inst_length,
            // promotable_branches,
            function_calls,
            // cheap_builtin_calls,
            is_pure,
            is_recursive,
            estimated_asm,
        }
    }

    // TODO: Estimate stack size
    // TODO: Hot/cold calling conventions
    // TODO: Function purity
    // TODO: inline(never) & inline(always)
    pub fn inline_cost(&self) -> usize {
        self.invocations + self.block_length + self.branches + self.function_calls
    }

    /// Returns true if the function is trivially inlinable, meaning that it's small
    /// and has very few invocations. Inlining trivial functions like this helps with
    /// both performance (via removal of indirection and cache locality) and code size
    pub fn trivially_inlinable(&self) -> bool {
        self.inline_cost() > Self::TRIVIALLY_INLINABLE
    }

    const TRIVIALLY_INLINABLE: usize = 10;
}