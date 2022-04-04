use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::{format, Display};
use std::iter::FromIterator;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::{collections::HashMap, hash::Hash};

use alga::general::{
    AbstractMagma, Additive, AdditiveMagma, Field, Identity, JoinSemilattice, Lattice,
    MeetSemilattice,
};
use anyhow::Context;
use cwe_checker_lib::analysis::graph;
use cwe_checker_lib::intermediate_representation::Tid;
use cwe_checker_lib::pcode::Label;
use env_logger::Target;
use itertools::Itertools;
use log::info;
use petgraph::dot::Dot;
use petgraph::graph::IndexType;
use petgraph::stable_graph::{StableDiGraph, StableGraph};
use petgraph::unionfind::UnionFind;
use petgraph::visit::{
    Dfs, EdgeRef, IntoEdgeReferences, IntoEdges, IntoEdgesDirected, IntoNeighborsDirected,
    IntoNodeReferences,
};
use petgraph::visit::{IntoNodeIdentifiers, Walker};
use petgraph::EdgeDirection::{self, Incoming};
use petgraph::{algo, Directed, EdgeType};
use petgraph::{
    graph::NodeIndex,
    graph::{EdgeIndex, Graph},
};

use crate::analysis::callgraph::CallGraph;
use crate::constraint_generation::{self, tid_to_tvar};
use crate::constraints::{
    ConstraintSet, DerivedTypeVar, FieldLabel, TyConstraint, TypeVariable, Variance,
};
use crate::graph_algos::mapping_graph::{self, MappingGraph};
use crate::graph_algos::{explore_paths, find_node};

use super::constraint_graph::TypeVarNode;
use super::dfa_operations::{union, Alphabet, Indices, DFA};
use super::scc_constraint_generation::SCCConstraints;
use super::type_lattice::{
    CustomLatticeElement, LatticeDefinition, NamedLattice, NamedLatticeElement,
};

// an equivalence between eq nodes implies an equivalence between edge
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord)]
struct EdgeImplication {
    eq: (NodeIndex, NodeIndex),
    edge: (NodeIndex, NodeIndex),
}

/// Labels for the sketch graph that mantain both an upper bound and lower bound on merged type
#[derive(Clone, PartialEq, Debug, Eq)]
pub struct LatticeBounds<T: Clone + Lattice> {
    upper_bound: T,
    lower_bound: T,
}

impl<T> Display for LatticeBounds<T>
where
    T: Display,
    T: Clone,
    T: Lattice,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{},{}]", self.lower_bound, self.upper_bound)
    }
}

impl<T> LatticeBounds<T>
where
    T: Lattice,
    T: Clone,
{
    fn refine_lower(&self, other: &T) -> Self {
        Self {
            upper_bound: self.upper_bound.clone(),
            lower_bound: self.lower_bound.join(other),
        }
    }

    fn refine_upper(&self, other: &T) -> Self {
        Self {
            upper_bound: self.upper_bound.meet(other),
            lower_bound: self.lower_bound.clone(),
        }
    }
}

impl<T: Lattice + Clone> JoinSemilattice for LatticeBounds<T> {
    fn join(&self, other: &Self) -> Self {
        Self {
            upper_bound: self.upper_bound.join(&other.upper_bound),
            lower_bound: self.lower_bound.join(&other.lower_bound),
        }
    }
}

impl<T: Lattice + Clone> MeetSemilattice for LatticeBounds<T> {
    fn meet(&self, other: &Self) -> Self {
        Self {
            upper_bound: self.upper_bound.meet(&other.upper_bound),
            lower_bound: self.lower_bound.meet(&other.lower_bound),
        }
    }
}

impl<T: Lattice + Clone> PartialOrd for LatticeBounds<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        if other == self {
            return Some(std::cmp::Ordering::Equal);
        }

        let j = self.join(other);
        if &j == self {
            Some(std::cmp::Ordering::Greater)
        } else if &j == other {
            Some(std::cmp::Ordering::Less)
        } else {
            None
        }
    }
}

impl<T: Lattice + Clone> Lattice for LatticeBounds<T> {}

// TODO(ian): This is probably an abelian group, but that requires an identity implementation which is hard because that requires a function that can produce a
// top and bottom element without context but top and bottom are runtime defined.
impl<T> AbstractMagma<Additive> for LatticeBounds<T>
where
    T: Lattice,
    T: Clone,
{
    fn operate(&self, right: &Self) -> Self {
        LatticeBounds {
            upper_bound: right.upper_bound.meet(&self.upper_bound),
            lower_bound: right.lower_bound.join(&self.lower_bound),
        }
    }
}

fn get_edge_set<C>(grph: &MappingGraph<C, DerivedTypeVar, FieldLabel>) -> HashSet<EdgeImplication>
where
    C: std::cmp::PartialEq,
{
    grph.get_graph()
        .edge_indices()
        .cartesian_product(grph.get_graph().edge_indices().collect::<Vec<_>>())
        .filter_map(|(e1, e2)| {
            let w1 = grph.get_graph().edge_weight(e1).unwrap();
            let w2 = grph.get_graph().edge_weight(e2).unwrap();
            let (src1, dst1) = grph.get_graph().edge_endpoints(e1).unwrap();
            let (src2, dst2) = grph.get_graph().edge_endpoints(e2).unwrap();

            if w1 == w2 || w1 == &FieldLabel::Load && w2 == &FieldLabel::Store {
                Some(EdgeImplication {
                    eq: (src1, src2),
                    edge: (dst1, dst2),
                })
            } else {
                None
            }
        })
        .collect()
}

fn constraint_quotients<C>(
    grph: &MappingGraph<C, DerivedTypeVar, FieldLabel>,
    cons: &ConstraintSet,
) -> UnionFind<usize>
where
    C: std::cmp::PartialEq,
{
    let mut uf: UnionFind<usize> =
        UnionFind::new(grph.get_graph().node_indices().max().unwrap().index() + 1);

    if cons.is_empty() {
        return uf;
    }

    for cons in cons.iter() {
        if let TyConstraint::SubTy(sub_cons) = cons {
            info!("{}", sub_cons);
            let lt_node = grph.get_node(&sub_cons.lhs).unwrap();
            let gt_node = grph.get_node(&sub_cons.rhs).unwrap();

            uf.union(lt_node.index(), gt_node.index());
        }
    }

    uf
}

