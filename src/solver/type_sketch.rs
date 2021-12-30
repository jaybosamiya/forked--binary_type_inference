use std::collections::{BTreeSet, HashSet};
use std::marker::PhantomData;
use std::{collections::HashMap, hash::Hash};

use alga::general::AbstractMagma;
use itertools::Itertools;
use log::info;
use petgraph::unionfind::UnionFind;
use petgraph::visit::Walker;
use petgraph::visit::{Dfs, EdgeRef, IntoNodeReferences};
use petgraph::{
    graph::NodeIndex,
    graph::{EdgeIndex, Graph},
};

use crate::constraints::{
    ConstraintSet, DerivedTypeVar, FieldLabel, TyConstraint, TypeVariable, Variance,
};

use super::constraint_graph::RuleContext;
use super::type_lattice::{NamedLattice, NamedLatticeElement};
// TODO(ian): use this abstraction for the transducer
struct NodeDefinedGraph<N: Clone + Hash + Eq, E: Hash + Eq> {
    grph: Graph<N, E>,
    nodes: HashMap<N, NodeIndex>,
}

impl<N: Clone + Hash + Eq, E: Hash + Eq + Clone> NodeDefinedGraph<N, E> {
    pub fn new() -> NodeDefinedGraph<N, E> {
        NodeDefinedGraph {
            grph: Graph::new(),
            nodes: HashMap::new(),
        }
    }

    pub fn get_graph(&self) -> &Graph<N, E> {
        &self.grph
    }

    pub fn edges_between(
        &self,
        a: NodeIndex,
        b: NodeIndex,
    ) -> impl Iterator<Item = EdgeIndex> + '_ {
        self.grph
            .edges_directed(a, petgraph::EdgeDirection::Outgoing)
            .filter(move |x| x.target() == b)
            .map(|x| x.id())
    }

    pub fn add_edge(&mut self, a: NodeIndex, b: NodeIndex, e: E) -> bool {
        if !self
            .edges_between(a, b)
            .any(|x| self.grph.edge_weight(x) == Some(&e))
        {
            self.grph.add_edge(a, b, e);
            true
        } else {
            false
        }
    }

    pub fn get_node(&self, wt: &N) -> Option<&NodeIndex> {
        self.nodes.get(wt)
    }

    pub fn add_node(&mut self, wt: N) -> NodeIndex {
        if let Some(x) = self.nodes.get(&wt) {
            *x
        } else {
            let nd = self.grph.add_node(wt.clone());
            self.nodes.insert(wt, nd);
            nd
        }
    }

    pub fn quoetient_graph(
        &self,
        groups: &Vec<BTreeSet<NodeIndex>>,
    ) -> NodeDefinedGraph<BTreeSet<NodeIndex>, E> {
        let mut nd: NodeDefinedGraph<BTreeSet<NodeIndex>, E> = NodeDefinedGraph::new();

        let repr_mapping = groups
            .iter()
            .enumerate()
            .map(|(repr_indx, s)| s.iter().map(move |node_idx| (node_idx, repr_indx)))
            .flatten()
            .collect::<HashMap<_, _>>();

        for grp in groups.iter() {
            let _new_node = nd.add_node(grp.clone());
        }

        for edge in self.get_graph().edge_references() {
            let repr_src = &groups[*repr_mapping.get(&edge.source()).unwrap()];
            let repr_dst = &groups[*repr_mapping.get(&edge.target()).unwrap()];

            let src_node = nd.add_node(repr_src.clone());
            let dst_node = nd.add_node(repr_dst.clone());
            nd.add_edge(src_node, dst_node, edge.weight().clone());
        }

        nd
    }
}

#[derive(Debug, Clone)]
/// A sketch is a graph with edges weighted by field labels.
/// These sketches represent the type of the type variable.
/// The sketch stores the entry index to the graph for convenience rather than having to find the root.
/// A sketche's nodes can be labeled by a type T.
pub struct Sketch<T> {
    /// The entry node which represents the type of this sketch.
    pub entry: NodeIndex,
    /// The graph rooted by entry. This graph is prefix closed.
    pub graph: Graph<T, FieldLabel>,
}

struct SketchGraph {
    grph: NodeDefinedGraph<DerivedTypeVar, FieldLabel>,
    quotient_graph: NodeDefinedGraph<BTreeSet<NodeIndex>, FieldLabel>,
    constraint_to_group: HashMap<NodeIndex, NodeIndex>,
}

// an equivalence between eq nodes implies an equivalence between edge
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord)]
struct EdgeImplication {
    eq: (NodeIndex, NodeIndex),
    edge: (NodeIndex, NodeIndex),
}

