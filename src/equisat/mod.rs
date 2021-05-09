use crate::dataflow::{
    operators::{FilterSplit, InspectExt, Reverse},
    Time,
};
use abomonation_derive::Abomonation;
use differential_dataflow::{
    algorithms::graphs::propagate,
    collection::concatenate,
    difference::{Abelian, Multiply, Semigroup},
    lattice::Lattice,
    operators::{
        arrange::{ArrangeByKey, Arranged, TraceAgent},
        iterate::SemigroupVariable,
        Join, JoinCore, Reduce, Threshold,
    },
    trace::implementations::ord::OrdValSpine,
    Collection, ExchangeData,
};
use std::iter;
use timely::{
    dataflow::{operators::probe::Handle, Scope, ScopeParent},
    order::Product,
    progress::Timestamp,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Abomonation)]
pub struct EClassId(u64);

impl EClassId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn as_enode(self) -> ENodeId {
        ENodeId(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Abomonation)]
pub struct ENodeId(u64);

impl ENodeId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn as_eclass(self) -> EClassId {
        EClassId(self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Abomonation)]
pub enum ENode {
    Add(Add),
    Sub(Sub),
    Constant,
}

impl ENode {
    /// Returns `true` if the enode is [`Add`].
    pub const fn is_add(&self) -> bool {
        matches!(self, Self::Add(..))
    }

    /// Returns `true` if the enode is [`Sub`].
    pub const fn is_sub(&self) -> bool {
        matches!(self, Self::Sub(..))
    }

    pub fn as_add(&self) -> Option<Add> {
        if let Self::Add(add) = self {
            Some(add.clone())
        } else {
            None
        }
    }

    pub fn as_sub(&self) -> Option<Sub> {
        if let Self::Sub(sub) = self {
            Some(sub.clone())
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Abomonation)]
pub struct Add {
    lhs: EClassId,
    rhs: EClassId,
}

impl Add {
    pub const fn new(lhs: EClassId, rhs: EClassId) -> Self {
        Self { lhs, rhs }
    }

    /// Get the [`Add`]'s left hand side
    pub const fn lhs(&self) -> EClassId {
        self.lhs
    }

    /// Get the [`Add`]'s right hand side
    pub const fn rhs(&self) -> EClassId {
        self.rhs
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Abomonation)]
pub struct Sub {
    lhs: EClassId,
    rhs: EClassId,
}

impl Sub {
    pub const fn new(lhs: EClassId, rhs: EClassId) -> Self {
        Self { lhs, rhs }
    }

    /// Get the [`Sub`]'s left hand side
    pub const fn lhs(&self) -> EClassId {
        self.lhs
    }