fn generate_quotient_groups<C>(
    grph: &MappingGraph<C, DerivedTypeVar, FieldLabel>,
    cons: &ConstraintSet,
) -> Vec<BTreeSet<NodeIndex>>
where
    C: std::cmp::PartialEq,
{
    let mut cons = constraint_quotients(grph, cons);
    info!("Constraint quotients {:#?}", cons.clone().into_labeling());
    info!("Node mapping {:#?}", grph.get_node_mapping());
    let mut edge_implications = get_edge_set(grph);

    while {
        let prev_labeling = cons.clone().into_labeling();

        for implic in edge_implications.clone().into_iter() {
            if cons.equiv(implic.eq.0.index(), implic.eq.1.index()) {
                edge_implications.remove(&implic);
                cons.union(implic.edge.0.index(), implic.edge.1.index());
            }
        }

        cons.clone().into_labeling() != prev_labeling
    } {}

    for (nd_idx, grouplab) in
        cons.clone()
            .into_labeling()
            .into_iter()
            .enumerate()
            .filter(|(ndidx, _repr)| {
                grph.get_graph()
                    .node_weight(NodeIndex::new(*ndidx))
                    .is_some()
            })
    {
        let nd_idx: NodeIndex = NodeIndex::new(nd_idx);
        info!("Node {}: in group {}", nd_idx.index(), grouplab);
        let _nd = grph.get_graph().node_weight(nd_idx).unwrap();
    }

    cons.into_labeling()
        .into_iter()
        .enumerate()
        .filter(|(ndidx, _repr)| {
            grph.get_graph()
                .node_weight(NodeIndex::new(*ndidx))
                .is_some()
        })
        .map(|(ndidx, repr)| (NodeIndex::new(ndidx), repr))
        .fold(
            HashMap::<usize, BTreeSet<NodeIndex>>::new(),
            |mut total, (nd_ind, repr_group)| {
                total.entry(repr_group).or_default().insert(nd_ind);
                total
            },
        )
        .into_values()
        .collect()
}

/// Creates a structured and labeled sketch graph
/// This algorithm creates polymorphic function types.
/// Type information flows up to callers but not down to callees (callees wont be unified).
/// The reachable subgraph of the callee is copied up to the caller. Callee nodes are labeled.
struct SketckGraphBuilder<'a, U: NamedLatticeElement, T: NamedLattice<U>> {
    // Allows us to map any tid to the correct constraintset
    scc_signatures: HashMap<Tid, Rc<ConstraintSet>>,
    // Collects a shared sketchgraph representing the functions in the SCC
    scc_repr: HashMap<TypeVariable, Rc<SketchGraph<LatticeBounds<U>>>>,
    cg: CallGraph,
    tid_to_cg_idx: HashMap<Tid, NodeIndex>,
    lattice: &'a T,
    type_lattice_elements: HashSet<TypeVariable>,
}