impl SketchGraph {
    fn insert_dtv(grph: &mut NodeDefinedGraph<DerivedTypeVar, FieldLabel>, dtv: DerivedTypeVar) {
        let mut curr_var = DerivedTypeVar::new(dtv.get_base_variable().clone());

        let mut prev = grph.add_node(curr_var.clone());
        for fl in dtv.get_field_labels() {
            curr_var.add_field_label(fl.clone());
            let next = grph.add_node(curr_var.clone());
            grph.add_edge(prev, next, fl.clone());
            prev = next;
        }
    }

    fn dts_from_constraint_set(s: &ConstraintSet) -> impl Iterator<Item = &DerivedTypeVar> {
        s.iter()
            .filter_map(|x| {
                if let TyConstraint::SubTy(x) = x {
                    Some(vec![&x.lhs, &x.rhs].into_iter())
                } else {
                    None
                }
            })
            .flatten()
    }

    fn constraint_quotients(
        grph: &NodeDefinedGraph<DerivedTypeVar, FieldLabel>,
        cons: &ConstraintSet,
    ) -> UnionFind<usize> {
        if cons.is_empty() {
            return UnionFind::new(0);
        }

        let mut uf: UnionFind<usize> =
            UnionFind::new(grph.get_graph().node_indices().max().unwrap().index() + 1);

        for cons in cons.iter() {
            if let TyConstraint::SubTy(sub_cons) = cons {
                let lt_node = grph.get_node(&sub_cons.lhs).unwrap();
                let gt_node = grph.get_node(&sub_cons.rhs).unwrap();

                uf.union(lt_node.index(), gt_node.index());
            }
        }

        uf
    }

    fn get_edge_set(
        grph: &NodeDefinedGraph<DerivedTypeVar, FieldLabel>,
    ) -> HashSet<EdgeImplication> {
        grph.get_graph()
            .edge_indices()
            .cartesian_product(grph.get_graph().edge_indices())
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

    fn quoetient_graph(
        grph: &NodeDefinedGraph<DerivedTypeVar, FieldLabel>,
        cons: &ConstraintSet,
    ) -> Vec<BTreeSet<NodeIndex>> {
        let mut cons = Self::constraint_quotients(grph, cons);
        let mut edge_implications = Self::get_edge_set(grph);

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

        for (nd_idx, grouplab) in cons.clone().into_labeling().into_iter().enumerate() {
            let nd_idx: NodeIndex = NodeIndex::new(nd_idx);
            let nd = grph.get_graph().node_weight(nd_idx).unwrap();
            info!("Node {}: {} in group {}", nd_idx.index(), nd, grouplab);
        }

        cons.into_labeling()
            .into_iter()
            .enumerate()
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

    pub fn new(s: &ConstraintSet) -> SketchGraph {
        let mut nd = NodeDefinedGraph::new();

        Self::dts_from_constraint_set(s)
            .cloned()
            .for_each(|f| Self::insert_dtv(&mut nd, f));

        let labeled = Self::quoetient_graph(&nd, s);
        let quotient_graph = nd.quoetient_graph(&labeled);

        let old_to_new = quotient_graph
            .get_graph()
            .node_references()
            .map(|(idx, child_node)| child_node.iter().map(move |child| (*child, idx)))
            .flatten()
            .collect();

        SketchGraph {
            grph: nd,
            quotient_graph,
            constraint_to_group: old_to_new,
        }
    }

    /// Gets initial unlabeled sketches
    pub fn get_initial_sketches(
        &self,
        rule_context: &RuleContext,
    ) -> (
        HashMap<TypeVariable, NodeIndex>,
        HashMap<NodeIndex, Sketch<NodeIndex>>,
    ) {
        let graphs = rule_context
            .get_interesting()
            .iter()
            .filter_map(|x| {
                self.get_repr_idx(&DerivedTypeVar::new(x.clone()))
                    .map(|x| (x, self.get_graph_for_idx(x)))
            })
            .collect::<HashMap<NodeIndex, Sketch<NodeIndex>>>();

        let var_map = rule_context
            .get_interesting()
            .iter()
            .filter_map(|x| {
                self.get_repr_idx(&DerivedTypeVar::new(x.clone()))
                    .map(|ndidx| (x.clone(), ndidx))
            })
            .collect();

        (var_map, graphs)
    }

    fn get_repr_idx(&self, dt: &DerivedTypeVar) -> Option<NodeIndex> {
        self.grph
            .get_node(&dt)
            .and_then(|old_idx| self.constraint_to_group.get(old_idx))
            .cloned()
    }

    fn add_edges_to_subgraph(
        &self,
        start: NodeIndex,
        node_map: &HashMap<NodeIndex, NodeIndex>,
        subgraph: &mut Graph<NodeIndex, FieldLabel>,
    ) {
        for e in self
            .quotient_graph
            .get_graph()
            .edges_directed(start, petgraph::EdgeDirection::Outgoing)
        {
            subgraph.add_edge(
                *node_map.get(&e.source()).unwrap(),
                *node_map.get(&e.target()).unwrap(),
                e.weight().clone(),
            );
        }
    }

    pub fn get_graph_for_idx(&self, root: NodeIndex) -> Sketch<NodeIndex> {
        let dfs = Dfs::new(self.quotient_graph.get_graph(), root);
        let mut reachable_subgraph = Graph::new();
        let reachable: Vec<_> = dfs.iter(self.quotient_graph.get_graph()).collect();
        let node_map = reachable
            .iter()
            .map(|old| {
                let new = reachable_subgraph.add_node(*old);
                (*old, new)
            })
            .collect::<HashMap<_, _>>();
        reachable
            .iter()
            .for_each(|x| self.add_edges_to_subgraph(*x, &node_map, &mut reachable_subgraph));

        Sketch {
            entry: *node_map.get(&root).unwrap(),
            graph: reachable_subgraph,
        }
    }
}

/// Binds a sketch node to its original node in the constraint graph
#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct GroupID {
    id: usize,
}

impl GroupID {
    fn new(id: usize) -> GroupID {
        GroupID { id }
    }