    /// Get the [`Sub`]'s right hand side
    pub const fn rhs(&self) -> EClassId {
        self.rhs
    }
}

type EClassLookup<S, R> =
    Arranged<S, TraceAgent<OrdValSpine<EClassId, EClassId, <S as ScopeParent>::Timestamp, R>>>;
type EClassMerger<S, R> = Collection<S, (EClassId, EClassId), R>;
type ENodeCollection<S, R> = Collection<S, (ENodeId, ENode), R>;

pub struct EGraph<S, R>
where
    S: Scope,
    S::Timestamp: Lattice,
    R: Semigroup,
{
    /// Collection of enodes and their ids
    enodes: Vec<ENodeCollection<S, R>>,
    /// A read-only collection of eclass ids to their parent eclasses
    eclass_mergers: Vec<EClassMerger<S, R>>,
    /// A write-only collection of eclass ids to their parent eclasses
    eclass_canon_lookup: EClassLookup<S, R>,
    eclass_mergers_feedback: SemigroupVariable<S, (EClassId, EClassId), R>,
    enodes_feedback: SemigroupVariable<S, (ENodeId, ENode), R>,
    canon_enode_ids: Collection<S, ENodeId, R>,
    scope: S,
}

impl<S, R> EGraph<S, R>
where
    S: Scope,
    S::Timestamp: Lattice + Timestamp,
    R: Semigroup,
{
    pub fn new(scope: &mut S, summary: <S::Timestamp as Timestamp>::Summary) -> Self
    where
        R: Abelian + ExchangeData + Multiply<Output = R> + From<i8>,
    {
        let eclass_mergers_feedback = SemigroupVariable::new(scope, summary.clone());
        let enodes_feedback = SemigroupVariable::new(scope, summary);

        let (eclass_canon_lookup, canon_enode_ids) =
            union(scope, &enodes_feedback, &eclass_mergers_feedback);

        Self {
            enodes: vec![],
            eclass_mergers: vec![],
            eclass_canon_lookup,
            eclass_mergers_feedback,
            enodes_feedback,
            canon_enode_ids,
            scope: scope.clone(),
        }
    }

    pub fn add_enodes(&mut self, enodes: Collection<S, (ENodeId, ENode), R>) -> &mut Self {
        self.enodes.push(enodes);
        self
    }

    pub fn add_rewrite<F>(&mut self, rewrite: F) -> &mut Self
    where
        F: FnOnce(&ENodeCollection<S, R>, &EClassLookup<S, R>) -> EClassMerger<S, R>,
    {
        self.eclass_mergers
            .push(rewrite(&self.enodes_feedback, &self.eclass_canon_lookup));
        self
    }

    pub fn scope(&self) -> S {
        self.scope.clone()
    }

    pub fn probe_with(&self, probe: &mut Handle<S::Timestamp>) {
        self.enodes.iter().for_each(|enodes| {
            enodes.probe_with(probe);
        });
        self.eclass_mergers.iter().for_each(|merger| {
            merger.probe_with(probe);
        });
        self.canon_enode_ids.probe_with(probe);
    }

    // pub fn enter<'a, T>(&self, scope: &mut Child<'a, S, T>) -> EGraph<Child<'a, S, T>, R>
    // where
    //     T: Refines<S::Timestamp> + Lattice,
    // {
    //     EGraph {
    //         enodes: self.enodes.enter(scope),
    //         eclass_nodes: self.eclass_nodes.enter(scope),
    //         eclass_mergers: self.eclass_mergers.enter(scope),
    //         eclass_canon_lookup: self.eclass_canon_lookup,
    //         canon_enode_ids: self.canon_enode_ids.enter(scope),
    //     }
    // }

    pub fn debug(&self) {
        self.enodes.iter().for_each(|enodes| {
            enodes.debug();
        });
        self.eclass_mergers.iter().for_each(|merger| {
            merger.debug();
        });
        self.eclass_canon_lookup
            .as_collection(|&src, &dest| (src, dest))
            .debug();
        self.canon_enode_ids.debug();
        self.eclass_mergers_feedback.debug();
        self.enodes_feedback.debug();
    }

    fn feedback(self) -> Collection<S, (ENodeId, ENode), R> {
        let mut scope = self.scope();

        self.eclass_mergers_feedback
            .set(&concatenate(&mut scope, self.eclass_mergers.into_iter()))
            .debug();

        self.enodes_feedback
            .set(&concatenate(&mut scope, self.enodes.into_iter()))
            .debug()
    }
}

fn union<S, R>(
    scope: &mut S,
    enodes: &Collection<S, (ENodeId, ENode), R>,
    raw_eclass_mergers: &Collection<S, (EClassId, EClassId), R>,
) -> (EClassLookup<S, R>, Collection<S, ENodeId, R>)
where
    S: Scope,
    S::Timestamp: Lattice,
    R: Abelian + ExchangeData + Multiply<Output = R> + From<i8>,
{
    let (canon_enode_ids, eclass_canon_lookup) = scope.iterative::<Time, _, _>(|scope| {
        let enodes = enodes.enter(scope);
        let eclass_mergers = SemigroupVariable::new(scope, Product::new(Default::default(), 1));

        let union_find = derive_canonical_eclass_ids(
            &eclass_mergers
                .concat(&raw_eclass_mergers.enter(scope))
                // This distinct could be unnecessary, but it's here to make sure that the
                // multiplicities from the variable don't overflow within the `propagate_core()`
                // call inside of canon id derivation
                .distinct_core(),
            &enodes,
        );

        let eclass_union_find = union_find
            .map(|(enode, eclass)| (enode.as_eclass(), eclass))
            .arrange_by_key();

        let (add_lhs, add_rhs) = enodes.filter_split(|(enode_id, enode)| {
            if let Some(add) = enode.as_add() {
                (Some((add.lhs(), enode_id)), Some((add.rhs(), enode_id)))
            } else {
                (None, None)
            }
        });
        let canon_add_lhs = add_lhs.join_core(&eclass_union_find, |_, &parent_enode, &eclass| {
            iter::once((parent_enode, eclass))
        });
        let canon_add_rhs = add_rhs.join_core(&eclass_union_find, |_, &parent_enode, &eclass| {
            iter::once((parent_enode, eclass))
        });

        let (sub_lhs, sub_rhs) = enodes.filter_split(|(enode_id, enode)| {
            if let Some(sub) = enode.as_sub() {
                (Some((sub.lhs(), enode_id)), Some((sub.rhs(), enode_id)))
            } else {
                (None, None)
            }
        });
        let canon_sub_lhs = sub_lhs.join_core(&eclass_union_find, |_, &parent_enode, &eclass| {
            iter::once((parent_enode, eclass))
        });
        let canon_sub_rhs = sub_rhs.join_core(&eclass_union_find, |_, &parent_enode, &eclass| {
            iter::once((parent_enode, eclass))
        });

        let canon_enodes = canon_add_lhs
            .join_map(&canon_add_rhs, |&enode, &lhs, &rhs| {
                (ENode::Add(Add::new(lhs, rhs)), enode)
            })
            .concat(
                &canon_sub_lhs.join_map(&canon_sub_rhs, |&enode, &lhs, &rhs| {
                    (ENode::Sub(Sub::new(lhs, rhs)), enode)
                }),
            )
            .arrange_by_key();

        let canon_edges = canon_enodes
            .reduce(|_enode, enodes, edges| {
                let (&first_enode, _) = enodes[0].clone();

                if enodes.len() == 1 {
                    edges.push(((first_enode, first_enode), R::from(1)));
                } else {
                    edges.reserve(enodes.len() - 1);
                    edges.extend(
                        enodes
                            .iter()
                            .skip(1)
                            .map(|&(&enode, _)| ((first_enode, enode), R::from(1))),
                    );
                }
            })
            .debug();

        let canon_enode_ids = canon_edges
            .map(|(_, (canonical_enode_id, _))| canonical_enode_id)
            .leave()
            .distinct_core::<R>();

        let canon_enode_edges =
            canon_edges.map(|(_enode, (src, dest))| (src.as_eclass(), dest.as_eclass()));
        let canon_eclass_lookup = canon_enode_edges.concat(&canon_enode_edges.reverse());

        (
            canon_enode_ids,
            eclass_mergers
                .set(&canon_eclass_lookup)
                .leave()
                .arrange_by_key(),
        )
    });

    (eclass_canon_lookup, canon_enode_ids)
}

fn derive_canonical_eclass_ids<S, R>(
    eclass_parents: &Collection<S, (EClassId, EClassId), R>,
    enodes: &Collection<S, (ENodeId, ENode), R>,
) -> Collection<S, (ENodeId, EClassId), R>
where
    S: Scope,
    S::Timestamp: Lattice,
    R: Abelian + ExchangeData + Multiply<Output = R> + From<i8>,
{
    let canonicalized_edges = eclass_parents.flat_map(|(src, dest)| {
        vec![
            (src.as_enode(), dest.as_enode()),
            (dest.as_enode(), src.as_enode()),
        ]
    });

    let implicit_eclass_assignment = enodes
        .map(|(enode, _)| (enode, enode.as_eclass()))
        .distinct_core();

    propagate::propagate_at(
        &canonicalized_edges,
        &implicit_eclass_assignment,
        |&eclass| eclass.0,
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        dataflow::{
            operators::{FilterMap, InspectExt},
            Diff,
        },
        equisat::{Add, EClassId, EGraph, ENode, ENodeId, Sub},
    };
    use differential_dataflow::{input::Input, operators::JoinCore};
    use std::iter;
    use timely::{
        dataflow::{operators::probe::Handle, Scope},
        order::Product,
    };

    #[test]
    fn union_find() {
        timely::execute_directly(|worker| {
            let mut probe = Handle::new();

            let mut enodes = worker.dataflow::<usize, _, _>(|scope| {
                let (enode_input, enodes) = scope.new_collection();

                scope
                    .iterative::<usize, _, _>(|scope| {
                        let mut graph =
                            EGraph::<_, Diff>::new(scope, Product::new(Default::default(), 1));

                        graph
                            .add_enodes(enodes.enter(scope))
                            // Delete subtraction
                            .add_rewrite(|enodes, eclass_lookup| {
                                enodes
                                    .debug()
                                    .filter_map(|(enode_id, enode)| {
                                        enode
                                            .as_sub()
                                            .map(move |sub| (enode_id.as_eclass(), sub.lhs()))
                                    })
                                    .debug()
                                    .join_core(eclass_lookup, |_enode_id, &lhs_enode, &eclass| {
                                        iter::once((eclass, lhs_enode))
                                    })
                                    .debug()
                            });

                        graph.debug();
                        graph.feedback().leave()
                    })
                    .inspect(|x| println!("{:?}", x))
                    .probe_with(&mut probe);

                enode_input
            });

            enodes.insert((
                ENodeId::new(0),
                ENode::Add(Add::new(EClassId::new(2), EClassId::new(1))),
            ));
            enodes.insert((
                ENodeId::new(1),
                ENode::Sub(Sub::new(EClassId::new(3), EClassId::new(2))),
            ));
            enodes.insert((ENodeId::new(2), ENode::Constant));
            enodes.insert((ENodeId::new(3), ENode::Constant));

            enodes.advance_to(1);
            enodes.flush();

            worker.step_while(|| probe.less_than(enodes.time()));
        });
    }
}