impl<'a, U: NamedLatticeElement, T: NamedLattice<U>> SketckGraphBuilder<'a, U, T>
where
    T: 'a,
    U: Display,
{
    pub fn new(
        cg: CallGraph,
        scc_constraints: Vec<SCCConstraints>,
        lattice: &'a T,
        type_lattice_elements: HashSet<TypeVariable>,
    ) -> SketckGraphBuilder<'a, U, T> {
        let scc_signatures = scc_constraints
            .into_iter()
            .map(|cons| {
                let repr = Rc::new(cons.constraints);
                cons.scc.into_iter().map(move |t| (t.clone(), repr.clone()))
            })
            .flatten()
            .collect::<HashMap<_, _>>();

        let cg_callers = cg
            .node_indices()
            .map(|idx| (cg[idx].clone(), idx))
            .collect();

        SketckGraphBuilder {
            scc_signatures,
            scc_repr: HashMap::new(),
            cg,
            tid_to_cg_idx: cg_callers,
            lattice,
            type_lattice_elements,
        }
    }

    /// The identity operation described for Lattice bounds
    fn identity_element(&self) -> LatticeBounds<U> {
        let bot = self.lattice.bot();
        let top = self.lattice.top();
        LatticeBounds {
            upper_bound: top,
            lower_bound: bot,
        }
    }

    fn insert_dtv(
        &self,
        grph: &mut MappingGraph<LatticeBounds<U>, DerivedTypeVar, FieldLabel>,
        dtv: DerivedTypeVar,
    ) {
        let mut curr_var = DerivedTypeVar::new(dtv.get_base_variable().clone());

        let mut prev = grph.add_node(curr_var.clone(), self.identity_element());
        for fl in dtv.get_field_labels() {
            curr_var.add_field_label(fl.clone());
            let next = grph.add_node(curr_var.clone(), self.identity_element());
            grph.add_edge(prev, next, fl.clone());
            prev = next;
        }
    }

    fn add_variable(
        &self,
        var: &DerivedTypeVar,
        is_internal_variable: &BTreeSet<TypeVariable>,
        nd_graph: &mut MappingGraph<LatticeBounds<U>, DerivedTypeVar, FieldLabel>,
    ) -> anyhow::Result<()> {
        if is_internal_variable.contains(var.get_base_variable())
            || self.type_lattice_elements.contains(var.get_base_variable())
        {
            self.insert_dtv(nd_graph, var.clone());
        } else {
            let ext = self
                .scc_repr
                .get(&var.get_base_variable().to_callee())
                .ok_or(anyhow::anyhow!(
                    "An external variable must have a representation already built {}",
                    var.get_base_variable().to_callee().to_string()
                ))?;

            ext.copy_reachable_subgraph_into(var, nd_graph);
        }

        Ok(())
    }

    fn add_nodes_and_initial_edges(
        &self,
        representing: &Vec<Tid>,
        cs_set: &ConstraintSet,
        nd_graph: &mut MappingGraph<LatticeBounds<U>, DerivedTypeVar, FieldLabel>,
    ) -> anyhow::Result<()> {
        let is_internal_variable = representing
            .iter()
            .map(|x| constraint_generation::tid_to_tvar(x))
            .collect::<BTreeSet<_>>();

        for constraint in cs_set.iter() {
            if let TyConstraint::SubTy(sty) = constraint {
                self.add_variable(&sty.lhs, &is_internal_variable, nd_graph)?;
                self.add_variable(&sty.rhs, &is_internal_variable, nd_graph)?;
            }
        }

        Ok(())
    }

    fn dtv_is_uninterpreted_lattice(&self, dtv: &DerivedTypeVar) -> bool {
        self.type_lattice_elements.contains(dtv.get_base_variable())
            && dtv.get_field_labels().is_empty()
    }

    fn update_lattice_node(
        grph: &mut MappingGraph<LatticeBounds<U>, DerivedTypeVar, FieldLabel>,
        lattice_elem: U,
        target_dtv: &DerivedTypeVar,
        operation: impl Fn(&U, &LatticeBounds<U>) -> LatticeBounds<U>,
    ) {
        let target_group_idx = *grph.get_node(target_dtv).unwrap();
        let orig_value = grph
            .get_graph_mut()
            .node_weight_mut(target_group_idx)
            .unwrap();
        *orig_value = operation(&lattice_elem, orig_value);
    }

    fn label_by(
        &self,
        grph: &mut MappingGraph<LatticeBounds<U>, DerivedTypeVar, FieldLabel>,
        cons: &ConstraintSet,
    ) {
        cons.iter()
            .filter_map(|x| {
                if let TyConstraint::SubTy(sy) = x {
                    Some(sy)
                } else {
                    None
                }
            })
            .for_each(|subty| {
                if self.dtv_is_uninterpreted_lattice(&subty.lhs)
                    && grph.get_node(&subty.rhs).is_some()
                {
                    Self::update_lattice_node(
                        grph,
                        self.lattice
                            .get_elem(&subty.lhs.get_base_variable().get_name())
                            .unwrap(),
                        &subty.rhs,
                        |x: &U, y: &LatticeBounds<U>| y.refine_lower(x),
                    );
                } else if self.dtv_is_uninterpreted_lattice(&subty.rhs)
                    && grph.get_node(&subty.lhs).is_some()
                {
                    Self::update_lattice_node(
                        grph,
                        self.lattice
                            .get_elem(&subty.rhs.get_base_variable().get_name())
                            .unwrap(),
                        &subty.lhs,
                        |x: &U, y: &LatticeBounds<U>| y.refine_upper(x),
                    );
                }
            });
    }

    fn build_and_label_scc_sketch(&mut self, to_reprs: &Vec<Tid>) -> anyhow::Result<()> {
        let sig = self
            .scc_signatures
            .get(&to_reprs[0])
            .expect("scc should have a sig");

        let mut nd_graph: MappingGraph<LatticeBounds<U>, DerivedTypeVar, FieldLabel> =
            MappingGraph::new();

        self.add_nodes_and_initial_edges(&to_reprs, sig, &mut nd_graph)?;
        let qgroups = generate_quotient_groups(&nd_graph, sig);

        info!("Quotient group for scc: {:#?}, {:#?}", to_reprs, qgroups);

        let mut quoted_graph = nd_graph.quoetient_graph(&qgroups);
        assert!(quoted_graph.get_graph().node_count() == qgroups.len());

        self.label_by(&mut quoted_graph, sig);

        let orig_sk_graph = SketchGraph {
            quotient_graph: quoted_graph,
            default_label: self.identity_element(),
        };

        let sk_graph = Rc::new(orig_sk_graph);

        for repr in to_reprs.iter() {
            self.scc_repr
                .insert(constraint_generation::tid_to_tvar(repr), sk_graph.clone());
        }

        Ok(())
    }

    fn get_topo_order_for_cg(&self) -> anyhow::Result<(Graph<Vec<Tid>, ()>, Vec<NodeIndex>)> {
        let condensed = petgraph::algo::condensation(self.cg.clone(), false);
        petgraph::algo::toposort(&condensed, None)
            .map_err(|_| anyhow::anyhow!("cycle error"))
            .with_context(|| "Constructing topological sort of codensed sccs for sketch building")
            .map(|sorted| (condensed, sorted))
    }

    pub fn build(&mut self) -> anyhow::Result<()> {
        let (condensed, mut sorted) = self.get_topo_order_for_cg()?;
        sorted.reverse();

        for idx in sorted {
            let associated_tids = &condensed[idx];
            // condensation shouldnt produce a node that doesnt represent any of the original nodes
            assert!(!associated_tids.is_empty());

            self.build_and_label_scc_sketch(associated_tids)?;
        }

        self.bind_polymorphic_types()?;

        Ok(())
    }

    fn get_built_sketch_from_scc(
        &self,
        associated_scc_tids: &Vec<Tid>,
    ) -> SketchGraph<LatticeBounds<U>> {
        assert!(!associated_scc_tids.is_empty());
        let target_tvar = tid_to_tvar(associated_scc_tids.iter().next().unwrap());
        let new_repr = self
            .scc_repr
            .get(&target_tvar)
            .expect("all type var representations should be built")
            .as_ref()
            .clone();
        new_repr
    }

    // TODO(ian): this could be generalized to let us swap to different lattice reprs
    fn refine_formal(
        &self,
        condensed: &Graph<Vec<Tid>, (), Directed>,
        target_scc_repr: &mut SketchGraph<LatticeBounds<U>>,
        target_dtv: DerivedTypeVar,
        target_idx: NodeIndex,
        merge_operator: &impl Fn(
            &Sketch<LatticeBounds<U>>,
            &Sketch<LatticeBounds<U>>,
        ) -> Sketch<LatticeBounds<U>>,
    ) {
        let parent_nodes = condensed.neighbors_directed(target_idx, EdgeDirection::Incoming);

        let orig_reprs = target_scc_repr.get_representing_sketch(target_dtv.clone());

        // There should only be one representation of a formal in an SCC
        assert_eq!(orig_reprs.len(), 1);
        let orig_repr = &orig_reprs[0];

        let mut call_site_type = parent_nodes
            .map(|scc_idx| {
                let wt = condensed
                    .node_weight(scc_idx)
                    .expect("Should have weight for node index");
                let repr_graph = self.get_built_sketch_from_scc(&wt);
                let sketch =
                    repr_graph.get_representing_sketchs_ignoring_callsite_tags(target_dtv.clone());
                for s in sketch.iter() {
                    println!("Member type for: {} {}", target_dtv, s);
                }
                sketch
            })
            .flatten()
            .reduce(|lhs, rhs| merge_operator(&lhs, &rhs))
            .unwrap_or(orig_repr.clone());
        println!("Merged param type for: {} {}", target_dtv, call_site_type);

        call_site_type.label_dtvs(&orig_repr);

        //let new_type = application_operator(orig_repr, &call_site_type);
        target_scc_repr.replace_dtv(&target_dtv, call_site_type);

        println!("After replace {}", target_scc_repr);
    }

    fn refine_formal_out(
        &self,
        condensed: &Graph<Vec<Tid>, (), Directed>,
        target_scc_repr: &mut SketchGraph<LatticeBounds<U>>,
        target_dtv: DerivedTypeVar,
        target_idx: NodeIndex,
    ) {
        self.refine_formal(
            condensed,
            target_scc_repr,
            target_dtv,
            target_idx,
            &Sketch::intersect,
        )
    }

    fn refine_formal_in(
        &self,
        condensed: &Graph<Vec<Tid>, (), Directed>,
        target_scc_repr: &mut SketchGraph<LatticeBounds<U>>,
        target_dtv: DerivedTypeVar,
        target_idx: NodeIndex,
    ) {
        self.refine_formal(
            condensed,
            target_scc_repr,
            target_dtv,
            target_idx,
            &Sketch::union,
        )
    }

    fn refine_formals(
        &mut self,
        condensed: &Graph<Vec<Tid>, (), Directed>,
        associated_scc_tids: &Vec<Tid>,
        target_idx: NodeIndex,
    ) {
        println!("Working on group {:?}", associated_scc_tids);
        let mut orig_repr = self.get_built_sketch_from_scc(associated_scc_tids);
        // for each in parameter without a callsite tag:
        //bind intersection
        let in_params = orig_repr
            .quotient_graph
            .get_node_mapping()
            .iter()
            .map(|(dtv, _idx)| dtv.clone())
            .filter(|dtv| dtv.get_base_variable().get_cs_tag().is_none() && dtv.is_in_parameter());

        for dtv in in_params.collect::<Vec<DerivedTypeVar>>() {
            self.refine_formal_in(condensed, &mut orig_repr, dtv, target_idx);
        }

        let out_params = orig_repr
            .quotient_graph
            .get_node_mapping()
            .iter()
            .map(|(dtv, _idx)| dtv.clone())
            .filter(|dtv| dtv.get_base_variable().get_cs_tag().is_none() && dtv.is_out_parameter());

        for dtv in out_params.collect::<Vec<DerivedTypeVar>>() {
            self.refine_formal_out(condensed, &mut orig_repr, dtv, target_idx);
        }

        // for each parameter in the scc without
    }

    pub fn bind_polymorphic_types(&mut self) -> anyhow::Result<()> {
        let (condensed, sorted) = self.get_topo_order_for_cg()?;
        for tgt_idx in sorted {
            let target_tid = &condensed[tgt_idx];
            self.refine_formals(&condensed, target_tid, tgt_idx);
        }

        Ok(())
    }
}

