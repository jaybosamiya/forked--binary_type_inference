use std::collections::{BTreeMap, BTreeSet, HashMap};

use std::ops::Deref;

use cwe_checker_lib::abstract_domain::DomainMap;
use cwe_checker_lib::analysis::graph::Graph;
use cwe_checker_lib::analysis::interprocedural_fixpoint_generic::NodeValue;
use cwe_checker_lib::intermediate_representation::{ByteSize, Project, Sub, Tid, Variable};
use petgraph::graph::NodeIndex;

use crate::analysis::reaching_definitions::{Context, TermSet};
use crate::constraint_generation::{self, RegisterMapping};
use crate::constraints::{ConstraintSet, DerivedTypeVar, SubtypeConstraint, TypeVariable};
use cwe_checker_lib::analysis::{forward_interprocedural_fixpoint, pointer_inference};

/// The context of register definitions for a given program ICFG node.
pub struct RegisterContext {
    mapping: BTreeMap<Variable, TermSet>,
}

impl RegisterContext {
    /// Creates a new register context that can answer register access queries from a reaching definitions [NodeValue].
    pub fn new(mapping: BTreeMap<Variable, TermSet>) -> RegisterContext {
        RegisterContext { mapping }
    }

    fn create_empty_var_name(
        var: &Variable,
        vman: &mut crate::constraints::VariableManager,
    ) -> TypeVariable {
        vman.fresh()
    }

    fn create_def_constraint(
        repr: TypeVariable,
        defined_var: &Variable,
        def: &Tid,
    ) -> SubtypeConstraint {
        let def_tvar = constraint_generation::tid_indexed_by_variable(def, defined_var);
        SubtypeConstraint::new(DerivedTypeVar::new(def_tvar), DerivedTypeVar::new(repr))
    }

    fn generate_multi_def_constraint(
        defined_var: &Variable,
        defs: &TermSet,
        vman: &mut crate::constraints::VariableManager,
    ) -> (TypeVariable, ConstraintSet) {
        let repr = vman.fresh();
        let constraints = ConstraintSet::from(
            defs.0
                .iter()
                .map(|tid| RegisterContext::create_def_constraint(repr.clone(), defined_var, tid))
                .collect::<BTreeSet<SubtypeConstraint>>(),
        );
        (repr, constraints)
    }
}

impl RegisterMapping for RegisterContext {
    fn access(
        &self,
        var: &Variable,
        vman: &mut crate::constraints::VariableManager,
    ) -> (
        crate::constraints::TypeVariable,
        crate::constraints::ConstraintSet,
    ) {
        let ts = self.mapping.get(var);
        ts.map(|ts| {
            if ts.0.len() == 1 {
                (
                    constraint_generation::tid_indexed_by_variable(ts.iter().next().unwrap(), var),
                    ConstraintSet::empty(),
                )
            } else {
                Self::generate_multi_def_constraint(var, ts, vman)
            }
        })
        .unwrap_or((
            Self::create_empty_var_name(var, vman),
            ConstraintSet::empty(),
        ))
    }
}

/// Runs reaching definitions on the project and produces a mapping from node index to the Register Context.
/// The register context can be queried to determine the representing type variable for an accessed register
pub fn run_analysis(proj: &Project, graph: &Graph) -> HashMap<NodeIndex, RegisterContext> {
    let cont = Context::new(&graph, &proj.program.term.extern_symbols);
    let bottom_btree = BTreeMap::new();
    let mut computation = forward_interprocedural_fixpoint::create_computation(cont, None);

    let entry_sub_to_entry_node_map =
        pointer_inference::compute_entry_function_to_entry_node_map(proj, graph);
    for (_sub_tid, start_node_index) in entry_sub_to_entry_node_map.into_iter() {
        computation.set_node_value(
            start_node_index,
            cwe_checker_lib::analysis::interprocedural_fixpoint_generic::NodeValue::Value(
                DomainMap::from(bottom_btree.clone()),
            ),
        );
    }

    computation.compute();
    computation
        .node_values()
        .iter()
        .filter_map(|(ind, dom_map)| match dom_map {
            NodeValue::CallFlowCombinator { .. } => None,
            NodeValue::Value(v) => Some((ind.clone(), RegisterContext::new(v.deref().clone()))),
        })
        .collect()
}