    /// Gets the groupid as a plain unsigned integer.
    pub fn get_id(&self) -> usize {
        self.id
    }
}

/// A lattice node with a group id
pub struct LabeledNode<U> {
    /// The ID of the equivalence group of this node.
    pub id: GroupID,
    /// An additional label value.
    pub value: U,
}

/// Get initial unlabeled sketches. The lookup map maps a type variable to its NodeIndex in the equivalence class graph. The sketch graph is labeled by indices into the equivalence
/// class graph.
pub fn get_initial_sketches(
    cons: &ConstraintSet,
    rule_context: &RuleContext,
) -> (
    HashMap<TypeVariable, NodeIndex>,
    HashMap<NodeIndex, Sketch<NodeIndex>>,
) {
    let grph = SketchGraph::new(cons);
    grph.get_initial_sketches(rule_context)
}

/// The context under which a labeling of sketches can be computed. Based on subtyping constraints
/// Sketch nodes will be lableed by computing joins and meets of euqivalence relations.
pub struct LabelingContext<U: NamedLatticeElement, T: NamedLattice<U>> {
    lattice: T,
    nm: std::marker::PhantomData<U>,
    type_lattice_elements: HashSet<TypeVariable>,
}

impl<U: NamedLatticeElement, T: NamedLattice<U>> LabelingContext<U, T> {
    /// Creates a new lattice context described by the named lattice itself which returns the lattice elem for a given string type var
    /// and the set of available elements.
    pub fn new(lattice: T, elements: HashSet<TypeVariable>) -> Self {
        Self {
            lattice,
            type_lattice_elements: elements,
            nm: PhantomData,
        }
    }