/// A constraint graph quotiented over a symmetric subtyping relation. This is not guarenteed to be a DFA since it was not extracted as a reachable subgraph of the constraints.
/// The constraing graph is used to generate sketches. And can stitch sketches back into itself.
#[derive(Clone)]
pub struct SketchGraph<U: std::cmp::PartialEq> {
    quotient_graph: MappingGraph<U, DerivedTypeVar, FieldLabel>,
    default_label: U,
}

impl<U> Display for SketchGraph<U>
where
    U: PartialEq,
    U: Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            Dot::new(&self.quotient_graph.get_graph().map(
                |nd_id, nd_weight| format!("{}:{}", nd_id.index(), nd_weight),
                |e_id, e_weight| format!("{}:{}", e_id.index(), e_weight),
            )),
        )
    }
}

impl<U: Display + Clone + std::cmp::PartialEq + AbstractMagma<Additive>> SketchGraph<U> {
    fn replace_dtv(&mut self, dtv: &DerivedTypeVar, sketch: Sketch<U>) {
        println!("Target {}", self);
        self.quotient_graph
            .replace_node(dtv.clone(), sketch.quotient_graph)
    }

    fn get_representations_by_dtv(
        &self,
        flter: &impl Fn(&DerivedTypeVar) -> bool,
    ) -> Vec<Sketch<U>> {
        self.quotient_graph
            .get_node_mapping()
            .iter()
            .filter(|(canidate, _idx)| flter(canidate))
            .map(|(repr_dtv, idx)| Sketch {
                quotient_graph: self.quotient_graph.get_reachable_subgraph(*idx),
                representing: repr_dtv.clone(),
                default_label: self.default_label.clone(),
            })
            .collect()
    }

    fn get_representing_sketchs_ignoring_callsite_tags(
        &self,
        dtv: DerivedTypeVar,
    ) -> Vec<Sketch<U>> {
        let target_calee = dtv.to_callee();
        self.get_representations_by_dtv(&|canidate| target_calee == canidate.to_callee())
    }

    fn get_representing_sketch(&self, dtv: DerivedTypeVar) -> Vec<Sketch<U>> {
        let target_calee = dtv.to_callee();
        self.get_representations_by_dtv(&|canidate| &target_calee == canidate)
    }
}

use crate::solver::dfa_operations::intersection;

impl Alphabet for FieldLabel {}

impl<T: std::cmp::PartialEq> DFA<FieldLabel> for Sketch<T> {
    fn entry(&self) -> usize {
        self.quotient_graph
            .get_node(&self.representing)
            .expect("subgraph should contain represented node")
            .index()
    }

    fn accept_indices(&self) -> Indices {
        self.quotient_graph
            .get_graph()
            .node_indices()
            .map(|i| i.index())
            .collect()
    }

    fn all_indices(&self) -> Indices {
        self.quotient_graph
            .get_graph()
            .node_indices()
            .map(|i| i.index())
            .collect()
    }

    fn dfa_edges(&self) -> Vec<(usize, FieldLabel, usize)> {
        self.quotient_graph
            .get_graph()
            .edge_references()
            .map(|e| (e.source().index(), e.weight().clone(), e.target().index()))
            .collect()
    }
}

struct ReprMapping(BTreeMap<NodeIndex, (Option<NodeIndex>, Option<NodeIndex>)>);

impl Deref for ReprMapping {
    type Target = BTreeMap<NodeIndex, (Option<NodeIndex>, Option<NodeIndex>)>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ReprMapping {
    fn get_representative_dtv_for<T: std::cmp::PartialEq>(
        &self,
        lhs: &Sketch<T>,
        rhs: &Sketch<T>,
        target: NodeIndex,
    ) -> Option<DerivedTypeVar> {
        self.0.get(&target).and_then(|(one, two)| {
            let lrepr = one.and_then(|repridx| {
                lhs.get_graph()
                    .get_group_for_node(repridx)
                    .into_iter()
                    .next()
            });
            let rrepr = two.and_then(|repridx| {
                rhs.get_graph()
                    .get_group_for_node(repridx)
                    .into_iter()
                    .next()
            });
            lrepr.or(rrepr)
        })
    }
}

/// A reachable subgraph of a sketch graph, representing a given root derived type var.
#[derive(Clone)]
pub struct Sketch<U: std::cmp::PartialEq> {
    quotient_graph: MappingGraph<U, DerivedTypeVar, FieldLabel>,
    representing: DerivedTypeVar,
    default_label: U,
}

impl<U: std::cmp::PartialEq> Display for Sketch<U>
where
    U: Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "{}",
            Dot::new(&self.quotient_graph.get_graph().map(
                |nd_id, nd_weight| format!("{}:{}", nd_id.index(), nd_weight),
                |e_id, e_weight| format!("{}:{}", e_id.index(), e_weight),
            )),
        )
    }
}

impl<U: std::cmp::PartialEq> Sketch<U> {
    fn get_graph(&self) -> &MappingGraph<U, DerivedTypeVar, FieldLabel> {
        &self.quotient_graph
    }
}

impl<U: std::cmp::PartialEq> Sketch<U> {
    fn get_entry(&self) -> NodeIndex {
        *self
            .quotient_graph
            .get_node(&self.representing)
            .expect("Should have the node being represented")
    }
}

impl<U: std::cmp::PartialEq + Clone> Sketch<U> {
    /// Labels this sketchs nodes with the dtvs in the argument,
    /// Also copies the repr.
    // We can actually label without caring about the node weights
    pub fn label_dtvs<V: std::cmp::PartialEq>(&mut self, other_sketch: &Sketch<V>) {
        let mapping: HashMap<DerivedTypeVar, NodeIndex> =
            explore_paths(self.quotient_graph.get_graph(), self.get_entry())
                .filter_map(|(pth, tgt)| {
                    let pth_as_weights = pth
                        .iter()
                        .map(|e| {
                            self.quotient_graph
                                .get_graph()
                                .edge_weight(*e)
                                .expect("indices should be valid")
                        })
                        .collect::<Vec<_>>();
                    let maybe_node = find_node(
                        other_sketch.quotient_graph.get_graph(),
                        other_sketch.get_entry(),
                        pth_as_weights.iter().map(|e| *e),
                    );
                    maybe_node.map(|other_idx| {
                        let grp = other_sketch.get_graph().get_group_for_node(other_idx);
                        grp.into_iter()
                            .map(|dtv| (dtv, tgt))
                            .collect::<Vec<_>>()
                            .into_iter()
                    })
                })
                .flatten()
                .collect();

        self.quotient_graph = self.quotient_graph.relable_representative_nodes(mapping);
    }
}

impl<U: std::cmp::PartialEq + AbstractMagma<Additive>> Sketch<U> {
    pub fn empty_sketch(representing: DerivedTypeVar, default_label: U) -> Sketch<U> {
        let mut grph = MappingGraph::new();
        grph.add_node(representing.clone(), default_label.clone());

        Sketch {
            quotient_graph: grph,
            representing,
            default_label,
        }
    }
}

impl<U: std::cmp::PartialEq + Clone + Lattice + AbstractMagma<Additive> + Display> Sketch<U> {
    /// Returns a graph of the dfa and the entry node index.
    fn create_graph_from_dfa(
        &self,
        dfa: &impl DFA<FieldLabel>,
    ) -> (NodeIndex, StableDiGraph<U, FieldLabel>) {
        let mut grph = StableDiGraph::new();

        let mut mp: HashMap<usize, NodeIndex> = HashMap::new();
        for nd in dfa.all_indices() {
            mp.insert(nd, grph.add_node(self.default_label.clone()));
        }

        dfa.dfa_edges().into_iter().for_each(|(st, w, end)| {
            let st = mp.get(&st).expect("Starting node should be in allindices");
            let end = mp.get(&end).expect("Ending node should be in allindices");
            grph.add_edge(*st, *end, w);
        });

        (
            *mp.get(&dfa.entry())
                .expect("Entry should be in all_indices"),
            grph,
        )
    }

    fn find_representative_nodes_for_new_nodes(
        &self,
        entry_node: NodeIndex,
        new_graph: &StableDiGraph<U, FieldLabel>,
        other_sketch: &Sketch<U>,
    ) -> ReprMapping {
        let pths = explore_paths(&new_graph, entry_node);
        ReprMapping(
            pths.map(|(pth, tgt)| {
                let pth_as_weights = pth
                    .iter()
                    .map(|e| new_graph.edge_weight(*e).expect("indices should be valid"))
                    .collect::<Vec<_>>();
                let lhs = find_node(
                    self.quotient_graph.get_graph(),
                    self.get_entry(),
                    pth_as_weights.iter().map(|e| *e),
                );
                let rhs = find_node(
                    other_sketch.quotient_graph.get_graph(),
                    other_sketch.get_entry(),
                    pth_as_weights.iter().map(|e| *e),
                );
                (tgt, (lhs, rhs))
            })
            .collect(),
        )
    }

    fn binop_sketch(
        &self,
        other: &Sketch<U>,
        lattice_op: &impl Fn(&U, &U) -> U,
        resultant_grph: impl DFA<FieldLabel>,
    ) -> Sketch<U> {
        // Shouldnt operate over sketches representing different types
        // We ignore callsite tags
        assert!(self.representing.to_callee() == other.representing.to_callee());

        let (entry, grph) = self.create_graph_from_dfa(&resultant_grph);
        // maps a new node index to an optional representation in both original graphs

        // find path to each node in grph lookup in both sketches intersect and annotate with set of nodes it is representing

        let mapping_from_new_node_to_representatives_in_orig =
            self.find_representative_nodes_for_new_nodes(entry, &grph, other);

        let mut weight_mapping = MappingGraph::from_dfa_and_labeling(grph);
        for (base_node, (o1, o2)) in mapping_from_new_node_to_representatives_in_orig.iter() {
            let self_label = o1
                .and_then(|o1| self.quotient_graph.get_graph().node_weight(o1).cloned())
                .unwrap_or(self.default_label.clone());

            let other_label = o2
                .and_then(|o2| other.quotient_graph.get_graph().node_weight(o2).cloned())
                .unwrap_or(self.default_label.clone());

            // Both nodes should recogonize the word in the case of an intersection
            //assert!(!self_dtvs.is_empty() && !other_dtvs.is_empty());

            let new_label = lattice_op(&self_label, &other_label);
            *weight_mapping
                .get_graph_mut()
                .node_weight_mut(*base_node)
                .unwrap() = new_label;
        }

        // At this point we have a new graph but it's not guarenteed to be a DFA so the last thing to do is quotient it.
        // We dont need to make anything equal via constraints that's already done, we just let edges sets do the work
        let quot_groups = generate_quotient_groups::<U>(&weight_mapping, &ConstraintSet::default());
        let quot_graph = weight_mapping.quoetient_graph(&quot_groups);
        let relab = quot_graph
            .relable_representative_nodes(HashMap::from([(self.representing.clone(), entry)]));

        Sketch {
            quotient_graph: relab,
            representing: self.representing.clone(),
            default_label: self.default_label.clone(),
        }
    }