    fn apply_variance(
        &self,
        entry: NodeIndex,
        orig_graph: &Graph<NodeIndex, FieldLabel>,
        labeling: &mut HashMap<NodeIndex, U>,
    ) {
        // Stores who we visited and how we visited them.
        let mut visited: HashMap<NodeIndex, Vec<FieldLabel>> = HashMap::new();

        let mut to_visit = Vec::new();
        to_visit.push((entry, Vec::new()));

        while let Some((next_nd, path)) = to_visit.pop() {
            if visited.contains_key(&next_nd) {
                continue;
            }

            visited.insert(next_nd, path.clone());

            for e in orig_graph.edges_directed(next_nd, petgraph::EdgeDirection::Outgoing) {
                if !visited.contains_key(&e.target()) {
                    let mut next_path = path.clone();
                    next_path.push(e.weight().clone());
                    to_visit.push((e.target(), next_path));
                }
            }
        }

        visited
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    v.iter()
                        .map(|x| x.variance())
                        .reduce(|x, y| x.operate(&y))
                        .unwrap_or(Variance::Covariant),
                )
            })
            .for_each(|(new_nd_index, var)| {
                let old_idx = orig_graph.node_weight(new_nd_index).unwrap();
                labeling.insert(
                    *old_idx,
                    match var {
                        Variance::Covariant => self.lattice.top(),
                        Variance::Contravariant => self.lattice.bot(),
                    },
                );
            });
    }

    fn get_initial_labels(
        &self,
        initial_sketches: &HashMap<NodeIndex, Sketch<NodeIndex>>,
    ) -> HashMap<NodeIndex, U> {
        let mut labeling = HashMap::new();
        initial_sketches.iter().for_each(|(_k, sketch)| {
            self.apply_variance(sketch.entry, &sketch.graph, &mut labeling);
        });
        labeling
    }

    fn dtv_is_uninterpreted_lattice(&self, dtv: &DerivedTypeVar) -> bool {
        self.type_lattice_elements.contains(dtv.get_base_variable())
            && dtv.get_field_labels().is_empty()
    }

    // TODO(ian): What about multiple edges with the same weight (paper claims it is prefix closed, is that actually true? can we prove it?)
    fn find_node_following_path<S>(
        entry: NodeIndex,
        path: &[FieldLabel],
        grph: &Graph<S, FieldLabel>,
    ) -> Option<NodeIndex> {
        let mut curr_node = entry;
        for pth_member in path.iter() {
            let found = grph
                .edges_directed(curr_node, petgraph::EdgeDirection::Outgoing)
                .find(|e| e.weight() == pth_member);

            if let Some(found_edge) = found {
                curr_node = found_edge.target();
            } else {
                return None;
            }
        }

        Some(curr_node)
    }

    fn update_lattice_node(
        initial_sketches: &HashMap<NodeIndex, Sketch<NodeIndex>>,
        lookup: &HashMap<TypeVariable, NodeIndex>,
        labeling: &mut HashMap<NodeIndex, U>,
        lattice_elem: U,
        target_dtv: &DerivedTypeVar,
        operation: impl Fn(&U, &U) -> U,
    ) {
        let repr = lookup.get(target_dtv.get_base_variable()).unwrap();
        let sketch = initial_sketches.get(repr).unwrap();

        let target_node_idx = Self::find_node_following_path(
            sketch.entry,
            target_dtv.get_field_labels(),
            &sketch.graph,
        )
        .expect("The sketch for a type variable should acccept its field labels");

        let weight_ref = sketch.graph.node_weight(target_node_idx).unwrap();
        let orig_value = labeling.get_mut(weight_ref).unwrap();
        *orig_value = operation(orig_value, &lattice_elem);
    }

    /// Provided sketches labeled by equivalence class node indeces, computes a labeling of each node by the given lattice and constraint set.
    pub fn label_sketches(
        &self,
        cons: &ConstraintSet,
        lookup: &HashMap<TypeVariable, NodeIndex>,
        sketches: &HashMap<NodeIndex, Sketch<NodeIndex>>,
    ) -> HashMap<NodeIndex, Sketch<LabeledNode<U>>> {
        let mut init = self.get_initial_labels(sketches);
        self.label_inited_sketches(cons, lookup, sketches, &mut init);
        sketches
            .iter()
            .map(|(idx, sketch)| {
                let new_graph = sketch.graph.map(
                    |_, old_idx| LabeledNode {
                        id: GroupID::new(old_idx.index()),
                        value: init.get(old_idx).unwrap().clone(),
                    },
                    |_, e| e.clone(),
                );
                (
                    *idx,
                    Sketch {
                        graph: new_graph,
                        entry: sketch.entry,
                    },
                )
            })
            .collect()
    }

    fn label_inited_sketches(
        &self,
        cons: &ConstraintSet,
        lookup: &HashMap<TypeVariable, NodeIndex>,
        sketches: &HashMap<NodeIndex, Sketch<NodeIndex>>,
        initial_labeling: &mut HashMap<NodeIndex, U>,
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
                    && lookup.contains_key(subty.rhs.get_base_variable())
                {
                    Self::update_lattice_node(
                        sketches,
                        lookup,
                        initial_labeling,
                        self.lattice
                            .get_elem(subty.lhs.get_base_variable().get_name())
                            .unwrap(),
                        &subty.rhs,
                        |x: &U, y: &U| x.join(&y),
                    );
                } else if self.dtv_is_uninterpreted_lattice(&subty.rhs)
                    && lookup.contains_key(subty.lhs.get_base_variable())
                {
                    Self::update_lattice_node(
                        sketches,
                        lookup,
                        initial_labeling,
                        self.lattice
                            .get_elem(subty.rhs.get_base_variable().get_name())
                            .unwrap(),
                        &subty.lhs,
                        |x: &U, y: &U| x.meet(&y),
                    );
                }
            });
    }
}