    fn intersect(&self, other: &Sketch<U>) -> Sketch<U> {
        self.binop_sketch(other, &U::meet, union(self, other))
    }

    fn union(&self, other: &Sketch<U>) -> Sketch<U> {
        self.binop_sketch(other, &U::join, intersection(self, other))
    }
}

impl<T: AbstractMagma<Additive> + std::cmp::PartialEq> SketchGraph<T> {
    fn add_idx_to(
        &self,
        from_base: &TypeVariable,
        reached_idx: NodeIndex,
        into: &mut MappingGraph<T, DerivedTypeVar, FieldLabel>,
    ) {
        let grp = self.quotient_graph.get_group_for_node(reached_idx);

        let rand_fst = grp.iter().next().expect("groups should be non empty");
        let _index_in_new_graph = into.add_node(
            Self::tag_base_with_destination_tag(from_base, rand_fst.clone()),
            self.quotient_graph
                .get_graph()
                .node_weight(reached_idx)
                .expect("index should have weight")
                .clone(),
        );

        for member in grp.iter() {
            into.merge_nodes(
                Self::tag_base_with_destination_tag(from_base, rand_fst.clone()),
                Self::tag_base_with_destination_tag(from_base, member.clone()),
            );
        }
    }

    fn get_key_and_weight_for_index(&self, idx: NodeIndex) -> (DerivedTypeVar, T) {
        let dtv = self
            .quotient_graph
            .get_group_for_node(idx)
            .into_iter()
            .next()
            .expect("groups should be non empty");

        (
            dtv,
            self.quotient_graph
                .get_graph()
                .node_weight(idx)
                .expect("every node should have a weight")
                .clone(),
        )
    }

    fn tag_base_with_destination_tag(
        from_base: &TypeVariable,
        target: DerivedTypeVar,
    ) -> DerivedTypeVar {
        if target.get_base_variable().to_callee() == from_base.to_callee() {
            DerivedTypeVar::create_with_path(
                from_base.clone(),
                Vec::from_iter(target.get_field_labels().into_iter().cloned()),
            )
        } else {
            target
        }
    }

    /// Copies the reachable subgraph from a DerivedTypeVar in from to the parent graph.
    /// The from variable may contain callsite tags which are stripped when looking up the subgraph but then attached to each node
    /// where the base matches the from var.
    pub fn copy_reachable_subgraph_into(
        &self,
        from: &DerivedTypeVar,
        into: &mut MappingGraph<T, DerivedTypeVar, FieldLabel>,
    ) {
        let representing = DerivedTypeVar::create_with_path(
            from.get_base_variable().to_callee(),
            Vec::from_iter(from.get_field_labels().iter().cloned()),
        );
        info!("Looking for repr {}", representing);

        if let Some(representing) = self.quotient_graph.get_node(&representing) {
            info!("Found repr");
            let reachable_idxs: BTreeSet<_> =
                Dfs::new(self.quotient_graph.get_graph(), *representing)
                    .iter(self.quotient_graph.get_graph())
                    .collect();
            info!(
                "Reaching set: {:#?}",
                &reachable_idxs.iter().map(|x| x.index()).collect::<Vec<_>>()
            );

            reachable_idxs.iter().for_each(|reached_idx| {
                self.add_idx_to(from.get_base_variable(), *reached_idx, into)
            });

            // add edges where both ends are in the subgraph
            for edge in self.quotient_graph.get_graph().edge_references() {
                if reachable_idxs.contains(&edge.target())
                    && reachable_idxs.contains(&edge.source())
                {
                    let (key1, w1) = self.get_key_and_weight_for_index(edge.source());
                    let key1 = Self::tag_base_with_destination_tag(from.get_base_variable(), key1);
                    info!("Source nd {}", key1);
                    let source = into.add_node(key1, w1);

                    let (key2, w2) = self.get_key_and_weight_for_index(edge.target());
                    let key2 = Self::tag_base_with_destination_tag(from.get_base_variable(), key2);
                    info!("Dst nd {}", key2);
                    let target = into.add_node(key2, w2);

                    into.add_edge(source, target, edge.weight().clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use cwe_checker_lib::intermediate_representation::Tid;
    use petgraph::{dot::Dot, graph::DiGraph, visit::EdgeRef};

    use crate::{
        analysis::callgraph::CallGraph,
        constraints::{
            parse_constraint_set, parse_derived_type_variable, ConstraintSet, DerivedTypeVar,
            Field, FieldLabel, TypeVariable,
        },
        solver::{
            scc_constraint_generation::SCCConstraints,
            type_lattice::{LatticeDefinition, NamedLatticeElement},
        },
    };

    use super::SketckGraphBuilder;

    #[test]
    fn test_simple_equivalence() {
        // should reduce to one type
        let (rem, test_set) = parse_constraint_set(
            "
            loop_breaker517.load.σ64@40 <= loop_breaker517
            sub_001014fb.out.load.σ64@40 <= loop_breaker517.store.σ64@0
            sub_001014fb.out.load.σ64@40 <= loop_breaker517
            sub_00101728.in_0 <= sub_001014fb.in_0
        ",
        )
        .expect("Should parse constraints");
        assert!(rem.len() == 0);

        //let _grph = SketchGraph::<()>::new(&test_set);
    }

    /*

    id:
        mov rax, rdi
        ret

    alias_id:
        mov rdi, rdi
        call id
        mov rax, rax
        ret

    caller1:
        mov rdi, rdi
        call alias_id
        mov rax, rax
        ret

    caller2:
        mov rdi, rdi
        call alias_id
        mov rax, rax
        ret

    */

    fn parse_cons_set(s: &str) -> ConstraintSet {
        let (rem, scc_id) = parse_constraint_set(s).expect("Should parse constraints");
        assert!(rem.len() == 0);
        scc_id
    }

    fn init() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[test]
    fn test_polymorphism_dont_unify() {
        init();
        let ids_scc = parse_cons_set(
            "
        sub_id.in_0 <= sub_id.out
        ",
        );

        let ids_tid = Tid::create("sub_id".to_owned(), "0x1000".to_owned());

        let alias_scc = parse_cons_set(
            "
        sub_alias.in_0 <= sub_id:0.in_0
        sub_id:0.out <= sub_alias.out
        ",
        );

        let alias_tid = Tid::create("sub_alias".to_owned(), "0x2000".to_owned());

        let caller1_scc = parse_cons_set(
            "
        sub_caller1.in_0 <= sub_alias:0.in_0
        sub_alias:0.out <= sub_caller1.out
        sub_caller1.in_0.load <= char
        ",
        );

        let caller1_tid = Tid::create("sub_caller1".to_owned(), "0x3000".to_owned());

        let caller2_scc = parse_cons_set(
            "
        sub_caller2.in_0 <= sub_alias:0.in_0
        sub_alias:0.out <= sub_caller2.out
        sub_caller2.in_0 <= int
        ",
        );

        let caller2_tid = Tid::create("sub_caller2".to_owned(), "0x4000".to_owned());

        let def = LatticeDefinition::new(
            vec![
                ("char".to_owned(), "top".to_owned()),
                ("int".to_owned(), "top".to_owned()),
                ("bottom".to_owned(), "char".to_owned()),
                ("bottom".to_owned(), "int".to_owned()),
            ],
            "top".to_owned(),
            "bottom".to_owned(),
            "int".to_owned(),
        );

        let lat = def.generate_lattice();
        let nd_set = lat
            .get_nds()
            .iter()
            .map(|x| TypeVariable::new(x.0.clone()))
            .collect::<HashSet<TypeVariable>>();

        let mut cg: CallGraph = DiGraph::new();

        let id_node = cg.add_node(ids_tid.clone());
        let alias_node = cg.add_node(alias_tid.clone());
        let c1_node = cg.add_node(caller1_tid.clone());
        let c2_node = cg.add_node(caller2_tid.clone());

        cg.add_edge(c1_node, alias_node, ());
        cg.add_edge(c2_node, alias_node, ());
        cg.add_edge(alias_node, id_node, ());

        let mut skb = SketckGraphBuilder::new(
            cg,
            vec![
                SCCConstraints {
                    constraints: ids_scc,
                    scc: vec![ids_tid.clone()],
                },
                SCCConstraints {
                    constraints: alias_scc,
                    scc: vec![alias_tid.clone()],
                },
                SCCConstraints {
                    constraints: caller1_scc,
                    scc: vec![caller1_tid.clone()],
                },
                SCCConstraints {
                    constraints: caller2_scc,
                    scc: vec![caller2_tid.clone()],
                },
            ],
            &lat,
            nd_set,
        );

        skb.build().expect("Should succeed in building sketch");

        let sketches = skb.scc_repr;

        let sg_c2 = sketches
            .get(&TypeVariable::new("sub_caller2".to_owned()))
            .unwrap();

        let (_, sub_c2_in) = parse_derived_type_variable("sub_caller2.in_0").unwrap();
        let idx = sg_c2.quotient_graph.get_node(&sub_c2_in).unwrap();

        let wght = sg_c2.quotient_graph.get_graph().node_weight(*idx).unwrap();
        assert_eq!(wght.upper_bound.get_name(), "int");
        assert_eq!(
            sg_c2
                .quotient_graph
                .get_graph()
                .edges_directed(*idx, petgraph::EdgeDirection::Outgoing)
                .count(),
            0
        );

        let sg_c1 = sketches
            .get(&TypeVariable::new("sub_caller1".to_owned()))
            .unwrap();

        let (_, sub_c1_in) = parse_derived_type_variable("sub_caller1.in_0").unwrap();
        let idx = sg_c1.quotient_graph.get_node(&sub_c1_in).unwrap();

        let wght = sg_c1.quotient_graph.get_graph().node_weight(*idx).unwrap();
        assert_eq!(wght.upper_bound.get_name(), "top");
        assert_eq!(
            sg_c1
                .quotient_graph
                .get_graph()
                .edges_directed(*idx, petgraph::EdgeDirection::Outgoing)
                .count(),
            1
        );
        let singl_edge = sg_c1
            .quotient_graph
            .get_graph()
            .edges_directed(*idx, petgraph::EdgeDirection::Outgoing)
            .next()
            .unwrap();

        assert_eq!(singl_edge.weight(), &FieldLabel::Load);
        let target = &sg_c1.quotient_graph.get_graph()[singl_edge.target()];
        assert_eq!(target.upper_bound.get_name(), "char");
    }

    #[test]
    fn test_intersected_pointer_should_be_applied_to_callee() {
        init();
        let ids_scc = parse_cons_set(
            "
        sub_id.in_0 <= sub_id.out
        ",
        );

        let ids_tid = Tid::create("sub_id".to_owned(), "0x1000".to_owned());

        let caller1_scc = parse_cons_set(
            "
        sub_caller1.in_0 <= sub_id.in_0
        sub_id.out <= sub_caller1.out
        sub_caller1.in_0.load <= char
        ",
        );

        let caller1_tid = Tid::create("sub_caller1".to_owned(), "0x3000".to_owned());

        let caller2_scc = parse_cons_set(
            "
        sub_caller2.in_0 <= sub_id.in_0
        sub_id.out <= sub_caller2.out
        sub_caller2.in_0.load <= int
        ",
        );

        let caller2_tid = Tid::create("sub_caller2".to_owned(), "0x4000".to_owned());

        let def = LatticeDefinition::new(
            vec![
                ("char".to_owned(), "bytetype".to_owned()),
                ("int".to_owned(), "bytetype".to_owned()),
                ("bottom".to_owned(), "char".to_owned()),
                ("bottom".to_owned(), "int".to_owned()),
                ("bytetype".to_owned(), "top".to_owned()),
            ],
            "top".to_owned(),
            "bottom".to_owned(),
            "int".to_owned(),
        );

        let lat = def.generate_lattice();
        let nd_set = lat
            .get_nds()
            .iter()
            .map(|x| TypeVariable::new(x.0.clone()))
            .collect::<HashSet<TypeVariable>>();

        let mut cg: CallGraph = DiGraph::new();

        let id_node = cg.add_node(ids_tid.clone());
        let c1_node = cg.add_node(caller1_tid.clone());
        let c2_node = cg.add_node(caller2_tid.clone());

        cg.add_edge(c1_node, id_node, ());
        cg.add_edge(c2_node, id_node, ());

        let mut skb = SketckGraphBuilder::new(
            cg,
            vec![
                SCCConstraints {
                    constraints: ids_scc,
                    scc: vec![ids_tid.clone()],
                },
                SCCConstraints {
                    constraints: caller1_scc,
                    scc: vec![caller1_tid.clone()],
                },
                SCCConstraints {
                    constraints: caller2_scc,
                    scc: vec![caller2_tid.clone()],
                },
            ],
            &lat,
            nd_set,
        );

        skb.build().expect("Should succeed in building sketch");

        let sketches = skb.scc_repr;

        let sg_id = sketches
            .get(&TypeVariable::new("sub_id".to_owned()))
            .unwrap();

        let (_, id_in0) = parse_derived_type_variable("sub_id.in_0").unwrap();

        let idx = sg_id.quotient_graph.get_node(&id_in0).unwrap();

        let wt = &sg_id.as_ref().quotient_graph.get_graph()[*idx];
        assert_eq!(wt.upper_bound.get_name(), "top");
        assert_eq!(wt.lower_bound.get_name(), "bottom");
        assert_eq!(
            sg_id
                .quotient_graph
                .get_graph()
                .edges_directed(*idx, petgraph::EdgeDirection::Outgoing)
                .count(),
            1
        );

        let e = sg_id
            .quotient_graph
            .get_graph()
            .edges_directed(*idx, petgraph::EdgeDirection::Outgoing)
            .next()
            .unwrap();

        assert_eq!(e.weight(), &FieldLabel::Load);

        let nidx = e.target();

        let wt = &sg_id.as_ref().quotient_graph.get_graph()[nidx];
        assert_eq!(wt.upper_bound.get_name(), "bytetype");
        assert_eq!(wt.lower_bound.get_name(), "bottom");
    }

    #[test]
    fn test_polymorphism_callsites() {
        init();
        let ids_scc = parse_cons_set(
            "
        sub_id.in_0 <= sub_id.out
        ",
        );

        let ids_tid = Tid::create("sub_id".to_owned(), "0x1000".to_owned());
        //σ{}@{}
        let caller_scc = parse_cons_set(
            "
        sub_caller.in_0 <= sub_id:0.in_0
        sub_id:0.out <= sub_caller.out.σ8@0  
        sub_caller.in_1 <= sub_id:1.in_0
        sub_id:1.out <= sub_caller.out.σ32@1  
        sub_caller.in_0 <= char
        sub_caller.in_1 <= int
        ",
        );

        let caller_tid = Tid::create("sub_caller".to_owned(), "0x2000".to_owned());

        let def = LatticeDefinition::new(
            vec![
                ("char".to_owned(), "top".to_owned()),
                ("int".to_owned(), "top".to_owned()),
                ("bottom".to_owned(), "char".to_owned()),
                ("bottom".to_owned(), "int".to_owned()),
            ],
            "top".to_owned(),
            "bottom".to_owned(),
            "int".to_owned(),
        );

        let lat = def.generate_lattice();
        let nd_set = lat
            .get_nds()
            .iter()
            .map(|x| TypeVariable::new(x.0.clone()))
            .collect::<HashSet<TypeVariable>>();

        let mut cg: CallGraph = DiGraph::new();

        let id_node = cg.add_node(ids_tid.clone());
        let caller_node = cg.add_node(caller_tid.clone());

        cg.add_edge(caller_node, id_node, ());

        let mut skb = SketckGraphBuilder::new(
            cg,
            vec![
                SCCConstraints {
                    constraints: ids_scc,
                    scc: vec![ids_tid.clone()],
                },
                SCCConstraints {
                    constraints: caller_scc,
                    scc: vec![caller_tid.clone()],
                },
            ],
            &lat,
            nd_set,
        );

        skb.build().expect("Should succeed in building sketch");

        let sketches = skb.scc_repr;

        let sg = sketches
            .get(&TypeVariable::new("sub_caller".to_owned()))
            .unwrap();

        let (_, sub_c_out) = parse_derived_type_variable("sub_caller.out").unwrap();
        let idx = sg.quotient_graph.get_node(&sub_c_out).unwrap();

        assert_eq!(
            sg.quotient_graph
                .get_graph()
                .edges_directed(*idx, petgraph::EdgeDirection::Outgoing)
                .count(),
            2
        );

        for edg in sg
            .quotient_graph
            .get_graph()
            .edges_directed(*idx, petgraph::EdgeDirection::Outgoing)
        {
            if let FieldLabel::Field(Field { offset: 0, size: 8 }) = edg.weight() {
                let wt = &sg.quotient_graph.get_graph()[edg.target()];
                assert_eq!(wt.upper_bound.get_name(), "char");
            } else {
                assert_eq!(edg.weight(), &FieldLabel::Field(Field::new(1, 32)));
                let wt = &sg.quotient_graph.get_graph()[edg.target()];
                assert_eq!(wt.upper_bound.get_name(), "int");
            }
        }
    }

    #[test]
    fn test_double_pointer() {
        // should reduce to one type
        let (rem, test_set) = parse_constraint_set(
            "
            curr_target.load.σ64@0.+8 <= curr_target
            target.load.σ64@8 <= curr_target.store.σ64@0
        ",
        )
        .expect("Should parse constraints");
        assert!(rem.len() == 0);
        /*
        let grph = SketchGraph::<()>::new(&test_set);

        println!(
            "{}",
            Dot::new(
                &grph
                    .quotient_graph
                    .map(|nd_id, _nd_weight| nd_id.index().to_string(), |_e, e2| e2)
            )
        );

        for (dtv, idx) in grph.dtv_to_group.iter() {
            println!("Dtv: {} Group: {}", dtv, idx.index());
        }*/
    }
}
